//! Several connections per download (ENGINE-SPEC §4) against
//! dhow-testserver: ramp-up, saturation, work stealing, downgrade, resume.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use dhow_core::download::ramp::{LinkState, RampConfig};
use dhow_core::download::{
    DownloadConfig, DownloadError, DownloadEvent, Downloaded, Request, download,
};
use dhow_core::http;
use dhow_core::journal::Journal;
use dhow_core::transfer::{OVERLAP, RetryPolicy, TransferConfig};
use dhow_core::writer::WriterConfig;
use dhow_testserver::{TestServer, blob};
use reqwest::Url;
use tokio::sync::watch;

const MB: u64 = 1_000_000;

fn config(window_ms: u64) -> DownloadConfig {
    DownloadConfig {
        transfer: TransferConfig {
            first_byte: Duration::from_secs(5),
            idle: Duration::from_secs(5),
            retry: RetryPolicy {
                max_failures: 10,
                base: Duration::from_millis(1),
                cap: Duration::from_millis(5),
                retry_after_cap: Duration::from_secs(1),
            },
        },
        writer: WriterConfig {
            commit_interval: Duration::from_millis(100),
            commit_bytes: 8 * 1024 * 1024,
        },
        ramp: RampConfig {
            window: Duration::from_millis(window_ms),
            min_chunk: 256 * 1024,
            min_steal_gain: Duration::from_millis(200),
            ..RampConfig::default()
        },
        ..DownloadConfig::default()
    }
}

struct Env {
    _dir: tempfile::TempDir,
    req: Request,
}

struct Run {
    result: Result<Downloaded, DownloadError>,
    events: Vec<DownloadEvent>,
    elapsed: Duration,
}

impl Run {
    fn max_connections(&self) -> u32 {
        self.events
            .iter()
            .filter_map(|e| match e {
                DownloadEvent::Connections(n) => Some(*n),
                _ => None,
            })
            .max()
            .unwrap_or(0)
    }
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
        };
        Self { _dir: dir, req }
    }

    async fn run(&self, config: &DownloadConfig, link: &LinkState) -> Run {
        let (_keep, stop) = watch::channel(false);
        self.run_until(config, link, stop).await
    }

    async fn run_until(
        &self,
        config: &DownloadConfig,
        link: &LinkState,
        stop: watch::Receiver<bool>,
    ) -> Run {
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let on_event = move |e: DownloadEvent| sink.lock().unwrap().push(e);
        let client = http::client().unwrap();
        let started = Instant::now();
        let result = tokio::time::timeout(
            Duration::from_secs(60),
            download(&self.req, &client, config, link, &on_event, &stop),
        )
        .await
        .expect("download hung");
        Run {
            result,
            events: events.lock().unwrap().clone(),
            elapsed: started.elapsed(),
        }
    }

    fn part_hash(&self) -> blake3::Hash {
        blake3::hash(&std::fs::read(&self.req.part).unwrap())
    }
}

