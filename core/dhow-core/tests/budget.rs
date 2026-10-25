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

fn config(window: Duration) -> DownloadConfig {
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
            ..WriterConfig::default()
        },
        ramp: RampConfig {
            window,
            min_chunk: 128 * 1024,
            min_steal_gain: Duration::from_millis(200),
            // Never saturated: these tests are about the budget, and a
            // short window on a busy machine measures scheduling as often as
            // the link (a burst, then "no gain"). T-10b covers saturation.
            min_gain: -1.0,
            ..RampConfig::default()
        },
        ..DownloadConfig::default()
    }
}

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// Connection counts per download over time, as their events report them.
#[derive(Default)]
struct Timeline {
    now: Vec<u32>,
    /// (when, download, connections), every change.
    changes: Vec<(Instant, usize, u32)>,
    max_total: u32,
    /// Most slots the budget itself had out (held and reserved), seen at
    /// any event: the events of different downloads may arrive out of
    /// order, so their sum can briefly exceed it.
    max_used: u32,
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
/// `stagger` apart (in age order), ramping up every `window`. With
/// `newcomers: Some((k, n))`, download `k` and younger start only once `n`
/// connections are open. `held` servers send nothing until every download
/// has its first connection (no bytes, no ramp-up). Asserts every file
/// bit-exact.
async fn run_all(
    jobs: Vec<Job>,
    budget: BudgetConfig,
    stagger: Duration,
    window: Duration,
    newcomers: Option<(usize, u32)>,
    held: &[TestServer],
) -> Timeline {
    let shared = Arc::new(Shared::new(budget));
    let timeline = Arc::new(Mutex::new(Timeline::new(jobs.len())));
    let dir = tempfile::tempdir().unwrap();
    let base_ms = 1_000_000;
    for s in held {
        s.hold();
    }
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
            limits: vec![],
        };
        tasks.push(tokio::spawn(async move {
            tokio::time::sleep(stagger * i as u32).await;
            if let Some((_, open)) = newcomers.filter(|&(k, _)| i >= k) {
                while timeline.lock().unwrap().now.iter().sum::<u32>() < open {
                    tokio::time::sleep(ms(10)).await;
                }
            }
            let (_keep, stop) = watch::channel(false);
            let budget = shared.budget.clone();
            let on_event = move |e: DownloadEvent| {
                let mut t = timeline.lock().unwrap();
                t.record(i, &e);
                t.max_used = t.max_used.max(budget.usage("").0);
            };
            let client = http::client().unwrap();
            let done = download(&req, &client, &config(window), &shared, &on_event, &stop).await;
            done.unwrap_or_else(|e| panic!("download {i}: {e}"));
            let bytes = std::fs::read(&req.part).unwrap();
            assert_eq!(
                blake3::hash(&bytes),
                blob::digest(job.seed, job.size),
                "download {i}"
            );
        }));
    }
    if !held.is_empty() {
        let all_started = async {
            while timeline.lock().unwrap().now.contains(&0) {
                tokio::time::sleep(ms(10)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(60), all_started)
            .await
            .expect("downloads never all started");
        for s in held {
            s.release();
        }
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

const T10: BudgetConfig = BudgetConfig {
    global: 16,
    per_host: 4,
    min_for_new: 1,
};

/// 3 MB at 500 kB/s per connection, each on its own server.
fn ten_jobs(servers: &[TestServer]) -> Vec<Job> {
    let size = 3_000_000;
    servers
        .iter()
        .enumerate()
        .map(|(i, s)| Job {
            url: s.url(&format!("/blob/{size}?seed={i}&throttle=500000")),
            seed: i as u64,
            size,
        })
        .collect()
}

/// T-10: 10 downloads, budget 16, per-host 4 (each on its own server),
/// started together. Their first connections leave 6 slots, exactly what
/// the two oldest claim (§10.3): those two ramp to 4 while the younger 8
/// stay at one connection, until one of the two gives a connection back.
/// This follows from the rule, not from who happens to ask first.
#[tokio::test(flavor = "multi_thread")]
async fn ten_downloads_share_a_budget_of_sixteen_oldest_first() {
    let servers: Vec<TestServer> = (0..10).map(|_| TestServer::start().unwrap()).collect();
    // Bytes flow only once all ten hold a connection, so none can ramp
    // before (a slow start on CI let a younger one grow into the room
    // latecomers hadn't taken yet).
    let t = run_all(
        ten_jobs(&servers),
        T10,
        Duration::ZERO,
        ms(200),
        None,
        &servers,
    )
    .await;
    assert_caps(&t);

    let mut now = [0; 10];
    for &(_, who, n) in &t.changes {
        if who < 2 && n < now[who] {
            break;
        }
        if n > 1 {
            assert!(
                now.iter().all(|&c| c > 0),
                "download {who} ramped before all ten had started: {now:?}"
            );
        }
        now[who] = n;
        assert!(who < 2 || n <= 1, "download {who} grew into a claimed slot");
    }
    assert_eq!(now[..2], [4, 4], "the two oldest ramp first");
}

/// §10.3 newcomers: arriving once the four oldest have spent the budget,
/// each still gets a connection, from the youngest download with spares.
#[tokio::test(flavor = "multi_thread")]
async fn newcomers_get_a_slot_when_the_budget_is_spent() {
    let servers: Vec<TestServer> = (0..10).map(|_| TestServer::start().unwrap()).collect();
    // The four oldest take 2.25 s at full speed, so all ten overlap
    // however slowly the newcomers start (at 1.5 s one sometimes finished
    // first on CI). Not longer: their tails' ordinary releases would then
    // seat the newcomers even without the guarantee.
    let mut jobs = ten_jobs(&servers);
    for (i, job) in jobs.iter_mut().enumerate().take(4) {
        job.size = 4_500_000;
        job.url = servers[i].url(&format!("/blob/4500000?seed={i}&throttle=500000"));
    }
    let t = run_all(jobs, T10, ms(60), ms(100), Some((4, T10.global)), &[]).await;
    assert_caps(&t);
    assert_eq!(t.max_holding, 10, "newcomers starved");
}

fn assert_caps(t: &Timeline) {
    assert!(t.max_used <= 16, "budget exceeded: {}", t.max_used);
    for who in 0..10 {
        assert!(
            t.peak(who) <= 4,
            "download {who} over the per-host cap: {}",
            t.peak(who)
        );
    }
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
    let t = run_all(jobs, budget, ms(400), ms(100), None, &[]).await;
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
            url: server.url(&format!("/blob/{size}?seed={i}&throttle=500000")),
            seed: i,
            size,
        })
        .collect();
    let budget = BudgetConfig {
        global: 16,
        per_host: 1,
        min_for_new: 1,
    };
    // The second arrives once the first holds the host's one slot.
    let t = run_all(jobs, budget, ms(0), ms(100), Some((1, 1)), &[]).await;
    assert_eq!(t.max_total, 1, "never two connections to the host");
    assert_eq!(t.waited, [false, true]);
}
