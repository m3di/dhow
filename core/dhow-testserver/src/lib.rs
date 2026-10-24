//! Local HTTP/1.1 server for Dhow tests.
//!
//! Routes:
//! - `GET|HEAD /blob/<size>?seed=<u64>` — `size` bytes of deterministic content
//!   (see [`blob`]), with `Accept-Ranges`, a strong `ETag`, `Last-Modified`,
//!   single-range `Range` and `If-Range` support.
//!
//! Unknown query parameters are a 400, so a misspelled knob fails the test
//! instead of silently testing the happy path.

pub mod blob;
mod http;

use std::io;
use std::net::SocketAddr;
use std::thread::JoinHandle;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

use http::{RangeSpec, Request};

pub const LAST_MODIFIED: &str = "Thu, 01 Jan 2026 00:00:00 GMT";
const BODY_BUF: usize = 64 * 1024;

/// A server running on its own thread, bound to an ephemeral loopback port.
/// Shuts down when dropped.
pub struct TestServer {
    addr: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl TestServer {
    pub fn start() -> io::Result<Self> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .build()?;
        let (shutdown, stop) = oneshot::channel();
        let thread = std::thread::spawn(move || {
            runtime.block_on(async move {
                let listener = TcpListener::from_std(listener).expect("adopt listener");
                tokio::select! {
                    _ = serve(listener) => {}
                    _ = stop => {}
                }
            })
        });
        Ok(Self {
            addr,
            shutdown: Some(shutdown),
            thread: Some(thread),
        })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// `path` must start with `/`, e.g. `server.url("/blob/1024?seed=1")`.
    pub fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub async fn serve(listener: TcpListener) -> io::Result<()> {
    loop {
        let (stream, _) = listener.accept().await?;
        tokio::spawn(async move {
            let _ = connection(stream).await;
        });
    }
}

async fn connection(mut stream: TcpStream) -> io::Result<()> {
    stream.set_nodelay(true)?;
    let mut buf = Vec::new();
    loop {
        let end = loop {
            if let Some(end) = http::head_end(&buf) {
                break end;
            }
            if buf.len() > http::MAX_HEAD {
                return simple(&mut stream, "431 Request Header Fields Too Large", false).await;
            }
            let mut chunk = [0; 4096];
            let n = stream.read(&mut chunk).await?;
            if n == 0 {
                return Ok(());
            }
            buf.extend_from_slice(&chunk[..n]);
        };
        let head: Vec<u8> = buf.drain(..end).collect();
        let Some(req) = http::parse_head(&head) else {
            return simple(&mut stream, "400 Bad Request", false).await;
        };
        // GET/HEAD only; a request body would desync the connection.
        if req.header("content-length").is_some_and(|v| v != "0")
            || req.header("transfer-encoding").is_some()
        {
            return simple(&mut stream, "400 Bad Request", false).await;
        }
        respond(&mut stream, &req).await?;
        if !req.keep_alive {
            return Ok(());
        }
    }
}

async fn respond(stream: &mut TcpStream, req: &Request) -> io::Result<()> {
    let keep = req.keep_alive;
    if req.method != "GET" && req.method != "HEAD" {
        return simple(stream, "405 Method Not Allowed", keep).await;
    }
    let Some(size) = req.path.strip_prefix("/blob/") else {
        return simple(stream, "404 Not Found", keep).await;
    };
    let Ok(size) = size.parse::<u64>() else {
        return simple(stream, "404 Not Found", keep).await;
    };
    let mut seed = 0;
    for (k, v) in &req.query {
        match (k.as_str(), v.parse()) {
            ("seed", Ok(v)) => seed = v,
            _ => return simple(stream, "400 Bad Request", keep).await,
        }
    }

    let etag = format!("\"{seed:x}-{size:x}\"");
    let identity_ok = req
        .header("if-range")
        .is_none_or(|v| v == etag || v == LAST_MODIFIED);
    let range = match req.header("range").filter(|_| identity_ok) {
        Some(v) => http::parse_range(v, size),
        None => None,
    };

    let mut headers = vec![
        ("Accept-Ranges", "bytes".to_owned()),
        ("ETag", etag),
        ("Last-Modified", LAST_MODIFIED.to_owned()),
        ("Content-Type", "application/octet-stream".to_owned()),
    ];
    let (status, body) = match range {
        None => ("200 OK", 0..size),
        Some(RangeSpec::Satisfiable(r)) => {
            headers.push((
                "Content-Range",
                format!("bytes {}-{}/{size}", r.start, r.end - 1),
            ));
            ("206 Partial Content", r)
        }
        Some(RangeSpec::Unsatisfiable) => {
            headers.push(("Content-Range", format!("bytes */{size}")));
            ("416 Range Not Satisfiable", 0..0)
        }
    };
    headers.push(("Content-Length", (body.end - body.start).to_string()));
    write_head(stream, status, &headers, keep).await?;
    if req.method == "HEAD" {
        return Ok(());
    }

    let mut buf = vec![0; BODY_BUF];
    let mut pos = body.start;
    while pos < body.end {
        let n = (body.end - pos).min(BODY_BUF as u64) as usize;
        blob::fill(seed, pos, &mut buf[..n]);
        stream.write_all(&buf[..n]).await?;
        pos += n as u64;
    }
    Ok(())
}

async fn simple(stream: &mut TcpStream, status: &str, keep_alive: bool) -> io::Result<()> {
    write_head(
        stream,
        status,
        &[("Content-Length", "0".to_owned())],
        keep_alive,
    )
    .await
}

async fn write_head(
    stream: &mut TcpStream,
    status: &str,
    headers: &[(&str, String)],
    keep_alive: bool,
) -> io::Result<()> {
    let mut head = format!("HTTP/1.1 {status}\r\n");
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    if !keep_alive {
        head.push_str("Connection: close\r\n");
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await
}
