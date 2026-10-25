//! Event delivery (ARCHITECTURE §3.2, §3.3): the engine never waits for the
//! shell. Events go into a queue that one thread per engine hands to the
//! listener in order; a `Progress` still queued is replaced by a newer one
//! for the same download. The thread is parked while nothing happens and
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
        if let Event::Progress { id, .. } = &event {
            // The newest event of this download, if still queued, is
            // overtaken: replace it rather than queue both.
            let last = q
                .events
                .iter_mut()
                .rev()
                .find(|e| e.download() == Some(id.as_str()));
            if let Some(slot @ Event::Progress { .. }) = last {
                *slot = event;
                return;
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
