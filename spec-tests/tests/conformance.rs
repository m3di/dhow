//! ENGINE-SPEC §15 conformance tests, through the `dhow` CLI.
//!
//! Not here yet: T-I6 (bytes past a chunk's end needs several connections,
//! M2) and the multi-connection / scheduling rows (M2+). T-I2 is covered by
//! the random kill -9 runs in `kill9.rs` and by the writer's unit tests.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use sha2::{Digest, Sha256};
use spec_tests::{Sandbox, TestServer, Value, assert_blob, blob, header_knob, named};

const MB: u64 = 1_000_000;

/// A 2 MB file at 2 MB/s: about a second, long enough to interrupt.
fn slow(name: &str, extra: &str) -> String {
    format!("/blob/2000000?throttle=2000000&{}{extra}", named(name))
}

fn halfway(e: &Value) -> bool {
    e["event"] == "progress" && e["downloaded"].as_u64().unwrap() >= MB
}

/// Starts a slow download, kills it with SIGKILL once it's halfway.
fn killed_halfway(sb: &Sandbox, server: &TestServer, path: &str) -> String {
    let mut run = sb.spawn_add(server, path, &[]);
    let added = run.wait_for(|e| e["event"] == "added");
    run.wait_for(halfway);
    run.kill9();
    added["id"].as_str().unwrap().to_owned()
}

#[test]
fn add_downloads_to_the_final_name() {
    let server = TestServer::start().unwrap();
    let sb = Sandbox::new();
    let out = sb.add(
        &server,
        &format!("/blob/700000?seed=9&{}", named("file.bin")),
        &[],
    );
    out.assert_success();
    assert_eq!(out.states(), ["active", "completing", "completed"]);
    assert!(out.has(
        "renamed",
        "path",
        sb.dl().join("file.bin").to_str().unwrap()
    ));
    assert_eq!(sb.files(), ["file.bin"]);
    assert_blob(&sb.dl().join("file.bin"), 9, 700_000);
    let ls = sb.ls();
    assert_eq!(ls.len(), 1);
    assert_eq!(
        (ls[0]["state"].as_str(), ls[0]["downloaded"].as_u64()),
        (Some("completed"), Some(700_000))
    );
}

/// T-I1: kill -9 at 50% → nothing at the final path; part and sidecar
/// present; resume completes bit-exact.
#[test]
fn t_i1_kill_at_half_leaves_no_final_file() {
    let server = TestServer::start().unwrap();
    let sb = Sandbox::new();
    let id = killed_halfway(&sb, &server, &slow("big.bin", ""));
    assert_eq!(sb.files(), ["big.bin.dhow", "big.bin.part"]);
    let row = &sb.ls()[0];
    assert!(
        row["downloaded"].as_u64().unwrap() > 0,
        "progress was committed: {row}"
    );

    let out = sb.dhow(["resume", &id]);
    out.assert_success();
    assert_eq!(sb.files(), ["big.bin"]);
    assert_blob(&sb.dl().join("big.bin"), 0, 2 * MB);
}

/// T-I3: a torn sidecar tail; resume completes bit-exact.
#[test]
fn t_i3_torn_sidecar_resumes() {
    let server = TestServer::start().unwrap();
    let sb = Sandbox::new();
    let id = killed_halfway(&sb, &server, &slow("t.bin", ""));
    let sidecar = sb.dl().join("t.bin.dhow");
    let len = std::fs::metadata(&sidecar).unwrap().len();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&sidecar)
        .unwrap()
        .set_len(len - 5)
        .unwrap();
    sb.dhow(["resume", &id]).assert_success();
    assert_blob(&sb.dl().join("t.bin"), 0, 2 * MB);
}

/// T-I3b: sidecar deleted, part kept → start over.
#[test]
fn t_i3b_missing_sidecar_restarts_from_zero() {
    let server = TestServer::start().unwrap();
    let sb = Sandbox::new();
    let id = killed_halfway(&sb, &server, &slow("t.bin", ""));
    std::fs::remove_file(sb.dl().join("t.bin.dhow")).unwrap();
    let out = sb.dhow(["resume", &id]);
    out.assert_success();
    assert!(out.has("reset", "reason", "unaccountable"));
    assert_blob(&sb.dl().join("t.bin"), 0, 2 * MB);
}

