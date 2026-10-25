//! Settings through the CLI (ARCHITECTURE §3.1, §5.1): stored for every
//! later `dhow`, all-or-nothing, and live in a running `watch`.
#![cfg(unix)]

use std::time::{Duration, Instant};

use serde_json::{Value, json};
use spec_tests::{Sandbox, TestServer, assert_blob, named};

fn settings(sb: &Sandbox) -> Value {
    let out = sb.dhow(["settings"]);
    out.assert_success();
    out.events()[0].clone()
}

#[test]
fn settings_persist_across_invocations() {
    let sb = Sandbox::new();
    let defaults = settings(&sb);
    assert_eq!(
        defaults,
        json!({
            "event": "settings", "global_speed_limit": null, "headroom_percent": 0,
            "global_connections": 16, "per_host_connections": 4,
            "min_connections_for_new": 1, "default_connections": 8,
            "max_connections_per_download": 16, "first_byte_timeout_ms": 30000,
            "idle_timeout_ms": 30000, "max_retries": 10,
            // The harness's DHOW_COMMIT_INTERVAL_MS, the process default.
            "commit_interval_ms": 100,
            "probe_range_get": true, "use_content_disposition": true,
            "append_type_extension": true, "auto_grouping": true,
            "extra_extensions": {}, "proxy": "system",
        })
    );
    let out = sb.dhow([
        "settings",
        "--global-limit",
        "2M",
        "--headroom",
        "10",
        "--global-connections",
        "12",
        "--per-host-connections",
        "3",
        "--min-connections-for-new",
        "0",
        "--default-connections",
        "4",
        "--max-connections-per-download",
        "6",
        "--first-byte-timeout-ms",
        "5000",
        "--idle-timeout-ms",
        "7000",
        "--max-retries",
        "3",
        "--commit-interval-ms",
        "1000",
        "--probe-range-get",
        "false",
        "--use-content-disposition",
        "false",
        "--append-type-extension",
        "false",
        "--auto-grouping",
        "false",
        "--extensions",
        "data=qqq",
        "--proxy",
        "none",
    ]);
    out.assert_success();
    let changed = json!({
        "event": "settings", "global_speed_limit": 2 << 20, "headroom_percent": 10,
        "global_connections": 12, "per_host_connections": 3,
        "min_connections_for_new": 0, "default_connections": 4,
        "max_connections_per_download": 6, "first_byte_timeout_ms": 5000,
        "idle_timeout_ms": 7000, "max_retries": 3, "commit_interval_ms": 1000,
        "probe_range_get": false, "use_content_disposition": false,
        "append_type_extension": false, "auto_grouping": false,
        "extra_extensions": { "data": ["qqq"] }, "proxy": "none",
    });
    assert_eq!(out.events(), std::slice::from_ref(&changed));
    assert_eq!(settings(&sb), changed);
    // One flag changes one setting.
    let out = sb.dhow(["settings", "--global-limit", "none"]);
    let mut expected = changed;
    expected["global_speed_limit"] = Value::Null;
    assert_eq!(out.events(), [expected.clone()]);
    assert_eq!(settings(&sb), expected);
}

#[test]
fn a_bad_flag_applies_nothing() {
    let sb = Sandbox::new();
    let before = settings(&sb);
    for bad in [
        &["--global-limit", "1M", "--headroom", "100"][..],
        &["--global-limit", "1M", "--per-host-connections", "0"],
        &["--headroom", "5", "--default-connections", "99"],
        &["--headroom", "5", "--min-connections-for-new", "2"],
        &["--headroom", "5", "--proxy", "ftp://p.test:21"],
        &["--headroom", "5", "--extensions", "films=x"],
        &["--headroom", "5", "--probe-range-get", "maybe"],
        &["--headroom", "5", "--max-connections-per-download", "4"],
        &["--headroom", "5", "--idle-timeout-ms", "10"],
        &["--headroom", "5", "--max-retries", "0"],
        &["--headroom", "5", "--commit-interval-ms", "0"],
    ] {
        let out = sb.dhow([&["settings"][..], bad].concat());
        assert!(!out.status.success(), "{bad:?}");
        assert_eq!(settings(&sb), before, "{bad:?}");
    }
}

/// A global limit lifted by another `dhow` reaches the download `watch`
/// is running.
#[test]
fn a_change_reaches_a_running_watch() {
    let server = TestServer::start().unwrap();
    let sb = Sandbox::new();
    sb.dhow(["settings", "--global-limit", "20K"])
        .assert_success();
    let path = format!("/blob/2000000?seed=3&{}", named("w.bin"));
    sb.add(&server, &path, &["--queue"]).assert_success();
    let mut watch = sb.spawn(["watch"]);
    watch.wait_for(|e| e["event"] == "progress" && e["downloaded"].as_u64() > Some(0));
    let lifted = Instant::now();
    sb.dhow(["settings", "--global-limit", "none"])
        .assert_success();
    watch.wait().assert_success();
    // 100 s at the old limit.
    assert!(
        lifted.elapsed() < Duration::from_secs(30),
        "{:?}",
        lifted.elapsed()
    );
    assert_blob(&sb.dl().join("w.bin"), 3, 2_000_000);
}
