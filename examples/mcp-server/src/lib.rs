//! Serve `tower-agent` over MCP on stdio.
//!
//! This is the first thing in the workspace that an MCP client can actually
//! talk to. Everything it composes is ordinary library code: the tools come
//! from `tower-agent-mcp`, the policy stack is the layer order documented in
//! `docs/resilience.md`, and the providers are the same services any other
//! host would build.
//!
//! # Configuration
//!
//! An MCP client launches a server with an environment block and no control
//! over argv, so configuration is environment variables rather than flags.
//!
//! | Variable | Default | Effect |
//! |---|---|---|
//! | `AGENT_MCP_PROVIDER` | `fake` | `fake`, `claude`, or `codex` |
//! | `AGENT_MCP_VERBOSE` | unset | any value publishes provider-authored text |
//! | `AGENT_MCP_CONCURRENCY` | `2` | turns admitted at once |
//! | `AGENT_MCP_TIMEOUT_SECS` | unset | deadline applied to every turn |
//!
//! The default provider is the fake one on purpose. A server that spends
//! money the first time someone runs it is a bad default, so the real
//! providers are opt-in.
//!
//! # Running it
//!
//! ```text
//! cargo build -p mcp-server-example
//! ```
//!
//! Then point a client at the built binary, for example in an MCP client's
//! server configuration:
//!
//! ```json
//! {
//!   "command": "/path/to/target/debug/agent-mcp",
//!   "env": { "AGENT_MCP_PROVIDER": "claude" }
//! }
//! ```

use std::env;
use std::sync::Arc;
use std::time::Duration;

use tower::ServiceBuilder;
use tower_agent::layer::{
    AdmissionLayer, CatchPanicLayer, DeadlineLayer, SuperviseLayer, ValidateTurnLayer,
};
use tower_agent::{
    AgentError, AgentRequest, BoxTurnService, FakeOptions, FakeService, Turn, TurnOutcome,
};
use tower_agent_claude::{ClaudeOptions, ClaudeService};
use tower_agent_codex::{CodexOptions, CodexService};
use tower_agent_mcp::{
    FixedScope, InMemoryContinuationStore, PlanTool, Projection, ProviderMessages, TurnTool,
};
use tower_agent_plan::RoutedTurnService;
use tower_mcp::McpRouter;

pub const SERVER_NAME: &str = "tower-agent";

/// Which provider the turn tool runs on.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    /// Scripted and effect-free. The default, so a first run costs nothing.
    Fake,
    Claude,
    Codex,
}

impl Provider {
    fn from_env(raw: Option<&str>) -> anyhow::Result<Self> {
        match raw.unwrap_or("fake") {
            "fake" => Ok(Self::Fake),
            "claude" => Ok(Self::Claude),
            "codex" => Ok(Self::Codex),
            other => {
                anyhow::bail!("AGENT_MCP_PROVIDER must be fake, claude, or codex, got {other}")
            }
        }
    }
}

/// Everything the server reads from its environment.
pub struct Settings {
    pub provider: Provider,
    pub messages: ProviderMessages,
    pub concurrency: usize,
    pub timeout: Option<Duration>,
}

impl Settings {
    /// Read settings, failing rather than guessing when a value is malformed.
    pub fn from_env() -> anyhow::Result<Self> {
        let provider = Provider::from_env(env::var("AGENT_MCP_PROVIDER").ok().as_deref())?;

        // Redacted unless a human asked otherwise. Provider error text has
        // been observed carrying session values, so verbosity is a decision
        // rather than a default.
        let messages = if env::var("AGENT_MCP_VERBOSE").is_ok() {
            ProviderMessages::Verbatim
        } else {
            ProviderMessages::Redacted
        };

        let concurrency = match env::var("AGENT_MCP_CONCURRENCY") {
            Ok(raw) => raw
                .parse::<usize>()
                .ok()
                .filter(|value| *value > 0)
                .ok_or_else(|| {
                    anyhow::anyhow!("AGENT_MCP_CONCURRENCY must be a positive integer")
                })?,
            Err(_) => 2,
        };

        let timeout = match env::var("AGENT_MCP_TIMEOUT_SECS") {
            Ok(raw) => Some(Duration::from_secs(raw.parse::<u64>().map_err(|_| {
                anyhow::anyhow!("AGENT_MCP_TIMEOUT_SECS must be a whole number of seconds")
            })?)),
            Err(_) => None,
        };

        Ok(Self {
            provider,
            messages,
            concurrency,
            timeout,
        })
    }
}

