#![allow(clippy::single_range_in_vec_init, clippy::reversed_empty_ranges)]

use std::fs::OpenOptions;

use super::*;

fn header(total: Option<u64>) -> Header {
    Header {
        preallocated: total.is_some(),
        total_size: total,
        url_hash: format::url_hash("https://example.com/f.bin"),
        etag: Some("\"v1\"".into()),
        last_modified: None,
        created_at: 1,
    }
}

fn fresh(total: Option<u64>) -> (tempfile::TempDir, PathBuf, Journal) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.bin.dhow");
    let j = Journal::create(&path, header(total)).unwrap();
    (dir, path, j)
}

#[test]
fn create_then_open_is_empty_with_same_header() {
    let (_dir, path, j) = fresh(Some(1000));
    drop(j);
    let j = Journal::open(&path).unwrap();
    assert_eq!(j.header(), &header(Some(1000)));
    assert!(j.committed().is_empty());
    assert_eq!(j.byte_len(), format::HEADER_LEN as u64);
}

#[test]
fn create_refuses_existing_file() {
    let (_dir, path, _j) = fresh(None);
    let err = Journal::create(&path, header(None)).err().unwrap();
    assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
}

#[test]
fn commits_survive_reopen() {
    let (_dir, path, mut j) = fresh(Some(1000));
    j.append(&[Record::Define(0..1000), Record::Commit(0..100)])
        .unwrap();
    j.append(&[Record::Commit(100..250), Record::Commit(600..700)])
        .unwrap();
    assert_eq!(j.committed().ranges(), [0..250, 600..700]);
    drop(j); // no sync: a kill -9 keeps page-cache writes (T-I2)
    let j = Journal::open(&path).unwrap();
    assert_eq!(j.committed().ranges(), [0..250, 600..700]);
}

#[test]
fn torn_tail_is_cut_so_later_records_are_not_hidden() {
    let (_dir, path, mut j) = fresh(Some(1000));
    j.append(&[Record::Commit(0..100), Record::Commit(200..300)])
        .unwrap();
    drop(j);
    // Tear the last record in half.
    let len = std::fs::metadata(&path).unwrap().len();
    OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(len - 10)
        .unwrap();

    let mut j = Journal::open(&path).unwrap();
    assert_eq!(j.committed().ranges(), [0..100]);
    assert_eq!(std::fs::metadata(&path).unwrap().len(), j.byte_len());
    j.append(&[Record::Commit(500..600)]).unwrap();
    drop(j);
    let j = Journal::open(&path).unwrap();
    assert_eq!(j.committed().ranges(), [0..100, 500..600]);
}

#[test]
fn corrupt_record_hides_everything_after_it() {
    let (_dir, path, mut j) = fresh(Some(1000));
    j.append(&[
        Record::Commit(0..10),
        Record::Commit(20..30),
        Record::Commit(40..50),
    ])
    .unwrap();
    drop(j);
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[format::HEADER_LEN + format::RECORD_LEN + 5] ^= 0xff; // second record
    std::fs::write(&path, bytes).unwrap();
    let j = Journal::open(&path).unwrap();
    assert_eq!(j.committed().ranges(), [0..10]);
}

#[test]
fn bad_header_is_reported_not_guessed() {
    let (_dir, path, j) = fresh(Some(1000));
    drop(j);
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[10] ^= 1;
    std::fs::write(&path, &bytes).unwrap();
    assert!(matches!(
        Journal::open(&path),
        Err(JournalError::Header(HeaderError::Checksum))
    ));
    std::fs::write(&path, &bytes[..100]).unwrap();
    assert!(matches!(
        Journal::open(&path),
        Err(JournalError::Header(HeaderError::Short))
    ));
}

#[test]
fn missing_sidecar_is_not_found() {
    let dir = tempfile::tempdir().unwrap();
    match Journal::open(&dir.path().join("nope.dhow")) {
        Err(JournalError::Io(e)) => assert_eq!(e.kind(), io::ErrorKind::NotFound),
        other => panic!("{:?}", other.err()),
    }
}

#[test]
fn append_refuses_records_replay_would_reject() {
    let (_dir, path, mut j) = fresh(Some(1000));
    j.append(&[Record::Commit(0..10)]).unwrap();
    let before = j.byte_len();
    for bad in [
        Record::Commit(5..5),
        Record::Commit(900..1001),
        Record::Define(3..1),
    ] {
        let err = j.append(&[Record::Commit(20..30), bad]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }
    // Nothing from a refused batch was written or counted.
    assert_eq!(j.byte_len(), before);
    assert_eq!(j.committed().ranges(), [0..10]);
    drop(j);
    assert_eq!(std::fs::metadata(&path).unwrap().len(), before);
}

/// Disjoint 1-byte commits every 4 bytes: nothing merges, so the journal
/// grows a record per commit.
fn fill_past_threshold(j: &mut Journal) -> usize {
    let n = (COMPACT_AT as usize / format::RECORD_LEN) + 10;
    let records: Vec<Record> = (0..n as u64)
        .map(|i| Record::Commit(i * 4..i * 4 + 1))
        .collect();
    j.append(&records).unwrap();
    n
}

#[test]
fn compaction_shrinks_and_preserves_the_committed_set() {
    let (_dir, path, mut j) = fresh(None);
    assert!(!j.compact_if_large().unwrap());
    fill_past_threshold(&mut j);
    // Merge most of it so compaction has something to remove.
    j.append(&[Record::Commit(0..40_000)]).unwrap();
    let committed = j.committed().clone();
    assert!(j.compact_if_large().unwrap());
    let expect = format::HEADER_LEN + (committed.ranges().len() + 1) * format::RECORD_LEN;
    assert_eq!(j.byte_len() as usize, expect);
    assert_eq!(std::fs::metadata(&path).unwrap().len() as usize, expect);
    // The journal keeps working on the new file.
    j.append(&[Record::Commit(1_000_000..1_000_100)]).unwrap();
    drop(j);
    let j = Journal::open(&path).unwrap();
    let mut expect_set = committed;
    expect_set.insert(1_000_000..1_000_100);
    assert_eq!(j.committed(), &expect_set);
    assert!(!tmp_path(&path).exists());
}

#[test]
fn crash_during_compaction_leaves_old_sidecar_usable() {
    let (_dir, path, mut j) = fresh(None);
    j.append(&[Record::Commit(0..10)]).unwrap();
    drop(j);
    // A half-written temp file from a crash before the rename.
    std::fs::write(tmp_path(&path), b"DHOW partial").unwrap();
    let j = Journal::open(&path).unwrap();
    assert_eq!(j.committed().ranges(), [0..10]);
    assert!(!tmp_path(&path).exists(), "stale temp removed");
}
