//! T-idle through the FFI engine (CLAUDE.md "idle means idle",
//! ARCHITECTURE §7): once its downloads are done, the engine's runtime has
//! no live task (no runner, no schedule sleeper) and nothing reaches the
//! listener. The core's T-idle watches RSS over the spec's 60 s; this one
//! watches tasks and events for 10 s unless `DHOW_IDLE_SECS` says
//! otherwise.

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use dhow_ffi::*;
use dhow_testserver::TestServer;

fn idle_for() -> Duration {
    let secs = std::env::var("DHOW_IDLE_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);
    Duration::from_secs(secs)
}

fn all_day() -> TimeOfDay {
    TimeOfDay {
        hour: 0,
        minute: 0,
        second: 0,
    }
}

#[test]
fn t_idle_ffi_nothing_runs_after_the_last_download() {
    let server = TestServer::start().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let dl = dir.path().join("dl");
    std::fs::create_dir(&dl).unwrap();
    let events = Arc::new(Recorder::default());
    let engine = open(&dir.path().join("data"), events.clone(), None);
    assert_eq!(engine.alive_tasks(), 0, "a fresh engine runs nothing");

    // A scheduled queue (its edges keep a sleeper while work waits) with a
    // limit, a download started by hand, two the queue starts.
    let week = vec![
        Weekday::Monday,
        Weekday::Tuesday,
        Weekday::Wednesday,
        Weekday::Thursday,
        Weekday::Friday,
        Weekday::Saturday,
        Weekday::Sunday,
    ];
    let q = engine
        .create_queue(QueueSpec {
            name: "always".into(),
            max_concurrent: 2,
            speed_limit: Some(4 << 20),
            schedule: Some(Schedule {
                windows: vec![ScheduleWindow {
                    days: week,
                    start: all_day(),
                    end: all_day(),
                }],
                action: ScheduleAction::StartAndStopOutside,
            }),
            on_complete: OnComplete::Nothing,
        })
        .unwrap();
    let mut ids = Vec::new();
    for seed in 0..3 {
        let mut req = request(
            server.url(&format!("/blob/1500000?seed={seed}&throttle=1000000")),
            &dl,
            &format!("{seed}.bin"),
            if seed == 0 {
                StartMode::Now
            } else {
                StartMode::Queued
            },
        );
        req.queue = Some(q.id.clone());
        ids.push(block(engine.add(req)).unwrap().id);
    }
    // Its segment map watched, and left on.
    engine.watch_ranges(ids[0].clone(), true).unwrap();
    for id in &ids {
        events.wait_state(id, DownloadState::Completed);
    }

    // Runs end, and the sleeper notices nothing waits, just after.
    let deadline = Instant::now() + Duration::from_secs(5);
    while engine.alive_tasks() > 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(
        engine.alive_tasks(),
        0,
        "tasks alive after the last download"
    );

    let seen = events.len();
    let step = idle_for() / 5;
    for _ in 0..5 {
        std::thread::sleep(step);
        assert_eq!(engine.alive_tasks(), 0, "a task came back");
    }
    let extra = events.events()[seen..].to_vec();
    assert!(extra.is_empty(), "events while idle: {extra:?}");

    // Queries don't wake anything either.
    engine.list().unwrap();
    engine.storage_breakdown().unwrap();
    engine.list_queues().unwrap();
    engine.watch_ranges(ids[1].clone(), true).unwrap();
    assert_eq!(engine.alive_tasks(), 0);
}
