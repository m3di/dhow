//! How many connections a download uses, when it adds one, and where work
//! is split (ENGINE-SPEC §4.1, §4.3, §4.4).

use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug)]
pub struct RampConfig {
    /// Connections per download unless the request overrides it.
    pub connections: u8,
    pub max_connections: u8,
    /// No more connections than `ceil(total / min_chunk)`; also the
    /// smallest half when splitting without speed estimates.
    pub min_chunk: u64,
    /// With speeds known, steal only if the work then finishes at least
    /// this much sooner (§4.3).
    pub min_steal_gain: Duration,
    /// How long throughput is measured after opening a connection (after a
    /// third of it again for the new connection to get going).
    pub window: Duration,
    /// Opening the last connection must have raised throughput this much.
    pub min_gain: f64,
    /// How long "the link saturates at k" is remembered (process-wide).
    pub saturation_ttl: Duration,
}

impl Default for RampConfig {
    fn default() -> Self {
        Self {
            connections: 8,
            max_connections: 16,
            min_chunk: 1024 * 1024,
            min_steal_gain: Duration::from_secs(1),
            window: Duration::from_secs(3),
            min_gain: 0.10,
            saturation_ttl: Duration::from_secs(60),
        }
    }
}

/// What this process has learned about the user's own link. Shared by all
/// downloads (ENGINE-SPEC §10.A).
#[derive(Debug, Default)]
pub struct LinkState {
    saturated: Mutex<Option<(u32, Instant)>>,
}

impl LinkState {
    pub fn record_saturation(&self, connections: u32, ttl: Duration) {
        *self.saturated.lock().unwrap() = Some((connections, Instant::now() + ttl));
    }

    /// Connections that saturated the link recently, if any.
    pub fn ceiling(&self) -> Option<u32> {
        let mut s = self.saturated.lock().unwrap();
        match *s {
            Some((k, until)) if Instant::now() < until => Some(k),
            Some(_) => {
                *s = None;
                None
            }
            None => None,
        }
    }
}

/// §4.1, without the per-host and global budgets (§10, M2.2).
pub fn connection_limit(
    total: Option<u64>,
    can_range: bool,
    requested: Option<u8>,
    config: &RampConfig,
    link: &LinkState,
) -> u32 {
    let Some(total) = total.filter(|_| can_range) else {
        return 1;
    };
    let wanted = u32::from(
        requested
            .unwrap_or(config.connections)
            .min(config.max_connections),
    );
    let by_size = total
        .div_ceil(config.min_chunk.max(1))
        .min(u64::from(u32::MAX)) as u32;
    let by_link = link.ceiling().unwrap_or(u32::MAX);
    wanted.min(by_size).min(by_link).max(1)
}

/// Where to split a chunk whose owner is at `pos` of `[.., end)`. With
/// both speeds known, the owner keeps a share proportional to its speed so
/// both parts finish together, and the split happens only if that beats
/// leaving it all to the owner by `min_steal_gain`. However small the
/// piece: a crawling owner's last kilobytes are exactly what must move.
/// Without speeds, halves chunks of at least `2 · min_chunk`.
pub fn split_point(
    pos: u64,
    end: u64,
    owner_rate: Option<f64>,
    new_rate: Option<f64>,
    config: &RampConfig,
) -> Option<u64> {
    let remaining = end.saturating_sub(pos);
    match (owner_rate, new_rate) {
        (Some(owner), Some(new)) if owner > 0.0 && new > 0.0 => {
            // Leave the owner at least one byte: it is mid-response.
            let keep = ((remaining as f64 * owner / (owner + new)) as u64).max(1);
            if keep >= remaining {
                return None;
            }
            let alone = remaining as f64 / owner;
            let together = (keep as f64 / owner).max((remaining - keep) as f64 / new);
            (alone - together >= config.min_steal_gain.as_secs_f64()).then_some(pos + keep)
        }
        _ => (remaining >= 2 * config.min_chunk).then_some(pos + remaining / 2),
    }
}

/// What ramp-up reads at each check: running totals.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sample {
    /// This download's bytes.
    pub bytes: u64,
    /// Times its limiter made a connection wait.
    pub waits: u64,
    /// Every download's bytes (the process-wide aggregate).
    pub aggregate: u64,
}

