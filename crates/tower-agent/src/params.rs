//! The call and its resolved parameters.
//!
//! A [`Call`] is the raw `prompt` tool input: a required `prompt` plus optional
//! overrides, and an optional `agent` selecting a configured profile. A [`Params`]
//! is what a backend actually runs: the call merged over the agent's defaults
//! over the server defaults (see [`crate::config::Config::resolve`]).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A raw call to the `prompt` tool. Only `prompt` is required; every other field
/// mirrors a backend parameter and is optional, filled from config when omitted.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct Call {
    /// The task or message to run. Required.
    pub prompt: String,
    /// Select a configured agent, using its defaults for the fields below.
    #[serde(default)]
    pub agent: Option<String>,
    /// The system prompt (the agent's instructions), overriding the agent's own.
    #[serde(default)]
    pub system: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    /// low | medium | high.
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub allowed_tools: Option<Vec<String>>,
    /// Working directory for the run.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Continue this session (thread) so the backend resumes; omit to start fresh.
    #[serde(default)]
    pub session: Option<String>,
}

/// The resolved parameters a backend runs.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Params {
    pub prompt: String,
    pub system: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub allowed_tools: Option<Vec<String>>,
    pub cwd: Option<String>,
    pub session: Option<String>,
    /// The environment the backend runs in (its `CLAUDE_CONFIG_DIR`), if any.
    pub config_dir: Option<String>,
}
