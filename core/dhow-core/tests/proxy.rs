//! Proxy modes (ARCHITECTURE §8). A second test server stands in for the
//! proxy: it answers absolute-form requests itself.

use std::path::Path;
use std::sync::{Arc, Mutex};

use dhow_core::download::DownloadConfig;
use dhow_core::engine::settings::{Settings, SettingsPatch};
use dhow_core::engine::{AddRequest, Engine, EngineError, State};
use dhow_core::naming::TargetFs;
use dhow_core::platform::{
    AdapterError, InterfaceKind, NetworkInfo, PlatformAdapter, ProxyConfig, ProxyMode,
};
use dhow_testserver::{SocksProxy, TestServer};
use reqwest::Url;
use tokio::sync::watch;

fn set_proxy(engine: &Engine, mode: ProxyMode) -> Result<Settings, EngineError> {
    let patch = SettingsPatch {
        proxy_mode: Some(mode),
        ..SettingsPatch::default()
    };
    engine.set_settings(patch, &|_| {})
}

fn proxied(proxy: &TestServer) -> usize {
    proxy
        .requests()
        .iter()
        .filter(|r| r.target.starts_with("http://"))
        .count()
}

fn custom(proxy: &TestServer) -> ProxyMode {
    ProxyMode::Custom(Box::new(ProxyConfig {
        http: Url::parse(&format!("http://{}", proxy.addr())).ok(),
        ..ProxyConfig::default()
    }))
}

