use std::io::{Read, Write};
use std::net::TcpStream;

use dhow_testserver::{LAST_MODIFIED, TestServer, blob};

struct Response {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Response {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Sends one request on `conn` and reads exactly one response.
fn exchange(conn: &mut TcpStream, head: &str) -> Response {
    conn.write_all(head.as_bytes()).unwrap();
    let mut raw = Vec::new();
    let mut byte = [0];
    while !raw.ends_with(b"\r\n\r\n") {
        conn.read_exact(&mut byte).unwrap();
        raw.push(byte[0]);
    }
    let text = String::from_utf8(raw).unwrap();
    let mut lines = text.split("\r\n");
    let status = lines.next().unwrap()[9..12].parse().unwrap();
    let headers: Vec<(String, String)> = lines
        .filter(|l| !l.is_empty())
        .map(|l| {
            let (k, v) = l.split_once(": ").unwrap();
            (k.to_owned(), v.to_owned())
        })
        .collect();
    let mut resp = Response {
        status,
        headers,
        body: Vec::new(),
    };
    if !head.starts_with("HEAD") {
        match resp.header("content-length") {
            Some(len) => {
                resp.body = vec![0; len.parse().unwrap()];
                conn.read_exact(&mut resp.body).unwrap();
            }
            None => {
                conn.read_to_end(&mut resp.body).unwrap();
            }
        }
    }
    resp
}

fn get(server: &TestServer, target: &str, extra: &str) -> Response {
    let mut conn = TcpStream::connect(server.addr()).unwrap();
    exchange(
        &mut conn,
        &format!("GET {target} HTTP/1.1\r\nHost: t\r\n{extra}\r\n"),
    )
}

#[test]
fn full_body_matches_blob() {
    let server = TestServer::start().unwrap();
    let r = get(&server, "/blob/300000?seed=9", "");
    assert_eq!(r.status, 200);
    assert_eq!(r.header("accept-ranges"), Some("bytes"));
    assert_eq!(r.header("etag"), Some("\"9-493e0\""));
    assert_eq!(r.header("last-modified"), Some(LAST_MODIFIED));
    assert_eq!(blake3::hash(&r.body), blob::digest(9, 300_000));
}

#[test]
fn head_has_headers_and_no_body() {
    let server = TestServer::start().unwrap();
    let mut conn = TcpStream::connect(server.addr()).unwrap();
    let r = exchange(&mut conn, "HEAD /blob/1000 HTTP/1.1\r\nHost: t\r\n\r\n");
    assert_eq!(r.status, 200);
    assert_eq!(r.header("content-length"), Some("1000"));
    // Connection is still usable, so no body bytes were sent.
    let r = exchange(&mut conn, "GET /blob/4 HTTP/1.1\r\nHost: t\r\n\r\n");
    assert_eq!(r.body, blob::bytes(0, 0..4));
}

#[test]
fn range_is_served_as_206() {
    let server = TestServer::start().unwrap();
    let r = get(&server, "/blob/1000?seed=2", "Range: bytes=100-199\r\n");
    assert_eq!(r.status, 206);
    assert_eq!(r.header("content-range"), Some("bytes 100-199/1000"));
    assert_eq!(r.body, blob::bytes(2, 100..200));
}

#[test]
fn probe_range_reveals_total() {
    let server = TestServer::start().unwrap();
    let r = get(&server, "/blob/5000", "Range: bytes=0-0\r\n");
    assert_eq!(r.status, 206);
    assert_eq!(r.header("content-range"), Some("bytes 0-0/5000"));
    assert_eq!(r.body.len(), 1);
}

#[test]
fn unsatisfiable_range_is_416() {
    let server = TestServer::start().unwrap();
    let r = get(&server, "/blob/1000", "Range: bytes=1000-\r\n");
    assert_eq!(r.status, 416);
    assert_eq!(r.header("content-range"), Some("bytes */1000"));
}

#[test]
fn if_range_mismatch_sends_full_body() {
    let server = TestServer::start().unwrap();
    let r = get(
        &server,
        "/blob/1000",
        "Range: bytes=10-19\r\nIf-Range: \"stale\"\r\n",
    );
    assert_eq!(r.status, 200);
    assert_eq!(r.body.len(), 1000);
}

#[test]
fn if_range_match_honors_range() {
    let server = TestServer::start().unwrap();
    for validator in ["\"0-3e8\"", LAST_MODIFIED] {
        let extra = format!("Range: bytes=10-19\r\nIf-Range: {validator}\r\n");
        assert_eq!(
            get(&server, "/blob/1000", &extra).status,
            206,
            "{validator}"
        );
    }
}

#[test]
fn keep_alive_serves_sequential_requests() {
    let server = TestServer::start().unwrap();
    let mut conn = TcpStream::connect(server.addr()).unwrap();
    for i in 0..3u64 {
        let head = format!("GET /blob/100 HTTP/1.1\r\nHost: t\r\nRange: bytes={i}-{i}\r\n\r\n");
        assert_eq!(exchange(&mut conn, &head).body, blob::bytes(0, i..i + 1));
    }
}

#[test]
fn unknown_knob_is_rejected() {
    let server = TestServer::start().unwrap();
    assert_eq!(get(&server, "/blob/10?sede=1", "").status, 400);
}

#[test]
fn unknown_path_is_404() {
    let server = TestServer::start().unwrap();
    assert_eq!(get(&server, "/nope", "").status, 404);
}

fn head(server: &TestServer, target: &str) -> Response {
    let mut conn = TcpStream::connect(server.addr()).unwrap();
    exchange(
        &mut conn,
        &format!("HEAD {target} HTTP/1.1\r\nHost: t\r\n\r\n"),
    )
}

#[test]
fn head_knob_sets_head_status_only() {
    let server = TestServer::start().unwrap();
    assert_eq!(head(&server, "/blob/10?head=405").status, 405);
    assert_eq!(get(&server, "/blob/10?head=405", "").status, 200);
}

#[test]
fn ranges_knob() {
    let server = TestServer::start().unwrap();
    let range = "Range: bytes=0-0\r\n";
    let r = get(&server, "/blob/10?ranges=none", range);
    assert_eq!((r.status, r.header("accept-ranges")), (200, Some("none")));
    let r = get(&server, "/blob/10?ranges=hidden", range);
    assert_eq!((r.status, r.header("accept-ranges")), (206, None));
    let r = get(&server, "/blob/10?ranges=ignore", range);
    assert_eq!((r.status, r.header("accept-ranges")), (200, None));
}

#[test]
fn length_omit_delimits_body_by_close() {
    let server = TestServer::start().unwrap();
    let r = get(&server, "/blob/100000?length=omit&seed=5", "");
    assert_eq!(r.header("content-length"), None);
    assert_eq!(r.header("connection"), Some("close"));
    assert_eq!(r.body, blob::bytes(5, 0..100_000));
    assert_eq!(
        head(&server, "/blob/10?length=omit").header("content-length"),
        None
    );
}

#[test]
fn head_length_omit_affects_head_only() {
    let server = TestServer::start().unwrap();
    assert_eq!(
        head(&server, "/blob/10?head_length=omit").header("content-length"),
        None
    );
    let r = get(&server, "/blob/10?head_length=omit", "");
    assert_eq!(r.header("content-length"), Some("10"));
}

#[test]
fn redirect_counts_down() {
    let server = TestServer::start().unwrap();
    let r = get(&server, "/blob/10?redirect=2&seed=3", "");
    assert_eq!(r.status, 302);
    assert_eq!(r.header("location"), Some("/blob/10?redirect=1&seed=3"));
    let r = get(&server, "/blob/10?redirect=0&seed=3", "");
    assert_eq!(r.body, blob::bytes(3, 0..10));
}

#[test]
fn header_knob_adds_and_replaces() {
    let server = TestServer::start().unwrap();
    let r = get(
        &server,
        "/blob/10?header=Content-Type%3A%20text%2Fplain&header=X-Extra%3A%20a%2Cb",
        "",
    );
    assert_eq!(r.header("content-type"), Some("text/plain"));
    assert_eq!(r.header("x-extra"), Some("a,b"));
    assert_eq!(
        r.headers
            .iter()
            .filter(|(k, _)| k == "Content-Type")
            .count(),
        1
    );
}

#[test]
fn requests_are_logged_in_order() {
    let server = TestServer::start().unwrap();
    head(&server, "/blob/1");
    get(&server, "/blob/2?seed=1", "X-Probe: yes\r\n");
    let seen = server.requests();
    assert_eq!(seen.len(), 2);
    assert_eq!(
        (seen[0].method.as_str(), seen[0].target.as_str()),
        ("HEAD", "/blob/1")
    );
    assert_eq!(seen[1].target, "/blob/2?seed=1");
    assert_eq!(seen[1].header("x-probe"), Some("yes"));
}

#[test]
fn fail_knob_fails_the_first_n_gets_then_serves() {
    let server = TestServer::start().unwrap();
    for _ in 0..2 {
        let mut conn = TcpStream::connect(server.addr()).unwrap();
        conn.write_all(b"GET /blob/10?fail=2 HTTP/1.1\r\nHost: t\r\n\r\n")
            .unwrap();
        let mut rest = Vec::new();
        conn.read_to_end(&mut rest).unwrap();
        assert!(rest.is_empty(), "reset mode closes without a response");
    }
    assert_eq!(get(&server, "/blob/10?fail=2", "").status, 200);
    // HEAD isn't counted; a different URL has its own count.
    assert_eq!(head(&server, "/blob/10?fail=1").status, 200);
    let mut conn = TcpStream::connect(server.addr()).unwrap();
    conn.write_all(b"GET /blob/10?fail=1 HTTP/1.1\r\nHost: t\r\n\r\n")
        .unwrap();
    let mut rest = Vec::new();
    conn.read_to_end(&mut rest).unwrap();
    assert!(rest.is_empty());
}

#[test]
fn fail_with_status_and_retry_after() {
    let server = TestServer::start().unwrap();
    let r = get(&server, "/blob/10?fail=1&fail_mode=503&retry_after=7", "");
    assert_eq!((r.status, r.header("retry-after")), (503, Some("7")));
    assert_eq!(
        get(&server, "/blob/10?fail=1&fail_mode=503&retry_after=7", "").status,
        200
    );
}

/// Reads one response head, then whatever body arrives until close.
fn raw_body(server: &TestServer, target: &str, extra_head: &str) -> (Response, Vec<u8>) {
    let mut conn = TcpStream::connect(server.addr()).unwrap();
    conn.write_all(
        format!("GET {target} HTTP/1.1\r\nHost: t\r\nConnection: close\r\n{extra_head}\r\n")
            .as_bytes(),
    )
    .unwrap();
    let mut raw = Vec::new();
    conn.read_to_end(&mut raw).unwrap();
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    let text = String::from_utf8(raw[..split].to_vec()).unwrap();
    let headers = text
        .split("\r\n")
        .skip(1)
        .filter(|l| !l.is_empty())
        .map(|l| {
            let (k, v) = l.split_once(": ").unwrap();
            (k.to_owned(), v.to_owned())
        })
        .collect();
    let status = text[9..12].parse().unwrap();
    (
        Response {
            status,
            headers,
            body: Vec::new(),
        },
        raw[split..].to_vec(),
    )
}

#[test]
fn truncate_cuts_every_body() {
    let server = TestServer::start().unwrap();
    let (r, body) = raw_body(&server, "/blob/1000?truncate=300", "");
    assert_eq!(r.header("content-length"), Some("1000"));
    assert_eq!(body, blob::bytes(0, 0..300));
    let (r, body) = raw_body(&server, "/blob/1000?truncate=300", "Range: bytes=500-\r\n");
    assert_eq!(r.status, 206);
    assert_eq!(body, blob::bytes(0, 500..800));
}

#[test]
fn extra_appends_garbage_counted_in_length_only() {
    let server = TestServer::start().unwrap();
    let (r, body) = raw_body(&server, "/blob/100?extra=20", "Range: bytes=10-19\r\n");
    assert_eq!(r.header("content-range"), Some("bytes 10-19/100"));
    assert_eq!(r.header("content-length"), Some("30"));
    assert_eq!(&body[..10], &blob::bytes(0, 10..20)[..]);
    assert_eq!(&body[10..], &[0xEE; 20][..]);
}

#[test]
fn stall_after_stops_sending_but_keeps_the_connection() {
    let server = TestServer::start().unwrap();
    let mut conn = TcpStream::connect(server.addr()).unwrap();
    conn.set_read_timeout(Some(std::time::Duration::from_millis(300)))
        .unwrap();
    conn.write_all(b"GET /blob/1000?stall_after=100 HTTP/1.1\r\nHost: t\r\n\r\n")
        .unwrap();
    let mut got = Vec::new();
    let mut buf = [0; 4096];
    let err = loop {
        match conn.read(&mut buf) {
            Ok(0) => panic!("connection closed; should stall"),
            Ok(n) => got.extend_from_slice(&buf[..n]),
            Err(e) => break e,
        }
    };
    assert!(matches!(
        err.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    ));
    let split = got.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    assert_eq!(&got[split..], &blob::bytes(0, 0..100)[..]);
}

#[test]
fn cut_first_cuts_only_the_first_get() {
    let server = TestServer::start().unwrap();
    let (_, body) = raw_body(&server, "/blob/1000?cut_first=100", "");
    assert_eq!(body.len(), 100);
    let (_, body) = raw_body(&server, "/blob/1000?cut_first=100", "");
    assert_eq!(body, blob::bytes(0, 0..1000));
}

#[test]
fn change_after_switches_content_and_etag() {
    let server = TestServer::start().unwrap();
    let path = "/blob/100?change_after=1";
    assert_eq!(head(&server, path).header("etag"), Some("\"0-64\""));
    let first = get(&server, path, "");
    assert_eq!(first.body, blob::bytes(0, 0..100));
    assert_eq!(head(&server, path).header("etag"), Some("\"1000-64\""));
    let second = get(&server, path, "");
    assert_eq!(second.header("etag"), Some("\"1000-64\""));
    assert_eq!(second.body, blob::bytes(0x1000, 0..100));
}

#[test]
fn lie_after_switches_content_but_not_etag() {
    let server = TestServer::start().unwrap();
    let path = "/blob/100?lie_after=1";
    get(&server, path, "");
    let second = get(&server, path, "");
    assert_eq!(second.header("etag"), Some("\"0-64\""));
    assert_eq!(second.body, blob::bytes(0x2000, 0..100));
}

#[test]
fn change_each_serves_a_new_file_every_get() {
    let server = TestServer::start().unwrap();
    let path = "/blob/10?change_each=1";
    let etags: Vec<_> = (0..3)
        .map(|_| get(&server, path, "").header("etag").unwrap().to_owned())
        .collect();
    assert_eq!(etags, ["\"0-a\"", "\"1-a\"", "\"2-a\""]);
    assert_eq!(
        head(&server, path).header("etag"),
        Some("\"3-a\""),
        "HEAD shows the next"
    );
}

#[test]
fn validators_knob() {
    let server = TestServer::start().unwrap();
    let r = get(&server, "/blob/10?validators=none", "");
    assert_eq!((r.header("etag"), r.header("last-modified")), (None, None));
    let r = get(&server, "/blob/10?validators=weak", "");
    assert_eq!(
        (r.header("etag"), r.header("last-modified")),
        (Some("W/\"0-a\""), None)
    );
    // A weak validator never satisfies If-Range.
    let r = get(
        &server,
        "/blob/10?validators=weak",
        "Range: bytes=0-0\r\nIf-Range: \"0-a\"\r\n",
    );
    assert_eq!(r.status, 200);
}
