//! A Tower-native finite-turn service backed by `codex-wrapper`.
//!
//! [`CodexService`] implements
//! `Service<AgentRequest<Turn<CodexOptions>>>` directly. The service owns no
//! protocol surface; callers may compose it with `tower-agent` middleware and
//! project it onto MCP, HTTP, a CLI, or another host.
//!
//! The locked `codex-wrapper` release buffers JSONL until the command exits and
//! does not expose verified subprocess-tree termination. Consequently this
//! service emits no incremental events, honors cancellation only before process
//! launch, and configures no wrapper timeout: that release's timeout can return
//! while leaving the child alive. A host can use `SuperviseLayer` to retain and
//! poll the future after caller drop, but cancellation and a finite safe deadline
//! remain blocked on a stronger wrapper process-ownership contract.
//! `codex-wrapper` 0.2 also has no stdin prompt path for `exec`, so prompt text
//! is present in the child argument vector. Hosts handling sensitive prompts
//! should wait for a wrapper API that can avoid that exposure.
//!
//! Enable the `legacy-server` feature to expose the previous `CodexBackend`
//! implementation for `tower-agent-server` during migration.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

pub use codex_wrapper::SandboxMode;
use codex_wrapper::command::exec::ExecResumeCommand;
use codex_wrapper::{Codex, ExecCommand, QueryResult};
use tower::Service;
use tower_agent::{
    AgentError, AgentEvent, AgentRequest, CancellationToken, Cost, EffectState, ErrorKind,
    FailurePhase, SessionHandle, Turn, TurnOutcome,
};

#[cfg(feature = "legacy-server")]
mod legacy;
#[cfg(feature = "legacy-server")]
pub use legacy::CodexBackend;

const PROVIDER: &str = "codex";

/// Per-turn controls supported by the Codex provider service.
///
/// Host-local launch configuration such as `CODEX_HOME` belongs on
/// [`CodexService`], not in the portable turn body.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CodexOptions {
    /// Instructions prepended to the user prompt because `codex exec` has no
    /// separate system-prompt argument.
    pub system_prompt: Option<String>,
    /// Override the Codex model for this turn.
    pub model: Option<String>,
    /// Extra directories made available to a fresh turn with `--add-dir`.
    /// `codex-wrapper` 0.2 cannot apply these to `exec resume`, so resumed turns
    /// carrying any extra directory are rejected.
    pub additional_directories: Vec<PathBuf>,
    /// Explicit sandbox mode. When absent, fresh and resumed turns use
    /// [`SandboxMode::ReadOnly`]. Resumed turns enforce it through a Codex
    /// configuration override because their command has no `--sandbox` flag.
    pub sandbox: Option<SandboxMode>,
}

/// A cloneable Tower service that runs one finite Codex turn per call.
#[derive(Clone, Debug)]
pub struct CodexService {
    codex_home: Option<PathBuf>,
}

impl CodexService {
    /// Create a service without the unsafe timeout in `codex-wrapper` 0.2.
    pub fn new() -> Self {
        Self { codex_home: None }
    }

    /// Set the host-local `CODEX_HOME` used for every call through this service.
    pub fn with_codex_home(mut self, path: impl Into<PathBuf>) -> Self {
        self.codex_home = Some(path.into());
        self
    }

    pub fn codex_home(&self) -> Option<&Path> {
        self.codex_home.as_deref()
    }

    fn build_codex(&self, working_directory: Option<&Path>) -> Result<Codex, AgentError> {
        let mut builder = Codex::builder();
        if let Some(directory) = working_directory {
            builder = builder.working_dir(directory.to_path_buf());
        }
        if let Some(directory) = &self.codex_home {
            let directory = directory.to_str().ok_or_else(|| {
                AgentError::new(
                    ErrorKind::Internal,
                    "configured CODEX_HOME is not valid UTF-8",
                    FailurePhase::Launch,
                    EffectState::None,
                )
            })?;
            builder = builder.env("CODEX_HOME", directory);
        }
        builder.build().map_err(map_launch_error)
    }
}

impl Default for CodexService {
    fn default() -> Self {
        Self::new()
    }
}

