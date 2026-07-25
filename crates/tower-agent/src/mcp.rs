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
use crate::budget::Budget;
use crate::bus::Bus;
use crate::config::Config;
use crate::error::RunError;
use crate::params::{Call, Params};
use crate::run::{RunKind, RunStatus, Runs};
use crate::session::{MemorySessionStore, SessionStore};

/// The server: a config, a backend, a session store, a bus, a run registry, and a
/// budget. Cheap to clone (all are shared).
#[derive(Clone)]
pub struct Server {
    pub(crate) config: Arc<Config>,
    pub(crate) backend: Arc<dyn Backend>,
    pub(crate) sessions: Arc<dyn SessionStore>,
    pub(crate) bus: Bus,
    pub(crate) runs: Arc<Runs>,
    pub(crate) budget: Arc<Budget>,
    /// The canonical allowed root for client-provided paths, if containment is on.
    pub(crate) root: Option<std::path::PathBuf>,
}

impl Server {
    /// A server with an in-memory session store.
    pub fn new(config: Config, backend: Arc<dyn Backend>) -> Self {
        Self::with_sessions(config, backend, Arc::new(MemorySessionStore::new()))
    }

    /// A server with a specific session store (e.g. a file-backed one).
    pub fn with_sessions(
        config: Config,
        backend: Arc<dyn Backend>,
        sessions: Arc<dyn SessionStore>,
    ) -> Self {
        let budget = Arc::new(Budget::new(config.budget.max_usd));
        let root = config
            .security
            .root
            .as_ref()
            .map(|r| std::fs::canonicalize(r).unwrap_or_else(|_| std::path::PathBuf::from(r)));
        Server {
            config: Arc::new(config),
            backend,
            sessions,
            bus: Bus::new(),
            runs: Arc::new(Runs::new()),
            budget,
            root,
        }
    }

    /// The configured agent names.
    pub fn agent_names(&self) -> Vec<String> {
        self.config.agent_names().cloned().collect()
    }

    /// Validate a call: a named agent must exist, the prompt must not be empty,
    /// and a named session must exist.
    fn validate(&self, call: &Call) -> Result<(), RunError> {
        if call.prompt.trim().is_empty() {
            return Err(RunError::EmptyPrompt);
        }
        if let Some(agent) = &call.agent
            && !self.config.has_agent(agent)
        {
            return Err(RunError::UnknownAgent(agent.clone()));
        }
        if let Some(session) = &call.session
            && !self.sessions.exists(session)
        {
            return Err(RunError::UnknownSession(session.clone()));
        }
        // Contain client-provided paths to the allowed root, if one is set.
        if let Some(root) = &self.root {
            if let Some(cwd) = &call.cwd {
                contained(cwd, root)?;
            }
            for dir in call.add_dirs.iter().flatten() {
                contained(dir, root)?;
            }
        }
        Ok(())
    }

    /// Validate, pick the session id (mint a fresh one, or reuse the named one),
    /// and resolve the params with the stored backend resume token. `structured`
    /// asks the backend for a `{summary, reply, posts}` result.
    fn prepare(
        &self,
        call: Call,
        structured: bool,
    ) -> Result<(String, Option<String>, Params), RunError> {
        self.validate(&call)?;
        let id = match &call.session {
            Some(id) => id.clone(),
            None => self.sessions.mint(),
        };
        let agent = call.agent.clone();
        let resume = self.sessions.backend_token(&id);
        let mut params = self.config.resolve(call);
        // The backend resumes with its own token; our session id is never handed
        // to it. A fresh session has no token, so it starts clean.
        params.session = resume;
        params.structured = structured;
        Ok((id, agent, params))
    }

    /// Record the completed turn and return the outcome carrying our session id.
    fn finish(&self, id: String, agent: Option<String>, mut outcome: Outcome) -> Outcome {
        let backend_token = outcome.session.take();
        let last = preview(&outcome.reply);
        self.sessions.record_turn(&id, agent, backend_token, last);
        outcome.session = Some(id);
        outcome
    }

    /// Resolve a call against config and run it through the backend, recording a
    /// run of the given kind. The shared path behind `run`, `run_structured`, and
    /// `tick`.
    async fn run_with(
        &self,
        call: Call,
        structured: bool,
        kind: RunKind,
    ) -> Result<Outcome, RunError> {
        if self.budget.exhausted() {
            return Err(RunError::BudgetExceeded);
        }
        let (id, agent, params) = self.prepare(call, structured)?;
        let run_id = self.runs.start(agent.clone(), Some(id.clone()), kind);
        let result = self.backend.run(&params).await;
        self.account(&run_id, &result);
        Ok(self.finish(id, agent, result?))
    }

