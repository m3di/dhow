//! A [`Writer`] on its own thread (I9). Connection tasks hold a cheap
//! [`WriterHandle`] and send `(chunk, offset, bytes)`; only the thread
//! touches the files. The bounded queue is the backpressure: a slow disk
//! slows the sockets instead of growing memory.
//!
//! The thread waits with a deadline only while uncommitted bytes exist, and
//! exits when every handle is dropped, so a finished or paused download
//! leaves no thread and no timer behind.

use std::io;
use std::ops::Range;
use std::thread;

use bytes::Bytes;
use flume::RecvTimeoutError;
use tokio::sync::oneshot;

use super::{ChunkId, Writer, WriterError};
use crate::rangeset::RangeSet;

/// Messages in flight; at 64 KiB reads, ~4 MiB of buffered data at most.
const QUEUE: usize = 64;

enum Msg {
    Define {
        chunk: ChunkId,
        range: Range<u64>,
        reply: oneshot::Sender<()>,
    },
    Write {
        chunk: ChunkId,
        offset: u64,
        data: Bytes,
    },
    Commit {
        reply: oneshot::Sender<RangeSet>,
    },
    Split {
        owner: ChunkId,
        new: ChunkId,
        at: u64,
        reply: oneshot::Sender<Option<Range<u64>>>,
    },
    Merge {
        into: ChunkId,
        from: ChunkId,
        reply: oneshot::Sender<bool>,
    },
    Finish {
        reply: oneshot::Sender<RangeSet>,
    },
}

/// The writer thread has stopped. Why is in the `done` result of [`spawn`].
#[derive(Debug, thiserror::Error)]
#[error("writer stopped")]
pub struct WriterGone;

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// [`WriterHandle::finish`] ran: final commit and checkpoint.
    Finished(RangeSet),
    /// Every handle was dropped (pause, cancel, shutdown): final commit.
    Closed(RangeSet),
}

#[derive(Clone)]
pub struct WriterHandle {
    tx: flume::Sender<Msg>,
}

impl WriterHandle {
    /// Resolves once the chunk is registered.
    pub async fn define(&self, chunk: ChunkId, range: Range<u64>) -> Result<(), WriterGone> {
        let (reply, rx) = oneshot::channel();
        self.send(Msg::Define {
            chunk,
            range,
            reply,
        })
        .await?;
        rx.await.map_err(|_| WriterGone)
    }

    /// Queues a write. Waits only when the queue is full.
    pub async fn write(&self, chunk: ChunkId, offset: u64, data: Bytes) -> Result<(), WriterGone> {
        self.send(Msg::Write {
            chunk,
            offset,
            data,
        })
        .await
    }

    /// Commits everything queued before this call; returns the committed set.
    pub async fn commit(&self) -> Result<RangeSet, WriterGone> {
        let (reply, rx) = oneshot::channel();
        self.send(Msg::Commit { reply }).await?;
        rx.await.map_err(|_| WriterGone)
    }

    /// See [`Writer::split`]. Resolves once the split is journaled.
    pub async fn split(
        &self,
        owner: ChunkId,
        new: ChunkId,
        at: u64,
    ) -> Result<Option<Range<u64>>, WriterGone> {
        let (reply, rx) = oneshot::channel();
        self.send(Msg::Split {
            owner,
            new,
            at,
            reply,
        })
        .await?;
        rx.await.map_err(|_| WriterGone)
    }

    /// See [`Writer::merge`].
    pub async fn merge(&self, into: ChunkId, from: ChunkId) -> Result<bool, WriterGone> {
        let (reply, rx) = oneshot::channel();
        self.send(Msg::Merge { into, from, reply }).await?;
        rx.await.map_err(|_| WriterGone)
    }

    /// Final commit and checkpoint (§8 step 2); the writer then stops.
    pub async fn finish(self) -> Result<RangeSet, WriterGone> {
        let (reply, rx) = oneshot::channel();
        self.send(Msg::Finish { reply }).await?;
        rx.await.map_err(|_| WriterGone)
    }

    async fn send(&self, msg: Msg) -> Result<(), WriterGone> {
        self.tx.send_async(msg).await.map_err(|_| WriterGone)
    }
}

/// Starts `writer` on a dedicated thread. `done` resolves when it stops:
/// with its outcome, or with the error that stopped it (any error is fatal;
/// the download fails with it).
pub fn spawn(
    writer: Writer,
) -> io::Result<(
    WriterHandle,
    oneshot::Receiver<Result<Outcome, WriterError>>,
)> {
    let (tx, rx) = flume::bounded(QUEUE);
    let (done_tx, done) = oneshot::channel();
    thread::Builder::new()
        .name("dhow-writer".into())
        .spawn(move || {
            let result = run(writer, rx);
            if let Err(e) = &result {
                tracing::error!(error = %e, "writer stopped");
            }
            let _ = done_tx.send(result);
        })?;
    Ok((WriterHandle { tx }, done))
}

fn run(mut w: Writer, rx: flume::Receiver<Msg>) -> Result<Outcome, WriterError> {
    loop {
        let msg = match w.commit_deadline() {
            Some(deadline) => match rx.recv_deadline(deadline) {
                Ok(msg) => Some(msg),
                Err(RecvTimeoutError::Timeout) => {
                    w.commit()?;
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => None,
            },
            None => rx.recv().ok(),
        };
        let Some(msg) = msg else {
            w.commit()?;
            return Ok(Outcome::Closed(w.committed().clone()));
        };
        match msg {
            Msg::Define {
                chunk,
                range,
                reply,
            } => {
                w.define(chunk, range)?;
                let _ = reply.send(());
            }
            Msg::Write {
                chunk,
                offset,
                data,
            } => w.write(chunk, offset, &data)?,
            Msg::Split {
                owner,
                new,
                at,
                reply,
            } => {
                let _ = reply.send(w.split(owner, new, at)?);
            }
            Msg::Merge { into, from, reply } => {
                let _ = reply.send(w.merge(into, from)?);
            }
            Msg::Commit { reply } => {
                w.commit()?;
                let _ = reply.send(w.committed().clone());
            }
            Msg::Finish { reply } => {
                let committed = w.finish()?;
                let _ = reply.send(committed.clone());
                return Ok(Outcome::Finished(committed));
            }
        }
    }
}