/// T-I4a: the ETag changes mid-download → reset, warned, new content.
#[test]
fn t_i4a_changed_file_restarts_with_new_content() {
    let server = TestServer::start().unwrap();
    let sb = Sandbox::new();
    let out = sb.add(
        &server,
        "/blob/1000000?cut_first=300000&change_after=1",
        &[],
    );
    out.assert_success();
    assert!(out.has("reset", "reason", "server_file_changed"));
    assert_blob(&sb.dl().join("1000000"), 0x1000, MB);
}

/// T-I4b: 200 to a resumed range → same.
#[test]
fn t_i4b_200_to_if_range_restarts() {
    let server = TestServer::start().unwrap();
    let sb = Sandbox::new();
    let out = sb.add(
        &server,
        "/blob/1000000?cut_first=300000&ignore_range_after=1",
        &[],
    );
    out.assert_success();
    assert!(out.has("reset", "reason", "server_file_changed"));
    assert!(out.has("warning", "kind", "not_resumable"));
    assert_blob(&sb.dl().join("1000000"), 0, MB);
}

/// T-I5: different bytes in the overlap window (same ETag) → reset.
#[test]
fn t_i5_overlap_mismatch_resets() {
    let server = TestServer::start().unwrap();
    let sb = Sandbox::new();
    let id = killed_halfway(&sb, &server, &slow("t.bin", "&lie_after=1"));
    let out = sb.dhow(["resume", &id]);
    out.assert_success();
    assert!(out.has("reset", "reason", "server_file_changed"));
    assert_blob(&sb.dl().join("t.bin"), 0x2000, 2 * MB);
}

/// T-I6b: more than the file's size on a single connection.
#[test]
fn t_i6b_extra_data_fails() {
    let server = TestServer::start().unwrap();
    let sb = Sandbox::new();
    let out = sb.add(&server, "/blob/100000?extra=1000&extra_delay=20", &[]);
    assert!(!out.status.success());
    assert!(out.has("state", "reason", "server_sent_extra_data"));
    assert!(
        !sb.files().contains(&"100000".to_owned()),
        "no final file (I1)"
    );
}

/// T-I7: every response cut short → keeps going, never completes early.
#[test]
fn t_i7_truncated_responses_complete() {
    let server = TestServer::start().unwrap();
    let sb = Sandbox::new();
    let out = sb.add(&server, "/blob/1000000?seed=4&truncate=90000", &[]);
    out.assert_success();
    assert_blob(&sb.dl().join("1000000"), 4, MB);
}

/// T-I8: Content-Digest mismatch → Failed(DigestMismatch), part kept.
#[test]
fn t_i8_digest_mismatch_keeps_the_part() {
    let server = TestServer::start().unwrap();
    let sb = Sandbox::new();
    let wrong = STANDARD.encode(Sha256::digest(b"not it"));
    let knob = header_knob("Content-Digest", &format!("sha-256=:{wrong}:"));
    let out = sb.add(&server, &format!("/blob/100000?{knob}"), &[]);
    assert!(!out.status.success());
    assert!(out.has("state", "reason", "digest_mismatch"));
    assert_eq!(sb.files(), ["100000.dhow", "100000.part"]);
}

#[test]
fn matching_digest_completes() {
    let server = TestServer::start().unwrap();
    let sb = Sandbox::new();
    let sha = STANDARD.encode(Sha256::digest(blob::bytes(0, 0..100_000)));
    let knob = header_knob("Content-Digest", &format!("sha-256=:{sha}:"));
    sb.add(&server, &format!("/blob/100000?{knob}"), &[])
        .assert_success();
    assert_eq!(sb.files(), ["100000"]);
}

