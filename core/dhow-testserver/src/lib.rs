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
//! | `fail=<n>` | the first `n` `GET`s of this exact URL fail (counted per server) |
//! | `fail_mode=reset\|<status>` | how they fail: close without a response (default), or a bare status |
//! | `retry_after=<value>` | `Retry-After` on failure statuses |
//! | `truncate=<n>` | every body is cut after `n` bytes by closing the connection |
//! | `stall_after=<n>` | every body stops after `n` bytes; the connection stays open |
//! | `extra_delay=<ms>` | flush and wait before sending the `extra` bytes, so they arrive in a separate read |
//! | `cut_first=<n>` | only the first `GET` of this URL is cut after `n` bytes |
//! | `change_after=<n>` | after `n` `GET`s of this URL it is a different file: new content and `ETag` |
//! | `lie_after=<n>` | after `n` `GET`s the content changes but the `ETag` doesn't (a lying server) |
//! | `throttle=<bytes/s>` | send the body at about this rate (in 10 ms slices) |
//! | `throttle_first=<bytes/s>` | the first `GET` of this URL at this rate instead (one slow connection) |
//! | `link=<bytes/s>` | all bodies of this URL share one link of this capacity (saturates with enough connections) |
//! | `validators=none\|weak` | no `ETag`/`Last-Modified` at all, or only a weak `ETag` |
//! | `change_each=1` | every `GET` serves a different file (`HEAD` shows the next one) |
//! | `ignore_range_after=<n>` | honor `Range` on the first `n` `GET`s of this URL, then ignore it |
//! | `extra=<n>` | `n` garbage bytes after the body, counted in `Content-Length` (not `Content-Range`) |
//! | `extra_mid=<n>` | the same, but only after ranges that end before the file does (several connections, T-I6) |
//!
//! A request in absolute form (`GET http://host:port/blob/…`, as a client
//! sends it to a forward proxy) is served as if for its path, so a second
//! server can stand in for a proxy: it answers for the origin, and its
//! [`TestServer::requests`] show the absolute targets and any
//! `Proxy-Authorization`.
//!
//! Unknown knobs are a 400, so a misspelled knob fails the test instead of
//! silently testing the happy path.

pub mod blob;
mod http;

use std::collections::HashMap;
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
    /// As in the request line (absolute form through a proxy).
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
/// Per-server state shared by every connection: request counts per exact
/// target (for `fail=` and friends) and `link=` schedules.
#[derive(Default)]
struct Shared {
    counts: Mutex<HashMap<String, u32>>,
    /// When the shared link is next free, per target.
    links: Mutex<HashMap<String, std::time::Instant>>,
}

type Counts = Arc<Shared>;

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
            .enable_all()
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
    let counts = Counts::default();
    loop {
        let (stream, _) = listener.accept().await?;
        let (log, counts) = (log.clone(), counts.clone());
        tokio::spawn(async move {
            let _ = connection(stream, log, counts).await;
        });
    }
}

async fn connection(mut stream: TcpStream, log: Log, counts: Counts) -> io::Result<()> {
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
            target: req.raw_target.clone(),
            headers: req.headers.clone(),
        });
        // GET/HEAD only; a request body would desync the connection.
        if req.header("content-length").is_some_and(|v| v != "0")
            || req.header("transfer-encoding").is_some()
        {
            return simple(&mut stream, "400 Bad Request", false).await;
        }
        if !respond(&mut stream, &req, &counts).await? {
            return Ok(());
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum FailMode {
    Reset,
    Status(u16),
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
    fail: u32,
    fail_mode: FailMode,
    retry_after: Option<String>,
    truncate: Option<u64>,
    stall_after: Option<u64>,
    extra: u64,
    extra_mid: u64,
    ignore_range_after: Option<u32>,
    extra_delay: Option<u64>,
    cut_first: Option<u64>,
    change_after: Option<u32>,
    lie_after: Option<u32>,
    change_each: bool,
    validators: Validators,
    throttle: Option<u64>,
    throttle_first: Option<u64>,
    link: Option<u64>,
}

#[derive(Clone, Copy, PartialEq)]
enum Validators {
    Strong,
    Weak,
    None,
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
        fail: 0,
        fail_mode: FailMode::Reset,
        retry_after: None,
        truncate: None,
        stall_after: None,
        extra: 0,
        extra_mid: 0,
        ignore_range_after: None,
        extra_delay: None,
        cut_first: None,
        change_after: None,
        lie_after: None,
        change_each: false,
        validators: Validators::Strong,
        throttle: None,
        throttle_first: None,
        link: None,
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
            ("fail", v) => k.fail = v.parse().ok()?,
            ("fail_mode", "reset") => k.fail_mode = FailMode::Reset,
            ("fail_mode", v) => {
                k.fail_mode = FailMode::Status(v.parse().ok().filter(|s| (100..600).contains(s))?)
            }
            ("retry_after", v) => k.retry_after = Some(v.to_owned()),
            ("truncate", v) => k.truncate = Some(v.parse().ok()?),
            ("stall_after", v) => k.stall_after = Some(v.parse().ok()?),
            ("extra", v) => k.extra = v.parse().ok()?,
            ("extra_mid", v) => k.extra_mid = v.parse().ok()?,
            ("ignore_range_after", v) => k.ignore_range_after = Some(v.parse().ok()?),
            ("extra_delay", v) => k.extra_delay = Some(v.parse().ok()?),
            ("cut_first", v) => k.cut_first = Some(v.parse().ok()?),
            ("change_after", v) => k.change_after = Some(v.parse().ok()?),
            ("lie_after", v) => k.lie_after = Some(v.parse().ok()?),
            ("change_each", "1") => k.change_each = true,
            ("validators", "weak") => k.validators = Validators::Weak,
            ("validators", "none") => k.validators = Validators::None,
            ("throttle", v) => k.throttle = Some(v.parse().ok().filter(|&r: &u64| r > 0)?),
            ("throttle_first", v) => {
                k.throttle_first = Some(v.parse().ok().filter(|&r: &u64| r > 0)?)
            }
            ("link", v) => k.link = Some(v.parse().ok().filter(|&r: &u64| r > 0)?),
            _ => return None,
        }
    }
    Some(k)
}

