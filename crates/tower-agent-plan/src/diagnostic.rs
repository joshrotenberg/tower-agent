use serde::{Deserialize, Serialize};

/// Stable diagnostic codes emitted by the shared resolver.
///
/// Provider planners add provider-conditional codes with the same stability
/// contract: a code is an identifier tests and adapters may match on, not a
/// message.
pub mod codes {
    /// The named provider is not one this build supports.
    pub const PROVIDER_MISMATCH: &str = "provider-mismatch";
    /// A resume binding names a different provider than the turn.
    pub const RESUME_PROVIDER_MISMATCH: &str = "resume-provider-mismatch";
    /// The bound prompt is empty or only whitespace.
    pub const BLANK_PROMPT: &str = "blank-prompt";
    /// The bound model is empty or only whitespace.
    pub const BLANK_MODEL: &str = "blank-model";
    /// A resume binding carries an empty value.
    pub const EMPTY_RESUME_VALUE: &str = "empty-resume-value";
    /// A resume value starts with `-` and would be read as a flag.
    pub const HYPHEN_RESUME_VALUE: &str = "hyphen-resume-value";
    /// An answer names a requirement that was never emitted.
    pub const UNKNOWN_REQUIREMENT: &str = "unknown-requirement";
    /// An answer targets a path an earlier layer already bound.
    pub const ANSWER_OVERRIDES_BOUND_PATH: &str = "answer-overrides-bound-path";
    /// A provider answer is not a recognized provider name.
    pub const INVALID_PROVIDER_ANSWER: &str = "invalid-provider-answer";
    /// No planner is enabled for the selected provider.
    pub const UNSUPPORTED_PROVIDER: &str = "unsupported-provider";
    /// A resumed turn also requests additional directories.
    pub const RESUMED_ADDITIONAL_DIRECTORIES: &str = "resumed-additional-directories";
    /// The requested filesystem permission has no provider equivalent.
    pub const UNSUPPORTED_FILESYSTEM_PERMISSION: &str = "unsupported-filesystem-permission";
    /// A provider adapter refused the compiled turn.
    pub const ADAPTER_REFUSAL: &str = "adapter-refusal";
}

/// Translate an adapter preflight refusal into a planning diagnostic.
///
/// The adapter's typed error stays the source of truth; the diagnostic
/// carries its category and fixed message text, which the adapters already
/// keep free of prompts, session values, and other bound data.
#[cfg(any(feature = "claude", feature = "codex"))]
pub(crate) fn adapter_refusal(error: tower_agent::AgentError) -> Diagnostic {
    Diagnostic::error(
        codes::ADAPTER_REFUSAL,
        None,
        format!("adapter refused the turn: {error}"),
    )
}

/// One structured planning diagnostic.
///
/// Diagnostics never contain resume values, prompts, or other bound data;
/// they name paths and state facts about them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    /// Stable machine-readable code from [`codes`].
    pub code: String,
    /// Whether this stops resolution or only annotates it.
    pub severity: Severity,
    /// Canonical dotted parameter path when the diagnostic concerns one.
    pub path: Option<String>,
    /// Human-readable explanation. Never carries bound values.
    pub message: String,
}

impl Diagnostic {
    /// A diagnostic that stops resolution.
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
/// Whether a diagnostic blocks resolution.
pub enum Severity {
    /// Annotates the result without blocking it.
    Warning,
    /// Blocks resolution. Any error makes a pass `Invalid`.
    Error,
}
