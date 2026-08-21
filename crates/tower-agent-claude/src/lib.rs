//! A Tower-native Claude Code provider for `tower-agent`.
//!
//! [`ClaudeService`] implements
//! `Service<AgentRequest<Turn<ClaudeOptions>>>` directly. User prompts travel
//! over buffered stdin; system-prompt flags remain in the child argument vector.
//! Headless calls leave permissions at the CLI default. Tools that require
//! approval need an explicit `allowed_tools` entry; this adapter does not expose
//! a bypass-all control.
//!
//! Calls use the wrapper's process-group ownership and bridge the request's
//! cancellation token into its awaited cancellation path. On Unix, the wrapper
//! terminates the complete provider process group and reaps the direct child
//! before returning a terminal cancellation result. Timeout and stdin setup
//! failures use the same ownership path. On non-Unix platforms, cleanup awaits
//! the direct child but cannot guarantee ownership of its descendants.
//!
//! [`ClaudeAmbientContext`] keeps inherited, setting-source hermetic, safe, and
//! bare modes distinct. The service can require a host-owned baseline that a
//! remote turn cannot weaken. These are provider context controls rather than
//! OS isolation; child environment and filesystem visibility remain separate.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use claude_wrapper::types::QueryResult;
use claude_wrapper::{Claude, Effort, HermeticScope, QueryCommand};
use tower::Service;
use tower_agent::{
    AgentError, AgentEvent, AgentRequest, ChildEnvironmentPolicy, Cost, EffectState, ErrorKind,
    FailureEvidence, FailurePhase, SessionHandle, TokenUsage, Turn, TurnOutcome,
};

const PROVIDER: &str = "claude";

/// Provider-specific controls for one Claude Code turn.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ClaudeOptions {
    pub system_prompt: Option<String>,
    pub append_system_prompt: Option<String>,
    pub model: Option<String>,
    /// Model to fall back to when the primary model is overloaded.
    pub fallback_model: Option<String>,
    pub effort: Option<ClaudeEffort>,
    pub allowed_tools: Vec<String>,
    pub disallowed_tools: Vec<String>,
    pub additional_directories: Vec<PathBuf>,
    pub max_turns: Option<u32>,
    /// CLI-side spend ceiling for the turn, in USD. Exceeding it surfaces
    /// as a typed budget failure with resume and spend evidence.
    pub max_budget_usd: Option<f64>,
    /// Permission posture for the turn. Bypass-all is deliberately not
    /// representable here; this adapter does not expose that control.
    pub permission_mode: Option<ClaudePermissionMode>,
    /// JSON Schema the terminal result must validate against.
    pub json_schema: Option<String>,
    /// When true, the CLI ignores all configured MCP servers (user scope
    /// and any project .mcp.json at the working directory) and boots none.
    /// Hosts that queue turns from inside a project directory want this:
    /// a project .mcp.json that registers the host itself would otherwise
    /// make every turn boot a nested host instance as an MCP server.
    pub strict_mcp_config: bool,
    /// Requested ambient-context mode. The service's host-owned baseline is
    /// always applied and cannot be weakened by a turn.
    pub ambient_context: ClaudeAmbientContext,
}

/// Mutually exclusive Claude ambient-context modes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ClaudeAmbientContext {
    /// Preserve the CLI's normal ambient behavior unless the host requires a
    /// stronger baseline.
    #[default]
    Inherit,
    /// Seal selected setting sources and MCP configuration while preserving
    /// normal OAuth and keychain authentication.
    Hermetic(ClaudeHermetic),
    /// Disable customizations while preserving normal authentication.
    Safe,
    /// Use the most minimal scripted mode. OAuth and keychain authentication
    /// are unavailable; use an API key or explicit helper.
    Bare,
}

/// Setting-source scope used by [`ClaudeAmbientContext::Hermetic`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClaudeHermetic {
    /// Drop user, project, and local setting sources.
    Full,
    /// Keep only the user's global scope; seal project and local.
    Project,
}

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

