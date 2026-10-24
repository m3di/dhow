//! Hand-rolled HTTP/1.1, on purpose: later knobs must be able to break the
//! protocol (send past `Content-Length`, inject garbage, stall headers), which
//! a correct HTTP library will not let us do.

use std::ops::Range;

pub const MAX_HEAD: usize = 16 * 1024;

pub struct Request {
    pub method: String,
    /// Path plus raw (still percent-encoded) query; an absolute-form
    /// target (`http://host/path`, as sent to a proxy) loses its origin.
    pub target: String,
    /// The request line's target exactly as received.
    pub raw_target: String,
    pub path: String,
    /// Percent-decoded.
    pub query: Vec<(String, String)>,
    pub keep_alive: bool,
    pub headers: Vec<(String, String)>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Position just past the `\r\n\r\n` ending the head, if the buffer holds one.
pub fn head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

pub fn parse_head(head: &[u8]) -> Option<Request> {
    let head = std::str::from_utf8(head).ok()?;
    let mut lines = head.split("\r\n");
    let mut parts = lines.next()?.split(' ');
    let (method, target, version) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    let raw_target = target;
    let target = match target.strip_prefix("http://") {
        Some(rest) => rest.find('/').map_or("/", |i| &rest[i..]),
        None => target,
    };
    let headers: Vec<(String, String)> = lines
        .filter(|l| !l.is_empty())
        .map(|l| {
            let (k, v) = l.split_once(':')?;
            Some((k.trim().to_owned(), v.trim().to_owned()))
        })
        .collect::<Option<_>>()?;
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let query = query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            Some((percent_decode(k)?, percent_decode(v)?))
        })
        .collect::<Option<_>>()?;
    let mut req = Request {
        method: method.to_owned(),
        target: target.to_owned(),
        raw_target: raw_target.to_owned(),
        path: path.to_owned(),
        query,
        keep_alive: version == "HTTP/1.1",
        headers,
    };
    if req
        .header("connection")
        .is_some_and(|v| v.eq_ignore_ascii_case("close"))
    {
        req.keep_alive = false;
    }
    Some(req)
}

fn percent_decode(s: &str) -> Option<String> {
    let mut out = Vec::with_capacity(s.len());
    let mut bytes = s.bytes();
    while let Some(b) = bytes.next() {
        out.push(match b {
            b'%' => {
                let hex = [bytes.next()?, bytes.next()?];
                u8::from_str_radix(std::str::from_utf8(&hex).ok()?, 16).ok()?
            }
            b'+' => b' ',
            b => b,
        });
    }
    String::from_utf8(out).ok()
}

pub enum RangeSpec {
    Satisfiable(Range<u64>),
    Unsatisfiable,
}

/// Parses a single `Range: bytes=…` value. `None` means "ignore the header and
/// send 200", which RFC 9110 allows for anything we don't handle (multi-range,
/// other units, malformed).
pub fn parse_range(value: &str, size: u64) -> Option<RangeSpec> {
    let spec = value.trim().strip_prefix("bytes=")?;
    if spec.contains(',') {
        return None;
    }
    let (first, last) = spec.split_once('-')?;
    let range = match (first, last) {
        ("", n) => {
            let n: u64 = n.parse().ok()?;
            if n == 0 || size == 0 {
                return Some(RangeSpec::Unsatisfiable);
            }
            size.saturating_sub(n)..size
        }
        (a, "") => a.parse().ok()?..size,
        (a, b) => {
            let (a, b): (u64, u64) = (a.parse().ok()?, b.parse().ok()?);
            if a > b {
                return None;
            }
            a..b.saturating_add(1).min(size)
        }
    };
    Some(if range.start >= size {
        RangeSpec::Unsatisfiable
    } else {
        RangeSpec::Satisfiable(range)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(v: &str, size: u64) -> Option<Option<Range<u64>>> {
        parse_range(v, size).map(|s| match s {
            RangeSpec::Satisfiable(r) => Some(r),
            RangeSpec::Unsatisfiable => None,
        })
    }

    #[test]
    fn range_forms() {
        assert_eq!(range("bytes=0-0", 10), Some(Some(0..1)));
        assert_eq!(range("bytes=2-5", 10), Some(Some(2..6)));
        assert_eq!(range("bytes=2-99", 10), Some(Some(2..10)));
        assert_eq!(range("bytes=4-", 10), Some(Some(4..10)));
        assert_eq!(range("bytes=-3", 10), Some(Some(7..10)));
        assert_eq!(range("bytes=-30", 10), Some(Some(0..10)));
    }

    #[test]
    fn unsatisfiable() {
        assert_eq!(range("bytes=10-", 10), Some(None));
        assert_eq!(range("bytes=10-20", 10), Some(None));
        assert_eq!(range("bytes=-0", 10), Some(None));
        assert_eq!(range("bytes=0-0", 0), Some(None));
    }

    #[test]
    fn ignored() {
        assert_eq!(range("bytes=0-1,4-5", 10), None);
        assert_eq!(range("items=0-1", 10), None);
        assert_eq!(range("bytes=5-2", 10), None);
        assert_eq!(range("bytes=x-", 10), None);
    }

    #[test]
    fn head() {
        let req =
            parse_head(b"GET /blob/10?seed=3 HTTP/1.1\r\nHost: x\r\nRANGE: bytes=0-0\r\n\r\n")
                .unwrap();
        assert_eq!(req.method, "GET");
        assert_eq!(req.path, "/blob/10");
        assert_eq!(req.query, [("seed".to_owned(), "3".to_owned())]);
        assert_eq!(req.header("range"), Some("bytes=0-0"));
        assert!(req.keep_alive);
        assert!(
            !parse_head(b"GET / HTTP/1.1\r\nConnection: close\r\n\r\n")
                .unwrap()
                .keep_alive
        );
        assert!(!parse_head(b"GET / HTTP/1.0\r\n\r\n").unwrap().keep_alive);
        assert!(parse_head(b"garbage\r\n\r\n").is_none());
        let req = parse_head(b"GET /b?header=X-A%3A%201+2 HTTP/1.1\r\n\r\n").unwrap();
        assert_eq!(req.query, [("header".to_owned(), "X-A: 1 2".to_owned())]);
        assert_eq!(req.target, "/b?header=X-A%3A%201+2");
        assert_eq!(req.raw_target, req.target);
        let req = parse_head(b"GET http://o.test:8/blob/9?seed=1 HTTP/1.1\r\n\r\n").unwrap();
        assert_eq!(
            (req.target.as_str(), req.path.as_str()),
            ("/blob/9?seed=1", "/blob/9")
        );
        assert_eq!(req.raw_target, "http://o.test:8/blob/9?seed=1");
        assert_eq!(
            parse_head(b"GET http://o.test HTTP/1.1\r\n\r\n")
                .unwrap()
                .path,
            "/"
        );
        assert!(parse_head(b"GET /b?x=%zz HTTP/1.1\r\n\r\n").is_none());
    }
}