/// T-I10: can't fit → Failed(NoSpace) at Add, before any byte.
#[test]
fn t_i10_no_space_fails_at_add() {
    let server = TestServer::start().unwrap();
    let sb = Sandbox::new();
    // 1 PiB: no test machine has that free.
    let out = sb.add(&server, "/blob/1125899906842624", &[]);
    assert!(!out.status.success());
    let added = &out.events()[0];
    assert_eq!(
        (added["state"].as_str(), added["reason"].as_str()),
        (Some("failed"), Some("no_space"))
    );
    assert!(sb.files().is_empty());
    assert!(server.requests().iter().all(|r| r.method == "HEAD"));
}

/// T-9a: ten consecutive resets → Failed(Transient); `Retry` completes.
#[test]
fn t_9a_resets_fail_then_resume_completes() {
    let server = TestServer::start().unwrap();
    let sb = Sandbox::new();
    let out = sb.add(&server, "/blob/100000?fail=10", &[]);
    assert!(!out.status.success());
    assert!(out.has("state", "reason", "transient"));
    let retries = out
        .events()
        .iter()
        .filter(|e| e["event"] == "retrying")
        .count();
    assert_eq!(retries, 9);
    sb.dhow(["resume", &out.id()]).assert_success();
    assert_blob(&sb.dl().join("100000"), 0, 100_000);
}

/// T-blind: no length, no ranges → completes, flagged SizeUnverified.
#[test]
fn t_blind_completes_flagged() {
    let server = TestServer::start().unwrap();
    let sb = Sandbox::new();
    let out = sb.add(
        &server,
        "/blob/300000?seed=6&length=omit&ranges=ignore",
        &[],
    );
    out.assert_success();
    assert!(out.has("warning", "kind", "size_unverified"));
    assert_blob(&sb.dl().join("300000"), 6, 300_000);
    assert_eq!(sb.ls()[0]["size_unverified"], true);
}

/// T-3 through the CLI: names from Content-Disposition, Content-Type and
/// collisions. (The full 50+ case table runs in dhow-core.)
#[test]
fn t_3_naming_through_the_cli() {
    let server = TestServer::start().unwrap();
    let sb = Sandbox::new();
    std::fs::write(sb.dl().join("taken.txt"), b"user's").unwrap();
    let cases: &[(String, &str)] = &[
        (
            header_knob(
                "Content-Disposition",
                "attachment; filename*=UTF-8''na%C3%AFve%20file.txt",
            ),
            "naïve file.txt",
        ),
        (named("../../.bashrc"), "bashrc"),
        (named("Quarterly Report.xlsx"), "Quarterly Report.xlsx"),
        (header_knob("Content-Type", "application/pdf"), "10.pdf"),
        (
            header_knob("Content-Type", "application/octet-stream"),
            "10",
        ),
        (named("taken.txt"), "taken (2).txt"),
        (named("same.zip"), "same.zip"),
        (named("same.zip"), "same (2).zip"),
    ];
    for (knob, expect) in cases {
        let out = sb.add(&server, &format!("/blob/10?{knob}"), &["--queue"]);
        out.assert_success();
        let path = out.events()[0]["path"].as_str().unwrap().to_owned();
        assert_eq!(path, sb.dl().join(expect).to_string_lossy(), "{knob}");
    }
    assert_eq!(std::fs::read(sb.dl().join("taken.txt")).unwrap(), b"user's");
    // And they download to exactly those names.
    sb.dhow(["watch"]).assert_success();
    let files = sb.files();
    for (_, expect) in cases {
        assert!(files.contains(&expect.to_string()), "{expect} in {files:?}");
    }
}

#[test]
fn output_name_override() {
    let server = TestServer::start().unwrap();
    let sb = Sandbox::new();
    sb.add(&server, "/blob/10", &["-o", "mine.dat"])
        .assert_success();
    assert_eq!(sb.files(), ["mine.dat"]);
}

/// Ids in the order their `state → active` events appeared.
fn start_order(out: &spec_tests::CliOutput) -> Vec<String> {
    out.events()
        .iter()
        .filter(|e| e["event"] == "state" && e["to"] == "active" && e["from"] == "queued")
        .map(|e| e["id"].as_str().unwrap().to_owned())
        .collect()
}

