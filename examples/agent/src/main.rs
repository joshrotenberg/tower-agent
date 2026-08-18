//! Minimal executable composition of the native provider services.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use clap::{Parser, ValueEnum};
use tower::{ServiceBuilder, ServiceExt};
use tower_agent::layer::{
    AdmissionLayer, AuthorityLayer, CatchPanicLayer, DeadlineLayer, SuperviseLayer,
    ValidateTurnLayer,
};
use tower_agent::{
    AgentRequest, AuthorityPolicy, CallContext, CancellationToken, FilesystemAuthority,
    SessionHandle, Turn, TurnOutcome,
};
use tower_agent_claude::{ClaudeOptions, ClaudeService};
use tower_agent_codex::{CodexOptions, CodexService};

#[derive(Parser)]
#[command(name = "agent", about = "run one finite agent turn")]
struct Cli {
    /// Provider CLI to invoke.
    #[arg(long, value_enum, default_value_t = Provider::Claude)]
    provider: Provider,
    /// User prompt for the turn.
    prompt: String,
    /// Working directory visible to the provider.
    #[arg(long)]
    working_directory: Option<PathBuf>,
    /// Provider-private session or thread id to resume.
    #[arg(long)]
    session: Option<String>,
    /// Provider model override.
    #[arg(long)]
    model: Option<String>,
    /// Additional directories made available to the provider.
    #[arg(long = "add-dir")]
    additional_directories: Vec<PathBuf>,
    /// Allow Codex to write within its workspace sandbox.
    #[arg(long)]
    workspace_write: bool,
    /// Host-side deadline in seconds.
    #[arg(long, default_value_t = 600)]
    timeout: u64,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Provider {
    Claude,
    Codex,
}

impl Provider {
    const fn tag(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();
    if cli.workspace_write && !matches!(cli.provider, Provider::Codex) {
        anyhow::bail!("--workspace-write is only enforceable by the Codex provider");
    }

    let cancellation = CancellationToken::new();
    let signal = cancellation.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            signal.cancel();
        }
    });
    let context = CallContext::new()
        .with_cancellation(cancellation)
        .with_deadline(Instant::now() + Duration::from_secs(cli.timeout));

    let outcome = match cli.provider {
        Provider::Claude => run_claude(&cli, context).await?,
        Provider::Codex => run_codex(&cli, context).await?,
    };
    print_outcome(&outcome);
    Ok(())
}

async fn run_claude(
    cli: &Cli,
    context: CallContext,
) -> Result<TurnOutcome, tower_agent::AgentError> {
    let options = ClaudeOptions {
        model: cli.model.clone(),
        additional_directories: cli.additional_directories.clone(),
        ..ClaudeOptions::default()
    };
    let request = AgentRequest::with_context(turn(cli, options), context);
    stack(ClaudeService::new()).oneshot(request).await
}

async fn run_codex(
    cli: &Cli,
    context: CallContext,
) -> Result<TurnOutcome, tower_agent::AgentError> {
    let authority = if cli.workspace_write {
        FilesystemAuthority::WorkspaceWrite
    } else {
        FilesystemAuthority::ReadOnly
    };
    let mut policy = AuthorityPolicy::new(authority);
    if authority == FilesystemAuthority::WorkspaceWrite {
        if let Some(directory) = &cli.working_directory {
            policy = policy.allow_writable_root(directory).map_err(|error| {
                tower_agent::AgentError::invalid_request(format!(
                    "working directory cannot be authorized: {error}"
                ))
            })?;
        }
        for directory in &cli.additional_directories {
            policy = policy.allow_writable_root(directory).map_err(|error| {
                tower_agent::AgentError::invalid_request(format!(
                    "additional directory cannot be authorized: {error}"
                ))
            })?;
        }
    }
    let options = CodexOptions {
        model: cli.model.clone(),
        additional_directories: cli.additional_directories.clone(),
        filesystem_authority: authority,
        ..CodexOptions::default()
    };
    let request = AgentRequest::with_context(turn(cli, options), context);
    let provider = ServiceBuilder::new()
        .layer(AuthorityLayer::new(policy.clone()))
        .service(CodexService::new().with_authority_policy(policy));
    stack(provider).oneshot(request).await
}

fn turn<O>(cli: &Cli, options: O) -> Turn<O> {
    let mut turn = Turn::new(cli.prompt.clone()).with_options(options);
    turn.working_directory.clone_from(&cli.working_directory);
    turn.session = cli
        .session
        .as_ref()
        .map(|session| SessionHandle::new(cli.provider.tag(), session));
    turn
}

fn stack<S, O>(
    service: S,
) -> impl tower::Service<AgentRequest<Turn<O>>, Response = TurnOutcome, Error = tower_agent::AgentError>
where
    S: tower::Service<
            AgentRequest<Turn<O>>,
            Response = TurnOutcome,
            Error = tower_agent::AgentError,
        > + Clone,
    S::Future: Send + 'static,
    O: Send + 'static,
{
    ServiceBuilder::new()
        .layer(SuperviseLayer::new())
        .layer(CatchPanicLayer::new())
        .layer(AdmissionLayer::single_flight())
        .layer(DeadlineLayer::new())
        .layer(ValidateTurnLayer::new())
        .service(service)
}

fn print_outcome(outcome: &TurnOutcome) {
    println!("{}", outcome.output);
    if let Some(session) = &outcome.session {
        eprintln!("session: {}:{}", session.provider(), session.value());
    }
    if let Some(total) = outcome.usage.and_then(tower_agent::TokenUsage::total) {
        eprintln!("tokens: {total}");
    }
}
