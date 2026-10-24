//! ROADMAP M1 exit: a `kill -9` at any random point during 100 runs never
//! yields a bad final file (ENGINE-SPEC I1–I3, I7, §8).
//!
//! `DHOW_KILL_RUNS` changes the count; `DHOW_KILL_SEED` replays a run.

use std::time::Duration;

use spec_tests::{Sandbox, TestServer, assert_blob, named};

const SIZE: u64 = 1_000_000;

#[test]
fn random_kill9_never_yields_a_bad_final_file() {
    let runs: u64 = std::env::var("DHOW_KILL_RUNS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100);
    let seed: u64 = std::env::var("DHOW_KILL_SEED")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| fastrand::u64(..));
    println!("DHOW_KILL_SEED={seed}");
    let mut rng = fastrand::Rng::with_seed(seed);
    let server = TestServer::start().unwrap();
    let (mut before_add, mut mid, mut after) = (0, 0, 0);

    for run_no in 0..runs {
        let sb = Sandbox::new();
        // 1 MB at 4 MB/s: ~250 ms of transfer after start-up.
        let path = format!(
            "/blob/{SIZE}?seed={run_no}&throttle=4000000&{}",
            named("k.bin")
        );
        let run = sb.spawn_add(&server, &path, &[]);
        let delay = rng.u64(0..450);
        std::thread::sleep(Duration::from_millis(delay));
        run.kill9();
        let ctx = format!("run {run_no}, killed after {delay} ms (DHOW_KILL_SEED={seed})");

        // I1: a file at the final name is always the complete, correct file.
        let files = sb.files();
        if files.iter().any(|f| f == "k.bin") {
            assert!(
                !files.iter().any(|f| f == "k.bin.part"),
                "{ctx}: final and part both exist"
            );
            assert_blob(&sb.dl().join("k.bin"), run_no, SIZE);
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
        assert_blob(&sb.dl().join("k.bin"), run_no, SIZE);
        assert_eq!(sb.ls()[0]["state"], "completed", "{ctx}");
    }

    println!("killed before add: {before_add}, mid-download: {mid}, after completion: {after}");
    assert!(
        mid * 4 >= runs,
        "too few kills landed mid-download ({mid}/{runs}); the test is not testing"
    );
}
