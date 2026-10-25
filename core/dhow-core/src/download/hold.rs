//! One run at a time on a part file, per process (ENGINE-SPEC §6). A run's
//! file work outlives its future when that is dropped: `prepare` and
//! finalize finish on the blocking pool, the writer thread does a final
//! commit. Each of them owns a clone of the run's [`PartHold`], so a rerun
//! on the same path waits until the last of them is done instead of
//! working on the same files in between. Another process is the caller's
//! business (run locks), as before.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};

use tokio::sync::OwnedMutexGuard;

type Lock = tokio::sync::Mutex<()>;

static LOCKS: Mutex<Option<HashMap<PathBuf, Weak<Lock>>>> = Mutex::new(None);

/// Held while any clone is alive.
#[derive(Clone, Debug)]
pub struct PartHold(#[allow(dead_code)] Arc<OwnedMutexGuard<()>>);

impl PartHold {
    /// Waits until no other hold on `part` is alive in this process.
    pub async fn acquire(part: &Path) -> Self {
        let lock = {
            let mut locks = LOCKS.lock().unwrap_or_else(|e| e.into_inner());
            let locks = locks.get_or_insert_with(HashMap::new);
            locks.retain(|_, l| l.strong_count() > 0);
            match locks.get(part).and_then(Weak::upgrade) {
                Some(l) => l,
                None => {
                    let l = Arc::new(Lock::new(()));
                    locks.insert(part.to_owned(), Arc::downgrade(&l));
                    l
                }
            }
        };
        Self(Arc::new(lock.lock_owned().await))
    }
}

/// Test hook: parks the next blocking thread that reaches a point for a
/// part file until the test releases it, so a test can drop a run while
/// its file work is half done.
#[cfg(test)]
pub(crate) mod park {
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Condvar, Mutex};

    #[derive(Debug, Default)]
    pub(crate) struct Park {
        /// (reached, released)
        state: Mutex<(bool, bool)>,
        changed: Condvar,
    }

    impl Park {
        /// Blocks until a thread is parked here (or it was released).
        pub(crate) fn wait_reached(&self) {
            let mut s = self.state.lock().unwrap();
            while !s.0 && !s.1 {
                s = self.changed.wait(s).unwrap();
            }
        }

        pub(crate) fn release(&self) {
            self.state.lock().unwrap().1 = true;
            self.changed.notify_all();
        }
    }

    /// Releases its park when dropped, so a failing test can't leave a
    /// thread parked (and the runtime waiting for it) forever.
    pub(crate) struct Armed(Arc<Park>);

    impl std::ops::Deref for Armed {
        type Target = Park;
        fn deref(&self) -> &Park {
            &self.0
        }
    }

    impl Armed {
        pub(crate) fn park(&self) -> Arc<Park> {
            self.0.clone()
        }
    }

    impl Drop for Armed {
        fn drop(&mut self) {
            self.0.release();
        }
    }

    static ARMED: Mutex<Vec<(&'static str, PathBuf, Arc<Park>)>> = Mutex::new(Vec::new());

    pub(crate) fn arm(point: &'static str, part: &Path) -> Armed {
        let park = Arc::new(Park::default());
        ARMED
            .lock()
            .unwrap()
            .push((point, part.to_owned(), park.clone()));
        Armed(park)
    }

    pub(crate) fn at(point: &'static str, part: &Path) {
        let park = {
            let mut armed = ARMED.lock().unwrap();
            let Some(i) = armed.iter().position(|(p, f, _)| *p == point && f == part) else {
                return;
            };
            armed.remove(i).2
        };
        let mut s = park.state.lock().unwrap();
        s.0 = true;
        park.changed.notify_all();
        while !s.1 {
            s = park.changed.wait(s).unwrap();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn a_second_hold_waits_for_every_clone_of_the_first() {
        let dir = tempfile::tempdir().unwrap();
        let part = dir.path().join("f.part");
        let first = PartHold::acquire(&part).await;
        let clone = first.clone();
        let other = PartHold::acquire(&dir.path().join("g.part")).await;
        let second = PartHold::acquire(&part);
        tokio::pin!(second);
        let wait = Duration::from_millis(50);
        assert!(tokio::time::timeout(wait, &mut second).await.is_err());
        drop(first);
        assert!(tokio::time::timeout(wait, &mut second).await.is_err());
        drop(clone);
        tokio::time::timeout(Duration::from_secs(5), &mut second)
            .await
            .expect("free once the last clone is gone");
        drop(other);
    }
}
