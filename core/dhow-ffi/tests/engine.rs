//! The FFI engine end to end against dhow-testserver: commands, the
//! in-process runner, events and the adapter (ARCHITECTURE §3, §4, §9).

mod common;

use std::path::Path;
use std::sync::Arc;

use common::*;
use dhow_ffi::*;
use dhow_testserver::{TestServer, blob};

struct Setup {
    server: TestServer,
    dir: tempfile::TempDir,
}

impl Setup {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("dl")).unwrap();
        Self {
            server: TestServer::start().unwrap(),
            dir,
        }
    }

    fn data(&self) -> std::path::PathBuf {
        self.dir.path().join("data")
    }

    fn dl(&self) -> std::path::PathBuf {
        self.dir.path().join("dl")
    }

    fn req(&self, path: &str, name: &str, start: StartMode) -> AddRequest {
        request(self.server.url(path), &self.dl(), name, start)
    }
}

fn assert_blob(path: &Path, seed: u64, size: u64) {
    let got = std::fs::read(path).unwrap();
    assert!(
        got == blob::bytes(seed, 0..size),
        "{} differs",
        path.display()
    );
}

#[test]
fn add_now_runs_to_completion() {
    let s = Setup::new();
    let events = Arc::new(Recorder::default());
    let adapter = Arc::new(Adapter::default());
    let engine = open(&s.data(), events.clone(), Some(adapter.clone()));

    let added = block(engine.add(s.req("/blob/300000?seed=1", "a.bin", StartMode::Now))).unwrap();
    assert_eq!(added.category, Category::Software);
    events.wait_state(&added.id, DownloadState::Completed);
    let done = engine.get(added.id.clone()).unwrap();
    assert_eq!(done.state, DownloadState::Completed);
    assert_eq!(done.downloaded, 300_000);
    assert_blob(Path::new(&done.final_path), 1, 300_000);

    // In order, on the listener: running, completing, completed, renamed.
    events.wait("renamed", |es| {
        es.iter()
            .any(|e| matches!(e, Event::Renamed { id, .. } if *id == added.id))
    });
    let es = events.events();
    let at = |pred: &dyn Fn(&Event) -> bool| es.iter().position(pred).unwrap();
    let active = at(&|e| is_state(e, &added.id, DownloadState::Active));
    let completing = at(&|e| is_state(e, &added.id, DownloadState::Completing));
    let completed = at(&|e| is_state(e, &added.id, DownloadState::Completed));
    let renamed = at(&|e| matches!(e, Event::Renamed { .. }));
    assert!(active < completing && completing < completed && completed < renamed);
    assert!(es.iter().any(|e| matches!(
        e,
        Event::Progress {
            downloaded: 300_000,
            total: Some(300_000),
            ..
        }
    )));

    // The adapter: kept awake while it ran (let go as the run ends, after
    // its last event), a token for the final file.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while adapter.awake.lock().unwrap().len() < 2 && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(*adapter.awake.lock().unwrap(), [true, false]);
    assert_eq!(
        *adapter.tokens.lock().unwrap(),
        std::slice::from_ref(&done.final_path)
    );
    let net = engine.network_info().unwrap();
    assert_eq!(net.interface_kind, InterfaceKind::Wifi);
    assert_eq!(net.proxy_in_use, ProxyMode::Direct);
    assert_eq!(engine.list().unwrap(), [done]);
}

#[test]
fn pause_and_resume() {
    let s = Setup::new();
    let events = Arc::new(Recorder::default());
    let engine = open(&s.data(), events.clone(), None);
    let path = "/blob/1000000?seed=2&throttle=400000";
    let id = block(engine.add(s.req(path, "b.bin", StartMode::Now)))
        .unwrap()
        .id;
    events.wait_progress(&id);

    let paused = block(engine.pause(id.clone())).unwrap();
    assert_eq!(paused.state, DownloadState::Paused);
    assert!(paused.downloaded < 1_000_000);
    assert!(Path::new(&paused.part_path).exists());

    let resumed = engine.resume(id.clone()).unwrap();
    assert_eq!(resumed.state, DownloadState::Paused, "as before it runs");
    events.wait_state(&id, DownloadState::Completed);
    assert_blob(Path::new(&engine.get(id).unwrap().final_path), 2, 1_000_000);
}

