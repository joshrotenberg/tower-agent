//! Planning at the MCP surface: what is missing is asked for, then run.

#![cfg(feature = "plan-claude")]

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{Value, json};
use tower::service_fn;
use tower_agent::{AgentError, AgentRequest, SessionHandle, TurnOutcome};
use tower_agent_mcp::{ContinuationStore, FixedScope, InMemoryContinuationStore, PlanTool, Scope};
use tower_agent_plan::{PartialTurn, ProviderId, ReadyTurn};
use tower_mcp::context::{
    ChannelClientRequester, ClientRequesterHandle, OutgoingRequestReceiver,
    outgoing_request_channel,
};
use tower_mcp::protocol::RequestId;
use tower_mcp::{RequestContext, Tool};

const PRIVATE: &str = "host-private-session";

/// What the planner produced, so a test can assert the fold rather than just
/// the fact that something ran.
#[derive(Clone, Default)]
struct Observed {
    launches: Arc<AtomicUsize>,
    prompt: Arc<Mutex<Option<String>>>,
    resumed: Arc<Mutex<Option<String>>>,
}

/// Records every server-to-client request, and answers with `respond`.
#[derive(Clone, Default)]
struct Asked(Arc<Mutex<Vec<(String, Value)>>>);

fn mock_client(
    mut rx: OutgoingRequestReceiver,
    asked: Asked,
    respond: impl Fn(&str, &Value) -> Value + Send + 'static,
) {
    tokio::spawn(async move {
        while let Some(request) = rx.recv().await {
            asked
                .0
                .lock()
                .unwrap()
                .push((request.method.clone(), request.params.clone()));
            let answer = respond(&request.method, &request.params);
            let _ = request.response_tx.send(Ok(answer));
        }
    });
}

fn context(
    asked: Asked,
    respond: impl Fn(&str, &Value) -> Value + Send + 'static,
) -> RequestContext {
    let (tx, rx) = outgoing_request_channel(8);
    mock_client(rx, asked, respond);
    let requester: ClientRequesterHandle = Arc::new(ChannelClientRequester::new(tx));
    RequestContext::new(RequestId::Number(1)).with_client_requester(requester)
}

fn planner(observed: Observed, store: Arc<dyn ContinuationStore>) -> Tool {
    let service = service_fn(move |request: AgentRequest<ReadyTurn>| {
        let observed = observed.clone();
        async move {
            observed.launches.fetch_add(1, Ordering::SeqCst);
            let ReadyTurn::Claude(turn) = request.body else {
                panic!("only the claude planner is enabled in this build");
            };
            *observed.prompt.lock().unwrap() = Some(turn.prompt.clone());
            *observed.resumed.lock().unwrap() =
                turn.session.as_ref().map(|s| s.value().to_string());

            let mut outcome = TurnOutcome::new(turn.prompt);
            outcome.session = Some(SessionHandle::new("claude", PRIVATE));
            Ok::<_, AgentError>(outcome)
        }
    });

    PlanTool::new(service, store, Arc::new(FixedScope::stdio()))
        .with_providers(vec![ProviderId::Claude])
        .build()
}