    /// Record a finished run's cost and status: bump the budget and the run
    /// registry.
    fn account(&self, run_id: &str, result: &Result<Outcome, crate::BackendError>) {
        match result {
            Ok(o) => {
                if let Some(cost) = o.cost_usd {
                    self.budget.record(cost);
                }
                self.runs
                    .finish(run_id, RunStatus::Done, Some(o.summary.clone()), o.cost_usd);
            }
            Err(e) => self
                .runs
                .finish(run_id, RunStatus::Failed, Some(e.to_string()), None),
        }
    }

    /// Run a call through the backend. This is the atom; the MCP `prompt` tool is
    /// a thin wrapper over it. A fresh call mints a session and returns its id;
    /// passing that id back continues the thread.
    pub async fn run(&self, call: Call) -> Result<Outcome, RunError> {
        self.run_with(call, false, RunKind::Invoke).await
    }

    /// Like [`Server::run`], but asking the backend for a structured result so
    /// the agent can emit `posts`. Used by the bus for a fired turn.
    pub(crate) async fn run_structured(&self, call: Call) -> Result<Outcome, RunError> {
        self.run_with(call, true, RunKind::Subscribe).await
    }

    /// Like [`Server::run`], but emitting incremental [`Event`]s to `events` as
    /// the backend produces them. The caller opts into streaming; a backend that
    /// does not stream simply sends nothing and returns the outcome.
    pub async fn run_streaming(
        &self,
        call: Call,
        events: UnboundedSender<Event>,
    ) -> Result<Outcome, RunError> {
        if self.budget.exhausted() {
            return Err(RunError::BudgetExceeded);
        }
        let (id, agent, params) = self.prepare(call, false)?;
        let run_id = self
            .runs
            .start(agent.clone(), Some(id.clone()), RunKind::Invoke);
        let result = self.backend.run_streaming(&params, events).await;
        self.account(&run_id, &result);
        Ok(self.finish(id, agent, result?))
    }

    /// The session registry: all threads, or one by id.
    pub fn sessions(&self) -> Arc<dyn SessionStore> {
        self.sessions.clone()
    }

    /// The run registry: recent runs, or one by id.
    pub fn runs(&self) -> Arc<Runs> {
        self.runs.clone()
    }

    /// The server budget: cumulative spend and the cap.
    pub fn budget(&self) -> Arc<Budget> {
        self.budget.clone()
    }

    /// The agents that carry a schedule.
    pub fn scheduled_agents(&self) -> Vec<crate::config::ScheduledAgent> {
        self.config.scheduled_agents()
    }

    /// Fire an agent's scheduled prompt once, optionally continuing a session.
    /// This is what the scheduler runs on each tick, and what `agent tick` calls.
    pub async fn tick(&self, agent: &str, session: Option<String>) -> Result<Outcome, RunError> {
        let call = Call {
            prompt: self.config.tick_prompt(agent),
            agent: Some(agent.to_string()),
            session,
            ..Default::default()
        };
        self.run_with(call, false, RunKind::Schedule).await
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
             backend parameter is optional. It returns a `session` id; pass it back to continue \
             the thread. `agents` lists the configured agents; `sessions` lists the threads.",
        )
        .tool(prompt_tool(server.clone()))
        .tool(agents_tool(server.clone()))
        .tool(sessions_tool(server.clone()))
        .tool(runs_tool(server.clone()))
        .tool(broadcast_tool(server.clone()))
        .tool(feed_tool(server))
}

#[derive(serde::Deserialize, JsonSchema)]
struct RunsInput {
    /// One run by id; omit to list recent runs.
    #[serde(default)]
    id: Option<String>,
    /// How many recent runs (default 50).
    #[serde(default)]
    limit: Option<usize>,
}

fn runs_tool(server: Server) -> Tool {
    ToolBuilder::new("runs")
        .description("Recent runs (invoke, schedule, subscribe), or one by id")
        .extractor_handler(
            server,
            |State(server): State<Server>, Json(input): Json<RunsInput>| async move {
                match input.id {
                    Some(id) => CallToolResult::from_serialize(&server.runs.get(&id)),
                    None => {
                        CallToolResult::from_serialize(&server.runs.list(input.limit.unwrap_or(50)))
                    }
                }
            },
        )
        .build()
}

#[derive(serde::Deserialize, JsonSchema)]
struct BroadcastInput {
    channel: String,
    body: String,
    /// Address one agent directly, so it reacts even if not subscribed.
    #[serde(default)]
    to: Option<String>,
}

