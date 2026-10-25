#![allow(clippy::single_range_in_vec_init)]

use std::path::PathBuf;

use bytes::Bytes;

use super::actor::{self, Outcome};
use super::*;
use crate::journal::{Header, format};

const NO_AUTO: WriterConfig = WriterConfig {
    commit_interval: Duration::from_secs(3600),
    commit_bytes: u64::MAX,
    slow_sync: Duration::ZERO,
};

struct Fixture {
    _dir: tempfile::TempDir,
    part: PathBuf,
    sidecar: PathBuf,
}

impl Fixture {
    fn new(total: Option<u64>, config: WriterConfig) -> (Self, Writer) {
        let dir = tempfile::tempdir().unwrap();
        let part = dir.path().join("f.bin.part");
        let sidecar = dir.path().join("f.bin.dhow");
        let (file, preallocated) = create_part(&part, total).unwrap();
        let header = Header {
            preallocated,
            total_size: total,
            url_hash: format::url_hash("https://example.com/f.bin"),
            etag: None,
            last_modified: None,
            created_at: 0,
        };
        let journal = Journal::create(&sidecar, header).unwrap();
        let writer = Writer::new(file, journal, config);
        (
            Self {
                _dir: dir,
                part,
                sidecar,
            },
            writer,
        )
    }

    /// What a crash right now would leave: replay of the sidecar on disk.
    fn committed_on_disk(&self) -> RangeSet {
        format::replay(&std::fs::read(&self.sidecar).unwrap())
            .unwrap()
            .committed
    }

    fn part_bytes(&self) -> Vec<u8> {
        std::fs::read(&self.part).unwrap()
    }
}

/// Awaits `f`, failing the test instead of hanging it if the writer never
/// answers (a regression here would otherwise stall CI).
async fn within<T>(f: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(10), f)
        .await
        .expect("writer did not respond within 10 s")
}

fn data(len: usize, byte: u8) -> Vec<u8> {
    vec![byte; len]
}

#[test]
fn writes_land_immediately_commits_wait() {
    let (fx, mut w) = Fixture::new(Some(100), NO_AUTO);
    w.define(0, 0..100).unwrap();
    w.write(0, 0, &data(40, 7)).unwrap();
    assert_eq!(&fx.part_bytes()[..40], &data(40, 7)[..]);
    assert!(
        fx.committed_on_disk().is_empty(),
        "nothing committed before commit()"
    );
    w.commit().unwrap();
    assert_eq!(fx.committed_on_disk().ranges(), [0..40]);
}

#[test]
fn commit_syncs_part_before_journaling() {
    let (_fx, mut w) = Fixture::new(Some(100), NO_AUTO);
    w.define(0, 0..100).unwrap();
    w.write(0, 0, &data(10, 1)).unwrap();
    w.write(0, 10, &data(10, 1)).unwrap();
    w.commit().unwrap();
    assert_eq!(
        w.ops,
        [
            Op::Write,
            Op::Write,
            Op::SyncPart,
            Op::AppendCommits(vec![0..20]),
            Op::SyncJournal
        ]
    );
}

#[test]
fn empty_commit_does_nothing() {
    let (_fx, mut w) = Fixture::new(Some(100), NO_AUTO);
    w.define(0, 0..100).unwrap();
    w.commit().unwrap();
    assert!(w.ops.is_empty());
}

#[test]
fn commits_itself_at_the_byte_threshold() {
    let config = WriterConfig {
        commit_bytes: 1000,
        ..NO_AUTO
    };
    let (fx, mut w) = Fixture::new(Some(5000), config);
    w.define(0, 0..5000).unwrap();
    w.write(0, 0, &data(999, 1)).unwrap();
    assert!(fx.committed_on_disk().is_empty());
    w.write(0, 999, &data(1, 1)).unwrap();
    assert_eq!(fx.committed_on_disk().ranges(), [0..1000]);
}

#[test]
fn deadline_exists_only_while_bytes_are_pending() {
    let config = WriterConfig {
        commit_interval: Duration::from_secs(2),
        ..NO_AUTO
    };
    let (_fx, mut w) = Fixture::new(Some(100), config);
    w.define(0, 0..100).unwrap();
    assert_eq!(w.commit_deadline(), None);
    let before = Instant::now();
    w.write(0, 0, &data(5, 1)).unwrap();
    let deadline = w.commit_deadline().unwrap();
    assert!(deadline >= before + Duration::from_secs(2));
    assert!(deadline <= Instant::now() + Duration::from_secs(2));
    // A later write doesn't push the deadline out.
    w.write(0, 5, &data(5, 1)).unwrap();
    assert_eq!(w.commit_deadline(), Some(deadline));
    w.commit().unwrap();
    assert_eq!(w.commit_deadline(), None);
}

