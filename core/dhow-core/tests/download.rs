//! Resume and reset (ENGINE-SPEC §5.4, §6 I3–I5, I7) against dhow-testserver.

use std::fs::OpenOptions;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dhow_core::download::{
    DownloadConfig, DownloadError, DownloadEvent, Downloaded, Request, ResetReason, Shared,
    download,
};
use dhow_core::http;
use dhow_core::journal::Journal;
use dhow_core::transfer::{ConnEvent, OVERLAP, RetryPolicy, TransferConfig, TransferError};
use dhow_core::writer::WriterConfig;
use dhow_testserver::{TestServer, blob};
use reqwest::Url;
use tokio::sync::watch;

const SIZE: u64 = 1_000_000;
const CUT: u64 = 300_000;

fn config(max_failures: u32) -> DownloadConfig {
    DownloadConfig {
        transfer: TransferConfig {
            first_byte: Duration::from_secs(5),
            idle: Duration::from_millis(500),
            retry: RetryPolicy {
                max_failures,
                base: Duration::from_millis(1),
                cap: Duration::from_millis(5),
                retry_after_cap: Duration::from_secs(1),
            },
        },
        // Commit every 100 kB so a torn tail leaves something behind.
        writer: WriterConfig {
            commit_interval: Duration::from_secs(60),
            commit_bytes: 100_000,
        },
        ..DownloadConfig::default()
    }
}

/// One download's files, reused across runs like a real `Retry`.
struct Env {
    _dir: tempfile::TempDir,
    req: Request,
}

impl Env {
    fn new(server: &TestServer, path: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let req = Request {
            url: Url::parse(&server.url(path)).unwrap(),
            headers: vec![],
            part: dir.path().join("f.part"),
            sidecar: dir.path().join("f.dhow"),
            connections: None,
            started_at: None,
            limits: vec![],
        };
        Self { _dir: dir, req }
    }

    async fn run(
        &self,
        config: &DownloadConfig,
    ) -> (Result<Downloaded, DownloadError>, Vec<DownloadEvent>) {
        self.run_until(config, watch::channel(false).0, |_| false)
            .await
    }

    /// Runs with `stop`, setting it once `stop_when` sees an event.
    async fn run_until(
        &self,
        config: &DownloadConfig,
        stop: watch::Sender<bool>,
        stop_when: impl Fn(&DownloadEvent) -> bool + Send + Sync + 'static,
    ) -> (Result<Downloaded, DownloadError>, Vec<DownloadEvent>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let rx = stop.subscribe();
        let on_event = move |e: DownloadEvent| {
            if stop_when(&e) {
                stop.send_replace(true);
            }
            sink.lock().unwrap().push(e);
        };
        let client = http::client().unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(30),
            download(
                &self.req,
                &client,
                config,
                &Shared::default(),
                &on_event,
                &rx,
            ),
        )
        .await
        .expect("download hung");
        let events = events.lock().unwrap().clone();
        (result, events)
    }

    fn part_hash(&self) -> blake3::Hash {
        blake3::hash(&std::fs::read(&self.req.part).unwrap())
    }

    fn committed_on_disk(&self) -> u64 {
        Journal::open(&self.req.sidecar).unwrap().committed().len()
    }
}

fn resets(events: &[DownloadEvent]) -> Vec<ResetReason> {
    events
        .iter()
        .filter_map(|e| match e {
            DownloadEvent::Reset(r) => Some(r.clone()),
            _ => None,
        })
        .collect()
}

fn resumed(events: &[DownloadEvent]) -> Option<u64> {
    events.iter().find_map(|e| match e {
        DownloadEvent::Resumed { committed } => Some(*committed),
        _ => None,
    })
}

/// GET `Range` headers seen from request `from` on.
fn ranges_from(server: &TestServer, from: usize) -> Vec<Option<String>> {
    server
        .requests()
        .iter()
        .filter(|r| r.method == "GET")
        .skip(from)
        .map(|r| r.header("range").map(str::to_owned))
        .collect()
}