#[tokio::test]
async fn a_complete_request_runs_without_asking_anything() {
    let observed = Observed::default();
    let tool = planner(observed.clone(), Arc::new(InMemoryContinuationStore::new()));
    let asked = Asked::default();
    let ctx = context(asked.clone(), |_, _| json!({ "action": "cancel" }));

    let result = tool
        .call_with_context(ctx, json!({ "prompt": "summarize", "provider": "claude" }))
        .await;

    assert!(!result.is_error);
    assert_eq!(observed.launches.load(Ordering::SeqCst), 1);
    assert_eq!(*observed.prompt.lock().unwrap(), Some("summarize".into()));
    // Nothing was missing, so the client was never interrupted.
    assert!(asked.0.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_missing_prompt_is_elicited_and_the_answer_is_folded_back() {
    let observed = Observed::default();
    let tool = planner(observed.clone(), Arc::new(InMemoryContinuationStore::new()));
    let asked = Asked::default();
    let ctx = context(asked.clone(), |method, _params| {
        assert_eq!(method, "elicitation/create");
        json!({ "action": "accept", "content": { "prompt": "elicited prompt" } })
    });

    let result = tool
        .call_with_context(ctx, json!({ "provider": "claude" }))
        .await;

    assert!(!result.is_error);
    // The answer became a planner Answer, resolved, compiled, and ran.
    assert_eq!(
        *observed.prompt.lock().unwrap(),
        Some("elicited prompt".into())
    );

    let asked = asked.0.lock().unwrap();
    assert_eq!(asked.len(), 1);
    let schema = &asked[0].1["requestedSchema"];
    assert!(schema["properties"]["prompt"].is_object());
    assert_eq!(schema["properties"]["prompt"]["type"], "string");
}

#[tokio::test]
async fn a_missing_provider_is_offered_as_a_choice_not_free_text() {
    let observed = Observed::default();
    let tool = planner(observed.clone(), Arc::new(InMemoryContinuationStore::new()));
    let asked = Asked::default();
    let ctx = context(
        asked.clone(),
        |_, _| json!({ "action": "accept", "content": { "provider": "claude", "prompt": "go" } }),
    );

    let result = tool.call_with_context(ctx, json!({})).await;
    assert!(!result.is_error);

    let asked = asked.0.lock().unwrap();
    let schema = &asked[0].1["requestedSchema"];
    // The requirement's ValueKind is a finite set, so it renders as a picker
    // and a client cannot answer with a provider no planner accepts.
    assert_eq!(schema["properties"]["provider"]["enum"], json!(["claude"]));
}

#[tokio::test]
async fn declining_refuses_the_turn_and_launches_nothing() {
    let observed = Observed::default();
    let tool = planner(observed.clone(), Arc::new(InMemoryContinuationStore::new()));
    let asked = Asked::default();
    let ctx = context(asked, |_, _| json!({ "action": "decline" }));

    let result = tool
        .call_with_context(ctx, json!({ "provider": "claude" }))
        .await;

    // Declining is an answer. Running with host defaults anyway would ignore
    // it, and nothing has launched, so refusing costs nothing.
    assert!(result.is_error);
    assert_eq!(observed.launches.load(Ordering::SeqCst), 0);

    // Cancelled rather than invalid: the request was fine and a person chose
    // to stop. This also separates a decline from a plan that merely failed to
    // converge, which refuses for a different reason.
    let structured = result.structured_content.as_ref().expect("structured");
    assert_eq!(structured["kind"], "cancelled");
    assert_eq!(structured["phase"], "validation");
    assert_eq!(structured["effects"], "none");
}

#[tokio::test]
async fn a_continuation_pins_the_provider_and_asks_nothing() {
    let store = Arc::new(InMemoryContinuationStore::new());
    let id = store
        .mint(
            SessionHandle::new("claude", PRIVATE),
            Scope::session("stdio"),
        )
        .await
        .expect("mint");

    let observed = Observed::default();
    let tool = planner(observed.clone(), store);
    let asked = Asked::default();
    let ctx = context(asked.clone(), |_, _| json!({ "action": "cancel" }));

    let result = tool
        .call_with_context(
            ctx,
            json!({ "prompt": "keep going", "continuation": id.as_str() }),
        )
        .await;

    assert!(!result.is_error);
    // The continuation named the provider, so provider selection was already
    // settled and the client was not asked to choose.
    assert!(asked.0.lock().unwrap().is_empty());
    assert_eq!(*observed.resumed.lock().unwrap(), Some(PRIVATE.to_string()));
}

#[tokio::test]
async fn a_continuation_from_another_scope_is_refused_before_planning() {
    let store = Arc::new(InMemoryContinuationStore::new());
    let id = store
        .mint(
            SessionHandle::new("claude", PRIVATE),
            Scope::session("somebody-else"),
        )
        .await
        .expect("mint");

    let observed = Observed::default();
    // The tool runs under FixedScope::stdio, which is not the minting scope.
    let tool = planner(observed.clone(), store);
    let asked = Asked::default();
    let ctx = context(asked.clone(), |_, _| json!({ "action": "cancel" }));

    let result = tool
        .call_with_context(
            ctx,
            json!({ "prompt": "keep going", "continuation": id.as_str() }),
        )
        .await;

    assert!(result.is_error);
    assert_eq!(observed.launches.load(Ordering::SeqCst), 0);
    assert!(asked.0.lock().unwrap().is_empty());
}

#[tokio::test]
async fn elicitation_that_never_converges_is_bounded() {
    let observed = Observed::default();
    let service = service_fn(|_: AgentRequest<ReadyTurn>| async {
        Ok::<_, AgentError>(TurnOutcome::new("never reached"))
    });
    let tool = PlanTool::new(
        service,
        Arc::new(InMemoryContinuationStore::new()),
        Arc::new(FixedScope::stdio()),
    )
    .with_providers(vec![ProviderId::Claude])
    .with_max_elicitation_rounds(2)
    .build();

    let asked = Asked::default();
    // A client that always accepts and never supplies anything.
    let ctx = context(
        asked.clone(),
        |_, _| json!({ "action": "accept", "content": {} }),
    );

    let result = tool
        .call_with_context(ctx, json!({ "provider": "claude" }))
        .await;

    assert!(result.is_error);
    assert_eq!(observed.launches.load(Ordering::SeqCst), 0);
    // Asked once, learned nothing, and stopped rather than asking forever.
    assert_eq!(asked.0.lock().unwrap().len(), 1);
    // Not a decline: nobody refused, the exchange just went nowhere.
    let structured = result.structured_content.as_ref().expect("structured");
    assert_eq!(structured["kind"], "invalid_request");
}

#[tokio::test]
async fn host_defaults_sit_beneath_what_the_client_supplies() {
    let observed = Observed::default();
    let service = service_fn({
        let observed = observed.clone();
        move |request: AgentRequest<ReadyTurn>| {
            let observed = observed.clone();
            async move {
                let ReadyTurn::Claude(turn) = request.body else {
                    panic!("claude only");
                };
                *observed.prompt.lock().unwrap() = Some(turn.prompt.clone());
                Ok::<_, AgentError>(TurnOutcome::new(turn.prompt))
            }
        }
    });

    let defaults = PartialTurn {
        provider: Some(ProviderId::Claude),
        ..PartialTurn::default()
    };
    let tool = PlanTool::new(
        service,
        Arc::new(InMemoryContinuationStore::new()),
        Arc::new(FixedScope::stdio()),
    )
    .with_application_defaults(defaults)
    .build();

    let asked = Asked::default();
    let ctx = context(asked.clone(), |_, _| json!({ "action": "cancel" }));

    // Only a prompt is supplied. The provider comes from host defaults, so
    // the client is not asked for something the host already decided.
    let result = tool.call_with_context(ctx, json!({ "prompt": "go" })).await;

    assert!(!result.is_error);
    assert!(asked.0.lock().unwrap().is_empty());
    assert_eq!(*observed.prompt.lock().unwrap(), Some("go".into()));
}
