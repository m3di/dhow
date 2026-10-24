//! Speed limits through the CLI (ENGINE-SPEC §12).
#![cfg(unix)]

use std::time::{Duration, Instant};

use spec_tests::{Sandbox, TestServer, assert_blob, named};

const KIB: u64 = 1024;

/// T-12: a global limit of 100 KiB/s over 3 downloads at once: the
/// aggregate stays within ±10 % over 10 s.
#[test]
fn t_12_global_limit_holds_the_aggregate() {
    let server = TestServer::start().unwrap();
    let sb = Sandbox::new();
    let out = sb.dhow(["settings", "--global-limit", "100K"]);
    out.assert_success();
    assert_eq!(out.events()[0]["global_speed_limit"], 100 * KIB);
    // 10 s at 100 KiB/s, plus the bucket's initial burst (two 100 ms ticks).
    let size = (10 * 100 * KIB + 20 * KIB) / 3;
    for seed in 0..3 {
        let path = format!("/blob/{size}?seed={seed}&{}", named(&format!("{seed}.bin")));
        sb.add(&server, &path, &["--queue"]).assert_success();
    }
    let started = Instant::now();
    sb.dhow(["watch"]).assert_success();
    let secs = started.elapsed().as_secs_f64();
    assert!((9.0..11.0).contains(&secs), "{secs} s for 10 s of data");
    for seed in 0..3 {
        assert_blob(&sb.dl().join(format!("{seed}.bin")), seed, size);
    }
}

/// §12: `0` pauses a download by limit (still active, nothing flows);
/// lifting it from another `dhow` applies to the running one at once.
#[test]
fn a_limit_change_reaches_the_running_download() {
    let server = TestServer::start().unwrap();
    let sb = Sandbox::new();
    let path = format!("/blob/2000000?seed=7&{}", named("z.bin"));
    let mut running = sb.spawn_add(&server, &path, &["--limit", "0"]);
    let id = running.wait_for(|e| e["event"] == "state" && e["to"] == "active")["id"]
        .as_str()
        .unwrap()
        .to_owned();
    std::thread::sleep(Duration::from_millis(1000));
    let row = sb.ls().into_iter().find(|r| r["id"] == id).unwrap();
    assert_eq!(row["state"], "active");
    assert_eq!(row["speed_limit"], 0);
    // At most the one read before the first wait.
    assert!(row["downloaded"].as_u64().unwrap() <= 64 * KIB, "{row}");

    let lifted = Instant::now();
    let out = sb.dhow(["limit", &id[..8], "none"]);
    out.assert_success();
    assert_eq!(out.events()[0]["speed_limit"], serde_json::Value::Null);
    running.wait().assert_success();
    assert!(
        lifted.elapsed() < Duration::from_secs(3),
        "{:?}",
        lifted.elapsed()
    );
    assert_blob(&sb.dl().join("z.bin"), 7, 2_000_000);
}

#[test]
fn limits_show_and_validate() {
    let sb = Sandbox::new();
    let out = sb.dhow(["queue", "add", "slow", "--limit", "1.5M"]);
    out.assert_success();
    assert_eq!(out.events()[0]["speed_limit"], 3 * 512 * KIB);
    let out = sb.dhow(["queue", "set", "slow", "--limit", "none"]);
    assert_eq!(out.events()[0]["speed_limit"], serde_json::Value::Null);
    let out = sb.dhow(["settings", "--headroom", "15"]);
    assert_eq!(out.events()[0]["headroom_percent"], 15);
    assert_eq!(
        out.events()[0]["global_speed_limit"],
        serde_json::Value::Null
    );
    assert!(!sb.dhow(["settings", "--headroom", "100"]).status.success());
    assert!(
        !sb.dhow(["queue", "add", "q", "--limit", "fast"])
            .status
            .success()
    );
}