/// A first run that dies after `CUT` bytes, leaving a journal behind.
async fn interrupted(env: &Env) {
    let (result, _) = env.run(&config(1)).await;
    assert!(
        matches!(
            result,
            Err(DownloadError::Transfer(TransferError::Transient { .. }))
        ),
        "{result:?}"
    );
    assert_eq!(env.committed_on_disk(), CUT);
}

#[tokio::test(flavor = "multi_thread")]
async fn fresh_download_completes() {
    let server = TestServer::start().unwrap();
    let env = Env::new(&server, "/blob/1000000?seed=1");
    let (result, events) = env.run(&config(10)).await;
    let done = result.unwrap();
    assert!(done.committed.is_exactly(SIZE));
    assert!(!done.size_unverified && !done.identity_weak);
    assert_eq!(env.part_hash(), blob::digest(1, SIZE));
    assert!(matches!(events.first(), Some(DownloadEvent::Probed(_))));
    assert_eq!(resumed(&events), None);
    assert!(resets(&events).is_empty());
}

/// I5: resume asks for the last 64 KiB again and checks them.
#[tokio::test(flavor = "multi_thread")]
async fn resume_reverifies_the_overlap_and_completes() {
    let server = TestServer::start().unwrap();
    let env = Env::new(&server, "/blob/1000000?cut_first=300000");
    interrupted(&env).await;
    let gets_before = ranges_from(&server, 0).len();

    let (result, events) = env.run(&config(10)).await;
    result.unwrap();
    assert_eq!(resumed(&events), Some(CUT));
    assert!(resets(&events).is_empty());
    assert_eq!(
        ranges_from(&server, gets_before),
        [Some(format!("bytes={}-{}", CUT - OVERLAP, SIZE - 1))]
    );
    assert_eq!(env.part_hash(), blob::digest(0, SIZE));
}

/// T-I5: same ETag, different bytes in the overlap: reset.
#[tokio::test(flavor = "multi_thread")]
async fn overlap_mismatch_resets() {
    let server = TestServer::start().unwrap();
    let env = Env::new(&server, "/blob/1000000?cut_first=300000&lie_after=1");
    interrupted(&env).await;
    let (result, events) = env.run(&config(10)).await;
    result.unwrap();
    assert_eq!(resumed(&events), Some(CUT));
    assert_eq!(resets(&events), [ResetReason::ServerFileChanged]);
    // The server keeps serving the new bytes; the file is all of them.
    assert_eq!(env.part_hash(), blob::digest(0x2000, SIZE));
}

/// T-I4a: the file changes mid-download; the result is the new file.
#[tokio::test(flavor = "multi_thread")]
async fn identity_change_mid_download_restarts_with_new_content() {
    let server = TestServer::start().unwrap();
    let env = Env::new(&server, "/blob/1000000?cut_first=300000&change_after=1");
    let (result, events) = env.run(&config(10)).await;
    result.unwrap();
    assert_eq!(resets(&events), [ResetReason::ServerFileChanged]);
    assert_eq!(env.part_hash(), blob::digest(0x1000, SIZE));
}

/// T-I4b: the server answers a resumed range with 200.
#[tokio::test(flavor = "multi_thread")]
async fn range_answered_with_200_restarts() {
    let server = TestServer::start().unwrap();
    let env = Env::new(
        &server,
        "/blob/1000000?cut_first=300000&ignore_range_after=1",
    );
    let (result, events) = env.run(&config(10)).await;
    result.unwrap();
    assert_eq!(resets(&events), [ResetReason::ServerFileChanged]);
    let downgraded = events
        .iter()
        .any(|e| matches!(e, DownloadEvent::Connection(ConnEvent::NotResumable)));
    assert!(
        downgraded,
        "after the restart the server's 200 is the whole file"
    );
    assert_eq!(env.part_hash(), blob::digest(0, SIZE));
}

/// The file changed while nobody was downloading: caught at the probe,
/// before a byte is requested.
#[tokio::test(flavor = "multi_thread")]
async fn identity_change_between_runs_resets_before_requesting() {
    let server = TestServer::start().unwrap();
    let env = Env::new(&server, "/blob/1000000?cut_first=300000&change_after=1");
    interrupted(&env).await;
    let gets_before = ranges_from(&server, 0).len();
    let (result, events) = env.run(&config(10)).await;
    result.unwrap();
    assert_eq!(resumed(&events), None);
    assert_eq!(resets(&events), [ResetReason::ServerFileChanged]);
    assert_eq!(
        ranges_from(&server, gets_before),
        [Some(format!("bytes=0-{}", SIZE - 1))]
    );
    assert_eq!(env.part_hash(), blob::digest(0x1000, SIZE));
}

