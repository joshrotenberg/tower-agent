//! The MCP surface is a thin projection that preserves middleware, and the
//! continuation scope is enforced end to end.
//!
//! Moved here from `tower-agent`, which could not use the shipped projection
//! because the core must not depend on an interface crate.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tower::{ServiceBuilder, service_fn};
use tower_agent::layer::{ObserveLayer, ReceiptObserver, ReceiptStatus, ValidateTurnLayer};
use tower_agent::{
    AgentError, AgentRequest, BoxTurnService, Cost, EffectState, ErrorKind, FailureEvidence,
    FailurePhase, SessionHandle, TokenUsage, Turn, TurnOutcome,
};
use tower_agent_mcp::{
    ContinuationStore, FixedScope, InMemoryContinuationStore, Projection, ProviderMessages, Scope,
    ScopeUnavailable, TurnTool,
};
use tower_mcp::testing::TestClient;
use tower_mcp::{McpRouter, RequestContext};

const PRIVATE: &str = "host-private-session";

/// Records what reached the provider so a test can prove a turn never launched
/// and that a resumed turn carried the session it claimed to.
#[derive(Clone, Default)]
struct Observed {
    launches: Arc<AtomicUsize>,
    last_resume: Arc<Mutex<Option<String>>>,
}

fn provider(observed: Observed) -> BoxTurnService {
    let service = service_fn(move |request: AgentRequest<Turn>| {
        let observed = observed.clone();
        async move {
            observed.launches.fetch_add(1, Ordering::SeqCst);
            *observed.last_resume.lock().unwrap() = request
                .body
                .session
                .as_ref()
                .map(|session| session.value().to_string());

            if request.body.prompt == "fail after effects" {
                return Err(AgentError::deadline_exceeded(EffectState::Possible)
                    .with_cause(AgentError::new(
                        ErrorKind::Provider,
                        format!("provider reported a partial edit for {PRIVATE}"),
                        FailurePhase::Settlement,
                        EffectState::Reported,
                    ))
                    .with_evidence(FailureEvidence {
                        session: Some(SessionHandle::new("fake", PRIVATE)),
                        ..FailureEvidence::default()
                    }));
            }

            let mut outcome = TurnOutcome::new(request.body.prompt);
            outcome.session = Some(SessionHandle::new("fake", PRIVATE));
            outcome.usage = Some(TokenUsage {
                input: Some(13),
                output: Some(8),
                ..TokenUsage::default()
            });
            outcome.cost = Some(Cost::usd(0.25));
            outcome.duration = Some(Duration::from_millis(42));
            outcome.provider_turns = Some(2);
            Ok::<_, AgentError>(outcome)
        }
    });
    BoxTurnService::new(service)
}

fn router(tool: tower_mcp::Tool) -> McpRouter {
    McpRouter::new()
        .server_info("tower-agent-mcp-test", "0.1.0")
        .tool(tool)
}

#[tokio::test]
async fn the_surface_projects_terminal_facts_and_preserves_middleware() {
    let (receipt_observer, mut receipts) = ReceiptObserver::channel(4);
    let observed = Observed::default();
    let service = ServiceBuilder::new()
        .layer(ObserveLayer::new(receipt_observer))
        .layer(ValidateTurnLayer::new())
        .service(provider(observed.clone()));

    let tool = TurnTool::new(
        BoxTurnService::new(service),
        Arc::new(InMemoryContinuationStore::new()),
        Arc::new(FixedScope::stdio()),
    )
    .build();
    let mut client = TestClient::from_router(router(tool));
    client.initialize().await;

    let tools = client.list_tools().await;
    let prompt = tools
        .iter()
        .find(|tool| tool["name"] == "prompt")
        .expect("prompt tool");
    assert_eq!(prompt["inputSchema"]["required"][0], "prompt");

    let success = client
        .call_tool("prompt", serde_json::json!({ "prompt": "hello" }))
        .await;
    assert!(!success.is_error);
    assert_eq!(success.first_text(), Some("hello"));

    let structured = success.structured_content.as_ref().expect("structured");
    assert_eq!(structured["output"], "hello");
    assert_eq!(structured["session"]["provider"], "fake");
    assert_eq!(structured["usage"]["total"], 21);
    assert_eq!(structured["cost"]["amount"], 0.25);
    assert_eq!(structured["durationMs"], 42);
    assert_eq!(structured["providerTurns"], 2);
    assert!(!structured.to_string().contains(PRIVATE), "{structured}");

    // The layers under the tool still ran and still recorded one receipt.
    assert_eq!(
        receipts.recv().await.expect("receipt").status,
        ReceiptStatus::Succeeded
    );

    // A refusal from ValidateTurnLayer arrives with its classification intact.
    let refused = client
        .call_tool("prompt", serde_json::json!({ "prompt": "   " }))
        .await;
    assert!(refused.is_error);
    let structured = refused.structured_content.as_ref().expect("structured");
    assert_eq!(structured["kind"], "invalid_request");
    assert_eq!(structured["phase"], "validation");
    assert_eq!(structured["effects"], "none");
    assert_eq!(structured["replaySafe"], true);
}

