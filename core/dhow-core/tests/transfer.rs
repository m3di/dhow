#![allow(clippy::single_range_in_vec_init)]

//! ENGINE-SPEC §7, §9 and I6/I7 against dhow-testserver, single connection.

use std::ops::Range;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use dhow_core::http;
use dhow_core::journal::{Header, Journal, format};
use dhow_core::probe::Resumable;
use dhow_core::rangeset::RangeSet;
use dhow_core::transfer::{
    Cause, ConnEvent, Cursor, IdentityChange, RetryPolicy, Source, TransferConfig, TransferError,
    run_connection, validator,
};
use dhow_core::writer::actor;
use dhow_core::writer::{Writer, WriterConfig, create_part};
use dhow_testserver::{LAST_MODIFIED, TestServer, blob};
use reqwest::Url;

const FAST: TransferConfig = TransferConfig {
    first_byte: Duration::from_secs(5),
    idle: Duration::from_millis(300),
    retry: RetryPolicy {
        max_failures: 10,
        base: Duration::from_millis(1),
        cap: Duration::from_millis(5),
        retry_after_cap: Duration::from_secs(300),
    },
};

struct Spec<'a> {
    path: &'a str,
    total: Option<u64>,
    resumable: Resumable,
    validator: Option<String>,
    range: Option<Range<u64>>,
}

impl<'a> Spec<'a> {
    fn new(path: &'a str, total: u64) -> Self {
        Self {
            path,
            total: Some(total),
            resumable: Resumable::Yes,
            validator: None,
            range: None,
        }
    }
}

struct Run {
    result: Result<dhow_core::transfer::Finished, TransferError>,
    part: Vec<u8>,
    committed: RangeSet,
    events: Vec<ConnEvent>,
    elapsed: Duration,
}

impl Run {
    fn retries(&self) -> usize {
        self.events
            .iter()
            .filter(|e| matches!(e, ConnEvent::Retrying { .. }))
            .count()
    }
}

async fn download(server: &TestServer, spec: Spec<'_>) -> Run {
    let dir = tempfile::tempdir().unwrap();
    let part = dir.path().join("f.part");
    let (file, preallocated) = create_part(&part, spec.total).unwrap();
    let url = Url::parse(&server.url(spec.path)).unwrap();
    let header = Header {
        preallocated,
        total_size: spec.total,
        url_hash: format::url_hash(url.as_str()),
        etag: None,
        last_modified: None,
        created_at: 0,
    };
    let journal = Journal::create(&dir.path().join("f.dhow"), header).unwrap();
    let (handle, done) = actor::spawn(Writer::new(file, journal, WriterConfig::default())).unwrap();
    let range = spec.range.unwrap_or(0..spec.total.unwrap_or(u64::MAX));
    handle.define(0, range.clone()).await.unwrap();

    let src = Source {
        client: http::client().unwrap(),
        url,
        headers: vec![],
        total: spec.total,
        resumable: spec.resumable,
        validator: spec.validator,
    };
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let on_event = move |e: ConnEvent| sink.lock().unwrap().push(e);
    let started = Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(30),
        run_connection(
            &src,
            0,
            &Cursor::new(range),
            None,
            &handle,
            &FAST,
            &on_event,
        ),
    )
    .await
    .expect("transfer hung");
    let elapsed = started.elapsed();
    let committed = match &result {
        Ok(_) => handle.finish().await.unwrap(),
        Err(_) => {
            drop(handle);
            match done.await.unwrap().unwrap() {
                actor::Outcome::Closed(c) | actor::Outcome::Finished(c) => c,
            }
        }
    };
    let events = events.lock().unwrap().clone();
    Run {
        result,
        part: std::fs::read(&part).unwrap(),
        committed,
        events,
        elapsed,
    }
}

