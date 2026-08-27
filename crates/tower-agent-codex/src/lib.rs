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
//! Abrupt worker death is separate: hosts can enable Linux parent-death
//! signaling and register [`SpawnReceipt`] values with an external watchdog.
//!
//! Filesystem authority is portable rather than a wrapper flag. Each turn asks
//! for a [`FilesystemAuthority`], while [`CodexService`] enforces a host-owned
//! [`AuthorityPolicy`] immediately before launch. The default is read-only.
//!
//! [`CodexAmbientContextPolicy::Automation`] applies the host-owned config and
//! project-instruction controls intended for queued execution. It reduces
//! ambient context but does not remove provider built-ins, managed host
//! instructions, workspace contents, or the child environment.
//! [`CodexSkillPolicy::DisableExact`] can additionally disable exact,
//! host-selected skill folders, but Codex currently has no documented global
//! skill-disable setting. Unlisted discovered skills can therefore remain.
//! Ephemeral turns are a separate per-turn persistence choice.

//! # Example
//!
//! ```
//! use tower_agent::{AuthorityPolicy, FilesystemAuthority, Turn};
//! use tower_agent_codex::{CodexAmbientContextPolicy, CodexOptions, CodexService};
//!
//! // Read-only is the default ceiling; workspace write is an explicit host choice.
//! let service = CodexService::new()
//!     .with_authority_policy(AuthorityPolicy::read_only())
//!     .with_ambient_context_policy(CodexAmbientContextPolicy::Automation);
//!
//! let allowed = Turn::new("inspect this repository").with_options(CodexOptions::default());
//! service.preflight(&allowed).expect("read-only is within the ceiling");
//!
//! let excessive = Turn::new("rewrite the repository").with_options(CodexOptions {
//!     filesystem_authority: FilesystemAuthority::WorkspaceWrite,
//!     ..CodexOptions::default()
//! });
//! // Refused before any process exists, and refused again at launch.
//! assert!(service.preflight(&excessive).is_err());
//! ```

use std::fmt;
use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use codex_wrapper::command::exec::ExecResumeCommand;
use codex_wrapper::{Codex, ExecCommand, QueryResult, SandboxMode, TurnFailureKind};
use tower::Service;
use tower_agent::{
    AgentError, AgentEvent, AgentRequest, AuthorityPolicy, CancellationToken,
    ChildEnvironmentPolicy, EffectState, ErrorKind, FailureEvidence, FailurePhase,
    FilesystemAuthority, OperationId, RequestsFilesystemAuthority, SessionHandle, SpawnObserver,
    SpawnReceipt, TokenUsage, Turn, TurnOutcome,
};

const PROVIDER: &str = "codex";

/// Maximum serialized size accepted for a Codex output schema.
pub const MAX_OUTPUT_SCHEMA_BYTES: usize = 1024 * 1024;

/// Maximum number of exact skill folders accepted by one host policy.
pub const MAX_DISABLED_SKILLS: usize = 256;

/// Maximum encoded Codex config override produced by one host skill policy.
pub const MAX_SKILL_CONFIG_BYTES: usize = 64 * 1024;

/// Per-turn controls supported by the Codex provider service.
///
/// Host-local launch configuration such as `CODEX_HOME` belongs on
/// [`CodexService`], not in the portable turn body.
#[derive(Clone, Default, PartialEq, Eq)]
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
    /// JSON Schema for the provider's structured final response.
    ///
    /// The adapter validates this as Draft 2020-12, bounds its serialized
    /// size, and materializes it in an owner-only temporary file. Callers do
    /// not supply a local filesystem path.
    pub output_schema: Option<serde_json::Value>,
    /// Portable filesystem authority requested for this turn.
    pub filesystem_authority: FilesystemAuthority,
    /// Do not persist this turn into resumable rollout history. Ephemeral
    /// outcomes deliberately omit a session handle even if the CLI reports a
    /// transient thread id.
    pub ephemeral: bool,
}

impl fmt::Debug for CodexOptions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CodexOptions")
            .field("system_prompt", &self.system_prompt)
            .field("model", &self.model)
            .field("additional_directories", &self.additional_directories)
            .field(
                "output_schema",
                &self.output_schema.as_ref().map(|_| "<redacted>"),
            )
            .field("filesystem_authority", &self.filesystem_authority)
            .field("ephemeral", &self.ephemeral)
            .finish()
    }
}

impl RequestsFilesystemAuthority for CodexOptions {
    fn filesystem_authority(&self) -> FilesystemAuthority {
        self.filesystem_authority
    }

    fn additional_filesystem_roots(&self) -> &[PathBuf] {
        &self.additional_directories
    }
}

/// Host-owned control over ambient Codex configuration and project context.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CodexAmbientContextPolicy {
    /// Preserve the Codex CLI's normal ambient configuration behavior.
    #[default]
    Inherit,
    /// Ignore user config and execpolicy rules, reject unknown config keys,
    /// and suppress project instruction documents.
    Automation,
}

/// Host-owned policy for discovered Codex skills.
///
/// Codex currently documents only exact-path skill enablement overrides. This
/// policy therefore does not claim to suppress every discovered skill or any
/// provider built-in or managed instruction.
#[derive(Clone, Default, PartialEq, Eq)]
pub enum CodexSkillPolicy {
    /// Preserve Codex's normal discovered-skill behavior.
    #[default]
    Inherit,
    /// Disable these exact skill folders for fresh and resumed turns.
    ///
    /// Each path is canonicalized before launch and must identify a directory
    /// containing `SKILL.md`.
    DisableExact(Vec<PathBuf>),
}

impl fmt::Debug for CodexSkillPolicy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Inherit => formatter.write_str("Inherit"),
            Self::DisableExact(paths) => formatter
                .debug_struct("DisableExact")
                .field("paths", &"<redacted>")
                .field("count", &paths.len())
                .finish(),
        }
    }
}

/// A cloneable Tower service that runs one finite Codex turn per call.
#[derive(Clone, Debug)]
pub struct CodexService {
    binary: Option<PathBuf>,
    codex_home: Option<PathBuf>,
    termination_grace: Option<Duration>,
    die_with_parent: bool,
    spawn_observer: Option<SpawnObserver>,
    authority_policy: AuthorityPolicy,
    child_environment: ChildEnvironmentPolicy,
    ambient_context: CodexAmbientContextPolicy,
    skill_policy: CodexSkillPolicy,
}

