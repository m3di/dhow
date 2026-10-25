//! The calls and events the macOS app (M4.2) needs besides #36's:
//! probe and blind mode, remove, retry, live connections, the segment map,
//! and the events that keep a shell's list current (ARCHITECTURE §3, §9).

mod common;

use std::path::{Path, PathBuf};
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

    fn data(&self) -> PathBuf {
        self.dir.path().join("data")
    }

    fn dl(&self) -> PathBuf {
        self.dir.path().join("dl")
    }

    fn req(&self, path: &str, name: &str, start: StartMode) -> AddRequest {
        request(self.server.url(path), &self.dl(), name, start)
    }
}

fn assert_blob(path: &str, seed: u64, size: u64) {
    assert!(
        std::fs::read(path).unwrap() == blob::bytes(seed, 0..size),
        "{path} differs"
    );
}

fn queue_changes(events: &[Event], queue: &str) -> Vec<QueueSnapshot> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::QueueChanged { snapshot } if snapshot.id == queue => Some(snapshot.clone()),
            _ => None,
        })
        .collect()
}

fn default_queue(engine: &Engine) -> String {
    engine.list_queues().unwrap()[0].id.clone()
}

#[test]
fn probe_then_add_blind_when_it_fails() {
    let s = Setup::new();
    let events = Arc::new(Recorder::default());
    let engine = open(&s.data(), events.clone(), None);

    let ok = block(engine.probe(s.server.url("/blob/4000?seed=1"), vec![])).unwrap();
    assert_eq!(ok.suggested_name, "4000");
    assert_eq!(ok.total_size, Some(4000));
    assert_eq!(ok.resumable, Resumable::Yes);
    assert_eq!(ok.category, Category::Other);
    assert!(ok.etag.is_some());

    // Both probe requests refused, twice (here and at the run); the plain
    // GET works: "Download anyway" (ENGINE-SPEC §2.4).
    let path = "/blob/200000?seed=2&head=403&fail=2&fail_mode=403";
    assert_eq!(
        block(engine.probe(s.server.url(path), vec![])),
        Err(DhowError::Probe {
            reason: ProbeFailure::Status { status: 403 }
        })
    );
    let mut req = s.req(path, "blind.bin", StartMode::Now);
    req.blind = true;
    let added = block(engine.add(req)).unwrap();
    assert_eq!(added.total_size, None);
    assert_eq!(added.resumable, Resumable::No);
    events.wait_state(&added.id, DownloadState::Completed);
    assert_blob(&engine.get(added.id).unwrap().final_path, 2, 200_000);
    assert!(
        engine.list().unwrap().len() == 1,
        "the probe recorded nothing"
    );
}

#[test]
fn remove_a_running_download_and_its_files() {
    let s = Setup::new();
    let events = Arc::new(Recorder::default());
    let engine = open(&s.data(), events.clone(), None);
    let path = "/blob/2000000?seed=3&throttle=200000";
    let added = block(engine.add(s.req(path, "r.bin", StartMode::Now))).unwrap();
    events.wait_progress(&added.id);
    assert!(Path::new(&added.part_path).exists());

    block(engine.remove(added.id.clone(), true)).unwrap();
    assert!(!Path::new(&added.part_path).exists());
    assert!(engine.list().unwrap().is_empty());
    assert_eq!(
        engine.get(added.id.clone()),
        Err(DhowError::NotFound {
            id: added.id.clone()
        })
    );
    // Then its queue's counts.
    let default = default_queue(&engine);
    events.wait("removed, then the queue", |es| {
        es.iter()
            .position(|e| matches!(e, Event::Removed { id } if *id == added.id))
            .is_some_and(|at| {
                queue_changes(&es[at..], &default)
                    .iter()
                    .any(|q| (q.unfinished, q.queued, q.running) == (0, 0, 0))
            })
    });

    // A completed one, its file kept.
    let done = block(engine.add(s.req("/blob/1000?seed=4", "k.bin", StartMode::Now))).unwrap();
    events.wait_state(&done.id, DownloadState::Completed);
    block(engine.remove(done.id.clone(), false)).unwrap();
    assert!(Path::new(&done.final_path).exists());
    assert_eq!(
        block(engine.remove(done.id.clone(), false)),
        Err(DhowError::NotFound { id: done.id })
    );
}

