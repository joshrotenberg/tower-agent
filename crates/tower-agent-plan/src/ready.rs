use crate::diagnostic::{Diagnostic, codes};
use crate::partial::PartialTurn;
use crate::requirement::Requirement;
use crate::resolve::{Layers, Resolution, ResolvedTurn, resolve};

/// A provider-committed, fully typed portable turn body.
///
/// One variant exists per enabled provider feature. The enum is
/// non-exhaustive because providers are an open set: a future provider,
/// including a REST-backed one, adds an options type, a service implementing
/// the same kernel contract, a planner fold, and a variant here, without
/// changing the planning pipeline.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum ReadyTurn {
    #[cfg(feature = "claude")]
    /// A turn compiled for the Claude adapter.
    Claude(tower_agent::Turn<tower_agent_claude::ClaudeOptions>),
    #[cfg(feature = "codex")]
    /// A turn compiled for the Codex adapter.
    Codex(tower_agent::Turn<tower_agent_codex::CodexOptions>),
}

impl ReadyTurn {
    /// The provider this turn is committed to.
    #[cfg(any(feature = "claude", feature = "codex"))]
    pub fn provider(&self) -> crate::provider::ProviderId {
        match self {
            #[cfg(feature = "claude")]
            Self::Claude(_) => crate::provider::ProviderId::Claude,
            #[cfg(feature = "codex")]
            Self::Codex(_) => crate::provider::ProviderId::Codex,
        }
    }

    /// The provider this turn is committed to.
    ///
    /// With no provider feature enabled the type has no variants, so no
    /// value of it can exist to call this on.
    #[cfg(not(any(feature = "claude", feature = "codex")))]
    pub fn provider(&self) -> crate::provider::ProviderId {
        match *self {}
    }
}

/// Compile a complete resolution into a provider-committed turn.
///
/// A provider without an enabled planner is an `unsupported-provider`
/// diagnostic, never a fallback to another provider.
pub fn compile(resolved: &ResolvedTurn) -> Result<ReadyTurn, Vec<Diagnostic>> {
    match resolved.provider() {
        #[cfg(feature = "claude")]
        crate::provider::ProviderId::Claude => crate::claude::plan(resolved).map(ReadyTurn::Claude),
        #[cfg(feature = "codex")]
        crate::provider::ProviderId::Codex => crate::codex::plan(resolved).map(ReadyTurn::Codex),
        // Reachable only in builds with a provider feature disabled;
        // providers stay an open set.
        #[allow(unreachable_patterns)]
        other => Err(vec![Diagnostic::error(
            codes::UNSUPPORTED_PROVIDER,
            Some("provider"),
            format!("no planner is enabled for provider {other}"),
        )]),
    }
}

/// The outcome of resolve-then-compile.
#[derive(Clone, Debug, PartialEq)]
pub enum Prepared {
    /// The turn compiled and is ready to run.
    Ready(ReadyTurn),
    /// Coherent but incomplete. Elicit the requirements and resolve again.
    Missing {
        /// The merged view the requirements were derived from.
        resolved: PartialTurn,
        /// Unresolved values in deterministic order.
        requirements: Vec<Requirement>,
    },
    /// Refused. Every diagnostic is an error, and none is elicitable.
    Invalid {
        /// What was wrong, in deterministic order.
        diagnostics: Vec<Diagnostic>,
    },
}

/// Resolve the layers, then compile the result when it is complete.
///
/// This is the ordinary high-level entry point. [`resolve`] and [`compile`]
/// stay independently callable and testable.
///
/// # Example
///
/// ```
/// use tower_agent_plan::{Layers, PartialTurn, Prepared, prepare};
///
/// let explicit = PartialTurn::default();
/// let Prepared::Missing { requirements, .. } = prepare(Layers::new(&explicit)) else {
///     panic!("nothing is bound yet");
/// };
/// assert_eq!(requirements[0].id, "provider");
/// ```
pub fn prepare(layers: Layers<'_>) -> Prepared {
    match resolve(layers) {
        Resolution::Complete(resolved) => match compile(&resolved) {
            Ok(turn) => Prepared::Ready(turn),
            Err(diagnostics) => Prepared::Invalid { diagnostics },
        },
        Resolution::Missing {
            resolved,
            requirements,
        } => Prepared::Missing {
            resolved,
            requirements,
        },
        Resolution::Invalid { diagnostics } => Prepared::Invalid { diagnostics },
    }
}
