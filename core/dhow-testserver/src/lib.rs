//! Local HTTP/1.1 server for Dhow tests.
//!
//! Route: `GET|HEAD /blob/<size>?<knobs>` serves `size` bytes of deterministic
//! content (see [`blob`]) with `Accept-Ranges`, a strong `ETag`,
//! `Last-Modified`, single-range `Range` and `If-Range` support. Knobs bend
//! that behaviour:
//!
//! | knob | effect |
//! |---|---|
//! | `seed=<u64>` | content seed (default 0) |
//! | `head=<status>` | answer `HEAD` with this bare status |
//! | `length=omit` | no `Content-Length` on 200s; body ends at connection close |
//! | `head_length=omit` | no `Content-Length` on `HEAD` only |
//! | `ranges=bytes\|none\|hidden\|ignore` | advertise+honor (default) / `Accept-Ranges: none`, ignore / no header, honor / no header, ignore |
//! | `redirect=<n>` | `302` to the same URL with `redirect=<n-1>`, until 0 |
//! | `header=<Name: value>` | add (or replace) a response header on 200/206; repeatable |
//!
//! Unknown knobs are a 400, so a misspelled knob fails the test instead of
//! silently testing the happy path.

pub mod blob;
mod http;

use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

use http::{RangeSpec, Request};

pub const LAST_MODIFIED: &str = "Thu, 01 Jan 2026 00:00:00 GMT";
const BODY_BUF: usize = 64 * 1024;

/// A request as the server received it.
#[derive(Clone, Debug)]
pub struct Seen {
    pub method: String,
    pub target: String,
    pub headers: Vec<(String, String)>,
}