impl Service<AgentRequest<Turn<CodexOptions>>> for CodexService {
    type Response = TurnOutcome;
    type Error = AgentError;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: AgentRequest<Turn<CodexOptions>>) -> Self::Future {
        let service = self.clone();
        let observer = request.context.events().clone();
        let prepared = prepare(request);

        Box::pin(async move {
            let (turn, cancellation) = prepared?;
            if cancellation.is_cancelled() {
                return Err(cancelled_before_launch());
            }

            let started = Instant::now();
            let codex = service.build_codex(turn.working_directory.as_deref())?;

            // This is the last cancellation point for which the current wrapper
            // can honestly guarantee that no provider process has been launched.
            if cancellation.is_cancelled() {
                return Err(cancelled_before_launch());
            }

            let _ = observer.try_emit(AgentEvent::Started);
            let result = match &turn.session {
                Some(session) => resume_command(&turn, session).execute_json(&codex).await,
                None => fresh_command(&turn).execute_json(&codex).await,
            }
            .map_err(map_run_error)?;

            let outcome = adapt_outcome(result, turn.session, started.elapsed());
            let _ = observer.try_emit(AgentEvent::OutputDelta {
                text: outcome.output.clone(),
            });
            Ok(outcome)
        })
    }
}

struct PreparedTurn {
    prompt: String,
    working_directory: Option<PathBuf>,
    session: Option<String>,
    model: Option<String>,
    additional_directories: Vec<String>,
    sandbox: Option<SandboxMode>,
}

fn prepare(
    request: AgentRequest<Turn<CodexOptions>>,
) -> Result<(PreparedTurn, CancellationToken), AgentError> {
    let cancellation = request.context.cancellation().clone();
    if cancellation.is_cancelled() {
        return Err(cancelled_before_launch());
    }

    let turn = request.body;
    if turn.prompt.trim().is_empty() {
        return Err(AgentError::invalid_request("prompt must not be empty"));
    }
    if turn
        .options
        .model
        .as_ref()
        .is_some_and(|model| model.trim().is_empty())
    {
        return Err(AgentError::invalid_request("Codex model must not be empty"));
    }

    let session = match turn.session {
        Some(session) if session.provider() != PROVIDER => {
            return Err(AgentError::unsupported(format!(
                "cannot resume {} session with Codex service",
                session.provider()
            )));
        }
        Some(session) if session.value().trim().is_empty() => {
            return Err(AgentError::invalid_request(
                "Codex session handle must not be empty",
            ));
        }
        Some(session) => Some(session.value().to_string()),
        None => None,
    };

    if session.is_some() && !turn.options.additional_directories.is_empty() {
        return Err(AgentError::unsupported(
            "codex-wrapper 0.2 cannot add directories to a resumed turn",
        ));
    }
    let additional_directories = turn
        .options
        .additional_directories
        .iter()
        .map(|path| {
            path.to_str().map(str::to_owned).ok_or_else(|| {
                AgentError::unsupported("Codex additional directories must be valid UTF-8")
            })
        })
        .collect::<Result<Vec<_>, _>>()?;

    Ok((
        PreparedTurn {
            prompt: compose_prompt(&turn.prompt, turn.options.system_prompt.as_deref()),
            working_directory: turn.working_directory,
            session,
            model: turn.options.model,
            additional_directories,
            sandbox: turn.options.sandbox,
        },
        cancellation,
    ))
}

fn compose_prompt(prompt: &str, system_prompt: Option<&str>) -> String {
    match system_prompt {
        Some(system_prompt) if !system_prompt.is_empty() => {
            format!("{system_prompt}\n\n{prompt}")
        }
        _ => prompt.to_string(),
    }
}

fn fresh_command(turn: &PreparedTurn) -> ExecCommand {
    let mut command = ExecCommand::new(turn.prompt.clone())
        .sandbox(turn.sandbox.unwrap_or(SandboxMode::ReadOnly))
        .skip_git_repo_check();
    if let Some(model) = &turn.model {
        command = command.model(model);
    }
    for directory in &turn.additional_directories {
        command = command.add_dir(directory);
    }
    command
}

