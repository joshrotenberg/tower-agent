//! A [`Backend`] for tower-agent backed by `codex-wrapper`.
//!
//! Proves the backend seam: the core names no backend, and a different CLI drops
//! in. It maps a resolved [`Params`] onto `codex exec`. Codex has no separate
//! system prompt, so an agent's `system` is folded into the prompt; its
//! permission model is sandbox modes rather than an allow-list, so
//! `allowed_tools`/`effort`/`max_turns` are not mapped. Each backend supports the
//! subset its CLI offers.
//!
//! `codex-wrapper`'s JSONL is parsed after the run, not via a live callback, so
//! this backend uses the non-streaming [`Backend`] default: a fine demonstration
//! that the seam does not require streaming.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use codex_wrapper::command::exec::ExecResumeCommand;
use codex_wrapper::{Codex, ExecCommand, SandboxMode};
use tower_agent::{Backend, BackendError, Outcome, Params, Post};

/// A backend that runs prompts through the Codex CLI via `codex-wrapper`.
pub struct CodexBackend {
    timeout: Duration,
}

impl CodexBackend {
    pub fn new(timeout: Duration) -> Self {
        CodexBackend { timeout }
    }

    /// Build the `Codex` for these params: working directory, timeout, and the
    /// per-agent environment (`CODEX_HOME`).
    fn build_codex(&self, params: &Params) -> Result<Codex, BackendError> {
        let mut builder = Codex::builder().timeout(self.timeout);
        if let Some(cwd) = &params.cwd {
            builder = builder.working_dir(cwd.clone());
        }
        if let Some(dir) = &params.config_dir {
            builder = builder.env("CODEX_HOME", dir.clone());
        }
        builder
            .build()
            .map_err(|e| BackendError::new(format!("codex unavailable: {e}")))
    }
}

/// Compose the full prompt: codex has no system prompt, so the agent's `system`
/// (and, for a structured turn, the report contract) is prepended to the task.
fn compose_prompt(params: &Params) -> String {
    let mut p = String::new();
    if let Some(system) = &params.system {
        p.push_str(system);
        p.push_str("\n\n");
    }
    if params.structured {
        p.push_str(REPORT_CONTRACT);
        p.push_str("\n\n");
    }
    p.push_str(&params.prompt);
    p
}

const REPORT_CONTRACT: &str = "Return a JSON object with these fields. `summary` is one line for \
    the operator's log. `reply` is your actual answer to whoever invoked you (the work product). \
    `posts` is an array of messages to other agents, each with a `channel`, a `body`, and \
    optionally `to` (address one agent directly) and `reply_to` (the id of the message you are \
    answering). Post when another agent should react; otherwise use an empty array.";

fn report_schema() -> String {
    r#"{"type":"object","properties":{"summary":{"type":"string"},"reply":{"type":"string"},"posts":{"type":"array","items":{"type":"object","properties":{"channel":{"type":"string"},"body":{"type":"string"},"to":{"type":"string"},"reply_to":{"type":"integer"}},"required":["channel","body"]}}},"required":["summary"]}"#.to_string()
}

/// Write the report schema to a unique temp file and return its path. Codex takes
/// the schema as a file path, not inline.
fn write_schema() -> std::io::Result<PathBuf> {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    let path = std::env::temp_dir().join(format!(
        "tower-agent-codex-schema-{}-{n}.json",
        std::process::id()
    ));
    std::fs::write(&path, report_schema())?;
    Ok(path)
}

/// The final answer text. `codex-wrapper`'s `QueryResult::result` reads the
/// completed event, but codex-cli 0.145 carries the answer in an
/// `item.completed` `agent_message` instead, so fall back to that when `result`
/// is empty. (A codex-wrapper update would remove the need for this.)
fn result_text(qr: &codex_wrapper::QueryResult) -> String {
    if !qr.result.is_empty() {
        return qr.result.clone();
    }
    qr.events
        .iter()
        .rev()
        .find_map(|e| {
            let item = e.extra.get("item")?;
            if item.get("type").and_then(|v| v.as_str()) == Some("agent_message") {
                item.get("text")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            } else {
                None
            }
        })
        .unwrap_or_default()
}

