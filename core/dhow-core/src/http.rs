//! The HTTP client and request plumbing shared by probe and transfer.

use std::time::Duration;

pub use reqwest::Url;
use reqwest::header::{ACCEPT_ENCODING, HeaderName, HeaderValue, IF_RANGE, RANGE};
use reqwest::redirect::Policy;
use reqwest::{Client, NoProxy, Proxy, RequestBuilder};

use crate::platform::{AdapterError, ProxyConfig, ProxyMode, redact_proxy_url};

pub const MAX_REDIRECTS: usize = 10;

/// A user-supplied request header (cookies, auth, referer); opaque to the core.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Header {
    pub name: String,
    pub value: String,
}

/// A client with no proxy.
pub fn client() -> Result<Client, reqwest::Error> {
    client_via(&ProxyConfig::default())
}

/// Timeouts per ENGINE-SPEC §7; proxies per ARCHITECTURE §8. Only
/// `proxy` is used, never reqwest's own reading of the environment.
pub fn client_via(proxy: &ProxyConfig) -> Result<Client, reqwest::Error> {
    let mut builder = Client::builder()
        .user_agent(concat!("Dhow/", env!("CARGO_PKG_VERSION")))
        .redirect(Policy::limited(MAX_REDIRECTS))
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(30))
        .no_proxy();
    let bypass = || NoProxy::from_string(&proxy.no_proxy.join(","));
    // Scheme-specific first: reqwest takes the first that matches.
    if let Some(url) = &proxy.http {
        builder = builder.proxy(Proxy::http(url.clone())?.no_proxy(bypass()));
    }
    if let Some(url) = &proxy.https {
        builder = builder.proxy(Proxy::https(url.clone())?.no_proxy(bypass()));
    }
    if let Some(url) = &proxy.all {
        builder = builder.proxy(Proxy::all(url.clone())?.no_proxy(bypass()));
    }
    if let Some(pac) = &proxy.pac {
        tracing::warn!(pac = %redact_proxy_url(pac), "proxy auto-config is not evaluated");
    }
    builder.build()
}

/// What a client is built for: the mode to report and the proxies it uses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Route {
    pub in_use: ProxyMode,
    pub proxies: ProxyConfig,
}

impl Route {
    /// `system`: the adapter's answer, asked only in `System` mode. A
    /// system without explicit proxies (none, unknown, or PAC only) is
    /// direct and reported as `None`.
    pub fn new(mode: &ProxyMode, system: Option<Result<ProxyConfig, AdapterError>>) -> Self {
        let direct = Self {
            in_use: ProxyMode::None,
            proxies: ProxyConfig::default(),
        };
        match mode {
            ProxyMode::None => direct,
            ProxyMode::Custom(c) => Self {
                in_use: mode.clone(),
                proxies: (**c).clone(),
            },
            ProxyMode::System => match system {
                Some(Ok(c)) if c.http.is_some() || c.https.is_some() || c.all.is_some() => Self {
                    in_use: ProxyMode::System,
                    proxies: c,
                },
                Some(Ok(c)) => Self {
                    proxies: ProxyConfig {
                        pac: c.pac,
                        ..direct.proxies
                    },
                    ..direct
                },
                Some(Err(AdapterError::Failed(e))) => {
                    tracing::warn!(error = %e, "no system proxy, going direct");
                    direct
                }
                Some(Err(AdapterError::Unsupported)) | None => direct,
            },
        }
    }
}

/// The client, built on first use and dropped with its connection pool when
/// the last user is done: pooled keep-alive connections and the pool's idle
/// sweep are tasks, and nothing may run when no transfer does (ARCHITECTURE
/// §7). Connections are reused while anything is running.
#[derive(Debug, Default)]
pub struct ClientSlot {
    inner: std::sync::Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    current: Option<(Route, Client)>,
    leases: usize,
}

impl ClientSlot {
    /// The current client if it was built for `route`, else a new one that
    /// later leases get. Leases of the old one keep it until they end: a
    /// running download finishes on the proxy it started with (§8).
    pub fn lease(&self, route: Route) -> Result<Lease<'_>, reqwest::Error> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let client = match &inner.current {
            Some((r, c)) if *r == route => c.clone(),
            _ => {
                tracing::debug!(proxy = ?route.in_use, "building the HTTP client");
                let c = client_via(&route.proxies)?;
                inner.current = Some((route, c.clone()));
                c
            }
        };
        inner.leases += 1;
        Ok(Lease { slot: self, client })
    }

    /// The mode the current client reports; `None` while there is none.
    pub fn in_use(&self) -> Option<ProxyMode> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.current.as_ref().map(|(r, _)| r.in_use.clone())
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
        inner.leases -= 1;
        if inner.leases == 0 {
            inner.current = None;
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
    fn system_without_explicit_proxies_is_direct() {
        let pac = ProxyConfig {
            pac: Url::parse("http://cfg.test/p.pac").ok(),
            ..ProxyConfig::default()
        };
        for answer in [
            None,
            Some(Err(AdapterError::Unsupported)),
            Some(Err(AdapterError::Failed("dconf".into()))),
            Some(Ok(ProxyConfig::default())),
            Some(Ok(pac)),
        ] {
            let route = Route::new(&ProxyMode::System, answer.clone());
            assert_eq!(route.in_use, ProxyMode::None, "{answer:?}");
            assert!(route.proxies.http.is_none() && route.proxies.all.is_none());
        }
        let p = ProxyConfig {
            https: Url::parse("http://p.test:3128").ok(),
            ..ProxyConfig::default()
        };
        let route = Route::new(&ProxyMode::System, Some(Ok(p.clone())));
        assert_eq!((route.in_use, route.proxies), (ProxyMode::System, p));
        let route = Route::new(&ProxyMode::None, None);
        assert_eq!(route.proxies, ProxyConfig::default());
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