#[test]
fn interleaved_chunks_commit_separately() {
    let (fx, mut w) = Fixture::new(Some(300), NO_AUTO);
    w.define(0, 0..100).unwrap();
    w.define(1, 100..300).unwrap();
    w.write(1, 100, &data(50, 2)).unwrap();
    w.write(0, 0, &data(30, 1)).unwrap();
    w.write(1, 150, &data(50, 2)).unwrap();
    w.commit().unwrap();
    assert_eq!(fx.committed_on_disk().ranges(), [0..30, 100..200]);
    let bytes = fx.part_bytes();
    assert_eq!(&bytes[0..30], &data(30, 1)[..]);
    assert_eq!(&bytes[100..200], &data(100, 2)[..]);
    assert_eq!(
        &bytes[30..100],
        &data(70, 0)[..],
        "untouched gap stays zero"
    );
}

#[test]
fn writes_outside_the_chunk_are_refused() {
    let (fx, mut w) = Fixture::new(Some(100), NO_AUTO);
    w.define(0, 0..50).unwrap();
    w.write(0, 0, &data(10, 1)).unwrap();
    // Past the chunk end.
    assert!(matches!(
        w.write(0, 10, &data(41, 9)),
        Err(WriterError::OutOfBounds {
            pos: 10,
            end: 50,
            ..
        })
    ));
    // Not at the chunk's position (gap, or rewind into written bytes).
    assert!(matches!(
        w.write(0, 11, &data(1, 9)),
        Err(WriterError::OutOfBounds { .. })
    ));
    assert!(matches!(
        w.write(0, 5, &data(1, 9)),
        Err(WriterError::OutOfBounds { .. })
    ));
    assert!(matches!(
        w.write(7, 0, &data(1, 9)),
        Err(WriterError::UnknownChunk { chunk: 7 })
    ));
    // Nothing from the refused writes landed.
    assert_eq!(
        &fx.part_bytes()[..60],
        &[data(10, 1), data(50, 0)].concat()[..]
    );
}

#[test]
fn chunks_cannot_overlap_data_or_each_other() {
    let (_fx, mut w) = Fixture::new(Some(100), NO_AUTO);
    w.define(0, 0..50).unwrap();
    w.write(0, 0, &data(50, 1)).unwrap();
    w.commit().unwrap();
    let bad = |w: &mut Writer, id, r: Range<u64>| {
        matches!(w.define(id, r), Err(WriterError::BadChunk { .. }))
    };
    assert!(
        bad(&mut w, 1, 40..60),
        "overlaps committed bytes and chunk 0"
    );
    assert!(bad(&mut w, 1, 60..101), "past total");
    assert!(bad(&mut w, 1, 70..70), "empty");
    assert!(bad(&mut w, 0, 60..70), "duplicate id");
    w.define(1, 50..100).unwrap();
    assert!(bad(&mut w, 2, 90..95), "overlaps chunk 1");
}

#[test]
fn resumed_writer_cannot_touch_committed_bytes() {
    let (fx, mut w) = Fixture::new(Some(100), NO_AUTO);
    w.define(0, 0..100).unwrap();
    w.write(0, 0, &data(60, 1)).unwrap();
    w.commit().unwrap();
    drop(w);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&fx.part)
        .unwrap();
    let mut w = Writer::new(file, Journal::open(&fx.sidecar).unwrap(), NO_AUTO);
    assert_eq!(w.committed().ranges(), [0..60]);
    assert!(w.define(0, 0..100).is_err());
    w.define(0, 60..100).unwrap();
}

#[test]
fn finish_commits_and_checkpoints() {
    let (fx, mut w) = Fixture::new(Some(20), NO_AUTO);
    w.define(0, 0..20).unwrap();
    w.write(0, 0, &data(20, 3)).unwrap();
    let committed = w.finish().unwrap();
    assert!(committed.is_exactly(20));
    let bytes = std::fs::read(&fx.sidecar).unwrap();
    let last: &[u8; format::RECORD_LEN] = bytes[bytes.len() - format::RECORD_LEN..]
        .try_into()
        .unwrap();
    assert_eq!(
        format::decode_record(last, Some(20)),
        Some(Record::Checkpoint)
    );
}

