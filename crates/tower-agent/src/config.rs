//! Config: server defaults and named agents, and how a call resolves against them.
//!
//! An **agent** is a named bundle of default parameters plus a base prompt (its
//! `system`). Nothing here is code. [`Config::resolve`] turns a [`Call`] into the
//! [`Params`] a backend runs, most specific wins: the call's explicit fields,
//! then the selected agent's defaults, then the server defaults.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;

use crate::params::{Call, Params};

/// The whole configuration: server defaults plus named agents.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub defaults: Defaults,
    #[serde(default)]
    pub agents: BTreeMap<String, AgentDef>,
}

/// Server-wide defaults, the least specific layer.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Defaults {
    pub model: Option<String>,
    pub effort: Option<String>,
    pub cwd: Option<String>,
    /// A shared environment (`CLAUDE_CONFIG_DIR`) unless an agent overrides it.
    pub config_dir: Option<String>,
}

/// A named agent: its instructions plus default parameters.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentDef {
    /// The agent's instructions, used as the system prompt.
    pub system: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub allowed_tools: Option<Vec<String>>,
    pub cwd: Option<String>,
    /// This agent's own environment (`CLAUDE_CONFIG_DIR`).
    pub config_dir: Option<String>,
}

impl Config {
    /// Load config from a TOML file.
    pub fn load(path: &Path) -> anyhow::Result<Config> {
        let text = std::fs::read_to_string(path)?;
        Config::parse(&text)
    }

    /// Parse config from a TOML string.
    pub fn parse(text: &str) -> anyhow::Result<Config> {
        Ok(toml::from_str(text)?)
    }

    /// The configured agent names.
    pub fn agent_names(&self) -> impl Iterator<Item = &String> {
        self.agents.keys()
    }

    /// Resolve a call into the parameters a backend runs: the call over the
    /// selected agent's defaults over the server defaults.
    pub fn resolve(&self, call: Call) -> Params {
        let agent = call.agent.as_ref().and_then(|a| self.agents.get(a));
        Params {
            prompt: call.prompt,
            system: call.system.or_else(|| agent.and_then(|a| a.system.clone())),
            model: call
                .model
                .or_else(|| agent.and_then(|a| a.model.clone()))
                .or_else(|| self.defaults.model.clone()),
            effort: call
                .effort
                .or_else(|| agent.and_then(|a| a.effort.clone()))
                .or_else(|| self.defaults.effort.clone()),
            allowed_tools: call
                .allowed_tools
                .or_else(|| agent.and_then(|a| a.allowed_tools.clone())),
            cwd: call
                .cwd
                .or_else(|| agent.and_then(|a| a.cwd.clone()))
                .or_else(|| self.defaults.cwd.clone()),
            session: call.session,
            config_dir: agent
                .and_then(|a| a.config_dir.clone())
                .or_else(|| self.defaults.config_dir.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config::parse(
            r#"
            [defaults]
            model = "sonnet"
            effort = "medium"

            [agents.tester]
            system = "You run the tests."
            model = "haiku"
            config_dir = ".agent/env/tester"
            "#,
        )
        .unwrap()
    }

    #[test]
    fn resolve_fills_from_agent_then_defaults() {
        let params = config().resolve(Call {
            prompt: "go".into(),
            agent: Some("tester".into()),
            system: None,
            model: None,
            effort: None,
            allowed_tools: None,
            cwd: None,
            session: None,
        });
        assert_eq!(params.prompt, "go");
        assert_eq!(params.system.as_deref(), Some("You run the tests."));
        assert_eq!(params.model.as_deref(), Some("haiku")); // agent beats default
        assert_eq!(params.effort.as_deref(), Some("medium")); // falls through to default
        assert_eq!(params.config_dir.as_deref(), Some(".agent/env/tester"));
    }

    #[test]
    fn call_overrides_agent() {
        let params = config().resolve(Call {
            prompt: "go".into(),
            agent: Some("tester".into()),
            system: None,
            model: Some("opus".into()),
            effort: None,
            allowed_tools: None,
            cwd: None,
            session: None,
        });
        assert_eq!(params.model.as_deref(), Some("opus"));
    }

    #[test]
    fn unknown_agent_falls_back_to_defaults() {
        let params = config().resolve(Call {
            prompt: "go".into(),
            agent: Some("nope".into()),
            system: None,
            model: None,
            effort: None,
            allowed_tools: None,
            cwd: None,
            session: None,
        });
        assert_eq!(params.model.as_deref(), Some("sonnet"));
        assert_eq!(params.system, None);
    }
}
