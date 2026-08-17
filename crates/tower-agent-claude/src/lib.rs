//! A Tower-native Claude Code provider for `tower-agent`.
//!
//! [`ClaudeService`] implements
//! `Service<AgentRequest<Turn<ClaudeOptions>>>` directly. The optional
//! `legacy-server` feature also exposes the original `ClaudeBackend` and its
//! `tower-agent-server::Backend` implementation for migration.
//!
//! Permissions are left at the CLI default. In headless (`--print`) runs the CLI
//! cannot prompt, so a call without `allowed_tools` simply cannot use tools that
//! need approval; give an agent an `allowed_tools` allowlist to let it act. A
//! bypass-everything mode is deliberately not built here: it is a mechanical
//! layer to add if and when a live run shows the default is too narrow.
//!
//! # Cancellation limitation
//!
//! The currently supported `claude-wrapper` API does not expose
//! ownership of the complete subprocess tree. This service rejects a call whose
//! cancellation token is already cancelled, but it does not claim that signalling
//! or dropping an in-flight call terminates every provider descendant. It also
//! configures no wrapper timeout because that release can return after killing
//! only the direct child while descendants remain. A finite safe deadline is
//! blocked on stronger wrapper process ownership.
//! The wrapper's buffered-stdin path can also return from a failed stdin write
//! without proving that the direct child was killed and reaped. Until the
//! wrapper owns that failure path, a returned I/O error is not process-cleanup
//! evidence.
//! The native path uses buffered JSON over stdin to keep the user prompt out of
//! process arguments, so it emits start and terminal-output observations rather
//! than incremental token deltas. Claude's system-prompt flags still place
//! those instruction values in the child argument vector; do not put secrets in
//! them until the wrapper offers a non-argv path.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

#[cfg(feature = "legacy-server")]
use async_trait::async_trait;
#[cfg(feature = "legacy-server")]
use claude_wrapper::streaming::{
    BlockDelta, BlockType, PartialMessageEvent, StreamEvent, stream_query,
};
#[cfg(feature = "legacy-server")]
use claude_wrapper::types::OutputFormat;
use claude_wrapper::types::QueryResult;
use claude_wrapper::{Claude, Effort, QueryCommand};
#[cfg(feature = "legacy-server")]
use tokio::sync::mpsc::UnboundedSender;
use tower::Service;
use tower_agent::{
    AgentError, AgentEvent, AgentRequest, Cost, EffectState, ErrorKind, FailurePhase,
    SessionHandle, Turn, TurnOutcome,
};
#[cfg(feature = "legacy-server")]
use tower_agent_server::{Backend, BackendError, Event, Outcome, Params, Post};

const PROVIDER: &str = "claude";

/// Provider-specific controls for one Claude Code turn.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClaudeOptions {
    pub system_prompt: Option<String>,
    pub append_system_prompt: Option<String>,
    pub model: Option<String>,
    pub effort: Option<ClaudeEffort>,
    pub allowed_tools: Vec<String>,
    pub disallowed_tools: Vec<String>,
    pub additional_directories: Vec<PathBuf>,
    pub max_turns: Option<u32>,
}

/// Claude Code effort levels supported by the original provider contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClaudeEffort {
    Low,
    Medium,
    High,
}

impl From<ClaudeEffort> for Effort {
    fn from(value: ClaudeEffort) -> Self {
        match value {
            ClaudeEffort::Low => Self::Low,
            ClaudeEffort::Medium => Self::Medium,
            ClaudeEffort::High => Self::High,
        }
    }
}

/// A finite-turn Tower service backed by the Claude Code CLI.
#[derive(Clone, Debug)]
pub struct ClaudeService {
    binary: Option<PathBuf>,
    config_directory: Option<PathBuf>,
}

impl ClaudeService {
    /// Create a service without the unsafe timeout in `claude-wrapper` 0.13.
    pub fn new() -> Self {
        Self {
            binary: None,
            config_directory: None,
        }
    }

    /// Override the `claude` executable, primarily for hermetic hosts and tests.
    pub fn with_binary(mut self, binary: impl Into<PathBuf>) -> Self {
        self.binary = Some(binary.into());
        self
    }

    /// Use an isolated host-owned Claude configuration directory.
    pub fn with_config_directory(mut self, directory: impl Into<PathBuf>) -> Self {
        self.config_directory = Some(directory.into());
        self
    }