#[test]
fn blind_mode_has_no_bound() {
    let (fx, mut w) = Fixture::new(None, NO_AUTO);
    w.define(0, 0..u64::MAX).unwrap();
    w.write(0, 0, &data(10, 4)).unwrap();
    w.write(0, 10, &data(10, 4)).unwrap();
    w.commit().unwrap();
    assert_eq!(fx.committed_on_disk().ranges(), [0..20]);
    assert_eq!(fx.part_bytes(), data(20, 4));
}

#[test]
fn bound_clamps_to_chunk_and_flags_overflow() {
    assert_eq!(bound(0, 100, Some(1000), 64), (64, Overflow::None));
    assert_eq!(bound(90, 100, Some(1000), 64), (10, Overflow::PastChunk));
    assert_eq!(bound(990, 1000, Some(1000), 64), (10, Overflow::PastTotal));
    assert_eq!(bound(100, 100, Some(1000), 1), (0, Overflow::PastChunk));
    assert_eq!(bound(0, u64::MAX, None, 64), (64, Overflow::None));
}

#[test]
fn part_is_sized_and_preallocated() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("p.part");
    let (file, preallocated) = create_part(&path, Some(10 * 1024 * 1024)).unwrap();
    assert_eq!(file.metadata().unwrap().len(), 10 * 1024 * 1024);
    if preallocated {
        assert!(file.allocated_size().unwrap() >= 10 * 1024 * 1024);
    }
    assert!(
        create_part(&path, Some(1)).is_err(),
        "never reuses an existing file"
    );
}

#[test]
fn unknown_or_zero_size_parts_start_empty() {
    let dir = tempfile::tempdir().unwrap();
    for (name, total) in [("a", None), ("b", Some(0))] {
        let (file, preallocated) = create_part(&dir.path().join(name), total).unwrap();
        assert_eq!(file.metadata().unwrap().len(), 0);
        assert!(!preallocated);
    }
}