fn resume_command(turn: &PreparedTurn, session: &str) -> ExecResumeCommand {
    let mut command = ExecResumeCommand::new()
        .session_id(session)
        .prompt(turn.prompt.clone())
        .config(sandbox_config(
            turn.sandbox.unwrap_or(SandboxMode::ReadOnly),
        ))
        .skip_git_repo_check();
    if let Some(model) = &turn.model {
        command = command.model(model);
    }
    command
}

fn sandbox_config(sandbox: SandboxMode) -> &'static str {
    match sandbox {
        SandboxMode::ReadOnly => "sandbox_mode=\"read-only\"",
        SandboxMode::WorkspaceWrite => "sandbox_mode=\"workspace-write\"",
        SandboxMode::DangerFullAccess => "sandbox_mode=\"danger-full-access\"",
    }
}

fn adapt_outcome(
    result: QueryResult,
    prior_session: Option<String>,
    duration: Duration,
) -> TurnOutcome {
    let output = result_text(&result);
    let session = result
        .thread_id
        .or(result.session_id)
        .or(prior_session)
        .map(|value| SessionHandle::new(PROVIDER, value));
    let mut outcome = TurnOutcome::new(output);
    outcome.session = session;
    outcome.cost = result.cost_usd.map(Cost::usd);
    outcome.duration = Some(duration);
    outcome
}

fn result_text(result: &QueryResult) -> String {
    if !result.result.is_empty() {
        return result.result.clone();
    }
    result
        .events
        .iter()
        .rev()
        .find_map(|event| {
            let item = event.extra.get("item")?;
            if item.get("type").and_then(|value| value.as_str()) == Some("agent_message") {
                item.get("text")
                    .and_then(|value| value.as_str())
                    .map(str::to_string)
            } else {
                None
            }
        })
        .unwrap_or_default()
}

fn cancelled_before_launch() -> AgentError {
    AgentError::new(
        ErrorKind::Cancelled,
        "Codex turn was cancelled before launch",
        FailurePhase::Admission,
        EffectState::None,
    )
}

fn map_launch_error(error: codex_wrapper::Error) -> AgentError {
    let message = match error {
        codex_wrapper::Error::NotFound => "Codex executable was not found".to_string(),
        _ => "Codex could not be initialized".to_string(),
    };
    AgentError::new(
        ErrorKind::Provider,
        message,
        FailurePhase::Launch,
        EffectState::None,
    )
}

fn map_run_error(error: codex_wrapper::Error) -> AgentError {
    match error {
        codex_wrapper::Error::NotFound => AgentError::new(
            ErrorKind::Provider,
            "Codex executable was not found",
            FailurePhase::Launch,
            EffectState::None,
        ),
        codex_wrapper::Error::Io { message, .. }
            if message.starts_with("failed to spawn codex") =>
        {
            AgentError::new(
                ErrorKind::Provider,
                "Codex process could not be launched",
                FailurePhase::Launch,
                EffectState::None,
            )
        }
        codex_wrapper::Error::Timeout { .. } => AgentError::new(
            ErrorKind::DeadlineExceeded,
            "Codex command exceeded its configured timeout",
            FailurePhase::Running,
            EffectState::Possible,
        ),
        codex_wrapper::Error::CommandFailed { exit_code, .. } => AgentError::new(
            ErrorKind::Provider,
            format!("Codex command failed with exit code {exit_code}"),
            FailurePhase::Running,
            EffectState::Possible,
        ),
        codex_wrapper::Error::Json { .. } => AgentError::new(
            ErrorKind::Provider,
            "Codex returned an invalid event stream",
            FailurePhase::Settlement,
            EffectState::Possible,
        ),
        _ => AgentError::new(
            ErrorKind::Provider,
            "Codex command failed",
            FailurePhase::Running,
            EffectState::Possible,
        ),
    }
}

#[cfg(test)]
mod tests {
    use codex_wrapper::CodexCommand;
    use tower::ServiceExt;
    use tower_agent::{CallContext, OperationId};

    use super::*;