    fn build_claude(
        &self,
        working_directory: Option<&Path>,
        config_directory: Option<&Path>,
    ) -> Result<Claude, AgentError> {
        let mut builder = Claude::builder();
        if let Some(binary) = &self.binary {
            builder = builder.binary(binary);
        }
        if let Some(directory) = working_directory {
            builder = builder.working_dir(directory);
        }
        if let Some(directory) = config_directory {
            let directory = directory.to_str().ok_or_else(|| {
                AgentError::new(
                    ErrorKind::Internal,
                    "configured Claude directory is not valid UTF-8",
                    FailurePhase::Launch,
                    EffectState::None,
                )
            })?;
            builder = builder.env("CLAUDE_CONFIG_DIR", directory);
        }
        builder
            .build()
            .map_err(|error| launch_error(format!("claude unavailable: {error}")))
    }
}

impl Default for ClaudeService {
    fn default() -> Self {
        Self::new()
    }
}

impl Service<AgentRequest<Turn<ClaudeOptions>>> for ClaudeService {
    type Response = TurnOutcome;
    type Error = AgentError;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: AgentRequest<Turn<ClaudeOptions>>) -> Self::Future {
        if request.context.cancellation().is_cancelled() {
            return Box::pin(async { Err(cancelled_before_launch()) });
        }
        if request.body.prompt.trim().is_empty() {
            return Box::pin(async {
                Err(AgentError::invalid_request("prompt must not be empty"))
            });
        }
        if let Some(session) = &request.body.session
            && session.provider() != PROVIDER
        {
            let found = session.provider().to_string();
            return Box::pin(async move {
                Err(AgentError::new(
                    ErrorKind::Unsupported,
                    format!("cannot resume {found} session with Claude service"),
                    FailurePhase::Validation,
                    EffectState::None,
                ))
            });
        }
        if request
            .body
            .session
            .as_ref()
            .is_some_and(|session| session.value().trim().is_empty())
        {
            return Box::pin(async {
                Err(AgentError::invalid_request(
                    "Claude session handle must not be empty",
                ))
            });
        }

        let query = match build_native_query(&request.body) {
            Ok(query) => query,
            Err(error) => return Box::pin(async move { Err(error) }),
        };
        let claude = match self.build_claude(
            request.body.working_directory.as_deref(),
            self.config_directory.as_deref(),
        ) {
            Ok(claude) => claude,
            Err(error) => return Box::pin(async move { Err(error) }),
        };
        let observer = request.context.events().clone();
        let cancellation = request.context.cancellation().clone();
        let prior_session = request
            .body
            .session
            .as_ref()
            .map(|session| session.value().to_string());

        Box::pin(run_native(
            claude,
            query,
            observer,
            cancellation,
            prior_session,
        ))
    }
}

fn build_native_query(turn: &Turn<ClaudeOptions>) -> Result<QueryCommand, AgentError> {
    let options = &turn.options;
    if options
        .model
        .as_ref()
        .is_some_and(|model| model.trim().is_empty())
    {
        return Err(AgentError::invalid_request(
            "Claude model must not be empty",
        ));
    }
    if options.max_turns == Some(0) {
        return Err(AgentError::invalid_request(
            "Claude maximum turns must be greater than zero",
        ));
    }
    if options
        .allowed_tools
        .iter()
        .chain(&options.disallowed_tools)
        .any(|tool| tool.trim().is_empty())
    {
        return Err(AgentError::invalid_request(
            "Claude tool patterns must not be empty",
        ));
    }
    let mut command = QueryCommand::new(turn.prompt.clone());
    if let Some(prompt) = &options.system_prompt {
        command = command.system_prompt(prompt);
    }
    if let Some(prompt) = &options.append_system_prompt {
        command = command.append_system_prompt(prompt);
    }
    if let Some(model) = &options.model {
        command = command.model(model);
    }
    if let Some(effort) = options.effort {
        command = command.effort(effort.into());
    }
    if let Some(session) = &turn.session {
        command = command.resume(session.value());
    }
    if let Some(turns) = options.max_turns {
        command = command.max_turns(turns);
    }
    if !options.allowed_tools.is_empty() {
        command = command.allowed_tools(options.allowed_tools.iter().cloned());
    }
    if !options.disallowed_tools.is_empty() {
        command = command.disallowed_tools(options.disallowed_tools.iter().cloned());
    }
    for directory in &options.additional_directories {
        let directory = directory.to_str().ok_or_else(|| {
            AgentError::invalid_request("additional Claude directories must be valid UTF-8")
        })?;
        command = command.add_dir(directory);
    }

    Ok(command.prompt_via_stdin(true))
}