/// T-I3b: a part file nobody can vouch for is discarded.
#[tokio::test(flavor = "multi_thread")]
async fn part_without_sidecar_starts_over() {
    let server = TestServer::start().unwrap();
    let env = Env::new(&server, "/blob/1000000?cut_first=300000");
    interrupted(&env).await;
    std::fs::remove_file(&env.req.sidecar).unwrap();
    let gets_before = ranges_from(&server, 0).len();
    let (result, events) = env.run(&config(10)).await;
    result.unwrap();
    assert_eq!(resets(&events), [ResetReason::Unaccountable]);
    assert_eq!(
        ranges_from(&server, gets_before),
        [Some(format!("bytes=0-{}", SIZE - 1))]
    );
    assert_eq!(env.part_hash(), blob::digest(0, SIZE));
}

#[tokio::test(flavor = "multi_thread")]
async fn sidecar_without_part_starts_over() {
    let server = TestServer::start().unwrap();
    let env = Env::new(&server, "/blob/1000000?cut_first=300000");
    interrupted(&env).await;
    std::fs::remove_file(&env.req.part).unwrap();
    let (result, events) = env.run(&config(10)).await;
    result.unwrap();
    assert_eq!(resets(&events), [ResetReason::Unaccountable]);
    assert_eq!(env.part_hash(), blob::digest(0, SIZE));
}

/// T-I3: a torn sidecar tail loses only the torn records.
#[tokio::test(flavor = "multi_thread")]
async fn torn_sidecar_resumes_from_what_survived() {
    let server = TestServer::start().unwrap();
    let env = Env::new(&server, "/blob/1000000?cut_first=300000");
    interrupted(&env).await;
    let len = std::fs::metadata(&env.req.sidecar).unwrap().len();
    OpenOptions::new()
        .write(true)
        .open(&env.req.sidecar)
        .unwrap()
        .set_len(len - 7)
        .unwrap();
    // Commits land on read boundaries; whatever the last intact one
    // covered, the resume must start exactly there.
    let survived = env.committed_on_disk();
    assert!(survived > 0 && survived < CUT, "{survived}");
    let (result, events) = env.run(&config(10)).await;
    result.unwrap();
    assert_eq!(resumed(&events), Some(survived));
    assert!(resets(&events).is_empty());
    assert_eq!(env.part_hash(), blob::digest(0, SIZE));
}

#[tokio::test(flavor = "multi_thread")]
async fn damaged_sidecar_header_starts_over() {
    let server = TestServer::start().unwrap();
    let env = Env::new(&server, "/blob/1000000?cut_first=300000");
    interrupted(&env).await;
    let mut bytes = std::fs::read(&env.req.sidecar).unwrap();
    bytes[20] ^= 0xff;
    std::fs::write(&env.req.sidecar, bytes).unwrap();
    let (result, events) = env.run(&config(10)).await;
    result.unwrap();
    assert_eq!(resets(&events), [ResetReason::Unaccountable]);
    assert_eq!(env.part_hash(), blob::digest(0, SIZE));
}

/// A non-resumable server that drops the connection: start over.
#[tokio::test(flavor = "multi_thread")]
async fn non_resumable_cut_restarts_from_zero() {
    let server = TestServer::start().unwrap();
    let env = Env::new(&server, "/blob/1000000?ranges=none&cut_first=300000");
    let (result, events) = env.run(&config(10)).await;
    result.unwrap();
    assert_eq!(resets(&events), [ResetReason::CannotResume]);
    assert_eq!(ranges_from(&server, 0), [None, None]);
    assert_eq!(env.part_hash(), blob::digest(0, SIZE));
}

