//! Speed limits (ENGINE-SPEC §12): token buckets at three levels (global,
//! queue, download), chained per connection, and the headroom cap on the
//! global one (§10.6).
//!
//! Bytes are debited as they arrive, which may put a bucket in debt; the
//! connection then doesn't read again until every bucket is out of it.
//! Waiting is a sleep for exactly the time the debt takes to refill, or,
//! at a limit of 0, until a limit changes: nothing polls.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tokio::sync::Notify;

/// The read size without a limit below it.
pub const READ_SIZE: usize = 64 * 1024;
/// Refill granularity.
const TICK: Duration = Duration::from_millis(100);
const TICKS_PER_SEC: f64 = 10.0;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// How long a reader must wait before its next read.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Wait {
    Ready,
    For(Duration),
    /// The limit is 0: until it changes.
    UntilChanged,
}

#[derive(Debug)]
struct BucketState {
    /// Bytes/s; `None`: unlimited.
    rate: Option<u64>,
    /// Bytes that may be read now; negative is debt.
    tokens: f64,
    /// Last refill, on the tick grid.
    last: Instant,
}

impl BucketState {
    /// Two ticks' allotment.
    fn burst(rate: u64) -> f64 {
        2.0 * rate as f64 / TICKS_PER_SEC
    }

    fn refill(&mut self, now: Instant) {
        let Some(rate) = self.rate else {
            self.last = now;
            return;
        };
        let ticks = (now.saturating_duration_since(self.last).as_nanos() / TICK.as_nanos()) as u32;
        if ticks == 0 {
            return;
        }
        self.last += TICK * ticks;
        let refilled = self.tokens + f64::from(ticks) * rate as f64 / TICKS_PER_SEC;
        self.tokens = refilled.min(Self::burst(rate));
    }

    fn wait(&mut self, now: Instant) -> Wait {
        self.refill(now);
        match self.rate {
            None => Wait::Ready,
            Some(0) => Wait::UntilChanged,
            Some(_) if self.tokens >= 0.0 => Wait::Ready,
            Some(rate) => {
                let per_tick = rate as f64 / TICKS_PER_SEC;
                let ticks = (-self.tokens / per_tick).ceil() as u32;
                let at = self.last + TICK * ticks;
                Wait::For(at.saturating_duration_since(now))
            }
        }
    }
}

/// One token bucket. Its limit changes take effect at once: waiters are
/// woken through the [`Notify`] all buckets of an engine share.
#[derive(Debug)]
pub struct Bucket {
    state: Mutex<BucketState>,
    changed: Arc<Notify>,
}

impl Bucket {
    pub fn new(rate: Option<u64>, changed: Arc<Notify>) -> Self {
        Self {
            state: Mutex::new(BucketState {
                rate,
                tokens: rate.map_or(0.0, BucketState::burst),
                last: Instant::now(),
            }),
            changed,
        }
    }

    pub fn rate(&self) -> Option<u64> {
        lock(&self.state).rate
    }

    pub fn set_rate(&self, rate: Option<u64>) {
        self.set_rate_at(rate, Instant::now());
    }

    fn set_rate_at(&self, rate: Option<u64>, now: Instant) {
        let mut s = lock(&self.state);
        if s.rate == rate {
            return;
        }
        s.refill(now);
        s.tokens = match (s.rate, rate) {
            (_, None) => 0.0,
            // Debt run up without a limit isn't owed.
            (None, Some(r)) => BucketState::burst(r),
            (Some(_), Some(r)) => s.tokens.min(BucketState::burst(r)),
        };
        s.rate = rate;
        s.last = now;
        drop(s);
        self.changed.notify_waiters();
    }

    fn debit(&self, n: u64, now: Instant) {
        let mut s = lock(&self.state);
        s.refill(now);
        if s.rate.is_some() {
            s.tokens -= n as f64;
        }
    }

    fn wait(&self, now: Instant) -> Wait {
        lock(&self.state).wait(now)
    }
}

/// The process-wide limit: the user's global limit, lowered by headroom
/// (§10.6) to `(1 − headroom) × peak`. The peak is the highest aggregate
/// throughput over a ramp-up step (unlimited, and long enough that a
/// stall and the backlog it leaves count together); the cap is lifted
/// while any download ramps up, so ramp-up measures the link and not the
/// cap, and applies in between.
#[derive(Debug)]
pub struct GlobalLimit {
    bucket: Arc<Bucket>,
    state: Mutex<GlobalState>,
    /// Bytes read by every connection in the process.
    total: AtomicU64,
}

