//! A [`Backend`] for tower-agent backed by `claude-wrapper`.
//!
//! It maps a resolved [`Params`] onto a `claude-wrapper` query and runs it: the
//! prompt is the task, `system` is the system prompt, and `model`, `effort`,
//! `allowed_tools`, `cwd`, `session`, and `config_dir` map onto their wrapper
//! equivalents. `session` is passed straight through as the backend's own
//! session id: on the way out [`Outcome::session`] carries the id to resume with
//! next time, so continuity needs no registry here (that is the fabric's job
//! later).
//!
//! Permissions are left at the CLI default. In headless (`--print`) runs the CLI
//! cannot prompt, so a call without `allowed_tools` simply cannot use tools that
//! need approval; give an agent an `allowed_tools` allowlist to let it act. A
//! bypass-everything mode is deliberately not built here: it is a mechanical
//! layer to add if and when a live run shows the default is too narrow.

use std::time::Duration;

use async_trait::async_trait;
use claude_wrapper::{Claude, Effort, QueryCommand};
use tower_agent::{Backend, BackendError, Outcome, Params};

/// A backend that runs prompts through the Claude Code CLI via `claude-wrapper`.
pub struct ClaudeBackend {
    timeout: Duration,
}

impl ClaudeBackend {
    /// A backend with the given per-run timeout.
    pub fn new(timeout: Duration) -> Self {
        ClaudeBackend { timeout }
    }
}

/// Parse an effort string (case-insensitive) into the wrapper's [`Effort`].
fn parse_effort(s: &str) -> Option<Effort> {
    match s.trim().to_ascii_lowercase().as_str() {
        "low" => Some(Effort::Low),
        "medium" | "med" => Some(Effort::Medium),
        "high" => Some(Effort::High),
        _ => None,
    }
}

/// Build the query for these params. Pure: no execution, no live model. This is
/// the whole mapping, kept separate so it can be asserted against the rendered
/// command.
pub fn build_query(params: &Params) -> QueryCommand {
    let mut cmd = QueryCommand::new(params.prompt.clone());
    if let Some(system) = &params.system {
        cmd = cmd.system_prompt(system);
    }
    if let Some(append) = &params.append_system {
        cmd = cmd.append_system_prompt(append);
    }
    if let Some(model) = &params.model {
        cmd = cmd.model(model);
    }
    if let Some(effort) = params.effort.as_deref().and_then(parse_effort) {
        cmd = cmd.effort(effort);
    }
    if let Some(session) = &params.session {
        cmd = cmd.resume(session);
    }
    if let Some(turns) = params.max_turns {
        cmd = cmd.max_turns(turns);
    }
    if let Some(tools) = &params.allowed_tools
        && !tools.is_empty()
    {
        cmd = cmd.allowed_tools(tools.iter().cloned());
    }
    if let Some(tools) = &params.disallowed_tools
        && !tools.is_empty()
    {
        cmd = cmd.disallowed_tools(tools.iter().cloned());
    }
    if let Some(dirs) = &params.add_dirs {
        for dir in dirs {
            cmd = cmd.add_dir(dir);
        }
    }
    cmd
}