async fn run_native(
    claude: Claude,
    query: QueryCommand,
    observer: tower_agent::EventObserver,
    cancellation: tower_agent::CancellationToken,
    prior_session: Option<String>,
) -> Result<TurnOutcome, AgentError> {
    if cancellation.is_cancelled() {
        return Err(cancelled_before_launch());
    }

    let _ = observer.try_emit(AgentEvent::Started);
    let result = query
        .execute_json(&claude)
        .await
        .map_err(map_wrapper_error)?;
    if result.is_error {
        return Err(map_query_error(&result));
    }

    let _ = observer.try_emit(AgentEvent::OutputDelta {
        text: result.result.clone(),
    });
    let mut outcome = TurnOutcome::new(result.result);
    outcome.session = (!result.session_id.is_empty())
        .then_some(result.session_id)
        .or(prior_session)
        .map(|value| SessionHandle::new(PROVIDER, value));
    outcome.cost = result.cost_usd.map(Cost::usd);
    outcome.duration = result.duration_ms.map(Duration::from_millis);
    outcome.provider_turns = result.num_turns;
    Ok(outcome)
}

fn cancelled_before_launch() -> AgentError {
    AgentError::new(
        ErrorKind::Cancelled,
        "Claude turn was cancelled before launch",
        FailurePhase::Admission,
        EffectState::None,
    )
}

fn map_query_error(result: &QueryResult) -> AgentError {
    let kind = match result.extra.get("subtype").and_then(|value| value.as_str()) {
        Some("error_max_turns") => ErrorKind::Limit,
        Some("error_max_budget_usd") => ErrorKind::Budget,
        _ => ErrorKind::Provider,
    };
    AgentError::new(
        kind,
        result.result.clone(),
        FailurePhase::Running,
        EffectState::Possible,
    )
}

fn launch_error(message: impl Into<String>) -> AgentError {
    AgentError::new(
        ErrorKind::Provider,
        message,
        FailurePhase::Launch,
        EffectState::None,
    )
}

fn map_wrapper_error(error: claude_wrapper::Error) -> AgentError {
    use claude_wrapper::Error;

    let (kind, message, phase, effects) = match error {
        Error::NotFound => (
            ErrorKind::Provider,
            "claude binary not found".to_string(),
            FailurePhase::Launch,
            EffectState::None,
        ),
        Error::Timeout { timeout_seconds } => (
            ErrorKind::DeadlineExceeded,
            format!("claude command timed out after {timeout_seconds}s"),
            FailurePhase::Running,
            EffectState::Possible,
        ),
        // Authentication classification is post-hoc over a failed invocation,
        // so even credential-shaped failures cannot prove that no earlier turn
        // or tool in the invocation produced effects.
        Error::Auth { kind, .. } => match kind {
            claude_wrapper::auth::AuthErrorKind::NotAuthenticated => (
                ErrorKind::Authentication,
                "Claude credentials are not configured".to_string(),
                FailurePhase::Running,
                EffectState::Possible,
            ),
            claude_wrapper::auth::AuthErrorKind::Expired => (
                ErrorKind::Authentication,
                "Claude credentials have expired".to_string(),
                FailurePhase::Running,
                EffectState::Possible,
            ),
            claude_wrapper::auth::AuthErrorKind::InvalidCredentials => (
                ErrorKind::Authentication,
                "Claude credentials were rejected".to_string(),
                FailurePhase::Running,
                EffectState::Possible,
            ),
            claude_wrapper::auth::AuthErrorKind::RateLimit => (
                ErrorKind::Limit,
                "Claude request was rate limited".to_string(),
                FailurePhase::Running,
                EffectState::Possible,
            ),
            claude_wrapper::auth::AuthErrorKind::ProviderError => (
                ErrorKind::Provider,
                "Claude authentication provider failed".to_string(),
                FailurePhase::Running,
                EffectState::Possible,
            ),
            claude_wrapper::auth::AuthErrorKind::Other => (
                ErrorKind::Provider,
                "Claude provider rejected the request".to_string(),
                FailurePhase::Running,
                EffectState::Possible,
            ),
        },
        Error::MaxTurnsExceeded { max_turns, .. } => (
            ErrorKind::Limit,
            max_turns.map_or_else(
                || "Claude reached its maximum turn limit".to_string(),
                |turns| format!("Claude reached its maximum turn limit of {turns}"),
            ),
            FailurePhase::Running,
            EffectState::Possible,
        ),
        Error::MaxBudgetExceeded { max_usd, .. } => (
            ErrorKind::Budget,
            max_usd.map_or_else(
                || "Claude reached its budget limit".to_string(),
                |amount| format!("Claude reached its budget limit of ${amount:.2}"),
            ),
            FailurePhase::Running,
            EffectState::Possible,
        ),
        Error::BudgetExceeded { .. } => (
            ErrorKind::Budget,
            "Claude wrapper budget exceeded before dispatch".to_string(),
            FailurePhase::Admission,
            EffectState::None,
        ),
        Error::VersionMismatch { found, minimum } => (
            ErrorKind::Unsupported,
            format!("Claude CLI {found} is older than required version {minimum}"),
            FailurePhase::Launch,
            EffectState::None,
        ),
        Error::Io { message, .. } if message.starts_with("failed to spawn claude") => (
            ErrorKind::Provider,
            format!("Claude process launch failed: {message}"),
            FailurePhase::Launch,
            EffectState::None,
        ),
        Error::Io { message, .. } => (
            ErrorKind::Provider,
            format!("Claude process I/O failed: {message}"),
            FailurePhase::Running,
            EffectState::Possible,
        ),
        Error::Json { .. } => (
            ErrorKind::Provider,
            "Claude returned an invalid terminal result".to_string(),
            FailurePhase::Settlement,
            EffectState::Possible,
        ),
        Error::CommandFailed { exit_code, .. } => (
            ErrorKind::Provider,
            format!("Claude command failed with exit code {exit_code}"),
            FailurePhase::Running,
            EffectState::Possible,
        ),
        _ => (
            ErrorKind::Provider,
            "Claude provider failed".to_string(),
            FailurePhase::Running,
            EffectState::Possible,
        ),
    };
    AgentError::new(kind, message, phase, effects)
}

