//! Injected time, so a scheduler can be driven by a test without sleeping.
//!
//! SDD §8 requires determinism: no test may wait on a wall clock. Everything
//! that needs "now" takes a [`Clock`] rather than calling `Utc::now()`, and
//! [`ManualClock`] lets a test place the process at a chosen instant and move it
//! forward by hand. The real binary wires [`SystemClock`]; nothing else does.

use std::sync::{Arc, Mutex};

use chrono::{DateTime, Duration, Utc};

/// A source of the current instant.
pub trait Clock: Send + Sync {
    /// The current time, always in UTC.
    fn now(&self) -> DateTime<Utc>;
}

/// The wall clock. The only implementation the shipped binary uses.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// A clock a test drives by hand.
///
/// Cloning shares the position, so the scheduler and the test never disagree
/// about what time it is.
#[derive(Debug, Clone)]
pub struct ManualClock {
    now: Arc<Mutex<DateTime<Utc>>>,
}

impl ManualClock {
    /// A clock stopped at `at`.
    pub fn at(at: DateTime<Utc>) -> Self {
        Self {
            now: Arc::new(Mutex::new(at)),
        }
    }

    /// Jump to an instant. Time may move backwards in a test; nothing depends on
    /// monotonicity, only on the value read.
    pub fn set(&self, at: DateTime<Utc>) {
        if let Ok(mut guard) = self.now.lock() {
            *guard = at;
        }
    }

    /// Move forward by `by`.
    pub fn advance(&self, by: Duration) {
        if let Ok(mut guard) = self.now.lock() {
            *guard += by;
        }
    }
}

impl Clock for ManualClock {
    fn now(&self) -> DateTime<Utc> {
        self.now
            .lock()
            .map(|guard| *guard)
            .unwrap_or_else(|_| Utc::now())
    }
}

impl Default for ManualClock {
    fn default() -> Self {
        Self::at(Utc::now())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_manual_clock_stands_still_until_it_is_moved() {
        let start = DateTime::from_timestamp(1_800_000_000, 0).unwrap();
        let clock = ManualClock::at(start);

        assert_eq!(clock.now(), start);
        clock.advance(Duration::seconds(90));
        assert_eq!(clock.now(), start + Duration::seconds(90));
    }

    #[test]
    fn clones_share_one_position() {
        let start = DateTime::from_timestamp(1_800_000_000, 0).unwrap();
        let clock = ManualClock::at(start);
        let observer = clock.clone();

        clock.set(start + Duration::hours(3));

        assert_eq!(observer.now(), start + Duration::hours(3));
    }
}