#[derive(Debug)]
struct GlobalState {
    user: Option<u64>,
    headroom_percent: u8,
    /// Downloads ramping up now.
    learners: u32,
    /// Bytes/s; 0 until measured.
    peak: f64,
}

impl GlobalState {
    fn effective(&self) -> Option<u64> {
        let cap = (self.headroom_percent > 0 && self.learners == 0 && self.peak > 0.0)
            .then(|| (self.peak * f64::from(100 - self.headroom_percent) / 100.0) as u64);
        match (self.user, cap) {
            (Some(u), Some(c)) => Some(u.min(c)),
            (u, c) => u.or(c),
        }
    }
}

impl GlobalLimit {
    pub fn new(changed: Arc<Notify>) -> Self {
        Self {
            bucket: Arc::new(Bucket::new(None, changed)),
            state: Mutex::new(GlobalState {
                user: None,
                headroom_percent: 0,
                learners: 0,
                peak: 0.0,
            }),
            total: AtomicU64::new(0),
        }
    }

    /// `settings.global_speed_limit`.
    pub fn set_user(&self, rate: Option<u64>) {
        let mut s = lock(&self.state);
        s.user = rate;
        self.bucket.set_rate(s.effective());
    }

    /// `settings.headroom_percent`, at most 99.
    pub fn set_headroom(&self, percent: u8) {
        let mut s = lock(&self.state);
        s.headroom_percent = percent.min(99);
        self.bucket.set_rate(s.effective());
    }

    /// The limit in force now.
    pub fn rate(&self) -> Option<u64> {
        self.bucket.rate()
    }

    /// The measured peak, bytes/s.
    pub fn peak(&self) -> Option<u64> {
        let peak = lock(&self.state).peak;
        (peak > 0.0).then_some(peak as u64)
    }

    /// Marks a download as ramping up while the returned guard says so.
    pub fn learner(&self) -> Learner<'_> {
        Learner {
            global: self,
            on: false,
        }
    }

    fn learning(&self, on: bool, now: Instant) {
        let mut s = lock(&self.state);
        if on {
            s.learners += 1;
            if s.learners == 1 {
                // A new ramp-up measures afresh: the link may have changed.
                s.peak = 0.0;
            }
        } else {
            s.learners -= 1;
        }
        self.bucket.set_rate_at(s.effective(), now);
    }

    /// Bytes read so far by every connection: ramp-up's aggregate.
    pub fn total(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }

    /// Aggregate throughput a ramp-up window measured. Raises the peak only
    /// while someone is ramping up (the cap is lifted then).
    pub fn offer_peak(&self, rate: f64) {
        let mut s = lock(&self.state);
        if s.learners > 0 {
            s.peak = s.peak.max(rate);
        }
    }
}

/// See [`GlobalLimit::learner`].
#[derive(Debug)]
pub struct Learner<'a> {
    global: &'a GlobalLimit,
    on: bool,
}

impl Learner<'_> {
    pub fn set(&mut self, on: bool) {
        if on != self.on {
            self.on = on;
            self.global.learning(on, Instant::now());
        }
    }
}

impl Drop for Learner<'_> {
    fn drop(&mut self) {
        self.set(false);
    }
}

/// What one download's connections acquire from: the global limit, its
/// queue's and its own.
#[derive(Debug)]
pub struct Limiter {
    global: Arc<GlobalLimit>,
    buckets: Vec<Arc<Bucket>>,
    changed: Arc<Notify>,
    waits: AtomicU64,
}

impl Limiter {
    pub fn new(global: Arc<GlobalLimit>, buckets: Vec<Arc<Bucket>>) -> Self {
        let changed = global.bucket.changed.clone();
        Self {
            global,
            buckets,
            changed,
            waits: AtomicU64::new(0),
        }
    }

    /// No limits at all.
    pub fn unlimited() -> Self {
        Self::new(Arc::new(GlobalLimit::new(Arc::default())), Vec::new())
    }

    fn all(&self) -> impl Iterator<Item = &Bucket> {
        std::iter::once(&*self.global.bucket).chain(self.buckets.iter().map(|b| &**b))
    }

    /// The most to hand on before the next [`consume`](Self::consume):
    /// [`READ_SIZE`], or the lowest limit below it, so small limits pace
    /// evenly.
    pub fn piece(&self) -> usize {
        self.all()
            .filter_map(Bucket::rate)
            .fold(READ_SIZE as u64, |p, r| p.min(r.max(1))) as usize
    }