/// Permission posture for one turn, mapping to `--permission-mode`.
///
/// The wrapper's bypass-all mode is intentionally absent: it stays behind
/// `claude_wrapper::dangerous::DangerousClient`, and this adapter does not
/// expose it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ClaudePermissionMode {
    /// Default interactive permissions (headless calls deny on prompt).
    #[default]
    Default,
    /// Auto-accept file edits.
    AcceptEdits,
    /// Deny anything that would ask.
    DontAsk,
    /// Plan mode (read-only).
    Plan,
    /// Auto mode.
    Auto,
}

impl From<ClaudePermissionMode> for claude_wrapper::PermissionMode {
    fn from(value: ClaudePermissionMode) -> Self {
        match value {
            ClaudePermissionMode::Default => Self::Default,
            ClaudePermissionMode::AcceptEdits => Self::AcceptEdits,
            ClaudePermissionMode::DontAsk => Self::DontAsk,
            ClaudePermissionMode::Plan => Self::Plan,
            ClaudePermissionMode::Auto => Self::Auto,
        }
    }
}

/// A cloneable finite-turn service backed by the Claude Code CLI.
#[derive(Clone, Debug)]
pub struct ClaudeService {
    binary: Option<PathBuf>,
    config_directory: Option<PathBuf>,
    kill_grace: Option<Duration>,
    child_environment: ChildEnvironmentPolicy,
    ambient_context: ClaudeAmbientContext,
}

