//! Several connections per download (ENGINE-SPEC §4) against
//! dhow-testserver: ramp-up, saturation, work stealing, downgrade, resume.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use dhow_core::download::budget::BudgetConfig;
use dhow_core::download::ramp::RampConfig;
use dhow_core::download::{
    DownloadConfig, DownloadError, DownloadEvent, Downloaded, Request, Shared, download,
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
        // No timed commits: each fsync holds every connection behind the
        // writer, and on a slow disk (100–270 ms on CI) the backlog it
        // leaves lands in some ramp-up window. Splits, stops and the end
        // still commit.
        writer: WriterConfig {
            commit_interval: Duration::from_secs(60),
            commit_bytes: 8 * 1024 * 1024,
            ..WriterConfig::default()
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
            started_at: None,
            limits: vec![],
        };
        Self { _dir: dir, req }
    }

    async fn run(&self, config: &DownloadConfig, shared: &Shared) -> Run {
        self.run_until(config, shared, |_| false).await
    }

    /// Runs until done, or stops it once `stop_when` sees an event.
    async fn run_until(
        &self,
        config: &DownloadConfig,
        shared: &Shared,
        stop_when: impl Fn(&DownloadEvent) -> bool + Send + Sync + 'static,
    ) -> Run {
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let (tx, stop) = watch::channel(false);
        let on_event = move |e: DownloadEvent| {
            if stop_when(&e) {
                tx.send_replace(true);
            }
            sink.lock().unwrap().push(e);
        };
        let client = http::client().unwrap();
        let started = Instant::now();
        let result = tokio::time::timeout(
            Duration::from_secs(60),
            download(&self.req, &client, config, shared, &on_event, &stop),
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
    // Never saturated (see T-10): this is about ramping and splitting.
    let mut config = config(100);
    config.ramp.min_gain = -1.0;
    let run = env.run(&config, &Shared::default()).await;
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
    // Per-host cap above the saturation point: this is about the link.
    let shared = Shared::new(BudgetConfig {
        per_host: 16,
        ..BudgetConfig::default()
    });
    let path = "/blob/6000000?throttle=500000&link=2000000";
    let env = Env::new(&server, path);
    let run = env.run(&config(200), &shared).await;
    run.result.as_ref().unwrap();
    assert_eq!(env.part_hash(), blob::digest(0, 6 * MB));
    let peak = run.max_connections();
    assert!((3..=5).contains(&peak), "peaked at {peak}");
    let ceiling = shared.link.ceiling().expect("saturation remembered");
    assert!((3..=5).contains(&ceiling), "{ceiling}");

    let env2 = Env::new(&server, "/blob/6000000?seed=1&throttle=500000&link=2000000");
    let run2 = env2.run(&config(200), &shared).await;
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
    let ideal = baseline.run(&config(100), &Shared::default()).await;
    ideal.result.as_ref().unwrap();

    let slow = Env::new(
        &server,
        "/blob/3000000?seed=3&throttle=1500000&throttle_first=1024",
    );
    let run = slow.run(&config(100), &Shared::default()).await;
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
    let run = env.run(&config(100), &Shared::default()).await;
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
    // Stopped once ramp-up (never saturated) opens a fourth connection:
    // the second has had two ramp steps to write, so the journal holds
    // several ranges, however slow the start was.
    let mut config = config(100);
    config.ramp.min_gain = -1.0;
    let fourth = |e: &DownloadEvent| matches!(e, DownloadEvent::Connections(n) if *n >= 4);
    let run = env.run_until(&config, &Shared::default(), fourth).await;
    assert!(
        matches!(run.result, Err(DownloadError::Stopped)),
        "{:?}",
        run.result
    );
    let committed = Journal::open(&env.req.sidecar).unwrap().committed().clone();
    assert!(committed.ranges().len() >= 2, "{committed:?}");

    let before = server.requests().len();
    let run = env.run(&config, &Shared::default()).await;
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
        let run = env.run(&config(50), &Shared::default()).await;
        run.result.as_ref().unwrap();
        assert_eq!(run.max_connections(), 1, "{path}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn per_download_connection_limit() {
    let server = TestServer::start().unwrap();
    let mut env = Env::new(&server, "/blob/4000000?throttle=1000000");
    env.req.connections = Some(2);
    let run = env.run(&config(100), &Shared::default()).await;
    run.result.as_ref().unwrap();
    assert_eq!(run.max_connections(), 2);
    assert_eq!(env.part_hash(), blob::digest(0, 4 * MB));
}

/// §12: a download's own limit paces it, and ramp-up doesn't take the limit
/// for a saturated link: no extra connections, nothing recorded.
#[tokio::test(flavor = "multi_thread")]
async fn a_download_limit_paces_it_without_fooling_ramp_up() {
    let server = TestServer::start().unwrap();
    let shared = Shared::default();
    let mut env = Env::new(&server, "/blob/600000");
    env.req.limits = vec![shared.bucket(Some(200_000))];
    let run = env.run(&config(200), &shared).await;
    run.result.as_ref().unwrap();
    assert_eq!(env.part_hash(), blob::digest(0, 600_000));
    // 600 kB at 200 kB/s, less the 40 kB initial burst: 2.8 s. The floor
    // is the limit holding; the ceiling catches pacing well under it, not
    // set-up and fsyncs (up to a second on CI).
    let secs = run.elapsed.as_secs_f64();
    assert!((2.6..4.0).contains(&secs), "{secs} s");
    assert_eq!(run.max_connections(), 1);
    assert_eq!(shared.link.ceiling(), None, "a limit is not the link");
}

/// §12: the global limit is one bucket for every download.
#[tokio::test(flavor = "multi_thread")]
async fn the_global_limit_is_shared() {
    let server = TestServer::start().unwrap();
    let shared = Shared::default();
    shared.global.set_user(Some(200_000));
    let (a, b) = (
        Env::new(&server, "/blob/300000"),
        Env::new(&server, "/blob/300000?seed=1"),
    );
    let config = config(200);
    let started = Instant::now();
    let (ra, rb) = tokio::join!(a.run(&config, &shared), b.run(&config, &shared));
    ra.result.unwrap();
    rb.result.unwrap();
    assert_eq!(b.part_hash(), blob::digest(1, 300_000));
    // As above: 2.8 s.
    let secs = started.elapsed().as_secs_f64();
    assert!((2.6..4.0).contains(&secs), "{secs} s");
}

/// A 4 MB download with 50 % headroom on a 1 MB/s link that saturates at
/// 4 connections, each commit taking `slow_sync` longer. Timed commits,
/// unlike the other tests here: each may hold the connections back (the
/// step's split waits behind it). Returns the learned peak.
async fn headroom_run(slow_sync: Duration) -> (f64, Run, Shared) {
    let server = TestServer::start().unwrap();
    let shared = Shared::new(BudgetConfig {
        per_host: 16,
        ..BudgetConfig::default()
    });
    shared.global.set_headroom(50);
    let env = Env::new(&server, "/blob/4000000?throttle=250000&link=1000000");
    let mut config = config(300);
    config.writer.commit_interval = Duration::from_millis(100);
    config.writer.slow_sync = slow_sync;
    let run = env.run(&config, &shared).await;
    run.result.as_ref().unwrap();
    assert_eq!(env.part_hash(), blob::digest(0, 4 * MB));
    let peak = shared.global.peak().expect("peak measured") as f64;
    (peak, run, shared)
}

/// §10.6 (issue #8): with 50 % headroom, once ramp-up has found the link
/// the download keeps to half of it.
#[tokio::test(flavor = "multi_thread")]
async fn headroom_leaves_part_of_the_link_free() {
    let (peak, run, shared) = headroom_run(Duration::ZERO).await;
    assert!((800_000.0..1_200_000.0).contains(&peak), "peak {peak}");
    let cap = shared.global.rate().expect("capped after ramp-up") as f64;
    assert!((cap / peak - 0.5).abs() < 0.01, "cap {cap}, peak {peak}");
    // Unlimited it takes about 4 s; at half the link after ramp-up, longer.
    let secs = run.elapsed.as_secs_f64();
    assert!(secs > 5.5, "{secs} s");
}

/// §10.6: on a slow disk (commits longer than the settling third) the
/// backlog a stall leaves arrives all at once in the next window, which
/// alone read 2.3 MB/s here. Over the whole step it is the link's rate.
#[tokio::test(flavor = "multi_thread")]
async fn headroom_peak_is_the_link_on_a_slow_disk() {
    let (peak, _, _) = headroom_run(Duration::from_millis(500)).await;
    // Low is the safe side: a stall that ends the step lowers it.
    assert!((700_000.0..1_200_000.0).contains(&peak), "peak {peak}");
}
