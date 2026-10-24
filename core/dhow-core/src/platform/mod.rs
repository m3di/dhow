//! What the core asks of the platform (ARCHITECTURE §4). Shells implement
//! [`PlatformAdapter`]; every method has a default that does nothing or
//! returns [`AdapterError::Unsupported`], so [`NoopAdapter`] is the trait
//! with nothing overridden.

pub mod linux;

use std::path::{Path, PathBuf};

use reqwest::Url;

/// An opaque handle that finds a file again after it moved (§6).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileToken(pub Vec<u8>);

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AdapterError {
    #[error("not supported on this platform")]
    Unsupported,
    #[error("{0}")]
    Failed(String),
}

/// Proxies by request scheme (§8). All `None` is a direct connection.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProxyConfig {
    pub http: Option<Url>,
    pub https: Option<Url>,
    /// For any scheme without its own (`all_proxy`, SOCKS).
    pub all: Option<Url>,
    /// Hosts, domains (`.example.com`), CIDRs or `*` that go direct.
    pub no_proxy: Vec<String>,
    /// Auto-configuration script. Informational: the core doesn't run PAC.
    pub pac: Option<Url>,
}

impl ProxyConfig {
    pub fn is_direct(&self) -> bool {
        self.http.is_none() && self.https.is_none() && self.all.is_none() && self.pac.is_none()
    }
}

/// Which proxy the HTTP client uses (§8); the setting arrives with M3.4.
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

    /// `Ok(None)`: the file is gone.
    fn file_token_resolve(&self, _token: &FileToken) -> Result<Option<PathBuf>, AdapterError> {
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
