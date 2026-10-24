//! The connection budget across concurrent downloads (ENGINE-SPEC §10.1–
//! 10.3) against dhow-testserver.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use dhow_core::download::budget::BudgetConfig;
use dhow_core::download::ramp::RampConfig;
use dhow_core::download::{DownloadConfig, DownloadEvent, Request, Shared, download};
use dhow_core::http;
use dhow_core::transfer::{RetryPolicy, TransferConfig};
use dhow_core::writer::WriterConfig;
use dhow_testserver::{TestServer, blob};
use reqwest::Url;
use tokio::sync::watch;

fn config() -> DownloadConfig {
    DownloadConfig {
        transfer: TransferConfig {
            first_byte: Duration::from_secs(10),
            idle: Duration::from_secs(10),
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
            window: Duration::from_millis(100),
            min_chunk: 128 * 1024,
            min_steal_gain: Duration::from_millis(200),
            ..RampConfig::default()
        },
        ..DownloadConfig::default()
    }
}

/// Connection counts per download over time, as their events report them.
#[derive(Default)]
struct Timeline {
    now: Vec<u32>,
    /// (when, download, connections), every change.
    changes: Vec<(Instant, usize, u32)>,
    max_total: u32,
    /// Most downloads holding a connection at the same moment.
    max_holding: usize,
    waited: Vec<bool>,
}

impl Timeline {
    fn new(n: usize) -> Self {
        Self {
            now: vec![0; n],
            waited: vec![false; n],
            ..Self::default()
        }
    }

    fn record(&mut self, who: usize, e: &DownloadEvent) {
        match e {
            DownloadEvent::Connections(n) => {
                self.now[who] = *n;
                self.changes.push((Instant::now(), who, *n));
                self.max_total = self.max_total.max(self.now.iter().sum());
                self.max_holding = self
                    .max_holding
                    .max(self.now.iter().filter(|&&n| n > 0).count());
            }
            DownloadEvent::Waiting(true) => self.waited[who] = true,
            _ => {}
        }
    }

    fn first_reach(&self, who: usize, n: u32) -> Option<Instant> {
        self.changes
            .iter()
            .find(|(_, w, c)| *w == who && *c >= n)
            .map(|(t, _, _)| *t)
    }

    fn peak(&self, who: usize) -> u32 {
        self.changes
            .iter()
            .filter(|(_, w, _)| *w == who)
            .map(|(_, _, c)| *c)
            .max()
            .unwrap_or(0)
    }
}

struct Job {
    url: String,
    seed: u64,
    size: u64,
}

/// Runs `jobs` concurrently through one shared budget, starting them
/// `stagger` apart (in age order). Asserts every file bit-exact.
async fn run_all(jobs: Vec<Job>, budget: BudgetConfig, stagger: Duration) -> Timeline {
    let shared = Arc::new(Shared::new(budget));
    let timeline = Arc::new(Mutex::new(Timeline::new(jobs.len())));
    let dir = tempfile::tempdir().unwrap();
    let base_ms = 1_000_000;
    let mut tasks = Vec::new();
    for (i, job) in jobs.into_iter().enumerate() {
        let (shared, timeline) = (shared.clone(), timeline.clone());
        let req = Request {
            url: Url::parse(&job.url).unwrap(),
            headers: vec![],
            part: dir.path().join(format!("{i}.part")),
            sidecar: dir.path().join(format!("{i}.dhow")),
            connections: None,
            started_at: Some(base_ms + i as i64),
        };
        tasks.push(tokio::spawn(async move {
            tokio::time::sleep(stagger * i as u32).await;
            let (_keep, stop) = watch::channel(false);
            let on_event = move |e: DownloadEvent| timeline.lock().unwrap().record(i, &e);
            let client = http::client().unwrap();
            let done = download(&req, &client, &config(), &shared, &on_event, &stop).await;
            done.unwrap_or_else(|e| panic!("download {i}: {e}"));
            let bytes = std::fs::read(&req.part).unwrap();
            assert_eq!(
                blake3::hash(&bytes),
                blob::digest(job.seed, job.size),
                "download {i}"
            );
        }));
    }
    for t in tasks {
        tokio::time::timeout(Duration::from_secs(60), t)
            .await
            .expect("downloads hung")
            .unwrap();
    }
    Arc::try_unwrap(timeline)
        .ok()
        .unwrap()
        .into_inner()
        .unwrap()
}

/// T-10: 10 downloads, budget 16, per-host 4 (each on its own server).
#[tokio::test(flavor = "multi_thread")]
async fn ten_downloads_share_a_budget_of_sixteen_oldest_first() {
    let servers: Vec<TestServer> = (0..10).map(|_| TestServer::start().unwrap()).collect();
    let size = 3_000_000;
    let jobs = servers
        .iter()
        .enumerate()
        .map(|(i, s)| Job {
            url: s.url(&format!("/blob/{size}?seed={i}&throttle=500000")),
            seed: i as u64,
            size,
        })
        .collect();
    let budget = BudgetConfig {
        global: 16,
        per_host: 4,
        min_for_new: 1,
    };
    let t = run_all(jobs, budget, Duration::from_millis(60)).await;

    assert!(t.max_total <= 16, "budget exceeded: {}", t.max_total);
    for who in 0..10 {
        assert!(
            t.peak(who) <= 4,
            "download {who} over the per-host cap: {}",
            t.peak(who)
        );
    }
    // Oldest first: the oldest is the first to reach 4.
    let oldest = t.first_reach(0, 4).expect("oldest reached 4");
    for who in 1..10 {
        if let Some(when) = t.first_reach(who, 4) {
            assert!(oldest <= when, "download {who} reached 4 before the oldest");
        }
    }
    // Newcomers got a connection even with the budget spent: more than
    // 16 / 4 downloads held connections at once.
    assert!(
        t.max_holding > 4,
        "newcomers starved: at most {} at once",
        t.max_holding
    );
}

/// §10.2: the per-host cap holds across downloads to one server, and a
/// newcomer still gets a slot from the youngest holder of several.
#[tokio::test(flavor = "multi_thread")]
async fn per_host_cap_is_shared_by_downloads_to_one_server() {
    let server = TestServer::start().unwrap();
    let size = 3_000_000;
    let jobs = (0..3)
        .map(|i| Job {
            url: server.url(&format!("/blob/{size}?seed={i}&throttle=500000")),
            seed: i,
            size,
        })
        .collect();
    let budget = BudgetConfig {
        global: 16,
        per_host: 4,
        min_for_new: 1,
    };
    let t = run_all(jobs, budget, Duration::from_millis(400)).await;
    assert!(t.max_total <= 4, "per-host cap exceeded: {}", t.max_total);
    assert_eq!(t.max_holding, 3, "all three ran at once");
}

/// With no slot to take, a newcomer waits (`Waiting(ConnectionBudget)`).
#[tokio::test(flavor = "multi_thread")]
async fn a_full_host_makes_newcomers_wait() {
    let server = TestServer::start().unwrap();
    let size = 500_000;
    let jobs = (0..2)
        .map(|i| Job {
            url: server.url(&format!("/blob/{size}?seed={i}&throttle=1000000")),
            seed: i,
            size,
        })
        .collect();
    let budget = BudgetConfig {
        global: 16,
        per_host: 1,
        min_for_new: 1,
    };
    let t = run_all(jobs, budget, Duration::from_millis(50)).await;
    assert_eq!(t.max_total, 1, "never two connections to the host");
    assert_eq!(t.waited, [false, true]);
}