/// The ramp-up decision of §4.4 (issue #8): after opening connection `k`,
/// let it settle, measure aggregate throughput for a window, and keep going
/// only if it rose by `min_gain` over what `k - 1` connections did. A
/// stalled download (no bytes in a window) doesn't ramp, and neither does a
/// limited one (§12): a window in which a speed limit made a connection
/// wait measures the limit, not the link, so it is measured again.
#[derive(Debug)]
pub struct Ramp {
    phase: Phase,
    /// Throughput measured before the last connection opened.
    previous: Option<f64>,
    /// Aggregate throughput of the step the last full window ended
    /// (headroom's peak, §10.6), until taken.
    aggregate: Option<f64>,
    /// When the step being measured began (the connection was asked for)
    /// and the aggregate then: the peak spans the whole step. `None`: the
    /// window alone (the first, a re-measured one, or after `Blocked`).
    step: Option<(Instant, u64)>,
    pub next_check: Instant,
    pub done: bool,
}

#[derive(Debug)]
enum Phase {
    /// A connection just opened; its start-up would skew the measurement.
    Settling,
    /// Ramp-up wanted one more and the budget refused: ask again, without
    /// re-measuring (the same connections would look like saturation).
    Blocked,
    Measuring {
        from: Instant,
        start: Sample,
    },
}

pub enum Step {
    /// Not yet: check again at `next_check`.
    Wait,
    /// Open one more connection.
    Open,
    /// Throughput stopped improving at this many connections.
    Saturated,
}

impl Ramp {
    pub fn new(now: Instant, config: &RampConfig) -> Self {
        Self {
            phase: Phase::Settling,
            previous: None,
            aggregate: None,
            step: None,
            next_check: now + settle(config),
            done: false,
        }
    }

    /// Called at `next_check`.
    pub fn check(&mut self, now: Instant, sample: Sample, config: &RampConfig) -> Step {
        let (from, start) = match self.phase {
            Phase::Measuring { from, start } if start.waits == sample.waits => (from, start),
            Phase::Settling => {
                self.measure(now, sample, config);
                return Step::Wait;
            }
            // A limit made it wait, so it measured the limit: measure again.
            Phase::Measuring { .. } => {
                self.remeasure(now, sample, config);
                return Step::Wait;
            }
            // While held back it wasn't probing, so the headroom cap applied
            // and left a backlog that drains once it lifts: this step counts
            // its window alone.
            Phase::Blocked => {
                self.step = None;
                self.phase = Phase::Settling;
                self.next_check = now + settle(config);
                return Step::Open;
            }
        };
        let delta = sample.bytes.saturating_sub(start.bytes);
        let elapsed = now.saturating_duration_since(from).as_secs_f64();
        if delta == 0 || elapsed <= 0.0 {
            self.remeasure(now, sample, config);
            return Step::Wait;
        }
        // Over the whole step: a stall that held the connections back
        // (a commit on a slow disk) is then counted with the backlog it
        // left, which arrives all at once.
        let (since, base) = self.step.unwrap_or((from, start.aggregate));
        let span = now.saturating_duration_since(since).as_secs_f64();
        self.aggregate = Some(sample.aggregate.saturating_sub(base) as f64 / span);
        let rate = delta as f64 / elapsed;
        if let Some(previous) = self.previous
            && rate < previous * (1.0 + config.min_gain)
        {
            self.done = true;
            return Step::Saturated;
        }
        self.previous = Some(rate);
        self.step = Some((now, sample.aggregate));
        self.phase = Phase::Settling;
        self.next_check = now + settle(config);
        Step::Open
    }

    fn measure(&mut self, now: Instant, start: Sample, config: &RampConfig) {
        self.phase = Phase::Measuring { from: now, start };
        self.next_check = now + config.window;
    }

    /// The window didn't count: its step starts over with the next one.
    fn remeasure(&mut self, now: Instant, start: Sample, config: &RampConfig) {
        self.step = None;
        self.measure(now, start, config);
    }

    /// The aggregate throughput of the last step measured, once.
    pub fn take_aggregate(&mut self) -> Option<f64> {
        self.aggregate.take()
    }

    /// Settling or measuring: what it sees is the link's answer to more
    /// connections (headroom learns the peak then, §10.6).
    pub fn probing(&self) -> bool {
        !self.done && !matches!(self.phase, Phase::Blocked)
    }

    /// The budget refused the connection `check` asked for: ask again in a
    /// window.
    pub fn blocked(&mut self, now: Instant, config: &RampConfig) {
        self.phase = Phase::Blocked;
        self.next_check = now + config.window;
    }