impl CodexService {
    /// Create a service using the wrapper's default process-group ownership.
    pub fn new() -> Self {
        Self {
            binary: None,
            codex_home: None,
            termination_grace: None,
            die_with_parent: false,
            spawn_observer: None,
            authority_policy: AuthorityPolicy::read_only(),
            child_environment: ChildEnvironmentPolicy::default(),
            ambient_context: CodexAmbientContextPolicy::default(),
            skill_policy: CodexSkillPolicy::default(),
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

    /// Ask Linux to kill each Codex child when this worker process dies.
    ///
    /// This is host-owned and off by default. Other platforms accept the
    /// setting but need an external watchdog driven by
    /// [`with_spawn_observer`](Self::with_spawn_observer).
    pub fn with_die_with_parent(mut self, enabled: bool) -> Self {
        self.die_with_parent = enabled;
        self
    }

    pub const fn die_with_parent(&self) -> bool {
        self.die_with_parent
    }

    /// Whether the configured parent-death policy has kernel support here.
    pub const fn die_with_parent_supported() -> bool {
        codex_wrapper::exec::die_with_parent_supported()
    }

    /// Register each spawned Codex child with a host-local watchdog.
    ///
    /// The observer runs inline at spawn time and must not block.
    pub fn with_spawn_observer(mut self, observer: SpawnObserver) -> Self {
        self.spawn_observer = Some(observer);
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

    /// Set the host-owned child environment policy. The compatibility default
    /// inherits the complete host environment.
    pub fn with_child_environment_policy(mut self, policy: ChildEnvironmentPolicy) -> Self {
        self.child_environment = policy;
        self
    }

    pub const fn child_environment_policy(&self) -> &ChildEnvironmentPolicy {
        &self.child_environment
    }

    /// Set the host-owned ambient-context policy. Remote turn options cannot
    /// weaken this baseline.
    pub fn with_ambient_context_policy(mut self, policy: CodexAmbientContextPolicy) -> Self {
        self.ambient_context = policy;
        self
    }

    pub const fn ambient_context_policy(&self) -> CodexAmbientContextPolicy {
        self.ambient_context
    }

    /// Set the host-owned discovered-skill policy. Remote turn options cannot
    /// weaken or replace this policy.
    pub fn with_skill_policy(mut self, policy: CodexSkillPolicy) -> Self {
        self.skill_policy = policy;
        self
    }

    pub const fn skill_policy(&self) -> &CodexSkillPolicy {
        &self.skill_policy
    }

    pub fn codex_home(&self) -> Option<&Path> {
        self.codex_home.as_deref()
    }

    /// Check every validation-phase refusal for one turn without launching.
    ///
    /// This is the same code path `call` takes before any launch work: the
    /// host authority policy, prompt and model checks, session handle
    /// checks, resumed-directory support, directory encoding, output-schema
    /// validation, and the service's skill-policy encoding. A turn that
    /// passes preflight cannot be refused by `call` during the validation
    /// phase.
    ///
    /// Preflight performs no I/O and spawns nothing. Two classes of refusal
    /// deliberately remain call-time: checks that need call-local context
    /// (Codex rejects a host-preassigned session from `CallContext`), and
    /// launch-phase construction (child-environment resolution and binary
    /// resolution).
    ///
    /// # Example
    ///
    /// ```
    /// use tower_agent::Turn;
    /// use tower_agent_codex::{CodexOptions, CodexService};
    ///
    /// let service = CodexService::new();
    /// let turn = Turn::new("inspect this repository").with_options(CodexOptions::default());
    /// assert!(service.preflight(&turn).is_ok());
    ///
    /// let blank = Turn::new("   ").with_options(CodexOptions::default());
    /// assert!(service.preflight(&blank).is_err());
    /// ```
    pub fn preflight(&self, turn: &Turn<CodexOptions>) -> Result<(), AgentError> {
        preflight_turn(&self.authority_policy, turn)?;
        render_skill_config(&self.skill_policy).map(|_| ())
    }

    fn build_codex(
        &self,
        working_directory: Option<&Path>,
        operation_id: OperationId,
    ) -> Result<Codex, AgentError> {
        let mut builder = Codex::builder();
        let environment = self.child_environment.resolve().map_err(|error| {
            AgentError::new(
                ErrorKind::Internal,
                format!("invalid Codex child environment policy: {error}"),
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
        builder = builder.die_with_parent(self.die_with_parent);
        if let Some(observer) = &self.spawn_observer {
            let observer = observer.clone();
            builder = builder.on_spawn(Arc::new(move |info| {
                observer.observe(SpawnReceipt::new(
                    PROVIDER,
                    operation_id,
                    info.pid,
                    info.pgid,
                ));
            }));
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
        if request.context.preassigned_session().is_some() {
            return Box::pin(async {
                Err(AgentError::unsupported(
                    "Codex does not support host-preassigned fresh session IDs",
                ))
            });
        }
        let service = self.clone();
        let observer = request.context.events().clone();
        let operation_id = request.context.operation_id();
        let prepared = prepare(request, &self.authority_policy);

        Box::pin(async move {
            let (turn, cancellation) = prepared?;
            if cancellation.is_cancelled() {
                return Err(cancelled_before_launch());
            }

            let skill_config = render_skill_config(&service.skill_policy)?;
            let started = Instant::now();
            let codex = service.build_codex(turn.working_directory.as_deref(), operation_id)?;

            if cancellation.is_cancelled() {
                return Err(cancelled_before_launch());
            }

            let output_schema_file = turn
                .output_schema
                .as_deref()
                .map(|schema| materialize_output_schema(schema, operation_id))
                .transpose()?;
            let output_schema_path = output_schema_file
                .as_ref()
                .map(|file| {
                    file.path().to_str().ok_or_else(|| {
                        AgentError::new(
                            ErrorKind::Internal,
                            "Codex output schema temporary path is not valid UTF-8",
                            FailurePhase::Launch,
                            EffectState::None,
                        )
                    })
                })
                .transpose()?;

            let _ = observer.try_emit(AgentEvent::Started);
            let result = match &turn.session {
                Some(session) => {
                    resume_command(
                        &turn,
                        session,
                        service.ambient_context,
                        output_schema_path,
                        skill_config.as_deref(),
                    )
                    .execute_json_cancellable(&codex, cancellation.cancelled())
                    .await
                }
                None => {
                    fresh_command(
                        &turn,
                        service.ambient_context,
                        output_schema_path,
                        skill_config.as_deref(),
                    )
                    .execute_json_cancellable(&codex, cancellation.cancelled())
                    .await
                }
            };
            drop(output_schema_file);
            let result = result.map_err(map_run_error)?;

            let outcome = settle_outcome(
                result,
                turn.session,
                started.elapsed(),
                turn.ephemeral,
                turn.output_schema.is_some(),
            )?;
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
    output_schema: Option<Vec<u8>>,
    filesystem_authority: FilesystemAuthority,
    ephemeral: bool,
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
    let validated = preflight_turn(authority_policy, &turn)?;

    Ok((
        PreparedTurn {
            prompt: compose_prompt(&turn.prompt, turn.options.system_prompt.as_deref()),
            working_directory: turn.working_directory,
            session: validated.session,
            model: turn.options.model,
            additional_directories: validated.additional_directories,
            output_schema: validated.output_schema,
            filesystem_authority: turn.options.filesystem_authority,
            ephemeral: turn.options.ephemeral,
        },
        cancellation,
    ))
}

/// The owned artifacts produced by the validation-phase checks.
struct ValidatedCodexTurn {
    session: Option<String>,
    additional_directories: Vec<String>,
    output_schema: Option<Vec<u8>>,
}

/// Every validation-phase refusal decision for one turn, in one place.
///
/// `prepare` and [`CodexService::preflight`] both route through this
/// function, so a turn that passes preflight cannot be refused by `call`
/// during the validation phase.
fn preflight_turn(
    authority_policy: &AuthorityPolicy,
    turn: &Turn<CodexOptions>,
) -> Result<ValidatedCodexTurn, AgentError> {
    authority_policy.authorize(turn)?;
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

    let session = match &turn.session {
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
        Some(session) if session.value().starts_with('-') => {
            return Err(AgentError::invalid_request(
                "Codex session handle must not begin with a hyphen",
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
    let output_schema = turn
        .options
        .output_schema
        .clone()
        .map(validate_output_schema)
        .transpose()?;

    Ok(ValidatedCodexTurn {
        session,
        additional_directories,
        output_schema,
    })
}

fn validate_output_schema(schema: serde_json::Value) -> Result<Vec<u8>, AgentError> {
    let encoded = serde_json::to_vec(&schema).map_err(|_| {
        AgentError::new(
            ErrorKind::Internal,
            "Codex output schema could not be serialized",
            FailurePhase::Validation,
            EffectState::None,
        )
    })?;
    if encoded.len() > MAX_OUTPUT_SCHEMA_BYTES {
        return Err(AgentError::invalid_request(format!(
            "Codex output schema exceeds the {MAX_OUTPUT_SCHEMA_BYTES}-byte limit"
        )));
    }
    if !jsonschema::draft202012::meta::is_valid(&schema) {
        return Err(AgentError::invalid_request(
            "Codex output schema is not valid JSON Schema Draft 2020-12",
        ));
    }
    Ok(encoded)
}

fn materialize_output_schema(
    schema: &[u8],
    operation_id: OperationId,
) -> Result<tempfile::NamedTempFile, AgentError> {
    let prefix = format!("tower-agent-codex-schema-{operation_id}-");
    let mut file = tempfile::Builder::new()
        .prefix(&prefix)
        .suffix(".json")
        .tempfile()
        .map_err(|_| output_schema_file_error())?;
    file.write_all(schema)
        .and_then(|()| file.as_file_mut().sync_all())
        .map_err(|_| output_schema_file_error())?;
    Ok(file)
}

fn output_schema_file_error() -> AgentError {
    AgentError::new(
        ErrorKind::Internal,
        "Codex output schema temporary file could not be prepared",
        FailurePhase::Launch,
        EffectState::None,
    )
}

fn compose_prompt(prompt: &str, system_prompt: Option<&str>) -> String {
    match system_prompt {
        Some(system_prompt) if !system_prompt.is_empty() => {
            format!("{system_prompt}\n\n{prompt}")
        }
        _ => prompt.to_string(),
    }
}

fn render_skill_config(policy: &CodexSkillPolicy) -> Result<Option<String>, AgentError> {
    let CodexSkillPolicy::DisableExact(paths) = policy else {
        return Ok(None);
    };
    if paths.is_empty() {
        return Ok(None);
    }
    if paths.len() > MAX_DISABLED_SKILLS {
        return Err(skill_policy_error(
            "Codex skill policy exceeds the configured path limit",
        ));
    }

    let mut canonical_paths = Vec::with_capacity(paths.len());
    for path in paths {
        let canonical = std::fs::canonicalize(path)
            .map_err(|_| skill_policy_error("configured Codex skill path could not be resolved"))?;
        if !canonical.is_dir() || !canonical.join("SKILL.md").is_file() {
            return Err(skill_policy_error(
                "configured Codex skill path does not identify a skill folder",
            ));
        }
        canonical_paths.push(canonical);
    }
    canonical_paths.sort();
    canonical_paths.dedup();

    let entries = canonical_paths
        .iter()
        .map(|path| {
            let path = path.to_str().ok_or_else(|| {
                skill_policy_error("configured Codex skill path is not valid UTF-8")
            })?;
            let encoded = serde_json::to_string(path).map_err(|_| {
                skill_policy_error("configured Codex skill path could not be encoded")
            })?;
            Ok(format!("{{path={encoded},enabled=false}}"))
        })
        .collect::<Result<Vec<_>, AgentError>>()?;
    let config = format!("skills.config=[{}]", entries.join(","));
    if config.len() > MAX_SKILL_CONFIG_BYTES {
        return Err(skill_policy_error(
            "Codex skill policy exceeds the encoded size limit",
        ));
    }
    Ok(Some(config))
}

fn skill_policy_error(message: &'static str) -> AgentError {
    AgentError::new(
        ErrorKind::Internal,
        message,
        FailurePhase::Launch,
        EffectState::None,
    )
}

fn fresh_command(
    turn: &PreparedTurn,
    ambient: CodexAmbientContextPolicy,
    output_schema_path: Option<&str>,
    skill_config: Option<&str>,
) -> ExecCommand {
    let mut command = ExecCommand::from_stdin(turn.prompt.clone())
        .sandbox(sandbox_mode(turn.filesystem_authority))
        .skip_git_repo_check();
    if ambient == CodexAmbientContextPolicy::Automation {
        command = command
            .strict_config()
            .ignore_user_config()
            .ignore_rules()
            .config("project_doc_max_bytes=0");
    }
    if let Some(config) = skill_config {
        command = command.strict_config().config(config);
    }
    if turn.ephemeral {
        command = command.ephemeral();
    }
    if let Some(model) = &turn.model {
        command = command.model(model);
    }
    if let Some(path) = output_schema_path {
        command = command.output_schema(path);
    }
    for directory in &turn.additional_directories {
        command = command.add_dir(directory);
    }
    command
}

fn resume_command(
    turn: &PreparedTurn,
    session: &str,
    ambient: CodexAmbientContextPolicy,
    output_schema_path: Option<&str>,
    skill_config: Option<&str>,
) -> ExecResumeCommand {
    let mut command = ExecResumeCommand::from_stdin(turn.prompt.clone())
        .session_id(session)
        .config(sandbox_config(turn.filesystem_authority))
        .skip_git_repo_check();
    if ambient == CodexAmbientContextPolicy::Automation {
        command = command
            .strict_config()
            .ignore_user_config()
            .ignore_rules()
            .config("project_doc_max_bytes=0");
    }
    if let Some(config) = skill_config {
        command = command.strict_config().config(config);
    }
    if turn.ephemeral {
        command = command.ephemeral();
    }
    if let Some(model) = &turn.model {
        command = command.model(model);
    }
    if let Some(path) = output_schema_path {
        command = command.output_schema(path);
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

fn settle_outcome(
    mut result: QueryResult,
    prior_session: Option<String>,
    duration: Duration,
    ephemeral: bool,
    structured_output: bool,
) -> Result<TurnOutcome, AgentError> {
    let terminal = validate_terminal_events(&result.events)?;
    let session = validated_result_session(&result, prior_session, ephemeral)?;
    let usage = result.usage.map(map_usage);
    let output = if structured_output {
        result
            .events
            .iter()
            .rev()
            .find_map(codex_wrapper::JsonLineEvent::agent_message_text)
            .unwrap_or_default()
    } else {
        std::mem::take(&mut result.result)
    };

    match terminal {
        TerminalState::Completed => {
            let mut outcome = TurnOutcome::new(output);
            outcome.session = session;
            outcome.usage = usage;
            outcome.duration = Some(duration);
            Ok(outcome)
        }
        // A request the API refused never began generating, so it is not an
        // uncertain turn: it carries no effects and no continuation. Saying
        // otherwise would make a durable host record a resumable turn for a
        // call that did nothing, and forbid the retry that fixing the request
        // makes safe.
        TerminalState::Failed(TurnFailureKind::ApiRequestRejected) => Err(AgentError::new(
            ErrorKind::InvalidRequest,
            "Codex rejected the request before generating output",
            FailurePhase::Validation,
            EffectState::None,
        )
        .with_evidence(FailureEvidence {
            usage,
            duration: Some(duration),
            ..FailureEvidence::default()
        })),
        TerminalState::Failed(failure) => {
            let (kind, message) = match failure {
                TurnFailureKind::RolloutBudgetExhausted => (
                    ErrorKind::Budget,
                    "Codex turn exhausted its rollout token budget",
                ),
                _ => (ErrorKind::Provider, "Codex reported a failed turn"),
            };
            Err(
                AgentError::new(kind, message, FailurePhase::Running, EffectState::Possible)
                    .with_evidence(FailureEvidence {
                        session,
                        usage,
                        duration: Some(duration),
                        ..FailureEvidence::default()
                    }),
            )
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TerminalState {
    Completed,
    Failed(TurnFailureKind),
}

fn validate_terminal_events(
    events: &[codex_wrapper::JsonLineEvent],
) -> Result<TerminalState, AgentError> {
    let mut terminals = events
        .iter()
        .enumerate()
        .filter(|(_, event)| event.is_turn_completed() || event.is_turn_failed());
    let Some((index, terminal)) = terminals.next() else {
        return Err(codex_settlement_error(
            "Codex event stream did not contain a terminal turn event",
        ));
    };
    if terminals.next().is_some() || index + 1 != events.len() {
        return Err(codex_settlement_error(
            "Codex event stream contained conflicting terminal state",
        ));
    }
    if terminal.is_turn_completed() {
        Ok(TerminalState::Completed)
    } else {
        Ok(TerminalState::Failed(
            terminal
                .turn_failure_kind()
                .unwrap_or(TurnFailureKind::Other),
        ))
    }
}

fn validated_result_session(
    result: &QueryResult,
    prior_session: Option<String>,
    ephemeral: bool,
) -> Result<Option<SessionHandle>, AgentError> {
    let thread = consistent_provider_handle(
        result
            .events
            .iter()
            .filter_map(codex_wrapper::JsonLineEvent::thread_id)
            .chain(result.thread_id.as_deref()),
    )?;
    let session = consistent_provider_handle(
        result
            .events
            .iter()
            .filter_map(codex_wrapper::JsonLineEvent::session_id)
            .chain(result.session_id.as_deref()),
    )?;
    let value = thread.or(session).or(prior_session);
    Ok((!ephemeral)
        .then_some(value)
        .flatten()
        .map(|value| SessionHandle::new(PROVIDER, value)))
}

fn consistent_provider_handle<'a>(
    mut values: impl Iterator<Item = &'a str>,
) -> Result<Option<String>, AgentError> {
    let Some(first) = values.next() else {
        return Ok(None);
    };
    if first.trim().is_empty() || first.starts_with('-') || values.any(|value| value != first) {
        return Err(codex_settlement_error(
            "Codex event stream contained invalid session evidence",
        ));
    }
    Ok(Some(first.to_string()))
}

fn codex_settlement_error(message: &'static str) -> AgentError {
    AgentError::new(
        ErrorKind::Provider,
        message,
        FailurePhase::Settlement,
        EffectState::Possible,
    )
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

/// Keep unclassified failures useful without forwarding provider output,
/// command arguments, paths, prompts, or credentials across the adapter.
fn command_failed_message(provider: &str, exit_code: i32) -> String {
    format!("{provider} command failed with exit code {exit_code}")
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

/// A request the upstream API refused before generating anything.
///
/// Generation never began, so the turn had no effects and left nothing to
/// continue. Reporting it as a running, possibly-effectful failure would make
/// a durable host record a resumable turn for a call that did nothing, and
/// would forbid the retry that correcting the request makes safe.
fn rejected_request_error() -> AgentError {
    AgentError::new(
        ErrorKind::InvalidRequest,
        "Codex rejected the request before generating output",
        FailurePhase::Validation,
        EffectState::None,
    )
}

/// Whether a captured event stream ends in an API request rejection.
///
/// Used only for classification. No diagnostic text from the stream reaches
/// the returned error.
fn stream_reports_a_rejected_request(stdout: &str) -> bool {
    stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<codex_wrapper::JsonLineEvent>(line).ok())
        .any(|event| event.turn_failure_kind() == Some(TurnFailureKind::ApiRequestRejected))
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
        codex_wrapper::Error::Io { .. } => AgentError::new(
            ErrorKind::Provider,
            "Codex process I/O failed",
            FailurePhase::Running,
            EffectState::Possible,
        ),
        codex_wrapper::Error::Timeout { .. } => AgentError::new(
            ErrorKind::DeadlineExceeded,
            "Codex command exceeded its configured timeout",
            FailurePhase::Running,
            EffectState::Possible,
        ),
        codex_wrapper::Error::Cancelled { .. } => cancelled_in_flight(),
        codex_wrapper::Error::Auth { .. } => AgentError::new(
            ErrorKind::Authentication,
            "Codex credentials were rejected",
            FailurePhase::Running,
            // The CLI may re-authenticate after tools have already run. Its
            // deterministic classification is retry evidence, not proof that
            // the turn had no effects.
            EffectState::Possible,
        ),
        codex_wrapper::Error::Config { .. } => AgentError::new(
            ErrorKind::Provider,
            "Codex rejected its launch configuration",
            FailurePhase::Validation,
            EffectState::None,
        ),
        codex_wrapper::Error::NotTrustedDirectory { .. } => AgentError::new(
            ErrorKind::Unauthorized,
            "Codex refused the working directory",
            FailurePhase::Validation,
            EffectState::None,
        ),
        codex_wrapper::Error::SessionNotFound { .. } => AgentError::new(
            ErrorKind::InvalidRequest,
            "Codex session was not found",
            FailurePhase::Validation,
            EffectState::None,
        ),
        codex_wrapper::Error::CommandFailed {
            exit_code, stdout, ..
        } => {
            // A rejected request is reported on the event stream and then the
            // CLI exits nonzero, so the terminal event arrives here rather
            // than through a parsed result. Recover the classification before
            // falling back to the conservative reading.
            if stream_reports_a_rejected_request(&stdout) {
                rejected_request_error()
            } else {
                AgentError::new(
                    ErrorKind::Provider,
                    command_failed_message("Codex", exit_code),
                    FailurePhase::Running,
                    EffectState::Possible,
                )
            }
        }
        codex_wrapper::Error::Json { .. } => AgentError::new(
            ErrorKind::Provider,
            "Codex returned an invalid event stream",
            FailurePhase::Settlement,
            EffectState::Possible,
        ),
        codex_wrapper::Error::TokenBudgetExceeded {
            total_tokens,
            max_tokens,
        } => AgentError::new(
            ErrorKind::Budget,
            format!("Codex token budget exceeded: {total_tokens} of {max_tokens} tokens"),
            FailurePhase::Running,
            EffectState::Possible,
        ),
        codex_wrapper::Error::InvalidRolloutBudget { .. } => AgentError::new(
            ErrorKind::InvalidRequest,
            "Codex rollout budget configuration was invalid",
            FailurePhase::Validation,
            EffectState::None,
        ),
        codex_wrapper::Error::DangerousNotAllowed { .. } => AgentError::new(
            ErrorKind::Unauthorized,
            "Codex safety controls cannot be bypassed by this host",
            FailurePhase::Validation,
            EffectState::None,
        ),
        codex_wrapper::Error::VersionMismatch { found, minimum } => AgentError::new(
            ErrorKind::Unsupported,
            format!("Codex CLI {found} is older than required version {minimum}"),
            FailurePhase::Launch,
            EffectState::None,
        ),
        codex_wrapper::Error::UntestedCliVersion {
            found,
            tested_min,
            tested_max,
        } => AgentError::new(
            ErrorKind::Unsupported,
            format!("Codex CLI {found} is outside the tested range {tested_min}..={tested_max}"),
            FailurePhase::Launch,
            EffectState::None,
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

    fn query_result(lines: &[&str]) -> QueryResult {
        QueryResult::from_events(
            lines
                .iter()
                .map(|line| serde_json::from_str(line).expect("valid Codex JSONL event"))
                .collect(),
        )
    }

    fn test_output_schema() -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "description": "schema-secret-sentinel",
            "properties": {"answer": {"type": "string"}},
            "required": ["answer"],
            "additionalProperties": false
        })
    }

    fn output_schema_options() -> CodexOptions {
        CodexOptions {
            output_schema: Some(test_output_schema()),
            ..CodexOptions::default()
        }
    }

    fn operation_schema_files(operation_id: OperationId) -> Vec<PathBuf> {
        let prefix = format!("tower-agent-codex-schema-{operation_id}-");
        let mut paths = std::fs::read_dir(std::env::temp_dir())
            .expect("read system temporary directory")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(&prefix) && name.ends_with(".json"))
            })
            .collect::<Vec<_>>();
        paths.sort();
        paths
    }

    fn create_test_skill(root: &Path, name: &str) -> PathBuf {
        let path = root.join(name);
        std::fs::create_dir_all(&path).expect("create test skill folder");
        std::fs::write(
            path.join("SKILL.md"),
            "---\nname: sentinel\ndescription: test only\n---\n",
        )
        .expect("write sentinel skill");
        path
    }

    #[test]
    fn exact_skill_policy_canonicalizes_deduplicates_and_encodes_paths() {
        let root = tempfile::tempdir().expect("create skill root");
        let skill = create_test_skill(root.path(), "quoted \"skill\"");
        let canonical = std::fs::canonicalize(&skill).expect("canonical skill path");
        let policy = CodexSkillPolicy::DisableExact(vec![skill.clone(), skill]);

        let config = render_skill_config(&policy)
            .expect("valid skill policy")
            .expect("disable policy emits config");
        let encoded =
            serde_json::to_string(canonical.to_str().expect("test path is UTF-8")).unwrap();
        assert_eq!(
            config,
            format!("skills.config=[{{path={encoded},enabled=false}}]")
        );

        let service = CodexService::new().with_skill_policy(policy.clone());
        assert_eq!(service.skill_policy(), &policy);
        assert!(!format!("{service:?}").contains(&canonical.display().to_string()));
        assert_eq!(
            render_skill_config(&CodexSkillPolicy::Inherit).unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn invalid_skill_policy_fails_before_launch_without_disclosing_paths() {
        let root = tempfile::tempdir().expect("create skill root");
        let not_a_skill = root.path().join("not-a-skill");
        std::fs::create_dir(&not_a_skill).expect("create non-skill folder");
        let invalid_folder =
            render_skill_config(&CodexSkillPolicy::DisableExact(vec![not_a_skill.clone()]))
                .expect_err("folder without SKILL.md is rejected");
        assert_eq!(invalid_folder.kind, ErrorKind::Internal);
        assert!(!format!("{invalid_folder:?}").contains(&not_a_skill.display().to_string()));

        let secret = root.path().join("secret-skill-path");
        let error = CodexService::new()
            .with_binary("/definitely/not/a/codex/binary")
            .with_skill_policy(CodexSkillPolicy::DisableExact(vec![secret.clone()]))
            .oneshot(request("hello", CodexOptions::default()))
            .await
            .expect_err("missing skill path must fail before provider launch");

        assert_eq!(error.kind, ErrorKind::Internal);
        assert_eq!(error.phase, FailurePhase::Launch);
        assert_eq!(error.effects, EffectState::None);
        assert!(!format!("{error:?}").contains(&secret.display().to_string()));
    }

    #[test]
    fn exact_skill_policy_bounds_the_number_of_paths_before_resolution() {
        let paths = vec![PathBuf::from("/not-resolved"); MAX_DISABLED_SKILLS + 1];
        let error = render_skill_config(&CodexSkillPolicy::DisableExact(paths))
            .expect_err("oversized policy is rejected");

        assert_eq!(error.kind, ErrorKind::Internal);
        assert_eq!(error.phase, FailurePhase::Launch);
        assert_eq!(error.effects, EffectState::None);
    }

    #[tokio::test]
    async fn invalid_and_oversized_output_schemas_fail_before_launch_without_disclosure() {
        let invalid_sentinel = "invalid-schema-secret";
        let invalid = CodexOptions {
            output_schema: Some(serde_json::json!({
                "type": invalid_sentinel,
                "description": invalid_sentinel
            })),
            ..CodexOptions::default()
        };
        let rendered = format!("{invalid:?}");
        assert!(rendered.contains("<redacted>"));
        assert!(!rendered.contains(invalid_sentinel));

        let error = CodexService::new()
            .with_binary("/definitely/not/a/codex/binary")
            .oneshot(request("hello", invalid))
            .await
            .expect_err("invalid schema must fail before launch");
        assert_eq!(error.kind, ErrorKind::InvalidRequest);
        assert_eq!(error.phase, FailurePhase::Validation);
        assert_eq!(error.effects, EffectState::None);
        assert!(!format!("{error:?}").contains(invalid_sentinel));

        let oversized_sentinel = "oversized-schema-secret";
        let oversized = CodexOptions {
            output_schema: Some(serde_json::json!({
                "type": "object",
                "title": oversized_sentinel,
                "description": "x".repeat(MAX_OUTPUT_SCHEMA_BYTES)
            })),
            ..CodexOptions::default()
        };
        let error = CodexService::new()
            .with_binary("/definitely/not/a/codex/binary")
            .oneshot(request("hello", oversized))
            .await
            .expect_err("oversized schema must fail before launch");
        assert_eq!(error.kind, ErrorKind::InvalidRequest);
        assert_eq!(error.phase, FailurePhase::Validation);
        assert_eq!(error.effects, EffectState::None);
        assert!(!format!("{error:?}").contains(oversized_sentinel));
    }

    #[cfg(unix)]
    #[test]
    fn output_schema_temporary_files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let encoded = validate_output_schema(test_output_schema()).expect("valid schema");
        let file = materialize_output_schema(&encoded, OperationId::from_u64(75000))
            .expect("materialize schema");
        let mode = file
            .as_file()
            .metadata()
            .expect("schema metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        let path = file.path().to_path_buf();
        drop(file);
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fresh_and_resumed_turns_receive_the_schema_and_clean_up_on_success() {
        use std::os::unix::fs::PermissionsExt;

        let binary = std::env::temp_dir().join(format!(
            "tower-agent-codex-output-schema-success-{}.sh",
            std::process::id()
        ));
        let recorded_path = std::env::temp_dir().join(format!(
            "tower-agent-codex-output-schema-success-{}.path",
            std::process::id()
        ));
        let recorded_schema = std::env::temp_dir().join(format!(
            "tower-agent-codex-output-schema-success-{}.json",
            std::process::id()
        ));
        let script = format!(
            concat!(
                "#!/bin/sh\n",
                "previous=\n",
                "schema_path=\n",
                "for argument in \"$@\"; do\n",
                "  case \"$argument\" in *schema-secret-sentinel*) exit 91;; esac\n",
                "  if [ \"$previous\" = --output-schema ]; then schema_path=$argument; fi\n",
                "  previous=$argument\n",
                "done\n",
                "[ -n \"$schema_path\" ] || exit 92\n",
                "[ -f \"$schema_path\" ] || exit 93\n",
                "printf '%s' \"$schema_path\" > '{}'\n",
                "cat \"$schema_path\" > '{}'\n",
                "cat >/dev/null\n",
                "printf '%s\\n' '{{\"type\":\"thread.started\",\"thread_id\":\"thread-schema\"}}'\n",
                "printf '%s\\n' '{{\"type\":\"item.completed\",\"item\":{{\"type\":\"agent_message\",\"text\":\"earlier\"}}}}'\n",
                "printf '%s\\n' '{{\"type\":\"item.completed\",\"item\":{{\"type\":\"agent_message\",\"text\":\"ok\"}}}}'\n",
                "printf '%s\\n' '{{\"type\":\"turn.completed\"}}'\n",
            ),
            recorded_path.display(),
            recorded_schema.display()
        );
        std::fs::write(&binary, script).expect("write fake Codex CLI");
        let mut permissions = std::fs::metadata(&binary).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&binary, permissions).unwrap();
        let service = CodexService::new().with_binary(&binary);

        let fresh = service
            .clone()
            .oneshot(request("fresh", output_schema_options()))
            .await
            .expect("fresh structured turn succeeds");
        assert_eq!(fresh.output, "ok");
        let fresh_path = PathBuf::from(
            std::fs::read_to_string(&recorded_path).expect("fresh schema path recorded"),
        );
        let fresh_schema: serde_json::Value = serde_json::from_slice(
            &std::fs::read(&recorded_schema).expect("fresh schema recorded"),
        )
        .expect("fresh schema remains JSON");
        assert_eq!(fresh_schema, test_output_schema());
        assert!(!fresh_path.exists());

        let resumed = service
            .oneshot(AgentRequest::new(
                Turn::new("resume")
                    .with_options(output_schema_options())
                    .resume(SessionHandle::new(PROVIDER, "thread-schema")),
            ))
            .await
            .expect("resumed structured turn succeeds");
        assert_eq!(resumed.output, "ok");
        let resume_path = PathBuf::from(
            std::fs::read_to_string(&recorded_path).expect("resume schema path recorded"),
        );
        let _ = std::fs::remove_file(binary);
        let _ = std::fs::remove_file(recorded_path);
        let _ = std::fs::remove_file(recorded_schema);
        assert!(!resume_path.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn output_schema_is_cleaned_after_provider_failure() {
        use std::os::unix::fs::PermissionsExt;

        let binary = std::env::temp_dir().join(format!(
            "tower-agent-codex-output-schema-failure-{}.sh",
            std::process::id()
        ));
        let recorded_path = std::env::temp_dir().join(format!(
            "tower-agent-codex-output-schema-failure-{}.path",
            std::process::id()
        ));
        let script = format!(
            concat!(
                "#!/bin/sh\n",
                "previous=\n",
                "for argument in \"$@\"; do\n",
                "  if [ \"$previous\" = --output-schema ]; then printf '%s' \"$argument\" > '{}'; fi\n",
                "  previous=$argument\n",
                "done\n",
                "cat >/dev/null\n",
                "printf '%s\\n' 'schema-secret-sentinel' >&2\n",
                "exit 23\n",
            ),
            recorded_path.display()
        );
        std::fs::write(&binary, script).expect("write failing fake Codex CLI");
        let mut permissions = std::fs::metadata(&binary).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&binary, permissions).unwrap();

        let error = CodexService::new()
            .with_binary(&binary)
            .oneshot(request("failure", output_schema_options()))
            .await
            .expect_err("provider failure remains an error");
        let schema_path = PathBuf::from(
            std::fs::read_to_string(&recorded_path).expect("failure schema path recorded"),
        );
        let _ = std::fs::remove_file(binary);
        let _ = std::fs::remove_file(recorded_path);
        assert!(!schema_path.exists());
        assert!(!format!("{error:?}").contains("schema-secret-sentinel"));
        assert!(!format!("{error:?}").contains(&schema_path.display().to_string()));
    }

    #[tokio::test]
    async fn output_schema_is_cleaned_after_launch_failure() {
        let operation_id = OperationId::from_u64(75004);
        let before = operation_schema_files(operation_id);
        let request = AgentRequest::with_context(
            Turn::new("hello").with_options(output_schema_options()),
            CallContext::new().with_operation_id(operation_id),
        );
        let error = CodexService::new()
            .with_binary("/definitely/not/a/codex/binary")
            .oneshot(request)
            .await
            .expect_err("missing binary must fail");

        assert_eq!(error.phase, FailurePhase::Launch);
        assert_eq!(operation_schema_files(operation_id), before);
        assert!(!format!("{error:?}").contains("schema-secret-sentinel"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn clear_child_environment_keeps_only_allowed_and_explicit_values() {
        use std::os::unix::fs::PermissionsExt;

        let path = std::env::temp_dir().join(format!(
            "tower-agent-codex-environment-{}.sh",
            std::process::id()
        ));
        std::fs::write(
            &path,
            concat!(
                "#!/bin/sh\n",
                "[ -z \"${HOME+x}\" ] || exit 91\n",
                "[ -n \"$PATH\" ] || exit 92\n",
                "[ \"$TOWER_AGENT_EXPLICIT\" = \"visible\" ] || exit 93\n",
                "[ \"$CODEX_HOME\" = \"/host/codex\" ] || exit 94\n",
                "cat >/dev/null\n",
                "printf '%s\\n' '{\"type\":\"thread.started\",\"thread_id\":\"thread-1\"}'\n",
                "printf '%s\\n' '{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"ok\"}}'\n",
                "printf '%s\\n' '{\"type\":\"turn.completed\"}'\n",
            ),
        )
        .expect("write fake Codex CLI");
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&path, permissions).unwrap();

        let policy = ChildEnvironmentPolicy::clear()
            .allow_ambient("PATH")
            .with_variable("TOWER_AGENT_EXPLICIT", "visible");
        let outcome = CodexService::new()
            .with_binary(&path)
            .with_codex_home("/host/codex")
            .with_child_environment_policy(policy)
            .oneshot(request("hello", CodexOptions::default()))
            .await
            .expect("filtered child environment reaches Codex");
        let _ = std::fs::remove_file(path);

        assert_eq!(outcome.output, "ok");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn automation_and_ephemeral_controls_reach_fresh_and_resume_argv() {
        use std::os::unix::fs::PermissionsExt;

        let path = std::env::temp_dir().join(format!(
            "tower-agent-codex-ambient-{}.sh",
            std::process::id()
        ));
        let argv_path = std::env::temp_dir().join(format!(
            "tower-agent-codex-ambient-{}.args",
            std::process::id()
        ));
        let script = format!(
            concat!(
                "#!/bin/sh\n",
                "printf '%s\\n' \"$@\" > '{}'\n",
                "cat >/dev/null\n",
                "printf '%s\\n' '{{\"type\":\"thread.started\",\"thread_id\":\"transient-thread\"}}'\n",
                "printf '%s\\n' '{{\"type\":\"item.completed\",\"item\":{{\"type\":\"agent_message\",\"text\":\"ok\"}}}}'\n",
                "printf '%s\\n' '{{\"type\":\"turn.completed\"}}'\n",
            ),
            argv_path.display()
        );
        std::fs::write(&path, script).expect("write fake Codex CLI");
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&path, permissions).unwrap();

        let skill_root = tempfile::tempdir().expect("create sentinel skill root");
        let sentinel_skill = create_test_skill(skill_root.path(), "sentinel");
        let skill_policy = CodexSkillPolicy::DisableExact(vec![sentinel_skill]);
        let skill_config = render_skill_config(&skill_policy)
            .expect("valid skill policy")
            .expect("skill config rendered");
        let service = CodexService::new()
            .with_binary(&path)
            .with_ambient_context_policy(CodexAmbientContextPolicy::Automation)
            .with_skill_policy(skill_policy);
        let options = CodexOptions {
            ephemeral: true,
            ..CodexOptions::default()
        };
        let fresh = service
            .clone()
            .oneshot(request("fresh", options.clone()))
            .await
            .expect("fresh automation turn succeeds");
        let fresh_args = std::fs::read_to_string(&argv_path).expect("fresh argv recorded");
        assert_eq!(
            fresh_args.lines().collect::<Vec<_>>(),
            [
                "exec",
                "-c",
                "project_doc_max_bytes=0",
                "-c",
                skill_config.as_str(),
                "--sandbox",
                "read-only",
                "--strict-config",
                "--skip-git-repo-check",
                "--ephemeral",
                "--ignore-user-config",
                "--ignore-rules",
                "-",
                "--json",
            ]
        );
        assert_eq!(
            fresh.session, None,
            "ephemeral thread must not be resumable"
        );

        let resumed = service
            .oneshot(AgentRequest::new(
                Turn::new("resume")
                    .with_options(options)
                    .resume(SessionHandle::new(PROVIDER, "thread-existing")),
            ))
            .await
            .expect("resumed automation turn succeeds");
        let resume_args = std::fs::read_to_string(&argv_path).expect("resume argv recorded");
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(argv_path);

        assert_eq!(
            resume_args.lines().collect::<Vec<_>>(),
            [
                "exec",
                "resume",
                "-c",
                "sandbox_mode=\"read-only\"",
                "-c",
                "project_doc_max_bytes=0",
                "-c",
                skill_config.as_str(),
                "--strict-config",
                "--skip-git-repo-check",
                "--ephemeral",
                "--ignore-user-config",
                "--ignore-rules",
                "thread-existing",
                "-",
                "--json",
            ]
        );
        assert_eq!(
            resumed.session, None,
            "ephemeral resume must not imply the turn was persisted"
        );
    }

    #[test]
    fn fresh_command_preserves_existing_read_only_default_and_maps_options() {
        let options = CodexOptions {
            system_prompt: Some("you are a helper".into()),
            model: Some("gpt-test".into()),
            additional_directories: vec![PathBuf::from("/work/extra")],
            output_schema: None,
            filesystem_authority: FilesystemAuthority::ReadOnly,
            ephemeral: false,
        };
        let (prepared, _) =
            prepare(request("do it", options), &AuthorityPolicy::read_only()).expect("valid turn");
        let args = fresh_command(&prepared, CodexAmbientContextPolicy::Inherit, None, None).args();

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

        let guarded = fresh_command(
            &prepared,
            CodexAmbientContextPolicy::Inherit,
            None,
            Some("skills.config=[]"),
        )
        .args();
        assert!(guarded.iter().any(|arg| arg == "--strict-config"));
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
        let args = fresh_command(&prepared, CodexAmbientContextPolicy::Inherit, None, None).args();

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
    async fn flag_shaped_resume_handles_are_rejected_before_launch() {
        for handle in ["--last", "-l"] {
            let turn = Turn::new("continue")
                .with_options(CodexOptions::default())
                .resume(SessionHandle::new(PROVIDER, handle));
            let error = CodexService::new()
                .with_binary("/definitely/not/a/codex/binary")
                .oneshot(AgentRequest::new(turn))
                .await
                .expect_err("flag-shaped session must be rejected");

            assert_eq!(error.kind, ErrorKind::InvalidRequest);
            assert_eq!(error.phase, FailurePhase::Validation);
            assert_eq!(error.effects, EffectState::None);
            assert!(!error.message.contains(handle));
        }
    }

    #[tokio::test]
    async fn preassigned_fresh_session_is_refused_before_codex_launch() {
        let request = AgentRequest::with_context(
            Turn::new("hello").with_options(CodexOptions::default()),
            CallContext::new().with_preassigned_session(SessionHandle::new(
                "codex",
                "11111111-1111-4111-8111-111111111111",
            )),
        );
        let error = CodexService::new()
            .with_binary("/definitely/not/a/codex/binary")
            .oneshot(request)
            .await
            .expect_err("unsupported preassignment must not launch Codex");

        assert_eq!(error.kind, ErrorKind::Unsupported);
        assert_eq!(error.phase, FailurePhase::Validation);
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
            output_schema: None,
            filesystem_authority: FilesystemAuthority::ReadOnly,
            ephemeral: false,
        };
        let args = resume_command(
            &turn,
            "thread-1",
            CodexAmbientContextPolicy::Inherit,
            None,
            None,
        )
        .args();

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
            events: vec![serde_json::from_str(r#"{"type":"turn.completed"}"#).unwrap()],
        };

        let outcome = settle_outcome(result, None, Duration::ZERO, false, false)
            .expect("completed result settles successfully");
        assert_eq!(
            outcome.session.as_ref().map(SessionHandle::value),
            Some("thread-1")
        );
    }

    #[test]
    fn terminal_events_gate_success_and_classify_failure() {
        let valid = query_result(&[
            r#"{"type":"thread.started","thread_id":"thread-valid"}"#,
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"done"}}"#,
            r#"{"type":"turn.completed","usage":{"input_tokens":3,"output_tokens":2}}"#,
        ]);
        let outcome = settle_outcome(valid, None, Duration::from_millis(12), false, false)
            .expect("one final completion is successful");
        assert_eq!(outcome.output, "done");
        assert_eq!(outcome.usage.and_then(TokenUsage::total), Some(5));

        let failed = query_result(&[
            r#"{"type":"thread.started","thread_id":"thread-failed"}"#,
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"partial-secret"}}"#,
            r#"{"type":"turn.failed","error":{"message":"shared rollout token budget exhausted"}}"#,
        ]);
        let error = settle_outcome(failed, None, Duration::from_millis(7), false, false)
            .expect_err("failed terminal event is never success");
        assert_eq!(error.kind, ErrorKind::Budget);
        assert_eq!(error.phase, FailurePhase::Running);
        assert_eq!(error.effects, EffectState::Possible);
        assert_eq!(
            error
                .evidence
                .as_deref()
                .and_then(|evidence| evidence.session.as_ref())
                .map(SessionHandle::value),
            Some("thread-failed")
        );
        assert!(!format!("{error:?}").contains("partial-secret"));

        let missing = query_result(&[
            r#"{"type":"thread.started","thread_id":"thread-missing"}"#,
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"partial-missing"}}"#,
        ]);
        let error = settle_outcome(missing, None, Duration::ZERO, false, false)
            .expect_err("missing terminal event is settlement failure");
        assert_eq!(error.kind, ErrorKind::Provider);
        assert_eq!(error.phase, FailurePhase::Settlement);
        assert_eq!(error.effects, EffectState::Possible);
        assert!(error.evidence.is_none());
        assert!(!format!("{error:?}").contains("partial-missing"));

        for conflicting in [
            query_result(&[
                r#"{"type":"turn.failed","error":{"message":"failed"}}"#,
                r#"{"type":"turn.completed"}"#,
            ]),
            query_result(&[
                r#"{"type":"turn.completed"}"#,
                r#"{"type":"item.completed","item":{"type":"agent_message","text":"after-terminal"}}"#,
            ]),
        ] {
            let error = settle_outcome(conflicting, None, Duration::ZERO, false, false)
                .expect_err("conflicting terminal state is settlement failure");
            assert_eq!(error.kind, ErrorKind::Provider);
            assert_eq!(error.phase, FailurePhase::Settlement);
            assert_eq!(error.effects, EffectState::Possible);
            assert!(error.evidence.is_none());
        }
    }

    #[test]
    fn structured_outcome_selects_the_final_agent_message() {
        let structured = query_result(&[
            r#"{"type":"thread.started","thread_id":"thread-structured"}"#,
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"{\"answer\":\"intermediate\"}"}}"#,
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"{\"answer\":\"final\"}"}}"#,
            r#"{"type":"turn.completed"}"#,
        ]);
        assert_eq!(
            structured.result,
            r#"{"answer":"intermediate"}{"answer":"final"}"#
        );

        let outcome = settle_outcome(structured, None, Duration::ZERO, false, true)
            .expect("structured completion selects one final message");
        assert_eq!(outcome.output, r#"{"answer":"final"}"#);

        let prose = query_result(&[
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"one "}}"#,
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"two"}}"#,
            r#"{"type":"turn.completed"}"#,
        ]);
        let outcome = settle_outcome(prose, None, Duration::ZERO, false, false)
            .expect("prose completion retains aggregation semantics");
        assert_eq!(outcome.output, "one two");
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
    async fn exit_zero_failed_stream_is_not_a_successful_outcome() {
        use std::os::unix::fs::PermissionsExt;

        let path = std::env::temp_dir().join(format!(
            "tower-agent-codex-terminal-failure-{}.sh",
            std::process::id()
        ));
        std::fs::write(
            &path,
            concat!(
                "#!/bin/sh\n",
                "cat >/dev/null\n",
                "printf '%s\\n' '{\"type\":\"thread.started\",\"thread_id\":\"thread-failed\"}'\n",
                "printf '%s\\n' '{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"partial-secret\"}}'\n",
                "printf '%s\\n' '{\"type\":\"turn.failed\",\"error\":{\"message\":\"shared rollout token budget exhausted\"}}'\n",
            ),
        )
        .expect("write failed-turn fake Codex CLI");
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&path, permissions).unwrap();

        let error = CodexService::new()
            .with_binary(&path)
            .oneshot(request("hello", CodexOptions::default()))
            .await
            .expect_err("exit-zero turn.failed stream must fail");
        let _ = std::fs::remove_file(path);

        assert_eq!(error.kind, ErrorKind::Budget);
        assert_eq!(error.phase, FailurePhase::Running);
        assert_eq!(error.effects, EffectState::Possible);
        assert!(!format!("{error:?}").contains("partial-secret"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn spawn_observer_receives_the_owned_codex_process_group() {
        use std::os::unix::fs::PermissionsExt;

        let path = std::env::temp_dir().join(format!(
            "tower-agent-codex-spawn-receipt-{}.sh",
            std::process::id()
        ));
        std::fs::write(
            &path,
            concat!(
                "#!/bin/sh\n",
                "cat >/dev/null\n",
                "printf '%s\\n' '{\"type\":\"thread.started\",\"thread_id\":\"thread-1\"}'\n",
                "printf '%s\\n' '{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"done\"}}'\n",
                "printf '%s\\n' '{\"type\":\"turn.completed\"}'\n",
            ),
        )
        .expect("write fake Codex CLI");
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&path, permissions).unwrap();

        let (sender, receiver) = std::sync::mpsc::channel();
        let service = CodexService::new()
            .with_binary(&path)
            .with_die_with_parent(true)
            .with_spawn_observer(SpawnObserver::new(move |receipt| {
                sender.send(receipt).unwrap();
            }));
        assert!(service.die_with_parent());
        assert_eq!(
            CodexService::die_with_parent_supported(),
            cfg!(target_os = "linux")
        );

        service
            .oneshot(AgentRequest::with_context(
                Turn::new("hello").with_options(CodexOptions::default()),
                CallContext::new().with_operation_id(OperationId::from_u64(63)),
            ))
            .await
            .expect("fake Codex run succeeds");
        let _ = std::fs::remove_file(path);

        let receipt = receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("spawn receipt delivered");
        assert_eq!(receipt.provider, PROVIDER);
        assert_eq!(receipt.operation_id, OperationId::from_u64(63));
        assert!(receipt.pid > 0);
        assert_eq!(receipt.process_group_id, Some(receipt.pid));
    }

    #[cfg(unix)]
    const CODEX_PDEATHSIG_HELPER: &str = "TOWER_AGENT_CODEX_PDEATHSIG_HELPER";

    #[cfg(unix)]
    #[test]
    fn codex_pdeathsig_helper_process() {
        if std::env::var(CODEX_PDEATHSIG_HELPER).is_err() {
            return;
        }
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        let path = std::env::temp_dir().join(format!(
            "tower-agent-codex-pdeathsig-{}.sh",
            std::process::id()
        ));
        std::fs::write(&path, "#!/bin/sh\ncat >/dev/null\nexec /bin/sleep 300\n")
            .expect("write blocking fake Codex CLI");
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&path, permissions).unwrap();

        let service = CodexService::new()
            .with_binary(&path)
            .with_die_with_parent(true)
            .with_spawn_observer(SpawnObserver::new(|receipt| {
                println!("PID {}", receipt.pid);
                let _ = std::io::stdout().flush();
            }));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _ = runtime.block_on(service.oneshot(request("hello", CodexOptions::default())));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn codex_child_dies_when_the_worker_is_sigkilled() {
        use std::io::{BufRead, BufReader};
        use std::process::{Command, Stdio};

        let mut helper = Command::new(std::env::current_exe().expect("test binary path"))
            .args([
                "--exact",
                "tests::codex_pdeathsig_helper_process",
                "--nocapture",
            ])
            .env(CODEX_PDEATHSIG_HELPER, "1")
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn Codex worker helper");
        let pid: u32 = BufReader::new(helper.stdout.take().expect("piped stdout"))
            .lines()
            .map_while(std::result::Result::ok)
            .find_map(|line| line.strip_prefix("PID ").and_then(|pid| pid.parse().ok()))
            .expect("helper reported provider pid");

        helper.kill().expect("SIGKILL the Codex worker helper");
        let _ = helper.wait();

        for _ in 0..50 {
            let output = Command::new("ps")
                .args(["-o", "state=", "-p", &pid.to_string()])
                .output()
                .expect("ps is available");
            let state = String::from_utf8_lossy(&output.stdout);
            if state.trim().is_empty() || state.trim().starts_with('Z') {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("Codex child {pid} survived its SIGKILLed worker");
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
        let schema_record = std::env::temp_dir().join(format!(
            "tower-agent-codex-cancel-{}.schema",
            std::process::id()
        ));
        let script = format!(
            concat!(
                "#!/bin/sh\n",
                "previous=\n",
                "for argument in \"$@\"; do\n",
                "  if [ \"$previous\" = --output-schema ]; then printf '%s' \"$argument\" > '{}'; fi\n",
                "  previous=$argument\n",
                "done\n",
                "cat >/dev/null\n",
                "sleep 30 </dev/null &\n",
                "child=$!\n",
                "printf 'parent=%s\\nchild=%s\\n' \"$$\" \"$child\" > '{}'\n",
                "wait \"$child\"\n",
            ),
            schema_record.display(),
            pid_path.display()
        );
        std::fs::write(&path, script).expect("write blocking fake Codex CLI");
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&path, permissions).unwrap();

        let cancellation = CancellationToken::new();
        let request = AgentRequest::with_context(
            Turn::new("hello").with_options(output_schema_options()),
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
        let output_schema_path = PathBuf::from(
            std::fs::read_to_string(&schema_record)
                .expect("fake Codex recorded its output schema path"),
        );
        assert!(output_schema_path.exists());
        cancellation.cancel();
        let error = tokio::time::timeout(Duration::from_secs(2), call)
            .await
            .expect("cancelled call must settle")
            .expect("provider task must not panic")
            .expect_err("cancelled call must fail");
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(pid_path);
        let _ = std::fs::remove_file(schema_record);

        assert_eq!(error.kind, ErrorKind::Cancelled);
        assert_eq!(error.phase, FailurePhase::Running);
        assert_eq!(error.effects, EffectState::Possible);
        assert!(!output_schema_path.exists());
        assert!(!format!("{error:?}").contains("schema-secret-sentinel"));
        assert!(!format!("{error:?}").contains(&output_schema_path.display().to_string()));
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

#[cfg(test)]
mod error_mapping_tests {
    use codex_wrapper::Error;
    use codex_wrapper::version::CliVersion;

    use super::*;

    struct Case {
        name: &'static str,
        error: Error,
        kind: ErrorKind,
        phase: FailurePhase,
        effects: EffectState,
    }

    #[test]
    fn typed_wrapper_failures_preserve_retry_evidence_without_leaking_diagnostics() {
        let secret = "secret-command-session-or-token";
        let json_source = serde_json::from_str::<serde_json::Value>("{")
            .expect_err("invalid JSON creates a parser error");
        let cases = vec![
            Case {
                name: "not found",
                error: Error::NotFound,
                kind: ErrorKind::Provider,
                phase: FailurePhase::Launch,
                effects: EffectState::None,
            },
            Case {
                name: "authentication",
                error: Error::Auth {
                    message: secret.into(),
                    command: secret.into(),
                    exit_code: 1,
                    working_dir: Some(PathBuf::from(secret)),
                },
                kind: ErrorKind::Authentication,
                phase: FailurePhase::Running,
                effects: EffectState::Possible,
            },
            Case {
                name: "configuration",
                error: Error::Config {
                    message: secret.into(),
                    command: secret.into(),
                    exit_code: 1,
                    working_dir: Some(PathBuf::from(secret)),
                },
                kind: ErrorKind::Provider,
                phase: FailurePhase::Validation,
                effects: EffectState::None,
            },
            Case {
                name: "untrusted directory",
                error: Error::NotTrustedDirectory {
                    message: secret.into(),
                    command: secret.into(),
                    exit_code: 1,
                    working_dir: Some(PathBuf::from(secret)),
                },
                kind: ErrorKind::Unauthorized,
                phase: FailurePhase::Validation,
                effects: EffectState::None,
            },
            Case {
                name: "session missing",
                error: Error::SessionNotFound {
                    message: secret.into(),
                    command: secret.into(),
                    exit_code: 1,
                    working_dir: Some(PathBuf::from(secret)),
                },
                kind: ErrorKind::InvalidRequest,
                phase: FailurePhase::Validation,
                effects: EffectState::None,
            },
            Case {
                name: "unclassified command failure",
                error: Error::CommandFailed {
                    command: secret.into(),
                    exit_code: 23,
                    stdout: secret.into(),
                    stderr: secret.into(),
                    working_dir: Some(PathBuf::from(secret)),
                },
                kind: ErrorKind::Provider,
                phase: FailurePhase::Running,
                effects: EffectState::Possible,
            },
            Case {
                name: "spawn I/O",
                error: Error::Io {
                    message: "failed to spawn codex: secret".into(),
                    source: std::io::Error::other(secret),
                    working_dir: Some(PathBuf::from(secret)),
                },
                kind: ErrorKind::Provider,
                phase: FailurePhase::Launch,
                effects: EffectState::None,
            },
            Case {
                name: "in-flight I/O",
                error: Error::Io {
                    message: secret.into(),
                    source: std::io::Error::other(secret),
                    working_dir: Some(PathBuf::from(secret)),
                },
                kind: ErrorKind::Provider,
                phase: FailurePhase::Running,
                effects: EffectState::Possible,
            },
            Case {
                name: "timeout",
                error: Error::Timeout { timeout_seconds: 5 },
                kind: ErrorKind::DeadlineExceeded,
                phase: FailurePhase::Running,
                effects: EffectState::Possible,
            },
            Case {
                name: "cancelled",
                error: Error::Cancelled { grace_seconds: 1 },
                kind: ErrorKind::Cancelled,
                phase: FailurePhase::Running,
                effects: EffectState::Possible,
            },
            Case {
                name: "invalid JSON",
                error: Error::Json {
                    message: secret.into(),
                    source: json_source,
                },
                kind: ErrorKind::Provider,
                phase: FailurePhase::Settlement,
                effects: EffectState::Possible,
            },
            Case {
                name: "token budget",
                error: Error::TokenBudgetExceeded {
                    total_tokens: 120,
                    max_tokens: 100,
                },
                kind: ErrorKind::Budget,
                phase: FailurePhase::Running,
                effects: EffectState::Possible,
            },
            Case {
                name: "invalid rollout budget",
                error: Error::InvalidRolloutBudget {
                    message: secret.into(),
                },
                kind: ErrorKind::InvalidRequest,
                phase: FailurePhase::Validation,
                effects: EffectState::None,
            },
            Case {
                name: "dangerous bypass refused",
                error: Error::DangerousNotAllowed { variable: secret },
                kind: ErrorKind::Unauthorized,
                phase: FailurePhase::Validation,
                effects: EffectState::None,
            },
            Case {
                name: "old CLI",
                error: Error::VersionMismatch {
                    found: CliVersion::new(0, 1, 0),
                    minimum: CliVersion::new(0, 145, 0),
                },
                kind: ErrorKind::Unsupported,
                phase: FailurePhase::Launch,
                effects: EffectState::None,
            },
            Case {
                name: "untested CLI",
                error: Error::UntestedCliVersion {
                    found: CliVersion::new(1, 0, 0),
                    tested_min: CliVersion::new(0, 145, 0),
                    tested_max: CliVersion::new(0, 147, 0),
                },
                kind: ErrorKind::Unsupported,
                phase: FailurePhase::Launch,
                effects: EffectState::None,
            },
        ];

        for case in cases {
            let mapped = map_run_error(case.error);
            assert_eq!(mapped.kind, case.kind, "{} kind", case.name);
            assert_eq!(mapped.phase, case.phase, "{} phase", case.name);
            assert_eq!(mapped.effects, case.effects, "{} effects", case.name);
            assert!(!mapped.message.is_empty(), "{} message", case.name);
            assert!(
                !mapped.message.contains(secret),
                "{} leaked a provider diagnostic: {}",
                case.name,
                mapped.message
            );
        }
    }
}

#[cfg(test)]
mod preflight_parity_tests {
    use tower::ServiceExt;
    use tower_agent::{AgentRequest, FilesystemAuthority, SessionHandle, Turn};

    use super::{CodexOptions, CodexService};

    fn invalid_turns() -> Vec<Turn<CodexOptions>> {
        vec![
            Turn::new("   ").with_options(CodexOptions::default()),
            Turn::new("hello").with_options(CodexOptions {
                model: Some("  ".to_string()),
                ..CodexOptions::default()
            }),
            Turn::new("hello")
                .with_options(CodexOptions::default())
                .resume(SessionHandle::new("claude", "abc")),
            Turn::new("hello")
                .with_options(CodexOptions::default())
                .resume(SessionHandle::new("codex", "-rf")),
            Turn::new("hello")
                .with_options(CodexOptions {
                    additional_directories: vec!["/tmp/a".into()],
                    ..CodexOptions::default()
                })
                .resume(SessionHandle::new("codex", "abc")),
            Turn::new("hello").with_options(CodexOptions {
                filesystem_authority: FilesystemAuthority::WorkspaceWrite,
                ..CodexOptions::default()
            }),
        ]
    }

    #[tokio::test]
    async fn preflight_refusals_match_call_refusals() {
        for turn in invalid_turns() {
            let service = CodexService::new();
            let preflight = service.preflight(&turn).expect_err("preflight refuses");
            let call = service
                .oneshot(AgentRequest::new(turn))
                .await
                .expect_err("call refuses");
            assert_eq!(preflight, call);
        }
    }

    #[test]
    fn preflight_accepts_a_valid_turn_without_any_launch_machinery() {
        let service = CodexService::new().with_binary("/nonexistent/codex-binary");
        let turn = Turn::new("hello").with_options(CodexOptions::default());
        assert!(service.preflight(&turn).is_ok());
    }
}

#[cfg(all(test, unix))]
mod rejected_request_tests {
    use std::os::unix::fs::PermissionsExt;

    use tower::ServiceExt;
    use tower_agent::{AgentRequest, EffectState, ErrorKind, FailurePhase, Turn};

    use super::{CodexOptions, CodexService};

    /// The terminal line and exit code captured from a live `codex-cli`
    /// 0.149.0 run whose output schema omitted a property `type`. Codex emits
    /// `turn.failed` and then exits 1, so both halves are reproduced.
    const CAPTURED_REJECTION: &str = concat!(
        "#!/bin/sh\n",
        "cat >/dev/null\n",
        "printf '%s\\n' '{\"type\":\"thread.started\",\"thread_id\":\"01a040b1-a346-7972-9d80-1fc6083f220c\"}'\n",
        "printf '%s\\n' '{\"type\":\"turn.started\"}'\n",
        r#"printf '%s\n' '{"type":"turn.failed","error":{"message":"{\n  \"type\": \"error\",\n  \"error\": {\n    \"type\": \"invalid_request_error\",\n    \"code\": \"invalid_json_schema\",\n    \"message\": \"Invalid schema for response_format ''codex_output_schema'': In context=(''properties'', ''ok''), schema must have a ''type'' key.\",\n    \"param\": \"text.format.schema\"\n  },\n  \"status\": 400\n}"}}'"#,
        "\n",
        // The real CLI exits nonzero after reporting the rejection.
        "exit 1\n",
    );

    fn scripted(name: &str, script: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "tower-agent-codex-{}-{}.sh",
            name,
            std::process::id()
        ));
        std::fs::write(&path, script).expect("write fake Codex CLI");
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&path, permissions).unwrap();
        path
    }

    #[tokio::test]
    async fn an_api_rejected_request_does_not_become_running_and_possible() {
        let path = scripted("rejected", CAPTURED_REJECTION);
        let error = CodexService::new()
            .with_binary(&path)
            .oneshot(AgentRequest::new(
                Turn::new("say ok").with_options(CodexOptions::default()),
            ))
            .await
            .expect_err("a rejected request is a failure");
        let _ = std::fs::remove_file(path);

        assert_eq!(error.kind, ErrorKind::InvalidRequest);
        assert_eq!(error.phase, FailurePhase::Validation);
        // Generation never began, so this is safe to correct and retry.
        assert_eq!(error.effects, EffectState::None);

        // A thread id was emitted, but the turn cannot be continued, so it
        // must not poison a managed continuation.
        let session = error
            .evidence
            .as_deref()
            .and_then(|evidence| evidence.session.clone());
        assert!(session.is_none(), "rejected requests advertise no session");

        // Provider diagnostics stay out of the public surface.
        for text in [error.message.clone(), format!("{error:?}")] {
            assert!(!text.contains("codex_output_schema"), "{text}");
            assert!(!text.contains("invalid_json_schema"), "{text}");
        }
    }

    #[tokio::test]
    async fn other_provider_failures_stay_conservative() {
        let path = scripted(
            "generic",
            concat!(
                "#!/bin/sh\n",
                "cat >/dev/null\n",
                "printf '%s\\n' '{\"type\":\"thread.started\",\"thread_id\":\"thread-1\"}'\n",
                "printf '%s\\n' '{\"type\":\"turn.failed\",\"error\":{\"message\":\"tool policy rejected\"}}'\n",
                "exit 1\n",
            ),
        );
        let error = CodexService::new()
            .with_binary(&path)
            .oneshot(AgentRequest::new(
                Turn::new("say ok").with_options(CodexOptions::default()),
            ))
            .await
            .expect_err("a generic turn failure is a failure");
        let _ = std::fs::remove_file(path);

        // A failure the provider did not classify may have done work.
        assert_eq!(error.kind, ErrorKind::Provider);
        assert_eq!(error.phase, FailurePhase::Running);
        assert_eq!(error.effects, EffectState::Possible);
    }
}