fn broadcast_tool(server: Server) -> Tool {
    ToolBuilder::new("broadcast")
        .description("Post a message to a channel; subscribed agents react, plus a directed `to`")
        .extractor_handler(
            server,
            |State(server): State<Server>, Json(input): Json<BroadcastInput>| async move {
                let msg =
                    server.broadcast(&input.channel, "operator", input.to.as_deref(), &input.body);
                CallToolResult::from_serialize(&msg)
            },
        )
        .build()
}

#[derive(serde::Deserialize, JsonSchema)]
struct FeedInput {
    /// Only this channel, if given.
    #[serde(default)]
    channel: Option<String>,
    /// How many recent messages (default 50).
    #[serde(default)]
    limit: Option<usize>,
}

fn feed_tool(server: Server) -> Tool {
    ToolBuilder::new("feed")
        .description("Recent messages on the bus (agent-to-agent traffic), newest last")
        .extractor_handler(
            server,
            |State(server): State<Server>, Json(input): Json<FeedInput>| async move {
                let messages = server.feed(input.channel.as_deref(), input.limit.unwrap_or(50));
                CallToolResult::from_serialize(&messages)
            },
        )
        .build()
}

/// A client-provided path must canonicalize to inside `root`, or it is rejected.
/// Canonicalization resolves `..` and symlinks and requires the path to exist.
fn contained(path: &str, root: &std::path::Path) -> Result<(), RunError> {
    let canon =
        std::fs::canonicalize(path).map_err(|_| RunError::PathNotAllowed(path.to_string()))?;
    if canon.starts_with(root) {
        Ok(())
    } else {
        Err(RunError::PathNotAllowed(path.to_string()))
    }
}