impl Seen {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

type Log = Arc<Mutex<Vec<Seen>>>;

/// A server running on its own thread, bound to an ephemeral loopback port.
/// Shuts down when dropped.
pub struct TestServer {
    addr: SocketAddr,
    log: Log,
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
        let log = Log::default();
        let (shutdown, stop) = oneshot::channel();
        let thread_log = log.clone();
        let thread = std::thread::spawn(move || {
            runtime.block_on(async move {
                let listener = TcpListener::from_std(listener).expect("adopt listener");
                tokio::select! {
                    _ = serve_logged(listener, thread_log) => {}
                    _ = stop => {}
                }
            })
        });
        Ok(Self {
            addr,
            log,
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

    /// Every request received so far, in arrival order.
    pub fn requests(&self) -> Vec<Seen> {
        self.log.lock().unwrap().clone()
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
    serve_logged(listener, Log::default()).await
}

async fn serve_logged(listener: TcpListener, log: Log) -> io::Result<()> {
    loop {
        let (stream, _) = listener.accept().await?;
        let log = log.clone();
        tokio::spawn(async move {
            let _ = connection(stream, log).await;
        });
    }
}

async fn connection(mut stream: TcpStream, log: Log) -> io::Result<()> {
    stream.set_nodelay(true)?;
    let mut buf = Vec::new();
    loop {
        let end = loop {
            if let Some(end) = http::head_end(&buf) {
                break end;
            }
            if buf.len() > http::MAX_HEAD {
                let status = "431 Request Header Fields Too Large";
                return simple(&mut stream, status, false).await;
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
        log.lock().unwrap().push(Seen {
            method: req.method.clone(),
            target: req.target.clone(),
            headers: req.headers.clone(),
        });
        // GET/HEAD only; a request body would desync the connection.
        if req.header("content-length").is_some_and(|v| v != "0")
            || req.header("transfer-encoding").is_some()
        {
            return simple(&mut stream, "400 Bad Request", false).await;
        }
        if !respond(&mut stream, &req).await? {
            return Ok(());
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Ranges {
    Bytes,
    None,
    Hidden,
    Ignore,
}

struct Knobs {
    seed: u64,
    head: Option<u16>,
    length: bool,
    head_length: bool,
    ranges: Ranges,
    redirect: u32,
    headers: Vec<(String, String)>,
}

fn knobs(query: &[(String, String)]) -> Option<Knobs> {
    let mut k = Knobs {
        seed: 0,
        head: None,
        length: true,
        head_length: true,
        ranges: Ranges::Bytes,
        redirect: 0,
        headers: Vec::new(),
    };
    for (name, value) in query {
        match (name.as_str(), value.as_str()) {
            ("seed", v) => k.seed = v.parse().ok()?,
            ("head", v) => k.head = Some(v.parse().ok().filter(|s| (100..600).contains(s))?),
            ("length", "omit") => k.length = false,
            ("head_length", "omit") => k.head_length = false,
            ("ranges", "bytes") => k.ranges = Ranges::Bytes,
            ("ranges", "none") => k.ranges = Ranges::None,
            ("ranges", "hidden") => k.ranges = Ranges::Hidden,
            ("ranges", "ignore") => k.ranges = Ranges::Ignore,
            ("redirect", v) => k.redirect = v.parse().ok()?,
            ("header", v) => {
                let (n, v) = v.split_once(':')?;
                k.headers.push((n.trim().to_owned(), v.trim().to_owned()));
            }
            _ => return None,
        }
    }
    Some(k)
}

/// Writes one response. Returns whether the connection may serve another.
async fn respond(stream: &mut TcpStream, req: &Request) -> io::Result<bool> {
    let keep = req.keep_alive;
    let is_head = req.method == "HEAD";
    if req.method != "GET" && !is_head {
        simple(stream, "405 Method Not Allowed", keep).await?;
        return Ok(keep);
    }
    let size = req.path.strip_prefix("/blob/").and_then(|s| s.parse().ok());
    let Some(size) = size else {
        simple(stream, "404 Not Found", keep).await?;
        return Ok(keep);
    };
    let Some(k) = knobs(&req.query) else {
        simple(stream, "400 Bad Request", keep).await?;
        return Ok(keep);
    };

    if k.redirect > 0 {
        let headers = [
            ("Location", redirect_target(&req.target, k.redirect - 1)),
            ("Content-Length", "0".to_owned()),
        ];
        write_head(stream, "302 Found", &headers, keep).await?;
        return Ok(keep);
    }
    if let (true, Some(status)) = (is_head, k.head) {
        simple(stream, &format!("{status} Knob"), keep).await?;
        return Ok(keep);
    }

    let etag = format!("\"{:x}-{size:x}\"", k.seed);
    let honor_range = matches!(k.ranges, Ranges::Bytes | Ranges::Hidden)
        && req
            .header("if-range")
            .is_none_or(|v| v == etag || v == LAST_MODIFIED);
    let range = match req.header("range").filter(|_| honor_range) {
        Some(v) => http::parse_range(v, size),
        None => None,
    };

    let mut headers = vec![
        ("ETag".to_owned(), etag),
        ("Last-Modified".to_owned(), LAST_MODIFIED.to_owned()),
        (
            "Content-Type".to_owned(),
            "application/octet-stream".to_owned(),
        ),
    ];
    match k.ranges {
        Ranges::Bytes => headers.push(("Accept-Ranges".to_owned(), "bytes".to_owned())),
        Ranges::None => headers.push(("Accept-Ranges".to_owned(), "none".to_owned())),
        Ranges::Hidden | Ranges::Ignore => {}
    }
    for (name, value) in k.headers {
        headers.retain(|(n, _)| !n.eq_ignore_ascii_case(&name));
        headers.push((name, value));
    }

    let (status, body, with_length) = match range {
        None => {
            let with_length = k.length && (!is_head || k.head_length);
            ("200 OK", 0..size, with_length)
        }
        Some(RangeSpec::Satisfiable(r)) => {
            let cr = format!("bytes {}-{}/{size}", r.start, r.end - 1);
            headers.push(("Content-Range".to_owned(), cr));
            ("206 Partial Content", r, true)
        }
        Some(RangeSpec::Unsatisfiable) => {
            headers.push(("Content-Range".to_owned(), format!("bytes */{size}")));
            ("416 Range Not Satisfiable", 0..0, true)
        }
    };
    // Without a length, a GET body is delimited by closing the connection.
    let keep = keep && (with_length || is_head);
    if with_length {
        headers.push((
            "Content-Length".to_owned(),
            (body.end - body.start).to_string(),
        ));
    }
    let headers: Vec<(&str, String)> = headers
        .iter()
        .map(|(n, v)| (n.as_str(), v.clone()))
        .collect();
    write_head(stream, status, &headers, keep).await?;
    if is_head {
        return Ok(keep);
    }

    let mut buf = vec![0; BODY_BUF];
    let mut pos = body.start;
    while pos < body.end {
        let n = (body.end - pos).min(BODY_BUF as u64) as usize;
        blob::fill(k.seed, pos, &mut buf[..n]);
        stream.write_all(&buf[..n]).await?;
        pos += n as u64;
    }
    Ok(keep)
}

/// The request target with `redirect=` set to `n`, raw encoding preserved.
fn redirect_target(target: &str, n: u32) -> String {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let query: Vec<String> = query
        .split('&')
        .map(|p| {
            if p.starts_with("redirect=") {
                format!("redirect={n}")
            } else {
                p.to_owned()
            }
        })
        .collect();
    format!("{path}?{}", query.join("&"))
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
