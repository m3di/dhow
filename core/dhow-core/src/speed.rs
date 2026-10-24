//! Speed and ETA from once-a-second samples (ENGINE-SPEC §14).

/// Samples the average spans: α = 2 / (N + 1).
pub const EMA_SAMPLES: u32 = 5;
/// No ETA before this many samples.
pub const MIN_SAMPLES: u32 = 3;
/// No ETA below this speed (bytes/s).
pub const MIN_SPEED: f64 = 1024.0;

/// 1 s samples are kept this long, then rolled into hourly rows once the
/// hour they belong to has ended.
pub const RING_MS: i64 = 10 * 60 * 1000;
pub const HOUR_MS: i64 = 3600 * 1000;

/// Exponential moving average of bytes/s.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Ema {
    value: f64,
    samples: u32,
}

impl Ema {
    pub fn push(&mut self, bytes_per_sec: f64) {
        self.value = if self.samples == 0 {
            bytes_per_sec
        } else {
            let alpha = 2.0 / f64::from(EMA_SAMPLES + 1);
            alpha * bytes_per_sec + (1.0 - alpha) * self.value
        };
        self.samples += 1;
    }

    /// Bytes/s, 0 before the first sample.
    pub fn speed(&self) -> u64 {
        self.value.round() as u64
    }

    /// Seconds left for `remaining` bytes; `None` when unknowable: no size,
    /// fewer than [`MIN_SAMPLES`], or slower than [`MIN_SPEED`].
    pub fn eta(&self, remaining: Option<u64>) -> Option<u64> {
        let remaining = remaining?;
        if self.samples < MIN_SAMPLES || self.value < MIN_SPEED {
            return None;
        }
        Some((remaining as f64 / self.value).ceil() as u64)
    }

    /// Several downloads together: speeds add up; the ETA needs
    /// [`MIN_SAMPLES`] from each.
    pub fn combined(emas: impl IntoIterator<Item = Ema>) -> Ema {
        emas.into_iter()
            .reduce(|a, b| Ema {
                value: a.value + b.value,
                samples: a.samples.min(b.samples),
            })
            .unwrap_or_default()
    }
}

/// 1 s samples taken before this (unix ms) are rolled up: their hour ended
/// and they are older than [`RING_MS`].
pub fn rollup_cutoff(now_ms: i64) -> i64 {
    (now_ms - RING_MS).div_euclid(HOUR_MS) * HOUR_MS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ema_weights_the_last_five_samples() {
        let mut e = Ema::default();
        assert_eq!(e.speed(), 0);
        e.push(3000.0);
        assert_eq!(e.speed(), 3000, "starts at the first sample");
        e.push(6000.0);
        assert_eq!(e.speed(), 4000, "α = 1/3");
        for _ in 0..30 {
            e.push(1000.0);
        }
        assert_eq!(e.speed(), 1000, "converges");
    }

    #[test]
    fn eta_needs_three_samples_a_size_and_1_kib_s() {
        let mut e = Ema::default();
        e.push(10_000.0);
        e.push(10_000.0);
        assert_eq!(e.eta(Some(100_000)), None, "two samples");
        e.push(10_000.0);
        assert_eq!(e.eta(Some(100_000)), Some(10));
        assert_eq!(e.eta(Some(100_001)), Some(11), "rounds up");
        assert_eq!(e.eta(Some(0)), Some(0));
        assert_eq!(e.eta(None), None, "blind mode");
        let mut slow = Ema::default();
        for _ in 0..5 {
            slow.push(1000.0);
        }
        assert_eq!(slow.eta(Some(1)), None, "under 1 KiB/s");
    }

    #[test]
    fn rollup_takes_whole_hours_ten_minutes_old() {
        let h = HOUR_MS;
        // 10:05: 09:55 is in the 09:00 hour, which isn't over 10 minutes yet.
        assert_eq!(rollup_cutoff(10 * h + 5 * 60_000), 9 * h);
        // 10:10: the 09:00 hour ended 10 minutes ago.
        assert_eq!(rollup_cutoff(10 * h + 10 * 60_000), 10 * h);
    }
}