/// A one-line, length-capped preview of an outcome, for the session registry.
fn preview(text: &str) -> Option<String> {
    let line = text.trim().lines().next().unwrap_or("").trim();
    if line.is_empty() {
        return None;
    }
    Some(line.chars().take(120).collect())
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
            Event::Thinking(t) => format!("[thinking] {t}"),
            Event::ToolUse { name } => format!("[tool] {name}"),
            Event::Turn { n } => format!("[turn {n}]"),
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

#[derive(serde::Deserialize, JsonSchema)]
struct SessionsInput {
    /// One session by id; omit to list them all.
    #[serde(default)]
    id: Option<String>,
}

fn sessions_tool(server: Server) -> Tool {
    ToolBuilder::new("sessions")
        .description("List sessions (threads), or one by id")
        .extractor_handler(
            server,
            |State(server): State<Server>, Json(input): Json<SessionsInput>| async move {
                match input.id {
                    Some(id) => CallToolResult::from_serialize(&server.sessions.get(&id)),
                    None => CallToolResult::from_serialize(&server.sessions.list()),
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
    use tower_mcp::ServerNotification;
    use tower_mcp::testing::TestClient;

    /// A backend that streams a few events then finishes, for exercising the
    /// streaming path deterministically.
    struct StreamBackend;

    #[async_trait::async_trait]
    impl Backend for StreamBackend {
        async fn run(&self, _params: &crate::Params) -> Result<Outcome, crate::BackendError> {
            Ok(Outcome::from_reply("hello", Some("s1".into())))
        }
        async fn run_streaming(
            &self,
            _params: &crate::Params,
            events: tokio::sync::mpsc::UnboundedSender<Event>,
        ) -> Result<Outcome, crate::BackendError> {
            let _ = events.send(Event::Status("starting".into()));
            let _ = events.send(Event::TextDelta("hel".into()));
            let _ = events.send(Event::TextDelta("lo".into()));
            Ok(Outcome::from_reply("hello", Some("s1".into())))
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
        let reply = outcome["reply"].as_str().expect("outcome.reply");
        serde_json::from_str(reply).expect("stub echoes resolved params as json")
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
                "timeout": 42
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
    async fn a_fresh_call_mints_a_session_that_can_continue() {
        let mut c = client().await;
        // Fresh call: a session id is minted and returned.
        let first: Value = c.call_tool_typed("prompt", json!({ "prompt": "go" })).await;
        let id = first["session"].as_str().expect("a minted session id");
        assert_eq!(id, "s1");

        // The registry lists it with one turn.
        let sessions: Vec<Value> = c.call_tool_typed("sessions", json!({})).await;
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0]["id"], "s1");
        assert_eq!(sessions[0]["turns"], 1);

        // Continue it: same id back, turn count bumps.
        let second: Value = c
            .call_tool_typed("prompt", json!({ "prompt": "again", "session": id }))
            .await;
        assert_eq!(second["session"], "s1");
        let one: Value = c.call_tool_typed("sessions", json!({ "id": "s1" })).await;
        assert_eq!(one["turns"], 2);
    }

    #[tokio::test]
    async fn unknown_session_is_an_error() {
        let mut c = client().await;
        let _err: Value = c
            .call_tool_expect_error("prompt", json!({ "prompt": "go", "session": "nope" }))
            .await;
    }

    #[tokio::test]
    async fn sessions_get_unknown_is_null() {
        let mut c = client().await;
        let got: Value = c.call_tool_typed("sessions", json!({ "id": "nope" })).await;
        assert!(got.is_null());
    }

    #[tokio::test]
    async fn resume_passes_the_stored_backend_token() {
        // A backend that reports its own token and records what it was resumed
        // with, so we can prove the server threads the stored token through.
        use std::sync::Mutex;
        #[derive(Default)]
        struct Recorder {
            seen: Mutex<Vec<Option<String>>>,
        }
        #[async_trait::async_trait]
        impl Backend for Recorder {
            async fn run(&self, params: &crate::Params) -> Result<Outcome, crate::BackendError> {
                self.seen.lock().unwrap().push(params.session.clone());
                let n = self.seen.lock().unwrap().len();
                Ok(Outcome::from_reply("ok", Some(format!("bk-{n}"))))
            }
        }

        let backend = Arc::new(Recorder::default());
        let server = Server::new(Config::default(), backend.clone());

        // Turn 1: no session in, backend sees no resume token, returns bk-1.
        let out1 = server
            .run(Call {
                prompt: "one".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        let id = out1.session.unwrap();

        // Turn 2: resume, the backend must see bk-1 (stored from turn 1).
        server
            .run(Call {
                prompt: "two".into(),
                session: Some(id),
                ..Default::default()
            })
            .await
            .unwrap();

        let seen = backend.seen.lock().unwrap().clone();
        assert_eq!(seen, vec![None, Some("bk-1".to_string())]);
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
    async fn plain_prompt_returns_reply_and_summary_without_posts() {
        let mut c = client().await;
        let outcome: Value = c.call_tool_typed("prompt", json!({ "prompt": "go" })).await;
        // The stub's reply is the resolved params JSON; the prompt is in there.
        assert!(outcome["reply"].as_str().unwrap().contains("go"));
        assert!(!outcome["summary"].as_str().unwrap().is_empty());
        assert!(outcome["posts"].is_null(), "posts omitted when empty");
    }

    #[tokio::test]
    async fn posts_pass_through_from_the_backend() {
        struct Poster;
        #[async_trait::async_trait]
        impl Backend for Poster {
            async fn run(&self, _p: &crate::Params) -> Result<Outcome, crate::BackendError> {
                Ok(Outcome {
                    summary: "posted".into(),
                    reply: "done".into(),
                    posts: vec![crate::Post {
                        channel: "board".into(),
                        body: "hi".into(),
                        to: Some("scout".into()),
                        reply_to: Some(3),
                    }],
                    session: None,
                    cost_usd: None,
                })
            }
        }

        let mut client =
            TestClient::from_router(Server::new(Config::default(), Arc::new(Poster)).router());
        client.initialize().await;
        let outcome: Value = client
            .call_tool_typed("prompt", json!({ "prompt": "go" }))
            .await;
        assert_eq!(outcome["reply"], "done");
        assert_eq!(outcome["summary"], "posted");
        assert_eq!(outcome["posts"][0]["channel"], "board");
        assert_eq!(outcome["posts"][0]["to"], "scout");
        assert_eq!(outcome["posts"][0]["reply_to"], 3);
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
        assert_eq!(outcome.reply, "hello");
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
        assert!(outcome.reply.contains("go"));
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
    async fn streaming_forwards_rich_events_as_progress() {
        struct Rich;
        #[async_trait::async_trait]
        impl Backend for Rich {
            async fn run(&self, _p: &crate::Params) -> Result<Outcome, crate::BackendError> {
                Ok(Outcome::from_reply("done", None))
            }
            async fn run_streaming(
                &self,
                _p: &crate::Params,
                events: tokio::sync::mpsc::UnboundedSender<Event>,
            ) -> Result<Outcome, crate::BackendError> {
                let _ = events.send(Event::Turn { n: 1 });
                let _ = events.send(Event::ToolUse {
                    name: "Bash".into(),
                });
                let _ = events.send(Event::Thinking("hmm".into()));
                let _ = events.send(Event::TextDelta("hi".into()));
                Ok(Outcome::from_reply("done", None))
            }
        }

        let mut client =
            TestClient::from_router(Server::new(Config::default(), Arc::new(Rich)).router());
        client.initialize().await;
        let _ = client
            .send_request(
                "tools/call",
                Some(json!({
                    "name": "prompt",
                    "arguments": { "prompt": "go" },
                    "_meta": { "progressToken": "p" }
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
        assert_eq!(
            messages,
            vec!["[turn 1]", "[tool] Bash", "[thinking] hmm", "hi"]
        );
    }

    #[tokio::test]
    async fn a_prompt_records_a_run() {
        let mut c = client().await;
        let _: Value = c.call_tool_typed("prompt", json!({ "prompt": "go" })).await;
        let runs: Vec<Value> = c.call_tool_typed("runs", json!({})).await;
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0]["kind"], "invoke");
        assert_eq!(runs[0]["status"], "done");
        assert!(runs[0]["summary"].is_string());
        assert!(runs[0]["ended_at"].is_string());
    }

    #[tokio::test]
    async fn a_failed_run_is_recorded_failed() {
        struct Boom;
        #[async_trait::async_trait]
        impl Backend for Boom {
            async fn run(&self, _p: &crate::Params) -> Result<Outcome, crate::BackendError> {
                Err(crate::BackendError::new("boom"))
            }
        }

        let server = Server::new(Config::default(), Arc::new(Boom));
        let mut client = TestClient::from_router(server.clone().router());
        client.initialize().await;
        let _err: Value = client
            .call_tool_expect_error("prompt", json!({ "prompt": "go" }))
            .await;

        let runs = server.runs().list(10);
        assert_eq!(runs.len(), 1);
        assert!(matches!(runs[0].status, crate::RunStatus::Failed));
        assert_eq!(runs[0].summary.as_deref(), Some("boom"));
    }

    #[tokio::test]
    async fn budget_cap_rejects_once_reached_and_records_cost() {
        struct Costed;
        #[async_trait::async_trait]
        impl Backend for Costed {
            async fn run(&self, _p: &crate::Params) -> Result<Outcome, crate::BackendError> {
                let mut o = Outcome::from_reply("ok", None);
                o.cost_usd = Some(0.6);
                Ok(o)
            }
        }

        let config = Config::parse("[budget]\nmax_usd = 1.0\n").unwrap();
        let server = Server::new(config, Arc::new(Costed));

        // Two runs (0.6 each) reach the 1.0 cap.
        for prompt in ["a", "b"] {
            server
                .run(Call {
                    prompt: prompt.into(),
                    ..Default::default()
                })
                .await
                .unwrap();
        }
        assert!(server.budget().spent() >= 1.0);

        // The next run is rejected.
        let err = server
            .run(Call {
                prompt: "c".into(),
                ..Default::default()
            })
            .await;
        assert!(matches!(err, Err(crate::RunError::BudgetExceeded)));

        // Cost is recorded on the runs.
        let runs = server.runs().list(10);
        assert_eq!(runs[0].cost_usd, Some(0.6));
    }

    #[tokio::test]
    async fn no_security_root_allows_any_path() {
        // server() sets no [security] root, so a client path passes unchecked.
        let mut c = client().await;
        let p = resolved(&mut c, json!({ "prompt": "go", "cwd": "/anywhere/fake" })).await;
        assert_eq!(p["cwd"], "/anywhere/fake");
    }

    #[tokio::test]
    async fn security_root_contains_client_paths() {
        use std::fs;
        let base = std::env::temp_dir().join(format!("tower-agent-contain-{}", std::process::id()));
        let inside = base.join("inside");
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&inside).unwrap();

        let config =
            Config::parse(&format!("[security]\nroot = \"{}\"\n", base.display())).unwrap();
        let mut client =
            TestClient::from_router(Server::new(config, Arc::new(StubBackend)).router());
        client.initialize().await;

        // Inside the root: accepted.
        let ok: Value = client
            .call_tool_typed(
                "prompt",
                json!({ "prompt": "go", "cwd": inside.to_str().unwrap() }),
            )
            .await;
        assert!(ok["reply"].is_string());

        // Outside the root: rejected.
        let _e1: Value = client
            .call_tool_expect_error("prompt", json!({ "prompt": "go", "cwd": "/tmp" }))
            .await;
        // Nonexistent: rejected.
        let _e2: Value = client
            .call_tool_expect_error(
                "prompt",
                json!({ "prompt": "go", "cwd": base.join("nope").to_str().unwrap() }),
            )
            .await;

        let _ = fs::remove_dir_all(&base);
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