#[test]
fn retry_puts_a_failed_download_back_in_its_queue() {
    let s = Setup::new();
    let events = Arc::new(Recorder::default());
    let engine = open(&s.data(), events.clone(), None);
    let path = "/blob/100000?seed=5&fail=1&fail_mode=403";
    let added = block(engine.add(s.req(path, "f.bin", StartMode::Queued))).unwrap();
    events.wait_state(&added.id, DownloadState::Failed);
    assert_eq!(
        engine.get(added.id.clone()).unwrap().state,
        DownloadState::Failed
    );

    let retried = engine.retry(added.id.clone()).unwrap();
    assert_eq!(retried.state, DownloadState::Queued);
    assert_eq!(retried.fail_reason, None);
    events.wait_state(&added.id, DownloadState::Completed);
    assert_blob(&engine.get(added.id).unwrap().final_path, 5, 100_000);
}

#[test]
fn set_connections_while_it_runs() {
    let s = Setup::new();
    let events = Arc::new(Recorder::default());
    let engine = open(&s.data(), events.clone(), None);
    let path = "/blob/8000000?seed=6&throttle=500000";
    let mut req = s.req(path, "c.bin", StartMode::Now);
    req.connections = Some(1);
    let id = block(engine.add(req)).unwrap().id;
    let at = |n: u32| {
        let id = id.clone();
        move |es: &[Event]| {
            es.iter().any(
                |e| matches!(e, Event::Progress { id: i, connections, .. } if *i == id && *connections == n),
            )
        }
    };
    events.wait("one connection", at(1));
    let snap = engine.set_connections(id.clone(), Some(3)).unwrap();
    assert_eq!(snap.connections, Some(3));
    events.wait("three connections", at(3));
    let n = events.len();
    engine.set_connections(id.clone(), Some(1)).unwrap();
    events.wait("back to one", |es| at(1)(&es[n..]));
    block(engine.pause(id.clone())).unwrap();

    engine.set_connections(id.clone(), None).unwrap();
    engine.resume(id.clone()).unwrap();
    events.wait_state(&id, DownloadState::Completed);
    assert_blob(&engine.get(id).unwrap().final_path, 6, 8_000_000);
}

#[test]
fn watch_ranges_on_and_off() {
    let s = Setup::new();
    let events = Arc::new(Recorder::default());
    let engine = open(&s.data(), events.clone(), None);
    let total = 4_000_000;
    let path = "/blob/4000000?seed=7&throttle=400000";
    let mut req = s.req(path, "m.bin", StartMode::Paused);
    req.connections = Some(2);
    let id = block(engine.add(req)).unwrap().id;

    // Not running: one map, at once.
    engine.watch_ranges(id.clone(), true).unwrap();
    events.wait("a map", |es| {
        es.iter().any(|e| {
            matches!(e, Event::RangeMap { id: i, map } if *i == id
                && map.total == Some(total)
                && map.pending == [ByteRange { start: 0, end: total }])
        })
    });
    // Running: its connections in the map.
    engine.resume(id.clone()).unwrap();
    events.wait("a map with connections", |es| {
        es.iter().any(
            |e| matches!(e, Event::RangeMap { id: i, map } if *i == id && !map.connections.is_empty()),
        )
    });
    // Off (that no more come is the core's test: here it would be a race
    // with the listener thread).
    engine.watch_ranges(id.clone(), false).unwrap();
    events.wait_state(&id, DownloadState::Completed);
    assert_eq!(
        engine.watch_ranges("nope".into(), true),
        Err(DhowError::NotFound { id: "nope".into() })
    );
}