#[cfg(feature = "legacy-server")]
pub struct ClaudeBackend {
    timeout: Duration,
    binary: Option<PathBuf>,
}

#[cfg(feature = "legacy-server")]
impl ClaudeBackend {
    pub fn new(timeout: Duration) -> Self {
        Self {
            timeout,
            binary: None,
        }
    }

    fn build_legacy_claude(&self, params: &Params) -> Result<Claude, BackendError> {
        let cwd = params.cwd.clone().unwrap_or_else(|| ".".to_string());
        let timeout = params
            .timeout
            .map(Duration::from_secs)
            .unwrap_or(self.timeout);
        let mut builder = Claude::builder().working_dir(cwd).timeout(timeout);
        if let Some(binary) = &self.binary {
            builder = builder.binary(binary);
        }
        if let Some(dir) = &params.config_dir {
            builder = builder.env("CLAUDE_CONFIG_DIR", dir);
        }
        builder
            .build()
            .map_err(|error| BackendError::new(format!("claude unavailable: {error}")))
    }
}

/// Parse an effort string (case-insensitive) into the wrapper's [`Effort`].
#[cfg(feature = "legacy-server")]
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
#[cfg(feature = "legacy-server")]
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

/// The report schema, hand-written with no `$schema` key (the CLI rejects the
/// draft URL that schemars emits).
#[cfg(feature = "legacy-server")]
fn report_schema() -> String {
    r#"{"type":"object","properties":{"summary":{"type":"string"},"reply":{"type":"string"},"posts":{"type":"array","items":{"type":"object","properties":{"channel":{"type":"string"},"body":{"type":"string"},"to":{"type":"string"},"reply_to":{"type":"integer"}},"required":["channel","body"]}}},"required":["summary"]}"#.to_string()
}

/// Instructions appended to the system prompt for a structured turn, explaining
/// the report the schema enforces.
#[cfg(feature = "legacy-server")]
const REPORT_CONTRACT: &str = "When you finish, return a JSON object matching the schema. `summary` \
    is one line for the operator's log. `reply` is your actual answer to whoever invoked you (the \
    work product, not a log line). `posts` are messages to other agents: each has a `channel`, a \
    `body`, and optionally `to` (address one agent directly, so it reaches them even if they do \
    not subscribe to the channel) and `reply_to` (the id of the message you are answering, to \
    thread it). Post when another agent should react; otherwise leave posts empty.";

/// Parse a structured report into an outcome, falling back to a plain reply if
/// the model did not return the expected JSON.
#[cfg(feature = "legacy-server")]
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

