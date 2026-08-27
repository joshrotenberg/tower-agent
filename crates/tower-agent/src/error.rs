use std::fmt;
use std::time::Duration;

use crate::FailureEvidence;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorKind {
    InvalidRequest,
    Authentication,
    Unauthorized,
    Unsupported,
    /// This host has no capacity right now. The provider was never asked.
    Busy,
    /// The provider or something it depends on is not serving. Distinct from
    /// [`ErrorKind::Busy`], which is this host's own capacity, and from
    /// [`ErrorKind::Limit`], which is a quota that a caller has spent.
    /// Retrying elsewhere may succeed where retrying here will not.
    Unavailable,
    DeadlineExceeded,
    Cancelled,
    Budget,
    Limit,
    Provider,
    Internal,
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidRequest => "invalid_request",
            Self::Authentication => "authentication",
            Self::Unauthorized => "unauthorized",
            Self::Unsupported => "unsupported",
            Self::Busy => "busy",
            Self::Unavailable => "unavailable",
            Self::DeadlineExceeded => "deadline_exceeded",
            Self::Cancelled => "cancelled",
            Self::Budget => "budget",
            Self::Limit => "limit",
            Self::Provider => "provider",
            Self::Internal => "internal",
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailurePhase {
    Admission,
    Validation,
    Launch,
    Running,
    Settlement,
}

impl fmt::Display for FailurePhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Admission => "admission",
            Self::Validation => "validation",
            Self::Launch => "launch",
            Self::Running => "running",
            Self::Settlement => "settlement",
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EffectState {
    None,
    Possible,
    Reported,
}

impl fmt::Display for EffectState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::None => "none",
            Self::Possible => "possible",
            Self::Reported => "reported",
        })
    }
}

impl EffectState {
    pub const fn combine(self, other: Self) -> Self {
        match (self, other) {
            (Self::Reported, _) | (_, Self::Reported) => Self::Reported,
            (Self::Possible, _) | (_, Self::Possible) => Self::Possible,
            (Self::None, Self::None) => Self::None,
        }
    }
}

/// The longest retry delay this crate will carry.
///
/// A provider or limiter can report an absurd or hostile value, and a host
/// that sleeps on it without question stalls a worker indefinitely. Guidance
/// above this is clamped rather than dropped: the fact that waiting was
/// advised survives, the unbounded duration does not.
pub const MAX_RETRY_AFTER: Duration = Duration::from_secs(60 * 60);

/// A failure whose category and execution evidence survive middleware.
#[derive(Clone, Debug, thiserror::Error, PartialEq)]
#[error("{kind}: {message}")]
pub struct AgentError {
    pub kind: ErrorKind,
    pub message: String,
    pub phase: FailurePhase,
    pub effects: EffectState,
    pub evidence: Option<Box<FailureEvidence>>,
    /// How long a limiter or provider asked the caller to wait.
    ///
    /// This is timing, never permission. Whether an operation may be tried
    /// again at all is decided by [`AgentError::effects`]: guidance to wait
    /// thirty seconds says nothing about whether the first attempt already
    /// spent money or wrote files. A caller must satisfy both.
    ///
    /// Absent means nobody said, and is never guessed.
    pub retry_after: Option<Duration>,
    #[source]
    pub cause: Option<Box<AgentError>>,
}

