//! The MCP projection: expose the server as an MCP surface.
//!
//! The core tool is [`prompt`](router): it runs a [`Call`] through the backend,
//! resolving defaults from config. [`agents`](router) lists the configured
//! agents. Sessions, broadcast, and feed are added as their enhancements land.

use std::sync::Arc;

use schemars::JsonSchema;
use serde::Serialize;
use tokio::sync::mpsc::UnboundedSender;
use tower_mcp::extract::{Context, Json, State};
use tower_mcp::{CallToolResult, Error, McpRouter, NoParams, TaskSupportMode, Tool, ToolBuilder};

use crate::backend::{Backend, Event, Outcome};
use crate::config::Config;
use crate::error::RunError;
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

    /// Validate a call: a named agent must exist, and the prompt must not be
    /// empty.
    fn validate(&self, call: &Call) -> Result<(), RunError> {
        if call.prompt.trim().is_empty() {
            return Err(RunError::EmptyPrompt);
        }
        if let Some(agent) = &call.agent
            && !self.config.has_agent(agent)
        {
            return Err(RunError::UnknownAgent(agent.clone()));
        }
        Ok(())
    }

    /// Resolve a call against config and run it through the backend. This is the
    /// atom; the MCP `prompt` tool is a thin wrapper over it.
    pub async fn run(&self, call: Call) -> Result<Outcome, RunError> {
        self.validate(&call)?;
        let params = self.config.resolve(call);
        Ok(self.backend.run(&params).await?)
    }

    /// Like [`Server::run`], but emitting incremental [`Event`]s to `events` as
    /// the backend produces them. The caller opts into streaming; a backend that
    /// does not stream simply sends nothing and returns the outcome.
    pub async fn run_streaming(
        &self,
        call: Call,
        events: UnboundedSender<Event>,
    ) -> Result<Outcome, RunError> {
        self.validate(&call)?;
        let params = self.config.resolve(call);
        Ok(self.backend.run_streaming(&params, events).await?)
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
             backend parameter (system, model, effort, allowed_tools, cwd, session) is optional. \
             May be called as a task for long runs, and streams incremental output when the call \
             carries a progress token",
        )
        // The prompt is long-running: let a client run it as a task (returning a
        // task id to poll, wait on, or cancel) instead of blocking. Optional, so
        // a simple client can still call it synchronously.
        .task_support(TaskSupportMode::Optional)
        .extractor_handler(
            server,
            |State(server): State<Server>, ctx: Context, Json(call): Json<Call>| async move {
                // Stream only when the caller opted in with a progress token;
                // otherwise take the cheaper non-streaming path.
                if ctx.progress_token().is_some() {
                    run_streamed(server, ctx, call).await
                } else {
                    match server.run(call).await {
                        Ok(outcome) => CallToolResult::from_serialize(&outcome),
                        Err(e) => Err(Error::tool(e.to_string())),
                    }
                }
            },
        )
        .build()
}