#[test]
fn cancel_a_running_download() {
    let s = Setup::new();
    let events = Arc::new(Recorder::default());
    let engine = open(&s.data(), events.clone(), None);
    let path = "/blob/2000000?seed=3&throttle=200000";
    let added = block(engine.add(s.req(path, "c.bin", StartMode::Now))).unwrap();
    events.wait_progress(&added.id);

    let cancelled = block(engine.cancel(added.id.clone(), true)).unwrap();
    assert_eq!(cancelled.state, DownloadState::Cancelled);
    assert!(!Path::new(&added.part_path).exists());
    assert!(!Path::new(&added.final_path).exists());
    // Not started again by the queue.
    let n = events.len();
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(
        !events.events()[n..]
            .iter()
            .any(|e| is_state(e, &added.id, DownloadState::Active))
    );
}

#[test]
fn queues_run_downloads_up_to_their_concurrency() {
    let s = Setup::new();
    let events = Arc::new(Recorder::default());
    let engine = open(&s.data(), events.clone(), None);
    let q = engine
        .create_queue(QueueSpec {
            name: "one at a time".into(),
            max_concurrent: 1,
            speed_limit: None,
            schedule: None,
            on_complete: OnComplete::Nothing,
        })
        .unwrap();
    let mut ids = Vec::new();
    for seed in 10..13 {
        let mut req = s.req(
            &format!("/blob/200000?seed={seed}&throttle=1000000"),
            &format!("{seed}.bin"),
            StartMode::Queued,
        );
        req.queue = Some(q.id.clone());
        ids.push(block(engine.add(req)).unwrap().id);
    }
    for id in &ids {
        events.wait_state(id, DownloadState::Completed);
    }
    // Never two at once, oldest first.
    let mut active = 0;
    let mut order = Vec::new();
    for e in events.events() {
        if let Event::StateChanged { id, from, to, .. } = e {
            if to == DownloadState::Active {
                active += 1;
                order.push(id);
            } else if from == DownloadState::Active {
                active -= 1;
            }
            assert!(active <= 1, "two downloads of a 1-queue ran at once");
        }
    }
    assert_eq!(order, ids);
}

#[test]
fn stop_queue_requeues_and_resume_schedule_continues() {
    let s = Setup::new();
    let events = Arc::new(Recorder::default());
    let engine = open(&s.data(), events.clone(), None);
    let q = engine
        .create_queue(QueueSpec {
            name: "q".into(),
            max_concurrent: 2,
            speed_limit: None,
            schedule: None,
            on_complete: OnComplete::Nothing,
        })
        .unwrap();
    let mut ids = Vec::new();
    for seed in 20..22 {
        let mut req = s.req(
            &format!("/blob/1000000?seed={seed}&throttle=300000"),
            &format!("{seed}.bin"),
            // One started by hand: stopping its queue stops it too.
            if seed == 20 {
                StartMode::Now
            } else {
                StartMode::Queued
            },
        );
        req.queue = Some(q.id.clone());
        ids.push(block(engine.add(req)).unwrap().id);
    }
    for id in &ids {
        events.wait_progress(id);
    }

    let stopped = engine.stop_queue(q.id.clone()).unwrap();
    assert_eq!(stopped.run_state, RunState::Stopped);
    for id in &ids {
        events.wait_state(id, DownloadState::Queued);
        assert_eq!(engine.get(id.clone()).unwrap().state, DownloadState::Queued);
    }

    engine.resume_schedule(q.id.clone()).unwrap();
    for id in &ids {
        events.wait_state(id, DownloadState::Completed);
    }
    for (id, seed) in ids.iter().zip(20..) {
        assert_blob(
            Path::new(&engine.get(id.clone()).unwrap().final_path),
            seed,
            1_000_000,
        );
    }
}

