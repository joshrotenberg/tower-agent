//! A Tower-native finite-turn service backed by `codex-wrapper`.
//!
//! [`CodexService`] implements
//! `Service<AgentRequest<Turn<CodexOptions>>>` directly. The service owns no
//! protocol surface; callers may compose it with `tower-agent` middleware and
//! project it onto MCP, HTTP, a CLI, or another host.
//!
//! Fresh and resumed turns send their prompts over stdin. The wrapper owns a
//! process group for every call, and this service bridges the request
//! cancellation token into its awaited cancellation path. On Unix, terminal
//! settlement follows process-group termination and direct-child reaping. On
//! non-Unix platforms, cleanup awaits the direct child but cannot guarantee
//! ownership of its descendants.
//!
//! Filesystem authority is portable rather than a wrapper flag. Each turn asks
//! for a [`FilesystemAuthority`], while [`CodexService`] enforces a host-owned
//! [`AuthorityPolicy`] immediately before launch. The default is read-only.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use codex_wrapper::command::exec::ExecResumeCommand;
use codex_wrapper::{Codex, ExecCommand, QueryResult, SandboxMode};
use tower::Service;
use tower_agent::{
    AgentError, AgentEvent, AgentRequest, AuthorityPolicy, CancellationToken, EffectState,
    ErrorKind, FailurePhase, FilesystemAuthority, RequestsFilesystemAuthority, SessionHandle,
    TokenUsage, Turn, TurnOutcome,
};

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
    /// `codex-wrapper` cannot apply these to `exec resume`, so resumed turns
    /// carrying any extra directory are rejected.
    pub additional_directories: Vec<PathBuf>,
    /// Portable filesystem authority requested for this turn.
    pub filesystem_authority: FilesystemAuthority,
}

impl RequestsFilesystemAuthority for CodexOptions {
    fn filesystem_authority(&self) -> FilesystemAuthority {
        self.filesystem_authority
    }

    fn additional_filesystem_roots(&self) -> &[PathBuf] {
        &self.additional_directories
    }
}

/// A cloneable Tower service that runs one finite Codex turn per call.
#[derive(Clone, Debug)]
pub struct CodexService {
    binary: Option<PathBuf>,
    codex_home: Option<PathBuf>,
    termination_grace: Option<Duration>,
    authority_policy: AuthorityPolicy,
}

impl CodexService {
    /// Create a service using the wrapper's default process-group ownership.
    pub fn new() -> Self {
        Self {
            binary: None,
            codex_home: None,
            termination_grace: None,
            authority_policy: AuthorityPolicy::read_only(),
        }
    }

    /// Override the `codex` executable, primarily for hermetic hosts and tests.
    pub fn with_binary(mut self, path: impl Into<PathBuf>) -> Self {
        self.binary = Some(path.into());
        self
    }

    /// Set the host-local `CODEX_HOME` used for every call through this service.
    pub fn with_codex_home(mut self, path: impl Into<PathBuf>) -> Self {
        self.codex_home = Some(path.into());
        self
    }

    /// Set how long an in-flight cancellation waits before forcing the owned
    /// Codex process group to stop.
    pub fn with_termination_grace(mut self, duration: Duration) -> Self {
        self.termination_grace = Some(duration);
        self
    }

    /// Set the host-owned filesystem ceiling enforced immediately before
    /// provider launch. The default permits read-only turns without explicit
    /// writable roots.
    pub fn with_authority_policy(mut self, policy: AuthorityPolicy) -> Self {
        self.authority_policy = policy;
        self
    }

    pub const fn authority_policy(&self) -> &AuthorityPolicy {
        &self.authority_policy
    }

    pub fn codex_home(&self) -> Option<&Path> {
        self.codex_home.as_deref()
    }

