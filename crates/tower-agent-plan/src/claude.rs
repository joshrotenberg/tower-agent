//! Claude provider planner.
//!
//! Folds a complete resolution into a concrete `Turn<ClaudeOptions>`. The
//! fold is honor-or-refuse: a resolved setting the fold cannot represent
//! becomes a diagnostic, never a silent drop or narrowing. The portable
//! filesystem-authority request is refused here because the Claude adapter
//! does not claim that contract; tool patterns are provider-specific
//! controls, not a portable sandbox. The adapter re-validates every control
//! at launch, and deeper validation parity arrives with the adapter
//! preflight seam.

use serde::{Deserialize, Serialize};
use tower_agent::Turn;
use tower_agent_claude::{
    ClaudeAmbientContext, ClaudeEffort, ClaudeHermetic, ClaudeOptions, ClaudePermissionMode,
};

use crate::diagnostic::{Diagnostic, codes};
use crate::provider::ProviderId;
use crate::resolve::ResolvedTurn;

/// Claude settings a planning layer may bind.
///
/// Only settings the shared vocabulary cannot express appear here. Model
/// name, working directory, additional directories, and resume fold from the
/// shared groups into [`ClaudeOptions`], so no concrete option field has two
/// planning paths.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PartialClaudeOptions {
    /// Replace the system prompt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    /// Append to the system prompt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub append_system_prompt: Option<String>,
    /// Model to fall back to when the primary model is overloaded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<EffortChoice>,
    /// Tool allowlist. A bound list replaces lower layers whole.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_tools: Option<Vec<String>>,
    /// Tool denylist. A bound list replaces lower layers whole.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disallowed_tools: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<u32>,
    /// CLI-side spend ceiling for the turn, in USD.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_budget_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<PermissionModeChoice>,
    /// JSON Schema the terminal result must validate against. A bound schema
    /// replaces lower layers whole.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub json_schema: Option<serde_json::Value>,
    /// Ignore all configured MCP servers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strict_mcp_config: Option<bool>,
    /// Requested ambient-context mode. The service's host-owned baseline is
    /// always applied and cannot be weakened by a turn.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ambient_context: Option<AmbientContextChoice>,
}

impl PartialClaudeOptions {
    pub(crate) fn merge_from(&mut self, layer: &Self) {
        if layer.system_prompt.is_some() {
            self.system_prompt.clone_from(&layer.system_prompt);
        }
        if layer.append_system_prompt.is_some() {
            self.append_system_prompt
                .clone_from(&layer.append_system_prompt);
        }
        if layer.fallback_model.is_some() {
            self.fallback_model.clone_from(&layer.fallback_model);
        }
        if layer.effort.is_some() {
            self.effort = layer.effort;
        }
        if layer.allowed_tools.is_some() {
            self.allowed_tools.clone_from(&layer.allowed_tools);
        }
        if layer.disallowed_tools.is_some() {
            self.disallowed_tools.clone_from(&layer.disallowed_tools);
        }
        if layer.max_turns.is_some() {
            self.max_turns = layer.max_turns;
        }
        if layer.max_budget_usd.is_some() {
            self.max_budget_usd = layer.max_budget_usd;
        }
        if layer.permission_mode.is_some() {
            self.permission_mode = layer.permission_mode;
        }
        if layer.json_schema.is_some() {
            self.json_schema.clone_from(&layer.json_schema);
        }
        if layer.strict_mcp_config.is_some() {
            self.strict_mcp_config = layer.strict_mcp_config;
        }
        if layer.ambient_context.is_some() {
            self.ambient_context = layer.ambient_context;
        }
    }
}

/// Wire form of the adapter's effort control.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffortChoice {
    Low,
    Medium,
    High,
}

impl From<EffortChoice> for ClaudeEffort {
    fn from(choice: EffortChoice) -> Self {
        match choice {
            EffortChoice::Low => Self::Low,
            EffortChoice::Medium => Self::Medium,
            EffortChoice::High => Self::High,
        }
    }
}

/// Wire form of the adapter's permission posture. Bypass-all is absent here
/// because the adapter deliberately does not expose it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionModeChoice {
    Default,
    AcceptEdits,
    DontAsk,
    Plan,
    Auto,
}

