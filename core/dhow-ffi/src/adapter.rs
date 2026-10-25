//! The platform adapter as a foreign trait (ARCHITECTURE §4): the shell
//! implements it, the engine calls it.

use std::path::Path;
use std::sync::Arc;

use dhow_core::platform as core;
use reqwest::Url;

use crate::error::AdapterError;
use crate::types::{BackgroundCapability, InterfaceKind, ProxyConfig};

/// Where a file token points now (ARCHITECTURE §6).
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct ResolvedFile {
    pub path: String,
    /// Replaces the stored token (a stale bookmark, re-created).
    pub token: Option<Vec<u8>>,
}

/// The adapter's view of the network; the engine adds `proxy_in_use`.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct AdapterNetworkInfo {
    pub interface_kind: InterfaceKind,
    pub vpn_active: bool,
    pub system_proxy: Option<ProxyConfig>,
}

/// Implemented by the shell. The engine calls it from its own threads,
/// never the shell's main thread, and every method may block: `get`,
/// `list` and `storage_breakdown` call the file-token methods on their
/// caller's thread (so shells query off the main thread), proxy lookups
/// run on the engine's blocking pool. An implementation must not hop
/// synchronously to the main thread, and must not call back into the
/// engine from `keep_awake` (the engine holds a lock).
#[uniffi::export(with_foreign)]
pub trait PlatformAdapter: Send + Sync {
    /// `Ok(None)`: this file can't be tracked.
    fn file_token_create(&self, path: String) -> Result<Option<Vec<u8>>, AdapterError>;

    /// `Ok(None)`: gone for good (deleted, or in the Trash); the token is
    /// dropped. `Err(Failed)`: can't tell now; asked again later.
    fn file_token_resolve(&self, token: Vec<u8>) -> Result<Option<ResolvedFile>, AdapterError>;

    /// The system's proxy for requests to `for_url`.
    fn system_proxy(&self, for_url: String) -> Result<ProxyConfig, AdapterError>;

    fn network_info(&self) -> Result<AdapterNetworkInfo, AdapterError>;

    /// `true` when the first transfer starts, `false` when the last ends.
    fn keep_awake(&self, on: bool);

    fn background_capability(&self) -> BackgroundCapability;
}

/// The shell's adapter as the core's.
pub(crate) struct Bridge(pub Arc<dyn PlatformAdapter>);

fn failed(what: &str) -> core::AdapterError {
    core::AdapterError::Failed(format!("adapter returned a bad {what}"))
}

impl core::PlatformAdapter for Bridge {
    fn file_token_create(
        &self,
        path: &Path,
    ) -> Result<Option<core::FileToken>, core::AdapterError> {
        let token = self
            .0
            .file_token_create(path.to_string_lossy().into_owned())?;
        Ok(token.map(core::FileToken))
    }

    fn file_token_resolve(
        &self,
        token: &core::FileToken,
    ) -> Result<Option<core::Resolved>, core::AdapterError> {
        let resolved = self.0.file_token_resolve(token.0.clone())?;
        Ok(resolved.map(|r| core::Resolved {
            path: r.path.into(),
            token: r.token.map(core::FileToken),
        }))
    }

    fn system_proxy(&self, for_url: &Url) -> Result<core::ProxyConfig, core::AdapterError> {
        let config = self.0.system_proxy(for_url.to_string())?;
        config.try_into().map_err(|_| failed("proxy URL"))
    }

    fn network_info(&self) -> Result<core::NetworkInfo, core::AdapterError> {
        let info = self.0.network_info()?;
        Ok(core::NetworkInfo {
            interface_kind: info.interface_kind.into(),
            vpn_active: info.vpn_active,
            system_proxy: info
                .system_proxy
                .map(TryInto::try_into)
                .transpose()
                .map_err(|_| failed("proxy URL"))?,
            proxy_in_use: core::ProxyMode::None,
        })
    }

    fn keep_awake(&self, on: bool) {
        self.0.keep_awake(on);
    }

    fn background_capability(&self) -> core::BackgroundCapability {
        self.0.background_capability().into()
    }
}