impl AgentError {
    pub fn new(
        kind: ErrorKind,
        message: impl Into<String>,
        phase: FailurePhase,
        effects: EffectState,
    ) -> Self {
        Self {
            kind,
            message: message.into(),
            phase,
            effects,
            evidence: None,
            retry_after: None,
            cause: None,
        }
    }

    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(
            ErrorKind::InvalidRequest,
            message,
            FailurePhase::Validation,
            EffectState::None,
        )
    }

    /// The provider or a dependency is not serving.
    ///
    /// Nothing was launched, so this carries no effects and is safe to retry
    /// once whatever is down recovers.
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(
            ErrorKind::Unavailable,
            message,
            FailurePhase::Admission,
            EffectState::None,
        )
    }

    /// Record how long a limiter or provider asked the caller to wait.
    ///
    /// Clamped to [`MAX_RETRY_AFTER`]. This never makes an operation safe to
    /// retry; see [`AgentError::retry_after`].
    #[must_use]
    pub fn with_retry_after(mut self, after: Duration) -> Self {
        self.retry_after = Some(after.min(MAX_RETRY_AFTER));
        self
    }

    /// A turn cancelled before the provider was launched.
    ///
    /// Nothing ran, so this carries no effects. Every CLI-backed adapter
    /// produces this failure, and each producing its own risked them
    /// drifting apart: the same rejection reported with different phases is
    /// how a consumer ends up unable to tell a safe retry from an unsafe one.
    pub fn cancelled_before_launch(provider: &str) -> Self {
        Self::new(
            ErrorKind::Cancelled,
            format!("{provider} turn was cancelled before launch"),
            FailurePhase::Admission,
            EffectState::None,
        )
    }

    /// A turn cancelled while the provider was running.
    ///
    /// The turn may already have acted, so effects stay possible. This is the
    /// counterpart to [`AgentError::cancelled_before_launch`] and the pair
    /// only means anything if the distinction is kept.
    pub fn cancelled_in_flight(provider: &str) -> Self {
        Self::new(
            ErrorKind::Cancelled,
            format!("{provider} turn was cancelled"),
            FailurePhase::Running,
            EffectState::Possible,
        )
    }

    /// The provider could not be started.
    ///
    /// Launch failed, so nothing ran and nothing was spent.
    pub fn launch_failed(provider: &str) -> Self {
        Self::new(
            ErrorKind::Provider,
            format!("{provider} could not be initialized"),
            FailurePhase::Launch,
            EffectState::None,
        )
    }

    /// The provider process exited nonzero without a usable terminal result.
    ///
    /// It ran, so effects are possible. The exit code is the provider's own
    /// and carries none of its output.
    pub fn command_failed(provider: &str, exit_code: i32) -> Self {
        Self::new(
            ErrorKind::Provider,
            format!("{provider} command failed with exit code {exit_code}"),
            FailurePhase::Running,
            EffectState::Possible,
        )
    }

    pub fn busy() -> Self {
        Self::new(
            ErrorKind::Busy,
            "agent service is busy",
            FailurePhase::Admission,
            EffectState::None,
        )
    }

    pub fn deadline_exceeded(effects: EffectState) -> Self {
        Self::new(
            ErrorKind::DeadlineExceeded,
            "agent operation exceeded its deadline",
            FailurePhase::Running,
            effects,
        )
    }

    pub fn cancelled(effects: EffectState) -> Self {
        Self::new(
            ErrorKind::Cancelled,
            "agent operation was cancelled",
            FailurePhase::Running,
            effects,
        )
    }

    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new(
            ErrorKind::Unsupported,
            message,
            FailurePhase::Validation,
            EffectState::None,
        )
    }

    pub fn with_cause(mut self, cause: AgentError) -> Self {
        self.effects = self.effects.combine(cause.effects);
        // Guidance from the settled call is better than none from an outer
        // wrapper, but an outer layer that has its own stays authoritative.
        if self.retry_after.is_none() {
            self.retry_after = cause.retry_after;
        }
        if let Some(cause_evidence) = cause.evidence.as_deref() {
            match self.evidence.as_deref_mut() {
                Some(evidence) => evidence.merge_missing(cause_evidence),
                None => self.evidence = Some(Box::new(cause_evidence.clone())),
            }
        }
        self.cause = Some(Box::new(cause));
        self
    }

    pub fn with_evidence(mut self, evidence: FailureEvidence) -> Self {
        self.evidence = Some(Box::new(evidence));
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// These four exist so every adapter reports the same rejection the same
    /// way. The classification is the point, not the wording: a consumer
    /// deciding whether a retry is safe reads phase and effects.
    #[test]
    fn the_shared_terminal_failures_classify_consistently() {
        let before = AgentError::cancelled_before_launch("Claude");
        assert_eq!(before.phase, FailurePhase::Admission);
        assert_eq!(before.effects, EffectState::None);

        let in_flight = AgentError::cancelled_in_flight("Codex");
        assert_eq!(in_flight.phase, FailurePhase::Running);
        // The turn may already have acted; this is the whole difference from
        // the pre-launch case.
        assert_eq!(in_flight.effects, EffectState::Possible);
        assert_eq!(before.kind, in_flight.kind);

        let launch = AgentError::launch_failed("Codex");
        assert_eq!(launch.phase, FailurePhase::Launch);
        assert_eq!(launch.effects, EffectState::None);

        let failed = AgentError::command_failed("Claude", 37);
        assert_eq!(failed.phase, FailurePhase::Running);
        assert_eq!(failed.effects, EffectState::Possible);
        assert!(failed.message.contains("37"));
    }

    /// The display name is prose. The tag a session carries is not, and the
    /// two are deliberately separate in the adapters.
    #[test]
    fn the_provider_name_reaches_the_message() {
        assert!(
            AgentError::cancelled_before_launch("Claude")
                .message
                .starts_with("Claude")
        );
        assert!(
            AgentError::cancelled_before_launch("Codex")
                .message
                .starts_with("Codex")
        );
    }

    #[test]
    fn host_capacity_and_provider_outage_are_distinguishable() {
        // The distinction the vocabulary exists for: both refuse before
        // launch with no effects, but one says "not here, now" and the other
        // says "not this provider". A caller with a second provider should
        // act differently on each.
        let busy = AgentError::busy();
        let down = AgentError::unavailable("circuit is open");

        assert_ne!(busy.kind, down.kind);
        assert_eq!(busy.phase, down.phase);
        assert_eq!(busy.effects, EffectState::None);
        assert_eq!(down.effects, EffectState::None);
    }

    #[test]
    fn retry_guidance_is_absent_until_someone_supplies_it() {
        assert_eq!(AgentError::busy().retry_after, None);
    }

    #[test]
    fn retry_guidance_is_clamped_rather_than_believed() {
        let hostile = AgentError::busy().with_retry_after(Duration::from_secs(86_400));
        assert_eq!(hostile.retry_after, Some(MAX_RETRY_AFTER));

        let ordinary = AgentError::busy().with_retry_after(Duration::from_secs(30));
        assert_eq!(ordinary.retry_after, Some(Duration::from_secs(30)));
    }

    #[test]
    fn guidance_says_nothing_about_whether_a_retry_is_safe() {
        // Timing and permission are independent. A provider can ask a caller
        // to come back in a minute after a turn that already spent money and
        // wrote files, and honoring the delay must not be read as consent to
        // replay the work.
        let effectful = AgentError::new(
            ErrorKind::Limit,
            "quota exhausted mid-turn",
            FailurePhase::Running,
            EffectState::Reported,
        )
        .with_retry_after(Duration::from_secs(60));

        assert_eq!(effectful.retry_after, Some(Duration::from_secs(60)));
        assert_eq!(effectful.effects, EffectState::Reported);
    }

    #[test]
    fn an_outer_error_adopts_guidance_from_its_cause() {
        let outer = AgentError::deadline_exceeded(EffectState::Possible)
            .with_cause(AgentError::busy().with_retry_after(Duration::from_secs(5)));
        assert_eq!(outer.retry_after, Some(Duration::from_secs(5)));
    }

    #[test]
    fn an_outer_error_keeps_its_own_guidance_over_a_cause() {
        let outer = AgentError::busy()
            .with_retry_after(Duration::from_secs(2))
            .with_cause(AgentError::busy().with_retry_after(Duration::from_secs(90)));
        assert_eq!(outer.retry_after, Some(Duration::from_secs(2)));
    }
}
