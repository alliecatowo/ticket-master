//! The determinism substrate: injected clocks and identifier sources.
//!
//! Nothing outside this module may read the wall clock or draw randomness directly. A workspace
//! hygiene test enforces that, because reproducible replay and deterministic scheduling depend
//! on it (`SPEC.md` §2.2, §16.14).

use crate::id::{Id, IdKind};
use crate::time_::Timestamp;
use parking_lot::Mutex;
use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};
use std::collections::BTreeMap;

/// Return a monotonic deadline for runtime timeouts that must include real waiting time.
///
/// Unlike [`Clock`], this is intentionally not replay-controlled: callers use it only to bound
/// foreground process lifetime, never for persisted decisions or ticket timestamps.
pub fn monotonic_deadline_after(duration: std::time::Duration) -> std::time::Instant {
    std::time::Instant::now() + duration
}

/// Return the current monotonic instant for measuring runtime-only elapsed time.
pub fn monotonic_now() -> std::time::Instant {
    std::time::Instant::now()
}

/// A source of the current time.
pub trait Clock: Send + Sync {
    /// The current instant.
    fn now(&self) -> Timestamp;
}

/// A source of fresh identifiers.
pub trait IdSource: Send + Sync {
    /// Allocate the next identifier of `kind`.
    fn next(&self, kind: IdKind) -> Id;
    /// Draw `n` lowercase hex characters.
    fn random_hex(&self, n: usize) -> String;
}

impl<T: Clock + ?Sized> Clock for std::sync::Arc<T> {
    fn now(&self) -> Timestamp {
        (**self).now()
    }
}

impl<T: IdSource + ?Sized> IdSource for std::sync::Arc<T> {
    fn next(&self, kind: IdKind) -> Id {
        (**self).next(kind)
    }
    fn random_hex(&self, n: usize) -> String {
        (**self).random_hex(n)
    }
}

/// The real wall clock.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        let d = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        Timestamp::from_unix_nanos(d.as_nanos() as i128)
    }
}

/// A clock that only moves when a test moves it.
#[derive(Debug)]
pub struct FixedClock(Mutex<Timestamp>);

impl FixedClock {
    /// Start at `t`.
    pub fn new(t: Timestamp) -> Self {
        FixedClock(Mutex::new(t))
    }

    /// Start at the Unix epoch.
    pub fn epoch() -> Self {
        FixedClock::new(Timestamp::EPOCH)
    }

    /// Move forward by whole seconds.
    pub fn advance_seconds(&self, secs: i64) {
        let mut g = self.0.lock();
        *g = g.plus_seconds(secs);
    }

    /// Move forward by whole milliseconds.
    pub fn advance_millis(&self, millis: i64) {
        let mut g = self.0.lock();
        *g = g.plus_millis(millis);
    }

    /// Jump to an absolute instant.
    pub fn set(&self, t: Timestamp) {
        *self.0.lock() = t;
    }
}

impl Clock for FixedClock {
    fn now(&self) -> Timestamp {
        *self.0.lock()
    }
}

/// Monotonic per-kind counters plus a seeded RNG for hex identifiers.
///
/// Counters are owned by project state: construct with [`CounterIds::with_counters`] after
/// replay so numeric suffixes are never reused across restarts.
#[derive(Debug)]
pub struct CounterIds {
    counters: Mutex<BTreeMap<String, u64>>,
    rng: Mutex<StdRng>,
}

/// A [`CounterIds`] seeded deterministically; the conventional choice in tests.
pub type TestIds = CounterIds;

impl CounterIds {
    /// A fresh source with all counters at zero and a fixed seed.
    pub fn new() -> Self {
        CounterIds::seeded(0)
    }

    /// A fresh source with all counters at zero and the given RNG seed.
    pub fn seeded(seed: u64) -> Self {
        CounterIds {
            counters: Mutex::new(BTreeMap::new()),
            rng: Mutex::new(StdRng::seed_from_u64(seed)),
        }
    }

