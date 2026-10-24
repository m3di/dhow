//! Sorted, disjoint, non-adjacent half-open byte intervals.

use std::ops::Range;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RangeSet {
    ranges: Vec<Range<u64>>,
}

impl RangeSet {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds `r`, merging with anything it overlaps or touches. Empty ranges
    /// are ignored.
    pub fn insert(&mut self, r: Range<u64>) {
        if r.start >= r.end {
            return;
        }
        // First range that ends at or after r.start: everything before it is
        // strictly left of r and untouched.
        let first = self.ranges.partition_point(|x| x.end < r.start);
        // First range that starts after r.end: it and everything after are
        // strictly right of r.
        let last = self.ranges.partition_point(|x| x.start <= r.end);
        let mut merged = r;
        if first < last {
            merged.start = merged.start.min(self.ranges[first].start);
            merged.end = merged.end.max(self.ranges[last - 1].end);
        }
        self.ranges.splice(first..last, [merged]);
    }

    pub fn ranges(&self) -> &[Range<u64>] {
        &self.ranges
    }

    /// Total bytes covered.
    pub fn len(&self) -> u64 {
        self.ranges.iter().map(|r| r.end - r.start).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    /// Whether any covered byte lies in `r`.
    pub fn intersects(&self, r: &Range<u64>) -> bool {
        let i = self.ranges.partition_point(|x| x.end <= r.start);
        self.ranges.get(i).is_some_and(|x| x.start < r.end) && r.start < r.end
    }

    /// The parts of `[0, total)` not covered, in order.
    pub fn gaps(&self, total: u64) -> Vec<Range<u64>> {
        let mut out = Vec::new();
        let mut at = 0;
        for r in &self.ranges {
            if r.start >= total {
                break;
            }
            if r.start > at {
                out.push(at..r.start);
            }
            at = at.max(r.end);
        }
        if at < total {
            out.push(at..total);
        }
        out
    }

    /// The covered range ending exactly at `at`, if any.
    pub fn ending_at(&self, at: u64) -> Option<&Range<u64>> {
        self.ranges.iter().find(|r| r.end == at)
    }

    /// Exactly `[0, total)`: the I7 completion condition.
    pub fn is_exactly(&self, total: u64) -> bool {
        match self.ranges.as_slice() {
            [] => total == 0,
            [only] => only.start == 0 && only.end == total,
            _ => false,
        }
    }
}

#[cfg(test)]
#[allow(clippy::single_range_in_vec_init, clippy::reversed_empty_ranges)]
mod tests {
    use super::*;

    fn set(rs: &[Range<u64>]) -> RangeSet {
        let mut s = RangeSet::new();
        for r in rs {
            s.insert(r.clone());
        }
        s
    }

    #[test]
    fn merges_overlapping_and_adjacent() {
        assert_eq!(set(&[0..10, 5..15]).ranges(), [0..15]);
        assert_eq!(set(&[0..10, 10..20]).ranges(), [0..20]);
        assert_eq!(set(&[20..30, 0..10]).ranges(), [0..10, 20..30]);
        assert_eq!(set(&[0..10, 20..30, 5..25]).ranges(), [0..30]);
        assert_eq!(set(&[0..10, 20..30, 40..50, 9..41]).ranges(), [0..50]);
        assert_eq!(set(&[5..6, 0..100]).ranges(), [0..100]);
        assert_eq!(set(&[0..100, 5..6]).ranges(), [0..100]);
    }

    #[test]
    fn intersects() {
        let s = set(&[10..20, 30..40]);
        assert!(s.intersects(&(15..16)));
        assert!(s.intersects(&(0..11)));
        assert!(s.intersects(&(19..31)));
        assert!(s.intersects(&(0..100)));
        assert!(!s.intersects(&(0..10)));
        assert!(!s.intersects(&(20..30)));
        assert!(!s.intersects(&(40..50)));
        assert!(!s.intersects(&(15..15)));
    }

    #[test]
    fn gaps_and_ending_at() {
        let s = set(&[10..20, 30..40]);
        assert_eq!(s.gaps(50), [0..10, 20..30, 40..50]);
        assert_eq!(s.gaps(35), [0..10, 20..30]);
        assert_eq!(set(&[0..50]).gaps(50), Vec::<Range<u64>>::new());
        assert_eq!(RangeSet::new().gaps(5), [0..5]);
        assert_eq!(s.ending_at(20), Some(&(10..20)));
        assert_eq!(s.ending_at(15), None);
    }

    #[test]
    fn ignores_empty() {
        assert!(set(&[5..5, 7..3]).is_empty());
    }

    #[test]
    fn len_and_completeness() {
        let s = set(&[0..10, 20..25]);
        assert_eq!(s.len(), 15);
        assert!(!s.is_exactly(25));
        assert!(set(&[0..25]).is_exactly(25));
        assert!(!set(&[0..25]).is_exactly(26));
        assert!(!set(&[1..25]).is_exactly(25));
        assert!(RangeSet::new().is_exactly(0));
    }

    #[test]
    fn matches_a_bitmap_model() {
        // Deterministic pseudo-random inserts checked against a byte bitmap.
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..200 {
            let mut s = RangeSet::new();
            let mut bits = [false; 256];
            for _ in 0..12 {
                let a = next() % 256;
                let b = (a + next() % 40).min(256);
                s.insert(a..b);
                bits[a as usize..b as usize].fill(true);
            }
            let mut expect = RangeSet::new();
            let mut i = 0;
            while i < 256 {
                if bits[i] {
                    let start = i;
                    while i < 256 && bits[i] {
                        i += 1;
                    }
                    expect.ranges.push(start as u64..i as u64);
                }
                i += 1;
            }
            assert_eq!(s, expect);
        }
    }
}
