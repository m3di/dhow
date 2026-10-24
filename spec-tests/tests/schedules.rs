//! Queue schedules and run control through the CLI (ENGINE-SPEC §11).
//! Windows are built from the local clock, to the second.
#![cfg(unix)]

use std::time::{Duration, Instant};

use jiff::{SignedDuration, Zoned};
use spec_tests::{Sandbox, TestServer, Value, assert_blob, named};

/// `<day> HH:MM:SS-HH:MM:SS` for [now + from, now + to), local time.
fn window(from_s: i64, to_s: i64) -> String {
    let now = Zoned::now();
    let start = now.checked_add(SignedDuration::from_secs(from_s)).unwrap();
    let end = now.checked_add(SignedDuration::from_secs(to_s)).unwrap();
    let day = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"]
        [start.weekday().to_monday_zero_offset() as usize];
    format!(
        "{day} {}-{}",
        start.strftime("%H:%M:%S"),
        end.strftime("%H:%M:%S")
    )
}

fn queue(sb: &Sandbox, args: &[&str]) -> String {
    let mut all = vec!["queue", "add", "Q"];
    all.extend_from_slice(args);
    let out = sb.dhow(all);
    out.assert_success();
    out.events()[0]["id"].as_str().unwrap().to_owned()
}

fn transitions(events: &[Value], id: &str) -> Vec<String> {
    events
        .iter()
        .filter(|e| e["event"] == "state" && e["id"] == id)
        .map(|e| e["to"].as_str().unwrap().to_owned())
        .collect()
}

/// T-11: a `stop-outside` window ends mid-transfer: the download pauses
/// with its progress committed, goes back to the queue, and resumes from
/// there when the next window opens.
#[test]
fn t_11_window_end_pauses_and_next_start_resumes() {
    let server = TestServer::start().unwrap();
    let sb = Sandbox::new();
    queue(
        &sb,
        &[
            "--window",
            &window(-60, 3),
            "--window",
            &window(6, 3600),
            "--action",
            "stop-outside",
        ],
    );
    let path = format!("/blob/3000000?seed=4&throttle=400000&{}", named("t.bin"));
    let id = sb
        .add(&server, &path, &["--queue", "--to", "Q", "-c", "1"])
        .id();

    let started = Instant::now();
    let out = sb.dhow(["watch"]);
    out.assert_success();
    let events = out.events();
    assert_eq!(
        transitions(&events, &id),
        [
            "active",
            "paused",
            "queued",
            "active",
            "completing",
            "completed"
        ],
        "{events:#?}"
    );
    assert!(
        // Whole-second window times, computed before the setup above: the
        // second window opens 5–6 s after that, ≥ 4 s after `started`.
        started.elapsed() >= Duration::from_secs(4),
        "waited for the next window"
    );
    // The second run started from what the first committed.
    let second_active = events
        .iter()
        .rposition(|e| e["event"] == "state" && e["to"] == "active")
        .unwrap();
    let resumed_at = events[second_active..]
        .iter()
        .find(|e| e["event"] == "progress")
        .and_then(|e| e["downloaded"].as_u64())
        .unwrap();
    assert!(resumed_at > 0, "resumed from the journal");
    assert_blob(&sb.dl().join("t.bin"), 4, 3_000_000);
}

/// `start`: downloads start inside the window and run on past its end.
#[test]
fn start_windows_let_running_downloads_finish() {
    let server = TestServer::start().unwrap();
    let sb = Sandbox::new();
    queue(&sb, &["--window", &window(-60, 2), "--action", "start"]);
    let path = format!("/blob/2000000?throttle=500000&{}", named("s.bin"));
    let id = sb
        .add(&server, &path, &["--queue", "--to", "Q", "-c", "1"])
        .id();
    let out = sb.dhow(["watch"]);
    out.assert_success();
    assert_eq!(
        transitions(&out.events(), &id),
        ["active", "completing", "completed"]
    );
    assert_blob(&sb.dl().join("s.bin"), 0, 2_000_000);
}

