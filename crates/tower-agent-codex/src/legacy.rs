//! Compatibility implementation of the original `tower-agent-server` backend.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use codex_wrapper::command::exec::ExecResumeCommand;
use codex_wrapper::{Codex, ExecCommand, SandboxMode};
use tower_agent_server::{Backend, BackendError, Outcome, Params, Post};

use super::result_text;

/// The original backend implementation retained for `tower-agent-server`.
pub struct CodexBackend {
    timeout: Duration,
}

impl CodexBackend {
    pub fn new(timeout: Duration) -> Self {
        Self { timeout }
    }

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
            .map_err(|error| BackendError::new(format!("codex unavailable: {error}")))
    }
}

fn compose_prompt(params: &Params) -> String {
    let mut prompt = String::new();
    if let Some(system) = &params.system {
        prompt.push_str(system);
        prompt.push_str("\n\n");
    }
    if params.structured {
        prompt.push_str(REPORT_CONTRACT);
        prompt.push_str("\n\n");
    }
    prompt.push_str(&params.prompt);
    prompt
}

const REPORT_CONTRACT: &str = "Return a JSON object with these fields. `summary` is one line for \
    the operator's log. `reply` is your actual answer to whoever invoked you (the work product). \
    `posts` is an array of messages to other agents, each with a `channel`, a `body`, and \
    optionally `to` (address one agent directly) and `reply_to` (the id of the message you are \
    answering). Post when another agent should react; otherwise use an empty array.";

fn report_schema() -> String {
    r#"{"type":"object","properties":{"summary":{"type":"string"},"reply":{"type":"string"},"posts":{"type":"array","items":{"type":"object","properties":{"channel":{"type":"string"},"body":{"type":"string"},"to":{"type":"string"},"reply_to":{"type":"integer"}},"required":["channel","body"]}}},"required":["summary"]}"#.to_string()
}

fn write_schema() -> std::io::Result<PathBuf> {
    static NEXT_SCHEMA: AtomicU64 = AtomicU64::new(0);
    let sequence = NEXT_SCHEMA.fetch_add(1, Ordering::SeqCst);
    let path = std::env::temp_dir().join(format!(
        "tower-agent-codex-schema-{}-{sequence}.json",
        std::process::id()
    ));
    std::fs::write(&path, report_schema())?;
    Ok(path)
}

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
            cost_usd: None,
        },
        Err(_) => Outcome::from_reply(json, session),
    }
}

#[async_trait]
impl Backend for CodexBackend {
    fn name(&self) -> &str {
        "codex"
    }

    async fn run(&self, params: &Params) -> Result<Outcome, BackendError> {
        let codex = self.build_codex(params)?;
        let prompt = compose_prompt(params);
        let schema = if params.structured {
            Some(
                write_schema()
                    .map_err(|error| BackendError::new(format!("schema file: {error}")))?,
            )
        } else {
            None
        };

        let result = match &params.session {
            Some(session_id) => {
                let mut command = ExecResumeCommand::new()
                    .session_id(session_id.clone())
                    .prompt(prompt)
                    .skip_git_repo_check();
                if let Some(model) = &params.model {
                    command = command.model(model);
                }
                command.execute_json(&codex).await
            }
            None => {
                let mut command = ExecCommand::new(prompt)
                    .sandbox(SandboxMode::ReadOnly)
                    .skip_git_repo_check();
                if let Some(model) = &params.model {
                    command = command.model(model);
                }
                if let Some(directories) = &params.add_dirs {
                    for directory in directories {
                        command = command.add_dir(directory.clone());
                    }
                }
                if let Some(path) = &schema {
                    command = command.output_schema(path.to_string_lossy().to_string());
                }
                command.execute_json(&codex).await
            }
        };

        if let Some(path) = &schema {
            let _ = std::fs::remove_file(path);
        }

        let result =
            result.map_err(|error| BackendError::new(format!("codex run failed: {error}")))?;
        let text = result_text(&result);
        let cost = result.cost_usd;
        let session = result.session_id.or(result.thread_id);
        let mut outcome = if params.structured {
            parse_report(&text, session)
        } else {
            Outcome::from_reply(text, session)
        };
        outcome.cost_usd = cost;
        Ok(outcome)
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
        let prompt = compose_prompt(&params);
        assert!(prompt.starts_with("you are a helper"));
        assert!(prompt.contains("summary"));
        assert!(prompt.ends_with("do it"));
    }

    #[test]
    fn plain_prompt_is_unchanged() {
        let params = Params {
            prompt: "hi".into(),
            ..Default::default()
        };
        assert_eq!(compose_prompt(&params), "hi");
    }

    #[tokio::test]
    #[ignore = "needs the codex CLI and auth"]
    async fn live_legacy_prompt() {
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
        assert!(outcome.session.is_some());
    }
}