    /// How many times a connection had to wait: ramp-up must not read a
    /// limited download as a saturated link.
    pub fn waits(&self) -> u64 {
        self.waits.load(Ordering::Relaxed)
    }

    /// Accounts `n` bytes just read, then waits until every bucket allows
    /// the next read (§7 `limiter.consume`).
    pub async fn consume(&self, n: u64) {
        let now = Instant::now();
        self.global.total.fetch_add(n, Ordering::Relaxed);
        for b in self.all() {
            b.debit(n, now);
        }
        let mut waited = false;
        loop {
            // Registered before the check, so a change during it still wakes.
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let now = Instant::now();
            let wait = self.all().map(|b| b.wait(now)).fold(Wait::Ready, longest);
            if wait == Wait::Ready {
                break;
            }
            if !waited {
                waited = true;
                self.waits.fetch_add(1, Ordering::Relaxed);
            }
            match wait {
                Wait::For(d) => {
                    tokio::select! {
                        _ = tokio::time::sleep(d) => {}
                        _ = &mut changed => {}
                    }
                }
                _ => changed.await,
            }
        }
    }
}

fn longest(a: Wait, b: Wait) -> Wait {
    match (a, b) {
        (Wait::UntilChanged, _) | (_, Wait::UntilChanged) => Wait::UntilChanged,
        (Wait::For(x), Wait::For(y)) => Wait::For(x.max(y)),
        (Wait::For(x), _) | (_, Wait::For(x)) => Wait::For(x),
        _ => Wait::Ready,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bucket(rate: Option<u64>) -> Bucket {
        Bucket::new(rate, Arc::default())
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn starts_with_a_burst_of_two_ticks() {
        let b = bucket(Some(10_000));
        let t0 = lock(&b.state).last;
        assert_eq!(lock(&b.state).tokens, 2_000.0);
        b.debit(2_000, t0);
        assert_eq!(b.wait(t0), Wait::Ready, "exactly spent is not debt");
        b.debit(1, t0);
        assert_eq!(b.wait(t0), Wait::For(ms(100)));
    }

    #[test]
    fn debt_is_repaid_in_whole_ticks() {
        let b = bucket(Some(10_000));
        let t0 = lock(&b.state).last;
        b.debit(2_000 + 2_500, t0);
        // 2 500 owed at 1 000 per tick: three ticks.
        assert_eq!(b.wait(t0), Wait::For(ms(300)));
        assert_eq!(b.wait(t0 + ms(150)), Wait::For(ms(150)));
        assert_eq!(b.wait(t0 + ms(299)), Wait::For(ms(1)));
        assert_eq!(b.wait(t0 + ms(300)), Wait::Ready);
    }

    #[test]
    fn refill_stops_at_the_burst() {
        let b = bucket(Some(10_000));
        let t0 = lock(&b.state).last;
        b.debit(2_000, t0);
        b.wait(t0 + Duration::from_secs(60));
        assert_eq!(lock(&b.state).tokens, 2_000.0);
    }

    #[test]
    fn paces_to_the_rate() {
        // Reading 1 000-byte pieces as fast as allowed for 10 s.
        let b = bucket(Some(5_000));
        let t0 = lock(&b.state).last;
        let mut now = t0;
        let mut read = 0;
        while now < t0 + Duration::from_secs(10) {
            match b.wait(now) {
                Wait::Ready => {
                    b.debit(1_000, now);
                    read += 1_000;
                }
                Wait::For(d) => now += d,
                Wait::UntilChanged => unreachable!(),
            }
        }
        // 10 s at 5 000/s, plus the initial burst and the last piece.
        assert!((50_000..=52_000).contains(&read), "{read}");
    }

    #[test]
    fn zero_waits_until_changed_and_unlimited_never_waits() {
        let b = bucket(Some(0));
        let t0 = Instant::now();
        assert_eq!(b.wait(t0), Wait::UntilChanged);
        b.set_rate_at(None, t0);
        b.debit(1 << 30, t0);
        assert_eq!(b.wait(t0), Wait::Ready);
    }

    #[test]
    fn changing_the_rate() {
        let b = bucket(Some(1_000));
        let t0 = lock(&b.state).last;
        b.debit(10_000, t0);
        assert!(matches!(b.wait(t0), Wait::For(d) if d > Duration::from_secs(9)));
        // Faster: the same debt repays sooner.
        b.set_rate_at(Some(100_000), t0);
        assert_eq!(b.wait(t0), Wait::For(ms(100)));
        // Lifted, then limited again: starts with a fresh burst.
        b.set_rate_at(None, t0);
        b.debit(1 << 20, t0);
        b.set_rate_at(Some(1_000), t0);
        assert_eq!(b.wait(t0), Wait::Ready);
        assert_eq!(lock(&b.state).tokens, 200.0);
        // Lower: what's left is capped to the new burst.
        b.set_rate_at(Some(500), t0);
        assert_eq!(lock(&b.state).tokens, 100.0);
    }

    #[test]
    fn pieces_shrink_to_small_limits() {
        let global = Arc::new(GlobalLimit::new(Arc::default()));
        let own = Arc::new(bucket(None));
        let l = Limiter::new(global.clone(), vec![own.clone()]);
        assert_eq!(l.piece(), READ_SIZE);
        own.set_rate(Some(10_000));
        assert_eq!(l.piece(), 10_000);
        global.set_user(Some(1_000));
        assert_eq!(l.piece(), 1_000);
        own.set_rate(Some(0));
        assert_eq!(l.piece(), 1, "never zero");
    }

    #[test]
    fn headroom_caps_at_the_peak_seen_while_ramping() {
        let g = GlobalLimit::new(Arc::default());
        g.set_user(Some(10_000_000));
        g.set_headroom(10);
        let t0 = Instant::now();
        // Not ramping: nothing is learned.
        g.offer_peak(5_000_000.0);
        assert_eq!(g.peak(), None);
        g.learning(true, t0);
        g.offer_peak(2_000_000.0);
        assert_eq!(g.rate(), Some(10_000_000), "no cap while ramping");
        g.offer_peak(500_000.0);
        assert_eq!(g.peak(), Some(2_000_000), "the peak only rises");
        g.learning(false, t0);
        assert_eq!(g.rate(), Some(1_800_000), "(1 − 10 %) × peak");
        g.set_user(Some(1_000_000));
        assert_eq!(g.rate(), Some(1_000_000), "the lower of both");
        g.set_user(None);
        g.set_headroom(0);
        assert_eq!(g.rate(), None);
    }

    #[test]
    fn a_new_ramp_up_measures_afresh() {
        let g = GlobalLimit::new(Arc::default());
        g.set_headroom(50);
        let t0 = Instant::now();
        g.learning(true, t0);
        g.offer_peak(4_000.0);
        g.learning(false, t0);
        assert_eq!(g.rate(), Some(2_000));
        // Two downloads ramping: the cap stays lifted until both are done.
        g.learning(true, t0);
        g.learning(true, t0);
        assert_eq!((g.rate(), g.peak()), (None, None));
        g.offer_peak(1_000.0);
        g.learning(false, t0);
        assert_eq!(g.rate(), None);
        g.learning(false, t0);
        assert_eq!(g.rate(), Some(500), "the link got slower");
    }

    #[tokio::test]
    async fn consume_paces_and_counts_waits() {
        let global = Arc::new(GlobalLimit::new(Arc::default()));
        global.set_user(Some(100_000));
        let l = Limiter::new(global, vec![]);
        let t0 = Instant::now();
        // Burst 20 000, then 10 000 per tick.
        for _ in 0..5 {
            l.consume(10_000).await;
        }
        let took = t0.elapsed();
        assert!(took >= ms(250) && took < ms(600), "{took:?}");
        assert!(l.waits() > 0);
    }

    #[tokio::test]
    async fn a_change_wakes_a_waiting_reader() {
        let changed = Arc::new(Notify::new());
        let global = Arc::new(GlobalLimit::new(changed.clone()));
        let own = Arc::new(Bucket::new(Some(0), changed));
        let l = Arc::new(Limiter::new(global, vec![own.clone()]));
        let reader = tokio::spawn({
            let l = l.clone();
            async move { l.consume(1).await }
        });
        tokio::time::sleep(ms(100)).await;
        assert!(!reader.is_finished(), "paused by limit");
        let t0 = Instant::now();
        own.set_rate(Some(1_000_000));
        reader.await.unwrap();
        // Woken at once; the byte it owes is repaid on the next tick
        // (§12: a change applies within 200 ms).
        assert!(t0.elapsed() < ms(200), "{:?}", t0.elapsed());
    }
}