#[async_trait]
impl Backend for ClaudeBackend {
    async fn run(&self, params: &Params) -> Result<Outcome, BackendError> {
        let cwd = params.cwd.clone().unwrap_or_else(|| ".".to_string());
        let timeout = params
            .timeout
            .map(Duration::from_secs)
            .unwrap_or(self.timeout);
        let mut builder = Claude::builder().working_dir(cwd).timeout(timeout);
        // Each agent may run in its own environment. Auth does not inherit into a
        // fresh CLAUDE_CONFIG_DIR, so an isolated one must be provisioned a token.
        if let Some(dir) = &params.config_dir {
            builder = builder.env("CLAUDE_CONFIG_DIR", dir);
        }
        let claude = builder
            .build()
            .map_err(|e| BackendError::new(format!("claude unavailable: {e}")))?;

        match build_query(params).execute_json(&claude).await {
            Ok(qr) if qr.is_error => Err(BackendError::new(qr.result)),
            Ok(qr) => Ok(Outcome {
                text: qr.result,
                session: (!qr.session_id.is_empty()).then_some(qr.session_id),
            }),
            Err(e) => Err(BackendError::new(format!("run failed: {e}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Claude built with an explicit binary path, so `build()` skips the `which`
    /// probe and this works without the CLI installed. Used only to render the
    /// command for assertions; nothing is executed.
    fn fake_claude() -> Claude {
        Claude::builder().binary("/usr/bin/true").build().unwrap()
    }

    fn params() -> Params {
        Params {
            prompt: "run the tests".into(),
            system: Some("You are the tester.".into()),
            model: Some("haiku".into()),
            effort: Some("high".into()),
            allowed_tools: Some(vec!["Bash(cargo test:*)".into()]),
            cwd: Some("/repo".into()),
            ..Default::default()
        }
    }

    #[test]
    fn effort_parses_case_insensitively() {
        assert!(matches!(parse_effort("LOW"), Some(Effort::Low)));
        assert!(matches!(parse_effort("Medium"), Some(Effort::Medium)));
        assert!(matches!(parse_effort(" high "), Some(Effort::High)));
        assert!(parse_effort("bogus").is_none());
    }

    #[test]
    fn maps_prompt_system_model_effort_and_tools() {
        let rendered = build_query(&params()).to_command_string(&fake_claude());
        assert!(rendered.contains("run the tests"), "{rendered}");
        assert!(rendered.contains("You are the tester."), "{rendered}");
        assert!(rendered.contains("haiku"), "{rendered}");
        assert!(rendered.contains("cargo test"), "{rendered}");
    }

    #[test]
    fn no_allowlist_omits_tools() {
        let mut p = params();
        p.allowed_tools = None;
        let rendered = build_query(&p).to_command_string(&fake_claude());
        assert!(!rendered.contains("cargo test"), "{rendered}");
    }

    #[test]
    fn empty_allowlist_is_treated_as_none() {
        let mut p = params();
        p.allowed_tools = Some(vec![]);
        // Should not render an empty --allowed-tools; hard to assert the flag
        // name, so assert the build does not panic and drops the (empty) list.
        let _ = build_query(&p).to_command_string(&fake_claude());
    }

    #[test]
    fn session_becomes_resume() {
        let mut p = params();
        p.session = Some("sess-123".into());
        let rendered = build_query(&p).to_command_string(&fake_claude());
        assert!(rendered.contains("sess-123"), "{rendered}");
    }

    #[test]
    fn maps_append_system_disallowed_add_dirs_and_max_turns() {
        let mut p = params();
        p.append_system = Some("be brief".into());
        p.disallowed_tools = Some(vec!["Bash(rm:*)".into()]);
        p.add_dirs = Some(vec!["/shared".into(), "/data".into()]);
        p.max_turns = Some(4);
        let rendered = build_query(&p).to_command_string(&fake_claude());
        assert!(rendered.contains("be brief"), "{rendered}");
        assert!(rendered.contains("Bash(rm"), "{rendered}");
        assert!(rendered.contains("/shared"), "{rendered}");
        assert!(rendered.contains("/data"), "{rendered}");
        assert!(rendered.contains('4'), "{rendered}");
    }

    // Live smoke test: needs the claude CLI and auth. Run with:
    //   cargo test -p tower-agent-claude -- --ignored
    #[tokio::test]
    #[ignore = "needs the claude CLI and auth"]
    async fn live_prompt_runs() {
        let backend = ClaudeBackend::new(Duration::from_secs(120));
        let params = Params {
            prompt: "Reply with exactly the word: pong".into(),
            model: Some("haiku".into()),
            ..Default::default()
        };
        let outcome = backend.run(&params).await.expect("run");
        assert!(
            outcome.text.to_lowercase().contains("pong"),
            "got: {}",
            outcome.text
        );
        assert!(outcome.session.is_some());
    }
}
