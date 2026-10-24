//! Sidecar byte format (ENGINE-SPEC §5.1). All integers little-endian.
//!
//! ```text
//! header (256 bytes)                       record (24 bytes)
//!   0  magic "DHOW"        4                 0  kind     u8
//!   4  version u16 = 1     2                 1  pad      [u8; 3]
//!   6  flags u16           2                 4  a        u64
//!   8  total_size u64      8 (MAX = unknown) 12 b        u64
//!  16  url_hash [u8; 32]  32                20  crc32    u32 over 0..20
//!  48  etag_len u16        2
//!  50  etag [u8; 120]    120
//! 170  last_mod i64        8 (unix s, 0 = none)
//! 178  created_at i64      8 (unix ms)
//! 186  reserved           66
//! 252  crc32 u32           4 over 0..252
//! ```

use std::ops::Range;

use crate::rangeset::RangeSet;

pub const HEADER_LEN: usize = 256;
pub const RECORD_LEN: usize = 24;
const MAGIC: &[u8; 4] = b"DHOW";
const VERSION: u16 = 1;
const ETAG_MAX: usize = 120;
const FLAG_PREALLOCATED: u16 = 1;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Header {
    pub preallocated: bool,
    pub total_size: Option<u64>,
    /// blake3 of the original URL.
    pub url_hash: [u8; 32],
    /// `None` also when the ETag was longer than the 120 bytes stored:
    /// a truncated ETag must never reach `If-Range`.
    pub etag: Option<String>,
    /// Unix seconds.
    pub last_modified: Option<i64>,
    /// Unix milliseconds.
    pub created_at: i64,
}

