//! Smoke tests for the harness itself. Spec scenarios (T-*) start in M1.

use std::time::{Duration, Instant};

use spec_tests::{Sandbox, TestServer, assert_blob, blob};

#[test]
fn cli_runs_in_sandbox() {
    let out = Sandbox::new().dhow(["--version"]);
    out.assert_success();
    assert!(out.stdout.starts_with("dhow "), "{out:?}");
}

#[test]
fn cli_logs_into_sandbox() {
    let sandbox = Sandbox::new();
    sandbox.dhow(std::iter::empty::<&str>()).assert_success();
    assert_eq!(std::fs::read_dir(sandbox.log_dir()).unwrap().count(), 1);
}

#[test]
fn cli_failure_is_reported() {
    assert!(!Sandbox::new().dhow(["--no-such-flag"]).status.success());
}

#[test]
fn server_starts_and_stops() {
    let server = TestServer::start().unwrap();
    let addr = server.addr();
    assert!(std::net::TcpStream::connect(addr).is_ok());
    drop(server);
    // A CLI spawned by a parallel test can briefly hold the listener between
    // fork and exec, so allow a short grace period.
    let deadline = Instant::now() + Duration::from_secs(2);
    while std::net::TcpStream::connect(addr).is_ok() {
        assert!(
            Instant::now() < deadline,
            "server still accepting after drop"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn assert_blob_accepts_exact_copy() {
    let sandbox = Sandbox::new();
    let path = sandbox.path().join("f");
    std::fs::write(&path, blob::bytes(4, 0..100_000)).unwrap();
    assert_blob(&path, 4, 100_000);
}

#[test]
#[should_panic(expected = "differs from blob")]
fn assert_blob_rejects_one_flipped_byte() {
    let sandbox = Sandbox::new();
    let path = sandbox.path().join("f");
    let mut bytes = blob::bytes(4, 0..100_000);
    bytes[54_321] ^= 1;
    std::fs::write(&path, bytes).unwrap();
    assert_blob(&path, 4, 100_000);
}

#[test]
#[should_panic(expected = "wrong size")]
fn assert_blob_rejects_truncation() {
    let sandbox = Sandbox::new();
    let path = sandbox.path().join("f");
    std::fs::write(&path, blob::bytes(4, 0..99_999)).unwrap();
    assert_blob(&path, 4, 100_000);
}