/// Outside its windows a queue waits; `watch` sleeps until the edge.
#[test]
fn watch_sleeps_until_the_window_opens() {
    let server = TestServer::start().unwrap();
    let sb = Sandbox::new();
    queue(&sb, &["--window", &window(4, 3600)]);
    let id = sb
        .add(&server, "/blob/1000", &["--queue", "--to", "Q"])
        .id();
    let started = Instant::now();
    let out = sb.dhow(["watch"]);
    out.assert_success();
    let elapsed = started.elapsed();
    // Opens 3–4 s after the window was computed, before the setup above.
    assert!(elapsed >= Duration::from_secs(2), "{elapsed:?}");
    assert_eq!(
        transitions(&out.events(), &id).last().map(String::as_str),
        Some("completed")
    );
    assert!(sb.ls()[0]["state"] == "completed");
}

/// `queue stop` pauses what the queue runs and puts it back in the queue;
/// `queue start` lets it run again.
#[test]
fn queue_stop_and_start() {
    let server = TestServer::start().unwrap();
    let sb = Sandbox::new();
    queue(&sb, &[]);
    let path = format!("/blob/2000000?throttle=500000&{}", named("q.bin"));
    let id = sb
        .add(&server, &path, &["--queue", "--to", "Q", "-c", "1"])
        .id();
    let mut watch = sb.spawn(["watch"]);
    watch.wait_for(|e| e["event"] == "progress" && e["downloaded"].as_u64() > Some(0));
    sb.dhow(["queue", "stop", "Q"]).assert_success();
    let out = watch.wait();
    out.assert_success();
    assert_eq!(
        transitions(&out.events(), &id),
        ["active", "paused", "queued"]
    );
    // Stopped without a schedule: nothing more to do until it's started.
    sb.dhow(["queue", "start", "Q"]).assert_success();
    sb.dhow(["watch"]).assert_success();
    assert_blob(&sb.dl().join("q.bin"), 0, 2_000_000);
}

/// Pausing one of several downloads a `watch` runs leaves the rest alone.
#[test]
fn pausing_one_download_in_watch_leaves_the_others_running() {
    let server = TestServer::start().unwrap();
    let sb = Sandbox::new();
    let slow = |n: &str| format!("/blob/2000000?throttle=500000&{}", named(n));
    let a = sb
        .add(&server, &slow("a.bin"), &["--queue", "-c", "1"])
        .id();
    let b = sb
        .add(&server, &slow("b.bin"), &["--queue", "-c", "1"])
        .id();
    let mut watch = sb.spawn(["watch"]);
    for id in [&a, &b] {
        let id = id.clone();
        watch.wait_for(move |e| {
            e["event"] == "state" && e["to"] == "active" && e["id"] == id.as_str()
        });
    }
    sb.dhow(["pause", &a]).assert_success();
    let out = watch.wait();
    out.assert_success();
    let ls = sb.ls();
    let state = |id: &str| ls.iter().find(|r| r["id"] == id).unwrap()["state"].clone();
    assert_eq!(state(&a), "paused", "the user's pause sticks");
    assert_eq!(state(&b), "completed");
    assert_blob(&sb.dl().join("b.bin"), 0, 2_000_000);
}

/// A `watch` waiting for a window picks up new work as soon as it's added.
#[test]
fn an_idle_watch_is_poked_by_new_work() {
    let server = TestServer::start().unwrap();
    let sb = Sandbox::new();
    queue(&sb, &["--window", &window(3600, 7200)]);
    sb.add(&server, "/blob/1000", &["--queue", "--to", "Q"]);
    let mut watch = sb.spawn(["watch"]);
    std::thread::sleep(Duration::from_millis(500));
    let added = Instant::now();
    let id = sb.add(&server, "/blob/2000", &["--queue"]).id();
    let id2 = id.clone();
    watch.wait_for(move |e| {
        e["event"] == "state" && e["to"] == "completed" && e["id"] == id2.as_str()
    });
    assert!(
        added.elapsed() < Duration::from_secs(3),
        "{:?}",
        added.elapsed()
    );
    watch.kill9();
}
