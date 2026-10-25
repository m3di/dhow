//! Event delivery (ARCHITECTURE §3.2, §3.3): the engine never waits for the
//! shell. Events go into a queue that one thread per engine hands to the
//! listener in order; a `Progress` or `RangeMap` still queued is replaced
//! by a newer one for the same download. The thread is parked while nothing happens and
//! is not a runtime task.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};

use crate::types::Event;

/// Implemented by the shell; called on the engine's event thread, one
/// event at a time. It may take its time (events queue meanwhile) and may
/// call the engine.
#[uniffi::export(with_foreign)]
pub trait EventListener: Send + Sync {
    fn on_event(&self, event: Event);
}

#[derive(Default)]
struct Queue {
    events: VecDeque<Event>,
    closed: bool,
}

#[derive(Default)]
struct Shared {
    queue: Mutex<Queue>,
    ready: Condvar,
}

pub(crate) struct Sink {
    shared: Arc<Shared>,
}

impl Sink {
    pub fn start(listener: Arc<dyn EventListener>) -> std::io::Result<Self> {
        let shared = Arc::new(Shared::default());
        let s = shared.clone();
        std::thread::Builder::new()
            .name("dhow-events".into())
            .spawn(move || deliver(&s, &*listener))?;
        Ok(Self { shared })
    }

    pub fn push(&self, event: Event) {
        let mut q = self.shared.queue.lock().unwrap_or_else(|e| e.into_inner());
        if q.closed {
            return;
        }
        if coalesces(&event) {
            // The newest queued event of this kind for this download is
            // overtaken, unless another event of it (a state change, ...)
            // came since: replace it rather than queue both. The other
            // coalescing kind may come between (both alternate while the
            // segment map is watched).
            let id = event.download().map(str::to_owned);
            for slot in q.events.iter_mut().rev() {
                if slot.download() != id.as_deref() {
                    continue;
                }
                if std::mem::discriminant(slot) == std::mem::discriminant(&event) {
                    *slot = event;
                    return;
                }
                if !coalesces(slot) {
                    break;
                }
            }
        }
        q.events.push_back(event);
        self.shared.ready.notify_one();
    }

    /// Delivers what is queued, then stops the thread.
    pub fn close(&self) {
        let mut q = self.shared.queue.lock().unwrap_or_else(|e| e.into_inner());
        q.closed = true;
        self.shared.ready.notify_one();
    }
}

impl Drop for Sink {
    fn drop(&mut self) {
        self.close();
    }
}

/// A newer one says all an older one did (§3.3).
fn coalesces(e: &Event) -> bool {
    matches!(e, Event::Progress { .. } | Event::RangeMap { .. })
}

fn deliver(shared: &Shared, listener: &dyn EventListener) {
    loop {
        let event = {
            let mut q = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if let Some(e) = q.events.pop_front() {
                    break e;
                }
                if q.closed {
                    return;
                }
                q = shared.ready.wait(q).unwrap_or_else(|e| e.into_inner());
            }
        };
        listener.on_event(event);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;
    use crate::types::{DownloadState, RangeMap};

    /// Holds the event thread in its first delivery until released.
    struct Gate {
        entered: Mutex<mpsc::Sender<()>>,
        release: Mutex<mpsc::Receiver<()>>,
    }

    impl EventListener for Gate {
        fn on_event(&self, _: Event) {
            let _ = self.entered.lock().unwrap().send(());
            let _ = self.release.lock().unwrap().recv();
        }
    }

    fn progress(id: &str, downloaded: u64) -> Event {
        Event::Progress {
            id: id.into(),
            downloaded,
            total: None,
            connections: 1,
            speed_bps: 0,
            eta_secs: None,
        }
    }

    fn map(id: &str, total: u64) -> Event {
        Event::RangeMap {
            id: id.into(),
            map: RangeMap {
                total: Some(total),
                done: vec![],
                connections: vec![],
                pending: vec![],
            },
        }
    }

    fn state(id: &str) -> Event {
        Event::StateChanged {
            id: id.into(),
            from: DownloadState::Queued,
            to: DownloadState::Active,
            reason: None,
        }
    }

    #[test]
    fn progress_and_range_maps_coalesce_together_but_not_past_other_events() {
        let (entered_tx, entered) = mpsc::channel();
        let (release, release_rx) = mpsc::channel::<()>();
        let sink = Sink::start(Arc::new(Gate {
            entered: Mutex::new(entered_tx),
            release: Mutex::new(release_rx),
        }))
        .unwrap();
        sink.push(state("x"));
        entered.recv().unwrap();
        let queued = |sink: &Sink| -> Vec<Event> {
            sink.shared
                .queue
                .lock()
                .unwrap()
                .events
                .iter()
                .cloned()
                .collect()
        };

        // Watched, they alternate; the newest of each stays.
        for n in 0..100 {
            sink.push(progress("a", n));
            sink.push(map("a", n));
            sink.push(progress("b", n));
        }
        assert_eq!(
            queued(&sink),
            [progress("a", 99), map("a", 99), progress("b", 99)]
        );
        // Never past another event of the same download.
        sink.push(state("a"));
        sink.push(progress("a", 100));
        sink.push(map("a", 100));
        assert_eq!(
            queued(&sink),
            [
                progress("a", 99),
                map("a", 99),
                progress("b", 99),
                state("a"),
                progress("a", 100),
                map("a", 100),
            ]
        );
        drop(release);
    }
}
