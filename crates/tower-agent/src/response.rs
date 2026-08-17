use std::time::Duration;

use crate::SessionHandle;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
}

impl TokenUsage {
    pub const fn total(self) -> u64 {
        self.input.saturating_add(self.output)
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
