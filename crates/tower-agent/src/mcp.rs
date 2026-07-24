//! The MCP projection: expose the server as an MCP surface.
//!
//! The core tool is [`prompt`](router): it runs a [`Call`] through the backend,
//! resolving defaults from config. [`agents`](router) lists the configured
//! agents. Sessions, broadcast, and feed are added as their enhancements land.

use std::sync::Arc;

use schemars::JsonSchema;
use serde::Serialize;
use tower_mcp::extract::{Json, State};
use tower_mcp::{CallToolResult, Error, McpRouter, NoParams, Tool, ToolBuilder};

use crate::backend::{Backend, Outcome};
use crate::config::Config;
use crate::params::Call;

/// The server: a config and a backend. Cheap to clone (both are shared).
#[derive(Clone)]
pub struct Server {
    config: Arc<Config>,
    backend: Arc<dyn Backend>,
}

impl Server {
    pub fn new(config: Config, backend: Arc<dyn Backend>) -> Self {
        Server {
            config: Arc::new(config),
            backend,
        }
    }

    /// The configured agent names.
    pub fn agent_names(&self) -> Vec<String> {
        self.config.agent_names().cloned().collect()
    }

    /// Resolve a call against config and run it through the backend. This is the
    /// atom; the MCP `prompt` tool is a thin wrapper over it.
    pub async fn run(&self, call: Call) -> Result<Outcome, crate::backend::BackendError> {
        let params = self.config.resolve(call);
        self.backend.run(&params).await
    }

    /// Build the MCP router that exposes this server.
    pub fn router(self) -> McpRouter {
        router(self)
    }
}

/// An agent as seen over the wire.
#[derive(Serialize, JsonSchema)]
struct AgentInfo {
    name: String,
    model: Option<String>,
    has_system: bool,
}

/// Build the router over a server.
pub fn router(server: Server) -> McpRouter {
    McpRouter::new()
        .server_info("tower-agent", env!("CARGO_PKG_VERSION"))
        .instructions(
            "An agent server. The `prompt` tool runs a prompt through the backend; it requires \
             a `prompt`, `agent` selects a configured profile of defaults, and any other \
             backend parameter is optional. `agents` lists the configured agents.",
        )
        .tool(prompt_tool(server.clone()))
        .tool(agents_tool(server))
}

fn prompt_tool(server: Server) -> Tool {
    ToolBuilder::new("prompt")
        .description(
            "Run a prompt. Requires `prompt`; `agent` selects a profile of defaults; any other \
             backend parameter (system, model, effort, allowed_tools, cwd, session) is optional",
        )
        .extractor_handler(
            server,
            |State(server): State<Server>, Json(call): Json<Call>| async move {
                match server.run(call).await {
                    Ok(outcome) => CallToolResult::from_serialize(&outcome),
                    Err(e) => Err(Error::tool(e.to_string())),
                }
            },
        )
        .build()
}

fn agents_tool(server: Server) -> Tool {
    ToolBuilder::new("agents")
        .description("List the configured agents and their defaults")
        .extractor_handler(
            server,
            |State(server): State<Server>, Json(_): Json<NoParams>| async move {
                let agents: Vec<AgentInfo> = server
                    .config
                    .agents
                    .iter()
                    .map(|(name, a)| AgentInfo {
                        name: name.clone(),
                        model: a.model.clone(),
                        has_system: a.system.is_some(),
                    })
                    .collect();
                CallToolResult::from_serialize(&agents)
            },
        )
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::StubBackend;
    use serde_json::{Value, json};
    use tower_mcp::testing::TestClient;

    fn server() -> Server {
        let config = Config::parse(
            r#"
            [defaults]
            model = "sonnet"
            effort = "medium"
            cwd = "/repo"

            [agents.tester]
            system = "You run the tests."
            model = "haiku"
            allowed_tools = ["Bash(cargo test:*)"]
            "#,
        )
        .unwrap();
        Server::new(config, Arc::new(StubBackend))
    }

    async fn client() -> TestClient {
        let mut client = TestClient::from_router(server().router());
        client.initialize().await;
        client
    }

    /// Run the `prompt` tool and return the resolved [`Params`] the stub echoed.
    async fn resolved(client: &mut TestClient, args: Value) -> Value {
        let outcome: Value = client.call_tool_typed("prompt", args).await;
        let text = outcome["text"].as_str().expect("outcome.text");
        serde_json::from_str(text).expect("stub echoes resolved params as json")
    }

    #[tokio::test]
    async fn agents_lists_configured() {
        let agents: Vec<Value> = client().await.call_tool_typed("agents", json!({})).await;
        assert_eq!(agents[0]["name"], "tester");
        assert_eq!(agents[0]["model"], "haiku");
        assert_eq!(agents[0]["has_system"], true);
    }

    #[tokio::test]
    async fn agent_defaults_fill_the_call() {
        let mut c = client().await;
        let p = resolved(&mut c, json!({ "agent": "tester", "prompt": "go" })).await;
        assert_eq!(p["prompt"], "go");
        assert_eq!(p["system"], "You run the tests.");
        assert_eq!(p["model"], "haiku"); // agent beats the sonnet default
        assert_eq!(p["effort"], "medium"); // falls through to the default
        assert_eq!(p["cwd"], "/repo"); // from the default
        assert_eq!(p["allowed_tools"][0], "Bash(cargo test:*)");
    }

    #[tokio::test]
    async fn call_overrides_every_field() {
        let mut c = client().await;
        let p = resolved(
            &mut c,
            json!({
                "agent": "tester",
                "prompt": "go",
                "system": "custom",
                "model": "opus",
                "effort": "high",
                "cwd": "/elsewhere",
                "allowed_tools": ["Read"],
                "session": "s-42"
            }),
        )
        .await;
        assert_eq!(p["system"], "custom");
        assert_eq!(p["model"], "opus");
        assert_eq!(p["effort"], "high");
        assert_eq!(p["cwd"], "/elsewhere");
        assert_eq!(p["allowed_tools"][0], "Read");
        assert_eq!(p["session"], "s-42");
    }

    #[tokio::test]
    async fn no_agent_uses_server_defaults_only() {
        let mut c = client().await;
        let p = resolved(&mut c, json!({ "prompt": "go" })).await;
        assert_eq!(p["model"], "sonnet");
        assert_eq!(p["effort"], "medium");
        assert!(p["system"].is_null());
        assert!(p["allowed_tools"].is_null());
    }

    #[tokio::test]
    async fn unknown_agent_falls_back_to_defaults() {
        let mut c = client().await;
        let p = resolved(&mut c, json!({ "agent": "nope", "prompt": "go" })).await;
        assert_eq!(p["model"], "sonnet");
        assert!(p["system"].is_null());
    }

    #[tokio::test]
    async fn session_round_trips_through_outcome() {
        let mut c = client().await;
        let outcome: Value = c
            .call_tool_typed("prompt", json!({ "prompt": "go", "session": "s-7" }))
            .await;
        assert_eq!(outcome["session"], "s-7");
    }

    #[tokio::test]
    async fn prompt_is_required() {
        let mut c = client().await;
        // Missing the required `prompt` field must be a tool error, not a run.
        let _err: Value = c
            .call_tool_expect_error("prompt", json!({ "agent": "tester" }))
            .await;
    }
}
