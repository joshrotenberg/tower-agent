//! A downstream MCP projection of a `tower-agent` service.
//!
//! `tower-mcp` is a development dependency for this example, not a dependency
//! of the kernel. The adapter owns its wire DTO, schema, progress mapping,
//! cancellation bridge, and result encoding.

use std::sync::atomic::{AtomicU64, Ordering};

use serde::Deserialize;
use tower::ServiceExt;
use tower_agent::{
    AgentError, AgentEvent, AgentRequest, BoxTurnService, CallContext, CancellationToken,
    EchoService, EventSendError, EventSink, OperationId, Turn, TurnOutcome,
};
use tower_mcp::extract::{Context, Json, State};
use tower_mcp::{CallToolResult, Error, McpRouter, Tool, ToolBuilder};

#[derive(Deserialize, tower_mcp::schemars::JsonSchema)]
#[schemars(crate = "tower_mcp::schemars")]
struct PromptInput {
    prompt: String,
}

fn prompt_tool(agent: BoxTurnService) -> Tool {
    ToolBuilder::new("prompt")
        .description("Run one finite agent turn")
        .extractor_handler(
            agent,
            |State(agent): State<BoxTurnService>,
             context: Context,
             Json(input): Json<PromptInput>| async move {
                let cancellation = CancellationToken::new();
                let events = tower_agent::EventObserver::new(McpProgress::new(context.clone()));
                let call_context = CallContext::new()
                    .with_cancellation(cancellation.clone())
                    .with_events(events);
                let operation_id = call_context.operation_id();
                let call = agent.oneshot(AgentRequest::with_context(
                    Turn::new(input.prompt),
                    call_context,
                ));
                tokio::pin!(call);

                let result = tokio::select! {
                    result = &mut call => result,
                    () = context.cancelled() => {
                        cancellation.cancel();
                        call.await
                    }
                };
                Ok::<_, Error>(adapt_result(result, operation_id))
            },
        )
        .build()
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
        // Provider session values are host-private. A production adapter can
        // mint its own public continuation id instead of exposing this token.
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

fn duration_millis(duration: std::time::Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

struct McpProgress {
    context: Context,
    sequence: AtomicU64,
}

impl McpProgress {
    fn new(context: Context) -> Self {
        Self {
            context,
            sequence: AtomicU64::new(0),
        }
    }
}

impl EventSink for McpProgress {
    fn try_emit(&self, event: AgentEvent) -> Result<(), EventSendError> {
        let message = match event {
            AgentEvent::Started => "started".to_string(),
            AgentEvent::OutputDelta { text } => text,
            AgentEvent::ThinkingDelta { text } => format!("[thinking] {text}"),
            AgentEvent::ToolStarted { name } => format!("[tool] {name}"),
            AgentEvent::TurnStarted { number } => format!("[turn {number}]"),
            AgentEvent::Status { message } | AgentEvent::Warning { message } => message,
            AgentEvent::Usage { usage } => usage.total().map_or_else(
                || "[tokens] unreported".to_string(),
                |total| format!("[tokens] {total}"),
            ),
            _ => return Ok(()),
        };
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed) + 1;
        self.context
            .report_progress_sync(sequence as f64, None, Some(&message));
        Ok(())
    }
}

fn main() {
    let agent = BoxTurnService::new(EchoService);
    let _router = McpRouter::new()
        .server_info("tower-agent-example", "0.1.0")
        .tool(prompt_tool(agent));
}
