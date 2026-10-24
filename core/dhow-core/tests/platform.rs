//! The engine and its platform adapter (ARCHITECTURE §4, §8).

use std::sync::{Arc, Mutex};

use dhow_core::download::DownloadConfig;
use dhow_core::engine::{AddRequest, Engine, State};
use dhow_core::naming::TargetFs;
use dhow_core::platform::{
    AdapterError, BackgroundCapability, InterfaceKind, NetworkInfo, NoopAdapter, PlatformAdapter,
    ProxyConfig, ProxyMode,
};
use dhow_testserver::TestServer;
use reqwest::Url;
use tokio::sync::watch;

#[derive(Default)]
struct Fake {
    awake: Mutex<Vec<bool>>,
}

impl PlatformAdapter for Fake {
    fn network_info(&self) -> Result<NetworkInfo, AdapterError> {
        Ok(NetworkInfo {
            interface_kind: InterfaceKind::Wifi,
            vpn_active: true,
            system_proxy: Some(ProxyConfig {
                https: Url::parse("http://proxy.test:3128").ok(),
                ..ProxyConfig::default()
            }),
            // Not the adapter's to say.
            proxy_in_use: ProxyMode::System,
        })
    }

    fn keep_awake(&self, on: bool) {
        self.awake.lock().unwrap().push(on);
    }
}

#[test]
fn noop_defaults() {
    let a = NoopAdapter;
    let url = Url::parse("http://x/").unwrap();
    assert_eq!(a.system_proxy(&url), Err(AdapterError::Unsupported));
    assert_eq!(a.network_info(), Err(AdapterError::Unsupported));
    assert_eq!(
        a.watch_network(Box::new(|_| {})),
        Err(AdapterError::Unsupported)
    );
    assert_eq!(
        a.file_token_create("/x".as_ref()),
        Err(AdapterError::Unsupported)
    );
    assert_eq!(
        a.background_capability(),
        BackgroundCapability::Unrestricted
    );
    a.keep_awake(true);

    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path(), DownloadConfig::default()).unwrap();
    assert_eq!(engine.network_info(), Err(AdapterError::Unsupported));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn engine_reports_the_adapter_and_keeps_awake_while_transferring() {
    let server = TestServer::start().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let engine = Engine::open_with(
        &dir.path().join("data"),
        DownloadConfig::default(),
        fake.clone(),
    )
    .unwrap();

    let info = engine.network_info().unwrap();
    assert_eq!(info.interface_kind, InterfaceKind::Wifi);
    assert!(info.vpn_active);
    assert_eq!(
        info.system_proxy.unwrap().https.unwrap().as_str(),
        "http://proxy.test:3128/"
    );
    // The default mode, with a system proxy reported.
    assert_eq!(info.proxy_in_use, ProxyMode::System);
    assert!(fake.awake.lock().unwrap().is_empty(), "nothing ran yet");

    let mut ids = Vec::new();
    for seed in 0..2 {
        let req = AddRequest {
            url: Url::parse(&server.url(&format!("/blob/200000?seed={seed}&throttle=1000000")))
                .unwrap(),
            headers: vec![],
            directory: dir.path().to_owned(),
            filename: Some(format!("{seed}.bin")),
            paused: false,
            connections: Some(1),
            queue: None,
            speed_limit: None,
            target_fs: TargetFs::host(),
        };
        ids.push(engine.add(req, &|_| {}).await.unwrap().id);
    }
    let (_keep, stop) = watch::channel(false);
    let runs = ids.iter().map(|id| engine.run(id, &|_| {}, &stop));
    for snap in futures_util::future::join_all(runs).await {
        assert_eq!(snap.unwrap().state, State::Completed);
    }
    // On at the first start, off at the last end; how often depends on
    // whether the two overlapped.
    let awake = fake.awake.lock().unwrap().clone();
    assert!(!awake.is_empty() && awake.len() % 2 == 0, "{awake:?}");
    assert!(
        awake.iter().enumerate().all(|(i, on)| *on == (i % 2 == 0)),
        "{awake:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keep_awake_ends_when_a_run_is_dropped() {
    let server = TestServer::start().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let engine = Engine::open_with(
        &dir.path().join("data"),
        DownloadConfig::default(),
        fake.clone(),
    )
    .unwrap();
    let req = AddRequest {
        url: Url::parse(&server.url("/blob/10000000?throttle=100000")).unwrap(),
        headers: vec![],
        directory: dir.path().to_owned(),
        filename: Some("slow.bin".into()),
        paused: false,
        connections: Some(1),
        queue: None,
        speed_limit: None,
        target_fs: TargetFs::host(),
    };
    let id = engine.add(req, &|_| {}).await.unwrap().id;
    let (_keep, stop) = watch::channel(false);
    let started = async {
        while fake.awake.lock().unwrap().is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    };
    // The run future is dropped mid-transfer, as a shell or a `?` would.
    tokio::select! {
        r = engine.run(&id, &|_| {}, &stop) => panic!("finished: {r:?}"),
        _ = started => {}
    }
    assert_eq!(*fake.awake.lock().unwrap(), [true, false]);
}