#[test]
fn settings_round_trip() {
    let s = Setup::new();
    let events = Arc::new(Recorder::default());
    let engine = open(&s.data(), events.clone(), None);
    let before = engine.settings().unwrap();
    assert_eq!(before.proxy_mode, ProxyMode::System);
    assert_eq!(before.global_speed_limit, None);

    let proxy = ProxyConfig {
        http: Some("http://user:pw@proxy.test:3128/".into()),
        ..ProxyConfig::default()
    };
    let after = engine
        .set_settings(SettingsPatch {
            global_speed_limit: Some(LimitPatch::Rate {
                bytes_per_sec: 1 << 20,
            }),
            headroom_percent: Some(20),
            extra_extensions: vec![CategoryExtensions {
                category: Category::Data,
                extensions: vec!["safetensors".into()],
            }],
            proxy_mode: Some(ProxyMode::Custom {
                config: proxy.clone(),
            }),
            ..SettingsPatch::default()
        })
        .unwrap();
    assert_eq!(after.global_speed_limit, Some(1 << 20));
    assert_eq!(after.headroom_percent, 20);
    assert_eq!(
        after.extra_extensions,
        [CategoryExtensions {
            category: Category::Data,
            extensions: vec!["safetensors".into()],
        }]
    );
    assert_eq!(after.proxy_mode, ProxyMode::Custom { config: proxy });
    assert_eq!(engine.settings().unwrap(), after);
    events.wait("settings changed", |es| {
        es.iter()
            .any(|e| matches!(e, Event::SettingsChanged { settings } if *settings == after))
    });

    // All or nothing: one bad field and nothing changes.
    let bad = engine.set_settings(SettingsPatch {
        global_speed_limit: Some(LimitPatch::Unlimited),
        headroom_percent: Some(100),
        ..SettingsPatch::default()
    });
    assert!(
        matches!(bad, Err(DhowError::InvalidSetting { .. })),
        "{bad:?}"
    );
    let bad_url = engine.set_settings(SettingsPatch {
        proxy_mode: Some(ProxyMode::Custom {
            config: ProxyConfig {
                http: Some("not a url".into()),
                ..ProxyConfig::default()
            },
        }),
        ..SettingsPatch::default()
    });
    assert!(matches!(bad_url, Err(DhowError::InvalidArgument { .. })));
    assert_eq!(engine.settings().unwrap(), after);

    engine
        .set_settings(SettingsPatch {
            global_speed_limit: Some(LimitPatch::Unlimited),
            proxy_mode: Some(ProxyMode::Direct),
            ..SettingsPatch::default()
        })
        .unwrap();
    let s = engine.settings().unwrap();
    assert_eq!(
        (s.global_speed_limit, s.proxy_mode),
        (None, ProxyMode::Direct)
    );
}

