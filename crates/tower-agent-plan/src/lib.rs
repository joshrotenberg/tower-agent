//! Layered planning for tower-agent turns.
//!
//! This crate turns partially specified turn data into a complete, typed turn
//! body, a structured list of missing requirements, or structured
//! diagnostics. It is the pure front half of one agent call; the kernel and
//! the provider adapters remain the effectful back half.
//!
//! ```text
//! PartialTurn layers
//!     -> resolve
//! Complete | Missing(requirements) | Invalid(diagnostics)
//!     -> provider planner fold
//!     -> adapter preflight (optional, per configured service)
//! ReadyTurn (a provider-committed Turn<O>)
//!     -> RoutedTurnService
//! tower-agent middleware and the provider service
//! ```
//!
//! The compile target of planning is the typed portable turn body, never a
//! process specification. Argv construction, environment policy, process
//! execution, cancellation, and settlement belong to the provider adapters
//! and the kernel middleware; this crate launches nothing and owns no
//! post-execution behavior.
//!
//! # Feature flags
//!
//! Provider planners are opt-in features, each pulling in only its own
//! adapter crate:
//!
//! - `claude`: the Claude planner and [`PartialClaudeOptions`].
//! - `codex`: the Codex planner and [`PartialCodexOptions`].
//!
//! [`RoutedTurnService`] and [`ReadyTurn`]'s variants require at least one of
//! them: with no provider feature there is no provider to route to.
//!
//! With no provider features enabled the crate still resolves, and
//! [`compile`] refuses every provider with an `unsupported-provider`
//! diagnostic.
//!
//! # Precedence and merge laws
//!
//! Layers merge from lowest to highest precedence: provider baseline
//! defaults, application defaults, the selected profile, the explicit
//! request, and elicited answers for paths still unbound.
//!
//! - A later bound scalar replaces an earlier scalar.
//! - Nested groups merge by field, not by group replacement.
//! - A bound list replaces lower layers whole; a bound empty list is a real
//!   value.
//! - Omission means no binding from that layer. Empty strings, empty lists,
//!   and `false` are bindings.
//! - Answers fill only paths still unbound after every layer. Answering a
//!   bound path or an unknown requirement is invalid.
//! - A profile/explicit provider mismatch is invalid, never an implicit
//!   conversion, and the provider is not inferred from a resume tag.
//! - A provider baseline cannot select or change the provider.
//! - Invalid bound values take priority over eliciting more values.
//!
//! # Example
//!
//! ```
//! use tower_agent_plan::{Answer, Layers, PartialTurn, Profile, ProviderId, Resolution, resolve};
//!
//! let profile = Profile {
//!     name: "careful-codex".to_string(),
//!     turn: PartialTurn {
//!         provider: Some(ProviderId::Codex),
//!         ..Default::default()
//!     },
//! };
//!
//! // The profile's effective signature is the requirements that remain.
//! let explicit = PartialTurn::default();
//! let pass = resolve(Layers::new(&explicit).with_profile(&profile));
//! let Resolution::Missing { requirements, .. } = pass else {
//!     panic!("expected missing requirements");
//! };
//! assert_eq!(requirements.len(), 1);
//! assert_eq!(requirements[0].id, "prompt");
//!
//! // Supplying the answers completes the same pass deterministically.
//! let answers = [Answer {
//!     id: "prompt".to_string(),
//!     value: "inspect this repository".to_string(),
//! }];
//! let pass = resolve(
//!     Layers::new(&explicit)
//!         .with_profile(&profile)
//!         .with_answers(&answers),
//! );
//! let Resolution::Complete(resolved) = pass else {
//!     panic!("expected a complete resolution");
//! };
//! assert_eq!(resolved.provider(), ProviderId::Codex);
//! assert_eq!(resolved.prompt(), "inspect this repository");
//! ```

#[cfg(feature = "claude")]
pub mod claude;
#[cfg(feature = "codex")]
pub mod codex;
mod diagnostic;
mod partial;
mod provider;
mod ready;
mod requirement;
mod resolve;
#[cfg(any(feature = "claude", feature = "codex"))]
mod router;

#[cfg(feature = "claude")]
pub use claude::PartialClaudeOptions;
#[cfg(feature = "codex")]
pub use codex::PartialCodexOptions;
pub use diagnostic::{Diagnostic, Severity, codes as diagnostic_codes};
pub use partial::{
    FilesystemChoice, PartialContext, PartialModel, PartialPermissions, PartialProviderOptions,
    PartialTurn, Profile, ResumeBinding,
};
pub use provider::{ProviderId, UnknownProvider};
pub use ready::{Prepared, ReadyTurn, compile, prepare};
pub use requirement::{Answer, Requirement, RequirementReason, ValueKind, ids as requirement_ids};
pub use resolve::{Layers, ProviderDefaults, Resolution, ResolvedTurn, resolve};
#[cfg(any(feature = "claude", feature = "codex"))]
pub use router::RoutedTurnService;