/// Most downloads active at once, from one process's event stream.
fn max_active(out: &spec_tests::CliOutput) -> usize {
    let mut active = std::collections::HashSet::new();
    let mut max = 0;
    for e in out.events().iter().filter(|e| e["event"] == "state") {
        let id = e["id"].as_str().unwrap().to_owned();
        match e["to"].as_str().unwrap() {
            "active" | "waiting" | "completing" => {
                active.insert(id);
            }
            _ => {
                active.remove(&id);
            }
        }
        max = max.max(active.len());
    }
    max
}

fn queue_slow(sb: &Sandbox, server: &TestServer, seed: u64, extra: &[&str]) -> String {
    let path = format!(
        "/blob/500000?seed={seed}&throttle=1000000&{}",
        named(&format!("{seed}.bin"))
    );
    let mut args = vec!["--queue"];
    args.extend_from_slice(extra);
    let out = sb.add(server, &path, &args);
    out.assert_success();
    out.id()
}

/// §10.4: `watch` runs a queue's downloads concurrently, at most
/// `max_concurrent` at once, starting them in queue order.
#[test]
fn watch_respects_queue_concurrency_and_order() {
    let server = TestServer::start().unwrap();
    let sb = Sandbox::new();
    sb.dhow(["queue", "set", "default", "--max", "2"])
        .assert_success();
    let ids: Vec<String> = (1..=4)
        .map(|seed| queue_slow(&sb, &server, seed, &[]))
        .collect();
    assert!(sb.files().is_empty(), "--queue only records");
    let out = sb.dhow(["watch"]);
    out.assert_success();
    assert_eq!(max_active(&out), 2);
    assert_eq!(start_order(&out), ids);
    for seed in 1..=4 {
        assert_blob(&sb.dl().join(format!("{seed}.bin")), seed, 500_000);
    }
}

#[test]
fn reorder_changes_what_runs_first() {
    let server = TestServer::start().unwrap();
    let sb = Sandbox::new();
    sb.dhow(["queue", "set", "default", "--max", "1"])
        .assert_success();
    let ids: Vec<String> = (1..=3)
        .map(|seed| queue_slow(&sb, &server, seed, &[]))
        .collect();
    sb.dhow(["reorder", &ids[2], "--before", &ids[0]])
        .assert_success();
    sb.dhow(["reorder", &ids[0]]).assert_success(); // to the end
    let out = sb.dhow(["watch"]);
    out.assert_success();
    assert_eq!(
        start_order(&out),
        [ids[2].clone(), ids[1].clone(), ids[0].clone()]
    );
    let positions: Vec<i64> = sb
        .ls()
        .iter()
        .map(|r| r["position"].as_i64().unwrap())
        .collect();
    assert_eq!(positions.len(), 3);
}

/// Each queue has its own limit; queues run side by side.
#[test]
fn queues_run_side_by_side_within_their_limits() {
    let server = TestServer::start().unwrap();
    let sb = Sandbox::new();
    let q = sb.dhow(["queue", "add", "Night", "--max", "1"]);
    q.assert_success();
    let night = q.events()[0]["id"].as_str().unwrap().to_owned();
    sb.dhow(["queue", "set", "default", "--max", "1"])
        .assert_success();
    let n1 = queue_slow(&sb, &server, 1, &["--to", "Night"]);
    let n2 = queue_slow(&sb, &server, 2, &["--to", "Night"]);
    let d1 = queue_slow(&sb, &server, 3, &[]);
    let out = sb.dhow(["watch"]);
    out.assert_success();
    assert_eq!(max_active(&out), 2, "one per queue");
    let order = start_order(&out);
    assert!(order.iter().position(|i| *i == n1) < order.iter().position(|i| *i == n2));
    assert!(order.contains(&d1));
    let ls = sb.ls();
    assert_eq!(
        ls.iter().filter(|r| r["queue"] == night.as_str()).count(),
        2
    );
    assert!(ls.iter().all(|r| r["state"] == "completed"));
}