#[test]
fn queue_schedules_and_errors_cross_the_boundary() {
    let s = Setup::new();
    let engine = open(&s.data(), Arc::new(Recorder::default()), None);
    let schedule = Schedule {
        windows: vec![ScheduleWindow {
            days: vec![Weekday::Monday, Weekday::Friday],
            start: TimeOfDay {
                hour: 22,
                minute: 0,
                second: 0,
            },
            end: TimeOfDay {
                hour: 6,
                minute: 30,
                second: 15,
            },
        }],
        action: ScheduleAction::StartAndStopOutside,
    };
    let q = engine
        .create_queue(QueueSpec {
            name: "night".into(),
            max_concurrent: 2,
            speed_limit: Some(1000),
            schedule: Some(schedule.clone()),
            on_complete: OnComplete::Sleep,
        })
        .unwrap();
    assert_eq!(q.schedule, Some(schedule));
    assert_eq!(engine.get_queue(q.id.clone()).unwrap(), q);
    let queues = engine.list_queues().unwrap();
    assert!(queues[0].is_default && queues[0].name.is_none());
    assert_eq!(queues.len(), 2);

    let no_days = QueueSpec {
        name: "x".into(),
        max_concurrent: 1,
        speed_limit: None,
        schedule: Some(Schedule {
            windows: vec![ScheduleWindow {
                days: vec![],
                start: TimeOfDay {
                    hour: 1,
                    minute: 0,
                    second: 0,
                },
                end: TimeOfDay {
                    hour: 2,
                    minute: 0,
                    second: 0,
                },
            }],
            action: ScheduleAction::Start,
        }),
        on_complete: OnComplete::Nothing,
    };
    assert!(matches!(
        engine.create_queue(no_days),
        Err(DhowError::InvalidQueue { .. })
    ));
    let default = queues[0].id.clone();
    assert_eq!(
        engine.delete_queue(default.clone(), q.id.clone()),
        Err(DhowError::DefaultQueue)
    );
    assert!(matches!(
        engine.get("nope".into()),
        Err(DhowError::NotFound { .. })
    ));
    let probe = block(engine.add(s.req(
        "/blob/10?head=404&fail=9&fail_mode=404",
        "x",
        StartMode::Queued,
    )));
    assert_eq!(
        probe,
        Err(DhowError::Probe {
            reason: ProbeFailure::Status { status: 404 }
        })
    );
    engine.delete_queue(q.id, default).unwrap();
}

#[test]
fn dropping_the_engine_mid_download_loses_nothing() {
    let s = Setup::new();
    let events = Arc::new(Recorder::default());
    let engine = open(&s.data(), events.clone(), None);
    let path = "/blob/1000000?seed=4&throttle=300000";
    let id = block(engine.add(s.req(path, "d.bin", StartMode::Now)))
        .unwrap()
        .id;
    events.wait_progress(&id);
    drop(engine);
    assert_eq!(dhow_core::runlock::holder(&s.data(), &id).unwrap(), None);

    // The next engine continues it from its journal.
    let events = Arc::new(Recorder::default());
    let engine = open(&s.data(), events.clone(), None);
    events.wait_state(&id, DownloadState::Completed);
    assert_blob(Path::new(&engine.get(id).unwrap().final_path), 4, 1_000_000);
}

#[test]
fn shutdown_stops_runs_and_requeues_them() {
    let s = Setup::new();
    let events = Arc::new(Recorder::default());
    let engine = open(&s.data(), events.clone(), None);
    let path = "/blob/2000000?seed=5&throttle=200000";
    let id = block(engine.add(s.req(path, "e.bin", StartMode::Now)))
        .unwrap()
        .id;
    events.wait_progress(&id);
    // The run lock carries no PID: the CLI must not signal an app.
    assert_eq!(dhow_core::runlock::holder(&s.data(), &id).unwrap(), Some(0));

    block(engine.shutdown());
    let snap = engine.get(id.clone()).unwrap();
    assert_eq!(snap.state, DownloadState::Queued);
    assert!(snap.downloaded > 0);
    assert_eq!(engine.resume(id.clone()), Err(DhowError::ShutDown));
    assert_eq!(dhow_core::runlock::holder(&s.data(), &id).unwrap(), None);
}

#[test]
fn a_download_another_process_runs_is_left_alone() {
    let s = Setup::new();
    let events = Arc::new(Recorder::default());
    let engine = open(&s.data(), events.clone(), None);
    let id = block(engine.add(s.req("/blob/1000?seed=6", "f.bin", StartMode::Paused)))
        .unwrap()
        .id;
    // As `dhow resume` would hold it.
    let cli = match dhow_core::runlock::acquire(&s.data(), &id, Some(4242)) {
        Ok(lock) => lock,
        Err(_) => panic!("lock taken"),
    };
    assert_eq!(
        engine.resume(id.clone()),
        Err(DhowError::RunningElsewhere { id: id.clone() })
    );
    assert_eq!(
        block(engine.pause(id.clone())),
        Err(DhowError::RunningElsewhere { id: id.clone() })
    );
    drop(cli);
    engine.resume(id.clone()).unwrap();
    events.wait_state(&id, DownloadState::Completed);
}