/// The policy stack, in the order `docs/resilience.md` specifies.
///
/// Panic normalization outermost so nothing below it can unwind past a
/// receipt. Supervise next, so a client that disconnects mid-turn does not
/// abandon a running provider. Admission inside supervision so its permit is
/// held through cleanup.
///
/// `AuthorityLayer` is deliberately absent. It is generic over options that
/// carry a portable filesystem request, and only `CodexOptions` does; Claude
/// expresses the same concern through its own controls. The host ceiling is
/// therefore set on the provider service rather than as a shared layer, which
/// is what `with_authority_policy` is for.
fn stack<S, O>(
    service: S,
    concurrency: usize,
) -> impl tower::Service<
    AgentRequest<Turn<O>>,
    Response = TurnOutcome,
    Error = AgentError,
    Future: Send + 'static,
> + Clone
+ Send
+ Sync
+ 'static
where
    S: tower::Service<AgentRequest<Turn<O>>, Response = TurnOutcome, Error = AgentError>
        + Clone
        + Send
        + Sync
        + 'static,
    S::Future: Send + 'static,
    O: Send + 'static,
{
    ServiceBuilder::new()
        .layer(CatchPanicLayer::new())
        .layer(SuperviseLayer::new())
        .layer(AdmissionLayer::new(concurrency))
        .layer(DeadlineLayer::new())
        .layer(ValidateTurnLayer::new())
        .service(service)
}

/// The turn tool's service, with provider options defaulted.
///
/// The tool speaks in portable `Turn<()>` bodies. Each provider needs its own
/// options type, so the conversion happens here rather than in the adapter,
/// which is the same split the kernel makes everywhere else: portable body in
/// the middle, provider-specific controls at the edge.
fn turn_service(settings: &Settings) -> BoxTurnService {
    let concurrency = settings.concurrency;
    match settings.provider {
        // FakeService rather than EchoService: it mints and continues session
        // handles, so continuation round-trips end to end without a provider.
        Provider::Fake => BoxTurnService::new(
            ServiceBuilder::new()
                .map_request(|request: AgentRequest<Turn>| {
                    request.map_body(|turn| turn.with_options(FakeOptions::default()))
                })
                .service(stack(FakeService, concurrency)),
        ),
        Provider::Claude => BoxTurnService::new(
            ServiceBuilder::new()
                .map_request(|request: AgentRequest<Turn>| {
                    request.map_body(|turn| turn.with_options(ClaudeOptions::default()))
                })
                .service(stack(ClaudeService::new(), concurrency)),
        ),
        Provider::Codex => BoxTurnService::new(
            ServiceBuilder::new()
                .map_request(|request: AgentRequest<Turn>| {
                    request.map_body(|turn| turn.with_options(CodexOptions::default()))
                })
                .service(stack(CodexService::new(), concurrency)),
        ),
    }
}

/// Build the router. Separated from serving so a test can drive it over pipes.
pub fn router(settings: &Settings) -> McpRouter {
    let store = Arc::new(InMemoryContinuationStore::new());
    let projection = Projection::new().with_provider_messages(settings.messages);

    // stdio carries exactly one client for the life of the process, so one
    // fixed scope is correct here. Over HTTP it would not be: every caller
    // would share a scope, and the scope check would then let any of them
    // continue any other's conversation.
    let scopes = Arc::new(FixedScope::stdio());

    let mut turn = TurnTool::new(turn_service(settings), store.clone(), scopes.clone())
        .with_projection(projection)
        .with_description("Run one finite agent turn, optionally continuing an earlier one");
    if let Some(timeout) = settings.timeout {
        turn = turn.with_turn_timeout(timeout);
    }
    let turn = turn.build();

    // The planning tool runs compiled turns, so it needs the routed service
    // rather than a single provider. It asks the client for whatever a turn
    // is still missing, which is why the transport has to be bidirectional.
    let planned = PlanTool::new(
        RoutedTurnService::new()
            .with_claude(stack(ClaudeService::new(), settings.concurrency))
            .with_codex(stack(CodexService::new(), settings.concurrency)),
        store,
        scopes,
    )
    .with_projection(projection);
    let planned = match settings.timeout {
        Some(timeout) => planned.with_turn_timeout(timeout),
        None => planned,
    }
    .build();

    McpRouter::new()
        .server_info(SERVER_NAME, env!("CARGO_PKG_VERSION"))
        .tool(turn)
        .tool(planned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_provider_costs_nothing_to_run() {
        // A server that spends money on its first run is a bad default.
        assert!(matches!(Provider::from_env(None), Ok(Provider::Fake)));
    }

    #[test]
    fn a_malformed_provider_fails_rather_than_falling_back() {
        // Falling back to the fake would look like a working server that
        // silently never calls the provider the operator asked for.
        assert!(Provider::from_env(Some("gemini")).is_err());
    }

    #[test]
    fn every_provider_name_is_accepted() {
        for name in ["fake", "claude", "codex"] {
            assert!(Provider::from_env(Some(name)).is_ok(), "{name}");
        }
    }
}
