use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorKind {
    InvalidRequest,
    Authentication,
    Unauthorized,
    Unsupported,
    Busy,
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

/// A failure whose category and execution evidence survive middleware.
#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
#[error("{kind}: {message}")]
pub struct AgentError {
    pub kind: ErrorKind,
    pub message: String,
    pub phase: FailurePhase,
    pub effects: EffectState,
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
        self.cause = Some(Box::new(cause));
        self
    }
}
