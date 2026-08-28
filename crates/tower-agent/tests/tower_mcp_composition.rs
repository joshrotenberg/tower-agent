//! Proves an MCP surface is a thin projection that preserves middleware.
//!
//! The projection helpers below are a local copy. The shipped implementation
//! is `tower_agent_mcp::Projection`, which enforces the same two rules and
//! adds continuation naming. This test cannot use it: `tower-agent-mcp`
//! depends on this crate, and `scripts/check-core-deps.sh` keeps the core
//! free of interface crates.
//!
//! Both are kept until `TurnTool` lands, at which point this test moves into
//! `tower-agent-mcp` and the copy goes away. Change one, change the other.

use serde::Deserialize;
use std::time::Duration;

use tower::{ServiceBuilder, ServiceExt, service_fn};
use tower_agent::layer::{ObserveLayer, ReceiptObserver, ReceiptStatus, ValidateTurnLayer};
use tower_agent::{
    AgentError, AgentRequest, BoxTurnService, CallContext, Cost, EffectState, ErrorKind,
    FailurePhase, OperationId, SessionHandle, TokenUsage, Turn, TurnOutcome,
};
use tower_mcp::extract::{Json, State};
use tower_mcp::testing::TestClient;
use tower_mcp::{CallToolResult, Error, McpRouter, ToolBuilder};

#[derive(Deserialize, tower_mcp::schemars::JsonSchema)]
#[schemars(crate = "tower_mcp::schemars")]
struct PromptInput {
    prompt: String,
}

fn router(agent: BoxTurnService) -> McpRouter {
    let prompt = ToolBuilder::new("prompt")
        .description("Run one finite agent turn")
        .extractor_handler(
            agent,
            |State(agent): State<BoxTurnService>, Json(input): Json<PromptInput>| async move {
                let context = CallContext::new();
                let operation_id = context.operation_id();
                let result = agent
                    .oneshot(AgentRequest::with_context(Turn::new(input.prompt), context))
                    .await;
                Ok::<_, Error>(adapt_result(result, operation_id))
            },
        )
        .build();

    McpRouter::new()
        .server_info("tower-agent-composition-test", "0.1.0")
        .tool(prompt)
}

fn adapt_result(
    result: Result<TurnOutcome, AgentError>,
    operation_id: OperationId,
) -> CallToolResult {
    match result {
        Ok(outcome) => {
            let structured = outcome_json(&outcome, operation_id);
            let mut result = CallToolResult::text(outcome.output.clone());
            result.structured_content = Some(structured);
            result
        }
        Err(error) => {
            let structured = error_json(&error, operation_id);
            let mut result = CallToolResult::error(public_error_message(&error));
            result.structured_content = Some(structured);
            result
        }
    }
}

fn outcome_json(outcome: &TurnOutcome, operation_id: OperationId) -> serde_json::Value {
    serde_json::json!({
        "operationId": operation_id.get(),
        "output": outcome.output,
        "session": outcome.session.as_ref().map(|session| serde_json::json!({
            "provider": session.provider(),
            "present": true,
        })),
        "usage": outcome.usage.map(|usage| serde_json::json!({
            "input": usage.input,
            "cachedInput": usage.cached_input,
            "cacheWriteInput": usage.cache_write_input,
            "output": usage.output,
            "reasoningOutput": usage.reasoning_output,
            "total": usage.total(),
        })),
        "cost": outcome.cost.as_ref().map(|cost| serde_json::json!({
            "amount": cost.amount,
            "currency": cost.currency,
        })),
        "durationMs": outcome.duration.map(duration_millis),
        "providerTurns": outcome.provider_turns,
    })
}

fn error_json(error: &AgentError, operation_id: OperationId) -> serde_json::Value {
    let mut value = error_evidence_json(error);
    value["operationId"] = serde_json::json!(operation_id.get());
    value
}

