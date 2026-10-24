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
        let len: usize = resp.header("content-length").unwrap().parse().unwrap();
        resp.body = vec![0; len];
        conn.read_exact(&mut resp.body).unwrap();
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