    /// A connection error: hold off for the backoff (§4.4).
    pub fn pause_until(&mut self, until: Instant) {
        self.next_check = self.next_check.max(until);
    }
}

fn settle(config: &RampConfig) -> Duration {
    config.window / 3
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1024 * 1024;

    fn s(bytes: u64, waits: u64) -> Sample {
        Sample {
            bytes,
            waits,
            aggregate: bytes * 2,
        }
    }

    #[test]
    fn limit_follows_4_1() {
        let c = RampConfig::default();
        let link = LinkState::default();
        assert_eq!(connection_limit(Some(100 * MIB), true, None, &c, &link), 8);
        assert_eq!(
            connection_limit(Some(100 * MIB), true, Some(32), &c, &link),
            16
        );
        assert_eq!(
            connection_limit(Some(100 * MIB), true, Some(2), &c, &link),
            2
        );
        assert_eq!(
            connection_limit(Some(3 * MIB), true, None, &c, &link),
            3,
            "min chunk"
        );
        assert_eq!(connection_limit(Some(10), true, None, &c, &link), 1);
        assert_eq!(connection_limit(Some(100 * MIB), false, None, &c, &link), 1);
        assert_eq!(connection_limit(None, true, None, &c, &link), 1);
        link.record_saturation(4, Duration::from_secs(60));
        assert_eq!(connection_limit(Some(100 * MIB), true, None, &c, &link), 4);
        link.record_saturation(4, Duration::ZERO);
        assert_eq!(link.ceiling(), None, "forgotten after the ttl");
    }

    #[test]
    fn split_without_speeds_halves_big_chunks_only() {
        let c = RampConfig::default();
        assert_eq!(split_point(0, 4 * MIB, None, None, &c), Some(2 * MIB));
        assert_eq!(
            split_point(MIB, 3 * MIB, None, Some(1.0), &c),
            Some(2 * MIB)
        );
        assert_eq!(split_point(0, 2 * MIB - 1, None, None, &c), None);
    }

    #[test]
    fn split_with_speeds_balances_finish_times() {
        let c = RampConfig::default();
        // Equal speeds: halves (10 s left → 5 s, saves 5 s).
        assert_eq!(
            split_point(0, 1000, Some(100.0), Some(100.0), &c),
            Some(500)
        );
        // Owner 3x slower: keeps a quarter.
        assert_eq!(
            split_point(0, 1000, Some(100.0), Some(300.0), &c),
            Some(250)
        );
        // A crawling owner keeps almost nothing (T-4.3).
        let at = split_point(0, 4 * MIB, Some(1024.0), Some(1e8), &c).unwrap();
        assert!(at > 0 && at < 1024, "{at}");
    }

    #[test]
    fn tiny_tails_move_when_it_saves_time() {
        let c = RampConfig::default();
        // 2 KB at 1 KiB/s is two seconds; a fast connection does it at once.
        assert!(split_point(0, 2281, Some(1024.0), Some(1e6), &c).is_some());
    }

    #[test]
    fn no_split_when_not_worth_it() {
        let c = RampConfig::default();
        // One second left at equal speeds: splitting saves half a second.
        assert_eq!(split_point(0, 1000, Some(1000.0), Some(1000.0), &c), None);
        // A new connection slower than the owner can't help much.
        assert_eq!(split_point(0, 1500, Some(1000.0), Some(10.0), &c), None);
        // Nothing to split.
        assert_eq!(split_point(10, 10, Some(1.0), Some(1.0), &c), None);
        assert_eq!(split_point(9, 10, Some(1.0), Some(1.0), &c), None);
    }

    #[test]
    fn ramp_opens_while_throughput_grows_and_stops_when_it_doesnt() {
        let c = RampConfig {
            window: Duration::from_secs(3),
            ..RampConfig::default()
        };
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut r = Ramp::new(t0, &c);
        assert_eq!(r.next_check, at(1), "settles a third of a window first");
        // Measure 1 → 4 s with one connection: 1 MB/s. Open a second.
        assert!(matches!(r.check(at(1), s(0, 0), &c), Step::Wait));
        assert!(matches!(r.check(at(4), s(3_000_000, 0), &c), Step::Open));
        // Settle, then 2 MB/s: open a third.
        assert!(matches!(r.check(at(5), s(4_000_000, 0), &c), Step::Wait));
        assert!(matches!(r.check(at(8), s(10_000_000, 0), &c), Step::Open));
        // 2.1 MB/s: under +10 %, saturated.
        assert!(matches!(r.check(at(9), s(12_000_000, 0), &c), Step::Wait));
        assert!(matches!(
            r.check(at(12), s(18_300_000, 0), &c),
            Step::Saturated
        ));
        assert!(r.done);
        // Every download's bytes over the last step, settling included:
        // headroom's peak.
        assert_eq!(r.take_aggregate(), Some(4_150_000.0));
        assert_eq!(r.take_aggregate(), None);
    }

    #[test]
    fn a_window_measured_again_starts_its_step_over() {
        let c = RampConfig::default();
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut r = Ramp::new(t0, &c);
        r.check(at(1), s(0, 0), &c);
        assert!(matches!(r.check(at(4), s(3_000_000, 0), &c), Step::Open));
        r.check(at(5), s(4_000_000, 0), &c);
        // Limited: measured again, and the limited time doesn't count.
        assert!(matches!(r.check(at(8), s(5_000_000, 1), &c), Step::Wait));
        assert!(matches!(r.check(at(11), s(11_000_000, 1), &c), Step::Open));
        assert_eq!(r.take_aggregate(), Some(4_000_000.0), "the window alone");
    }

    #[test]
    fn a_step_the_budget_held_back_counts_its_window_alone() {
        let c = RampConfig::default();
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut r = Ramp::new(t0, &c);
        r.check(at(1), s(0, 0), &c);
        assert!(matches!(r.check(at(4), s(3_000_000, 0), &c), Step::Open));
        r.blocked(at(4), &c);
        assert!(!r.probing(), "the headroom cap applies meanwhile");
        // Asked again: settling drains what the cap held back (4 MB in 1 s).
        assert!(matches!(r.check(at(7), s(6_000_000, 0), &c), Step::Open));
        r.check(at(8), s(10_000_000, 0), &c);
        assert!(matches!(r.check(at(11), s(16_000_000, 0), &c), Step::Open));
        assert_eq!(r.take_aggregate(), Some(4_000_000.0), "the window alone");
    }

    #[test]
    fn the_peak_counts_a_stall_with_its_backlog() {
        let c = RampConfig::default();
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut r = Ramp::new(t0, &c);
        r.check(at(1), s(0, 0), &c);
        assert!(matches!(r.check(at(4), s(3_000_000, 0), &c), Step::Open));
        // The step began with a 2 s stall: nothing arrived until the
        // window, which then got the backlog all at once.
        r.check(at(6), s(3_000_000, 0), &c);
        assert!(matches!(r.check(at(9), s(13_000_000, 0), &c), Step::Open));
        // The window alone saw 20 MB in 3 s; over the step it is 4 MB/s.
        assert_eq!(r.take_aggregate(), Some(4_000_000.0));
    }

    #[test]
    fn a_limited_window_is_measured_again() {
        let c = RampConfig::default();
        let t0 = Instant::now();
        let mut r = Ramp::new(t0, &c);
        let m = t0 + settle(&c);
        r.check(m, s(0, 0), &c);
        assert!(matches!(r.check(m + c.window, s(1_000, 0), &c), Step::Open));
        let m = m + c.window + settle(&c);
        r.check(m, s(1_000, 0), &c);
        // No faster with two, but a limit made it wait: neither saturated
        // nor a reason to open more.
        assert!(matches!(r.check(m + c.window, s(2_000, 3), &c), Step::Wait));
        assert!(!r.done);
        // The limit lifted: measured for real.
        let m = m + c.window * 2;
        assert!(matches!(r.check(m, s(10_000, 3), &c), Step::Open));
    }

    #[test]
    fn a_slow_connection_still_ramps_but_a_stalled_one_does_not() {
        let c = RampConfig::default();
        let t0 = Instant::now();
        let mut r = Ramp::new(t0, &c);
        let measuring = t0 + settle(&c);
        r.check(measuring, s(0, 0), &c);
        // Nothing arrived in the window: wait.
        assert!(matches!(
            r.check(measuring + c.window, s(0, 0), &c),
            Step::Wait
        ));
        // A trickle is enough to try another connection.
        let later = measuring + c.window * 2;
        assert!(matches!(r.check(later, s(1024, 0), &c), Step::Open));
    }
}