async fn fetch(engine: &Engine, origin: &TestServer, dir: &Path, seed: u64) {
    let req = AddRequest {
        url: Url::parse(&origin.url(&format!("/blob/100000?seed={seed}"))).unwrap(),
        headers: vec![],
        directory: dir.to_owned(),
        filename: Some(format!("{seed}.bin")),
        paused: false,
        connections: Some(1),
        queue: None,
        speed_limit: None,
        target_fs: TargetFs::host(),
        blind: false,
    };
    let id = engine.add(req, &|_| {}).await.unwrap().id;
    let (_keep, stop) = watch::channel(false);
    let snap = engine.run(&id, &|_| {}, &stop).await.unwrap();
    assert_eq!(snap.state, State::Completed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn custom_routes_through_the_proxy_none_bypasses_it() {
    let origin = TestServer::start().unwrap();
    let proxy = TestServer::start().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(&dir.path().join("data"), DownloadConfig::default()).unwrap();
    assert_eq!(engine.settings().unwrap().proxy_mode, ProxyMode::System);

    set_proxy(&engine, custom(&proxy)).unwrap();
    fetch(&engine, &origin, dir.path(), 1).await;
    assert!(origin.requests().is_empty(), "went direct");
    let target = format!("http://{}/blob/100000?seed=1", origin.addr());
    let seen = proxy.requests();
    assert!(
        !seen.is_empty() && seen.iter().all(|r| r.target == target),
        "{seen:?}"
    );

    // Takes effect at the next download, the client being rebuilt.
    set_proxy(&engine, ProxyMode::None).unwrap();
    fetch(&engine, &origin, dir.path(), 2).await;
    assert!(!origin.requests().is_empty());
    assert_eq!(proxy.requests().len(), seen.len(), "went through the proxy");

    // Stored: another engine on the same data sees it.
    set_proxy(&engine, custom(&proxy)).unwrap();
    let other = Engine::open(&dir.path().join("data"), DownloadConfig::default()).unwrap();
    assert_eq!(other.settings().unwrap().proxy_mode, custom(&proxy));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_change_while_running_applies_to_the_next_lease() {
    let origin = TestServer::start().unwrap();
    let proxy = TestServer::start().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(&dir.path().join("data"), DownloadConfig::default()).unwrap();
    set_proxy(&engine, ProxyMode::None).unwrap();
    let req = AddRequest {
        url: Url::parse(&origin.url("/blob/400000?seed=5&throttle=1000000")).unwrap(),
        headers: vec![],
        directory: dir.path().to_owned(),
        filename: Some("slow.bin".into()),
        paused: false,
        connections: Some(1),
        queue: None,
        speed_limit: None,
        target_fs: TargetFs::host(),
        blind: false,
    };
    let id = engine.add(req, &|_| {}).await.unwrap().id;
    let probed = origin.requests().len();
    let (_keep, stop) = watch::channel(false);
    let switched = async {
        while origin.requests().len() == probed {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        set_proxy(&engine, custom(&proxy)).unwrap();
        // The running download keeps its client; this one gets the new.
        fetch(&engine, &origin, dir.path(), 6).await;
    };
    let (snap, ()) = tokio::join!(engine.run(&id, &|_| {}, &stop), switched);
    assert_eq!(snap.unwrap().state, State::Completed);
    let direct: Vec<_> = origin.requests().into_iter().map(|r| r.target).collect();
    assert!(direct.iter().all(|t| t.contains("seed=5")), "{direct:?}");
    assert!(proxied(&proxy) > 0);
    assert!(proxy.requests().iter().all(|r| r.target.contains("seed=6")));
}

#[test]
fn custom_needs_a_usable_proxy() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path(), DownloadConfig::default()).unwrap();
    let url = |u: &str| Url::parse(u).ok();
    for bad in [
        ProxyConfig::default(),
        ProxyConfig {
            all: url("socks4://p.test:1080"),
            ..ProxyConfig::default()
        },
        ProxyConfig {
            http: url("http://p.test:1"),
            pac: url("http://p.test/p.pac"),
            ..ProxyConfig::default()
        },
    ] {
        let r = set_proxy(&engine, ProxyMode::Custom(Box::new(bad.clone())));
        assert!(
            matches!(r, Err(EngineError::InvalidSetting { .. })),
            "{bad:?}"
        );
    }
    assert_eq!(engine.settings().unwrap().proxy_mode, ProxyMode::System);
}

/// A platform whose system proxy is a test server, asked per URL.
struct Fake {
    proxy: ProxyConfig,
    asked: Mutex<Vec<Url>>,
}

impl PlatformAdapter for Fake {
    fn system_proxy(&self, for_url: &Url) -> Result<ProxyConfig, AdapterError> {
        self.asked.lock().unwrap().push(for_url.clone());
        Ok(self.proxy.clone())
    }

    fn network_info(&self) -> Result<NetworkInfo, AdapterError> {
        Ok(NetworkInfo {
            interface_kind: InterfaceKind::Ethernet,
            vpn_active: false,
            system_proxy: Some(self.proxy.clone()),
            proxy_in_use: ProxyMode::None,
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn system_uses_the_adapters_proxy_and_its_bypass_list() {
    let origin = TestServer::start().unwrap();
    let proxy = TestServer::start().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut config = ProxyConfig {
        http: Url::parse(&format!("http://{}", proxy.addr())).ok(),
        ..ProxyConfig::default()
    };
    let fake = Arc::new(Fake {
        proxy: config.clone(),
        asked: Mutex::default(),
    });
    let engine = Engine::open_with(
        &dir.path().join("data"),
        DownloadConfig::default(),
        fake.clone(),
    )
    .unwrap();
    assert_eq!(
        engine.network_info().unwrap().proxy_in_use,
        ProxyMode::System
    );
    fetch(&engine, &origin, dir.path(), 1).await;
    assert!(origin.requests().is_empty(), "went direct");
    assert!(proxied(&proxy) > 0);
    let asked = fake.asked.lock().unwrap().clone();
    assert!(!asked.is_empty() && asked.iter().all(|u| u.port() == Some(origin.addr().port())));

    // Through `no_proxy`, the same system goes direct.
    config.no_proxy = vec!["127.0.0.1".into()];
    let fake = Arc::new(Fake {
        proxy: config,
        asked: Mutex::default(),
    });
    let engine =
        Engine::open_with(&dir.path().join("data"), DownloadConfig::default(), fake).unwrap();
    let before = proxy.requests().len();
    fetch(&engine, &origin, dir.path(), 2).await;
    assert!(!origin.requests().is_empty());
    assert_eq!(proxy.requests().len(), before);

    // `None` overrides the system.
    set_proxy(&engine, ProxyMode::None).unwrap();
    assert_eq!(engine.network_info().unwrap().proxy_in_use, ProxyMode::None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn socks5_proxies_relay_with_and_without_auth() {
    let origin = TestServer::start().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(&dir.path().join("data"), DownloadConfig::default()).unwrap();
    let target = origin.addr().to_string();

    let open = SocksProxy::start(None).unwrap();
    let all = Url::parse(&format!("socks5://{}", open.addr())).ok();
    set_proxy(
        &engine,
        ProxyMode::Custom(Box::new(ProxyConfig {
            all,
            ..ProxyConfig::default()
        })),
    )
    .unwrap();
    fetch(&engine, &origin, dir.path(), 1).await;
    let connects = open.connects();
    assert!(
        !connects.is_empty()
            && connects
                .iter()
                .all(|c| c.target == target && c.user.is_none())
    );

    let authed = SocksProxy::start(Some(("u", "s3cret"))).unwrap();
    let all = Url::parse(&format!("socks5h://u:s3cret@{}", authed.addr())).ok();
    set_proxy(
        &engine,
        ProxyMode::Custom(Box::new(ProxyConfig {
            all,
            ..ProxyConfig::default()
        })),
    )
    .unwrap();
    fetch(&engine, &origin, dir.path(), 2).await;
    let connects = authed.connects();
    assert!(!connects.is_empty(), "not relayed");
    assert!(
        connects.iter().all(|c| c.user.as_deref() == Some("u")),
        "{connects:?}"
    );
    // Origin-form requests: the origin saw them, through the relay.
    assert!(origin.requests().iter().all(|r| r.target.starts_with('/')));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_socks_only_system_proxy_is_used() {
    let origin = TestServer::start().unwrap();
    let socks = SocksProxy::start(None).unwrap();
    let dir = tempfile::tempdir().unwrap();
    // As from `all_proxy`, GNOME's socks keys or KDE's `socksProxy`.
    let fake = Arc::new(Fake {
        proxy: ProxyConfig {
            all: Url::parse(&format!("socks5://{}", socks.addr())).ok(),
            ..ProxyConfig::default()
        },
        asked: Mutex::default(),
    });
    let engine =
        Engine::open_with(&dir.path().join("data"), DownloadConfig::default(), fake).unwrap();
    fetch(&engine, &origin, dir.path(), 1).await;
    assert!(!socks.connects().is_empty(), "went direct");
}

/// A system proxy that answers until told to fail.
struct Flaky {
    proxy: ProxyConfig,
    fail: std::sync::atomic::AtomicBool,
}

impl PlatformAdapter for Flaky {
    fn system_proxy(&self, _for_url: &Url) -> Result<ProxyConfig, AdapterError> {
        if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(AdapterError::Failed("gsettings did not answer".into()));
        }
        Ok(self.proxy.clone())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failing_adapter_keeps_the_last_known_proxy() {
    let origin = TestServer::start().unwrap();
    let proxy = TestServer::start().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let flaky = Arc::new(Flaky {
        proxy: ProxyConfig {
            http: Url::parse(&format!("http://{}", proxy.addr())).ok(),
            ..ProxyConfig::default()
        },
        fail: true.into(),
    });
    let engine = Engine::open_with(
        &dir.path().join("data"),
        DownloadConfig::default(),
        flaky.clone(),
    )
    .unwrap();
    // Nothing known yet: direct, with a warning.
    fetch(&engine, &origin, dir.path(), 1).await;
    assert_eq!(proxied(&proxy), 0);
    let direct = origin.requests().len();

    flaky.fail.store(false, std::sync::atomic::Ordering::SeqCst);
    fetch(&engine, &origin, dir.path(), 2).await;
    let via = proxied(&proxy);
    assert!(via > 0);

    // Failing again, after idle dropped the client: still the proxy.
    flaky.fail.store(true, std::sync::atomic::Ordering::SeqCst);
    fetch(&engine, &origin, dir.path(), 3).await;
    assert!(proxied(&proxy) > via, "went direct");
    assert_eq!(origin.requests().len(), direct);
}