    /// Restore persisted counters. Each value is the highest suffix already allocated.
    pub fn with_counters(counters: BTreeMap<String, u64>, seed: u64) -> Self {
        CounterIds {
            counters: Mutex::new(counters),
            rng: Mutex::new(StdRng::seed_from_u64(seed)),
        }
    }

    /// The current high-water mark for every counter, for persistence.
    pub fn snapshot(&self) -> BTreeMap<String, u64> {
        self.counters.lock().clone()
    }

    /// Raise a counter's high-water mark, e.g. while replaying events.
    pub fn observe(&self, kind: IdKind, number: u64) {
        let mut g = self.counters.lock();
        let e = g.entry(kind.counter().to_string()).or_insert(0);
        if number > *e {
            *e = number;
        }
    }

    /// The next numeric suffix for `kind`, without rendering an identifier.
    pub fn next_number(&self, kind: IdKind) -> u64 {
        let mut g = self.counters.lock();
        let e = g.entry(kind.counter().to_string()).or_insert(0);
        *e += 1;
        *e
    }
}

impl Default for CounterIds {
    fn default() -> Self {
        CounterIds::new()
    }
}

impl IdSource for CounterIds {
    fn next(&self, kind: IdKind) -> Id {
        match kind {
            IdKind::Artifact | IdKind::Lease => {
                Id::new(format!("{}{}", kind.prefix(), self.random_hex(12)))
            }
            IdKind::Participant => Id::new(format!("agent:local/{}", self.random_hex(6))),
            _ => {
                let n = self.next_number(kind);
                Id::new(format!(
                    "{}{:0width$}",
                    kind.prefix(),
                    n,
                    width = kind.pad()
                ))
            }
        }
    }

    fn random_hex(&self, n: usize) -> String {
        let mut rng = self.rng.lock();
        (0..n)
            .map(|_| std::char::from_digit(rng.random_range(0..16), 16).unwrap_or('0'))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_clock_only_moves_when_told() {
        let c = FixedClock::epoch();
        assert_eq!(c.now(), Timestamp::EPOCH);
        c.advance_seconds(60);
        assert_eq!(c.now().unix_seconds(), 60);
        c.advance_millis(500);
        assert_eq!(c.now().millis_since(Timestamp::EPOCH), 60_500);
    }

    #[test]
    fn counters_are_monotonic_and_padded() {
        let ids = CounterIds::new();
        assert_eq!(ids.next(IdKind::Ticket).as_str(), "T-1");
        assert_eq!(ids.next(IdKind::Ticket).as_str(), "T-2");
        assert_eq!(ids.next(IdKind::Decision).as_str(), "D-001");
        assert_eq!(ids.next(IdKind::Verification).as_str(), "V-1");
    }

    #[test]
    fn counters_survive_a_restore() {
        let ids = CounterIds::new();
        ids.next(IdKind::Ticket);
        ids.next(IdKind::Ticket);
        let snap = ids.snapshot();
        let restored = CounterIds::with_counters(snap, 0);
        assert_eq!(restored.next(IdKind::Ticket).as_str(), "T-3");
    }

    #[test]
    fn observe_raises_but_never_lowers() {
        let ids = CounterIds::new();
        ids.observe(IdKind::Ticket, 40);
        ids.observe(IdKind::Ticket, 7);
        assert_eq!(ids.next(IdKind::Ticket).as_str(), "T-41");
    }

    #[test]
    fn hex_ids_are_deterministic_for_a_seed() {
        let a = CounterIds::seeded(7).next(IdKind::Artifact);
        let b = CounterIds::seeded(7).next(IdKind::Artifact);
        assert_eq!(a, b);
        assert_eq!(a.kind(), Some(IdKind::Artifact));
        assert_ne!(a, CounterIds::seeded(8).next(IdKind::Artifact));
    }

    #[test]
    fn leases_render_as_hex() {
        let id = CounterIds::new().next(IdKind::Lease);
        assert_eq!(id.kind(), Some(IdKind::Lease));
    }
}
