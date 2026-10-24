//! The part-file writer (ENGINE-SPEC §6 I2, I6, I9, I10; §7 batching).
//!
//! [`Writer`] is the synchronous core: it owns the part file and the journal
//! and is the only thing that writes either. [`actor`] runs one on its own
//! thread so connection tasks only ever send `(chunk, offset, bytes)`.

pub mod actor;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io;
use std::ops::Range;
use std::path::Path;
use std::time::{Duration, Instant};

use fs4::FileExt;

use crate::journal::{Journal, Record};
use crate::rangeset::RangeSet;

pub type ChunkId = u32;

#[derive(Clone, Copy, Debug)]
pub struct WriterConfig {
    /// Commit at most this long after the first uncommitted write.
    pub commit_interval: Duration,
    /// Commit when one chunk has this many uncommitted bytes.
    pub commit_bytes: u64,
}

impl Default for WriterConfig {
    fn default() -> Self {
        Self {
            commit_interval: Duration::from_secs(2),
            commit_bytes: 8 * 1024 * 1024,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum WriterError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("not enough space: {needed} bytes needed, {available} available")]
    NoSpace { needed: u64, available: u64 },
    #[error("chunk {chunk} is not defined")]
    UnknownChunk { chunk: ChunkId },
    #[error("chunk {chunk} range {range:?} is empty, out of bounds or overlaps existing data")]
    BadChunk { chunk: ChunkId, range: Range<u64> },
    /// A connection tried to write outside its chunk's `[pos, end)` (I6).
    /// Always a bug upstream; the write is refused, nothing lands.
    #[error("chunk {chunk}: write of {len} bytes at {offset} is outside [{pos}, {end})")]
    OutOfBounds {
        chunk: ChunkId,
        offset: u64,
        len: usize,
        pos: u64,
        end: u64,
    },
}

/// What happens to bytes a connection received (I6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Overflow {
    /// Everything fits in the chunk.
    None,
    /// Bytes past the chunk's end: drop them and close the connection.
    PastChunk,
    /// Bytes past `total_size`: the download fails (`ServerSentExtraData`).
    PastTotal,
}

/// How many of `len` received bytes at `pos` may be written to a chunk
/// ending at `end`, and what to do about the rest.
pub fn bound(pos: u64, end: u64, total: Option<u64>, len: usize) -> (usize, Overflow) {
    let room = end.saturating_sub(pos);
    if len as u64 <= room {
        return (len, Overflow::None);
    }
    let past_total = total.is_some_and(|t| pos + len as u64 > t);
    let overflow = if past_total {
        Overflow::PastTotal
    } else {
        Overflow::PastChunk
    };
    (room as usize, overflow)
}

/// Creates the part file (`create_new`), sized to `total`. Preallocates when
/// the filesystem supports it, else extends sparsely. Fails fast with
/// [`WriterError::NoSpace`] before writing anything (I10), and leaves no
/// file behind on failure. Returns the file and whether it was preallocated.
pub fn create_part(path: &Path, total: Option<u64>) -> Result<(File, bool), WriterError> {
    create_part_with(path, total, |dir| fs4::available_space(dir))
}