fn error_evidence_json(error: &AgentError) -> serde_json::Value {
    let evidence = error.evidence.as_deref();
    serde_json::json!({
        "kind": error.kind.to_string(),
        "phase": error.phase.to_string(),
        "effects": error.effects.to_string(),
        "message": public_error_message(error),
        "evidence": {
            "session": evidence.and_then(|evidence| evidence.session.as_ref()).map(|session| serde_json::json!({
                "provider": session.provider(),
                "present": true,
            })),
            "usage": evidence.and_then(|evidence| evidence.usage).map(|usage| serde_json::json!({
                "input": usage.input,
                "cachedInput": usage.cached_input,
                "cacheWriteInput": usage.cache_write_input,
                "output": usage.output,
                "reasoningOutput": usage.reasoning_output,
                "total": usage.total(),
            })),
            "cost": evidence.and_then(|evidence| evidence.cost.as_ref()).map(|cost| serde_json::json!({
                "amount": cost.amount,
                "currency": cost.currency,
            })),
            "durationMs": evidence.and_then(|evidence| evidence.duration).map(duration_millis),
            "providerTurns": evidence.and_then(|evidence| evidence.provider_turns),
        },
        "cause": error.cause.as_deref().map(error_evidence_json),
    })
}

fn public_error_message(error: &AgentError) -> String {
    format!("agent operation failed ({})", error.kind)
}

fn duration_millis(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

#[tokio::test]
async fn mcp_is_a_thin_projection_and_preserves_middleware() {
    let (observer, mut receipts) = ReceiptObserver::channel(4);
    let provider = service_fn(|request: AgentRequest<Turn>| async move {
        if request.body.prompt == "fail after effects" {
            return Err(
                AgentError::deadline_exceeded(EffectState::Possible).with_cause(AgentError::new(
                    ErrorKind::Provider,
                    "provider reported a partial edit for host-private-session",
                    FailurePhase::Settlement,
                    EffectState::Reported,
                )),
            );
        }

        let mut outcome = TurnOutcome::new(request.body.prompt);
        outcome.session = Some(SessionHandle::new("fake", "host-private-session"));
        outcome.usage = Some(TokenUsage {
            input: Some(13),
            output: Some(8),
            ..TokenUsage::default()
        });
        outcome.cost = Some(Cost::usd(0.25));
        outcome.duration = Some(Duration::from_millis(42));
        outcome.provider_turns = Some(2);
        Ok::<_, AgentError>(outcome)
    });
    let service = ServiceBuilder::new()
        .layer(ObserveLayer::new(observer))
        .layer(ValidateTurnLayer::new())
        .service(provider);
    let mut client = TestClient::from_router(router(BoxTurnService::new(service)));
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
    assert_eq!(
        success.structured_content.as_ref().unwrap()["output"],
        "hello"
    );
    let structured = success.structured_content.as_ref().unwrap();
    assert_eq!(structured["session"]["provider"], "fake");
    assert_eq!(structured["session"]["present"], true);
    assert!(structured["session"].get("value").is_none());
    assert_eq!(structured["usage"]["total"], 21);
    assert_eq!(structured["cost"]["amount"], 0.25);
    assert_eq!(structured["durationMs"], 42);
    assert_eq!(structured["providerTurns"], 2);
    assert_eq!(
        receipts.recv().await.expect("success receipt").status,
        ReceiptStatus::Succeeded
    );

    let failure = client
        .call_tool("prompt", serde_json::json!({ "prompt": "  " }))
        .await;
    assert!(failure.is_error);
    assert_eq!(
        failure.structured_content.as_ref().unwrap()["kind"],
        "invalid_request"
    );
    assert_eq!(
        failure.structured_content.as_ref().unwrap()["phase"],
        "validation"
    );
    assert_eq!(
        failure.structured_content.as_ref().unwrap()["effects"],
        "none"
    );
    assert!(matches!(
        receipts.recv().await.expect("failure receipt").status,
        ReceiptStatus::Failed { .. }
    ));

    let caused = client
        .call_tool(
            "prompt",
            serde_json::json!({ "prompt": "fail after effects" }),
        )
        .await;
    assert!(caused.is_error);
    let structured = caused.structured_content.as_ref().unwrap();
    assert_eq!(structured["kind"], "deadline_exceeded");
    assert_eq!(structured["effects"], "reported");
    assert_eq!(structured["cause"]["kind"], "provider");
    assert_eq!(structured["cause"]["phase"], "settlement");
    assert_eq!(structured["cause"]["effects"], "reported");
    assert!(!structured.to_string().contains("host-private-session"));
    assert!(matches!(
        receipts
            .recv()
            .await
            .expect("caused failure receipt")
            .status,
        ReceiptStatus::Failed {
            effects: EffectState::Reported,
            ..
        }
    ));
}