/// Map a stream event to the tower-agent [`Event`]s it produces: a new assistant
/// message is a turn boundary; a partial message carries text, thinking, or a
/// tool-use start. `turn` counts assistant messages.
#[cfg(feature = "legacy-server")]
fn classify(event: &StreamEvent, turn: &mut u32) -> Vec<Event> {
    if event.event_type() == Some("assistant") {
        *turn += 1;
        return vec![Event::Turn { n: *turn }];
    }
    match event.partial_message() {
        Some(PartialMessageEvent::BlockStart {
            block_type: BlockType::ToolUse { name, .. },
            ..
        }) => vec![Event::ToolUse { name }],
        Some(PartialMessageEvent::BlockDelta {
            delta: BlockDelta::Text(t),
            ..
        }) => vec![Event::TextDelta(t)],
        Some(PartialMessageEvent::BlockDelta {
            delta: BlockDelta::Thinking(t),
            ..
        }) => vec![Event::Thinking(t)],
        _ => vec![],
    }
}

#[cfg(feature = "legacy-server")]
#[async_trait]
impl Backend for ClaudeBackend {
    fn name(&self) -> &str {
        "claude"
    }

    async fn run(&self, params: &Params) -> Result<Outcome, BackendError> {
        let claude = self.build_legacy_claude(params)?;
        // Send the prompt over stdin so it does not appear in argv (visible to
        // `ps` and crash dumps). The streaming path cannot do this yet (the
        // wrapper nulls the child's stdin there), so it stays argv for now.
        let mut cmd = build_query(params).prompt_via_stdin(true);
        if params.structured {
            cmd = cmd
                .json_schema(report_schema())
                .append_system_prompt(REPORT_CONTRACT);
        }
        match cmd.execute_json(&claude).await {
            Ok(qr) if qr.is_error => Err(BackendError::new(qr.result)),
            Ok(qr) => {
                let cost = qr.cost_usd;
                let session = (!qr.session_id.is_empty()).then_some(qr.session_id);
                let mut outcome = if params.structured {
                    parse_report(&qr.result, session)
                } else {
                    Outcome::from_reply(qr.result, session)
                };
                outcome.cost_usd = cost;
                Ok(outcome)
            }
            Err(e) => Err(BackendError::new(format!("run failed: {e}"))),
        }
    }

    async fn run_streaming(
        &self,
        params: &Params,
        events: UnboundedSender<Event>,
    ) -> Result<Outcome, BackendError> {
        let claude = self.build_legacy_claude(params)?;
        // Stream JSON with partial messages so assistant text arrives as deltas.
        let cmd = build_query(params)
            .output_format(OutputFormat::StreamJson)
            .include_partial_messages();

        let mut final_result: Option<QueryResult> = None;
        let mut session_seen: Option<String> = None;
        let mut accumulated = String::new();
        let mut turn = 0u32;

        let outcome = stream_query(&claude, &cmd, |event: StreamEvent| {
            if session_seen.is_none()
                && let Some(id) = event.session_id()
            {
                session_seen = Some(id.to_string());
            }
            if event.is_result() {
                if let Ok(qr) = serde_json::from_value::<QueryResult>(event.data.clone()) {
                    final_result = Some(qr);
                }
                return;
            }
            for ev in classify(&event, &mut turn) {
                if let Event::TextDelta(t) = &ev {
                    accumulated.push_str(t);
                }
                // The receiver may be gone (caller stopped listening); ignore.
                let _ = events.send(ev);
            }
        })
        .await;
        outcome.map_err(|e| BackendError::new(format!("stream failed: {e}")))?;

        let (reply, session_id, cost) = match final_result {
            Some(qr) if qr.is_error => return Err(BackendError::new(qr.result)),
            Some(qr) => (qr.result, qr.session_id, qr.cost_usd),
            None => (accumulated, session_seen.unwrap_or_default(), None),
        };
        let mut outcome =
            Outcome::from_reply(reply, (!session_id.is_empty()).then_some(session_id));
        outcome.cost_usd = cost;
        Ok(outcome)
    }
}

#[cfg(test)]
mod native_tests {
    use tower::ServiceExt;
    use tower_agent::{CallContext, CancellationToken, EventObserver};

    use super::*;

    fn fake_claude() -> Claude {
        Claude::builder().binary("/usr/bin/true").build().unwrap()
    }

    #[test]
    fn native_options_map_to_the_query_without_a_server_type() {
        let options = ClaudeOptions {
            system_prompt: Some("You are the tester.".into()),
            append_system_prompt: Some("Be brief.".into()),
            model: Some("haiku".into()),
            effort: Some(ClaudeEffort::High),
            allowed_tools: vec!["Bash(cargo test:*)".into()],
            disallowed_tools: vec!["Bash(rm:*)".into()],
            additional_directories: vec![PathBuf::from("/shared")],
            max_turns: Some(4),
        };
        let turn = Turn::new("run the tests")
            .resume(SessionHandle::new(PROVIDER, "sess-123"))
            .with_options(options);

        let rendered = build_native_query(&turn)
            .expect("valid query")
            .to_command_string(&fake_claude());
        assert!(
            !rendered.contains("run the tests"),
            "prompt must travel over stdin: {rendered}"
        );
        for expected in [
            "You are the tester.",
            "Be brief.",
            "haiku",
            "cargo test",
            "Bash(rm",
            "/shared",
            "sess-123",
        ] {
            assert!(
                rendered.contains(expected),
                "missing {expected:?}: {rendered}"
            );
        }
    }