/// No validators at all: resume relies on the overlap alone (I4, I5).
#[tokio::test(flavor = "multi_thread")]
async fn weak_identity_resumes_on_overlap_alone() {
    let server = TestServer::start().unwrap();
    let env = Env::new(&server, "/blob/1000000?validators=none&cut_first=300000");
    interrupted(&env).await;
    let (result, events) = env.run(&config(10)).await;
    let done = result.unwrap();
    assert!(done.identity_weak);
    assert_eq!(resumed(&events), Some(CUT));
    let last = server
        .requests()
        .into_iter()
        .rev()
        .find(|r| r.method == "GET")
        .unwrap();
    assert_eq!(last.header("if-range"), None);
    assert_eq!(env.part_hash(), blob::digest(0, SIZE));
}

#[tokio::test(flavor = "multi_thread")]
async fn weak_identity_still_catches_a_lying_server() {
    let server = TestServer::start().unwrap();
    let env = Env::new(
        &server,
        "/blob/1000000?validators=none&cut_first=300000&lie_after=1",
    );
    interrupted(&env).await;
    let (result, events) = env.run(&config(10)).await;
    result.unwrap();
    assert_eq!(resets(&events), [ResetReason::ServerFileChanged]);
    assert_eq!(env.part_hash(), blob::digest(0x2000, SIZE));
}

/// A server whose file changes on every request can't loop forever.
#[tokio::test(flavor = "multi_thread")]
async fn endless_identity_changes_give_up() {
    let server = TestServer::start().unwrap();
    let env = Env::new(&server, "/blob/1000000?change_each=1&truncate=50000");
    let (result, events) = env.run(&config(10)).await;
    assert!(
        matches!(result, Err(DownloadError::TooManyResets(3))),
        "{result:?}"
    );
    assert_eq!(resets(&events).len(), 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn blind_download_is_flagged_size_unverified() {
    let server = TestServer::start().unwrap();
    let env = Env::new(&server, "/blob/300000?seed=6&length=omit&ranges=ignore");
    let (result, _) = env.run(&config(10)).await;
    let done = result.unwrap();
    assert!(done.size_unverified);
    assert_eq!(done.committed.len(), 300_000);
    assert_eq!(env.part_hash(), blob::digest(6, 300_000));
}

/// A completed journal resumed again has nothing left to fetch.
#[tokio::test(flavor = "multi_thread")]
async fn rerun_of_complete_download_fetches_nothing() {
    let server = TestServer::start().unwrap();
    let env = Env::new(&server, "/blob/100000");
    env.run(&config(10)).await.0.unwrap();
    let gets_before = ranges_from(&server, 0).len();
    let (result, events) = env.run(&config(10)).await;
    assert!(result.unwrap().committed.is_exactly(100_000));
    assert_eq!(resumed(&events), Some(100_000));
    assert_eq!(ranges_from(&server, gets_before).len(), 0);
}

/// Pause: the stop signal ends the run after a final commit; resuming
/// continues from there.
#[tokio::test(flavor = "multi_thread")]
async fn stop_commits_and_resume_continues() {
    let server = TestServer::start().unwrap();
    let env = Env::new(&server, "/blob/1000000?throttle=1000000");
    // Stopped once bytes flow, with about a second of data still to come.
    let received =
        |e: &DownloadEvent| matches!(e, DownloadEvent::Connection(ConnEvent::Received(_)));
    let (result, _) = env
        .run_until(&config(10), watch::channel(false).0, received)
        .await;
    assert!(matches!(result, Err(DownloadError::Stopped)), "{result:?}");
    let saved = env.committed_on_disk();
    assert!(saved > 0 && saved < SIZE, "{saved}");
    let (result, events) = env.run(&config(10)).await;
    result.unwrap();
    assert_eq!(resumed(&events), Some(saved));
    assert_eq!(env.part_hash(), blob::digest(0, SIZE));
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_before_start_does_nothing() {
    let server = TestServer::start().unwrap();
    let env = Env::new(&server, "/blob/1000");
    let (result, events) = env
        .run_until(&config(10), watch::channel(true).0, |_| false)
        .await;
    assert!(matches!(result, Err(DownloadError::Stopped)));
    assert!(events.is_empty());
    assert!(server.requests().is_empty());
}