/// Run a call with streaming: forward each backend [`Event`] to the caller as a
/// progress notification, then return the final outcome. The backend runs on its
/// own task so events can be drained as they arrive.
async fn run_streamed(server: Server, ctx: Context, call: Call) -> Result<CallToolResult, Error> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    let handle = tokio::spawn(async move { server.run_streaming(call, tx).await });

    let mut n = 0.0f64;
    while let Some(event) = rx.recv().await {
        n += 1.0;
        let message = match event {
            Event::TextDelta(t) => t,
            Event::Status(s) => s,
        };
        ctx.report_progress(n, None, Some(&message)).await;
    }

    match handle.await {
        Ok(Ok(outcome)) => CallToolResult::from_serialize(&outcome),
        Ok(Err(e)) => Err(Error::tool(e.to_string())),
        Err(e) => Err(Error::tool(format!("run task failed: {e}"))),
    }
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
    use tower_mcp::ServerNotification;
    use tower_mcp::testing::TestClient;

    /// A backend that streams a few events then finishes, for exercising the
    /// streaming path deterministically.
    struct StreamBackend;

    #[async_trait::async_trait]
    impl Backend for StreamBackend {
        async fn run(&self, _params: &crate::Params) -> Result<Outcome, crate::BackendError> {
            Ok(Outcome {
                text: "hello".into(),
                session: Some("s1".into()),
            })
        }
        async fn run_streaming(
            &self,
            _params: &crate::Params,
            events: tokio::sync::mpsc::UnboundedSender<Event>,
        ) -> Result<Outcome, crate::BackendError> {
            let _ = events.send(Event::Status("starting".into()));
            let _ = events.send(Event::TextDelta("hel".into()));
            let _ = events.send(Event::TextDelta("lo".into()));
            Ok(Outcome {
                text: "hello".into(),
                session: Some("s1".into()),
            })
        }
    }

    fn server() -> Server {
        let config = Config::parse(
            r#"
            [defaults]
            model = "sonnet"
            effort = "medium"
            cwd = "/repo"
            max_turns = 10

            [agents.tester]
            system = "You run the tests."
            model = "haiku"
            allowed_tools = ["Bash(cargo test:*)"]
            disallowed_tools = ["Bash(rm:*)"]
            add_dirs = ["/shared"]
            max_turns = 3
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
        assert_eq!(p["disallowed_tools"][0], "Bash(rm:*)");
        assert_eq!(p["add_dirs"][0], "/shared");
        assert_eq!(p["max_turns"], 3); // agent beats the default of 10
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
                "append_system": "and be brief",
                "model": "opus",
                "effort": "high",
                "cwd": "/elsewhere",
                "allowed_tools": ["Read"],
                "disallowed_tools": ["Write"],
                "add_dirs": ["/tmp"],
                "max_turns": 1,
                "timeout": 42,
                "session": "s-42"
            }),
        )
        .await;
        assert_eq!(p["system"], "custom");
        assert_eq!(p["append_system"], "and be brief");
        assert_eq!(p["model"], "opus");
        assert_eq!(p["effort"], "high");
        assert_eq!(p["cwd"], "/elsewhere");
        assert_eq!(p["allowed_tools"][0], "Read");
        assert_eq!(p["disallowed_tools"][0], "Write");
        assert_eq!(p["add_dirs"][0], "/tmp");
        assert_eq!(p["max_turns"], 1);
        assert_eq!(p["timeout"], 42);
        assert_eq!(p["session"], "s-42");
    }

    #[tokio::test]
    async fn no_agent_uses_server_defaults_only() {
        let mut c = client().await;
        let p = resolved(&mut c, json!({ "prompt": "go" })).await;
        assert_eq!(p["model"], "sonnet");
        assert_eq!(p["effort"], "medium");
        assert_eq!(p["max_turns"], 10);
        assert!(p["system"].is_null());
        assert!(p["allowed_tools"].is_null());
    }

    #[tokio::test]
    async fn unknown_agent_is_an_error() {
        let mut c = client().await;
        // A named agent that does not exist is a request error, not a silent run
        // with server defaults.
        let err: Value = c
            .call_tool_expect_error("prompt", json!({ "agent": "nope", "prompt": "go" }))
            .await;
        assert!(
            err.to_string().contains("nope"),
            "error should name the agent: {err}"
        );
    }

    #[tokio::test]
    async fn empty_prompt_is_an_error() {
        let mut c = client().await;
        let _err: Value = c
            .call_tool_expect_error("prompt", json!({ "prompt": "   " }))
            .await;
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

    #[tokio::test]
    async fn run_streaming_forwards_events_and_returns_outcome() {
        let server = Server::new(Config::default(), Arc::new(StreamBackend));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let outcome = server
            .run_streaming(
                Call {
                    prompt: "go".into(),
                    ..Default::default()
                },
                tx,
            )
            .await
            .unwrap();
        assert_eq!(outcome.text, "hello");
        let mut got = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            got.push(ev);
        }
        assert_eq!(got.len(), 3, "three events streamed");
    }

    #[tokio::test]
    async fn stub_streaming_sends_no_events_but_returns_outcome() {
        // The default streaming impl does not stream: no events, but the outcome
        // still comes back.
        let server = Server::new(Config::default(), Arc::new(StubBackend));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let outcome = server
            .run_streaming(
                Call {
                    prompt: "go".into(),
                    ..Default::default()
                },
                tx,
            )
            .await
            .unwrap();
        assert!(rx.try_recv().is_err(), "stub streams nothing");
        assert!(outcome.text.contains("go"));
    }

    #[tokio::test]
    async fn prompt_tool_advertises_task_support() {
        let mut c = client().await;
        let tools = c.list_tools().await;
        let prompt = tools.iter().find(|t| t["name"] == "prompt").unwrap();
        // Optional: a client MAY run it as a task, or call it synchronously.
        assert_eq!(prompt["execution"]["taskSupport"], "optional");
    }

    #[tokio::test]
    async fn prompt_can_run_as_a_task() {
        let mut c = client().await;
        // A task-augmented call returns a task handle immediately (async), not
        // the inline result. This is the long-running execution model.
        let created = c
            .send_request(
                "tools/call",
                Some(json!({ "name": "prompt", "arguments": { "prompt": "go" }, "task": {} })),
            )
            .await;
        assert_eq!(created["resultType"], "task", "resp: {created}");
        assert!(
            created["task"]["taskId"].as_str().is_some(),
            "task-augmented call should return a task handle: {created}"
        );
    }

    #[tokio::test]
    async fn streaming_emits_progress_only_with_a_token() {
        // With a progress token, deltas arrive as progress notifications.
        let mut client = TestClient::from_router(
            Server::new(Config::default(), Arc::new(StreamBackend)).router(),
        );
        client.initialize().await;
        let _ = client
            .send_request(
                "tools/call",
                Some(json!({
                    "name": "prompt",
                    "arguments": { "prompt": "go" },
                    "_meta": { "progressToken": "p1" }
                })),
            )
            .await;
        let messages: Vec<String> = client
            .drain_notifications()
            .into_iter()
            .filter_map(|n| match n {
                ServerNotification::Progress(p) => p.message,
                _ => None,
            })
            .collect();
        assert_eq!(messages, vec!["starting", "hel", "lo"]);

        // Without a token, no progress notifications are sent.
        let mut plain = TestClient::from_router(
            Server::new(Config::default(), Arc::new(StreamBackend)).router(),
        );
        plain.initialize().await;
        let _ = plain
            .send_request(
                "tools/call",
                Some(json!({ "name": "prompt", "arguments": { "prompt": "go" } })),
            )
            .await;
        let any_progress = plain
            .drain_notifications()
            .into_iter()
            .any(|n| matches!(n, ServerNotification::Progress(_)));
        assert!(!any_progress, "no token means no progress");
    }
}
