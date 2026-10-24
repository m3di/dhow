//! Retry timing (ENGINE-SPEC §9).

use std::time::{Duration, SystemTime};

#[derive(Clone, Copy, Debug)]
pub struct RetryPolicy {
    /// Consecutive failed attempts before giving up (`settings.max_retries`).
    /// An attempt that wrote at least one byte resets the count.
    pub max_failures: u32,
    /// Backoff after the n-th consecutive failure: `base · 2^(n-1)`, ±25 %.
    pub base: Duration,
    pub cap: Duration,
    /// Longest `Retry-After` honored.
    pub retry_after_cap: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_failures: 10,
            base: Duration::from_secs(1),
            cap: Duration::from_secs(60),
            retry_after_cap: Duration::from_secs(300),
        }
    }
}

impl RetryPolicy {
    /// Delay after the `failures`-th consecutive failure (1-based); `jitter`
    /// in `[0, 1)` maps to a factor in `[0.75, 1.25)`.
    pub fn backoff(&self, failures: u32, jitter: f64) -> Duration {
        let doublings = failures.saturating_sub(1).min(31);
        let exp = self.base.saturating_mul(1 << doublings).min(self.cap);
        exp.mul_f64(0.75 + 0.5 * jitter).min(self.cap)
    }
}

/// `Retry-After`: delta-seconds or an HTTP-date relative to `now`.
pub fn retry_after(value: &str, now: SystemTime) -> Option<Duration> {
    let value = value.trim();
    if let Ok(secs) = value.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let at = httpdate::parse_http_date(value).ok()?;
    Some(at.duration_since(now).unwrap_or(Duration::ZERO))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_with_jitter_and_caps() {
        let p = RetryPolicy::default();
        assert_eq!(p.backoff(1, 0.5), Duration::from_secs(1));
        assert_eq!(p.backoff(2, 0.5), Duration::from_secs(2));
        assert_eq!(p.backoff(4, 0.5), Duration::from_secs(8));
        assert_eq!(p.backoff(1, 0.0), Duration::from_millis(750));
        assert!(p.backoff(1, 0.999) < Duration::from_millis(1250));
        assert_eq!(p.backoff(7, 0.5), Duration::from_secs(60));
        assert_eq!(
            p.backoff(7, 0.999),
            Duration::from_secs(60),
            "jitter never exceeds the cap"
        );
        assert_eq!(p.backoff(1000, 0.5), Duration::from_secs(60));
    }

    #[test]
    fn retry_after_forms() {
        let now = httpdate::parse_http_date("Thu, 01 Jan 2026 00:00:00 GMT").unwrap();
        assert_eq!(retry_after("120", now), Some(Duration::from_secs(120)));
        assert_eq!(retry_after(" 0 ", now), Some(Duration::ZERO));
        assert_eq!(
            retry_after("Thu, 01 Jan 2026 00:00:30 GMT", now),
            Some(Duration::from_secs(30))
        );
        assert_eq!(
            retry_after("Wed, 31 Dec 2025 23:00:00 GMT", now),
            Some(Duration::ZERO)
        );
        assert_eq!(retry_after("soon", now), None);
        assert_eq!(retry_after("-5", now), None);
    }
}
