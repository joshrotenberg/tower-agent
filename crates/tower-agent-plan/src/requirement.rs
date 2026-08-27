use serde::{Deserialize, Serialize};

use crate::ProviderId;

/// Stable identifiers for the requirements the shared resolver can emit.
///
/// Provider planners add provider-conditional requirements with their own
/// stable identifiers.
pub mod ids {
    /// The provider to run the turn on.
    pub const PROVIDER: &str = "provider";
    /// The user prompt for the turn.
    pub const PROMPT: &str = "prompt";
}

/// A structured description of one unresolved value.
///
/// Requirements are adapter-neutral data. A CLI may render them as missing
/// flags, an MCP adapter as elicitation, a UI as form fields; none of those
/// choices belongs here. The requirements remaining after resolution are the
/// effective callable signature of a partial turn or profile.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Requirement {
    /// Stable identifier answers refer back to.
    pub id: String,
    /// Canonical dotted parameter path, such as `context.cwd`.
    pub path: String,
    /// The kind of value that satisfies the requirement.
    pub kind: ValueKind,
    /// Concise human label.
    pub label: String,
    /// Why the value is required.
    pub reason: RequirementReason,
    /// Whether the value is unsuitable for ordinary prompting or elicitation.
    /// Secrets are never requirements; they belong to configured environment
    /// and authentication mechanisms.
    pub sensitive: bool,
    /// Provider context when the requirement is provider-conditional.
    pub provider: Option<ProviderId>,
}

/// The kind of value a requirement accepts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValueKind {
    /// A known provider name.
    Provider,
    /// Freeform text.
    Text,
}

/// Why a requirement exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequirementReason {
    /// No layer selected a provider.
    ProviderSelection,
    /// The provider needs this input to run at all.
    ProviderInput,
}

/// One elicited answer, keyed by the id of a previously emitted requirement.
///
/// Answers fill only paths still unbound after every layer; an answer for an
/// unknown requirement or an already-bound path is invalid, never an
/// override.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Answer {
    /// Id of the requirement this answers.
    pub id: String,
    /// The supplied value, still unvalidated.
    pub value: String,
}
