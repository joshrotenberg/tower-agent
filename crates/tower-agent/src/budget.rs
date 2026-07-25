//! A server-wide spend cap.
//!
//! The first-run experiment found runs unbounded in cost. A [`Budget`] tracks
//! cumulative spend and, if a cap is set, refuses further runs once it is
//! reached, so a long-lived server cannot run away. Costs come from the backend
//! ([`crate::Outcome::cost_usd`]).

use std::sync::Mutex;

/// Tracks cumulative spend against an optional cap (USD).
pub struct Budget {
    spent: Mutex<f64>,
    cap: Option<f64>,
}

impl Budget {
    /// A budget with an optional cap. `None` means no limit.
    pub fn new(cap: Option<f64>) -> Self {
        Budget {
            spent: Mutex::new(0.0),
            cap,
        }
    }

    /// The cumulative spend so far.
    pub fn spent(&self) -> f64 {
        *self.spent.lock().unwrap()
    }

    /// The remaining budget, or `None` if uncapped.
    pub fn remaining(&self) -> Option<f64> {
        self.cap.map(|c| (c - self.spent()).max(0.0))
    }

    /// Whether the cap is set and already reached.
    pub fn exhausted(&self) -> bool {
        self.cap.is_some_and(|c| self.spent() >= c)
    }

    /// Add to the cumulative spend.
    pub fn record(&self, cost_usd: f64) {
        *self.spent.lock().unwrap() += cost_usd;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uncapped_is_never_exhausted() {
        let b = Budget::new(None);
        b.record(1000.0);
        assert!(!b.exhausted());
        assert_eq!(b.remaining(), None);
    }

    #[test]
    fn capped_exhausts_at_the_limit() {
        let b = Budget::new(Some(1.0));
        assert!(!b.exhausted());
        b.record(0.6);
        assert!(!b.exhausted());
        assert_eq!(b.remaining(), Some(0.4));
        b.record(0.5);
        assert!(b.exhausted());
        assert_eq!(b.remaining(), Some(0.0));
    }
}