#[tokio::test]
async fn a_failure_is_redacted_through_its_cause_and_still_resumable() {
    let observed = Observed::default();
    let tool = TurnTool::new(
        provider(observed),
        Arc::new(InMemoryContinuationStore::new()),
        Arc::new(FixedScope::stdio()),
    )
    .build();
    let mut client = TestClient::from_router(router(tool));
    client.initialize().await;

    let failed = client
        .call_tool(
            "prompt",
            serde_json::json!({ "prompt": "fail after effects" }),
        )
        .await;

    assert!(failed.is_error);
    let structured = failed.structured_content.as_ref().expect("structured");
    assert_eq!(structured["kind"], "deadline_exceeded");
    assert_eq!(structured["effects"], "reported");
    assert_eq!(structured["cause"]["kind"], "provider");
    assert_eq!(structured["cause"]["phase"], "settlement");

    // The provider put a session value in the cause message.
    assert!(!structured.to_string().contains(PRIVATE), "{structured}");

    // Not replayable, and still resumable. Both claims, neither implying the
    // other.
    assert_eq!(structured["replaySafe"], false);
    assert!(structured["evidence"]["session"]["continuation"].is_string());
}

#[tokio::test]
async fn a_continuation_resumes_the_conversation_it_names() {
    let observed = Observed::default();
    let tool = TurnTool::new(
        provider(observed.clone()),
        Arc::new(InMemoryContinuationStore::new()),
        Arc::new(FixedScope::stdio()),
    )
    .build();
    let mut client = TestClient::from_router(router(tool));
    client.initialize().await;

    let first = client
        .call_tool("prompt", serde_json::json!({ "prompt": "start" }))
        .await;
    let id = first.structured_content.as_ref().expect("structured")["session"]["continuation"]
        .as_str()
        .expect("continuation")
        .to_string();
    assert_eq!(*observed.last_resume.lock().unwrap(), None);

    let second = client
        .call_tool(
            "prompt",
            serde_json::json!({ "prompt": "continue", "continuation": id }),
        )
        .await;

    assert!(!second.is_error);
    // The public name resolved back to the provider's private handle, which
    // the client never saw.
    assert_eq!(
        *observed.last_resume.lock().unwrap(),
        Some(PRIVATE.to_string())
    );
}

#[tokio::test]
async fn a_continuation_from_another_scope_is_refused_without_launching() {
    let current = Arc::new(Mutex::new(Scope::session("alice")));
    let reader = Arc::clone(&current);
    let scopes = move |_: &RequestContext| Ok(reader.lock().unwrap().clone());

    let observed = Observed::default();
    let tool = TurnTool::new(
        provider(observed.clone()),
        Arc::new(InMemoryContinuationStore::new()),
        Arc::new(scopes),
    )
    .build();
    let mut client = TestClient::from_router(router(tool));
    client.initialize().await;

    let mine = client
        .call_tool("prompt", serde_json::json!({ "prompt": "start" }))
        .await;
    let id = mine.structured_content.as_ref().expect("structured")["session"]["continuation"]
        .as_str()
        .expect("continuation")
        .to_string();
    let launched = observed.launches.load(Ordering::SeqCst);

    // Same identifier, different caller.
    *current.lock().unwrap() = Scope::session("bob");
    let stolen = client
        .call_tool(
            "prompt",
            serde_json::json!({ "prompt": "continue", "continuation": id.clone() }),
        )
        .await;

    assert!(stolen.is_error);
    let structured = stolen.structured_content.as_ref().expect("structured");
    assert_eq!(structured["kind"], "invalid_request");
    assert_eq!(structured["phase"], "validation");
    assert_eq!(structured["effects"], "none");
    // The refusal happened before launch, so no turn ran on someone else's
    // conversation and nothing was spent finding that out.
    assert_eq!(observed.launches.load(Ordering::SeqCst), launched);

    // The owner can still use it.
    *current.lock().unwrap() = Scope::session("alice");
    let owner = client
        .call_tool(
            "prompt",
            serde_json::json!({ "prompt": "continue", "continuation": id }),
        )
        .await;
    assert!(!owner.is_error);
}