impl From<PermissionModeChoice> for ClaudePermissionMode {
    fn from(choice: PermissionModeChoice) -> Self {
        match choice {
            PermissionModeChoice::Default => Self::Default,
            PermissionModeChoice::AcceptEdits => Self::AcceptEdits,
            PermissionModeChoice::DontAsk => Self::DontAsk,
            PermissionModeChoice::Plan => Self::Plan,
            PermissionModeChoice::Auto => Self::Auto,
        }
    }
}

/// Wire form of the adapter's mutually exclusive ambient-context modes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AmbientContextChoice {
    Inherit,
    HermeticProject,
    HermeticFull,
    Safe,
    Bare,
}

impl From<AmbientContextChoice> for ClaudeAmbientContext {
    fn from(choice: AmbientContextChoice) -> Self {
        match choice {
            AmbientContextChoice::Inherit => Self::Inherit,
            AmbientContextChoice::HermeticProject => Self::Hermetic(ClaudeHermetic::Project),
            AmbientContextChoice::HermeticFull => Self::Hermetic(ClaudeHermetic::Full),
            AmbientContextChoice::Safe => Self::Safe,
            AmbientContextChoice::Bare => Self::Bare,
        }
    }
}

/// Check a folded turn against one configured service without launching.
///
/// [`crate::compile`] proves the fold is representable; this proves the
/// specific configured service will not refuse the turn during its
/// validation phase (ambient-context baseline combination and every option
/// check). An adapter refusal becomes an `adapter-refusal` diagnostic.
///
/// # Example
///
/// ```
/// use tower_agent::Turn;
/// use tower_agent_claude::{ClaudeOptions, ClaudeService};
///
/// let service = ClaudeService::new();
/// let turn = Turn::new("inspect this repository").with_options(ClaudeOptions::default());
/// assert!(tower_agent_plan::claude::preflight(&service, &turn).is_ok());
/// ```
pub fn preflight(
    service: &tower_agent_claude::ClaudeService,
    turn: &Turn<ClaudeOptions>,
) -> Result<(), Diagnostic> {
    service
        .preflight(turn)
        .map_err(crate::diagnostic::adapter_refusal)
}

/// Fold a complete resolution into a concrete Claude turn.
pub fn plan(resolved: &ResolvedTurn) -> Result<Turn<ClaudeOptions>, Vec<Diagnostic>> {
    if resolved.provider() != ProviderId::Claude {
        return Err(vec![Diagnostic::error(
            codes::UNSUPPORTED_PROVIDER,
            Some("provider"),
            format!(
                "the claude planner cannot plan provider {}",
                resolved.provider()
            ),
        )]);
    }

    let partial = resolved.partial();
    if partial.permissions.filesystem.is_some() {
        return Err(vec![Diagnostic::error(
            codes::UNSUPPORTED_FILESYSTEM_PERMISSION,
            Some("permissions.filesystem"),
            "the Claude adapter does not claim the portable filesystem-authority contract",
        )]);
    }

    let options_layer = partial.provider_options.claude.clone().unwrap_or_default();
    let json_schema = options_layer
        .json_schema
        .map(|schema| serde_json::to_string(&schema).expect("a JSON value serializes"));

    let mut turn = Turn::new(resolved.prompt()).with_options(ClaudeOptions {
        system_prompt: options_layer.system_prompt,
        append_system_prompt: options_layer.append_system_prompt,
        model: partial.model.name.clone(),
        fallback_model: options_layer.fallback_model,
        effort: options_layer.effort.map(Into::into),
        allowed_tools: options_layer.allowed_tools.unwrap_or_default(),
        disallowed_tools: options_layer.disallowed_tools.unwrap_or_default(),
        additional_directories: partial
            .context
            .additional_directories
            .clone()
            .unwrap_or_default(),
        max_turns: options_layer.max_turns,
        max_budget_usd: options_layer.max_budget_usd,
        permission_mode: options_layer.permission_mode.map(Into::into),
        json_schema,
        strict_mcp_config: options_layer.strict_mcp_config.unwrap_or_default(),
        ambient_context: options_layer
            .ambient_context
            .map(Into::into)
            .unwrap_or_default(),
    });
    if let Some(cwd) = partial.context.cwd.clone() {
        turn = turn.in_directory(cwd);
    }
    if let Some(resume) = &partial.context.resume {
        turn = turn.resume(resume.to_session_handle());
    }
    Ok(turn)
}