    fn build_codex(&self, working_directory: Option<&Path>) -> Result<Codex, AgentError> {
        let mut builder = Codex::builder();
        if let Some(binary) = &self.binary {
            builder = builder.binary(binary);
        }
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
        if let Some(duration) = self.termination_grace {
            builder = builder.termination_grace(duration);
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
        let prepared = prepare(request, &self.authority_policy);

        Box::pin(async move {
            let (turn, cancellation) = prepared?;
            if cancellation.is_cancelled() {
                return Err(cancelled_before_launch());
            }

            let started = Instant::now();
            let codex = service.build_codex(turn.working_directory.as_deref())?;

            if cancellation.is_cancelled() {
                return Err(cancelled_before_launch());
            }

            let _ = observer.try_emit(AgentEvent::Started);
            let result = match &turn.session {
                Some(session) => {
                    resume_command(&turn, session)
                        .execute_json_cancellable(&codex, cancellation.cancelled())
                        .await
                }
                None => {
                    fresh_command(&turn)
                        .execute_json_cancellable(&codex, cancellation.cancelled())
                        .await
                }
            };
            let result = result.map_err(map_run_error)?;

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
    filesystem_authority: FilesystemAuthority,
}

fn prepare(
    request: AgentRequest<Turn<CodexOptions>>,
    authority_policy: &AuthorityPolicy,
) -> Result<(PreparedTurn, CancellationToken), AgentError> {
    let cancellation = request.context.cancellation().clone();
    if cancellation.is_cancelled() {
        return Err(cancelled_before_launch());
    }

    let turn = request.body;
    authority_policy.authorize(&turn)?;
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
            "codex-wrapper cannot add directories to a resumed turn",
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
            filesystem_authority: turn.options.filesystem_authority,
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
    let mut command = ExecCommand::from_stdin(turn.prompt.clone())
        .sandbox(sandbox_mode(turn.filesystem_authority))
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
    let mut command = ExecResumeCommand::from_stdin(turn.prompt.clone())
        .session_id(session)
        .config(sandbox_config(turn.filesystem_authority))
        .skip_git_repo_check();
    if let Some(model) = &turn.model {
        command = command.model(model);
    }
    command
}

fn sandbox_mode(authority: FilesystemAuthority) -> SandboxMode {
    match authority {
        FilesystemAuthority::ReadOnly => SandboxMode::ReadOnly,
        FilesystemAuthority::WorkspaceWrite => SandboxMode::WorkspaceWrite,
        FilesystemAuthority::FullAccess => SandboxMode::DangerFullAccess,
    }
}

fn sandbox_config(authority: FilesystemAuthority) -> &'static str {
    match authority {
        FilesystemAuthority::ReadOnly => "sandbox_mode=\"read-only\"",
        FilesystemAuthority::WorkspaceWrite => "sandbox_mode=\"workspace-write\"",
        FilesystemAuthority::FullAccess => "sandbox_mode=\"danger-full-access\"",
    }
}

fn adapt_outcome(
    result: QueryResult,
    prior_session: Option<String>,
    duration: Duration,
) -> TurnOutcome {
    let output = result.result;
    let session = result
        .thread_id
        .or(result.session_id)
        .or(prior_session)
        .map(|value| SessionHandle::new(PROVIDER, value));
    let mut outcome = TurnOutcome::new(output);
    outcome.session = session;
    outcome.usage = result.usage.map(map_usage);
    outcome.duration = Some(duration);
    outcome
}

fn map_usage(usage: codex_wrapper::TokenUsage) -> TokenUsage {
    TokenUsage {
        input: usage.input_tokens,
        cached_input: usage.cached_input_tokens,
        cache_write_input: usage.cache_write_input_tokens,
        output: usage.output_tokens,
        reasoning_output: usage.reasoning_output_tokens,
        provider_total: usage.total(),
    }
}

fn cancelled_before_launch() -> AgentError {
    AgentError::new(
        ErrorKind::Cancelled,
        "Codex turn was cancelled before launch",
        FailurePhase::Admission,
        EffectState::None,
    )
}

fn cancelled_in_flight() -> AgentError {
    AgentError::new(
        ErrorKind::Cancelled,
        "Codex turn was cancelled",
        FailurePhase::Running,
        EffectState::Possible,
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
        codex_wrapper::Error::Cancelled { .. } => cancelled_in_flight(),
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
            filesystem_authority: FilesystemAuthority::ReadOnly,
        };
        let (prepared, _) =
            prepare(request("do it", options), &AuthorityPolicy::read_only()).expect("valid turn");
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
        assert_eq!(args.last().map(String::as_str), Some("-"));
        assert!(!args.iter().any(|arg| arg.contains("do it")));
    }

    #[test]
    fn authorized_workspace_write_maps_to_the_provider_sandbox() {
        let crate_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let policy = AuthorityPolicy::new(FilesystemAuthority::WorkspaceWrite)
            .allow_writable_root(&crate_root)
            .expect("crate root exists");
        let options = CodexOptions {
            filesystem_authority: FilesystemAuthority::WorkspaceWrite,
            ..Default::default()
        };
        let turn = Turn::new("edit")
            .with_options(options)
            .in_directory(&crate_root);
        let (prepared, _) = prepare(AgentRequest::new(turn), &policy).expect("authorized turn");
        let args = fresh_command(&prepared).args();

        assert!(
            args.windows(2)
                .any(|pair| pair == ["--sandbox", "workspace-write"])
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
    fn resume_enforces_the_read_only_default_and_uses_stdin() {
        let turn = PreparedTurn {
            prompt: "continue".into(),
            working_directory: None,
            session: Some("thread-1".into()),
            model: None,
            additional_directories: Vec::new(),
            filesystem_authority: FilesystemAuthority::ReadOnly,
        };
        let args = resume_command(&turn, "thread-1").args();

        assert!(
            args.windows(2)
                .any(|pair| pair == ["-c", "sandbox_mode=\"read-only\""])
        );
        assert_eq!(args.last().map(String::as_str), Some("-"));
        assert!(!args.iter().any(|arg| arg.contains("continue")));
    }

    #[tokio::test]
    async fn provider_launch_ceiling_cannot_be_bypassed_by_omitting_middleware() {
        let options = CodexOptions {
            filesystem_authority: FilesystemAuthority::WorkspaceWrite,
            ..Default::default()
        };

        let error = CodexService::new()
            .oneshot(request("write a file", options))
            .await
            .expect_err("the provider's default ceiling must remain read-only");

        assert_eq!(error.kind, ErrorKind::Unauthorized);
        assert_eq!(error.phase, FailurePhase::Validation);
        assert_eq!(error.effects, EffectState::None);
    }

    #[test]
    fn outcome_prefers_the_native_thread_id_for_resume() {
        let result = QueryResult {
            result: "done".into(),
            session_id: Some("session-1".into()),
            thread_id: Some("thread-1".into()),
            usage: None,
            events: Vec::new(),
        };

        let outcome = adapt_outcome(result, None, Duration::ZERO);
        assert_eq!(
            outcome.session.as_ref().map(SessionHandle::value),
            Some("thread-1")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fresh_turn_uses_stdin_and_maps_usage() {
        use std::os::unix::fs::PermissionsExt;

        let path = std::env::temp_dir().join(format!(
            "tower-agent-codex-success-{}.sh",
            std::process::id()
        ));
        std::fs::write(
            &path,
            concat!(
                "#!/bin/sh\n",
                "case \" $* \" in *\" secret-prompt \"*) exit 91;; esac\n",
                "prompt=$(cat)\n",
                "[ \"$prompt\" = \"secret-prompt\" ] || exit 92\n",
                "printf '%s\\n' '{\"type\":\"thread.started\",\"thread_id\":\"thread-1\"}'\n",
                "printf '%s\\n' '{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"done\"}}'\n",
                "printf '%s\\n' '{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":3,\"output_tokens\":2}}'\n",
            ),
        )
        .expect("write fake Codex CLI");
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&path, permissions).unwrap();

        let outcome = CodexService::new()
            .with_binary(&path)
            .oneshot(request("secret-prompt", CodexOptions::default()))
            .await
            .expect("fake Codex run succeeds");
        let _ = std::fs::remove_file(path);

        assert_eq!(outcome.output, "done");
        assert_eq!(outcome.usage.and_then(TokenUsage::total), Some(5));
        assert_eq!(
            outcome.session.as_ref().map(SessionHandle::value),
            Some("thread-1")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn in_flight_cancellation_settles_the_service_call() {
        use std::os::unix::fs::PermissionsExt;

        let path = std::env::temp_dir().join(format!(
            "tower-agent-codex-cancel-{}.sh",
            std::process::id()
        ));
        let pid_path = std::env::temp_dir().join(format!(
            "tower-agent-codex-cancel-{}.pid",
            std::process::id()
        ));
        let script = format!(
            "#!/bin/sh\ncat >/dev/null\nsleep 30 </dev/null &\nchild=$!\nprintf 'parent=%s\\nchild=%s\\n' \"$$\" \"$child\" > '{}'\nwait \"$child\"\n",
            pid_path.display()
        );
        std::fs::write(&path, script).expect("write blocking fake Codex CLI");
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&path, permissions).unwrap();

        let cancellation = CancellationToken::new();
        let request = AgentRequest::with_context(
            Turn::new("hello").with_options(CodexOptions::default()),
            CallContext::new().with_cancellation(cancellation.clone()),
        );
        let call = tokio::spawn(
            CodexService::new()
                .with_binary(&path)
                .with_termination_grace(Duration::from_millis(10))
                .oneshot(request),
        );
        for _ in 0..1000 {
            if pid_path.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            pid_path.exists(),
            "fake Codex did not record its process tree"
        );
        let pids =
            std::fs::read_to_string(&pid_path).expect("fake Codex recorded its process tree");
        cancellation.cancel();
        let error = tokio::time::timeout(Duration::from_secs(2), call)
            .await
            .expect("cancelled call must settle")
            .expect("provider task must not panic")
            .expect_err("cancelled call must fail");
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(pid_path);

        assert_eq!(error.kind, ErrorKind::Cancelled);
        assert_eq!(error.phase, FailurePhase::Running);
        assert_eq!(error.effects, EffectState::Possible);
        for line in pids.lines() {
            let (_, pid) = line.split_once('=').expect("pid line has a key");
            let output = std::process::Command::new("ps")
                .args(["-o", "state=", "-p", pid])
                .output()
                .expect("ps is available");
            let state = String::from_utf8_lossy(&output.stdout);
            assert!(
                state.trim().is_empty() || state.trim().starts_with('Z'),
                "process {pid} survived terminal settlement with state {state:?}"
            );
        }
    }

    #[tokio::test]
    #[ignore = "needs the codex CLI and auth"]
    async fn live_fresh_and_resume() {
        const MEMORY_TOKEN: &str = "tower-agent-resume-7b91f2";

        let service = CodexService::new();
        let fresh = service
            .clone()
            .oneshot(request(
                &format!(
                    "Remember the token {MEMORY_TOKEN}. Reply with exactly the word: fresh-pong"
                ),
                CodexOptions::default(),
            ))
            .await
            .expect("run fresh turn");
        assert!(
            fresh.output.to_lowercase().contains("fresh-pong"),
            "fresh output: {}",
            fresh.output
        );
        let session = fresh.session.expect("fresh turn must return a session");

        let resumed = service
            .oneshot(AgentRequest::new(
                Turn::new(
                    "Reply with exactly the token I asked you to remember in the previous turn",
                )
                .with_options(CodexOptions::default())
                .resume(session.clone()),
            ))
            .await
            .expect("resume returned session");
        assert!(
            resumed.output.contains(MEMORY_TOKEN),
            "resumed output: {}",
            resumed.output
        );
        assert_eq!(resumed.session.as_ref(), Some(&session));
    }
}
