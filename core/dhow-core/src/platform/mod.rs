//! What the core asks of the platform (ARCHITECTURE §4). Shells implement
//! [`PlatformAdapter`]; every method has a default that does nothing or
//! returns [`AdapterError::Unsupported`], so [`NoopAdapter`] is the trait
//! with nothing overridden.

pub mod linux;

use std::fmt;
use std::path::{Path, PathBuf};

use reqwest::Url;

/// An opaque handle that finds a file again after it moved (§6).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileToken(pub Vec<u8>);

/// Where a [`FileToken`] points now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    pub path: PathBuf,
    /// Replaces the stored token (a stale macOS bookmark, re-created).
    pub token: Option<FileToken>,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AdapterError {
    #[error("not supported on this platform")]
    Unsupported,
    #[error("{0}")]
    Failed(String),
}

/// Proxies by request scheme (§8). All `None` is a direct connection.
/// `Debug` redacts like [`redact_proxy_url`], so `NetworkInfo` and
/// `ProxyMode` (derived) are safe in logs too (§5.3).
#[derive(Clone, Default, PartialEq, Eq)]
pub struct ProxyConfig {
    pub http: Option<Url>,
    pub https: Option<Url>,
    /// For any scheme without its own (`all_proxy`, SOCKS).
    pub all: Option<Url>,
    /// Hosts, domains (`.example.com`), CIDRs or `*` that go direct.
    pub no_proxy: Vec<String>,
    /// Auto-configuration script. Informational: the core doesn't run PAC.
    pub pac: Option<Url>,
    /// Hosts without a dot (`intranet`, `nas`) go direct: macOS's "Exclude
    /// simple hostnames". Linux desktops have no such setting.
    pub exclude_simple_hosts: bool,
}

impl fmt::Debug for ProxyConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let url = |u: &Option<Url>| u.as_ref().map(redact_proxy_url);
        f.debug_struct("ProxyConfig")
            .field("http", &url(&self.http))
            .field("https", &url(&self.https))
            .field("all", &url(&self.all))
            .field("no_proxy", &self.no_proxy)
            .field("pac", &url(&self.pac))
            .field("exclude_simple_hosts", &self.exclude_simple_hosts)
            .finish()
    }
}

/// What may be shown or logged of a proxy URL: userinfo (a name can be a
/// token too) becomes `***`, query and fragment go.
pub fn redact_proxy_url(url: &Url) -> String {
    let mut u = url.clone();
    if !u.username().is_empty() || u.password().is_some() {
        let _ = u.set_password(None);
        let _ = u.set_username("***");
    }
    u.set_query(None);
    u.set_fragment(None);
    u.to_string()
}

impl ProxyConfig {
    pub fn is_direct(&self) -> bool {
        self.http.is_none() && self.https.is_none() && self.all.is_none() && self.pac.is_none()
    }
}

/// Which proxy the HTTP client uses (§8): the `proxy_mode` setting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProxyMode {
    System,
    None,
    Custom(Box<ProxyConfig>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InterfaceKind {
    Wifi,
    Ethernet,
    Cellular,
    Vpn,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetworkInfo {
    /// The interface traffic leaves by.
    pub interface_kind: InterfaceKind,
    pub vpn_active: bool,
    /// `None`: no system proxy configured.
    pub system_proxy: Option<ProxyConfig>,
    /// Filled in by the engine; adapters leave it as they like.
    pub proxy_in_use: ProxyMode,
}

/// How long the process may keep transferring in the background.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackgroundCapability {
    Unrestricted,
    TimeLimited,
    /// iOS: only through system background sessions.
    None,
}

pub trait PlatformAdapter: Send + Sync {
    /// `Ok(None)`: this file can't be tracked (§6).
    fn file_token_create(&self, _path: &Path) -> Result<Option<FileToken>, AdapterError> {
        Err(AdapterError::Unsupported)
    }

    /// `Ok(None)`: the file is gone for good (deleted, or in the Trash or
    /// Recycle Bin), and the token is dropped. `Err`: can't tell now (an
    /// unmounted volume); asked again on a later query.
    fn file_token_resolve(&self, _token: &FileToken) -> Result<Option<Resolved>, AdapterError> {
        Err(AdapterError::Unsupported)
    }

    /// The system's proxy for requests to `for_url` (PAC can differ per URL).
    fn system_proxy(&self, _for_url: &Url) -> Result<ProxyConfig, AdapterError> {
        Err(AdapterError::Unsupported)
    }

    fn network_info(&self) -> Result<NetworkInfo, AdapterError> {
        Err(AdapterError::Unsupported)
    }

    /// Calls `cb` whenever the network changes, from the platform's own
    /// notifications: never by polling (ARCHITECTURE §7).
    fn watch_network(&self, _cb: Box<dyn Fn(NetworkInfo) + Send>) -> Result<(), AdapterError> {
        Err(AdapterError::Unsupported)
    }

    /// `true` when the first transfer starts, `false` when the last ends.
    /// Called with an engine lock held: must not call back into the engine.
    fn keep_awake(&self, _on: bool) {}

    fn background_capability(&self) -> BackgroundCapability {
        BackgroundCapability::Unrestricted
    }
}

/// The adapter of a platform that offers nothing.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoopAdapter;

impl PlatformAdapter for NoopAdapter {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_and_display_redact_credentials() {
        let secret = |u: &str| Some(Url::parse(u).unwrap());
        let config = ProxyConfig {
            http: secret("http://tok3n@p.test:1/"),
            https: secret("http://user:pa55@p.test:2/?key=s3cret#f"),
            all: secret("socks5://p.test:3"),
            no_proxy: vec!["lan".into()],
            pac: secret("http://cfg.test/p.pac?auth=s3cret"),
            exclude_simple_hosts: false,
        };
        assert_eq!(
            redact_proxy_url(config.http.as_ref().unwrap()),
            "http://***@p.test:1/"
        );
        assert_eq!(
            redact_proxy_url(config.https.as_ref().unwrap()),
            "http://***@p.test:2/"
        );
        let info = NetworkInfo {
            interface_kind: InterfaceKind::Wifi,
            vpn_active: false,
            system_proxy: Some(config.clone()),
            proxy_in_use: ProxyMode::Custom(Box::new(config)),
        };
        let text = format!("{info:?}");
        for leak in ["tok3n", "user", "pa55", "s3cret"] {
            assert!(!text.contains(leak), "{leak} in {text}");
        }
        assert!(
            text.contains("socks5://p.test:3") && text.contains("cfg.test/p.pac"),
            "{text}"
        );
    }
}