/// What another process (the CLI) records, the app announces when it
/// looks: at `list` and `recheck`.
#[test]
fn downloads_added_and_removed_elsewhere_are_announced() {
    let s = Setup::new();
    let events = Arc::new(Recorder::default());
    let engine = open(
        &s.data(),
        events.clone(),
        Some(Arc::new(Adapter::default())),
    );
    let mine = block(engine.add(s.req("/blob/10", "mine.bin", StartMode::Paused))).unwrap();
    let added = |es: &[Event], id: &str| {
        es.iter()
            .filter(|e| matches!(e, Event::DownloadAdded { snapshot } if snapshot.id == id))
            .count()
    };
    events.wait("mine added", |es| added(es, &mine.id) == 1);

    let cli = dhow_core::engine::Engine::open(&s.data(), Default::default()).unwrap();
    let req = dhow_core::engine::AddRequest {
        url: s.server.url("/blob/20").parse().unwrap(),
        headers: vec![],
        directory: s.dl(),
        filename: Some("theirs.bin".into()),
        paused: true,
        connections: None,
        queue: None,
        speed_limit: None,
        target_fs: dhow_core::naming::TargetFs::host(),
        blind: false,
    };
    let theirs = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(cli.add(req, &|_| {}))
        .unwrap();
    assert_eq!(engine.list().unwrap().len(), 2);
    engine.list().unwrap();
    let default = default_queue(&engine);
    events.wait("theirs added, then its queue", |es| {
        es.iter()
            .position(
                |e| matches!(e, Event::DownloadAdded { snapshot } if snapshot.id == theirs.id),
            )
            .is_some_and(|at| {
                queue_changes(&es[at..], &default)
                    .iter()
                    .any(|q| q.unfinished == 2)
            })
    });
    // A fence: everything before it is delivered.
    engine.network_changed().unwrap();
    events.wait("fence", |es| {
        es.iter().any(|e| matches!(e, Event::NetworkChanged { .. }))
    });
    let es = events.events();
    assert_eq!(added(&es, &theirs.id), 1, "once");
    assert_eq!(added(&es, &mine.id), 1);

    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(cli.remove(&theirs.id, false, &|_| {}))
        .unwrap();
    engine.recheck();
    events.wait("removed", |es| {
        es.iter()
            .any(|e| matches!(e, Event::Removed { id } if *id == theirs.id))
    });
}

#[test]
fn queue_changes_are_announced() {
    let s = Setup::new();
    let events = Arc::new(Recorder::default());
    let engine = open(&s.data(), events.clone(), None);
    let q = engine
        .create_queue(QueueSpec {
            name: "later".into(),
            max_concurrent: 1,
            speed_limit: None,
            schedule: None,
            on_complete: OnComplete::Sleep,
        })
        .unwrap();
    let stopped = engine.stop_queue(q.id.clone()).unwrap();
    assert_eq!(stopped.run_state, RunState::Stopped);
    let counts = |es: &[Event]| -> Vec<(RunState, u32, u32)> {
        queue_changes(es, &q.id)
            .iter()
            .map(|q| (q.run_state, q.queued, q.running))
            .collect()
    };
    events.wait("created, stopped", |es| {
        counts(es) == [(RunState::Auto, 0, 0), (RunState::Stopped, 0, 0)]
    });

    // Counts follow what is in it: queued while stopped, running, then
    // dry, which is when the shell does `on_complete`.
    let mut req = s.req(
        "/blob/300000?seed=8&throttle=600000",
        "q.bin",
        StartMode::Queued,
    );
    req.queue = Some(q.id.clone());
    let id = block(engine.add(req)).unwrap().id;
    events.wait("queued", |es| {
        counts(es).last() == Some(&(RunState::Stopped, 1, 0))
    });
    engine.resume_schedule(q.id.clone()).unwrap();
    events.wait_state(&id, DownloadState::Completed);
    events.wait("ran, then dry", |es| {
        let c = counts(es);
        c.contains(&(RunState::Auto, 0, 1)) && c.last() == Some(&(RunState::Auto, 0, 0))
    });
    assert_eq!(engine.get_queue(q.id.clone()).unwrap().unfinished, 0);

    let default = default_queue(&engine);
    engine.delete_queue(q.id.clone(), default).unwrap();
    events.wait("queue removed", |es| {
        es.iter()
            .any(|e| matches!(e, Event::QueueRemoved { id } if *id == q.id))
    });
}

#[test]
fn network_changed_asks_the_adapter() {
    let s = Setup::new();
    let events = Arc::new(Recorder::default());
    let engine = open(
        &s.data(),
        events.clone(),
        Some(Arc::new(Adapter::default())),
    );
    let info = engine.network_changed().unwrap();
    assert_eq!(info.interface_kind, InterfaceKind::Wifi);
    events.wait("network changed", |es| {
        es.iter()
            .any(|e| matches!(e, Event::NetworkChanged { info: i } if *i == info))
    });
}
