//! Deterministic content: byte `i` of blob `(seed, size)` is byte `i` of a
//! blake3 XOF stream keyed by `seed`, so any range can be produced directly
//! and tests can compute the expected digest without storing the file.

use std::ops::Range;

const CONTEXT: &str = "dhow-testserver blob v1";
const BUF: usize = 64 * 1024;

fn stream(seed: u64, offset: u64) -> blake3::OutputReader {
    let mut hasher = blake3::Hasher::new_derive_key(CONTEXT);
    hasher.update(&seed.to_le_bytes());
    let mut reader = hasher.finalize_xof();
    reader.set_position(offset);
    reader
}

/// Fills `buf` with the blob's bytes starting at `offset`.
pub fn fill(seed: u64, offset: u64, buf: &mut [u8]) {
    stream(seed, offset).fill(buf);
}

/// The blob's bytes in `range`. For tests; servers stream via [`fill`].
pub fn bytes(seed: u64, range: Range<u64>) -> Vec<u8> {
    let mut buf = vec![0; (range.end - range.start) as usize];
    fill(seed, range.start, &mut buf);
    buf
}

/// blake3 of the whole blob, i.e. what a bit-exact download hashes to.
pub fn digest(seed: u64, size: u64) -> blake3::Hash {
    let mut reader = stream(seed, 0);
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0; BUF];
    let mut left = size;
    while left > 0 {
        let n = left.min(BUF as u64) as usize;
        reader.fill(&mut buf[..n]);
        hasher.update(&buf[..n]);
        left -= n as u64;
    }
    hasher.finalize()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_agree_with_whole() {
        let whole = bytes(7, 0..200_000);
        assert_eq!(bytes(7, 70_000..140_001), whole[70_000..140_001]);
        assert_eq!(digest(7, 200_000), blake3::hash(&whole));
    }

    #[test]
    fn seeds_differ() {
        assert_ne!(bytes(1, 0..32), bytes(2, 0..32));
    }
}
