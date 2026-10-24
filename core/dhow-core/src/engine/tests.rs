use std::sync::Mutex as StdMutex;

use dhow_testserver::{TestServer, blob};

use super::*;
use crate::transfer::RetryPolicy;

struct Fx {
    _dir: tempfile::TempDir,
    downloads: PathBuf,
    engine: Engine,
    events: Arc<StdMutex<Vec<EngineEvent>>>,
}

fn fast() -> DownloadConfig {
    let mut c = DownloadConfig::default();
    c.transfer.retry = RetryPolicy {
        max_failures: 3,
        base: Duration::from_millis(1),
        cap: Duration::from_millis(5),
        retry_after_cap: Duration::from_secs(1),
    };
    c
}

impl Fx {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let downloads = dir.path().join("dl");
        std::fs::create_dir(&downloads).unwrap();
        let engine = Engine::open(&dir.path().join("data"), fast()).unwrap();
        Self {
            _dir: dir,
            downloads,
            engine,
            events: Arc::default(),
        }
    }

    fn request(&self, server: &TestServer, path: &str) -> AddRequest {
        AddRequest {
            url: Url::parse(&server.url(path)).unwrap(),
            headers: vec![],
            directory: self.downloads.clone(),
            filename: None,
            paused: false,
            connections: None,
            target_fs: TargetFs::Posix,
        }
    }

    async fn run(&self, id: &str) -> Snapshot {
        let (_keep, stop) = watch::channel(false);
        let sink = self.events.clone();
        let on_event = move |e: EngineEvent| sink.lock().unwrap().push(e);
        self.engine.run(id, &on_event, &stop).await.unwrap()
    }

    fn states(&self) -> Vec<State> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                EngineEvent::StateChanged { to, .. } => Some(*to),
                _ => None,
            })
            .collect()
    }

    fn files(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(&self.downloads)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }
}

fn disposition(name: &str) -> String {
    format!("header=Content-Disposition%3A%20attachment%3B%20filename%3D{name}")
}