#[tokio::test]
async fn an_unknown_continuation_refuses_rather_than_starting_a_new_conversation() {
    let observed = Observed::default();
    let tool = TurnTool::new(
        provider(observed.clone()),
        Arc::new(InMemoryContinuationStore::new()),
        Arc::new(FixedScope::stdio()),
    )
    .build();
    let mut client = TestClient::from_router(router(tool));
    client.initialize().await;

    let result = client
        .call_tool(
            "prompt",
            serde_json::json!({ "prompt": "continue", "continuation": "never-minted" }),
        )
        .await;

    // Silently starting a fresh conversation would answer a different question
    // and look like success to a caller who asked to continue one.
    assert!(result.is_error);
    assert_eq!(observed.launches.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_request_with_no_scope_never_reaches_the_provider() {
    let observed = Observed::default();
    let scopes = |_: &RequestContext| Err(ScopeUnavailable("unauthenticated".to_string()));
    let tool = TurnTool::new(
        provider(observed.clone()),
        Arc::new(InMemoryContinuationStore::new()),
        Arc::new(scopes),
    )
    .build();
    let mut client = TestClient::from_router(router(tool));
    client.initialize().await;

    let result = client
        .call_tool("prompt", serde_json::json!({ "prompt": "hello" }))
        .await;

    assert!(result.is_error);
    assert_eq!(observed.launches.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn verbatim_provider_text_is_opt_in_at_the_tool() {
    let observed = Observed::default();
    let tool = TurnTool::new(
        provider(observed),
        Arc::new(InMemoryContinuationStore::new()),
        Arc::new(FixedScope::stdio()),
    )
    .with_projection(Projection::new().with_provider_messages(ProviderMessages::Verbatim))
    .build();
    let mut client = TestClient::from_router(router(tool));
    client.initialize().await;

    let failed = client
        .call_tool(
            "prompt",
            serde_json::json!({ "prompt": "fail after effects" }),
        )
        .await;

    // What the opt-in costs, asserted rather than described.
    let structured = failed.structured_content.as_ref().expect("structured");
    assert!(structured.to_string().contains(PRIVATE));
}

#[tokio::test]
async fn a_store_failure_costs_resumability_and_not_the_turn() {
    struct Refusing;

    impl ContinuationStore for Refusing {
        fn mint(
            &self,
            _session: SessionHandle,
            _scope: Scope,
        ) -> tower_agent_mcp::StoreFuture<
            '_,
            Result<tower_agent_mcp::ContinuationId, tower_agent_mcp::ContinuationError>,
        > {
            Box::pin(async {
                Err(tower_agent_mcp::ContinuationError::Backend(
                    "disk on fire".to_string(),
                ))
            })
        }

        fn resolve(
            &self,
            _id: tower_agent_mcp::ContinuationId,
            _scope: Scope,
        ) -> tower_agent_mcp::StoreFuture<
            '_,
            Result<Option<SessionHandle>, tower_agent_mcp::ContinuationError>,
        > {
            Box::pin(async { Ok(None) })
        }

        fn forget_scope(
            &self,
            _scope: Scope,
        ) -> tower_agent_mcp::StoreFuture<'_, Result<(), tower_agent_mcp::ContinuationError>>
        {
            Box::pin(async { Ok(()) })
        }
    }

    let observed = Observed::default();
    let tool = TurnTool::new(
        provider(observed),
        Arc::new(Refusing),
        Arc::new(FixedScope::stdio()),
    )
    .build();
    let mut client = TestClient::from_router(router(tool));
    client.initialize().await;

    let result = client
        .call_tool("prompt", serde_json::json!({ "prompt": "hello" }))
        .await;

    // The work is done. Discarding a settled result because bookkeeping failed
    // would be the worse outcome, so the turn succeeds and reports a session
    // that has no name.
    assert!(!result.is_error);
    let structured = result.structured_content.as_ref().expect("structured");
    assert_eq!(structured["session"]["present"], true);
    assert_eq!(
        structured["session"]["continuation"],
        serde_json::Value::Null
    );
}