    #[tokio::test]
    async fn rejects_foreign_sessions_before_launch() {
        let service = ClaudeService::new().with_binary("/definitely/not/a/claude/binary");
        let turn = Turn::new("hello")
            .resume(SessionHandle::new("codex", "session"))
            .with_options(ClaudeOptions::default());

        let error = service
            .oneshot(AgentRequest::new(turn))
            .await
            .expect_err("foreign session must be rejected");
        assert_eq!(error.kind, ErrorKind::Unsupported);
        assert_eq!(error.phase, FailurePhase::Validation);
        assert_eq!(error.effects, EffectState::None);
    }

    #[tokio::test]
    async fn rejects_pre_cancelled_calls_before_launch() {
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let request = AgentRequest::with_context(
            Turn::new("hello").with_options(ClaudeOptions::default()),
            CallContext::new().with_cancellation(cancellation),
        );
        let service = ClaudeService::new().with_binary("/definitely/not/a/claude/binary");

        let error = service
            .oneshot(request)
            .await
            .expect_err("cancelled call must not launch");
        assert_eq!(error.kind, ErrorKind::Cancelled);
        assert_eq!(error.phase, FailurePhase::Admission);
        assert_eq!(error.effects, EffectState::None);
    }

    #[tokio::test]
    async fn rejects_empty_session_handles_before_launch() {
        let turn = Turn::new("hello")
            .resume(SessionHandle::new(PROVIDER, "  "))
            .with_options(ClaudeOptions::default());
        let error = ClaudeService::new()
            .with_binary("/definitely/not/a/claude/binary")
            .oneshot(AgentRequest::new(turn))
            .await
            .expect_err("empty session must be rejected");

        assert_eq!(error.kind, ErrorKind::InvalidRequest);
        assert_eq!(error.phase, FailurePhase::Validation);
        assert_eq!(error.effects, EffectState::None);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn missing_terminal_json_is_a_settlement_failure() {
        let error = ClaudeService::new()
            .with_binary("/usr/bin/true")
            .oneshot(AgentRequest::new(
                Turn::new("hello").with_options(ClaudeOptions::default()),
            ))
            .await
            .expect_err("empty output is not a successful turn");

        assert_eq!(error.kind, ErrorKind::Provider);
        assert_eq!(error.phase, FailurePhase::Settlement);
        assert_eq!(error.effects, EffectState::Possible);
    }

    #[test]
    fn terminal_limit_subtype_stays_typed() {
        let mut extra = std::collections::HashMap::new();
        extra.insert("subtype".into(), serde_json::json!("error_max_turns"));
        let result = QueryResult {
            result: "maximum turns reached".into(),
            session_id: "session-1".into(),
            cost_usd: None,
            duration_ms: None,
            num_turns: Some(4),
            is_error: true,
            extra,
        };

        let error = map_query_error(&result);
        assert_eq!(error.kind, ErrorKind::Limit);
        assert_eq!(error.phase, FailurePhase::Running);
        assert_eq!(error.effects, EffectState::Possible);
    }

    #[test]
    fn terminal_budget_subtype_stays_typed() {
        let mut extra = std::collections::HashMap::new();
        extra.insert("subtype".into(), serde_json::json!("error_max_budget_usd"));
        let result = QueryResult {
            result: "maximum budget reached".into(),
            session_id: "session-1".into(),
            cost_usd: Some(1.0),
            duration_ms: None,
            num_turns: Some(2),
            is_error: true,
            extra,
        };

        let error = map_query_error(&result);
        assert_eq!(error.kind, ErrorKind::Budget);
        assert_eq!(error.phase, FailurePhase::Running);
        assert_eq!(error.effects, EffectState::Possible);
    }

    #[test]
    fn invalid_native_options_are_refused_before_launch() {
        let turn = Turn::new("hello").with_options(ClaudeOptions {
            max_turns: Some(0),
            ..Default::default()
        });

        let error = build_native_query(&turn).expect_err("zero turns is invalid");
        assert_eq!(error.kind, ErrorKind::InvalidRequest);
        assert_eq!(error.phase, FailurePhase::Validation);
        assert_eq!(error.effects, EffectState::None);
    }

    #[test]
    fn authentication_subtypes_keep_conservative_effect_evidence() {
        use claude_wrapper::Error;
        use claude_wrapper::auth::AuthErrorKind;

        let cases = [
            (
                AuthErrorKind::NotAuthenticated,
                ErrorKind::Authentication,
                EffectState::Possible,
            ),
            (
                AuthErrorKind::Expired,
                ErrorKind::Authentication,
                EffectState::Possible,
            ),
            (
                AuthErrorKind::InvalidCredentials,
                ErrorKind::Authentication,
                EffectState::Possible,
            ),
            (
                AuthErrorKind::RateLimit,
                ErrorKind::Limit,
                EffectState::Possible,
            ),
            (
                AuthErrorKind::ProviderError,
                ErrorKind::Provider,
                EffectState::Possible,
            ),
            (
                AuthErrorKind::Other,
                ErrorKind::Provider,
                EffectState::Possible,
            ),
        ];

        for (kind, expected_kind, expected_effects) in cases {
            let error = map_wrapper_error(Error::Auth {
                kind,
                command: "claude --resume private-session".into(),
                exit_code: 1,
                message: "rejected private-session".into(),
            });
            assert_eq!(error.kind, expected_kind);
            assert_eq!(error.phase, FailurePhase::Running);
            assert_eq!(error.effects, expected_effects);
            assert!(!error.message.contains("private-session"));
        }
    }

    #[test]
    fn command_failures_do_not_expose_provider_diagnostics() {
        let error = map_wrapper_error(claude_wrapper::Error::CommandFailed {
            command: "claude --resume private-session".into(),
            exit_code: 7,
            stdout: "private-session".into(),
            stderr: "failed for private-session".into(),
            working_dir: Some(PathBuf::from("/private/worktree")),
        });

        assert_eq!(error.kind, ErrorKind::Provider);
        assert_eq!(error.phase, FailurePhase::Running);
        assert_eq!(error.effects, EffectState::Possible);
        assert_eq!(error.message, "Claude command failed with exit code 7");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn returns_terminal_evidence_without_putting_the_prompt_in_argv() {
        use std::os::unix::fs::PermissionsExt;
        use std::sync::atomic::{AtomicU64, Ordering};

        static NEXT_SCRIPT: AtomicU64 = AtomicU64::new(1);
        let path = std::env::temp_dir().join(format!(
            "tower-agent-claude-{}-{}.sh",
            std::process::id(),
            NEXT_SCRIPT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(
            &path,
            concat!(
                "#!/bin/sh\n",
                "case \" $* \" in *\" hello \"*) exit 91;; esac\n",
                "prompt=$(cat)\n",
                "[ \"$prompt\" = \"hello\" ] || exit 92\n",
                "printf '%s\\n' '{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"hello\",\"session_id\":\"native-session\",\"total_cost_usd\":0.01,\"duration_ms\":12,\"num_turns\":1,\"is_error\":false}'\n",
            ),
        )
        .expect("write fake Claude CLI");
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&path, permissions).unwrap();

        let (events, mut receiver) = EventObserver::channel(4);
        let request = AgentRequest::with_context(
            Turn::new("hello").with_options(ClaudeOptions::default()),
            CallContext::new().with_events(events),
        );
        let outcome = ClaudeService::new()
            .with_binary(&path)
            .oneshot(request)
            .await
            .expect("fake Claude run succeeds");
        let _ = std::fs::remove_file(path);

        assert_eq!(outcome.output, "hello");
        assert_eq!(
            outcome.session.as_ref().map(SessionHandle::provider),
            Some(PROVIDER)
        );
        assert_eq!(
            outcome.session.as_ref().map(SessionHandle::value),
            Some("native-session")
        );
        assert_eq!(outcome.cost, Some(Cost::usd(0.01)));
        assert_eq!(outcome.duration, Some(Duration::from_millis(12)));
        assert_eq!(outcome.provider_turns, Some(1));
        assert_eq!(receiver.recv().await, Some(AgentEvent::Started));
        assert_eq!(
            receiver.recv().await,
            Some(AgentEvent::OutputDelta {
                text: "hello".into()
            })
        );
    }
}

#[cfg(all(test, feature = "legacy-server"))]
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
            outcome.reply.to_lowercase().contains("pong"),
            "got: {}",
            outcome.reply
        );
        assert!(outcome.session.is_some());
    }

