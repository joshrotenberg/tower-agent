//! `agent`: the reference binary for tower-agent.
//!
//! Loads config (server defaults and named agents), picks a backend, and either
//! runs a single prompt (`run`) or serves the agent server over stdio (`serve`).
//! M0 ships the stub backend only, so the whole surface works without a live
//! model; the claude backend lands in M1.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand, ValueEnum};
use tower_agent::{Backend, Call, Config, Server, StubBackend};
use tower_agent_claude::ClaudeBackend;

#[derive(Parser)]
#[command(name = "agent", about = "an agent server over MCP")]
struct Cli {
    /// Config file: server defaults and named agents.
    #[arg(long, default_value = ".agent/config.toml")]
    config: PathBuf,
    /// Which backend runs prompts.
    #[arg(long, value_enum, default_value_t = BackendKind::Claude)]
    backend: BackendKind,
    /// Per-run backend timeout, in seconds.
    #[arg(long, default_value_t = 600)]
    timeout: u64,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Clone, Copy, ValueEnum)]
enum BackendKind {
    /// Run prompts through the Claude Code CLI.
    Claude,
    /// Run no model; echo the resolved parameters as JSON (a dry run).
    Stub,
}

#[derive(Subcommand)]
enum Cmd {
    /// List the configured agents.
    List,
    /// Run a single prompt and print the outcome.
    Run {
        /// The task or message.
        prompt: String,
        /// Select a configured agent, using its defaults.
        #[arg(long)]
        agent: Option<String>,
        /// Override the model.
        #[arg(long)]
        model: Option<String>,
        /// Bound the number of agentic turns.
        #[arg(long)]
        max_turns: Option<u32>,
        /// Continue a session (thread) so the backend resumes.
        #[arg(long)]
        session: Option<String>,
    },
    /// Serve the agent server over stdio (MCP).
    Serve,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Logs go to stderr so stdout stays clean for MCP under `serve`.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();
    let config = if cli.config.exists() {
        Config::load(&cli.config)?
    } else {
        eprintln!(
            "no config at {}, using empty defaults",
            cli.config.display()
        );
        Config::default()
    };
    let backend: Arc<dyn Backend> = match cli.backend {
        BackendKind::Stub => Arc::new(StubBackend),
        BackendKind::Claude => Arc::new(ClaudeBackend::new(Duration::from_secs(cli.timeout))),
    };
    let server = Server::new(config, backend);

    match cli.cmd {
        Cmd::List => {
            let names = server.agent_names();
            if names.is_empty() {
                eprintln!("no agents configured");
            }
            for name in names {
                println!("{name}");
            }
        }
        Cmd::Run {
            prompt,
            agent,
            model,
            max_turns,
            session,
        } => {
            let call = Call {
                prompt,
                agent,
                model,
                max_turns,
                session,
                ..Default::default()
            };
            let outcome = server
                .run(call)
                .await
                .map_err(|e| anyhow::anyhow!("run: {e}"))?;
            println!("{}", serde_json::to_string_pretty(&outcome)?);
        }
        Cmd::Serve => {
            let router = server.router();
            let mut transport = tower_mcp::StdioTransport::new(router);
            transport
                .run()
                .await
                .map_err(|e| anyhow::anyhow!("stdio serve: {e}"))?;
        }
    }
    Ok(())
}
