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
        Self::with(fast())
    }

    fn with(config: DownloadConfig) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let downloads = dir.path().join("dl");
        std::fs::create_dir(&downloads).unwrap();
        let engine = Engine::open(&dir.path().join("data"), config).unwrap();
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
            queue: None,
            speed_limit: None,
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

/// Two downloads, one slot for their host: the second is `waiting` (in the
/// database and the events) until the first finishes.
#[tokio::test(flavor = "multi_thread")]
async fn budget_wait_is_a_visible_state() {
    let server = TestServer::start().unwrap();
    let mut config = fast();
    config.budget.per_host = 1;
    let fx = Fx::with(config);
    let a = fx
        .engine
        .add(fx.request(&server, "/blob/400000?seed=1&throttle=1000000"))
        .await
        .unwrap();
    let b = fx
        .engine
        .add(fx.request(&server, "/blob/400000?seed=2&throttle=1000000"))
        .await
        .unwrap();
    let seen_waiting = Arc::new(StdMutex::new(false));
    let engine = &fx.engine;
    let (_keep, stop) = watch::channel(false);
    let first = engine.run(&a.id, &|_| {}, &stop);
    let second = async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let flag = seen_waiting.clone();
        let b_id = b.id.clone();
        let on_event = move |e: EngineEvent| {
            if matches!(&e, EngineEvent::StateChanged { id, to: State::Waiting, .. } if *id == b_id)
            {
                *flag.lock().unwrap() = true;
            }
        };
        engine.run(&b.id, &on_event, &stop).await
    };
    let (ra, rb) = tokio::join!(first, second);
    assert_eq!(ra.unwrap().state, State::Completed);
    assert_eq!(rb.unwrap().state, State::Completed);
    assert!(*seen_waiting.lock().unwrap(), "b waited for the budget");
}

mod queue_tests {
    use super::*;
    use crate::engine::queues::{OnComplete, QueueSpec};
    use crate::storage::DEFAULT_QUEUE_ID;

    fn spec(name: &str, max: u32) -> QueueSpec {
        QueueSpec {
            name: name.into(),
            max_concurrent: max,
            ..QueueSpec::default()
        }
    }

    async fn add_to(fx: &Fx, server: &TestServer, queue: Option<&str>) -> Snapshot {
        let mut req = fx.request(server, "/blob/10");
        req.queue = queue.map(str::to_owned);
        fx.engine.add(req).await.unwrap()
    }