#[tokio::test(flavor = "multi_thread")]
async fn add_then_run_completes_with_events_in_order() {
    let server = TestServer::start().unwrap();
    let fx = Fx::new();
    let path = format!("/blob/400000?seed=3&{}", disposition("a.bin"));
    let added = fx.engine.add(fx.request(&server, &path)).await.unwrap();
    assert_eq!(added.state, State::Queued);
    assert_eq!(added.final_path, fx.downloads.join("a.bin"));
    assert_eq!(added.total_size, Some(400_000));
    assert!(fx.files().is_empty(), "add writes nothing");

    let done = fx.run(&added.id).await;
    assert_eq!(done.state, State::Completed);
    assert_eq!(done.downloaded, 400_000);
    assert_eq!(fx.files(), ["a.bin"]);
    assert_eq!(
        blake3::hash(&std::fs::read(&done.final_path).unwrap()),
        blob::digest(3, 400_000)
    );
    assert_eq!(
        fx.states(),
        [State::Active, State::Completing, State::Completed]
    );
    let events = fx.events.lock().unwrap();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, EngineEvent::Renamed { path, .. } if *path == done.final_path))
    );
    assert!(matches!(
        events
            .iter()
            .rev()
            .find(|e| matches!(e, EngineEvent::Progress { .. })),
        Some(EngineEvent::Progress {
            downloaded: 400_000,
            total: Some(400_000),
            ..
        })
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn queued_downloads_reserve_their_names() {
    let server = TestServer::start().unwrap();
    let fx = Fx::new();
    let path = format!("/blob/10?{}", disposition("same.bin"));
    let first = fx.engine.add(fx.request(&server, &path)).await.unwrap();
    let second = fx.engine.add(fx.request(&server, &path)).await.unwrap();
    assert_eq!(first.final_path, fx.downloads.join("same.bin"));
    assert_eq!(second.final_path, fx.downloads.join("same (2).bin"));
    assert_eq!(fx.engine.list().unwrap().len(), 2);
}

/// T-I10: fails at Add, before anything is written.
#[tokio::test(flavor = "multi_thread")]
async fn too_big_for_the_disk_fails_at_add() {
    let server = TestServer::start().unwrap();
    let fx = Fx::new();
    let added = fx
        .engine
        .add(fx.request(&server, "/blob/1125899906842624"))
        .await
        .unwrap();
    assert_eq!(added.state, State::Failed);
    assert_eq!(added.fail_reason, Some(FailReason::NoSpace));
    assert!(fx.files().is_empty());
    assert!(
        server.requests().iter().all(|r| r.method == "HEAD"),
        "no body fetched"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn user_filename_is_sanitized_and_empty_rejected() {
    let server = TestServer::start().unwrap();
    let fx = Fx::new();
    let mut req = fx.request(&server, "/blob/10");
    req.filename = Some("../../.hidden".into());
    let added = fx.engine.add(req).await.unwrap();
    assert_eq!(added.final_path, fx.downloads.join("hidden"));
    let mut req = fx.request(&server, "/blob/10");
    req.filename = Some("/".into());
    assert!(matches!(
        fx.engine.add(req).await,
        Err(EngineError::InvalidName(_))
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn failures_are_recorded_with_a_reason() {
    let server = TestServer::start().unwrap();
    let fx = Fx::new();
    let added = fx
        .engine
        .add(fx.request(&server, "/blob/1000?fail=100&fail_mode=403"))
        .await
        .unwrap();
    let done = fx.run(&added.id).await;
    assert_eq!(
        (done.state, done.fail_reason),
        (State::Failed, Some(FailReason::Http(403)))
    );
    assert_eq!(
        fx.engine.get(&added.id).unwrap().fail_reason,
        Some(FailReason::Http(403))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_pauses_and_run_again_finishes() {
    let server = TestServer::start().unwrap();
    let fx = Fx::new();
    let added = fx
        .engine
        .add(fx.request(&server, "/blob/1000000?throttle=2000000"))
        .await
        .unwrap();
    let (tx, stop) = watch::channel(false);
    let pause = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        tx.send(true).unwrap();
    });
    let paused = fx.engine.run(&added.id, &|_| {}, &stop).await.unwrap();
    pause.await.unwrap();
    assert_eq!(paused.state, State::Paused);
    assert!(paused.downloaded > 0 && paused.downloaded < 1_000_000);
    let done = fx.run(&added.id).await;
    assert_eq!(done.state, State::Completed);
    assert_eq!(
        blake3::hash(&std::fs::read(&done.final_path).unwrap()),
        blob::digest(0, 1_000_000)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn pause_and_cancel_when_not_running() {
    let server = TestServer::start().unwrap();
    let fx = Fx::new();
    let a = fx
        .engine
        .add(fx.request(&server, "/blob/10"))
        .await
        .unwrap();
    assert_eq!(
        fx.engine.pause(&a.id, &|_| {}).unwrap().state,
        State::Paused
    );
    // Leave partial files behind, then cancel with deletion.
    std::fs::write(&a.part_path, b"x").unwrap();
    std::fs::write(a.sidecar_path(), b"y").unwrap();
    let c = fx.engine.cancel(&a.id, true, &|_| {}).unwrap();
    assert_eq!(c.state, State::Cancelled);
    assert!(fx.files().is_empty());
    // Running a cancelled download does nothing.
    assert_eq!(fx.run(&a.id).await.state, State::Cancelled);
}

#[tokio::test(flavor = "multi_thread")]
async fn digest_mismatch_fails_without_a_final_file() {
    let server = TestServer::start().unwrap();
    let fx = Fx::new();
    let path = "/blob/1000?header=Content-Digest%3A%20sha-256%3D%3AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA%3D%3A";
    let added = fx.engine.add(fx.request(&server, path)).await.unwrap();
    let done = fx.run(&added.id).await;
    assert_eq!(done.fail_reason, Some(FailReason::DigestMismatch));
    assert_eq!(fx.files(), ["1000.dhow", "1000.part"]);
}

/// §8: the process died between rename and sidecar removal.
#[tokio::test(flavor = "multi_thread")]
async fn completing_after_a_crash_is_finished_without_downloading() {
    let server = TestServer::start().unwrap();
    let fx = Fx::new();
    let added = fx
        .engine
        .add(fx.request(&server, "/blob/10"))
        .await
        .unwrap();
    std::fs::write(&added.final_path, blob::bytes(0, 0..10)).unwrap();
    std::fs::write(added.sidecar_path(), b"stale").unwrap();
    fx.engine
        .transition(&added.id, State::Completing, None, &|_| {})
        .unwrap();
    let gets = server.requests().len();
    let done = fx.run(&added.id).await;
    assert_eq!(done.state, State::Completed);
    assert_eq!(fx.files(), ["10"]);
    assert_eq!(server.requests().len(), gets, "nothing fetched");
}

#[tokio::test(flavor = "multi_thread")]
async fn resolve_accepts_unique_prefixes() {
    let server = TestServer::start().unwrap();
    let fx = Fx::new();
    let a = fx
        .engine
        .add(fx.request(&server, "/blob/10"))
        .await
        .unwrap();
    assert_eq!(fx.engine.resolve(&a.id).unwrap(), a.id);
    assert_eq!(fx.engine.resolve(&a.id[..30]).unwrap(), a.id);
    assert!(matches!(
        fx.engine.resolve("zzz"),
        Err(EngineError::NotFound(_))
    ));
    fx.engine
        .add(fx.request(&server, "/blob/10"))
        .await
        .unwrap();
    // Two UUIDv7s made within a millisecond share a long prefix.
    assert!(matches!(
        fx.engine.resolve(""),
        Err(EngineError::Ambiguous(_))
    ));
}

#[test]
fn fail_reasons_round_trip() {
    for r in [
        FailReason::Transient,
        FailReason::Http(404),
        FailReason::TooManyRedirects,
        FailReason::InvalidRequest,
        FailReason::NoSpace,
        FailReason::DigestMismatch,
        FailReason::ServerSentExtraData,
        FailReason::ServerKeptChanging,
        FailReason::Io,
    ] {
        assert_eq!(FailReason::parse(&r.as_db()), r);
    }
}
