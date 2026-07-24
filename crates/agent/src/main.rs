//! `agent`: the reference binary for tower-agent.
//!
//! Loads config (server defaults and named agents), picks a backend, and either
//! runs a single prompt (`run`) or serves the agent server over stdio (`serve`).
//! M0 ships the stub backend only, so the whole surface works without a live
//! model; the claude backend lands in M1.

use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use tower_agent::{Call, Config, Server, StubBackend};

#[derive(Parser)]
#[command(name = "agent", about = "an agent server over MCP")]
struct Cli {
    /// Config file: server defaults and named agents.
    #[arg(long, default_value = ".agent/config.toml")]
    config: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
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
    let server = Server::new(config, Arc::new(StubBackend));

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
            session,
        } => {
            let call = Call {
                prompt,
                agent,
                system: None,
                model: None,
                effort: None,
                allowed_tools: None,
                cwd: None,
                session,
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