    // Live streaming smoke test: needs the claude CLI and auth.
    //   cargo test -p tower-agent-claude -- --ignored
    #[tokio::test]
    #[ignore = "needs the claude CLI and auth"]
    async fn live_streaming_emits_deltas() {
        let backend = ClaudeBackend::new(Duration::from_secs(120));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let params = Params {
            prompt: "Count from 1 to 5, one number per line.".into(),
            model: Some("haiku".into()),
            ..Default::default()
        };
        let outcome = backend.run_streaming(&params, tx).await.expect("run");
        let mut deltas = 0;
        while rx.try_recv().is_ok() {
            deltas += 1;
        }
        assert!(deltas > 0, "expected streamed text deltas, got none");
        assert!(outcome.reply.contains('5'), "final text: {}", outcome.reply);
        assert!(outcome.session.is_some());
    }

    // Live session test: a minted id resumes the thread (real memory) and the
    // registry records the turns.
    //   cargo test -p tower-agent-claude -- --ignored
    #[tokio::test]
    #[ignore = "needs the claude CLI and auth"]
    async fn live_session_resume_and_registry() {
        use std::sync::Arc;
        use tower_agent_server::{Call, Config, Server};

        let server = Server::new(
            Config::default(),
            Arc::new(ClaudeBackend::new(Duration::from_secs(120))),
        );
        let first = server
            .run(Call {
                prompt: "Remember the word Kestrel. Acknowledge in one word.".into(),
                model: Some("haiku".into()),
                ..Default::default()
            })
            .await
            .expect("turn 1");
        let id = first.session.clone().expect("a minted session id");

        let second = server
            .run(Call {
                prompt: "What word did I ask you to remember? Reply with just that word.".into(),
                model: Some("haiku".into()),
                session: Some(id.clone()),
                ..Default::default()
            })
            .await
            .expect("turn 2");
        assert!(
            second.reply.to_lowercase().contains("kestrel"),
            "resumed thread should recall the word, got: {}",
            second.reply
        );
        assert_eq!(second.session.as_deref(), Some(id.as_str()));

        let info = server.sessions().get(&id).expect("session in registry");
        assert_eq!(info.turns, 2);
    }

