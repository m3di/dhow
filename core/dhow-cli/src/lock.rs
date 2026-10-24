//! One process runs a download at a time. The runner holds an exclusive
//! lock on `<data>/locks/<id>.lock` containing its PID; the lock dies with
//! the process (also on `kill -9`), so a crashed run never blocks a resume.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub struct RunLock {
    _file: File,
}

pub enum Held {
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

fn read_pid(file: &mut File) -> u32 {
    let mut s = String::new();
    let _ = file.seek(SeekFrom::Start(0));
    let _ = file.read_to_string(&mut s);
    s.trim().parse().unwrap_or(0)
}

pub fn acquire(data_dir: &Path, id: &str) -> Result<RunLock, Held> {
    let mut file = open(data_dir, id)?;
    match file.try_lock() {
        Ok(()) => {
            file.set_len(0)?;
            file.seek(SeekFrom::Start(0))?;
            write!(file, "{}", std::process::id())?;
            file.flush()?;
            Ok(RunLock { _file: file })
        }
        Err(TryLockError::WouldBlock) => Err(Held::By(read_pid(&mut file))),
        Err(TryLockError::Error(e)) => Err(Held::Io(e)),
    }
}

/// The PID running `id`, if any process is.
pub fn holder(data_dir: &Path, id: &str) -> io::Result<Option<u32>> {
    let mut file = open(data_dir, id)?;
    match file.try_lock() {
        Ok(()) => Ok(None),
        Err(TryLockError::WouldBlock) => Ok(Some(read_pid(&mut file))),
        Err(TryLockError::Error(e)) => Err(e),
    }
}

/// Wakes the process running a download so it re-reads that download's
/// state (the caller has already recorded a pause or cancel), and waits
/// until it has let go of it.
pub fn stop_runner(data_dir: &Path, id: &str, pid: u32) -> io::Result<()> {
    poke(pid)?;
    let deadline = Instant::now() + Duration::from_secs(60);
    while holder(data_dir, id)?.is_some() {
        if Instant::now() > deadline {
            return Err(io::Error::other(format!("process {pid} did not stop")));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

/// A running `dhow` that wants to hear about queue changes. Its lock in
/// `<data>/runners/` holds its PID; dropping it unregisters.
pub struct Runner {
    _file: File,
    path: PathBuf,
}

impl Drop for Runner {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub fn register_runner(data_dir: &Path) -> io::Result<Runner> {
    let dir = data_dir.join("runners");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.lock", std::process::id()));
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)?;
    file.try_lock()
        .map_err(|_| io::Error::other("runner lock taken"))?;
    write!(file, "{}", std::process::id())?;
    file.flush()?;
    Ok(Runner { _file: file, path })
}

/// Tells every registered runner to re-check queues and its downloads.
/// Best effort: a runner that is gone is cleaned up, errors are ignored.
pub fn poke_runners(data_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(data_dir.join("runners")) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(mut file) = OpenOptions::new().read(true).write(true).open(entry.path()) else {
            continue;
        };
        match file.try_lock() {
            // Nobody holds it: a runner that died without cleaning up.
            Ok(()) => {
                let _ = std::fs::remove_file(entry.path());
            }
            Err(TryLockError::WouldBlock) => {
                let pid = read_pid(&mut file);
                if pid != std::process::id() {
                    let _ = poke(pid);
                }
            }
            Err(TryLockError::Error(_)) => {}
        }
    }
}

/// `SIGUSR1`: "re-check". Every `dhow` runner installs a handler for it
/// before it takes any lock, so a lock's PID can always be poked.
#[cfg(unix)]
fn poke(pid: u32) -> io::Result<()> {
    use rustix::process::{Pid, Signal, kill_process};
    let pid = i32::try_from(pid)
        .ok()
        .and_then(Pid::from_raw)
        .ok_or_else(|| io::Error::other("bad pid in lock file"))?;
    kill_process(pid, Signal::USR1).map_err(io::Error::from)
}

#[cfg(not(unix))]
fn poke(pid: u32) -> io::Result<()> {
    Err(io::Error::other(format!(
        "download is running in process {pid}; stop it there (Ctrl-C)"
    )))
}