pub fn url_hash(url: &str) -> [u8; 32] {
    *blake3::hash(url.as_bytes()).as_bytes()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Record {
    /// A chunk `[start, end)` exists.
    Define(Range<u64>),
    /// `[start, end)` is on disk and fsync'd (I2).
    Commit(Range<u64>),
    /// Informational: a chunk was split; its owner keeps `[.., keep_to)`, the
    /// new chunk starts at `new_start` and has its own `Define`.
    Split { keep_to: u64, new_start: u64 },
    /// Everything before was compacted (§5.3).
    Checkpoint,
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum HeaderError {
    #[error("sidecar shorter than its header")]
    Short,
    #[error("not a Dhow sidecar")]
    Magic,
    #[error("sidecar version {0} is not supported")]
    Version(u16),
    #[error("sidecar header checksum mismatch")]
    Checksum,
}

/// What replay recovered (§5.2).
#[derive(Debug, PartialEq, Eq)]
pub struct Replayed {
    pub header: Header,
    pub committed: RangeSet,
    /// Byte length of the valid prefix; anything after it is a torn or
    /// corrupt tail.
    pub valid_len: u64,
}

pub fn encode_header(h: &Header) -> [u8; HEADER_LEN] {
    let mut b = [0u8; HEADER_LEN];
    b[0..4].copy_from_slice(MAGIC);
    b[4..6].copy_from_slice(&VERSION.to_le_bytes());
    let flags = if h.preallocated { FLAG_PREALLOCATED } else { 0 };
    b[6..8].copy_from_slice(&flags.to_le_bytes());
    b[8..16].copy_from_slice(&h.total_size.unwrap_or(u64::MAX).to_le_bytes());
    b[16..48].copy_from_slice(&h.url_hash);
    let etag = h.etag.as_deref().unwrap_or_default().as_bytes();
    let etag_len = u16::try_from(etag.len()).unwrap_or(u16::MAX);
    b[48..50].copy_from_slice(&etag_len.to_le_bytes());
    let stored = etag.len().min(ETAG_MAX);
    b[50..50 + stored].copy_from_slice(&etag[..stored]);
    b[170..178].copy_from_slice(&h.last_modified.unwrap_or(0).to_le_bytes());
    b[178..186].copy_from_slice(&h.created_at.to_le_bytes());
    let crc = crc32fast::hash(&b[..252]);
    b[252..256].copy_from_slice(&crc.to_le_bytes());
    b
}

pub fn decode_header(b: &[u8]) -> Result<Header, HeaderError> {
    let b: &[u8; HEADER_LEN] = b
        .get(..HEADER_LEN)
        .and_then(|s| s.try_into().ok())
        .ok_or(HeaderError::Short)?;
    if &b[0..4] != MAGIC {
        return Err(HeaderError::Magic);
    }
    if crc32fast::hash(&b[..252]) != u32_at(b, 252) {
        return Err(HeaderError::Checksum);
    }
    let version = u16_at(b, 4);
    if version != VERSION {
        return Err(HeaderError::Version(version));
    }
    let etag_len = u16_at(b, 48) as usize;
    let etag = match etag_len {
        0 => None,
        n if n > ETAG_MAX => None,
        n => std::str::from_utf8(&b[50..50 + n]).ok().map(str::to_owned),
    };
    let total = u64_at(b, 8);
    let last_mod = u64_at(b, 170) as i64;
    Ok(Header {
        preallocated: u16_at(b, 6) & FLAG_PREALLOCATED != 0,
        total_size: (total != u64::MAX).then_some(total),
        url_hash: b[16..48].try_into().unwrap(),
        etag,
        last_modified: (last_mod != 0).then_some(last_mod),
        created_at: u64_at(b, 178) as i64,
    })
}

pub fn encode_record(r: &Record) -> [u8; RECORD_LEN] {
    let (kind, a, b) = match r {
        Record::Define(r) => (1u8, r.start, r.end),
        Record::Commit(r) => (2, r.start, r.end),
        Record::Split { keep_to, new_start } => (3, *keep_to, *new_start),
        Record::Checkpoint => (4, 0, 0),
    };
    let mut buf = [0u8; RECORD_LEN];
    buf[0] = kind;
    buf[4..12].copy_from_slice(&a.to_le_bytes());
    buf[12..20].copy_from_slice(&b.to_le_bytes());
    let crc = crc32fast::hash(&buf[..20]);
    buf[20..24].copy_from_slice(&crc.to_le_bytes());
    buf
}

/// `None` for anything the writer could not have produced: bad checksum,
/// unknown kind, non-zero padding, empty or out-of-bounds range. Replay stops
/// at the first such record: bytes we cannot account for are absent (I3).
pub fn decode_record(buf: &[u8; RECORD_LEN], total: Option<u64>) -> Option<Record> {
    if crc32fast::hash(&buf[..20]) != u32_at(buf, 20) || buf[1..4] != [0, 0, 0] {
        return None;
    }
    let (a, b) = (u64_at(buf, 4), u64_at(buf, 12));
    let in_bounds = |end: u64| total.is_none_or(|t| end <= t);
    match buf[0] {
        1 if a < b && in_bounds(b) => Some(Record::Define(a..b)),
        2 if a < b && in_bounds(b) => Some(Record::Commit(a..b)),
        3 if a <= b && in_bounds(b) => Some(Record::Split {
            keep_to: a,
            new_start: b,
        }),
        4 if a == 0 && b == 0 => Some(Record::Checkpoint),
        _ => None,
    }
}

/// Replays a whole sidecar (§5.2). A partial trailing record is a torn
/// write and is ignored like a corrupt one.
pub fn replay(bytes: &[u8]) -> Result<Replayed, HeaderError> {
    let header = decode_header(bytes)?;
    let mut committed = RangeSet::new();
    let mut valid_len = HEADER_LEN;
    let (records, _torn) = bytes[HEADER_LEN..].as_chunks::<RECORD_LEN>();
    for chunk in records {
        let Some(record) = decode_record(chunk, header.total_size) else {
            break;
        };
        if let Record::Commit(r) = record {
            committed.insert(r);
        }
        valid_len += RECORD_LEN;
    }
    Ok(Replayed {
        header,
        committed,
        valid_len: valid_len as u64,
    })
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(b[at..at + 2].try_into().unwrap())
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

#[cfg(test)]
#[allow(clippy::reversed_empty_ranges)]
mod tests {
    use super::*;

    fn header() -> Header {
        Header {
            preallocated: true,
            total_size: Some(1000),
            url_hash: url_hash("https://example.com/f.zip"),
            etag: Some("\"abc\"".into()),
            last_modified: Some(1_767_225_600),
            created_at: 1_777_000_000_000,
        }
    }

    fn sidecar(records: &[Record]) -> Vec<u8> {
        let mut out = encode_header(&header()).to_vec();
        for r in records {
            out.extend_from_slice(&encode_record(r));
        }
        out
    }

    #[test]
    fn header_round_trips() {
        let h = header();
        assert_eq!(decode_header(&encode_header(&h)).unwrap(), h);
        let blind = Header {
            preallocated: false,
            total_size: None,
            etag: None,
            last_modified: None,
            ..h
        };
        assert_eq!(decode_header(&encode_header(&blind)).unwrap(), blind);
    }

    #[test]
    fn etag_at_limit_round_trips_and_longer_is_dropped() {
        let at_limit = Header {
            etag: Some("e".repeat(120)),
            ..header()
        };
        assert_eq!(decode_header(&encode_header(&at_limit)).unwrap(), at_limit);
        let longer = Header {
            etag: Some("e".repeat(121)),
            ..header()
        };
        assert_eq!(decode_header(&encode_header(&longer)).unwrap().etag, None);
    }

    #[test]
    fn header_rejections() {
        let good = encode_header(&header());
        assert_eq!(decode_header(&good[..255]), Err(HeaderError::Short));
        let mut bad = good;
        bad[0] = b'X';
        assert_eq!(decode_header(&bad), Err(HeaderError::Magic));
        let mut bad = good;
        bad[100] ^= 1;
        assert_eq!(decode_header(&bad), Err(HeaderError::Checksum));
        let mut v2 = good;
        v2[4] = 2;
        let crc = crc32fast::hash(&v2[..252]);
        v2[252..].copy_from_slice(&crc.to_le_bytes());
        assert_eq!(decode_header(&v2), Err(HeaderError::Version(2)));
    }

    #[test]
    fn records_round_trip() {
        for r in [
            Record::Define(0..500),
            Record::Commit(10..20),
            Record::Split {
                keep_to: 250,
                new_start: 250,
            },
            Record::Checkpoint,
        ] {
            assert_eq!(decode_record(&encode_record(&r), Some(1000)), Some(r));
        }
    }

    #[test]
    fn implausible_records_are_rejected() {
        let t = Some(1000);
        assert_eq!(
            decode_record(&encode_record(&Record::Commit(5..5)), t),
            None
        );
        assert_eq!(
            decode_record(&encode_record(&Record::Commit(0..1001)), t),
            None
        );
        assert_eq!(
            decode_record(&encode_record(&Record::Define(9..3)), t),
            None
        );
        let split = Record::Split {
            keep_to: 9,
            new_start: 3,
        };
        assert_eq!(decode_record(&encode_record(&split), t), None);
        // Blind mode: no upper bound.
        assert!(decode_record(&encode_record(&Record::Commit(0..u64::MAX)), None).is_some());
        let mut unknown = encode_record(&Record::Checkpoint);
        unknown[0] = 9;
        let crc = crc32fast::hash(&unknown[..20]);
        unknown[20..].copy_from_slice(&crc.to_le_bytes());
        assert_eq!(decode_record(&unknown, t), None);
    }

    #[test]
    fn replay_unions_commits() {
        let bytes = sidecar(&[
            Record::Define(0..500),
            Record::Define(500..1000),
            Record::Commit(0..100),
            Record::Commit(500..600),
            Record::Commit(100..200),
            Record::Checkpoint,
        ]);
        let r = replay(&bytes).unwrap();
        assert_eq!(r.committed.ranges(), [0..200, 500..600]);
        assert_eq!(r.valid_len, bytes.len() as u64);
    }

    /// Every possible torn write: the committed set is exactly the union of
    /// the records that were completely written.
    #[test]
    fn every_truncation_point() {
        let commits: Vec<Record> = (0..40)
            .map(|i| Record::Commit(i * 20..i * 20 + 15))
            .collect();
        let bytes = sidecar(&commits);
        for len in HEADER_LEN..=bytes.len() {
            let r = replay(&bytes[..len]).unwrap();
            let whole = (len - HEADER_LEN) / RECORD_LEN;
            let mut expect = RangeSet::new();
            for c in &commits[..whole] {
                if let Record::Commit(range) = c {
                    expect.insert(range.clone());
                }
            }
            assert_eq!(r.committed, expect, "len {len}");
            assert_eq!(r.valid_len as usize, HEADER_LEN + whole * RECORD_LEN);
        }
    }

    /// Every single-bit flip in the records: replay keeps exactly the
    /// records before the damaged one (CRC32 catches all 1-bit errors).
    #[test]
    fn every_bit_flip() {
        let commits: Vec<Record> = (0..8)
            .map(|i| Record::Commit(i * 100..i * 100 + 50))
            .collect();
        let bytes = sidecar(&commits);
        for bit in HEADER_LEN * 8..bytes.len() * 8 {
            let mut damaged = bytes.clone();
            damaged[bit / 8] ^= 1 << (bit % 8);
            let r = replay(&damaged).unwrap();
            let hit = (bit / 8 - HEADER_LEN) / RECORD_LEN;
            assert_eq!(r.committed.ranges().len(), hit, "bit {bit}");
            assert_eq!(r.valid_len as usize, HEADER_LEN + hit * RECORD_LEN);
        }
    }
}
