//! One process runs a download at a time (ARCHITECTURE §2). The runner
//! holds an exclusive lock on `<data>/locks/<id>.lock`; the lock dies with
//! the process (also on `kill -9`), so a crashed run never blocks a resume.
//! A CLI runner writes its PID in it so other `dhow`s can signal it; an
//! in-process shell writes nothing (PID 0): it handles no signals.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub struct RunLock {
    _file: File,
}

pub enum Held {
    /// By this PID; 0: a process that takes no signals.
    By(u32),
    Io(io::Error),
}

impl From<io::Error> for Held {
    fn from(e: io::Error) -> Self {
        Held::Io(e)
    }
}

fn path(data_dir: &Path, id: &str) -> PathBuf {
    data_dir.join("locks").join(format!("{id}.lock"))
}

fn open(data_dir: &Path, id: &str) -> io::Result<File> {
    let p = path(data_dir, id);
    std::fs::create_dir_all(p.parent().expect("has a parent"))?;
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(p)
}

pub fn read_pid(file: &mut File) -> u32 {
    let mut s = String::new();
    let _ = file.seek(SeekFrom::Start(0));
    let _ = file.read_to_string(&mut s);
    s.trim().parse().unwrap_or(0)
}

/// Takes the lock, recording `pid` (`None`: don't signal this process).
pub fn acquire(data_dir: &Path, id: &str, pid: Option<u32>) -> Result<RunLock, Held> {
    let mut file = open(data_dir, id)?;
    match file.try_lock() {
        Ok(()) => {
            file.set_len(0)?;
            if let Some(pid) = pid {
                file.seek(SeekFrom::Start(0))?;
                write!(file, "{pid}")?;
                file.flush()?;
            }
            Ok(RunLock { _file: file })
        }
        Err(TryLockError::WouldBlock) => Err(Held::By(read_pid(&mut file))),
        Err(TryLockError::Error(e)) => Err(Held::Io(e)),
    }
}

/// The PID running `id`, if any process is (0: one that takes no signals).
/// Probes with a shared lock, so probes don't collide with each other.
pub fn holder(data_dir: &Path, id: &str) -> io::Result<Option<u32>> {
    let mut file = open(data_dir, id)?;
    match file.try_lock_shared() {
        Ok(()) => Ok(None),
        Err(TryLockError::WouldBlock) => Ok(Some(read_pid(&mut file))),
        Err(TryLockError::Error(e)) => Err(e),
    }
}
