use std::time::Duration;

use crate::SessionHandle;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TokenUsage {
    pub input: Option<u64>,
    pub cached_input: Option<u64>,
    pub cache_write_input: Option<u64>,
    pub output: Option<u64>,
    pub reasoning_output: Option<u64>,
    /// Total computed or explicitly reported by the provider adapter.
    pub provider_total: Option<u64>,
}

impl TokenUsage {
    /// Best available total without inventing zero for missing evidence.
    pub fn total(self) -> Option<u64> {
        self.provider_total.or_else(|| {
            let buckets = [
                self.input,
                self.cached_input,
                self.cache_write_input,
                self.output,
                self.reasoning_output,
            ];
            buckets
                .iter()
                .any(Option::is_some)
                .then(|| buckets.into_iter().flatten().sum())
        })
    }

    pub const fn is_empty(self) -> bool {
        self.input.is_none()
            && self.cached_input.is_none()
            && self.cache_write_input.is_none()
            && self.output.is_none()
            && self.reasoning_output.is_none()
            && self.provider_total.is_none()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Cost {
    pub amount: f64,
    pub currency: String,
}

impl Cost {
    pub fn usd(amount: f64) -> Self {
        Self {
            amount,
            currency: "USD".to_string(),
        }
    }
}

/// The typed terminal result of one finite turn.
#[derive(Clone, Debug, PartialEq)]
pub struct TurnOutcome {
    pub output: String,
    /// The schema-constrained payload a provider validated, when one was
    /// requested.
    ///
    /// This stays separate from `output` so a caller can revalidate the exact
    /// value the provider produced rather than reparsing prose. It is `None`
    /// whenever no structured output was requested; a turn that requested one
    /// and did not receive it fails settlement instead of succeeding with an
    /// absent payload.
    pub structured: Option<serde_json::Value>,
    pub session: Option<SessionHandle>,
    pub usage: Option<TokenUsage>,
    pub cost: Option<Cost>,
    pub duration: Option<Duration>,
    pub provider_turns: Option<u32>,
}

/// Partial terminal evidence retained when a provider call fails.
///
/// Every field is optional because absence means neither the provider nor the
/// host's pre-launch assignment established it. Provider-private session
/// handles remain redacted in `Debug` and must be translated by a host before
/// crossing a protocol boundary.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FailureEvidence {
    pub session: Option<SessionHandle>,
    pub usage: Option<TokenUsage>,
    pub cost: Option<Cost>,
    pub duration: Option<Duration>,
    pub provider_turns: Option<u32>,
}

impl FailureEvidence {
    pub fn merge_missing(&mut self, other: &Self) {
        if self.session.is_none() {
            self.session.clone_from(&other.session);
        }
        if self.usage.is_none() {
            self.usage = other.usage;
        }
        if self.cost.is_none() {
            self.cost.clone_from(&other.cost);
        }
        if self.duration.is_none() {
            self.duration = other.duration;
        }
        if self.provider_turns.is_none() {
            self.provider_turns = other.provider_turns;
        }
    }
}

impl TurnOutcome {
    pub fn new(output: impl Into<String>) -> Self {
        Self {
            output: output.into(),
            structured: None,
            session: None,
            usage: None,
            cost: None,
            duration: None,
            provider_turns: None,
        }
    }
}

/// A terminal response that can contribute its facts to an outer failure.
///
/// Middleware that converts a settled call into a failure, such as a deadline
/// that elapsed while the provider was finishing, needs the settled response's
/// accounting without knowing the concrete response type. Without this
/// projection an outer error would discard evidence the provider actually
/// produced, leaving a host unable to reconcile spend or offer continuation.
pub trait TerminalEvidence {
    /// The terminal facts this response established.
    fn terminal_evidence(&self) -> FailureEvidence;
}

impl TerminalEvidence for TurnOutcome {
    fn terminal_evidence(&self) -> FailureEvidence {
        FailureEvidence {
            session: self.session.clone(),
            usage: self.usage,
            cost: self.cost.clone(),
            duration: self.duration,
            provider_turns: self.provider_turns,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn total_stays_absent_when_every_bucket_is_absent() {
        let usage = TokenUsage::default();
        assert_eq!(usage.total(), None);
        assert!(usage.is_empty());
    }

    #[test]
    fn total_sums_only_present_buckets_without_treating_absence_as_zero() {
        let usage = TokenUsage {
            input: Some(30),
            output: Some(12),
            ..TokenUsage::default()
        };
        assert_eq!(usage.total(), Some(42));
        assert!(!usage.is_empty());
    }

    #[test]
    fn a_reported_total_wins_over_the_computed_sum() {
        let usage = TokenUsage {
            input: Some(1),
            provider_total: Some(99),
            ..TokenUsage::default()
        };
        assert_eq!(usage.total(), Some(99));
    }

    #[test]
    fn an_explicit_zero_bucket_is_a_value_not_an_absence() {
        let usage = TokenUsage {
            input: Some(0),
            ..TokenUsage::default()
        };
        assert!(!usage.is_empty());
        assert_eq!(usage.total(), Some(0));
    }

    #[test]
    fn merge_missing_fills_only_absent_fields() {
        let mut evidence = FailureEvidence {
            cost: Some(Cost::usd(1.0)),
            ..FailureEvidence::default()
        };
        evidence.merge_missing(&FailureEvidence {
            session: Some(SessionHandle::new("fake", "s")),
            cost: Some(Cost::usd(9.0)),
            provider_turns: Some(3),
            ..FailureEvidence::default()
        });

        assert_eq!(evidence.cost, Some(Cost::usd(1.0)));
        assert_eq!(evidence.provider_turns, Some(3));
        assert_eq!(
            evidence.session.as_ref().map(SessionHandle::value),
            Some("s")
        );
        assert_eq!(evidence.usage, None);
    }

    #[test]
    fn a_successful_outcome_projects_every_terminal_fact() {
        let outcome = TurnOutcome {
            session: Some(SessionHandle::new("fake", "s")),
            usage: Some(TokenUsage {
                input: Some(7),
                ..TokenUsage::default()
            }),
            cost: Some(Cost::usd(0.19)),
            duration: Some(Duration::from_secs(3)),
            provider_turns: Some(4),
            ..TurnOutcome::new("done")
        };

        let evidence = outcome.terminal_evidence();
        assert_eq!(evidence.session, outcome.session);
        assert_eq!(evidence.usage, outcome.usage);
        assert_eq!(evidence.cost, outcome.cost);
        assert_eq!(evidence.duration, outcome.duration);
        assert_eq!(evidence.provider_turns, outcome.provider_turns);
    }

    #[test]
    fn an_empty_outcome_projects_absence_rather_than_zero() {
        let evidence = TurnOutcome::new("done").terminal_evidence();
        assert_eq!(evidence, FailureEvidence::default());
        assert_eq!(evidence.usage, None);
        assert_eq!(evidence.cost, None);
        assert_eq!(evidence.duration, None);
        assert_eq!(evidence.provider_turns, None);
    }

    #[test]
    fn debug_output_never_reveals_a_session_value() {
        let outcome = TurnOutcome {
            session: Some(SessionHandle::new("fake", "host-private-session")),
            ..TurnOutcome::new("done")
        };
        let rendered = format!("{outcome:?}");
        assert!(!rendered.contains("host-private-session"), "{rendered}");

        let evidence = outcome.terminal_evidence();
        let rendered = format!("{evidence:?}");
        assert!(!rendered.contains("host-private-session"), "{rendered}");
    }
}