#[test]
fn no_space_fails_fast_and_leaves_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("big.part");
    let mib = 1024 * 1024;
    let result = create_part_with(&path, Some(10 * mib), |_| Ok(mib));
    assert!(matches!(
        result,
        Err(WriterError::NoSpace { needed, available }) if needed == 10 * mib && available == mib
    ));
    assert!(!path.exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn actor_writes_commits_and_closes() {
    let (fx, w) = Fixture::new(Some(1000), NO_AUTO);
    let (handle, done) = actor::spawn(w).unwrap();
    handle.define(0, 0..1000).await.unwrap();
    for i in 0..10u64 {
        handle
            .write(0, i * 100, Bytes::from(data(100, i as u8)))
            .await
            .unwrap();
    }
    let committed = within(handle.commit()).await.unwrap();
    assert!(committed.is_exactly(1000));
    drop(handle);
    assert!(matches!(within(done).await.unwrap(), Ok(Outcome::Closed(c)) if c.is_exactly(1000)));
    let bytes = fx.part_bytes();
    for i in 0..10usize {
        assert_eq!(&bytes[i * 100..(i + 1) * 100], &data(100, i as u8)[..]);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn actor_commits_on_the_timer() {
    let config = WriterConfig {
        commit_interval: Duration::from_millis(50),
        ..NO_AUTO
    };
    let (fx, w) = Fixture::new(Some(100), config);
    let (handle, _done) = actor::spawn(w).unwrap();
    handle.define(0, 0..100).await.unwrap();
    handle.write(0, 0, Bytes::from(data(10, 1))).await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while fx.committed_on_disk().is_empty() {
        assert!(Instant::now() < deadline, "timed commit never happened");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(fx.committed_on_disk().ranges(), [0..10]);
}

#[tokio::test(flavor = "multi_thread")]
async fn actor_dropped_handles_still_commit() {
    let (fx, w) = Fixture::new(Some(100), NO_AUTO);
    let (handle, done) = actor::spawn(w).unwrap();
    handle.define(0, 0..100).await.unwrap();
    handle.write(0, 0, Bytes::from(data(30, 1))).await.unwrap();
    drop(handle); // pause/cancel: no explicit commit
    within(done).await.unwrap().unwrap();
    assert_eq!(fx.committed_on_disk().ranges(), [0..30]);
}

#[tokio::test(flavor = "multi_thread")]
async fn actor_finish_reports_finished() {
    let (_fx, w) = Fixture::new(Some(10), NO_AUTO);
    let (handle, done) = actor::spawn(w).unwrap();
    handle.define(0, 0..10).await.unwrap();
    handle.write(0, 0, Bytes::from(data(10, 1))).await.unwrap();
    assert!(within(handle.finish()).await.unwrap().is_exactly(10));
    assert!(matches!(within(done).await.unwrap(), Ok(Outcome::Finished(c)) if c.is_exactly(10)));
}

#[tokio::test(flavor = "multi_thread")]
async fn actor_error_is_fatal_and_reported() {
    let (fx, w) = Fixture::new(Some(100), NO_AUTO);
    let (handle, done) = actor::spawn(w).unwrap();
    handle.define(0, 0..50).await.unwrap();
    handle.write(0, 0, Bytes::from(data(10, 1))).await.unwrap();
    handle.write(0, 10, Bytes::from(data(41, 9))).await.unwrap(); // past chunk end
    assert!(matches!(
        within(done).await.unwrap(),
        Err(WriterError::OutOfBounds { .. })
    ));
    assert!(within(handle.commit()).await.is_err(), "writer is gone");
    assert!(
        fx.committed_on_disk().is_empty(),
        "a failed writer commits nothing more"
    );
}

#[test]
fn split_journals_before_returning_and_bounds_the_owner() {
    let (fx, mut w) = Fixture::new(Some(1000), NO_AUTO);
    w.define(0, 0..1000).unwrap();
    w.write(0, 0, &data(100, 1)).unwrap();
    assert_eq!(w.split(0, 1, 600).unwrap(), Some(600..1000));
    let replayed = format::replay(&std::fs::read(&fx.sidecar).unwrap()).unwrap();
    assert_eq!(
        replayed.valid_len as usize,
        format::HEADER_LEN + 3 * format::RECORD_LEN,
        "define, split, define"
    );
    // The owner's in-flight bytes past 600 are dropped, the rest kept.
    w.write(0, 100, &data(550, 2)).unwrap();
    w.write(0, 650, &data(50, 2)).unwrap();
    w.write(1, 600, &data(400, 3)).unwrap();
    w.commit().unwrap();
    assert!(fx.committed_on_disk().is_exactly(1000));
    let bytes = fx.part_bytes();
    assert_eq!(&bytes[100..600], &data(500, 2)[..]);
    assert_eq!(
        &bytes[600..1000],
        &data(400, 3)[..],
        "the new chunk's bytes, not the owner's"
    );
}

#[test]
fn split_refuses_points_outside_the_unwritten_part() {
    let (_fx, mut w) = Fixture::new(Some(1000), NO_AUTO);
    w.define(0, 0..1000).unwrap();
    w.write(0, 0, &data(300, 1)).unwrap();
    assert_eq!(w.split(0, 1, 299).unwrap(), None, "already written");
    assert_eq!(w.split(0, 1, 1000).unwrap(), None, "nothing left to take");
    assert!(matches!(
        w.split(9, 1, 500),
        Err(WriterError::UnknownChunk { .. })
    ));
    assert!(matches!(
        w.split(0, 0, 500),
        Err(WriterError::BadChunk { .. })
    ));
}

#[test]
fn unsplit_chunks_stay_strict() {
    let (_fx, mut w) = Fixture::new(Some(1000), NO_AUTO);
    w.define(0, 0..100).unwrap();
    assert!(matches!(
        w.write(0, 0, &data(101, 1)),
        Err(WriterError::OutOfBounds { .. })
    ));
}

#[test]
fn merge_undoes_a_split_before_anything_was_dropped() {
    let (_fx, mut w) = Fixture::new(Some(1000), NO_AUTO);
    w.define(0, 0..1000).unwrap();
    w.write(0, 0, &data(100, 1)).unwrap();
    w.split(0, 1, 500).unwrap().unwrap();
    assert!(w.merge(0, 1).unwrap());
    w.write(0, 100, &data(900, 1)).unwrap();
    w.commit().unwrap();
    assert!(w.committed().is_exactly(1000));
}

#[test]
fn merge_refuses_after_drops_or_writes() {
    let (_fx, mut w) = Fixture::new(Some(1000), NO_AUTO);
    w.define(0, 0..1000).unwrap();
    w.split(0, 1, 500).unwrap().unwrap();
    w.write(1, 500, &data(10, 2)).unwrap();
    assert!(!w.merge(0, 1).unwrap(), "the new chunk already has bytes");
    w.split(1, 2, 800).unwrap().unwrap();
    w.write(1, 510, &data(400, 2)).unwrap(); // crosses 800: tail dropped
    assert!(!w.merge(1, 2).unwrap(), "owner already dropped bytes");
}