fn ranges_requested(server: &TestServer) -> Vec<Option<String>> {
    server
        .requests()
        .iter()
        .filter(|r| r.method == "GET")
        .map(|r| r.header("range").map(str::to_owned))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn downloads_bit_exact_with_range_and_if_range() {
    let server = TestServer::start().unwrap();
    let size = 1024 * 1024 + 7;
    let path = format!("/blob/{size}?seed=3");
    let mut spec = Spec::new(&path, size);
    spec.validator = validator(Some("\"3-100007\""), Some(LAST_MODIFIED));
    let run = download(&server, spec).await;
    assert!(run.result.as_ref().unwrap().content_length_seen);
    assert!(run.committed.is_exactly(size));
    assert_eq!(blake3::hash(&run.part), blob::digest(3, size));
    let req = &server.requests()[0];
    assert_eq!(
        req.header("range"),
        Some(format!("bytes=0-{}", size - 1)).as_deref()
    );
    assert_eq!(req.header("if-range"), Some("\"3-100007\""));
    assert_eq!(run.retries(), 0);
}

/// T-I7: every response is cut short; the loop resumes from where each one
/// stopped and never completes early.
#[tokio::test(flavor = "multi_thread")]
async fn truncated_responses_resume_until_complete() {
    let server = TestServer::start().unwrap();
    let size = 1_000_000;
    let path = format!("/blob/{size}?seed=4&truncate=90000");
    let run = download(&server, Spec::new(&path, size)).await;
    run.result.as_ref().unwrap();
    assert!(run.committed.is_exactly(size));
    assert_eq!(blake3::hash(&run.part), blob::digest(4, size));
    let ranges = ranges_requested(&server);
    assert_eq!(ranges.len(), 12, "ceil(1e6 / 9e4) responses");
    for (i, r) in ranges.iter().enumerate() {
        assert_eq!(
            r.as_deref(),
            Some(format!("bytes={}-999999", i * 90_000).as_str())
        );
    }
    // Every attempt made progress, so the consecutive count never grew.
    assert!(
        run.events
            .iter()
            .all(|e| !matches!(e, ConnEvent::Retrying { failures, .. } if *failures > 1))
    );
}

/// T-9a: ten consecutive resets give up; a later `Retry` completes.
#[tokio::test(flavor = "multi_thread")]
async fn ten_consecutive_resets_fail_then_retry_completes() {
    let server = TestServer::start().unwrap();
    let path = "/blob/50000?fail=10";
    let run = download(&server, Spec::new(path, 50_000)).await;
    assert!(
        matches!(
            run.result,
            Err(TransferError::Transient {
                failures: 10,
                last: Cause::Network
            })
        ),
        "{:?}",
        run.result
    );
    assert_eq!(run.retries(), 9);
    assert!(run.committed.is_empty());

    let run = download(&server, Spec::new(path, 50_000)).await;
    run.result.as_ref().unwrap();
    assert_eq!(blake3::hash(&run.part), blob::digest(0, 50_000));
}

#[tokio::test(flavor = "multi_thread")]
async fn nine_resets_are_survived() {
    let server = TestServer::start().unwrap();
    let run = download(&server, Spec::new("/blob/50000?fail=9", 50_000)).await;
    run.result.as_ref().unwrap();
    assert_eq!(run.retries(), 9);
    let failures: Vec<u32> = run
        .events
        .iter()
        .filter_map(|e| match e {
            ConnEvent::Retrying { failures, .. } => Some(*failures),
            _ => None,
        })
        .collect();
    assert_eq!(failures, (1..=9).collect::<Vec<_>>());
}

#[tokio::test(flavor = "multi_thread")]
async fn honors_retry_after_on_503() {
    let server = TestServer::start().unwrap();
    let run = download(
        &server,
        Spec::new("/blob/1000?fail=1&fail_mode=503&retry_after=1", 1000),
    )
    .await;
    run.result.as_ref().unwrap();
    assert!(run.elapsed >= Duration::from_secs(1), "{:?}", run.elapsed);
    assert!(matches!(
        run.events.first(),
        Some(ConnEvent::Retrying { delay, cause: Cause::Status(503), .. }) if *delay == Duration::from_secs(1)
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn permanent_errors_do_not_retry() {
    let server = TestServer::start().unwrap();
    for status in [404, 403, 410] {
        let path = format!("/blob/1000?fail=100&fail_mode={status}");
        let run = download(&server, Spec::new(&path, 1000)).await;
        assert!(matches!(run.result, Err(TransferError::Permanent(s)) if s == status));
        assert_eq!(run.retries(), 0);
    }
}

/// T-I6b: a single-connection response longer than the file, with the
/// extra bytes in the same read as the last real ones, and in a later read
/// (only reading to EOF catches that one).
#[tokio::test(flavor = "multi_thread")]
async fn bytes_past_total_fail_the_download() {
    let server = TestServer::start().unwrap();
    for knobs in ["extra=500", "extra=500&extra_delay=50"] {
        let path = format!("/blob/10000?{knobs}");
        let run = download(&server, Spec::new(&path, 10_000)).await;
        assert!(
            matches!(run.result, Err(TransferError::ServerSentExtraData)),
            "{knobs}: {:?}",
            run.result
        );
        assert_eq!(run.part.len(), 10_000, "nothing written past total");
        assert_eq!(blake3::hash(&run.part), blob::digest(0, 10_000));
    }
}

/// T-I6: bytes past the chunk's end (not the file's) are dropped.
#[tokio::test(flavor = "multi_thread")]
async fn bytes_past_chunk_are_dropped() {
    let server = TestServer::start().unwrap();
    let mut spec = Spec::new("/blob/100000?extra=1048576", 100_000);
    spec.range = Some(0..40_000);
    let run = download(&server, spec).await;
    run.result.as_ref().unwrap();
    assert_eq!(run.committed.ranges(), [0..40_000]);
    assert_eq!(&run.part[..40_000], &blob::bytes(0, 0..40_000)[..]);
    assert!(run.part[40_000..].iter().all(|&b| b == 0), "rest untouched");
}

/// T-blind: no length, no ranges.
#[tokio::test(flavor = "multi_thread")]
async fn blind_mode_reads_to_eof() {
    let server = TestServer::start().unwrap();
    let mut spec = Spec::new("/blob/300000?seed=8&length=omit&ranges=ignore", 0);
    spec.total = None;
    spec.resumable = Resumable::No;
    let run = download(&server, spec).await;
    assert!(!run.result.as_ref().unwrap().content_length_seen);
    assert_eq!(blake3::hash(&run.part), blob::digest(8, 300_000));
    assert_eq!(ranges_requested(&server), [None]);
}

#[tokio::test(flavor = "multi_thread")]
async fn non_resumable_downloads_without_range() {
    let server = TestServer::start().unwrap();
    let mut spec = Spec::new("/blob/200000?ranges=none", 200_000);
    spec.resumable = Resumable::No;
    let run = download(&server, spec).await;
    run.result.as_ref().unwrap();
    assert_eq!(blake3::hash(&run.part), blob::digest(0, 200_000));
    assert_eq!(ranges_requested(&server), [None]);
}

#[tokio::test(flavor = "multi_thread")]
async fn non_resumable_cut_short_needs_restart() {
    let server = TestServer::start().unwrap();
    let mut spec = Spec::new("/blob/200000?ranges=none&truncate=50000", 200_000);
    spec.resumable = Resumable::No;
    let run = download(&server, spec).await;
    assert!(matches!(run.result, Err(TransferError::NeedsRestart)));
    assert_eq!(ranges_requested(&server).len(), 1, "no pointless retry");
}

#[tokio::test(flavor = "multi_thread")]
async fn non_resumable_retries_before_any_byte() {
    let server = TestServer::start().unwrap();
    let mut spec = Spec::new("/blob/1000?ranges=none&fail=2", 1000);
    spec.resumable = Resumable::No;
    let run = download(&server, spec).await;
    run.result.as_ref().unwrap();
    assert_eq!(run.retries(), 2);
}

/// §2.3: `Unknown` is planned as resumable and downgraded on a 200.
#[tokio::test(flavor = "multi_thread")]
async fn unknown_resumability_downgrades_on_full_response() {
    let server = TestServer::start().unwrap();
    let mut spec = Spec::new("/blob/100000?ranges=ignore", 100_000);
    spec.resumable = Resumable::Unknown;
    let run = download(&server, spec).await;
    run.result.as_ref().unwrap();
    assert!(run.events.contains(&ConnEvent::NotResumable));
    assert_eq!(blake3::hash(&run.part), blob::digest(0, 100_000));
}

/// I4: a stale `If-Range` makes the server send a different file whole.
/// Nothing of it is written; the coordinator resets.
#[tokio::test(flavor = "multi_thread")]
async fn stale_if_range_is_a_validator_mismatch() {
    let server = TestServer::start().unwrap();
    let mut spec = Spec::new("/blob/100000", 100_000);
    spec.validator = Some("\"stale\"".into());
    let run = download(&server, spec).await;
    assert!(
        matches!(
            run.result,
            Err(TransferError::IdentityChanged(
                IdentityChange::ValidatorMismatch
            ))
        ),
        "{:?}",
        run.result
    );
    assert!(run.committed.is_empty());
    assert!(!run.events.contains(&ConnEvent::NotResumable));
}

/// A server that honored a range and then ignores one: past offset 0 the
/// full body is useless (T-9b's server; single connection can't downgrade).
#[tokio::test(flavor = "multi_thread")]
async fn range_ignored_past_offset_zero_is_an_identity_change() {
    let server = TestServer::start().unwrap();
    let mut spec = Spec::new("/blob/100000?ignore_range_after=1&truncate=30000", 100_000);
    spec.validator = Some("\"0-186a0\"".into());
    let run = download(&server, spec).await;
    assert!(
        matches!(
            run.result,
            Err(TransferError::IdentityChanged(IdentityChange::RangeIgnored))
        ),
        "{:?}",
        run.result
    );
    assert_eq!(run.committed.ranges(), [0..30_000], "what arrived is kept");
}

/// Same server behaviour at offset 0 with a matching validator: the server
/// simply doesn't do ranges, so the download continues non-resumable.
#[tokio::test(flavor = "multi_thread")]
async fn range_ignored_with_matching_validator_downgrades() {
    let server = TestServer::start().unwrap();
    let mut spec = Spec::new("/blob/100000?ignore_range_after=0", 100_000);
    spec.validator = Some("\"0-186a0\"".into());
    let run = download(&server, spec).await;
    run.result.as_ref().unwrap();
    assert!(run.events.contains(&ConnEvent::NotResumable));
    assert_eq!(blake3::hash(&run.part), blob::digest(0, 100_000));
}

#[tokio::test(flavor = "multi_thread")]
async fn total_mismatch_is_an_identity_change() {
    let server = TestServer::start().unwrap();
    let run = download(&server, Spec::new("/blob/1000", 2000)).await;
    assert!(
        matches!(
            run.result,
            Err(TransferError::IdentityChanged(
                IdentityChange::TotalMismatch {
                    expected: 2000,
                    got: Some(1000)
                }
            ))
        ),
        "{:?}",
        run.result
    );
    assert!(run.committed.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn stalled_body_times_out_and_resumes() {
    let server = TestServer::start().unwrap();
    let run = download(
        &server,
        Spec::new("/blob/300000?seed=2&stall_after=100000", 300_000),
    )
    .await;
    run.result.as_ref().unwrap();
    assert_eq!(blake3::hash(&run.part), blob::digest(2, 300_000));
    let timeouts = run
        .events
        .iter()
        .filter(|e| {
            matches!(
                e,
                ConnEvent::Retrying {
                    cause: Cause::Timeout,
                    ..
                }
            )
        })
        .count();
    assert_eq!(timeouts, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn follows_redirects_on_every_request() {
    let server = TestServer::start().unwrap();
    let run = download(
        &server,
        Spec::new("/blob/100000?redirect=2&truncate=60000", 100_000),
    )
    .await;
    run.result.as_ref().unwrap();
    assert_eq!(blake3::hash(&run.part), blob::digest(0, 100_000));
    let targets: Vec<String> = server.requests().iter().map(|r| r.target.clone()).collect();
    assert_eq!(
        targets.iter().filter(|t| t.contains("redirect=2")).count(),
        2
    );
}