fn create_part_with(
    path: &Path,
    total: Option<u64>,
    available: impl Fn(&Path) -> io::Result<u64>,
) -> Result<(File, bool), WriterError> {
    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    if let Some(needed) = total {
        let available = available(dir)?;
        if available < needed {
            return Err(WriterError::NoSpace { needed, available });
        }
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(path)?;
    let sized = (|| -> Result<bool, WriterError> {
        let Some(total) = total.filter(|&t| t > 0) else {
            return Ok(false);
        };
        let preallocated = match file.allocate(total) {
            Ok(()) => true,
            Err(e) if e.kind() == io::ErrorKind::StorageFull => {
                let available = available(dir).unwrap_or(0);
                return Err(WriterError::NoSpace {
                    needed: total,
                    available,
                });
            }
            Err(e) => {
                tracing::warn!(error = %e, "writer: preallocation unsupported, using sparse file");
                false
            }
        };
        if file.metadata()?.len() != total {
            file.set_len(total)?;
        }
        file.sync_all()?;
        Ok(preallocated)
    })();
    match sized {
        Ok(preallocated) => Ok((file, preallocated)),
        Err(e) => {
            drop(file);
            let _ = std::fs::remove_file(path);
            Err(e)
        }
    }
}

struct Chunk {
    start: u64,
    end: u64,
    /// Next byte expected; everything in `[start, written)` is on the page cache.
    written: u64,
    /// Everything in `[start, committed)` is durable and journaled.
    committed: u64,
}

#[cfg(test)]
#[derive(Clone, Debug, PartialEq, Eq)]
enum Op {
    Write,
    SyncPart,
    AppendCommits(Vec<Range<u64>>),
    SyncJournal,
    Checkpoint,
}

pub struct Writer {
    part: File,
    journal: Journal,
    config: WriterConfig,
    chunks: HashMap<ChunkId, Chunk>,
    /// When the oldest uncommitted write happened; `None` when all is committed.
    pending_since: Option<Instant>,
    #[cfg(test)]
    ops: Vec<Op>,
}

impl Writer {
    pub fn new(part: File, journal: Journal, config: WriterConfig) -> Self {
        Self {
            part,
            journal,
            config,
            chunks: HashMap::new(),
            pending_since: None,
            #[cfg(test)]
            ops: Vec::new(),
        }
    }

    pub fn committed(&self) -> &RangeSet {
        self.journal.committed()
    }

    pub fn total_size(&self) -> Option<u64> {
        self.journal.header().total_size
    }

    /// Registers chunk `id` for `range`. The range may not touch committed
    /// bytes or another chunk, so nothing can overwrite durable data.
    pub fn define(&mut self, id: ChunkId, range: Range<u64>) -> Result<(), WriterError> {
        let in_bounds = self.total_size().is_none_or(|t| range.end <= t);
        let overlaps = self.committed().intersects(&range)
            || self
                .chunks
                .values()
                .any(|c| c.start < range.end && range.start < c.end);
        if range.start >= range.end || !in_bounds || overlaps || self.chunks.contains_key(&id) {
            return Err(WriterError::BadChunk { chunk: id, range });
        }
        self.journal.append(&[Record::Define(range.clone())])?;
        self.chunks.insert(
            id,
            Chunk {
                start: range.start,
                end: range.end,
                written: range.start,
                committed: range.start,
            },
        );
        Ok(())
    }

    /// Writes `data` at `offset`, which must be exactly where chunk `id` is
    /// and must fit before its end (I6). Not durable until [`Self::commit`];
    /// commits by itself once the chunk has `commit_bytes` pending.
    pub fn write(&mut self, id: ChunkId, offset: u64, data: &[u8]) -> Result<(), WriterError> {
        let chunk = self
            .chunks
            .get(&id)
            .ok_or(WriterError::UnknownChunk { chunk: id })?;
        if offset != chunk.written || offset + data.len() as u64 > chunk.end {
            return Err(WriterError::OutOfBounds {
                chunk: id,
                offset,
                len: data.len(),
                pos: chunk.written,
                end: chunk.end,
            });
        }
        if data.is_empty() {
            return Ok(());
        }
        write_at(&self.part, offset, data)?;
        #[cfg(test)]
        self.ops.push(Op::Write);
        let chunk = self.chunks.get_mut(&id).expect("checked above");
        chunk.written += data.len() as u64;
        let pending = chunk.written - chunk.committed;
        self.pending_since.get_or_insert_with(Instant::now);
        if pending >= self.config.commit_bytes {
            self.commit()?;
        }
        Ok(())
    }

    /// When the next timed commit is due; `None` when nothing is pending, so
    /// an idle writer has no deadline (and its thread no timer).
    pub fn commit_deadline(&self) -> Option<Instant> {
        self.pending_since.map(|t| t + self.config.commit_interval)
    }

    /// Makes every written byte durable, then journals it (I2): part
    /// `sync_data` returns before any commit record is appended.
    pub fn commit(&mut self) -> Result<(), WriterError> {
        let pending: Vec<Range<u64>> = self
            .chunks
            .values()
            .filter(|c| c.written > c.committed)
            .map(|c| c.committed..c.written)
            .collect();
        if pending.is_empty() {
            self.pending_since = None;
            return Ok(());
        }
        self.part.sync_data()?;
        #[cfg(test)]
        self.ops.push(Op::SyncPart);
        let records: Vec<Record> = pending.iter().cloned().map(Record::Commit).collect();
        self.journal.append(&records)?;
        #[cfg(test)]
        self.ops.push(Op::AppendCommits(pending));
        self.journal.sync()?;
        #[cfg(test)]
        self.ops.push(Op::SyncJournal);
        for c in self.chunks.values_mut() {
            c.committed = c.written;
        }
        self.pending_since = None;
        self.journal.compact_if_large()?;
        Ok(())
    }

    /// Final commit plus a checkpoint (§8 step 2). Returns the committed set.
    pub fn finish(mut self) -> Result<RangeSet, WriterError> {
        self.commit()?;
        self.journal.append(&[Record::Checkpoint])?;
        #[cfg(test)]
        self.ops.push(Op::Checkpoint);
        self.journal.sync()?;
        Ok(self.journal.committed().clone())
    }
}

/// Reads exactly `len` bytes at `offset`. For the coordinator, before a
/// writer owns the file (I9: reading is not writing).
pub fn read_at(file: &File, offset: u64, len: usize) -> io::Result<Vec<u8>> {
    let mut buf = vec![0; len];
    #[cfg(unix)]
    std::os::unix::fs::FileExt::read_exact_at(file, &mut buf, offset)?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        let mut done = 0;
        while done < len {
            let n = file.seek_read(&mut buf[done..], offset + done as u64)?;
            if n == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            done += n;
        }
    }
    Ok(buf)
}

#[cfg(unix)]
fn write_at(file: &File, offset: u64, data: &[u8]) -> io::Result<()> {
    std::os::unix::fs::FileExt::write_all_at(file, data, offset)
}

#[cfg(windows)]
fn write_at(file: &File, mut offset: u64, mut data: &[u8]) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !data.is_empty() {
        let n = file.seek_write(data, offset)?;
        if n == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        data = &data[n..];
        offset += n as u64;
    }
    Ok(())
}