/// Writes one response. Returns whether the connection may serve another.
async fn respond(stream: &mut TcpStream, req: &Request, counts: &Counts) -> io::Result<bool> {
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
    if !is_head && k.fail > 0 {
        let seen = {
            let mut counts = counts.counts.lock().unwrap();
            let n = counts.entry(req.target.clone()).or_default();
            *n += 1;
            *n
        };
        if seen <= k.fail {
            return match k.fail_mode {
                FailMode::Reset => Ok(false),
                FailMode::Status(code) => {
                    let mut headers = vec![("Content-Length", "0".to_owned())];
                    headers.extend(k.retry_after.clone().map(|v| ("Retry-After", v)));
                    write_head(stream, &format!("{code} Knob"), &headers, keep).await?;
                    Ok(keep)
                }
            };
        }
    }

    // GETs of this URL before this request; HEAD looks without counting.
    let gets_before = {
        let mut counts = counts.counts.lock().unwrap();
        let n = counts.entry(format!("gets:{}", req.target)).or_default();
        let before = *n;
        if !is_head {
            *n += 1;
        }
        before
    };
    let mut seed = k.seed;
    if k.change_after.is_some_and(|n| gets_before >= n) {
        seed ^= 0x1000;
    }
    if k.change_each {
        seed = seed.wrapping_add(u64::from(gets_before));
    }
    let content_seed = if k.lie_after.is_some_and(|n| gets_before >= n) {
        seed ^ 0x2000
    } else {
        seed
    };
    let etag = format!("\"{seed:x}-{size:x}\"");
    let range_allowed = match k.ignore_range_after {
        Some(n) if !is_head => {
            let mut counts = counts.counts.lock().unwrap();
            let seen = counts
                .entry(format!("ignore_range_after:{}", req.target))
                .or_default();
            *seen += 1;
            *seen <= n
        }
        _ => true,
    };
    // If-Range only ever matches a strong validator (RFC 9110 §13.1.5).
    let honor_range = range_allowed
        && matches!(k.ranges, Ranges::Bytes | Ranges::Hidden)
        && req.header("if-range").is_none_or(|v| {
            k.validators == Validators::Strong && (v == etag || v == LAST_MODIFIED)
        });
    let range = match req.header("range").filter(|_| honor_range) {
        Some(v) => http::parse_range(v, size),
        None => None,
    };

    let mut headers = vec![(
        "Content-Type".to_owned(),
        "application/octet-stream".to_owned(),
    )];
    match k.validators {
        Validators::Strong => {
            headers.push(("ETag".to_owned(), etag));
            headers.push(("Last-Modified".to_owned(), LAST_MODIFIED.to_owned()));
        }
        Validators::Weak => headers.push(("ETag".to_owned(), format!("W/{etag}"))),
        Validators::None => {}
    }
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
    let extra = if is_head || status.starts_with("416") {
        0
    } else if body.end < size {
        k.extra + k.extra_mid
    } else {
        k.extra
    };
    if with_length {
        headers.push((
            "Content-Length".to_owned(),
            (body.end - body.start + extra).to_string(),
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

    let first_cut = k.cut_first.filter(|_| gets_before == 0);
    let cut = [k.truncate, k.stall_after, first_cut]
        .into_iter()
        .flatten()
        .min();
    let stop = cut.map_or(body.end, |n| body.end.min(body.start + n));
    let mut buf = vec![0; BODY_BUF];
    let mut pos = body.start;
    // Throttled: 10 ms slices on a schedule. A shared link adds a second
    // schedule all connections of this URL wait on.
    let throttle = match k.throttle_first {
        Some(r) if gets_before == 0 => Some(r),
        _ => k.throttle,
    };
    let slice = match (throttle, k.link) {
        (Some(r), _) | (None, Some(r)) => (r / 100).clamp(1, BODY_BUF as u64),
        (None, None) => BODY_BUF as u64,
    };
    let paced_from = std::time::Instant::now();
    while pos < stop {
        let n = (stop - pos).min(slice) as usize;
        if let Some(rate) = k.link {
            let at = {
                let mut links = counts.links.lock().unwrap();
                let now = std::time::Instant::now();
                let next = links.entry(req.target.clone()).or_insert(now);
                let at = (*next).max(now);
                *next = at + std::time::Duration::from_secs_f64(n as f64 / rate as f64);
                at
            };
            tokio::time::sleep_until(at.into()).await;
        }
        blob::fill(content_seed, pos, &mut buf[..n]);
        stream.write_all(&buf[..n]).await?;
        pos += n as u64;
        // Paced by schedule, not by sleeping after each write, so the
        // rate holds however long the writes take.
        if let Some(rate) = throttle {
            stream.flush().await?;
            let due = paced_from
                + std::time::Duration::from_secs_f64((pos - body.start) as f64 / rate as f64);
            tokio::time::sleep_until(due.into()).await;
        }
    }
    if stop < body.end {
        stream.flush().await?;
        if k.stall_after.is_some_and(|n| body.start + n == stop) {
            std::future::pending::<()>().await;
        }
        return Ok(false);
    }
    if extra > 0 {
        if let Some(ms) = k.extra_delay {
            stream.flush().await?;
            tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
        }
        stream.write_all(&vec![0xEE; extra as usize]).await?;
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
