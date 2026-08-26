//! Codex provider planner.
//!
//! Folds a complete resolution into a concrete `Turn<CodexOptions>`. The fold
//! is honor-or-refuse: a resolved setting the fold cannot represent becomes a
//! diagnostic, never a silent drop or narrowing. The adapter re-validates
//! every control at launch; the cheap adapter invariants that need no
//! provider machinery are mirrored here so refusal happens before a service
//! is involved. Deeper validation parity, such as output-schema checking,
//! arrives with the adapter preflight seam.

use serde::{Deserialize, Serialize};
use tower_agent::Turn;
use tower_agent_codex::CodexOptions;

use crate::diagnostic::{Diagnostic, codes};
use crate::provider::ProviderId;
use crate::resolve::ResolvedTurn;

/// Codex settings a planning layer may bind.
///
/// Only settings the shared vocabulary cannot express appear here. Model
/// name, working directory, additional directories, resume, and filesystem
/// authority fold from the shared groups into [`CodexOptions`], so no
/// concrete option field has two planning paths.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PartialCodexOptions {
    /// Instructions prepended to the user prompt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    /// JSON Schema for the structured final response. A bound schema
    /// replaces lower layers whole.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<serde_json::Value>,
    /// Do not persist the turn into resumable rollout history.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ephemeral: Option<bool>,
}

impl PartialCodexOptions {
    pub(crate) fn merge_from(&mut self, layer: &Self) {
        if layer.system_prompt.is_some() {
            self.system_prompt.clone_from(&layer.system_prompt);
        }
        if layer.output_schema.is_some() {
            self.output_schema.clone_from(&layer.output_schema);
        }
        if layer.ephemeral.is_some() {
            self.ephemeral = layer.ephemeral;
        }
    }
}

/// Fold a complete resolution into a concrete Codex turn.
pub fn plan(resolved: &ResolvedTurn) -> Result<Turn<CodexOptions>, Vec<Diagnostic>> {
    if resolved.provider() != ProviderId::Codex {
        return Err(vec![Diagnostic::error(
            codes::UNSUPPORTED_PROVIDER,
            Some("provider"),
            format!(
                "the codex planner cannot plan provider {}",
                resolved.provider()
            ),
        )]);
    }

    let partial = resolved.partial();
    let additional_directories = partial
        .context
        .additional_directories
        .clone()
        .unwrap_or_default();

    let mut diagnostics = Vec::new();
    if partial.context.resume.is_some() && !additional_directories.is_empty() {
        diagnostics.push(Diagnostic::error(
            codes::RESUMED_ADDITIONAL_DIRECTORIES,
            Some("context.additional_directories"),
            "codex cannot add directories to a resumed turn",
        ));
    }
    if !diagnostics.is_empty() {
        return Err(diagnostics);
    }

    let options_layer = partial.provider_options.codex.clone().unwrap_or_default();
    let mut turn = Turn::new(resolved.prompt()).with_options(CodexOptions {
        system_prompt: options_layer.system_prompt,
        model: partial.model.name.clone(),
        additional_directories,
        output_schema: options_layer.output_schema,
        filesystem_authority: partial
            .permissions
            .filesystem
            .map(Into::into)
            .unwrap_or_default(),
        ephemeral: options_layer.ephemeral.unwrap_or_default(),
    });
    if let Some(cwd) = partial.context.cwd.clone() {
        turn = turn.in_directory(cwd);
    }
    if let Some(resume) = &partial.context.resume {
        turn = turn.resume(resume.to_session_handle());
    }
    Ok(turn)
}