impl ClaudeService {
    /// Create a service using the wrapper's default process-group ownership.
    pub fn new() -> Self {
        Self {
            binary: None,
            config_directory: None,
            kill_grace: None,
            child_environment: ChildEnvironmentPolicy::default(),
            ambient_context: ClaudeAmbientContext::default(),
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

    /// Set how long cancellation waits before forcing the owned Claude
    /// process group to stop.
    pub fn with_kill_grace(mut self, duration: Duration) -> Self {
        self.kill_grace = Some(duration);
        self
    }

    /// Set the host-owned child environment policy. The compatibility default
    /// inherits the complete host environment.
    pub fn with_child_environment_policy(mut self, policy: ChildEnvironmentPolicy) -> Self {
        self.child_environment = policy;
        self
    }

    pub const fn child_environment_policy(&self) -> &ChildEnvironmentPolicy {
        &self.child_environment
    }

    /// Set the host-owned minimum ambient-context mode. A turn may strengthen
    /// a hermetic setting-source seal but cannot replace safe or bare mode with
    /// a conflicting posture.
    pub fn with_ambient_context_policy(mut self, policy: ClaudeAmbientContext) -> Self {
        self.ambient_context = policy;
        self
    }

    pub const fn ambient_context_policy(&self) -> ClaudeAmbientContext {
        self.ambient_context
    }

    fn build_claude(
        &self,
        working_directory: Option<&Path>,
        config_directory: Option<&Path>,
    ) -> Result<Claude, AgentError> {
        let mut builder = Claude::builder();
        let environment = self.child_environment.resolve().map_err(|error| {
            AgentError::new(
                ErrorKind::Internal,
                format!("invalid Claude child environment policy: {error}"),
                FailurePhase::Launch,
                EffectState::None,
            )
        })?;
        if environment.clear_inherited() {
            builder = builder.clear_env();
        }
        builder = builder.envs(environment.variables());
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
        if let Some(duration) = self.kill_grace {
            builder = builder.kill_grace(duration);
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

        let ambient_context = match effective_ambient_context(
            self.ambient_context,
            request.body.options.ambient_context,
        ) {
            Ok(context) => context,
            Err(error) => return Box::pin(async move { Err(error) }),
        };
        let query = match build_query(&request.body, ambient_context) {
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

        Box::pin(run(claude, query, observer, cancellation, prior_session))
    }
}

fn effective_ambient_context(
    host: ClaudeAmbientContext,
    requested: ClaudeAmbientContext,
) -> Result<ClaudeAmbientContext, AgentError> {
    match (host, requested) {
        (ClaudeAmbientContext::Inherit, requested) => Ok(requested),
        (host, ClaudeAmbientContext::Inherit) => Ok(host),
        (host, requested) if host == requested => Ok(host),
        (
            ClaudeAmbientContext::Hermetic(ClaudeHermetic::Project),
            ClaudeAmbientContext::Hermetic(ClaudeHermetic::Full),
        )
        | (
            ClaudeAmbientContext::Hermetic(ClaudeHermetic::Full),
            ClaudeAmbientContext::Hermetic(ClaudeHermetic::Project),
        ) => Ok(ClaudeAmbientContext::Hermetic(ClaudeHermetic::Full)),
        _ => Err(AgentError::invalid_request(
            "requested Claude ambient-context mode conflicts with the host baseline",
        )),
    }
}

fn build_query(
    turn: &Turn<ClaudeOptions>,
    ambient_context: ClaudeAmbientContext,
) -> Result<QueryCommand, AgentError> {
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
    if options
        .max_budget_usd
        .is_some_and(|budget| !budget.is_finite() || budget <= 0.0)
    {
        return Err(AgentError::invalid_request(
            "Claude budget must be a positive amount",
        ));
    }
    if options
        .fallback_model
        .as_ref()
        .is_some_and(|model| model.trim().is_empty())
    {
        return Err(AgentError::invalid_request(
            "Claude fallback model must not be empty",
        ));
    }
    if options
        .json_schema
        .as_ref()
        .is_some_and(|schema| schema.trim().is_empty())
    {
        return Err(AgentError::invalid_request(
            "Claude JSON schema must not be empty",
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
    if let Some(model) = &options.fallback_model {
        command = command.fallback_model(model);
    }
    if let Some(effort) = options.effort {
        command = command.effort(effort.into());
    }
    if let Some(budget) = options.max_budget_usd {
        command = command.max_budget_usd(budget);
    }
    if let Some(mode) = options.permission_mode {
        command = command.permission_mode(mode.into());
    }
    if let Some(schema) = &options.json_schema {
        command = command.json_schema(schema);
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

    if options.strict_mcp_config {
        command = command.strict_mcp_config();
    }
    command = match ambient_context {
        ClaudeAmbientContext::Inherit => command,
        ClaudeAmbientContext::Hermetic(hermetic) => command.hermetic_scoped(match hermetic {
            ClaudeHermetic::Full => HermeticScope::Full,
            ClaudeHermetic::Project => HermeticScope::Project,
        }),
        ClaudeAmbientContext::Safe => command.safe_mode(),
        ClaudeAmbientContext::Bare => command.bare(),
    };

    Ok(command.prompt_via_stdin(true))
}

async fn run(
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
        .execute_json_cancellable(&claude, cancellation.cancelled())
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
    outcome.usage = result
        .usage
        .as_ref()
        .filter(|usage| !usage.is_empty())
        .map(map_usage);
    outcome.cost = result.cost_usd.map(Cost::usd);
    outcome.duration = result.duration_ms.map(Duration::from_millis);
    outcome.provider_turns = result.num_turns;
    Ok(outcome)
}

fn map_usage(usage: &claude_wrapper::TokenUsage) -> TokenUsage {
    TokenUsage {
        input: usage.input_tokens,
        cached_input: usage.cached_input_tokens,
        cache_write_input: usage.cache_write_input_tokens,
        output: usage.output_tokens,
        reasoning_output: usage.reasoning_output_tokens,
        provider_total: (!usage.is_empty()).then(|| usage.total()),
    }
}

fn query_evidence(result: &QueryResult) -> FailureEvidence {
    FailureEvidence {
        session: (!result.session_id.is_empty())
            .then(|| SessionHandle::new(PROVIDER, result.session_id.clone())),
        usage: result
            .usage
            .as_ref()
            .filter(|usage| !usage.is_empty())
            .map(map_usage),
        cost: result.cost_usd.map(Cost::usd),
        duration: result.duration_ms.map(Duration::from_millis),
        provider_turns: result.num_turns,
    }
}

/// Bounded stderr tail so a terminal failure explains itself without
/// carrying unbounded provider output.
fn command_failed_message(provider: &str, exit_code: i32, stderr: &str) -> String {
    let tail: String = stderr
        .trim()
        .chars()
        .rev()
        .take(400)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    if tail.is_empty() {
        format!("{provider} command failed with exit code {exit_code}")
    } else {
        format!("{provider} command failed with exit code {exit_code}: {tail}")
    }
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
    .with_evidence(query_evidence(result))
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

    match error {
        Error::MaxTurnsExceeded {
            max_turns,
            cost_usd,
            num_turns,
            session_id,
            ..
        } => rail_stop_error(
            ErrorKind::Limit,
            max_turns.map_or_else(
                || "Claude reached its maximum turn limit".to_string(),
                |turns| format!("Claude reached its maximum turn limit of {turns}"),
            ),
            session_id,
            cost_usd,
            num_turns,
        ),
        Error::MaxBudgetExceeded {
            max_usd,
            cost_usd,
            num_turns,
            session_id,
            ..
        } => rail_stop_error(
            ErrorKind::Budget,
            max_usd.map_or_else(
                || "Claude reached its budget limit".to_string(),
                |amount| format!("Claude reached its budget limit of ${amount:.2}"),
            ),
            session_id,
            cost_usd,
            num_turns,
        ),
        error => map_other_wrapper_error(error),
    }
}

fn rail_stop_error(
    kind: ErrorKind,
    message: String,
    session_id: Option<String>,
    cost_usd: Option<f64>,
    provider_turns: Option<u32>,
) -> AgentError {
    AgentError::new(kind, message, FailurePhase::Running, EffectState::Possible).with_evidence(
        FailureEvidence {
            session: session_id.map(|value| SessionHandle::new(PROVIDER, value)),
            cost: cost_usd.map(Cost::usd),
            provider_turns,
            ..FailureEvidence::default()
        },
    )
}

fn map_other_wrapper_error(error: claude_wrapper::Error) -> AgentError {
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
        Error::Cancelled => (
            ErrorKind::Cancelled,
            "Claude turn was cancelled".to_string(),
            FailurePhase::Running,
            EffectState::Possible,
        ),
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
        Error::CommandFailed {
            exit_code, stderr, ..
        } => (
            ErrorKind::Provider,
            command_failed_message("Claude", exit_code, &stderr),
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

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    #[tokio::test]
    async fn clear_child_environment_keeps_only_allowed_and_explicit_values() {
        use std::os::unix::fs::PermissionsExt;

        let path = std::env::temp_dir().join(format!(
            "tower-agent-claude-environment-{}.sh",
            std::process::id()
        ));
        std::fs::write(
            &path,
            concat!(
                "#!/bin/sh\n",
                "[ -z \"${HOME+x}\" ] || exit 91\n",
                "[ -n \"$PATH\" ] || exit 92\n",
                "[ \"$TOWER_AGENT_EXPLICIT\" = \"visible\" ] || exit 93\n",
                "[ \"$CLAUDE_CONFIG_DIR\" = \"/host/claude\" ] || exit 94\n",
                "cat >/dev/null\n",
                "printf '%s\\n' '{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"ok\",\"session_id\":\"s\",\"is_error\":false}'\n",
            ),
        )
        .expect("write fake Claude CLI");
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&path, permissions).unwrap();

        let policy = ChildEnvironmentPolicy::clear()
            .allow_ambient("PATH")
            .with_variable("TOWER_AGENT_EXPLICIT", "visible");
        let outcome = ClaudeService::new()
            .with_binary(&path)
            .with_config_directory("/host/claude")
            .with_child_environment_policy(policy)
            .oneshot(AgentRequest::new(
                Turn::new("hello").with_options(ClaudeOptions::default()),
            ))
            .await
            .expect("filtered child environment reaches Claude");
        let _ = std::fs::remove_file(path);

        assert_eq!(outcome.output, "ok");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ambient_context_modes_reach_exact_fake_cli_argv() {
        use std::os::unix::fs::PermissionsExt;

        let path = std::env::temp_dir().join(format!(
            "tower-agent-claude-ambient-{}.sh",
            std::process::id()
        ));
        let argv_path = std::env::temp_dir().join(format!(
            "tower-agent-claude-ambient-{}.args",
            std::process::id()
        ));
        let script = format!(
            concat!(
                "#!/bin/sh\n",
                "printf '%s\\n' \"$@\" > '{}'\n",
                "cat >/dev/null\n",
                "printf '%s\\n' '{{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"ok\",\"session_id\":\"s\",\"is_error\":false}}'\n",
            ),
            argv_path.display()
        );
        std::fs::write(&path, script).expect("write fake Claude CLI");
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&path, permissions).unwrap();

        let cases = vec![
            (
                ClaudeAmbientContext::Hermetic(ClaudeHermetic::Full),
                vec![
                    "--print",
                    "--output-format",
                    "json",
                    "--strict-mcp-config",
                    "--setting-sources",
                    "",
                    "--exclude-dynamic-system-prompt-sections",
                ],
            ),
            (
                ClaudeAmbientContext::Hermetic(ClaudeHermetic::Project),
                vec![
                    "--print",
                    "--output-format",
                    "json",
                    "--strict-mcp-config",
                    "--setting-sources",
                    "user",
                    "--exclude-dynamic-system-prompt-sections",
                ],
            ),
            (
                ClaudeAmbientContext::Safe,
                vec!["--print", "--output-format", "json", "--safe-mode"],
            ),
            (
                ClaudeAmbientContext::Bare,
                vec!["--print", "--output-format", "json", "--bare"],
            ),
        ];

        for (ambient_context, expected) in cases {
            let options = ClaudeOptions {
                ambient_context,
                ..ClaudeOptions::default()
            };
            ClaudeService::new()
                .with_binary(&path)
                .oneshot(AgentRequest::new(Turn::new("hello").with_options(options)))
                .await
                .expect("ambient-context fake turn succeeds");
            let args = std::fs::read_to_string(&argv_path).expect("argv recorded");
            assert_eq!(args.lines().collect::<Vec<_>>(), expected);
        }

        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(argv_path);
    }

    #[test]
    fn host_ambient_context_cannot_be_weakened_or_replaced() {
        assert_eq!(
            effective_ambient_context(
                ClaudeAmbientContext::Hermetic(ClaudeHermetic::Full),
                ClaudeAmbientContext::Hermetic(ClaudeHermetic::Project),
            )
            .expect("full host seal wins"),
            ClaudeAmbientContext::Hermetic(ClaudeHermetic::Full)
        );
        assert_eq!(
            effective_ambient_context(ClaudeAmbientContext::Safe, ClaudeAmbientContext::Inherit,)
                .expect("inherit request keeps host safe mode"),
            ClaudeAmbientContext::Safe
        );

        let error =
            effective_ambient_context(ClaudeAmbientContext::Safe, ClaudeAmbientContext::Bare)
                .expect_err("safe and bare are non-overlapping postures");
        assert_eq!(error.kind, ErrorKind::InvalidRequest);
        assert_eq!(error.phase, FailurePhase::Validation);
        assert_eq!(error.effects, EffectState::None);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn strict_mcp_config_reaches_the_cli_argv() {
        use std::os::unix::fs::PermissionsExt;

        let path = std::env::temp_dir().join(format!(
            "tower-agent-claude-strict-{}.sh",
            std::process::id()
        ));
        std::fs::write(
            &path,
            concat!(
                "#!/bin/sh\n",
                "case \" $* \" in *\" --strict-mcp-config \"*) ;; *) exit 93;; esac\n",
                "cat >/dev/null\n",
                "printf '%s\\n' '{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"ok\",\"session_id\":\"s\",\"total_cost_usd\":0.01,\"duration_ms\":1,\"num_turns\":1,\"usage\":{\"input_tokens\":1,\"output_tokens\":1},\"is_error\":false}'\n",
            ),
        )
        .expect("write fake Claude CLI");
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&path, permissions).unwrap();

        let options = ClaudeOptions {
            strict_mcp_config: true,
            ..ClaudeOptions::default()
        };
        let request = AgentRequest::new(Turn::new("hello").with_options(options));
        let outcome = ClaudeService::new()
            .with_binary(&path)
            .oneshot(request)
            .await
            .expect("strict flag present, fake run succeeds");
        let _ = std::fs::remove_file(path);
        assert_eq!(outcome.output, "ok");
    }

    use tower::ServiceExt;
    use tower_agent::{CallContext, CancellationToken, EventObserver};

    use super::*;

    fn fake_claude() -> Claude {
        Claude::builder().binary("/usr/bin/true").build().unwrap()
    }

    #[test]
    fn options_map_without_putting_the_user_prompt_in_argv() {
        let options = ClaudeOptions {
            system_prompt: Some("You are the tester.".into()),
            append_system_prompt: Some("Be brief.".into()),
            model: Some("haiku".into()),
            fallback_model: Some("sonnet".into()),
            effort: Some(ClaudeEffort::High),
            allowed_tools: vec!["Bash(cargo test:*)".into()],
            disallowed_tools: vec!["Bash(rm:*)".into()],
            additional_directories: vec![PathBuf::from("/shared")],
            max_turns: Some(4),
            max_budget_usd: Some(2.5),
            permission_mode: Some(ClaudePermissionMode::Plan),
            json_schema: Some(r#"{"type":"object"}"#.into()),
            strict_mcp_config: true,
            ambient_context: ClaudeAmbientContext::Hermetic(ClaudeHermetic::Full),
        };
        let turn = Turn::new("run the tests")
            .resume(SessionHandle::new(PROVIDER, "sess-123"))
            .with_options(options);

        let rendered = build_query(&turn, ClaudeAmbientContext::Hermetic(ClaudeHermetic::Full))
            .expect("valid query")
            .to_command_string(&fake_claude());
        assert!(!rendered.contains("run the tests"));
        for expected in [
            "You are the tester.",
            "haiku",
            "sonnet",
            "/shared",
            "sess-123",
            "2.5",
            "plan",
            "type",
            "--strict-mcp-config",
            "--setting-sources",
        ] {
            assert!(
                rendered.contains(expected),
                "missing {expected:?}: {rendered}"
            );
        }
        assert!(!rendered.contains("bypass"));
    }

    #[test]
    fn budget_and_schema_are_validated_before_launch() {
        for options in [
            ClaudeOptions {
                max_budget_usd: Some(0.0),
                ..ClaudeOptions::default()
            },
            ClaudeOptions {
                max_budget_usd: Some(f64::NAN),
                ..ClaudeOptions::default()
            },
            ClaudeOptions {
                json_schema: Some("   ".into()),
                ..ClaudeOptions::default()
            },
            ClaudeOptions {
                fallback_model: Some("".into()),
                ..ClaudeOptions::default()
            },
        ] {
            let turn = Turn::new("hello").with_options(options);
            let error = build_query(&turn, ClaudeAmbientContext::Inherit)
                .expect_err("invalid options must be refused");
            assert_eq!(error.kind, ErrorKind::InvalidRequest);
        }
    }

    #[tokio::test]
    async fn rejects_foreign_sessions_before_launch() {
        let turn = Turn::new("hello")
            .resume(SessionHandle::new("codex", "session"))
            .with_options(ClaudeOptions::default());
        let error = ClaudeService::new()
            .with_binary("/definitely/not/a/claude/binary")
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
        let error = ClaudeService::new()
            .with_binary("/definitely/not/a/claude/binary")
            .oneshot(request)
            .await
            .expect_err("cancelled call must not launch");
        assert_eq!(error.kind, ErrorKind::Cancelled);
        assert_eq!(error.phase, FailurePhase::Admission);
        assert_eq!(error.effects, EffectState::None);
    }

    #[test]
    fn terminal_error_retains_session_and_accounting_evidence() {
        let mut extra = std::collections::HashMap::new();
        extra.insert("subtype".into(), serde_json::json!("error_max_budget_usd"));
        let result = QueryResult {
            result: "maximum budget reached".into(),
            session_id: "session-1".into(),
            cost_usd: Some(1.0),
            duration_ms: Some(12),
            num_turns: Some(2),
            is_error: true,
            usage: Some(claude_wrapper::TokenUsage {
                input_tokens: Some(10),
                output_tokens: Some(4),
                ..Default::default()
            }),
            extra,
        };

        let error = map_query_error(&result);
        let evidence = error.evidence.as_deref().expect("failure evidence");
        assert_eq!(error.kind, ErrorKind::Budget);
        assert_eq!(evidence.cost, Some(Cost::usd(1.0)));
        assert_eq!(evidence.provider_turns, Some(2));
        assert_eq!(evidence.duration, Some(Duration::from_millis(12)));
        assert_eq!(evidence.usage.and_then(TokenUsage::total), Some(14));
        assert_eq!(
            evidence.session.as_ref().map(SessionHandle::value),
            Some("session-1")
        );
    }

    #[test]
    fn wrapper_rail_stop_retains_resume_and_spend_evidence() {
        let wrapper_error = claude_wrapper::Error::from_command_failure(
            "claude --print --max-turns 4".into(),
            1,
            r#"{"type":"result","subtype":"error_max_turns","is_error":true,"num_turns":4,"session_id":"session-2","total_cost_usd":0.25,"errors":["Reached maximum number of turns (4)"]}"#.into(),
            String::new(),
            None,
        );
        let error = map_wrapper_error(wrapper_error);
        let evidence = error.evidence.as_deref().expect("failure evidence");

        assert_eq!(error.kind, ErrorKind::Limit);
        assert_eq!(evidence.cost, Some(Cost::usd(0.25)));
        assert_eq!(evidence.provider_turns, Some(4));
        assert_eq!(
            evidence.session.as_ref().map(SessionHandle::value),
            Some("session-2")
        );
    }

    #[test]
    fn authentication_subtypes_keep_conservative_effect_evidence() {
        use claude_wrapper::auth::AuthErrorKind;

        let cases = [
            (AuthErrorKind::NotAuthenticated, ErrorKind::Authentication),
            (AuthErrorKind::Expired, ErrorKind::Authentication),
            (AuthErrorKind::InvalidCredentials, ErrorKind::Authentication),
            (AuthErrorKind::RateLimit, ErrorKind::Limit),
            (AuthErrorKind::ProviderError, ErrorKind::Provider),
            (AuthErrorKind::Other, ErrorKind::Provider),
        ];
        for (kind, expected_kind) in cases {
            let error = map_wrapper_error(claude_wrapper::Error::Auth {
                kind,
                command: "claude --resume private-session".into(),
                exit_code: 1,
                message: "rejected private-session".into(),
            });
            assert_eq!(error.kind, expected_kind);
            assert_eq!(error.effects, EffectState::Possible);
            assert!(!error.message.contains("private-session"));
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn returns_terminal_evidence_without_putting_prompt_in_argv() {
        use std::os::unix::fs::PermissionsExt;

        let path = std::env::temp_dir().join(format!(
            "tower-agent-claude-success-{}.sh",
            std::process::id()
        ));
        std::fs::write(
            &path,
            concat!(
                "#!/bin/sh\n",
                "case \" $* \" in *\" hello \"*) exit 91;; esac\n",
                "prompt=$(cat)\n",
                "[ \"$prompt\" = \"hello\" ] || exit 92\n",
                "printf '%s\\n' '{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"hello\",\"session_id\":\"native-session\",\"total_cost_usd\":0.01,\"duration_ms\":12,\"num_turns\":1,\"usage\":{\"input_tokens\":3,\"output_tokens\":2},\"is_error\":false}'\n",
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
        assert_eq!(outcome.cost, Some(Cost::usd(0.01)));
        assert_eq!(outcome.usage.and_then(TokenUsage::total), Some(5));
        assert_eq!(outcome.duration, Some(Duration::from_millis(12)));
        assert_eq!(outcome.provider_turns, Some(1));
        assert_eq!(receiver.recv().await, Some(AgentEvent::Started));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn in_flight_cancellation_settles_the_service_call() {
        use std::os::unix::fs::PermissionsExt;

        let path = std::env::temp_dir().join(format!(
            "tower-agent-claude-cancel-{}.sh",
            std::process::id()
        ));
        let pid_path = std::env::temp_dir().join(format!(
            "tower-agent-claude-cancel-{}.pid",
            std::process::id()
        ));
        let script = format!(
            "#!/bin/sh\ncat >/dev/null\nsleep 30 </dev/null &\nchild=$!\nprintf 'parent=%s\\nchild=%s\\n' \"$$\" \"$child\" > '{}'\nwait \"$child\"\n",
            pid_path.display()
        );
        std::fs::write(&path, script).expect("write blocking fake Claude CLI");
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&path, permissions).unwrap();

        let cancellation = CancellationToken::new();
        let request = AgentRequest::with_context(
            Turn::new("hello").with_options(ClaudeOptions::default()),
            CallContext::new().with_cancellation(cancellation.clone()),
        );
        let call = tokio::spawn(
            ClaudeService::new()
                .with_binary(&path)
                .with_kill_grace(Duration::from_millis(10))
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
            "fake Claude did not record its process tree"
        );
        let pids =
            std::fs::read_to_string(&pid_path).expect("fake Claude recorded its process tree");
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
}
