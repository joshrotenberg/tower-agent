use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// The provider a partial turn selects.
///
/// The kernel identifies a provider by its concrete service type. The planner
/// works with partial data before a service exists, so provider selection is a
/// value here and becomes a concrete service choice only when a resolved turn
/// is folded into a provider planner.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderId {
    Claude,
    Codex,
}

impl ProviderId {
    /// The provider tag used by `SessionHandle` values from the adapters.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
}

impl fmt::Display for ProviderId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A provider name that does not match a known provider.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnknownProvider(pub String);

impl fmt::Display for UnknownProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "unknown provider: {}", self.0)
    }
}

impl std::error::Error for UnknownProvider {}

impl FromStr for ProviderId {
    type Err = UnknownProvider;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "claude" => Ok(Self::Claude),
            "codex" => Ok(Self::Codex),
            other => Err(UnknownProvider(other.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_str() {
        for provider in [ProviderId::Claude, ProviderId::Codex] {
            assert_eq!(provider.as_str().parse::<ProviderId>(), Ok(provider));
        }
        assert!("gemini".parse::<ProviderId>().is_err());
    }
}
