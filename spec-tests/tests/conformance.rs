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

#[test]
fn queue_then_watch_runs_everything_in_order() {
    let server = TestServer::start().unwrap();
    let sb = Sandbox::new();
    for seed in 1..=3 {
        sb.add(
            &server,
            &format!("/blob/50000?seed={seed}&{}", named(&format!("{seed}.bin"))),
            &["--queue"],
        )
        .assert_success();
    }
    assert!(sb.files().is_empty(), "--queue only records");
    let out = sb.dhow(["watch"]);
    out.assert_success();
    let finished: Vec<_> = out
        .events()
        .iter()
        .filter(|e| e["event"] == "renamed")
        .map(|e| {
            e["path"]
                .as_str()
                .unwrap()
                .rsplit('/')
                .next()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(finished, ["1.bin", "2.bin", "3.bin"]);
    for seed in 1..=3 {
        assert_blob(&sb.dl().join(format!("{seed}.bin")), seed, 50_000);
    }
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
    assert_eq!(paused.events()[0]["state"], "paused");
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
