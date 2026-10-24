//! The sidecar journal (ENGINE-SPEC §5): the authoritative record of which
//! bytes of a part file are durable. Single owner; the writer actor (§6 I9)
//! holds it.
//!
//! Durability is the caller's call: [`Journal::append`] writes, and
//! [`Journal::sync`] makes it durable. A commit record must only be appended
//! after the part file's own `sync_data` returned (I2). `sync_data` is
//! `F_FULLFSYNC` on Apple, `fdatasync` on Linux/Android and
//! `FlushFileBuffers` on Windows.

pub mod format;

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub use format::{Header, HeaderError, Record};

use crate::rangeset::RangeSet;

/// Compact when the sidecar grows past this (§5.3).
pub const COMPACT_AT: u64 = 64 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    #[error(transparent)]
    Io(#[from] io::Error),
    /// Unusable sidecar: treat the download as having none (§5.4).
    #[error(transparent)]
    Header(#[from] HeaderError),
}

pub struct Journal {
    path: PathBuf,
    file: File,
    header: Header,
    committed: RangeSet,
    len: u64,
}

impl Journal {
    /// Creates a new sidecar at `path` (which must not exist) and makes its
    /// header and directory entry durable.
    pub fn create(path: &Path, header: Header) -> io::Result<Self> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)?;
        file.write_all(&format::encode_header(&header))?;
        file.sync_data()?;
        sync_dir(path)?;
        Ok(Self {
            path: path.to_owned(),
            file,
            header,
            committed: RangeSet::new(),
            len: format::HEADER_LEN as u64,
        })
    }

    /// Opens and replays an existing sidecar. A torn or corrupt tail is cut
    /// off (and that made durable) so new records never land behind garbage
    /// that would hide them from the next replay. A leftover compaction temp
    /// file is removed.
    pub fn open(path: &Path) -> Result<Self, JournalError> {
        remove_if_exists(&tmp_path(path))?;
        let mut file = OpenOptions::new().read(true).write(true).open(path)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let replayed = format::replay(&bytes)?;
        if replayed.valid_len < bytes.len() as u64 {
            tracing::warn!(
                discarded = bytes.len() as u64 - replayed.valid_len,
                "journal: discarding torn or corrupt tail"
            );
            file.set_len(replayed.valid_len)?;
            file.sync_data()?;
        }
        file.seek(SeekFrom::Start(replayed.valid_len))?;
        Ok(Self {
            path: path.to_owned(),
            file,
            header: replayed.header,
            committed: replayed.committed,
            len: replayed.valid_len,
        })
    }

    pub fn header(&self) -> &Header {
        &self.header
    }

    pub fn committed(&self) -> &RangeSet {
        &self.committed
    }

    pub fn byte_len(&self) -> u64 {
        self.len
    }

    /// Writes `records` in one write, not yet durable. Records that would
    /// fail replay's checks are refused up front, so nothing the writer
    /// appends can silently truncate the journal on the next open.
    pub fn append(&mut self, records: &[Record]) -> io::Result<()> {
        let mut buf = Vec::with_capacity(records.len() * format::RECORD_LEN);
        for r in records {
            let encoded = format::encode_record(r);
            if format::decode_record(&encoded, self.header.total_size).is_none() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("journal: refusing invalid record {r:?}"),
                ));
            }
            buf.extend_from_slice(&encoded);
        }
        self.file.write_all(&buf)?;
        self.len += buf.len() as u64;
        for r in records {
            if let Record::Commit(range) = r {
                self.committed.insert(range.clone());
            }
        }
        Ok(())
    }

    pub fn sync(&mut self) -> io::Result<()> {
        self.file.sync_data()
    }

    /// Compacts if the sidecar is past [`COMPACT_AT`]. Returns whether it did.
    pub fn compact_if_large(&mut self) -> io::Result<bool> {
        if self.len <= COMPACT_AT {
            return Ok(false);
        }
        self.compact()?;
        Ok(true)
    }

    /// Rewrites the sidecar as header + one commit per committed interval +
    /// checkpoint (§5.3): temp file, sync, atomic rename, sync directory.
    /// A crash at any point leaves either the old or the new sidecar.
    pub fn compact(&mut self) -> io::Result<()> {
        let tmp = tmp_path(&self.path);
        let mut records: Vec<Record> = self
            .committed
            .ranges()
            .iter()
            .cloned()
            .map(Record::Commit)
            .collect();
        records.push(Record::Checkpoint);
        let mut bytes = format::encode_header(&self.header).to_vec();
        for r in &records {
            bytes.extend_from_slice(&format::encode_record(r));
        }
        let mut new = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)?;
        new.write_all(&bytes)?;
        new.sync_data()?;
        // Close the old handle first: Windows won't replace an open file.
        let old = std::mem::replace(&mut self.file, new);
        drop(old);
        if let Err(e) = std::fs::rename(&tmp, &self.path) {
            self.file = OpenOptions::new().read(true).write(true).open(&self.path)?;
            self.file.seek(SeekFrom::Start(self.len))?;
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        sync_dir(&self.path)?;
        tracing::debug!(from = self.len, to = bytes.len(), "journal: compacted");
        self.len = bytes.len() as u64;
        Ok(())
    }
}

/// `<sidecar>.tmp`, i.e. `<final>.dhow.tmp`.
fn tmp_path(path: &Path) -> PathBuf {
    let mut p = path.as_os_str().to_owned();
    p.push(".tmp");
    p.into()
}

fn remove_if_exists(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// Makes a directory entry change (create, rename) durable. POSIX only;
/// Windows has no directory handles to sync and NTFS journals metadata.
fn sync_dir(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        let dir = match path.parent() {
            Some(d) if !d.as_os_str().is_empty() => d,
            _ => Path::new("."),
        };
        File::open(dir)?.sync_all()?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests;