    #[test]
    fn classify_counts_assistant_messages_as_turns() {
        use serde_json::json;
        let mut turn = 0;
        let a1: StreamEvent = serde_json::from_value(json!({"type":"assistant"})).unwrap();
        assert!(matches!(
            classify(&a1, &mut turn).as_slice(),
            [Event::Turn { n: 1 }]
        ));
        let a2: StreamEvent = serde_json::from_value(json!({"type":"assistant"})).unwrap();
        assert!(matches!(
            classify(&a2, &mut turn).as_slice(),
            [Event::Turn { n: 2 }]
        ));
    }

    #[test]
    fn classify_maps_text_tool_and_thinking() {
        use serde_json::json;
        let mut turn = 0;

        let text: StreamEvent = serde_json::from_value(
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}),
        )
        .unwrap();
        assert!(
            matches!(classify(&text, &mut turn).as_slice(), [Event::TextDelta(t)] if t.as_str() == "hi")
        );

        let tool: StreamEvent = serde_json::from_value(json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"x","name":"Bash"}})).unwrap();
        assert!(
            matches!(classify(&tool, &mut turn).as_slice(), [Event::ToolUse { name }] if name.as_str() == "Bash")
        );

        let think: StreamEvent = serde_json::from_value(json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hmm"}})).unwrap();
        assert!(
            matches!(classify(&think, &mut turn).as_slice(), [Event::Thinking(t)] if t.as_str() == "hmm")
        );
    }

    // Live: a streamed run that uses a tool emits ToolUse and Turn events.
    //   cargo test -p tower-agent-claude -- --ignored
    #[tokio::test]
    #[ignore = "needs the claude CLI and auth"]
    async fn live_streaming_emits_tool_use() {
        let backend = ClaudeBackend::new(Duration::from_secs(120));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let params = Params {
            prompt: "Use the Bash tool to run: echo hello. Then reply done.".into(),
            model: Some("haiku".into()),
            allowed_tools: Some(vec!["Bash".into()]),
            ..Default::default()
        };
        backend.run_streaming(&params, tx).await.expect("run");

        let mut tool_uses = 0;
        let mut turns = 0;
        while let Ok(ev) = rx.try_recv() {
            match ev {
                Event::ToolUse { .. } => tool_uses += 1,
                Event::Turn { .. } => turns += 1,
                _ => {}
            }
        }
        assert!(tool_uses >= 1, "expected a tool-use event");
        assert!(turns >= 1, "expected a turn event");
    }
}