    fn order(fx: &Fx, queue: &str) -> Vec<String> {
        fx.engine
            .list()
            .unwrap()
            .into_iter()
            .filter(|s| s.queue_id == queue)
            .map(|s| s.id)
            .collect()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn queue_crud() {
        let server = TestServer::start().unwrap();
        let fx = Fx::new();
        let night = fx.engine.create_queue(&spec("Night", 2)).unwrap();
        assert_eq!(
            (
                night.name.as_deref(),
                night.max_concurrent,
                night.is_default
            ),
            (Some("Night"), 2, false)
        );
        add_to(&fx, &server, Some(&night.id)).await;
        let queues = fx.engine.list_queues().unwrap();
        assert_eq!(queues.len(), 2);
        assert!(
            queues[0].is_default && queues[0].name.is_none(),
            "default first, unnamed"
        );
        assert_eq!(queues[1].unfinished, 1);
        let renamed = fx
            .engine
            .update_queue(
                &night.id,
                &QueueSpec {
                    on_complete: OnComplete::Sleep,
                    ..spec("Late", 1)
                },
            )
            .unwrap();
        assert_eq!(
            (renamed.name.as_deref(), renamed.on_complete),
            (Some("Late"), OnComplete::Sleep)
        );
        assert!(matches!(
            fx.engine.create_queue(&spec(" ", 1)),
            Err(EngineError::InvalidQueue(_))
        ));
        assert!(matches!(
            fx.engine.create_queue(&spec("Zero", 0)),
            Err(EngineError::InvalidQueue(_))
        ));
        assert!(matches!(
            fx.engine.delete_queue(DEFAULT_QUEUE_ID, &night.id),
            Err(EngineError::DefaultQueue)
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn deleting_a_queue_moves_its_downloads_in_order() {
        let server = TestServer::start().unwrap();
        let fx = Fx::new();
        let q = fx.engine.create_queue(&spec("Q", 1)).unwrap();
        let first_default = add_to(&fx, &server, None).await;
        let a = add_to(&fx, &server, Some(&q.id)).await;
        let b = add_to(&fx, &server, Some(&q.id)).await;
        fx.engine.delete_queue(&q.id, DEFAULT_QUEUE_ID).unwrap();
        assert_eq!(order(&fx, DEFAULT_QUEUE_ID), [first_default.id, a.id, b.id]);
        assert!(matches!(
            fx.engine.queue(&q.id),
            Err(EngineError::NotFound(_))
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn move_and_reorder() {
        let server = TestServer::start().unwrap();
        let fx = Fx::new();
        let a = add_to(&fx, &server, None).await;
        let b = add_to(&fx, &server, None).await;
        let c = add_to(&fx, &server, None).await;
        fx.engine.reorder(&c.id, Some(&a.id)).unwrap();
        assert_eq!(
            order(&fx, DEFAULT_QUEUE_ID),
            [c.id.clone(), a.id.clone(), b.id.clone()]
        );
        fx.engine.reorder(&c.id, None).unwrap();
        assert_eq!(
            order(&fx, DEFAULT_QUEUE_ID),
            [a.id.clone(), b.id.clone(), c.id.clone()]
        );

        let q = fx.engine.create_queue(&spec("Q", 1)).unwrap();
        let moved = fx.engine.move_to_queue(&a.id, &q.id).unwrap();
        assert_eq!(moved.queue_id, q.id);
        assert!(matches!(
            fx.engine.reorder(&b.id, Some(&a.id)),
            Err(EngineError::InvalidQueue(_))
        ));
        assert!(matches!(
            fx.engine
                .add({
                    let mut r = fx.request(&server, "/blob/10");
                    r.queue = Some("nope".into());
                    r
                })
                .await,
            Err(EngineError::NotFound(_))
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn resolve_queue_by_name_default_or_prefix() {
        let fx = Fx::new();
        let q = fx.engine.create_queue(&spec("Night", 1)).unwrap();
        assert_eq!(
            fx.engine.resolve_queue("default").unwrap(),
            DEFAULT_QUEUE_ID
        );
        assert_eq!(fx.engine.resolve_queue("Night").unwrap(), q.id);
        assert_eq!(fx.engine.resolve_queue(&q.id[..30]).unwrap(), q.id);
        assert!(matches!(
            fx.engine.resolve_queue("Day"),
            Err(EngineError::NotFound(_))
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn next_runnable_respects_queue_order_and_concurrency() {
        let server = TestServer::start().unwrap();
        let fx = Fx::new();
        // Default queue: max_concurrent 3.
        let d: Vec<Snapshot> =
            futures_util::future::join_all((0..5).map(|_| add_to(&fx, &server, None))).await;
        let mut d = d;
        d.sort_by_key(|s| s.position);
        let ids: Vec<String> = d.iter().map(|s| s.id.clone()).collect();
        let night = fx.engine.create_queue(&spec("Night", 1)).unwrap();
        let n1 = add_to(&fx, &server, Some(&night.id)).await;
        let _n2 = add_to(&fx, &server, Some(&night.id)).await;

        let runnable = |running: &[&str]| -> Vec<String> {
            fx.engine
                .next_runnable(|id| running.contains(&id))
                .unwrap()
                .into_iter()
                .map(|s| s.id)
                .collect()
        };
        assert_eq!(
            runnable(&[]),
            [
                ids[0].clone(),
                ids[1].clone(),
                ids[2].clone(),
                n1.id.clone()
            ]
        );
        // Two running in the default queue leave room for one more.
        assert_eq!(runnable(&[&ids[0], &ids[1], &n1.id]), [ids[2].clone()]);
        // Paused and failed don't run; left "active" by a dead process does.
        fx.engine.pause(&ids[2], &|_| {}).unwrap();
        fx.engine
            .transition(&ids[3], State::Failed, Some(FailReason::Transient), &|_| {})
            .unwrap();
        fx.engine
            .transition(&ids[4], State::Active, None, &|_| {})
            .unwrap();
        assert_eq!(runnable(&[&ids[0], &ids[1], &n1.id]), [ids[4].clone()]);
    }
}

mod schedule_tests {
    use jiff::Zoned;
    use jiff::tz::TimeZone;

    use super::*;
    use crate::engine::queues::{QueueSpec, RunState};
    use crate::schedule::{Action, Schedule, Window};
    use crate::storage::DEFAULT_QUEUE_ID;

    // 2026-10-26 is a Monday.
    fn utc(s: &str) -> Zoned {
        s.parse::<jiff::civil::DateTime>()
            .unwrap()
            .to_zoned(TimeZone::UTC)
            .unwrap()
    }

    fn scheduled(fx: &Fx, action: Action) -> String {
        let spec = QueueSpec {
            name: "Office".into(),
            schedule: Some(Schedule {
                windows: vec![Window::parse("mon 09:00-17:00").unwrap()],
                action,
            }),
            ..QueueSpec::default()
        };
        fx.engine.create_queue(&spec).unwrap().id
    }

    async fn add_to(fx: &Fx, server: &TestServer, queue: &str) -> String {
        let mut req = fx.request(server, "/blob/10");
        req.queue = Some(queue.into());
        fx.engine.add(req).await.unwrap().id
    }

    fn runnable(fx: &Fx, now: &Zoned) -> Vec<String> {
        fx.engine
            .next_runnable_at(now, |_| false)
            .unwrap()
            .into_iter()
            .map(|s| s.id)
            .collect()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn windows_gate_starting_and_stop_and_start_outside_stops_running() {
        let server = TestServer::start().unwrap();
        let fx = Fx::new();
        let q = scheduled(&fx, Action::StartAndStopOutside);
        let dl = add_to(&fx, &server, &q).await;
        let other = fx
            .engine
            .add(fx.request(&server, "/blob/10"))
            .await
            .unwrap()
            .id;
        assert_eq!(
            runnable(&fx, &utc("2026-10-26T10:00")),
            [other.clone(), dl.clone()]
        );
        assert_eq!(
            runnable(&fx, &utc("2026-10-26T18:00")),
            vec![other.clone()],
            "default queue unaffected"
        );
        assert_eq!(
            fx.engine
                .to_stop_at(&utc("2026-10-26T18:00"), [dl.as_str(), other.as_str()])
                .unwrap(),
            vec![dl.clone()]
        );
        assert!(
            fx.engine
                .to_stop_at(&utc("2026-10-26T10:00"), [dl.as_str()])
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn start_lets_running_downloads_continue_past_the_end() {
        let server = TestServer::start().unwrap();
        let fx = Fx::new();
        let q = scheduled(&fx, Action::Start);
        let dl = add_to(&fx, &server, &q).await;
        assert!(
            runnable(&fx, &utc("2026-10-26T18:00")).is_empty(),
            "nothing new starts"
        );
        assert!(
            fx.engine
                .to_stop_at(&utc("2026-10-26T18:00"), [dl.as_str()])
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn one_next_edge_across_queues_and_none_without_schedules() {
        let fx = Fx::new();
        assert_eq!(
            fx.engine.next_edge_at(&utc("2026-10-26T10:00")).unwrap(),
            None
        );
        scheduled(&fx, Action::Start);
        let evening = QueueSpec {
            name: "Evening".into(),
            schedule: Some(Schedule {
                windows: vec![Window::parse("mon 12:00-13:00").unwrap()],
                action: Action::Start,
            }),
            ..QueueSpec::default()
        };
        fx.engine.create_queue(&evening).unwrap();
        let edge = |at: &str| fx.engine.next_edge_at(&utc(at)).unwrap().unwrap();
        assert_eq!(
            edge("2026-10-26T10:00"),
            utc("2026-10-26T12:00").timestamp()
        );
        assert_eq!(
            edge("2026-10-26T12:30"),
            utc("2026-10-26T13:00").timestamp()
        );
        assert_eq!(
            edge("2026-10-26T13:00"),
            utc("2026-10-26T17:00").timestamp()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn overrides_last_until_the_next_edge() {
        let server = TestServer::start().unwrap();
        let fx = Fx::new();
        let q = scheduled(&fx, Action::StartAndStopOutside);
        let dl = add_to(&fx, &server, &q).await;
        // StartQueue on Monday evening: runs until the next edge (next Monday 09:00).
        let snap = fx
            .engine
            .set_run_state(&q, RunState::Started, &utc("2026-10-26T18:00"))
            .unwrap();
        assert_eq!(
            snap.run_state_until,
            Some(utc("2026-11-02T09:00").timestamp().as_millisecond())
        );
        assert_eq!(runnable(&fx, &utc("2026-10-28T18:00")), vec![dl.clone()]);
        // StopQueue Monday morning: stopped, and running ones stop, until 17:00.
        fx.engine
            .set_run_state(&q, RunState::Stopped, &utc("2026-10-26T10:00"))
            .unwrap();
        assert!(runnable(&fx, &utc("2026-10-26T11:00")).is_empty());
        assert_eq!(
            fx.engine
                .to_stop_at(&utc("2026-10-26T11:00"), [dl.as_str()])
                .unwrap(),
            vec![dl.clone()]
        );
        // After 17:00 the schedule rules again (outside: still not running).
        assert!(runnable(&fx, &utc("2026-10-26T17:30")).is_empty());
        assert!(
            !runnable(&fx, &utc("2026-11-02T10:00")).is_empty(),
            "next window as usual"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stopping_an_unscheduled_queue_lasts_until_started() {
        let server = TestServer::start().unwrap();
        let fx = Fx::new();
        let dl = fx
            .engine
            .add(fx.request(&server, "/blob/10"))
            .await
            .unwrap()
            .id;
        let snap = fx
            .engine
            .set_run_state(
                DEFAULT_QUEUE_ID,
                RunState::Stopped,
                &utc("2026-10-26T10:00"),
            )
            .unwrap();
        assert_eq!(snap.run_state_until, None);
        assert!(runnable(&fx, &utc("2027-01-01T00:00")).is_empty());
        assert_eq!(
            fx.engine.next_edge_at(&utc("2026-10-26T10:00")).unwrap(),
            None,
            "no timer for it"
        );
        fx.engine
            .set_run_state(DEFAULT_QUEUE_ID, RunState::Auto, &utc("2026-10-26T10:00"))
            .unwrap();
        assert_eq!(runnable(&fx, &utc("2026-10-26T10:00")), vec![dl]);
    }

    #[test]
    fn default_queue_has_no_schedule_and_schedules_need_windows() {
        let fx = Fx::new();
        let with = |windows| QueueSpec {
            name: "x".into(),
            schedule: Some(Schedule {
                windows,
                action: Action::Start,
            }),
            ..QueueSpec::default()
        };
        assert!(matches!(
            fx.engine.update_queue(
                DEFAULT_QUEUE_ID,
                &with(vec![Window::parse("daily 01:00-02:00").unwrap()])
            ),
            Err(EngineError::InvalidQueue(_))
        ));
        assert!(matches!(
            fx.engine.create_queue(&with(vec![])),
            Err(EngineError::InvalidQueue(_))
        ));
        let q = fx
            .engine
            .create_queue(&with(vec![Window::parse("daily 01:00-02:00").unwrap()]))
            .unwrap();
        assert_eq!(
            fx.engine
                .queue(&q.id)
                .unwrap()
                .schedule
                .unwrap()
                .windows
                .len(),
            1,
            "round-trips"
        );
    }
}

mod limit_tests {
    use super::*;
    use crate::engine::limits::LimitSettings;
    use crate::engine::queues::QueueSpec;

    #[tokio::test(flavor = "multi_thread")]
    async fn limits_are_stored() {
        let server = TestServer::start().unwrap();
        let fx = Fx::new();
        assert_eq!(
            fx.engine.limit_settings().unwrap(),
            LimitSettings::default()
        );
        fx.engine.set_global_limit(Some(1_000_000)).unwrap();
        let s = fx.engine.set_headroom(20).unwrap();
        assert_eq!(
            s,
            LimitSettings {
                global: Some(1_000_000),
                headroom_percent: 20
            }
        );
        assert!(matches!(
            fx.engine.set_headroom(100),
            Err(EngineError::InvalidSetting(_))
        ));
        assert_eq!(fx.engine.shared.global.rate(), Some(1_000_000));
        fx.engine.set_global_limit(None).unwrap();
        assert_eq!(fx.engine.limit_settings().unwrap().global, None);
        assert_eq!(fx.engine.shared.global.rate(), None);

        let mut req = fx.request(&server, "/blob/10");
        req.speed_limit = Some(5_000);
        let added = fx.engine.add(req).await.unwrap();
        assert_eq!(added.speed_limit, Some(5_000));
        let snap = fx.engine.set_speed_limit(&added.id, Some(0)).unwrap();
        assert_eq!(snap.speed_limit, Some(0));
        assert!(matches!(
            fx.engine.set_speed_limit("nope", None),
            Err(EngineError::NotFound(_))
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_queue_limit_is_shared_by_its_downloads() {
        let server = TestServer::start().unwrap();
        let fx = Fx::new();
        let q = fx
            .engine
            .create_queue(&QueueSpec {
                name: "slow".into(),
                speed_limit: Some(200_000),
                ..QueueSpec::default()
            })
            .unwrap();
        let mut ids = Vec::new();
        for seed in 0..2 {
            let mut req = fx.request(&server, &format!("/blob/300000?seed={seed}"));
            req.queue = Some(q.id.clone());
            ids.push(fx.engine.add(req).await.unwrap().id);
        }
        let started = Instant::now();
        let (a, b) = tokio::join!(fx.run(&ids[0]), fx.run(&ids[1]));
        assert_eq!((a.state, b.state), (State::Completed, State::Completed));
        // 600 kB at 200 kB/s together, less the 40 kB burst.
        let secs = started.elapsed().as_secs_f64();
        assert!((2.6..3.3).contains(&secs), "{secs} s");
    }

    /// §12: 0 is "paused by limit"; lifting it applies at once.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_limit_changes_while_running() {
        let server = TestServer::start().unwrap();
        let fx = Fx::new();
        let mut req = fx.request(&server, "/blob/2000000");
        req.speed_limit = Some(0);
        let id = fx.engine.add(req).await.unwrap().id;
        let run = fx.run(&id);
        let lift = async {
            tokio::time::sleep(Duration::from_millis(500)).await;
            // Still active, with almost nothing through (one read before
            // the first wait).
            let p = fx
                .events
                .lock()
                .unwrap()
                .iter()
                .any(|e| matches!(e, EngineEvent::Progress { downloaded, .. } if *downloaded > 64 * 1024));
            assert!(!p, "paused by limit");
            assert_eq!(fx.engine.get(&id).unwrap().state, State::Active);
            fx.engine.set_speed_limit(&id, None).unwrap();
            Instant::now()
        };
        let (done, lifted) = tokio::join!(run, lift);
        assert_eq!(done.state, State::Completed);
        assert!(
            lifted.elapsed() < Duration::from_secs(2),
            "{:?}",
            lifted.elapsed()
        );
    }
}
