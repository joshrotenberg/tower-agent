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
            session: None,
            usage: None,
            cost: None,
            duration: None,
            provider_turns: None,
        }
    }
}