fn range_starts(server: &TestServer) -> Vec<u64> {
    server
        .requests()
        .iter()
        .filter(|r| r.method == "GET")
        .filter_map(|r| {
            r.header("range")?
                .strip_prefix("bytes=")?
                .split('-')
                .next()?
                .parse()
                .ok()
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn ramps_up_and_completes_bit_exact() {
    let server = TestServer::start().unwrap();
    let env = Env::new(&server, "/blob/4000000?seed=7&throttle=1000000");
    let run = env.run(&config(100), &LinkState::default()).await;
    run.result.as_ref().unwrap();
    assert_eq!(env.part_hash(), blob::digest(7, 4 * MB));
    assert!(
        run.max_connections() >= 4,
        "ramped to {}",
        run.max_connections()
    );
    let mut starts = range_starts(&server);
    starts.dedup();
    assert!(starts.len() >= 4, "{starts:?}");
    // Each new connection was journaled before it requested (§4.3): the
    // sidecar is gone after completion, but nothing overlapped.
    assert!(run.result.unwrap().committed.is_exactly(4 * MB));
}

/// T-10b (issue #8): a link that saturates at 4 connections stops the ramp
/// at 4 ± 1 even though 8 are allowed, and later downloads start there.
#[tokio::test(flavor = "multi_thread")]
async fn ramp_stops_where_the_link_saturates() {
    let server = TestServer::start().unwrap();
    let link = LinkState::default();
    let path = "/blob/6000000?throttle=500000&link=2000000";
    let env = Env::new(&server, path);
    let run = env.run(&config(200), &link).await;
    run.result.as_ref().unwrap();
    assert_eq!(env.part_hash(), blob::digest(0, 6 * MB));
    let peak = run.max_connections();
    assert!((3..=5).contains(&peak), "peaked at {peak}");
    let ceiling = link.ceiling().expect("saturation remembered");
    assert!((3..=5).contains(&ceiling), "{ceiling}");

    let env2 = Env::new(&server, "/blob/6000000?seed=1&throttle=500000&link=2000000");
    let run2 = env2.run(&config(200), &link).await;
    run2.result.as_ref().unwrap();
    assert!(
        run2.max_connections() <= ceiling,
        "{} > {ceiling}",
        run2.max_connections()
    );
}

/// T-4.3: one connection crawls at 1 KiB/s; work stealing takes its chunk
/// and the download finishes within 1.5× of the all-fast time.
#[tokio::test(flavor = "multi_thread")]
async fn work_stealing_rescues_a_crawling_connection() {
    let server = TestServer::start().unwrap();
    let baseline = Env::new(&server, "/blob/3000000?seed=2&throttle=1500000");
    let ideal = baseline.run(&config(100), &LinkState::default()).await;
    ideal.result.as_ref().unwrap();

    let slow = Env::new(
        &server,
        "/blob/3000000?seed=3&throttle=1500000&throttle_first=1024",
    );
    let run = slow.run(&config(100), &LinkState::default()).await;
    run.result.as_ref().unwrap();
    assert_eq!(slow.part_hash(), blob::digest(3, 3 * MB));
    let limit = ideal.elapsed.mul_f64(1.5) + Duration::from_millis(500);
    assert!(
        run.elapsed <= limit,
        "{:?} vs ideal {:?}",
        run.elapsed,
        ideal.elapsed
    );
}

/// T-9b: the server honored a range once and then ignores them. The
/// download drops to one connection, keeps its bytes and completes.
#[tokio::test(flavor = "multi_thread")]
async fn server_that_stops_ranging_downgrades_to_one_connection() {
    let server = TestServer::start().unwrap();
    let env = Env::new(
        &server,
        "/blob/3000000?throttle=1000000&ignore_range_after=1",
    );
    let run = env.run(&config(100), &LinkState::default()).await;
    run.result.as_ref().unwrap();
    assert!(run.events.contains(&DownloadEvent::Downgraded));
    assert!(
        !run.events
            .iter()
            .any(|e| matches!(e, DownloadEvent::Reset(_))),
        "no restart"
    );
    assert_eq!(env.part_hash(), blob::digest(0, 3 * MB));
}

/// A multi-connection run stopped midway leaves several committed ranges;
/// resuming fills every gap, each re-verifying its overlap (I5).
#[tokio::test(flavor = "multi_thread")]
async fn resume_after_a_multi_connection_stop() {
    let server = TestServer::start().unwrap();
    let env = Env::new(&server, "/blob/4000000?seed=5&throttle=1000000");
    let (tx, stop) = watch::channel(false);
    let stopper = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(900)).await;
        tx.send(true).unwrap();
    });
    let run = env
        .run_until(&config(100), &LinkState::default(), stop)
        .await;
    stopper.await.unwrap();
    assert!(
        matches!(run.result, Err(DownloadError::Stopped)),
        "{:?}",
        run.result
    );
    let committed = Journal::open(&env.req.sidecar).unwrap().committed().clone();
    assert!(committed.ranges().len() >= 2, "{committed:?}");

    let before = server.requests().len();
    let run = env.run(&config(100), &LinkState::default()).await;
    run.result.as_ref().unwrap();
    assert_eq!(env.part_hash(), blob::digest(5, 4 * MB));
    let resumed: Vec<u64> = server.requests()[before..]
        .iter()
        .filter(|r| r.method == "GET")
        .filter_map(|r| {
            r.header("range")?
                .strip_prefix("bytes=")?
                .split('-')
                .next()?
                .parse()
                .ok()
        })
        .collect();
    for gap in committed.gaps(4 * MB) {
        if gap.start > 0 {
            let verify_from = committed
                .ending_at(gap.start)
                .unwrap()
                .start
                .max(gap.start - OVERLAP.min(gap.start));
            assert!(resumed.contains(&verify_from), "gap {gap:?}: {resumed:?}");
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn non_resumable_and_small_files_use_one_connection() {
    let server = TestServer::start().unwrap();
    for path in [
        "/blob/3000000?ranges=none&throttle=3000000",
        "/blob/200000?throttle=400000",
    ] {
        let env = Env::new(&server, path);
        let run = env.run(&config(50), &LinkState::default()).await;
        run.result.as_ref().unwrap();
        assert_eq!(run.max_connections(), 1, "{path}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn per_download_connection_limit() {
    let server = TestServer::start().unwrap();
    let mut env = Env::new(&server, "/blob/4000000?throttle=1000000");
    env.req.connections = Some(2);
    let run = env.run(&config(100), &LinkState::default()).await;
    run.result.as_ref().unwrap();
    assert_eq!(run.max_connections(), 2);
    assert_eq!(env.part_hash(), blob::digest(0, 4 * MB));
}
