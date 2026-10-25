//! What the FFI tests share: a listener that records, an adapter that
//! records, and a way to await the engine's async calls the way a foreign
//! executor does (outside the engine's runtime).

#![allow(dead_code)]

use std::future::Future;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use dhow_ffi::*;

#[derive(Default)]
pub struct Recorder {
    events: Mutex<Vec<Event>>,
    cv: Condvar,
}

impl EventListener for Recorder {
    fn on_event(&self, event: Event) {
        self.events.lock().unwrap().push(event);
        self.cv.notify_all();
    }
}

impl Recorder {
    pub fn events(&self) -> Vec<Event> {
        self.events.lock().unwrap().clone()
    }

    pub fn len(&self) -> usize {
        self.events.lock().unwrap().len()
    }

    /// Waits until `pred` holds for the events so far.
    pub fn wait(&self, what: &str, pred: impl Fn(&[Event]) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut events = self.events.lock().unwrap();
        while !pred(&events) {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "timed out waiting for {what}: {events:#?}");
            events = self.cv.wait_timeout(events, left).unwrap().0;
        }
    }

    pub fn wait_state(&self, id: &str, state: DownloadState) {
        self.wait(&format!("{id} {state:?}"), |es| {
            es.iter().any(|e| is_state(e, id, state))
        });
    }

    pub fn wait_progress(&self, id: &str) {
        self.wait("progress", |es| {
            es.iter().any(
                |e| matches!(e, Event::Progress { id: i, downloaded, .. } if i == id && *downloaded > 0),
            )
        });
    }
}

pub fn is_state(e: &Event, id: &str, state: DownloadState) -> bool {
    matches!(e, Event::StateChanged { id: i, to, .. } if i == id && *to == state)
}

#[derive(Default)]
pub struct Adapter {
    pub awake: Mutex<Vec<bool>>,
    pub tokens: Mutex<Vec<String>>,
}

impl PlatformAdapter for Adapter {
    fn file_token_create(&self, path: String) -> Result<Option<Vec<u8>>, AdapterError> {
        self.tokens.lock().unwrap().push(path.clone());
        Ok(Some(path.into_bytes()))
    }

    fn file_token_resolve(&self, token: Vec<u8>) -> Result<Option<ResolvedFile>, AdapterError> {
        let path = String::from_utf8(token).unwrap();
        Ok(Path::new(&path)
            .exists()
            .then_some(ResolvedFile { path, token: None }))
    }

    fn system_proxy(&self, _for_url: String) -> Result<ProxyConfig, AdapterError> {
        Ok(ProxyConfig::default())
    }

    fn network_info(&self) -> Result<AdapterNetworkInfo, AdapterError> {
        Ok(AdapterNetworkInfo {
            interface_kind: InterfaceKind::Wifi,
            vpn_active: false,
            system_proxy: None,
        })
    }

    fn keep_awake(&self, on: bool) {
        self.awake.lock().unwrap().push(on);
    }

    fn background_capability(&self) -> BackgroundCapability {
        BackgroundCapability::Unrestricted
    }
}

pub fn open(data: &Path, listener: Arc<Recorder>, adapter: Option<Arc<Adapter>>) -> Arc<Engine> {
    let config = EngineConfig {
        rampup_window_ms: Some(300),
        min_chunk_bytes: Some(256 * 1024),
        ..EngineConfig::default()
    };
    Engine::open(
        data.to_string_lossy().into_owned(),
        config,
        listener,
        adapter.map(|a| a as Arc<dyn PlatformAdapter>),
    )
    .unwrap()
}

/// Polls `f` on a thread of its own, as Swift's executor would.
pub fn block<F: Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(f)
}

pub fn request(url: String, dir: &Path, name: &str, start: StartMode) -> AddRequest {
    AddRequest {
        url,
        headers: vec![],
        directory: dir.to_string_lossy().into_owned(),
        filename: Some(name.into()),
        start,
        connections: None,
        queue: None,
        speed_limit: None,
        target_fs: None,
        blind: false,
    }
}