#[test]
fn deleting_a_queue_moves_its_downloads() {
    let server = TestServer::start().unwrap();
    let sb = Sandbox::new();
    sb.dhow(["queue", "add", "Temp"]).assert_success();
    let id = queue_slow(&sb, &server, 1, &["--to", "Temp"]);
    sb.dhow(["queue", "rm", "Temp"]).assert_success();
    let queues = sb.dhow(["queue", "ls"]).events();
    assert_eq!(queues.len(), 1);
    assert_eq!(queues[0]["default"], true);
    assert_eq!(queues[0]["unfinished"], 1);
    let row = &sb.ls()[0];
    assert_eq!(
        (row["id"].as_str(), row["queue"].as_str()),
        (Some(id.as_str()), queues[0]["id"].as_str())
    );
    let refused = sb.dhow(["queue", "rm", "default"]);
    assert!(!refused.status.success());
    assert!(
        refused.stderr.contains("default queue cannot be deleted"),
        "{refused:?}"
    );
}

/// `pause` from another process: the runner commits and exits; `resume`
/// continues from what it committed.
#[cfg(unix)]
#[test]
fn pause_from_another_process_then_resume() {
    let server = TestServer::start().unwrap();
    let sb = Sandbox::new();
    let mut run = sb.spawn_add(&server, &slow("p.bin", ""), &[]);
    let id = run.wait_for(|e| e["event"] == "added")["id"]
        .as_str()
        .unwrap()
        .to_owned();
    run.wait_for(|e| e["event"] == "progress" && e["downloaded"].as_u64().unwrap() > 0);
    let paused = sb.dhow(["pause", &id]);
    paused.assert_success();
    let summary = paused
        .events()
        .into_iter()
        .find(|e| e["event"] == "download")
        .unwrap();
    assert_eq!(summary["state"], "paused");
    let runner = run.wait();
    assert!(runner.status.success(), "{runner:?}");
    assert!(runner.has("state", "to", "paused"));
    let saved = sb.ls()[0]["downloaded"].as_u64().unwrap();
    assert!(saved > 0 && saved < 2 * MB, "{saved}");

    let out = sb.dhow(["resume", &id[..8]]);
    out.assert_success();
    let first = out
        .events()
        .into_iter()
        .find(|e| e["event"] == "progress")
        .unwrap();
    assert_eq!(
        first["downloaded"].as_u64(),
        Some(saved),
        "resumed from the commit"
    );
    assert_blob(&sb.dl().join("p.bin"), 0, 2 * MB);
}

#[cfg(unix)]
#[test]
fn cancel_while_running_deletes_partials() {
    let server = TestServer::start().unwrap();
    let sb = Sandbox::new();
    let mut run = sb.spawn_add(&server, &slow("c.bin", ""), &[]);
    let id = run.wait_for(|e| e["event"] == "added")["id"]
        .as_str()
        .unwrap()
        .to_owned();
    run.wait_for(|e| e["event"] == "progress" && e["downloaded"].as_u64().unwrap() > 0);
    let out = sb.dhow(["cancel", &id, "--delete-partial"]);
    out.assert_success();
    run.wait();
    assert_eq!(sb.ls()[0]["state"], "cancelled");
    assert!(sb.files().is_empty());
}

#[test]
fn one_process_per_download() {
    let server = TestServer::start().unwrap();
    let sb = Sandbox::new();
    let mut run = sb.spawn_add(&server, &slow("o.bin", ""), &[]);
    let id = run.wait_for(|e| e["event"] == "added")["id"]
        .as_str()
        .unwrap()
        .to_owned();
    run.wait_for(|e| e["event"] == "state" && e["to"] == "active");
    let second = sb.dhow(["resume", &id]);
    assert!(!second.status.success());
    assert!(
        second
            .stderr
            .contains(&format!("running in process {}", run.pid())),
        "{second:?}"
    );
    run.wait().assert_success();
    assert_blob(&sb.dl().join("o.bin"), 0, 2 * MB);
}

#[test]
fn unknown_id_is_an_error() {
    let sb = Sandbox::new();
    let out = sb.dhow(["resume", "nope"]);
    assert!(!out.status.success());
    assert!(out.stderr.contains("no download matches"), "{out:?}");
}
