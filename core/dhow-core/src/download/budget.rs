//! The connection budget shared by every download in a process
//! (ENGINE-SPEC §10.1–10.3, §10.5).
//!
//! A download holds a [`Ticket`] and asks it for every connection. The
//! budget enforces the global and per-host caps and the order: a download
//! still ramping up has a claim on the connections it may yet open, and
//! younger downloads grow only into room beyond older claims. A newcomer
//! always gets its first connection, taken if need be from the youngest
//! download that has more than one.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::Notify;

#[derive(Clone, Copy, Debug)]
pub struct BudgetConfig {
    /// `settings.global_connections`.
    pub global: u32,
    /// `settings.per_host_connections`, per `host:port`.
    pub per_host: u32,
    /// `settings.min_connections_for_new`: a newcomer gets one connection
    /// even when the global budget is spent (0: it waits).
    pub min_for_new: u32,
}

impl Default for BudgetConfig {
    fn default() -> Self {
        Self {
            global: 16,
            per_host: 4,
            min_for_new: 1,
        }
    }
}

/// Oldest first: start time, then arrival order.
type Key = (i64, u64);

#[derive(Debug)]
struct Entry {
    host: String,
    held: u32,
    /// Most connections it will use (§4.1).
    limit: u32,
    /// Still ramping up: its unopened connections are claimed.
    ramping: bool,
    /// Connections this download was asked to give up.
    shed: u32,
    /// Who gets each connection given up, in order.
    shed_for: VecDeque<Key>,
    /// Slots given up by others for this download.
    reserved: u32,
}

#[derive(Debug, Default)]
struct State {
    entries: BTreeMap<Key, Entry>,
}

impl State {
    fn used(&self) -> u32 {
        self.entries.values().map(|e| e.held + e.reserved).sum()
    }

    fn host_used(&self, host: &str) -> u32 {
        self.entries
            .values()
            .filter(|e| e.host == host)
            .map(|e| e.held + e.reserved)
            .sum()
    }

    /// Connections an older, still-ramping download may yet open: only as
    /// many as its host has room for (a host-capped elder doesn't hold up
    /// other hosts, §10.5).
    fn claim(&self, e: &Entry, config: &BudgetConfig) -> u32 {
        if !e.ramping {
            return 0;
        }
        let host_room = config.per_host.saturating_sub(self.host_used(&e.host));
        e.limit.saturating_sub(e.held).min(host_room)
    }

    /// A first connection: the caps alone decide.
    fn can_start(&self, key: Key, config: &BudgetConfig) -> bool {
        let host = &self.entries[&key].host;
        self.used() < config.global && self.host_used(host) < config.per_host
    }

    /// One more connection for an already running download: the caps, and
    /// room left after every older download's claim (§10.3, oldest first).
    fn can_grow(&self, key: Key, config: &BudgetConfig) -> bool {
        if !self.can_start(key, config) {
            return false;
        }
        let host = &self.entries[&key].host;
        let older = self.entries.range(..key).map(|(_, e)| e);
        let (claimed, host_claimed) = older.fold((0, 0), |(all, same), e| {
            let c = self.claim(e, config);
            (all + c, same + if &e.host == host { c } else { 0 })
        });
        config.global - self.used() > claimed
            && config.per_host - self.host_used(host) > host_claimed
    }
}

#[derive(Debug, Default)]
pub struct Budget {
    config: BudgetConfig,
    state: Mutex<State>,
    /// Woken on every change; waiters re-check.
    changed: Notify,
    arrivals: AtomicU64,
}

