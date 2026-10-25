//! The proxy setting (ARCHITECTURE §8) and the client it leases. A mode
//! changed by another process applies here at the next lease.

use std::io;

use reqwest::Url;
use serde::{Deserialize, Serialize};

use super::{Engine, EngineError};
use crate::http::{self, Lease, Route};
use crate::platform::{AdapterError, ProxyConfig, ProxyMode};

/// `proxy_mode` as stored; URLs as strings (`url` has no serde here).
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Stored {
    System,
    None,
    Custom {
        http: Option<String>,
        https: Option<String>,
        all: Option<String>,
        no_proxy: Vec<String>,
    },
}

impl Stored {
    /// A `Custom` mode needs at least one `http://`, `https://`,
    /// `socks5://` or `socks5h://` proxy and no PAC script (the core
    /// doesn't run PAC).
    pub(super) fn new(mode: &ProxyMode) -> Result<Self, EngineError> {
        Ok(match mode {
            ProxyMode::System => Stored::System,
            ProxyMode::None => Stored::None,
            ProxyMode::Custom(c) => {
                let urls = [&c.http, &c.https, &c.all];
                if c.pac.is_some() {
                    return Err(EngineError::InvalidSetting("PAC scripts are not supported"));
                }
                if urls.iter().all(|u| u.is_none()) {
                    return Err(EngineError::InvalidSetting("a custom proxy needs a URL"));
                }
                if urls.iter().flat_map(|u| u.as_ref()).any(|u| {
                    !matches!(u.scheme(), "http" | "https" | "socks5" | "socks5h")
                        || u.host().is_none()
                }) {
                    return Err(EngineError::InvalidSetting(
                        "proxies are http, https, socks5 or socks5h URLs",
                    ));
                }
                let s = |u: &Option<Url>| u.as_ref().map(Url::to_string);
                Stored::Custom {
                    http: s(&c.http),
                    https: s(&c.https),
                    all: s(&c.all),
                    no_proxy: c.no_proxy.clone(),
                }
            }
        })
    }

    /// `None`: an unreadable URL.
    pub(super) fn mode(self) -> Option<ProxyMode> {
        let parse = |u: Option<String>| u.map(|u| Url::parse(&u)).transpose().ok();
        Some(match self {
            Stored::System => ProxyMode::System,
            Stored::None => ProxyMode::None,
            Stored::Custom {
                http,
                https,
                all,
                no_proxy,
            } => ProxyMode::Custom(Box::new(ProxyConfig {
                http: parse(http)?,
                https: parse(https)?,
                all: parse(all)?,
                no_proxy,
                pac: None,
            })),
        })
    }
}

impl Engine {
    /// The proxy the HTTP client uses now, or would for the next request
    /// when none is built (`system`: the adapter's current view).
    pub(super) fn proxy_in_use(&self, system: Option<&ProxyConfig>) -> ProxyMode {
        if let Some(mode) = self.client.in_use() {
            return mode;
        }
        let mode = self.settings().map_or(ProxyMode::System, |s| s.proxy_mode);
        Route::new(&mode, Some(Ok(system.cloned().unwrap_or_default()))).in_use
    }

    /// A client for requests to `url` under the current setting. The
    /// system proxy is asked on every lease, off the async workers: the
    /// adapter may block (ARCHITECTURE §8). A changed answer builds a new
    /// client for the leases from then on. When the adapter fails, the
    /// last answer it gave stands: an unknown proxy is not a direct one.
    pub(super) async fn lease(&self, url: &Url) -> Result<Lease<'_>, EngineError> {
        let mode = self.settings()?.proxy_mode;
        let system = match mode {
            ProxyMode::System => {
                let (platform, url) = (self.platform.clone(), url.clone());
                let asked = tokio::task::spawn_blocking(move || platform.system_proxy(&url));
                let answer = asked.await.map_err(io::Error::other)?;
                let mut last = self.last_system.lock().unwrap_or_else(|e| e.into_inner());
                Some(match (answer, &*last) {
                    (Ok(c), _) => {
                        *last = Some(c.clone());
                        Ok(c)
                    }
                    (Err(AdapterError::Failed(e)), Some(known)) => {
                        tracing::warn!(error = %e, "system proxy unknown, keeping the last one");
                        Ok(known.clone())
                    }
                    (Err(e), _) => Err(e),
                })
            }
            _ => None,
        };
        let route = http::Route::new(&mode, system);
        Ok(self.client.lease(route).map_err(io::Error::other)?)
    }
}
