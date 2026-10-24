//! The HTTP client and request plumbing shared by probe and transfer.

use std::time::Duration;

pub use reqwest::Url;
use reqwest::header::{ACCEPT_ENCODING, HeaderName, HeaderValue, IF_RANGE, RANGE};
use reqwest::redirect::Policy;
use reqwest::{Client, RequestBuilder};

pub const MAX_REDIRECTS: usize = 10;

/// A user-supplied request header (cookies, auth, referer); opaque to the core.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Header {
    pub name: String,
    pub value: String,
}

/// Timeouts per ENGINE-SPEC §7. Proxy handling arrives with M3.4; until then
/// no proxy is used, not even from the environment.
pub fn client() -> Result<Client, reqwest::Error> {
    Client::builder()
        .user_agent(concat!("Dhow/", env!("CARGO_PKG_VERSION")))
        .redirect(Policy::limited(MAX_REDIRECTS))
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(30))
        .no_proxy()
        .build()
}

/// The client, built on first use and dropped with its connection pool when
/// the last user is done: pooled keep-alive connections and the pool's idle
/// sweep are tasks, and nothing may run when no transfer does (ARCHITECTURE
/// §7). Connections are reused while anything is running.
#[derive(Debug, Default)]
pub struct ClientSlot {
    inner: std::sync::Mutex<(Option<Client>, usize)>,
}

impl ClientSlot {
    pub fn lease(&self) -> Result<Lease<'_>, reqwest::Error> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let client = match &inner.0 {
            Some(c) => c.clone(),
            None => inner.0.insert(client()?).clone(),
        };
        inner.1 += 1;
        Ok(Lease { slot: self, client })
    }
}

/// A [`ClientSlot`]'s client, in use.
#[derive(Debug)]
pub struct Lease<'a> {
    slot: &'a ClientSlot,
    pub client: Client,
}

impl Drop for Lease<'_> {
    fn drop(&mut self) {
        let mut inner = self.slot.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.1 -= 1;
        if inner.1 == 0 {
            inner.0 = None;
        }
    }
}

/// Adds the user's headers plus `Accept-Encoding: identity`: a compressed
/// response would make byte ranges meaningless. `Range`, `If-Range` and
/// `Accept-Encoding` belong to the engine, so user values for them are
/// dropped. Errors with the name of the first invalid header.
pub(crate) fn with_headers(
    mut req: RequestBuilder,
    headers: &[Header],
) -> Result<RequestBuilder, String> {
    req = req.header(ACCEPT_ENCODING, "identity");
    for h in headers {
        let name = HeaderName::from_bytes(h.name.as_bytes()).map_err(|_| h.name.clone())?;
        let value = HeaderValue::from_str(&h.value).map_err(|_| h.name.clone())?;
        if [ACCEPT_ENCODING, RANGE, IF_RANGE].contains(&name) {
            continue;
        }
        req = req.header(name, value);
    }
    Ok(req)
}

/// `scheme://host[:port]/path` — what may appear in logs. Query strings and
/// userinfo can carry credentials (ARCHITECTURE §5.3).
pub(crate) fn redact(url: &Url) -> String {
    format!("{}{}", url.origin().ascii_serialization(), url.path())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_drops_credentials() {
        let url = Url::parse("https://user:pw@example.com:8443/a/b.zip?token=s3cret#frag").unwrap();
        assert_eq!(redact(&url), "https://example.com:8443/a/b.zip");
    }

    #[test]
    fn engine_headers_win() {
        let client = client().unwrap();
        let user = [
            Header {
                name: "Cookie".into(),
                value: "a=1".into(),
            },
            Header {
                name: "accept-encoding".into(),
                value: "gzip".into(),
            },
            Header {
                name: "Range".into(),
                value: "bytes=5-".into(),
            },
        ];
        let req = with_headers(client.get("http://x/"), &user)
            .unwrap()
            .build()
            .unwrap();
        let h = req.headers();
        assert_eq!(h.get("cookie").unwrap(), "a=1");
        assert_eq!(
            h.get_all("accept-encoding").iter().collect::<Vec<_>>(),
            ["identity"]
        );
        assert!(h.get("range").is_none());
    }

    #[test]
    fn invalid_header_is_named() {
        let client = client().unwrap();
        let bad = [Header {
            name: "Bad Name".into(),
            value: "v".into(),
        }];
        assert_eq!(
            with_headers(client.get("http://x/"), &bad).unwrap_err(),
            "Bad Name"
        );
        let bad = [Header {
            name: "X-Ok".into(),
            value: "line\nbreak".into(),
        }];
        assert_eq!(
            with_headers(client.get("http://x/"), &bad).unwrap_err(),
            "X-Ok"
        );
    }
}
