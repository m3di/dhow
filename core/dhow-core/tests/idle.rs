//! T-idle (CLAUDE.md "idle means idle", ARCHITECTURE §7): once every
//! download is done, the engine has no live tasks, emits nothing, and its
//! memory stays flat. The engine lives on (as in a shell); only the
//! transfers ended.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use dhow_core::download::DownloadConfig;
use dhow_core::engine::queues::QueueSpec;
use dhow_core::engine::{AddRequest, Engine, EngineEvent, State};
use dhow_core::naming::TargetFs;
use dhow_testserver::TestServer;
use reqwest::Url;
use tokio::sync::watch;

/// How long idle is watched: the spec's 60 s, unless `DHOW_IDLE_SECS`
/// says otherwise (for quick local runs).
fn idle_for() -> Duration {
    let secs = std::env::var("DHOW_IDLE_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(60);
    Duration::from_secs(secs)
}

/// Resident set size in KiB, from `ps` (Linux and macOS alike).
fn rss_kib() -> u64 {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .expect("ps");
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .expect("rss")
}

#[test]
fn t_idle_nothing_runs_after_the_last_download() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    let server = TestServer::start().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let downloads = dir.path().join("dl");
    std::fs::create_dir(&downloads).unwrap();
    let events = Arc::new(Mutex::new(Vec::<EngineEvent>::new()));

    rt.block_on(async {
        let mut config = DownloadConfig::default();
        config.ramp.window = Duration::from_millis(300);
        config.ramp.min_chunk = 256 * 1024;
        let engine = Engine::open(&dir.path().join("data"), config).unwrap();
        // Everything that keeps state between reads: limits, headroom, a
        // queue bucket, several connections, sampling.
        engine.set_global_limit(Some(8 << 20)).unwrap();
        engine.set_headroom(10).unwrap();
        let q = engine
            .create_queue(&QueueSpec {
                name: "q".into(),
                speed_limit: Some(4 << 20),
                ..QueueSpec::default()
            })
            .unwrap();
        let mut ids = Vec::new();
        for seed in 0..4 {
            let path = format!("/blob/3000000?seed={seed}&throttle=1000000");
            let req = AddRequest {
                url: Url::parse(&server.url(&path)).unwrap(),
                headers: vec![],
                directory: downloads.clone(),
                filename: Some(format!("{seed}.bin")),
                paused: false,
                connections: None,
                queue: (seed % 2 == 0).then(|| q.id.clone()),
                speed_limit: (seed == 3).then_some(2 << 20),
                target_fs: TargetFs::host(),
            };
            ids.push(engine.add(req, &|_| {}).await.unwrap().id);
        }
        let (_keep, stop) = watch::channel(false);
        let sink = events.clone();
        let on_event = move |e: EngineEvent| sink.lock().unwrap().push(e);
        let runs = ids.iter().map(|id| engine.run(id, &on_event, &stop));
        for snap in futures_util::future::join_all(runs).await {
            assert_eq!(snap.unwrap().state, State::Completed);
        }

        let metrics = tokio::runtime::Handle::current().metrics();
        // Connections close as their tasks notice; give them a moment.
        let mut alive = metrics.num_alive_tasks();
        for _ in 0..50 {
            if alive == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            alive = metrics.num_alive_tasks();
        }
        assert_eq!(alive, 0, "tasks alive after the last download");

        let seen = events.lock().unwrap().len();
        let rss = rss_kib();
        let step = idle_for() / 6;
        for _ in 0..6 {
            tokio::time::sleep(step).await;
            assert_eq!(metrics.num_alive_tasks(), 0, "a task came back");
        }
        let extra: Vec<_> = events.lock().unwrap()[seen..].to_vec();
        assert!(extra.is_empty(), "events while idle: {extra:?}");
        let grown = rss_kib().saturating_sub(rss);
        // Flat: allow allocator noise, not growth.
        assert!(grown < 1024, "RSS grew {grown} KiB while idle");
        drop(engine);
    });
}
