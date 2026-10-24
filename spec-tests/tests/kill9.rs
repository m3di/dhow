//! ROADMAP M1 exit: a `kill -9` at any random point during 100 runs never
//! yields a bad final file (ENGINE-SPEC I1–I3, I7, §8).
//!
//! `DHOW_KILL_RUNS` changes the count; `DHOW_KILL_SEED` replays a run.

use std::time::Duration;

use spec_tests::{Sandbox, TestServer, assert_blob, named};

#[test]
fn random_kill9_never_yields_a_bad_final_file() {
    kill_loop(Loop {
        runs_var: "DHOW_KILL_RUNS",
        runs: 100,
        // 1 MB at 4 MB/s: ~250 ms of transfer after start-up.
        size: 1_000_000,
        knobs: "throttle=4000000",
        max_delay_ms: 450,
        env: &[],
        multi: false,
    });
}

/// The same with several connections splitting work (§4.3): 2 MB at
/// 500 kB/s per connection, ramp-up every 50 ms, so kills land while
/// chunks are being split and stolen.
#[test]
fn random_kill9_with_several_connections() {
    kill_loop(Loop {
        runs_var: "DHOW_KILL_RUNS_MULTI",
        runs: 40,
        size: 2_000_000,
        knobs: "throttle=500000",
        max_delay_ms: 1500,
        env: &[("DHOW_RAMP_WINDOW_MS", "50"), ("DHOW_MIN_CHUNK_KIB", "256")],
        multi: true,
    });
}

struct Loop {
    runs_var: &'static str,
    runs: u64,
    size: u64,
    knobs: &'static str,
    max_delay_ms: u64,
    env: &'static [(&'static str, &'static str)],
    /// Assert that a fair share of kills hit several open connections.
    multi: bool,
}

fn kill_loop(l: Loop) {
    let runs: u64 = std::env::var(l.runs_var)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(l.runs);
    let seed: u64 = std::env::var("DHOW_KILL_SEED")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| fastrand::u64(..));
    println!("DHOW_KILL_SEED={seed}");
    let mut rng = fastrand::Rng::with_seed(seed);
    let server = TestServer::start().unwrap();
    let (mut before_add, mut mid, mut after, mut with_several) = (0, 0, 0, 0);
    let size = l.size;

    for run_no in 0..runs {
        let sb = Sandbox::with_env(l.env);
        let path = format!("/blob/{size}?seed={run_no}&{}&{}", l.knobs, named("k.bin"));
        let run = sb.spawn_add(&server, &path, &[]);
        let delay = rng.u64(0..l.max_delay_ms);
        std::thread::sleep(Duration::from_millis(delay));
        let seen = run.kill9();
        if seen
            .iter()
            .any(|e| e["event"] == "progress" && e["connections"].as_u64() >= Some(2))
        {
            with_several += 1;
        }
        let ctx = format!("run {run_no}, killed after {delay} ms (DHOW_KILL_SEED={seed})");

        // I1: a file at the final name is always the complete, correct file.
        let files = sb.files();
        if files.iter().any(|f| f == "k.bin") {
            assert!(
                !files.iter().any(|f| f == "k.bin.part"),
                "{ctx}: final and part both exist"
            );
            assert_blob(&sb.dl().join("k.bin"), run_no, size);
        }

        let rows = sb.ls();
        match rows.as_slice() {
            [] => {
                // Killed before the download was recorded: nothing may exist.
                assert!(
                    files.is_empty(),
                    "{ctx}: files without a download: {files:?}"
                );
                before_add += 1;
                continue;
            }
            [row] => {
                if row["state"] == "completed" {
                    after += 1;
                } else {
                    mid += 1;
                }
                let id = row["id"].as_str().unwrap();
                let out = sb.dhow(["resume", id]);
                assert!(out.status.success(), "{ctx}: resume failed: {out:?}");
            }
            more => panic!("{ctx}: {} downloads recorded", more.len()),
        }
        assert_eq!(sb.files(), ["k.bin"], "{ctx}");
        assert_blob(&sb.dl().join("k.bin"), run_no, size);
        assert_eq!(sb.ls()[0]["state"], "completed", "{ctx}");
    }

    println!(
        "killed before add: {before_add}, mid-download: {mid}, after completion: {after}, \
         with several connections open: {with_several}"
    );
    assert!(
        mid * 4 >= runs,
        "too few kills landed mid-download ({mid}/{runs}); the test is not testing"
    );
    if l.multi {
        assert!(
            with_several * 3 >= runs,
            "too few kills hit several open connections ({with_several}/{runs}); the test is not testing"
        );
    }
}