    fn request(prompt: &str, options: CodexOptions) -> AgentRequest<Turn<CodexOptions>> {
        AgentRequest::new(Turn::new(prompt).with_options(options))
    }

    #[test]
    fn fresh_command_preserves_existing_read_only_default_and_maps_options() {
        let options = CodexOptions {
            system_prompt: Some("you are a helper".into()),
            model: Some("gpt-test".into()),
            additional_directories: vec![PathBuf::from("/work/extra")],
            sandbox: None,
        };
        let (prepared, _) = prepare(request("do it", options)).expect("valid turn");
        let args = fresh_command(&prepared).args();

        assert!(
            args.windows(2)
                .any(|pair| pair == ["--sandbox", "read-only"])
        );
        assert!(args.windows(2).any(|pair| pair == ["--model", "gpt-test"]));
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--add-dir", "/work/extra"])
        );
        assert_eq!(
            args.last().map(String::as_str),
            Some("you are a helper\n\ndo it")
        );
    }

    #[tokio::test]
    async fn pre_cancelled_turn_fails_before_looking_for_codex() {
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let context = CallContext::new()
            .with_operation_id(OperationId::from_u64(7))
            .with_cancellation(cancellation);
        let error = CodexService::new()
            .oneshot(AgentRequest::with_context(
                Turn::new("hello").with_options(CodexOptions::default()),
                context,
            ))
            .await
            .expect_err("pre-cancelled turn must fail");

        assert_eq!(error.kind, ErrorKind::Cancelled);
        assert_eq!(error.phase, FailurePhase::Admission);
        assert_eq!(error.effects, EffectState::None);
    }

    #[tokio::test]
    async fn foreign_session_is_rejected_before_launch() {
        let turn = Turn::new("continue")
            .with_options(CodexOptions::default())
            .resume(SessionHandle::new("claude", "session-1"));
        let error = CodexService::new()
            .oneshot(AgentRequest::new(turn))
            .await
            .expect_err("foreign session must fail");

        assert_eq!(error.kind, ErrorKind::Unsupported);
        assert_eq!(error.effects, EffectState::None);
    }

    #[tokio::test]
    async fn resume_refuses_options_the_wrapper_cannot_honor() {
        let options = CodexOptions {
            additional_directories: vec![PathBuf::from("/work/extra")],
            ..Default::default()
        };
        let turn = Turn::new("continue")
            .with_options(options)
            .resume(SessionHandle::new(PROVIDER, "session-1"));
        let error = CodexService::new()
            .oneshot(AgentRequest::new(turn))
            .await
            .expect_err("unsupported resume option must fail");

        assert_eq!(error.kind, ErrorKind::Unsupported);
        assert_eq!(error.phase, FailurePhase::Validation);
        assert_eq!(error.effects, EffectState::None);
    }

    #[test]
    fn resume_enforces_the_read_only_default() {
        let turn = PreparedTurn {
            prompt: "continue".into(),
            working_directory: None,
            session: Some("thread-1".into()),
            model: None,
            additional_directories: Vec::new(),
            sandbox: None,
        };
        let args = resume_command(&turn, "thread-1").args();

        assert!(
            args.windows(2)
                .any(|pair| pair == ["-c", "sandbox_mode=\"read-only\""])
        );
    }

    #[test]
    fn outcome_prefers_the_native_thread_id_for_resume() {
        let result = QueryResult {
            result: "done".into(),
            session_id: Some("session-1".into()),
            thread_id: Some("thread-1".into()),
            cost_usd: None,
            events: Vec::new(),
        };

        let outcome = adapt_outcome(result, None, Duration::ZERO);
        assert_eq!(
            outcome.session.as_ref().map(SessionHandle::value),
            Some("thread-1")
        );
    }

    #[tokio::test]
    #[ignore = "needs the codex CLI and auth"]
    async fn live_prompt() {
        let outcome = CodexService::new()
            .oneshot(request(
                "Reply with exactly the word: pong",
                CodexOptions::default(),
            ))
            .await
            .expect("run");
        assert!(
            outcome.output.to_lowercase().contains("pong"),
            "got: {}",
            outcome.output
        );
        assert!(outcome.session.is_some());
    }
}
