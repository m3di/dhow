//! Several connections and several downloads through the CLI (ENGINE-SPEC
//! §4, §6 I6, §10): the M2 exit runs.

use std::collections::HashMap;

use spec_tests::{Sandbox, TestServer, Value, assert_blob, named};

/// Fast ramp-up and small chunks, so a test sees several connections.
fn sandbox() -> Sandbox {
    Sandbox::with_env(&[
        ("DHOW_RAMP_WINDOW_MS", "200"),
        ("DHOW_MIN_CHUNK_KIB", "256"),
    ])
}

/// Connection counts per download, in the order the CLI reported them.
fn connections(events: &[Value]) -> HashMap<String, Vec<u64>> {
    let mut by_id: HashMap<String, Vec<u64>> = HashMap::new();
    for e in events.iter().filter(|e| e["event"] == "progress") {
        let seen = by_id
            .entry(e["id"].as_str().unwrap().to_owned())
            .or_default();
        let n = e["connections"].as_u64().unwrap();
        if seen.last() != Some(&n) {
            seen.push(n);
        }
    }
    by_id
}

/// T-I6: every range that ends before the file is followed by 1 MiB of
/// garbage; several connections drop it, the file is bit-exact.
#[test]
fn t_i6_bytes_past_each_chunk_are_dropped() {
    let server = TestServer::start().unwrap();
    let sb = sandbox();
    let path = format!(
        "/blob/4000000?seed=6&throttle=1000000&extra_mid=1048576&{}",
        named("i6.bin")
    );
    let out = sb.add(&server, &path, &[]);
    out.assert_success();
    let counts = connections(&out.events());
    let most = counts.values().flatten().max().copied().unwrap_or(0);
    assert!(most >= 2, "needs several connections: {counts:?}");
    assert_blob(&sb.dl().join("i6.bin"), 6, 4_000_000);
}

/// M2 exit: 10 downloads at once (each on its own server, so per-host caps
/// don't serialize them) complete bit-exact, and the CLI output shows the
/// ramp: connections grow one at a time, never past the budget of 16, and
/// the oldest download ramps first. Long enough (8 s on one connection)
/// that splitting work is worth `min_steal_gain` (1 s) for a while.
#[test]
fn ten_parallel_downloads_ramp_and_complete() {
    let servers: Vec<TestServer> = (0..10).map(|_| TestServer::start().unwrap()).collect();
    let sb = sandbox();
    let out = sb.dhow(["queue", "add", "all", "--max", "10"]);
    out.assert_success();
    let mut ids = Vec::new();
    for (seed, server) in servers.iter().enumerate() {
        let path = format!(
            "/blob/4000000?seed={seed}&throttle=500000&{}",
            named(&format!("{seed}.bin"))
        );
        let added = sb.add(server, &path, &["--queue", "--to", "all"]);
        added.assert_success();
        ids.push(added.id());
    }
    let out = sb.dhow(["watch"]);
    out.assert_success();
    let events = out.events();
    for seed in 0..10 {
        assert_blob(&sb.dl().join(format!("{seed}.bin")), seed as u64, 4_000_000);
    }

    let counts = connections(&events);
    for (id, seq) in &counts {
        // Up one at a time (the end of each chunk may drop several at once).
        let rises: Vec<_> = seq.windows(2).filter(|w| w[1] > w[0]).collect();
        assert!(rises.iter().all(|w| w[1] == w[0] + 1), "{id}: {seq:?}");
    }
    let oldest = &counts[&ids[0]];
    assert!(
        oldest.iter().max().unwrap() >= &3,
        "oldest ramped: {oldest:?}"
    );
    // Never more than the budget at once.
    let mut now: HashMap<&str, u64> = HashMap::new();
    for e in events.iter().filter(|e| e["event"] == "progress") {
        now.insert(
            e["id"].as_str().unwrap(),
            e["connections"].as_u64().unwrap(),
        );
        let total: u64 = now.values().sum();
        assert!(total <= 16, "{total} connections at once");
    }
}
