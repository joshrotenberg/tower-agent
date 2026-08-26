use serde::{Deserialize, Serialize};

/// Stable diagnostic codes emitted by the shared resolver.
///
/// Provider planners add provider-conditional codes with the same stability
/// contract: a code is an identifier tests and adapters may match on, not a
/// message.
pub mod codes {
    pub const PROVIDER_MISMATCH: &str = "provider-mismatch";
    pub const RESUME_PROVIDER_MISMATCH: &str = "resume-provider-mismatch";
    pub const BLANK_PROMPT: &str = "blank-prompt";
    pub const BLANK_MODEL: &str = "blank-model";
    pub const EMPTY_RESUME_VALUE: &str = "empty-resume-value";
    pub const HYPHEN_RESUME_VALUE: &str = "hyphen-resume-value";
    pub const UNKNOWN_REQUIREMENT: &str = "unknown-requirement";
    pub const ANSWER_OVERRIDES_BOUND_PATH: &str = "answer-overrides-bound-path";
    pub const INVALID_PROVIDER_ANSWER: &str = "invalid-provider-answer";
    pub const UNSUPPORTED_PROVIDER: &str = "unsupported-provider";
    pub const RESUMED_ADDITIONAL_DIRECTORIES: &str = "resumed-additional-directories";
    pub const UNSUPPORTED_FILESYSTEM_PERMISSION: &str = "unsupported-filesystem-permission";
}

/// One structured planning diagnostic.
///
/// Diagnostics never contain resume values, prompts, or other bound data;
/// they name paths and state facts about them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    /// Stable machine-readable code from [`codes`].
    pub code: String,
    pub severity: Severity,
    /// Canonical dotted parameter path when the diagnostic concerns one.
    pub path: Option<String>,
    pub message: String,
}

impl Diagnostic {
    pub fn error(code: &str, path: Option<&str>, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            severity: Severity::Error,
            path: path.map(str::to_string),
            message: message.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Warning,
    Error,
}
