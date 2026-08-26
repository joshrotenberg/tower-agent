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
    #[cfg(feature = "codex")]
    Codex(tower_agent::Turn<tower_agent_codex::CodexOptions>),
}

/// Compile a complete resolution into a provider-committed turn.
///
/// A provider without an enabled planner is an `unsupported-provider`
/// diagnostic, never a fallback to another provider.
pub fn compile(resolved: &ResolvedTurn) -> Result<ReadyTurn, Vec<Diagnostic>> {
    match resolved.provider() {
        #[cfg(feature = "codex")]
        crate::provider::ProviderId::Codex => crate::codex::plan(resolved).map(ReadyTurn::Codex),
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
    Ready(ReadyTurn),
    Missing {
        /// The merged view the requirements were derived from.
        resolved: PartialTurn,
        /// Unresolved values in deterministic order.
        requirements: Vec<Requirement>,
    },
    Invalid {
        diagnostics: Vec<Diagnostic>,
    },
}

/// Resolve the layers, then compile the result when it is complete.
///
/// This is the ordinary high-level entry point. [`resolve`] and [`compile`]
/// stay independently callable and testable.
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