/// Parse a structured report into an outcome, falling back to a plain reply if
/// the model did not return the expected JSON.
fn parse_report(json: &str, session: Option<String>) -> Outcome {
    #[derive(serde::Deserialize)]
    struct Raw {
        summary: String,
        #[serde(default)]
        reply: String,
        #[serde(default)]
        posts: Vec<Post>,
    }
    match serde_json::from_str::<Raw>(json) {
        Ok(raw) => Outcome {
            summary: raw.summary,
            reply: raw.reply,
            posts: raw.posts,
            session,
        },
        Err(_) => Outcome::from_reply(json, session),
    }
}

#[async_trait]
impl Backend for CodexBackend {
    async fn run(&self, params: &Params) -> Result<Outcome, BackendError> {
        let codex = self.build_codex(params)?;
        let prompt = compose_prompt(params);
        let schema = if params.structured {
            Some(write_schema().map_err(|e| BackendError::new(format!("schema file: {e}")))?)
        } else {
            None
        };

        let result = match &params.session {
            // Resume a persisted session by its id.
            Some(session_id) => {
                let mut cmd = ExecResumeCommand::new()
                    .session_id(session_id.clone())
                    .prompt(prompt)
                    .skip_git_repo_check();
                if let Some(model) = &params.model {
                    cmd = cmd.model(model);
                }
                cmd.execute_json(&codex).await
            }
            // A fresh run persists (no `.ephemeral()`), so it can be resumed.
            None => {
                let mut cmd = ExecCommand::new(prompt)
                    .sandbox(SandboxMode::ReadOnly)
                    .skip_git_repo_check();
                if let Some(model) = &params.model {
                    cmd = cmd.model(model);
                }
                if let Some(dirs) = &params.add_dirs {
                    for dir in dirs {
                        cmd = cmd.add_dir(dir.clone());
                    }
                }
                if let Some(path) = &schema {
                    cmd = cmd.output_schema(path.to_string_lossy().to_string());
                }
                cmd.execute_json(&codex).await
            }
        };

        if let Some(path) = &schema {
            let _ = std::fs::remove_file(path);
        }

        let qr = result.map_err(|e| BackendError::new(format!("codex run failed: {e}")))?;
        let text = result_text(&qr);
        // Codex reports a session as `session_id` or, on newer CLIs, `thread_id`;
        // either resumes the thread.
        let session = qr.session_id.or(qr.thread_id);
        Ok(if params.structured {
            parse_report(&text, session)
        } else {
            Outcome::from_reply(text, session)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_prepends_system_and_contract() {
        let params = Params {
            prompt: "do it".into(),
            system: Some("you are a helper".into()),
            structured: true,
            ..Default::default()
        };
        let p = compose_prompt(&params);
        assert!(p.starts_with("you are a helper"));
        assert!(p.contains("summary"));
        assert!(p.ends_with("do it"));
    }

    #[test]
    fn plain_prompt_is_unchanged() {
        let params = Params {
            prompt: "hi".into(),
            ..Default::default()
        };
        assert_eq!(compose_prompt(&params), "hi");
    }

    // Live: needs the codex CLI and auth.
    //   cargo test -p tower-agent-codex -- --ignored
    #[tokio::test]
    #[ignore = "needs the codex CLI and auth"]
    async fn live_prompt() {
        let backend = CodexBackend::new(Duration::from_secs(120));
        let outcome = backend
            .run(&Params {
                prompt: "Reply with exactly the word: pong".into(),
                ..Default::default()
            })
            .await
            .expect("run");
        assert!(
            outcome.reply.to_lowercase().contains("pong"),
            "got: {}",
            outcome.reply
        );
        // Codex returns a session token (session_id or thread_id).
        assert!(outcome.session.is_some());
    }
}
