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
    pub add_dirs: Option<Vec<String>>,
    pub max_turns: Option<u32>,
    pub cwd: Option<String>,
    pub timeout: Option<u64>,
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
    pub disallowed_tools: Option<Vec<String>>,
    pub add_dirs: Option<Vec<String>>,
    pub max_turns: Option<u32>,
    pub cwd: Option<String>,
    pub timeout: Option<u64>,
    /// This agent's own environment (`CLAUDE_CONFIG_DIR`).
    pub config_dir: Option<String>,
    /// A cron expression: the server fires this agent's `schedule_prompt` on
    /// this cadence. Optional seconds are supported (6-field).
    pub schedule: Option<String>,
    /// The prompt fired on each scheduled tick. A generic default is used when a
    /// scheduled agent omits it.
    pub schedule_prompt: Option<String>,
}

/// A scheduled agent: its name, cron expression, and the prompt fired each tick.
#[derive(Debug, Clone)]
pub struct ScheduledAgent {
    pub name: String,
    pub schedule: String,
    pub prompt: String,
}

/// The default prompt for a scheduled tick when the agent does not set one.
pub(crate) fn default_tick_prompt() -> String {
    "Your scheduled run fired. Do your job for this cadence.".to_string()
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

    /// Whether an agent by this name is configured.
    pub fn has_agent(&self, name: &str) -> bool {
        self.agents.contains_key(name)
    }

    /// The agents that carry a schedule, with the prompt fired each tick.
    pub fn scheduled_agents(&self) -> Vec<ScheduledAgent> {
        self.agents
            .iter()
            .filter_map(|(name, a)| {
                a.schedule.as_ref().map(|schedule| ScheduledAgent {
                    name: name.clone(),
                    schedule: schedule.clone(),
                    prompt: a
                        .schedule_prompt
                        .clone()
                        .unwrap_or_else(default_tick_prompt),
                })
            })
            .collect()
    }

    /// The prompt to fire when ticking an agent: its `schedule_prompt`, or the
    /// generic default.
    pub fn tick_prompt(&self, agent: &str) -> String {
        self.agents
            .get(agent)
            .and_then(|a| a.schedule_prompt.clone())
            .unwrap_or_else(default_tick_prompt)
    }

    /// Resolve a call into the parameters a backend runs: the call over the
    /// selected agent's defaults over the server defaults. An unknown agent name
    /// contributes no defaults; callers that require the agent to exist should
    /// check [`Config::has_agent`] first (the server does).
    pub fn resolve(&self, call: Call) -> Params {
        let agent = call.agent.as_ref().and_then(|a| self.agents.get(a));
        Params {
            prompt: call.prompt,
            system: call.system.or_else(|| agent.and_then(|a| a.system.clone())),
            append_system: call.append_system,
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
            disallowed_tools: call
                .disallowed_tools
                .or_else(|| agent.and_then(|a| a.disallowed_tools.clone())),
            add_dirs: call
                .add_dirs
                .or_else(|| agent.and_then(|a| a.add_dirs.clone()))
                .or_else(|| self.defaults.add_dirs.clone()),
            max_turns: call
                .max_turns
                .or_else(|| agent.and_then(|a| a.max_turns))
                .or(self.defaults.max_turns),
            cwd: call
                .cwd
                .or_else(|| agent.and_then(|a| a.cwd.clone()))
                .or_else(|| self.defaults.cwd.clone()),
            timeout: call
                .timeout
                .or_else(|| agent.and_then(|a| a.timeout))
                .or(self.defaults.timeout),
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
            max_turns = 10

            [agents.tester]
            system = "You run the tests."
            model = "haiku"
            disallowed_tools = ["Bash(rm:*)"]
            max_turns = 3
            config_dir = ".agent/env/tester"
            "#,
        )
        .unwrap()
    }

    fn call(agent: Option<&str>) -> Call {
        Call {
            prompt: "go".into(),
            agent: agent.map(Into::into),
            system: None,
            append_system: None,
            model: None,
            effort: None,
            allowed_tools: None,
            disallowed_tools: None,
            add_dirs: None,
            max_turns: None,
            cwd: None,
            timeout: None,
            session: None,
        }
    }

    #[test]
    fn resolve_fills_from_agent_then_defaults() {
        let p = config().resolve(call(Some("tester")));
        assert_eq!(p.system.as_deref(), Some("You run the tests."));
        assert_eq!(p.model.as_deref(), Some("haiku")); // agent beats default
        assert_eq!(p.effort.as_deref(), Some("medium")); // falls through to default
        assert_eq!(p.max_turns, Some(3)); // agent beats the default of 10
        assert_eq!(
            p.disallowed_tools.as_deref(),
            Some(&["Bash(rm:*)".into()][..])
        );
        assert_eq!(p.config_dir.as_deref(), Some(".agent/env/tester"));
    }

    #[test]
    fn call_overrides_agent() {
        let mut c = call(Some("tester"));
        c.model = Some("opus".into());
        c.max_turns = Some(1);
        let p = config().resolve(c);
        assert_eq!(p.model.as_deref(), Some("opus"));
        assert_eq!(p.max_turns, Some(1));
    }

    #[test]
    fn unknown_agent_contributes_no_defaults() {
        let p = config().resolve(call(Some("nope")));
        assert_eq!(p.model.as_deref(), Some("sonnet")); // server default only
        assert_eq!(p.system, None);
        assert_eq!(p.max_turns, Some(10)); // server default
    }

    #[test]
    fn unknown_config_key_is_rejected() {
        let err = Config::parse(
            r#"
            [defaults]
            modle = "sonnet"
            "#,
        );
        assert!(err.is_err(), "a typo in a config key must be rejected");
    }

    #[test]
    fn has_agent() {
        let c = config();
        assert!(c.has_agent("tester"));
        assert!(!c.has_agent("nope"));
    }
}
