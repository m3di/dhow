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
        Self::with_adapter(config, Arc::new(NoopAdapter))
    }

    fn with_adapter(config: DownloadConfig, platform: Arc<dyn PlatformAdapter>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let downloads = dir.path().join("dl");
        std::fs::create_dir(&downloads).unwrap();
        let engine = Engine::open_with(&dir.path().join("data"), config, platform).unwrap();
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
            blind: false,
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
    let added = fx
        .engine
        .add(fx.request(&server, &path), &|_| {})
        .await
        .unwrap();
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
    let first = fx
        .engine
        .add(fx.request(&server, &path), &|_| {})
        .await
        .unwrap();
    let second = fx
        .engine
        .add(fx.request(&server, &path), &|_| {})
        .await
        .unwrap();
    assert_eq!(first.final_path, fx.downloads.join("same.bin"));
    assert_eq!(second.final_path, fx.downloads.join("same (2).bin"));
    assert_eq!(fx.engine.list(&|_| {}).unwrap().len(), 2);
}

/// T-I10: fails at Add, before anything is written.
#[tokio::test(flavor = "multi_thread")]
async fn too_big_for_the_disk_fails_at_add() {
    let server = TestServer::start().unwrap();
    let fx = Fx::new();
    let added = fx
        .engine
        .add(fx.request(&server, "/blob/1125899906842624"), &|_| {})
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
    let added = fx.engine.add(req, &|_| {}).await.unwrap();
    assert_eq!(added.final_path, fx.downloads.join("hidden"));
    let mut req = fx.request(&server, "/blob/10");
    req.filename = Some("/".into());
    assert!(matches!(
        fx.engine.add(req, &|_| {}).await,
        Err(EngineError::InvalidName(_))
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn failures_are_recorded_with_a_reason() {
    let server = TestServer::start().unwrap();
    let fx = Fx::new();
    let added = fx
        .engine
        .add(
            fx.request(&server, "/blob/1000?fail=100&fail_mode=403"),
            &|_| {},
        )
        .await
        .unwrap();
    let done = fx.run(&added.id).await;
    assert_eq!(
        (done.state, done.fail_reason),
        (State::Failed, Some(FailReason::Http(403)))
    );
    assert_eq!(
        fx.engine.get(&added.id, &|_| {}).unwrap().fail_reason,
        Some(FailReason::Http(403))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_pauses_and_run_again_finishes() {
    let server = TestServer::start().unwrap();
    let fx = Fx::new();
    let added = fx
        .engine
        .add(
            fx.request(&server, "/blob/1000000?throttle=1000000"),
            &|_| {},
        )
        .await
        .unwrap();
    // Stopped once bytes flow, whenever that is, with about a second of
    // data still to come.
    let (tx, stop) = watch::channel(false);
    let on_event = move |e: EngineEvent| {
        if matches!(e, EngineEvent::Progress { downloaded, .. } if downloaded > 0) {
            tx.send_replace(true);
        }
    };
    let paused = fx.engine.run(&added.id, &on_event, &stop).await.unwrap();
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
        .add(fx.request(&server, "/blob/10"), &|_| {})
        .await
        .unwrap();
    assert_eq!(
        fx.engine.pause(&a.id, &|_| {}).unwrap().state,
        State::Paused
    );
    // Leave partial files behind, then cancel with deletion.
    std::fs::write(&a.part_path, b"x").unwrap();
    std::fs::write(a.sidecar_path(), b"y").unwrap();
    let c = fx.engine.cancel(&a.id, true, &|_| {}).await.unwrap();
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
    let added = fx
        .engine
        .add(fx.request(&server, path), &|_| {})
        .await
        .unwrap();
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
        .add(fx.request(&server, "/blob/10"), &|_| {})
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
        .add(fx.request(&server, "/blob/10"), &|_| {})
        .await
        .unwrap();
    assert_eq!(fx.engine.resolve(&a.id).unwrap(), a.id);
    assert_eq!(fx.engine.resolve(&a.id[..30]).unwrap(), a.id);
    assert!(matches!(
        fx.engine.resolve("zzz"),
        Err(EngineError::NotFound(_))
    ));
    fx.engine
        .add(fx.request(&server, "/blob/10"), &|_| {})
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
        .add(
            fx.request(&server, "/blob/400000?seed=1&throttle=400000"),
            &|_| {},
        )
        .await
        .unwrap();
    let b = fx
        .engine
        .add(
            fx.request(&server, "/blob/400000?seed=2&throttle=1000000"),
            &|_| {},
        )
        .await
        .unwrap();
    let seen_waiting = Arc::new(StdMutex::new(false));
    let engine = &fx.engine;
    let (_keep, stop) = watch::channel(false);
    // b starts once a has the slot (bytes flowing), not on a timer.
    let (a_flows, mut flowing) = watch::channel(false);
    let on_a = move |e: EngineEvent| {
        if matches!(e, EngineEvent::Progress { downloaded, .. } if downloaded > 0) {
            a_flows.send_replace(true);
        }
    };
    let first = engine.run(&a.id, &on_a, &stop);
    let second = async {
        flowing.wait_for(|f| *f).await.unwrap();
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
        fx.engine.add(req, &|_| {}).await.unwrap()
    }

    fn order(fx: &Fx, queue: &str) -> Vec<String> {
        fx.engine
            .list(&|_| {})
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
                .add(
                    {
                        let mut r = fx.request(&server, "/blob/10");
                        r.queue = Some("nope".into());
                        r
                    },
                    &|_| {}
                )
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
        fx.engine.add(req, &|_| {}).await.unwrap().id
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
            .add(fx.request(&server, "/blob/10"), &|_| {})
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
            .add(fx.request(&server, "/blob/10"), &|_| {})
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
    use crate::engine::queues::QueueSpec;
    use crate::engine::settings::{LimitPatch, SettingsPatch};

    #[tokio::test(flavor = "multi_thread")]
    async fn limits_are_stored() {
        let server = TestServer::start().unwrap();
        let fx = Fx::new();
        let set = |patch| fx.engine.set_settings(patch, &|_| {});
        let s = set(SettingsPatch {
            global_speed_limit: Some(LimitPatch::Rate(1_000_000)),
            headroom_percent: Some(20),
            ..SettingsPatch::default()
        })
        .unwrap();
        assert_eq!(
            (s.global_speed_limit, s.headroom_percent),
            (Some(1_000_000), 20)
        );
        assert!(matches!(
            set(SettingsPatch {
                headroom_percent: Some(100),
                ..SettingsPatch::default()
            }),
            Err(EngineError::InvalidSetting { .. })
        ));
        assert_eq!(fx.engine.shared.global.rate(), Some(1_000_000));
        set(SettingsPatch {
            global_speed_limit: Some(LimitPatch::Unlimited),
            ..SettingsPatch::default()
        })
        .unwrap();
        assert_eq!(fx.engine.settings().unwrap().global_speed_limit, None);
        assert_eq!(fx.engine.shared.global.rate(), None);

        let mut req = fx.request(&server, "/blob/10");
        req.speed_limit = Some(5_000);
        let added = fx.engine.add(req, &|_| {}).await.unwrap();
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
            ids.push(fx.engine.add(req, &|_| {}).await.unwrap().id);
        }
        let started = Instant::now();
        let (a, b) = tokio::join!(fx.run(&ids[0]), fx.run(&ids[1]));
        assert_eq!((a.state, b.state), (State::Completed, State::Completed));
        // 600 kB at 200 kB/s together, less the 40 kB burst. The floor is
        // the limit holding; the ceiling only catches a stall (CI is slow).
        let secs = started.elapsed().as_secs_f64();
        assert!((2.6..5.0).contains(&secs), "{secs} s");
    }

    /// §12: 0 is "paused by limit"; lifting it applies at once.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_limit_changes_while_running() {
        let server = TestServer::start().unwrap();
        let fx = Fx::new();
        let mut req = fx.request(&server, "/blob/2000000");
        req.speed_limit = Some(0);
        let id = fx.engine.add(req, &|_| {}).await.unwrap().id;
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
            assert_eq!(fx.engine.get(&id, &|_| {}).unwrap().state, State::Active);
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

    /// A shell cancels a run by dropping its future: nothing it registered
    /// may outlive it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_dropped_run_leaves_nothing_behind() {
        let server = TestServer::start().unwrap();
        let fx = Fx::new();
        let id = fx
            .engine
            .add(
                fx.request(&server, "/blob/1000000?throttle=100000"),
                &|_| {},
            )
            .await
            .unwrap()
            .id;
        let (tx, mut flowing) = watch::channel(false);
        let on_event = move |e: EngineEvent| {
            if matches!(e, EngineEvent::Progress { downloaded, .. } if downloaded > 0) {
                tx.send_replace(true);
            }
        };
        let (_keep, stop) = watch::channel(false);
        tokio::select! {
            _ = fx.engine.run(&id, &on_event, &stop) => panic!("finished before the drop"),
            _ = flowing.wait_for(|f| *f) => {}
        }
        assert_eq!(fx.engine.own_buckets(), 0);
        assert_eq!(fx.engine.sampling_count(), 0);
        assert_eq!(fx.engine.shared.budget.usage("").0, 0);
    }
}

mod speed_tests {
    use super::*;
    use crate::engine::speed::SpeedSample;
    use crate::speed::HOUR_MS;

    impl Engine {
        fn record_sample_row(&self, id: Option<&str>, at: i64, bps: u64) {
            self.db()
                .conn()
                .execute(
                    "INSERT INTO speed_samples VALUES (?1, 1, ?2, ?3)",
                    params![id, at, bps as i64],
                )
                .unwrap();
        }
    }

    fn progress(fx: &Fx) -> Vec<(u64, Option<u64>, u64)> {
        fx.events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                EngineEvent::Progress {
                    downloaded,
                    speed_bps,
                    eta_secs,
                    ..
                } => Some((*speed_bps, *eta_secs, *downloaded)),
                _ => None,
            })
            .collect()
    }

    /// §14: one sample a second while it runs, a 5-sample EMA, an ETA from
    /// the third sample on; the global aggregate alongside.
    #[tokio::test(flavor = "multi_thread")]
    async fn samples_ema_and_eta() {
        let server = TestServer::start().unwrap();
        let fx = Fx::new();
        let mut req = fx.request(&server, "/blob/1000000?throttle=200000");
        req.connections = Some(1);
        let id = fx.engine.add(req, &|_| {}).await.unwrap().id;
        assert_eq!(fx.run(&id).await.state, State::Completed);

        let samples = fx.engine.speed_history(Some(&id)).unwrap();
        assert!((4..=6).contains(&samples.len()), "{samples:?}");
        assert!(samples.iter().all(|s| s.period_secs == 1));
        // Whole seconds at about the throttle (the first includes the probe).
        for s in &samples[1..samples.len() - 1] {
            assert!((160_000..240_000).contains(&s.bytes_per_sec), "{samples:?}");
        }
        let global = fx.engine.speed_history(None).unwrap();
        let sum = |v: &[SpeedSample]| v.iter().map(|s| s.bytes_per_sec).sum::<u64>();
        assert_eq!(sum(&global), sum(&samples), "one download: same total");

        let p = progress(&fx);
        let with_speed: Vec<_> = p.iter().filter(|(s, ..)| *s > 0).collect();
        assert!(!with_speed.is_empty(), "{p:?}");
        // ETA appears only once three samples exist, and then fits.
        let first_eta = p.iter().position(|(_, eta, _)| eta.is_some()).unwrap();
        let (speed, eta, downloaded) = p[first_eta];
        assert!(
            (150_000..250_000).contains(&speed),
            "{speed} B/s at the first ETA"
        );
        let expected = (1_000_000 - downloaded).div_ceil(speed);
        assert_eq!(eta, Some(expected));
        assert!(
            downloaded >= 400_000,
            "ETA before 3 s of data: {downloaded}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn old_samples_roll_up_into_hours() {
        let fx = Fx::new();
        let h = HOUR_MS;
        // Two samples in one old hour, one in the next.
        for (at, bps) in [(5 * h + 10_000, 100), (5 * h + 20_000, 300), (6 * h, 50)] {
            fx.engine.record_sample_row(None, at, bps);
        }
        fx.engine.roll_up(6 * h).unwrap();
        assert_eq!(
            fx.engine.speed_history(None).unwrap(),
            [
                SpeedSample {
                    at: 5 * h,
                    period_secs: 3600,
                    bytes_per_sec: 200,
                },
                SpeedSample {
                    at: 6 * h,
                    period_secs: 1,
                    bytes_per_sec: 50,
                },
            ]
        );
    }
}

mod category_tests {
    use super::*;
    use crate::engine::categories::CategoryUsage;
    use crate::engine::settings::{CategoryExtensions, SettingsPatch};
    use crate::naming::ExtraExtensions;

    fn set_extras(
        fx: &Fx,
        category: Category,
        extensions: &[&str],
    ) -> Result<ExtraExtensions, EngineError> {
        let patch = SettingsPatch {
            extra_extensions: vec![CategoryExtensions {
                category,
                extensions: extensions.iter().map(|e| e.to_string()).collect(),
            }],
            ..SettingsPatch::default()
        };
        Ok(fx.engine.set_settings(patch, &|_| {})?.extra_extensions)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn category_from_name_then_type_with_user_extras() {
        let server = TestServer::start().unwrap();
        let fx = Fx::new();
        let add = |path: String, name: Option<&str>| {
            let mut req = fx.request(&server, &path);
            req.filename = name.map(Into::into);
            fx.engine.add(req, &|_| {})
        };
        let named = add(format!("/blob/10?{}", disposition("Clip.MKV")), None);
        assert_eq!(named.await.unwrap().category, Category::Video);
        let typed = add(
            "/blob/10?header=Content-Type%3A%20audio%2Fmpeg".into(),
            Some("song"),
        );
        assert_eq!(typed.await.unwrap().category, Category::Audio);
        // The user's name beats the server's type.
        let renamed = add(
            "/blob/10?header=Content-Type%3A%20audio%2Fmpeg".into(),
            Some("notes.txt"),
        );
        assert_eq!(renamed.await.unwrap().category, Category::Document);
        let unknown = add("/blob/10".into(), Some("thing.qqq"));
        assert_eq!(unknown.await.unwrap().category, Category::Other);

        let extra = set_extras(&fx, Category::Data, &[".QQQ", "mkv"]).unwrap();
        assert_eq!(extra[&Category::Data], ["mkv", "qqq"]);
        let qqq = add("/blob/10".into(), Some("thing2.qqq"));
        assert_eq!(qqq.await.unwrap().category, Category::Data);
        let mkv = add("/blob/10".into(), Some("b.mkv"));
        assert_eq!(mkv.await.unwrap().category, Category::Data, "extras win");
        // Already-added downloads keep theirs.
        assert_eq!(
            fx.engine.list(&|_| {}).unwrap()[0].category,
            Category::Video
        );

        // Moving an extension takes it out of its old category.
        let extra = set_extras(&fx, Category::Code, &["qqq"]).unwrap();
        assert_eq!(extra[&Category::Data], ["mkv"]);
        assert_eq!(extra[&Category::Code], ["qqq"]);
        let extra = set_extras(&fx, Category::Data, &[]).unwrap();
        assert!(!extra.contains_key(&Category::Data));
        assert_eq!(fx.engine.settings().unwrap().extra_extensions, extra);
        assert!(matches!(
            set_extras(&fx, Category::Video, &["a/b"]),
            Err(EngineError::InvalidSetting { .. })
        ));
        // `Other` takes a built-in back out of its category.
        set_extras(&fx, Category::Other, &["bin"]).unwrap();
        let bin = add("/blob/10".into(), Some("firmware.bin"));
        assert_eq!(bin.await.unwrap().category, Category::Other);
    }

    /// The second gets ` (2)` inside the user's extension
    /// (`src (2).tar.lz`); the category comes from the name before.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_name_collision_keeps_the_category() {
        let server = TestServer::start().unwrap();
        let fx = Fx::new();
        set_extras(&fx, Category::Data, &["safetensors", "tar.lz"]).unwrap();
        for name in ["model.safetensors", "src.tar.lz"] {
            let path = format!("/blob/10?{}", disposition(name));
            let first = fx
                .engine
                .add(fx.request(&server, &path), &|_| {})
                .await
                .unwrap();
            let second = fx
                .engine
                .add(fx.request(&server, &path), &|_| {})
                .await
                .unwrap();
            assert_ne!(first.final_path, second.final_path);
            assert_eq!(first.category, Category::Data, "{name}");
            assert_eq!(second.category, Category::Data, "{name}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn storage_breakdown_sums_completed_and_reports_missing_files() {
        let server = TestServer::start().unwrap();
        let fx = Fx::new();
        let mut ids = vec![];
        for (name, size) in [
            ("a.mp4", 1000),
            ("b.mp4", 2000),
            ("c.zip", 300),
            ("d.zip", 40),
        ] {
            let path = format!("/blob/{size}?{}", disposition(name));
            let added = fx
                .engine
                .add(fx.request(&server, &path), &|_| {})
                .await
                .unwrap();
            ids.push(added.id);
        }
        // Not completed: not counted.
        fx.engine
            .add(
                fx.request(&server, &format!("/blob/5?{}", disposition("e.pdf"))),
                &|_| {},
            )
            .await
            .unwrap();
        for id in &ids {
            assert_eq!(fx.run(id).await.state, State::Completed);
        }
        std::fs::remove_file(fx.downloads.join("d.zip")).unwrap();

        assert_eq!(
            fx.engine.storage_breakdown(&|_| {}).unwrap(),
            [
                CategoryUsage {
                    category: Category::Video,
                    bytes: 3000,
                    files: 2,
                    missing: vec![],
                },
                CategoryUsage {
                    category: Category::Archive,
                    bytes: 300,
                    files: 1,
                    missing: vec![ids[3].clone()],
                },
            ]
        );
    }
}

mod group_tests {
    use super::*;
    use crate::engine::groups::GroupSnapshot;

    impl Fx {
        async fn add_named(&self, server: &TestServer, name: &str) -> Snapshot {
            let mut req = self.request(server, "/blob/1000");
            req.filename = Some(name.into());
            let sink = self.events.clone();
            let on_event = move |e: EngineEvent| sink.lock().unwrap().push(e);
            self.engine.add(req, &on_event).await.unwrap()
        }

        fn group_events(&self) -> Vec<(DownloadId, Option<GroupSnapshot>)> {
            std::mem::take(&mut *self.events.lock().unwrap())
                .into_iter()
                .filter_map(|e| match e {
                    EngineEvent::GroupChanged { id, group } => Some((id, group)),
                    _ => None,
                })
                .collect()
        }

        fn group_of(&self, id: &str) -> Option<GroupSnapshot> {
            let g = self.engine.get(id, &|_| {}).unwrap().group?;
            Some(self.engine.group(&g).unwrap())
        }
    }

    /// T-13, and the rest of §13.2's join rules.
    #[tokio::test(flavor = "multi_thread")]
    async fn parts_with_one_key_in_one_directory_group() {
        let server = TestServer::start().unwrap();
        let fx = Fx::new();
        let a1 = fx.add_named(&server, "A.part1.rar").await;
        assert_eq!(a1.group, None, "nothing to group with yet");
        let a2 = fx.add_named(&server, "A.part2.rar").await;
        let b = fx.add_named(&server, "B.rar").await;
        assert_eq!(b.group, None);

        let groups = fx.engine.list_groups().unwrap();
        assert_eq!(groups.len(), 1);
        let g = &groups[0];
        assert_eq!((g.name.as_str(), g.auto), ("A", true));
        assert_eq!(g.members, [a1.id.clone(), a2.id.clone()]);
        assert_eq!(a2.group.as_ref(), Some(&g.id));
        assert_eq!(
            fx.group_events(),
            [(a1.id.clone(), Some(g.clone()))],
            "the partner hears; the new one's group is in its snapshot"
        );

        // Joins the existing group; other extensions and directories don't.
        let a3 = fx.add_named(&server, "a_PART-3.rar").await;
        assert_eq!(a3.group.as_ref(), Some(&g.id));
        assert!(fx.group_events().is_empty());
        assert_eq!(fx.add_named(&server, "A.part4.zip").await.group, None);
        let mut req = fx.request(&server, "/blob/1000");
        req.directory = fx.downloads.join("other");
        std::fs::create_dir(&req.directory).unwrap();
        req.filename = Some("A.part5.rar".into());
        assert_eq!(fx.engine.add(req, &|_| {}).await.unwrap().group, None);
        // A name collision's ` (2)` is stripped too.
        let a1_again = fx.add_named(&server, "A.part1.rar").await;
        assert!(a1_again.final_path.ends_with("A.part1 (2).rar"));
        assert_eq!(a1_again.group.as_ref(), Some(&g.id));
        assert_eq!(fx.engine.list_groups().unwrap().len(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn manual_changes_make_groups_and_downloads_manual() {
        let server = TestServer::start().unwrap();
        let fx = Fx::new();
        let a1 = fx.add_named(&server, "Album.cd1.flac").await;
        let a2 = fx.add_named(&server, "Album.cd2.flac").await;
        let b = fx.add_named(&server, "Booklet.pdf").await;
        let g = fx.engine.list_groups().unwrap()[0].id.clone();
        fx.group_events();

        // Into an automatic group: it becomes manual.
        let moved = fx.engine.set_group(&b.id, Some(&g), &|_| {}).unwrap();
        assert_eq!(moved.group.as_ref(), Some(&g));
        let snap = fx.engine.group(&g).unwrap();
        assert!(!snap.auto);
        assert_eq!(snap.members, [a1.id.clone(), a2.id.clone(), b.id.clone()]);
        // ...so a new part doesn't join it, and has no one else to pair with.
        let a3 = fx.add_named(&server, "Album.cd3.flac").await;
        assert_eq!(a3.group, None);

        // A manual group keeps its last member; empty, it's gone.
        for id in [&a1.id, &a2.id] {
            fx.engine.set_group(id, None, &|_| {}).unwrap();
        }
        assert_eq!(fx.engine.group(&g).unwrap().members, [b.id.as_str()]);
        fx.engine.set_group(&b.id, None, &|_| {}).unwrap();
        assert!(matches!(fx.engine.group(&g), Err(EngineError::NotFound(_))));
        assert!(matches!(
            fx.engine.set_group(&b.id, Some("nope"), &|_| {}),
            Err(EngineError::NotFound(_))
        ));

        // Taken out by hand: a1 and a2 stay out; a fourth part pairs with
        // the third, which was never placed.
        let a4 = fx.add_named(&server, "Album.cd4.flac").await;
        let g2 = a4.group.clone().expect("grouped with cd3");
        assert_eq!(
            fx.engine.group(&g2).unwrap().members,
            [a3.id.clone(), a4.id.clone()]
        );

        // Renaming makes it manual.
        assert!(matches!(
            fx.engine.rename_group(&g2, "  ", &|_| {}),
            Err(EngineError::InvalidName(_))
        ));
        fx.group_events();
        let sink = fx.events.clone();
        let on_event = move |e: EngineEvent| sink.lock().unwrap().push(e);
        let renamed = fx.engine.rename_group(&g2, " Live ", &on_event).unwrap();
        assert_eq!((renamed.name.as_str(), renamed.auto), ("Live", false));
        assert_eq!(fx.group_events().len(), 2, "one per member");
        assert_eq!(fx.engine.resolve_group("Live").unwrap(), g2);
        assert_eq!(fx.engine.resolve_group(&g2[..12]).unwrap(), g2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_auto_group_down_to_one_dissolves() {
        let server = TestServer::start().unwrap();
        let fx = Fx::new();
        let a1 = fx.add_named(&server, "Show.S01E01.mkv").await;
        let a2 = fx.add_named(&server, "Show.S01E02.mkv").await;
        let g = a2.group.clone().unwrap();
        fx.group_events();
        let sink = fx.events.clone();
        let on_event = move |e: EngineEvent| sink.lock().unwrap().push(e);
        fx.engine.set_group(&a1.id, None, &on_event).unwrap();
        assert_eq!(
            fx.group_events(),
            [(a1.id.clone(), None), (a2.id.clone(), None)]
        );
        assert!(fx.engine.list_groups().unwrap().is_empty());
        assert_eq!(fx.engine.get(&a2.id, &|_| {}).unwrap().group, None);
        assert!(matches!(fx.engine.group(&g), Err(EngineError::NotFound(_))));
        // a1 was placed by hand; a2 wasn't.
        let a3 = fx.add_named(&server, "Show.S01E03.mkv").await;
        assert_eq!(
            fx.group_of(&a3.id).unwrap().members,
            [a2.id.clone(), a3.id.clone()]
        );
        assert_eq!(fx.engine.get(&a1.id, &|_| {}).unwrap().group, None);
    }

    /// The `Renamed` path (M3.5): a member whose key no longer matches
    /// leaves, and its group dissolves.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_renamed_member_leaves() {
        let server = TestServer::start().unwrap();
        let fx = Fx::new();
        let a1 = fx.add_named(&server, "Movie.Part1.rar").await;
        let a2 = fx.add_named(&server, "Movie.Part2.rar").await;
        assert!(a2.group.is_some());
        fx.engine
            .db()
            .conn()
            .execute(
                "UPDATE downloads SET final_path = ?2 WHERE id = ?1",
                params![a2.id, path_str(&fx.downloads.join("Trailer.rar"))],
            )
            .unwrap();
        assert_eq!(
            fx.engine.auto_group(&a2.id).unwrap(),
            [a2.id.clone(), a1.id.clone()]
        );
        assert!(fx.engine.list_groups().unwrap().is_empty());
        assert!(fx.engine.auto_group(&a1.id).unwrap().is_empty());
    }

    /// Another process commits while grouping waits for the write lock:
    /// a deferred read-then-write would fail with `SQLITE_BUSY`.
    #[tokio::test(flavor = "multi_thread")]
    async fn grouping_waits_for_another_writer() {
        let server = TestServer::start().unwrap();
        let fx = Fx::new();
        let a1 = fx.add_named(&server, "Movie.Part1.rar").await;
        let a2 = fx.add_named(&server, "Movie.Part2.rar").await;
        fx.engine.set_group(&a1.id, None, &|_| {}).unwrap();
        fx.engine
            .db()
            .conn()
            .execute("UPDATE downloads SET group_pinned = 0", [])
            .unwrap();
        assert_eq!(fx.engine.get(&a2.id, &|_| {}).unwrap().group, None);

        let other = rusqlite::Connection::open(fx._dir.path().join("data/dhow.sqlite")).unwrap();
        other.execute_batch("BEGIN IMMEDIATE").unwrap();
        other
            .execute("INSERT INTO settings (key, value) VALUES ('x', '1')", [])
            .unwrap();
        let (locked, wait) = std::sync::mpsc::channel();
        let writer = std::thread::spawn(move || {
            locked.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(300));
            other.execute_batch("COMMIT").unwrap();
        });
        wait.recv().unwrap();
        assert_eq!(
            fx.engine.auto_group(&a2.id).unwrap(),
            [a1.id.clone(), a2.id.clone()]
        );
        writer.join().unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn groups_aggregate_progress_speed_and_eta() {
        let server = TestServer::start().unwrap();
        let fx = Fx::new();
        let a1 = fx.add_named(&server, "Set.01.bin").await;
        let a2 = fx.add_named(&server, "Set.02.bin").await;
        let g = a2.group.clone().unwrap();
        // A cancelled member stays a member but doesn't count.
        let mut req = fx.request(&server, "/blob/5000");
        req.filename = Some("Set.03.bin".into());
        let a3 = fx.engine.add(req, &|_| {}).await.unwrap();
        fx.engine.cancel(&a3.id, false, &|_| {}).await.unwrap();
        let snap = fx.engine.group(&g).unwrap();
        assert_eq!(snap.members.len(), 3);
        assert_eq!((snap.downloaded, snap.total), (0, Some(2000)));
        assert_eq!((snap.speed_bps, snap.eta_secs), (0, None));

        assert_eq!(fx.run(&a1.id).await.state, State::Completed);
        // a2 running at 4000 B/s by its recent samples; a stale one
        // doesn't count.
        let now = now_ms();
        {
            let db = fx.engine.db();
            db.conn()
                .execute(
                    "UPDATE downloads SET state = 'active' WHERE id = ?1",
                    [&a2.id],
                )
                .unwrap();
            for (at, bps) in [
                (now - 60_000, 1_000_000),
                (now - 2000, 4000),
                (now - 1000, 4000),
                (now, 4000),
            ] {
                db.conn()
                    .execute(
                        "INSERT INTO speed_samples (download_id, period_secs, at, bytes_per_sec)
                         VALUES (?1, 1, ?2, ?3)",
                        params![a2.id, at, bps],
                    )
                    .unwrap();
            }
        }
        let snap = fx.engine.group(&g).unwrap();
        assert_eq!((snap.downloaded, snap.total), (1000, Some(2000)));
        assert_eq!((snap.speed_bps, snap.eta_secs), (4000, Some(1)));
        // Not running: no speed.
        fx.engine
            .db()
            .conn()
            .execute(
                "UPDATE downloads SET state = 'paused' WHERE id = ?1",
                [&a2.id],
            )
            .unwrap();
        assert_eq!(fx.engine.group(&g).unwrap().speed_bps, 0);
    }
}

mod token_tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use super::*;
    use crate::engine::categories::CategoryUsage;
    use crate::platform::{FileToken, Resolved};

    /// Tokens are indexes into `files`, which the test keeps up to date as
    /// it moves them, as the OS would.
    #[derive(Default)]
    struct Tracker {
        files: StdMutex<Vec<PathBuf>>,
        resolves: AtomicUsize,
        fail: AtomicBool,
        refresh: AtomicBool,
    }

    impl PlatformAdapter for Tracker {
        fn file_token_create(&self, path: &Path) -> Result<Option<FileToken>, AdapterError> {
            let mut files = self.files.lock().unwrap();
            files.push(path.to_owned());
            Ok(Some(FileToken(
                (files.len() as u32 - 1).to_le_bytes().into(),
            )))
        }

        fn file_token_resolve(&self, token: &FileToken) -> Result<Option<Resolved>, AdapterError> {
            self.resolves.fetch_add(1, Ordering::SeqCst);
            if self.fail.load(Ordering::SeqCst) {
                return Err(AdapterError::Failed("boom".into()));
            }
            let at = u32::from_le_bytes(token.0[..4].try_into().unwrap()) as usize;
            let path = self.files.lock().unwrap()[at].clone();
            // Stale: a fresh token comes back, marked.
            let token = self.refresh.load(Ordering::SeqCst).then(|| {
                let mut t = token.0[..4].to_vec();
                t.push(0xff);
                FileToken(t)
            });
            Ok(path.is_file().then_some(Resolved { path, token }))
        }
    }

    impl Tracker {
        fn move_file(&self, from: &Path, to: &Path) {
            std::fs::rename(from, to).unwrap();
            for f in self.files.lock().unwrap().iter_mut() {
                if f == from {
                    *f = to.to_owned();
                }
            }
        }

        fn resolves(&self) -> usize {
            self.resolves.load(Ordering::SeqCst)
        }
    }

    fn tracked() -> (Fx, Arc<Tracker>) {
        let tracker = Arc::new(Tracker::default());
        (Fx::with_adapter(fast(), tracker.clone()), tracker)
    }

    impl Fx {
        async fn completed(&self, server: &TestServer, name: &str) -> Snapshot {
            let mut req = self.request(server, "/blob/1000");
            req.filename = Some(name.into());
            let added = self.engine.add(req, &|_| {}).await.unwrap();
            let done = self.run(&added.id).await;
            assert_eq!(done.state, State::Completed);
            self.events.lock().unwrap().clear();
            done
        }

        fn token(&self, id: &str) -> Option<Vec<u8>> {
            self.engine
                .db()
                .conn()
                .query_row(
                    "SELECT file_token FROM downloads WHERE id = ?1",
                    [id],
                    |r| r.get(0),
                )
                .unwrap()
        }

        fn sink(&self) -> impl Fn(EngineEvent) + Send + Sync + use<> {
            let sink = self.events.clone();
            move |e: EngineEvent| sink.lock().unwrap().push(e)
        }

        fn taken(&self) -> Vec<EngineEvent> {
            std::mem::take(&mut *self.events.lock().unwrap())
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_moved_file_is_found_on_query_once() {
        let server = TestServer::start().unwrap();
        let (fx, tracker) = tracked();
        let done = fx.completed(&server, "x.mkv").await;
        assert_eq!(done.category, Category::Video);
        assert!(fx.token(&done.id).is_some(), "§8 step 8");
        assert_eq!(
            *tracker.files.lock().unwrap(),
            std::slice::from_ref(&done.final_path)
        );
        // Unfinished downloads have no final file to look for.
        let mut req = fx.request(&server, "/blob/1000");
        req.paused = true;
        fx.engine.add(req, &|_| {}).await.unwrap();

        let on_event = fx.sink();
        assert_eq!(fx.engine.get(&done.id, &on_event).unwrap(), done);
        assert_eq!(fx.engine.list(&on_event).unwrap().len(), 2);
        assert_eq!(tracker.resolves(), 0, "the file is where it was");

        let elsewhere = fx.downloads.join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        let moved = elsewhere.join("x.zip");
        tracker.move_file(&done.final_path, &moved);
        let snap = fx.engine.get(&done.id, &on_event).unwrap();
        assert_eq!(snap.final_path, moved);
        assert_eq!(snap.category, Category::Archive, "§13.1: from the new name");
        assert_eq!(snap.downloaded, 1000);
        assert_eq!(
            fx.taken(),
            [EngineEvent::Renamed {
                id: done.id.clone(),
                path: moved.clone(),
            }]
        );
        let listed = fx.engine.list(&on_event).unwrap();
        assert_eq!(listed[0], snap);
        assert!(fx.taken().is_empty(), "found once");
        assert_eq!(tracker.resolves(), 1);
    }

    /// §13.2 step 8: a part renamed out of its group's key leaves, and the
    /// group dissolves.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_moved_part_leaves_its_group() {
        let server = TestServer::start().unwrap();
        let (fx, tracker) = tracked();
        let a1 = fx.completed(&server, "Movie.Part1.rar").await;
        let a2 = fx.completed(&server, "Movie.Part2.rar").await;
        assert!(fx.engine.get(&a2.id, &|_| {}).unwrap().group.is_some());

        let trailer = fx.downloads.join("Trailer.rar");
        tracker.move_file(&a2.final_path, &trailer);
        let on_event = fx.sink();
        let listed = fx.engine.list(&on_event).unwrap();
        assert_eq!(listed[1].final_path, trailer);
        assert_eq!(listed[1].category, Category::Archive);
        assert!(listed.iter().all(|s| s.group.is_none()));
        assert_eq!(
            fx.taken(),
            [
                EngineEvent::Renamed {
                    id: a2.id.clone(),
                    path: trailer,
                },
                EngineEvent::GroupChanged {
                    id: a2.id.clone(),
                    group: None,
                },
                EngineEvent::GroupChanged {
                    id: a1.id.clone(),
                    group: None,
                },
            ]
        );
        assert!(fx.engine.list_groups().unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn storage_breakdown_follows_moved_files() {
        let server = TestServer::start().unwrap();
        let (fx, tracker) = tracked();
        let a = fx.completed(&server, "a.mp4").await;
        let b = fx.completed(&server, "b.mp4").await;
        let c = fx.completed(&server, "c.mp4").await;
        tracker.move_file(&a.final_path, &fx.downloads.join("a2.mp4"));
        // Deleted: the token resolves to nothing.
        std::fs::remove_file(&b.final_path).unwrap();
        // The adapter fails: missing too.
        tracker.move_file(&c.final_path, &fx.downloads.join("c2.mp4"));
        let on_event = fx.sink();
        let failing = |fail| tracker.fail.store(fail, Ordering::SeqCst);
        failing(true);
        let snap = fx.engine.get(&c.id, &on_event).unwrap();
        assert_eq!(snap.final_path, c.final_path);
        assert!(fx.taken().is_empty());
        failing(false);

        let usage = |fx: &Fx| fx.engine.storage_breakdown(&on_event).unwrap();
        assert_eq!(
            usage(&fx),
            [CategoryUsage {
                category: Category::Video,
                bytes: 2000,
                files: 2,
                missing: vec![b.id.clone()],
            }]
        );
        assert_eq!(fx.taken().len(), 2, "a and c renamed");
        assert_eq!(
            fx.engine.get(&b.id, &|_| {}).unwrap().final_path,
            b.final_path
        );
    }

    /// `Ok(None)` is final: the token goes, and later queries don't ask.
    /// A failure is asked again.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_deleted_file_is_resolved_once_and_a_failure_again() {
        let server = TestServer::start().unwrap();
        let (fx, tracker) = tracked();
        let gone = fx.completed(&server, "gone.mp4").await;
        let away = fx.completed(&server, "away.mp4").await;
        std::fs::remove_file(&gone.final_path).unwrap();
        for _ in 0..2 {
            assert_eq!(fx.engine.get(&gone.id, &|_| {}).unwrap(), gone);
        }
        assert_eq!(tracker.resolves(), 1);
        assert_eq!(fx.token(&gone.id), None);

        tracker.move_file(&away.final_path, &fx.downloads.join("away2.mp4"));
        tracker.fail.store(true, Ordering::SeqCst);
        for _ in 0..2 {
            assert_eq!(fx.engine.get(&away.id, &|_| {}).unwrap(), away);
        }
        assert_eq!(tracker.resolves(), 3);
        assert!(fx.token(&away.id).is_some());
    }

    /// Two queries holding the same stale row: one `Renamed`.
    #[tokio::test(flavor = "multi_thread")]
    async fn racing_queries_rename_once_and_store_a_refreshed_token() {
        let server = TestServer::start().unwrap();
        let (fx, tracker) = tracked();
        let done = fx.completed(&server, "x.mkv").await;
        let stale = fx.engine.stored(&done.id).unwrap();
        let moved = fx.downloads.join("y.mkv");
        tracker.move_file(&done.final_path, &moved);
        tracker.refresh.store(true, Ordering::SeqCst);
        let on_event = fx.sink();
        for _ in 0..2 {
            let snap = fx.engine.located(stale.clone(), &on_event).unwrap();
            assert_eq!(snap.final_path, moved);
        }
        let renamed = fx
            .taken()
            .into_iter()
            .filter(|e| matches!(e, EngineEvent::Renamed { .. }))
            .count();
        assert_eq!(renamed, 1);
        assert_eq!(fx.token(&done.id).unwrap().last(), Some(&0xff));
    }

    /// §13.1: a new name without a known extension keeps the category,
    /// here from the `Content-Type`.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_name_without_a_known_extension_keeps_the_category() {
        let server = TestServer::start().unwrap();
        let (fx, tracker) = tracked();
        let mut req = fx.request(&server, "/blob/1000?header=Content-Type%3A%20video%2Fmp4");
        req.filename = Some("clip".into());
        let added = fx.engine.add(req, &|_| {}).await.unwrap();
        assert_eq!(added.category, Category::Video);
        let done = fx.run(&added.id).await;
        let moved = fx.downloads.join("clip-old");
        tracker.move_file(&done.final_path, &moved);
        let snap = fx.engine.get(&done.id, &|_| {}).unwrap();
        assert_eq!((snap.final_path, snap.category), (moved, Category::Video));
    }

    /// §8: the token is also created when completion is finished after a
    /// crash.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_recovered_completion_gets_a_token() {
        let server = TestServer::start().unwrap();
        let (fx, tracker) = tracked();
        let added = fx
            .engine
            .add(fx.request(&server, "/blob/10"), &|_| {})
            .await
            .unwrap();
        std::fs::write(&added.final_path, blob::bytes(0, 0..10)).unwrap();
        std::fs::write(added.sidecar_path(), b"stale").unwrap();
        fx.engine
            .transition(&added.id, State::Completing, None, &|_| {})
            .unwrap();
        assert_eq!(fx.run(&added.id).await.state, State::Completed);
        assert!(fx.token(&added.id).is_some());
        assert_eq!(*tracker.files.lock().unwrap(), [added.final_path]);
    }

    /// `remove(delete_files)` deletes the download's file, found by its
    /// token, not another one put at its old path; a file the adapter
    /// reports gone deletes nothing; an adapter that can't tell removes
    /// nothing.
    #[tokio::test(flavor = "multi_thread")]
    async fn remove_deletes_the_file_its_token_finds() {
        let server = TestServer::start().unwrap();
        let (fx, tracker) = tracked();
        let a = fx.completed(&server, "a.bin").await;
        let moved = fx.downloads.join("moved.bin");
        tracker.move_file(&a.final_path, &moved);
        std::fs::write(&a.final_path, b"the user's").unwrap();
        fx.engine.remove(&a.id, true, &|_| {}).await.unwrap();
        assert!(!moved.exists(), "the download's file is deleted");
        assert_eq!(std::fs::read(&a.final_path).unwrap(), b"the user's");

        let b = fx.completed(&server, "b.bin").await;
        tracker.fail.store(true, Ordering::SeqCst);
        assert!(fx.engine.remove(&b.id, true, &|_| {}).await.is_err());
        assert!(b.final_path.exists());
        assert!(fx.engine.get(&b.id, &|_| {}).is_ok(), "nothing removed");
        tracker.fail.store(false, Ordering::SeqCst);
        // Gone for good (in the Trash): only the row goes.
        tracker.move_file(&b.final_path, &fx.downloads.join("b-elsewhere"));
        std::fs::remove_file(fx.downloads.join("b-elsewhere")).unwrap();
        std::fs::write(&b.final_path, b"another").unwrap();
        fx.engine.remove(&b.id, true, &|_| {}).await.unwrap();
        assert_eq!(std::fs::read(&b.final_path).unwrap(), b"another");
    }

    /// Linux and Android: no tokens, and a moved file is just missing.
    #[tokio::test(flavor = "multi_thread")]
    async fn without_tokens_a_moved_file_is_missing() {
        let server = TestServer::start().unwrap();
        let fx = Fx::new();
        let done = fx.completed(&server, "x.mkv").await;
        assert_eq!(fx.token(&done.id), None);
        std::fs::rename(&done.final_path, fx.downloads.join("y.mkv")).unwrap();
        let on_event = fx.sink();
        let snap = fx.engine.get(&done.id, &on_event).unwrap();
        assert_eq!(snap.final_path, done.final_path);
        assert!(fx.taken().is_empty());
    }
}

/// A run dropped while its file work is half done: a rerun (or a cancel)
/// waits for that work instead of interleaving with it (download::hold).
mod hold_tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;
    use crate::download::hold::park::{self, Armed};

    async fn added(fx: &Fx, server: &TestServer, path: &str) -> DownloadId {
        fx.engine
            .add(fx.request(server, path), &|_| {})
            .await
            .unwrap()
            .id
    }

    /// Runs `id` until its blocking work parks at `point`, then drops the
    /// run, leaving that work parked. `"commit"` (the writer's final
    /// commit) is only reached once the run is dropped: it is dropped as
    /// soon as bytes flow.
    async fn orphan_at(fx: &Fx, id: &str, point: &'static str) -> Armed {
        let snap = fx.engine.stored(id).unwrap();
        // The writer knows its files by the sidecar.
        let key = match point {
            "commit" => snap.sidecar_path(),
            _ => snap.part_path,
        };
        let armed = park::arm(point, &key);
        let p = armed.park();
        let reached = tokio::task::spawn_blocking(move || p.wait_reached());
        let (tx, mut flowing) = watch::channel(false);
        let on_event = move |e: EngineEvent| {
            if matches!(e, EngineEvent::Progress { downloaded, .. } if downloaded > 0) {
                tx.send_replace(true);
            }
        };
        let (_keep, stop) = watch::channel(false);
        let drop_on_flow = async {
            if point == "commit" {
                flowing.wait_for(|f| *f).await.unwrap();
            } else {
                std::future::pending::<()>().await;
            }
        };
        tokio::select! {
            r = fx.engine.run(id, &on_event, &stop) => panic!("finished before parking: {r:?}"),
            _ = drop_on_flow => {}
            r = reached => r.unwrap(),
        }
        if point == "commit" {
            let p = armed.park();
            let reached = tokio::task::spawn_blocking(move || p.wait_reached());
            tokio::time::timeout(Duration::from_secs(10), reached)
                .await
                .expect("the writer parked")
                .unwrap();
        }
        armed
    }

    /// Runs `fut` while the orphan stays parked for 300 ms, then releases
    /// it. Nothing may have happened before the release.
    async fn after_release<T>(
        armed: &Armed,
        started: &AtomicBool,
        fut: impl std::future::Future<Output = T>,
    ) -> T {
        let release = async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            let early = started.load(Ordering::SeqCst);
            armed.release();
            early
        };
        let (out, early) = tokio::join!(fut, release);
        assert!(!early, "went ahead while the orphan held the files");
        out
    }

    fn noting(started: &AtomicBool) -> impl Fn(EngineEvent) + Send + Sync + '_ {
        move |e: EngineEvent| {
            if matches!(e, EngineEvent::StateChanged { .. }) {
                started.store(true, Ordering::SeqCst);
            }
        }
    }

    async fn rerun(fx: &Fx, id: &str, armed: &Armed) -> Snapshot {
        let started = AtomicBool::new(false);
        let on_event = noting(&started);
        let (_keep, stop) = watch::channel(false);
        let run = fx.engine.run(id, &on_event, &stop);
        after_release(armed, &started, run).await.unwrap()
    }

    fn assert_done(fx: &Fx, done: &Snapshot) {
        assert_eq!(done.state, State::Completed);
        assert_eq!(
            blake3::hash(&std::fs::read(&done.final_path).unwrap()),
            blob::digest(0, 1_000_000)
        );
        // One file: no second copy under a de-duplicated name, no leftovers.
        let name = done.final_path.file_name().unwrap().to_string_lossy();
        assert_eq!(fx.files(), [name.into_owned()]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_rerun_waits_for_a_dropped_runs_prepare() {
        let server = TestServer::start().unwrap();
        let fx = Fx::new();
        let id = added(&fx, &server, "/blob/1000000").await;
        let armed = orphan_at(&fx, &id, "prepare").await;
        let done = rerun(&fx, &id, &armed).await;
        assert_done(&fx, &done);
    }

    /// The orphan's writer was making its final commit.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_rerun_waits_for_a_dropped_runs_writer() {
        let server = TestServer::start().unwrap();
        let fx = Fx::new();
        let id = added(&fx, &server, "/blob/1000000?throttle=1000000").await;
        let armed = orphan_at(&fx, &id, "commit").await;
        let done = rerun(&fx, &id, &armed).await;
        assert_done(&fx, &done);
    }

    /// The orphan was finalizing (digest check, rename): the rerun finds
    /// its rename done and only records it, rather than downloading again
    /// to a de-duplicated name.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_rerun_waits_for_a_dropped_runs_finalize() {
        let server = TestServer::start().unwrap();
        let fx = Fx::new();
        let id = added(&fx, &server, "/blob/1000000").await;
        let armed = orphan_at(&fx, &id, "finalize").await;
        assert_eq!(fx.engine.stored(&id).unwrap().state, State::Completing);
        let done = rerun(&fx, &id, &armed).await;
        assert_done(&fx, &done);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_waiting_rerun_can_be_paused() {
        let server = TestServer::start().unwrap();
        let fx = Fx::new();
        let id = added(&fx, &server, "/blob/1000000").await;
        let _armed = orphan_at(&fx, &id, "prepare").await;
        let (tx, stop) = watch::channel(false);
        let run = fx.engine.run(&id, &|_| {}, &stop);
        let pause = async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            tx.send_replace(true);
        };
        let joined = async { tokio::join!(run, pause) };
        let (snap, ()) = tokio::time::timeout(Duration::from_secs(10), joined)
            .await
            .expect("paused while waiting");
        assert_eq!(snap.unwrap().state, State::Paused);
    }

    /// A cancel deleting the partial files waits too, or the orphan would
    /// create them again behind it.
    #[tokio::test(flavor = "multi_thread")]
    async fn cancel_waits_for_a_dropped_runs_prepare() {
        let server = TestServer::start().unwrap();
        let fx = Fx::new();
        let id = added(&fx, &server, "/blob/1000000").await;
        let armed = orphan_at(&fx, &id, "prepare").await;
        let started = AtomicBool::new(false);
        let on_event = noting(&started);
        let cancel = fx.engine.cancel(&id, true, &on_event);
        let snap = after_release(&armed, &started, cancel).await.unwrap();
        assert_eq!(snap.state, State::Cancelled);
        // Whatever the orphan does, it has done by now.
        drop(PartHold::acquire(&snap.part_path).await);
        assert!(fx.files().is_empty(), "{:?}", fx.files());
    }

    /// Cancelling one whose orphaned finalize renames it: it is complete,
    /// not a complete file under a cancelled download.
    #[tokio::test(flavor = "multi_thread")]
    async fn cancel_after_a_dropped_runs_rename_completes_it() {
        let server = TestServer::start().unwrap();
        let fx = Fx::new();
        let id = added(&fx, &server, "/blob/1000000").await;
        let armed = orphan_at(&fx, &id, "finalize").await;
        let started = AtomicBool::new(false);
        let on_event = noting(&started);
        let cancel = fx.engine.cancel(&id, true, &on_event);
        let snap = after_release(&armed, &started, cancel).await.unwrap();
        assert_done(&fx, &snap);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn snapshot_has_validators_and_connections_and_a_run_can_be_failed() {
    let server = TestServer::start().unwrap();
    let fx = Fx::new();
    let mut req = fx.request(&server, "/blob/1000?seed=1");
    req.connections = Some(2);
    let added = fx.engine.add(req, &|_| {}).await.unwrap();
    assert!(added.etag.is_some() && added.last_modified.is_some());
    assert_eq!(added.connections, Some(2));

    // §9 Local: what the engine couldn't run is failed, once.
    let sink = fx.events.clone();
    let on_event = move |e: EngineEvent| sink.lock().unwrap().push(e);
    let failed = fx.engine.fail(&added.id, &on_event).unwrap();
    assert_eq!(
        (failed.state, failed.fail_reason),
        (State::Failed, Some(FailReason::Io))
    );
    fx.engine.fail(&added.id, &on_event).unwrap();
    assert_eq!(fx.states(), [State::Failed]);
    assert_eq!(fx.run(&added.id).await.state, State::Completed);
    let done = fx.engine.fail(&added.id, &on_event).unwrap();
    assert_eq!(done.state, State::Completed);
}

/// What a shell needs besides M1–M3: probe, blind mode, remove, retry,
/// live connections and the segment map (ARCHITECTURE §3).
#[allow(clippy::single_range_in_vec_init)]
mod shell_tests {
    use super::*;
    use crate::storage::DEFAULT_QUEUE_ID;

    type Events = Arc<StdMutex<Vec<EngineEvent>>>;

    fn recorder() -> (Events, impl Fn(EngineEvent) + Send + Sync + Clone + 'static) {
        let events: Events = Arc::default();
        let sink = events.clone();
        (events, move |e| sink.lock().unwrap().push(e))
    }

    fn maps(events: &Events) -> Vec<RangeMap> {
        events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                EngineEvent::RangeMap { map, .. } => Some(map.clone()),
                _ => None,
            })
            .collect()
    }

    /// `done`, the connections' chunks and `pending` cover `[0, total)`
    /// once each.
    fn assert_partition(map: &RangeMap, total: u64) {
        let mut all: Vec<std::ops::Range<u64>> = map
            .done
            .iter()
            .cloned()
            .chain(map.connections.iter().map(|c| c.start..c.end))
            .chain(map.pending.iter().cloned())
            .collect();
        all.sort_by_key(|r| r.start);
        let mut at = 0;
        for r in &all {
            assert_eq!(r.start, at, "gap or overlap in {map:?}");
            at = r.end;
        }
        assert_eq!(at, total, "{map:?}");
        for c in &map.connections {
            assert!(c.start <= c.at && c.at <= c.end, "{c:?}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn probe_names_and_categorizes_and_records_nothing() {
        let server = TestServer::start().unwrap();
        let fx = Fx::new();
        let url = server.url(&format!("/blob/5000?{}", disposition("clip.mkv")));
        let url = Url::parse(&url).unwrap();
        let p = fx.engine.probe(&url, &[], TargetFs::Posix).await.unwrap();
        assert_eq!(p.info.suggested_name, "clip.mkv");
        assert_eq!(p.info.total_size, Some(5000));
        assert_eq!(p.info.resumable, Resumable::Yes);
        assert_eq!(p.category, Category::Video);
        // The naming toggles apply as at `add` (§3).
        let patch = settings::SettingsPatch {
            use_content_disposition: Some(false),
            ..Default::default()
        };
        fx.engine.set_settings(patch, &|_| {}).unwrap();
        let p = fx.engine.probe(&url, &[], TargetFs::Posix).await.unwrap();
        assert_eq!(p.info.suggested_name, "5000");
        assert!(fx.engine.list(&|_| {}).unwrap().is_empty());
        let nobody = Url::parse("http://127.0.0.1:1/x").unwrap();
        assert!(matches!(
            fx.engine.probe(&nobody, &[], TargetFs::Posix).await,
            Err(EngineError::Probe(ProbeError::Connect))
        ));
    }

    /// §2.4: HEAD and the range GET fail (at `add`, and again at the run's
    /// probe); the plain GET works.
    #[tokio::test(flavor = "multi_thread")]
    async fn blind_mode_adds_and_runs_what_cannot_be_probed() {
        let server = TestServer::start().unwrap();
        let fx = Fx::new();
        let path = "/blob/300000?seed=4&head=403&fail=2&fail_mode=403";
        let refused = fx.engine.add(fx.request(&server, path), &|_| {}).await;
        assert!(matches!(
            refused,
            Err(EngineError::Probe(ProbeError::Status(403)))
        ));
        let mut req = fx.request(&server, path);
        req.blind = true;
        let added = fx.engine.add(req, &|_| {}).await.unwrap();
        assert_eq!(added.state, State::Queued);
        assert_eq!(added.total_size, None);
        assert_eq!(added.resumable, Resumable::No);
        assert_eq!(added.final_path, fx.downloads.join("300000"));
        let done = fx.run(&added.id).await;
        assert_eq!(done.state, State::Completed, "{:?}", done.fail_reason);
        assert_eq!(
            blake3::hash(&std::fs::read(&done.final_path).unwrap()),
            blob::digest(4, 300_000)
        );
        // Blind is for a server that can't be probed, not for a request
        // that can't be made.
        let mut bad = fx.request(&server, "/blob/1");
        bad.url = Url::parse("ftp://example.com/x").unwrap();
        bad.blind = true;
        assert!(matches!(
            fx.engine.add(bad, &|_| {}).await,
            Err(EngineError::Probe(ProbeError::UnsupportedScheme(_)))
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn add_announces_the_download_before_its_group() {
        let server = TestServer::start().unwrap();
        let fx = Fx::new();
        let (events, on_event) = recorder();
        let mut a = fx.request(&server, "/blob/10");
        a.filename = Some("Movie.Part1.rar".into());
        let a = fx.engine.add(a, &on_event).await.unwrap();
        let mut b = fx.request(&server, "/blob/10");
        b.filename = Some("Movie.Part2.rar".into());
        let b = fx.engine.add(b, &on_event).await.unwrap();
        assert!(b.group.is_some());
        let es = events.lock().unwrap().clone();
        assert_eq!(es.len(), 3, "{es:?}");
        assert!(matches!(&es[0], EngineEvent::DownloadAdded { snapshot } if snapshot.id == a.id));
        assert!(matches!(&es[1], EngineEvent::DownloadAdded { snapshot } if *snapshot == b));
        assert!(matches!(&es[2], EngineEvent::GroupChanged { id, group: Some(_) } if *id == a.id));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn remove_forgets_and_deletes_only_when_asked() {
        let server = TestServer::start().unwrap();
        let fx = Fx::new();
        let add = |name: &str, path: &str| {
            let mut r = fx.request(&server, path);
            r.filename = Some(name.into());
            fx.engine.add(r, &|_| {})
        };
        let a = add("Show.Part1.mkv", "/blob/2000?seed=1").await.unwrap();
        let b = add("Show.Part2.mkv", "/blob/2000?seed=2").await.unwrap();
        let c = add("partial.bin", "/blob/1000000?throttle=1000000")
            .await
            .unwrap();
        let done = fx.run(&a.id).await;
        assert_eq!(done.state, State::Completed);

        // A completed one, with its file; its auto group dissolves (§13.2).
        let (events, on_event) = recorder();
        fx.engine.remove(&a.id, true, &on_event).await.unwrap();
        assert!(!done.final_path.exists());
        assert!(matches!(
            fx.engine.get(&a.id, &|_| {}),
            Err(EngineError::NotFound(_))
        ));
        let es = events.lock().unwrap().clone();
        assert!(matches!(
            &es[0],
            EngineEvent::Removed { id, queue_id } if *id == a.id && queue_id == DEFAULT_QUEUE_ID
        ));
        assert!(matches!(&es[1], EngineEvent::GroupChanged { id, group: None } if *id == b.id));
        assert!(fx.engine.list_groups().unwrap().is_empty());
        assert_eq!(fx.engine.speed_history(Some(&a.id)).unwrap(), []);

        // An unfinished one, with its part file and sidecar.
        let (tx, stop) = watch::channel(false);
        let stop_on_bytes = move |e: EngineEvent| {
            if matches!(e, EngineEvent::Progress { downloaded, .. } if downloaded > 0) {
                tx.send_replace(true);
            }
        };
        let paused = fx.engine.run(&c.id, &stop_on_bytes, &stop).await.unwrap();
        assert_eq!(paused.state, State::Paused);
        assert!(paused.part_path.exists());
        fx.engine.remove(&c.id, true, &|_| {}).await.unwrap();
        assert!(!paused.part_path.exists() && !paused.sidecar_path().exists());
        assert!(matches!(
            fx.engine.remove(&c.id, false, &|_| {}).await,
            Err(EngineError::NotFound(_))
        ));

        // Without `delete_files` nothing on disk is touched.
        let done = fx.run(&b.id).await;
        fx.engine.remove(&b.id, false, &|_| {}).await.unwrap();
        assert!(done.final_path.exists());
        assert!(fx.engine.list(&|_| {}).unwrap().is_empty());
        assert_eq!(fx.files(), ["Show.Part2.mkv"]);
    }

    /// Without tokens, a file of another size at the final path isn't the
    /// download's.
    #[tokio::test(flavor = "multi_thread")]
    async fn remove_without_tokens_deletes_only_a_file_of_its_size() {
        let server = TestServer::start().unwrap();
        let fx = Fx::new();
        let add = async |path: &str| {
            let added = fx
                .engine
                .add(fx.request(&server, path), &|_| {})
                .await
                .unwrap();
            fx.run(&added.id).await
        };
        let a = add("/blob/1000?seed=1").await;
        std::fs::write(&a.final_path, b"replaced").unwrap();
        fx.engine.remove(&a.id, true, &|_| {}).await.unwrap();
        assert_eq!(std::fs::read(&a.final_path).unwrap(), b"replaced");
        let b = add("/blob/2000?seed=2").await;
        fx.engine.remove(&b.id, true, &|_| {}).await.unwrap();
        assert!(!b.final_path.exists());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn retry_requeues_only_a_failed_download() {
        let server = TestServer::start().unwrap();
        let fx = Fx::new();
        let path = "/blob/1000?fail=1&fail_mode=403";
        let f = fx
            .engine
            .add(fx.request(&server, path), &|_| {})
            .await
            .unwrap();
        assert_eq!(fx.run(&f.id).await.state, State::Failed);
        let (events, on_event) = recorder();
        let q = fx.engine.retry(&f.id, &on_event).unwrap();
        assert_eq!((q.state, q.fail_reason), (State::Queued, None));
        assert!(matches!(
            events.lock().unwrap()[..],
            [EngineEvent::StateChanged {
                from: State::Failed,
                to: State::Queued,
                ..
            }]
        ));
        // The server serves it now.
        assert_eq!(fx.run(&f.id).await.state, State::Completed);
        let again = fx.engine.retry(&f.id, &on_event).unwrap();
        assert_eq!(again.state, State::Completed);
        assert_eq!(events.lock().unwrap().len(), 1);
    }

    fn live_config() -> DownloadConfig {
        let mut c = fast();
        c.ramp.window = Duration::from_millis(100);
        c.ramp.min_chunk = 64 * 1024;
        c.ramp.min_steal_gain = Duration::from_millis(10);
        // Never saturated: this is about the override.
        c.ramp.min_gain = -1.0;
        c
    }

    struct Running {
        /// The connections its `Progress` reports.
        conns: watch::Receiver<u32>,
        stop: watch::Sender<bool>,
        task: tokio::task::JoinHandle<Snapshot>,
    }

    fn spawn_run(
        fx: &Arc<Fx>,
        id: &str,
        on_event: impl Fn(EngineEvent) + Send + Sync + 'static,
    ) -> Running {
        let (conns_tx, conns) = watch::channel(0);
        let (stop, stop_rx) = watch::channel(false);
        let (fx, id) = (fx.clone(), id.to_owned());
        let task = tokio::spawn(async move {
            let on_event = move |e: EngineEvent| {
                if let EngineEvent::Progress { connections, .. } = &e {
                    conns_tx.send_replace(*connections);
                }
                on_event(e);
            };
            fx.engine.run(&id, &on_event, &stop_rx).await.unwrap()
        });
        Running { conns, stop, task }
    }

    async fn reach(conns: &mut watch::Receiver<u32>, n: u32) {
        tokio::time::timeout(Duration::from_secs(60), conns.wait_for(|c| *c == n))
            .await
            .unwrap_or_else(|_| panic!("never reached {n} connections"))
            .unwrap();
    }

    async fn map_where(
        maps: &mut watch::Receiver<RangeMap>,
        total: u64,
        pred: impl Fn(&RangeMap) -> bool,
    ) -> RangeMap {
        let found = tokio::time::timeout(Duration::from_secs(60), maps.wait_for(pred))
            .await
            .map(|m| m.unwrap().clone());
        let Ok(m) = found else {
            panic!("no such map; the last: {:?}", *maps.borrow());
        };
        assert_partition(&m, total);
        m
    }

    /// Bytes no connection has received yet.
    fn left(m: &RangeMap) -> u64 {
        let pending: u64 = m.pending.iter().map(|r| r.end - r.start).sum();
        pending + m.connections.iter().map(|c| c.end - c.at).sum::<u64>()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn set_connections_applies_to_a_running_download() {
        let server = TestServer::start().unwrap();
        let fx = Arc::new(Fx::with(live_config()));
        let total = 24_000_000;
        let mut req = fx.request(&server, "/blob/24000000?seed=5&throttle=1000000");
        req.connections = Some(1);
        let added = fx.engine.add(req, &|_| {}).await.unwrap();
        fx.engine.watch_ranges(&added.id, true, &|_| {}).unwrap();
        let (map_tx, mut maps) = watch::channel(RangeMap::default());
        let mut run = spawn_run(&fx, &added.id, move |e: EngineEvent| {
            if let EngineEvent::RangeMap { map, .. } = e {
                map_tx.send_replace(map);
            }
        });
        reach(&mut run.conns, 1).await;

        let up = fx.engine.set_connections(&added.id, Some(4)).unwrap();
        assert_eq!(up.connections, Some(4));
        reach(&mut run.conns, 4).await;
        map_where(&mut maps, total, |m| m.connections.len() == 4).await;
        // Lowered: at once, not as chunks end (they would all have ended,
        // so with little left).
        fx.engine.set_connections(&added.id, Some(1)).unwrap();
        let one = map_where(&mut maps, total, |m| {
            m.connections.len() == 1 && left(m) > total / 2
        })
        .await;
        assert!(!one.pending.is_empty(), "the rest went back: {one:?}");

        // What the stopped connections had is intact.
        fx.engine.set_connections(&added.id, None).unwrap();
        let done = run.task.await.unwrap();
        assert_eq!(done.state, State::Completed);
        assert_eq!(fx.engine.running_controls(), 0, "gone with its run");
        assert_eq!(
            blake3::hash(&std::fs::read(&done.final_path).unwrap()),
            blob::digest(5, total)
        );
        assert!(matches!(
            fx.engine.set_connections("nope", Some(2)),
            Err(EngineError::NotFound(_))
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn watched_ranges_while_running_and_not() {
        let server = TestServer::start().unwrap();
        let fx = Arc::new(Fx::with(live_config()));
        let total = 6_000_000;
        let mut req = fx.request(&server, "/blob/6000000?seed=6&throttle=400000");
        req.connections = Some(3);
        let added = fx.engine.add(req, &|_| {}).await.unwrap();

        // Not running: one map at once, from the sidecar (none yet).
        let (events, on_event) = recorder();
        fx.engine.watch_ranges(&added.id, true, &on_event).unwrap();
        let fresh = RangeMap {
            total: Some(total),
            pending: vec![0..total],
            ..RangeMap::default()
        };
        assert_eq!(maps(&events), [fresh]);

        // Running: maps with its connections.
        let (map_tx, mut map_rx) = watch::channel(RangeMap::default());
        let sink = on_event.clone();
        let mut run = spawn_run(&fx, &added.id, move |e: EngineEvent| {
            if let EngineEvent::RangeMap { map, .. } = &e {
                map_tx.send_replace(map.clone());
            }
            sink(e);
        });
        reach(&mut run.conns, 3).await;
        let wait = map_rx.wait_for(|m| m.connections.len() == 3);
        let live = tokio::time::timeout(Duration::from_secs(30), wait)
            .await
            .expect("a map with three connections")
            .unwrap()
            .clone();
        assert_partition(&live, total);
        run.stop.send_replace(true);
        let paused = run.task.await.unwrap();
        assert_eq!(paused.state, State::Paused);

        // Its last map is the sidecar's: committed, no connections.
        let last = maps(&events).pop().unwrap();
        assert!(last.connections.is_empty());
        assert_partition(&last, total);
        let committed: u64 = last.done.iter().map(|r| r.end - r.start).sum();
        assert_eq!(committed, paused.downloaded);

        // Off: a run sends none.
        fx.engine.watch_ranges(&added.id, false, &on_event).unwrap();
        let before = maps(&events).len();
        let done = fx.run(&added.id).await;
        assert_eq!(done.state, State::Completed);
        assert_eq!(maps(&events).len(), before);
        fx.engine.watch_ranges(&added.id, true, &on_event).unwrap();
        let whole = RangeMap {
            total: Some(total),
            done: vec![0..total],
            ..RangeMap::default()
        };
        assert_eq!(maps(&events).pop().unwrap(), whole);
    }
}