impl Budget {
    pub fn new(config: BudgetConfig) -> Self {
        Self {
            config,
            ..Self::default()
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Registers a download. `started_at` (unix ms of its first start)
    /// orders it; `host` is `host:port`.
    pub fn register(self: &Arc<Self>, started_at: i64, host: String) -> Ticket {
        let key = (started_at, self.arrivals.fetch_add(1, Ordering::Relaxed));
        self.state().entries.insert(
            key,
            Entry {
                host,
                held: 0,
                limit: 1,
                ramping: false,
                shed: 0,
                shed_for: VecDeque::new(),
                reserved: 0,
            },
        );
        Ticket {
            budget: self.clone(),
            key,
        }
    }

    /// Connections held, total and on `host`.
    pub fn usage(&self, host: &str) -> (u32, u32) {
        let s = self.state();
        (s.used(), s.host_used(host))
    }
}

/// One download's claim on the budget. Dropping it returns everything.
pub struct Ticket {
    budget: Arc<Budget>,
    key: Key,
}

impl Ticket {
    /// Waits for this download's first connection.
    pub async fn first(&self) {
        loop {
            // Registered before checking, so a release in between isn't missed.
            let changed = self.budget.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.try_first() {
                return;
            }
            changed.await;
        }
    }

    /// Takes the first connection if it can be had now; otherwise makes
    /// sure one is on its way (asks the youngest download with spare
    /// connections to give one up).
    pub fn try_first(&self) -> bool {
        let config = self.budget.config;
        let mut s = self.budget.state();
        let key = self.key;
        let e = s.entries.get_mut(&key).expect("registered");
        if e.reserved > 0 {
            e.reserved -= 1;
            e.held += 1;
            return true;
        }
        if s.can_start(key, &config) {
            let e = s.entries.get_mut(&key).expect("registered");
            e.held += 1;
            drop(s);
            self.budget.changed.notify_waiters();
            return true;
        }
        let already_asked = s.entries.values().any(|v| v.shed_for.contains(&key));
        if config.min_for_new == 0 || already_asked {
            return false;
        }
        let host = s.entries[&key].host.clone();
        // The per-host cap stays absolute: only a download on the same host
        // can give up a slot that host needs.
        let host_full = s.host_used(&host) >= config.per_host;
        let victim = s
            .entries
            .iter()
            .rev()
            .find(|(k, v)| **k != key && v.held > v.shed + 1 && (!host_full || v.host == host))
            .map(|(k, _)| *k);
        if let Some(v) = victim {
            let v = s.entries.get_mut(&v).expect("found above");
            v.shed += 1;
            v.shed_for.push_back(key);
            drop(s);
            self.budget.changed.notify_waiters();
        }
        false
    }

    /// The most connections this download will use; while it has fewer it
    /// counts as ramping and claims the rest ahead of younger downloads.
    pub fn set_limit(&self, limit: u32) {
        let mut s = self.budget.state();
        let e = s.entries.get_mut(&self.key).expect("registered");
        e.limit = limit.max(1);
        e.ramping = e.limit > 1;
        drop(s);
        self.budget.changed.notify_waiters();
    }

    /// One more connection for ramp-up, if the budget allows it now.
    pub fn try_grow(&self) -> bool {
        let config = self.budget.config;
        let mut s = self.budget.state();
        if !s.can_grow(self.key, &config) {
            return false;
        }
        s.entries.get_mut(&self.key).expect("registered").held += 1;
        true
    }

    /// Ramp-up is over (saturated, nothing left to split, downgraded):
    /// release the claim so younger downloads can grow.
    pub fn stop_wanting(&self) {
        let mut s = self.budget.state();
        s.entries.get_mut(&self.key).expect("registered").ramping = false;
        drop(s);
        self.budget.changed.notify_waiters();
    }

    /// Connections this download was asked to give up and hasn't yet.
    pub fn shed_requested(&self) -> u32 {
        self.budget.state().entries[&self.key].shed
    }

    /// A connection ended. `shed`: it ended because it was asked to; the
    /// slot goes to the download it was given up for.
    pub fn release(&self, shed: bool) {
        let mut s = self.budget.state();
        let e = s.entries.get_mut(&self.key).expect("registered");
        e.held = e.held.saturating_sub(1);
        let target = if shed && e.shed > 0 {
            e.shed -= 1;
            e.shed_for.pop_front()
        } else {
            None
        };
        if let Some(t) = target.and_then(|t| s.entries.get_mut(&t)) {
            t.reserved += 1;
        }
        drop(s);
        self.budget.changed.notify_waiters();
    }

    /// Resolves on the next budget change (grants, releases, shed
    /// requests). Enable it before checking state, or a change in between
    /// is missed.
    pub fn notified(&self) -> tokio::sync::futures::Notified<'_> {
        self.budget.changed.notified()
    }

    /// Returns every slot still held (end of a run, or before a reset).
    pub fn release_all(&self) {
        while self.held() > 0 {
            self.release(false);
        }
    }

    pub fn held(&self) -> u32 {
        self.budget.state().entries[&self.key].held
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        let mut s = self.budget.state();
        if let Some(e) = s.entries.remove(&self.key) {
            // Slots promised away by this download, never handed over:
            // hand them over now.
            for t in e.shed_for {
                if let Some(t) = s.entries.get_mut(&t) {
                    t.reserved += 1;
                }
            }
        }
        // Reservations made for it are simply free again.
        drop(s);
        self.budget.changed.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn budget(global: u32, per_host: u32) -> Arc<Budget> {
        Arc::new(Budget::new(BudgetConfig {
            global,
            per_host,
            min_for_new: 1,
        }))
    }

    fn ticket(b: &Arc<Budget>, age: i64, host: &str, limit: u32) -> Ticket {
        let t = b.register(age, host.into());
        t.set_limit(limit);
        t
    }

    #[test]
    fn global_and_per_host_caps() {
        let b = budget(3, 2);
        let a = ticket(&b, 1, "a:80", 8);
        assert!(a.try_first());
        assert!(a.try_grow());
        assert!(!a.try_grow(), "per-host 2");
        let c = ticket(&b, 2, "c:80", 8);
        assert!(c.try_first());
        assert!(!c.try_grow(), "global 3");
        assert_eq!(b.usage("a:80"), (3, 2));
    }

    #[test]
    fn younger_downloads_grow_only_beyond_older_claims() {
        let b = budget(6, 8);
        let old = ticket(&b, 1, "x:80", 4);
        let young = ticket(&b, 2, "y:80", 4);
        assert!(old.try_first() && young.try_first());
        // old may still open 3; 6 - 2 used = 4 free: young can take 1.
        assert!(young.try_grow());
        // Now 3 free, all claimed by old.
        assert!(!young.try_grow());
        assert!(old.try_grow() && old.try_grow() && old.try_grow());
        // old at its limit; young still can't: budget full.
        assert!(!young.try_grow());
    }

    #[test]
    fn a_finished_ramp_releases_its_claim() {
        let b = budget(4, 8);
        let old = ticket(&b, 1, "x:80", 8);
        let young = ticket(&b, 2, "y:80", 8);
        assert!(old.try_first() && young.try_first());
        assert!(!young.try_grow(), "2 free, both claimed by old");
        old.stop_wanting(); // saturated at 1
        assert!(young.try_grow());
    }

    #[test]
    fn a_host_capped_elder_does_not_block_other_hosts() {
        let b = budget(10, 1);
        let old = ticket(&b, 1, "x:80", 8);
        let young = ticket(&b, 2, "y:80", 8);
        assert!(old.try_first());
        assert!(!old.try_grow(), "its host is full");
        assert!(young.try_first());
        let b = budget(10, 2);
        let old = ticket(&b, 1, "x:80", 8);
        let young = ticket(&b, 2, "y:80", 8);
        assert!(old.try_first() && old.try_grow());
        assert!(young.try_first());
        assert!(young.try_grow(), "old's claim is 0: its host is full");
    }

    #[test]
    fn newcomer_gets_a_slot_from_the_youngest_with_spares() {
        let b = budget(4, 4);
        let a = ticket(&b, 1, "a:80", 2);
        let c = ticket(&b, 2, "c:80", 2);
        assert!(a.try_first() && a.try_grow());
        assert!(c.try_first() && c.try_grow());
        let n = ticket(&b, 3, "n:80", 2);
        assert!(!n.try_first());
        assert_eq!(
            (a.shed_requested(), c.shed_requested()),
            (0, 1),
            "youngest gives"
        );
        // c drops a connection; it is reserved for n, nobody else gets it.
        c.release(true);
        assert!(!a.try_grow());
        assert!(n.try_first());
        assert_eq!(b.usage("n:80"), (4, 1));
    }

    #[test]
    fn per_host_cap_beats_the_newcomer_guarantee() {
        let b = budget(16, 2);
        let a = ticket(&b, 1, "h:80", 8);
        let c = ticket(&b, 2, "h:80", 8);
        assert!(a.try_first() && c.try_first());
        let n = ticket(&b, 3, "h:80", 8);
        // Both holders on h have one connection: nobody to take from.
        assert!(!n.try_first());
        assert_eq!(a.shed_requested() + c.shed_requested(), 0);
        drop(c);
        assert!(n.try_first());
    }

    #[test]
    fn no_newcomer_guarantee_when_disabled() {
        let b = Arc::new(Budget::new(BudgetConfig {
            global: 2,
            per_host: 4,
            min_for_new: 0,
        }));
        let a = ticket(&b, 1, "a:80", 2);
        assert!(a.try_first() && a.try_grow());
        let n = ticket(&b, 2, "n:80", 2);
        assert!(!n.try_first());
        assert_eq!(a.shed_requested(), 0);
    }

    #[test]
    fn dropping_a_ticket_frees_and_hands_over() {
        let b = budget(2, 4);
        let a = ticket(&b, 1, "a:80", 2);
        assert!(a.try_first() && a.try_grow());
        let n = ticket(&b, 2, "n:80", 2);
        assert!(!n.try_first());
        assert_eq!(a.shed_requested(), 1);
        drop(a); // finished before giving the slot up
        assert!(n.try_first());
        assert!(n.try_grow(), "the rest of a's slots are free too");
    }

    #[tokio::test]
    async fn first_waits_for_a_release() {
        let b = budget(1, 4);
        let a = b.register(1, "a:80".into());
        assert!(a.try_first());
        let n = b.register(2, "n:80".into());
        let waiter = tokio::spawn(async move {
            n.first().await;
            n.held()
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(!waiter.is_finished());
        drop(a);
        assert_eq!(waiter.await.unwrap(), 1);
    }
}
