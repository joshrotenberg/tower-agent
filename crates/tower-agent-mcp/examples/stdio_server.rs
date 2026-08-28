//! Compose a `tower-agent` service into an MCP router.
//!
//! The adapter owns the wire schema, redaction, and continuation naming. The
//! execution policy is composed into the service before it gets here, and the
//! tool does not change it.
//!
//! Progress reporting is not wired yet. `AgentEvent` maps onto MCP progress
//! notifications and that is the next step of `docs/mcp.md`; until then a turn
//! reports only its terminal result.
//!
//! Run with `cargo run -p tower-agent-mcp --example stdio_server`.

use std::sync::Arc;

use tower::ServiceBuilder;
use tower_agent::layer::{AdmissionLayer, CatchPanicLayer, DeadlineLayer, ValidateTurnLayer};
use tower_agent::{BoxTurnService, EchoService};
use tower_agent_mcp::{FixedScope, InMemoryContinuationStore, TurnTool};
use tower_mcp::McpRouter;

fn main() {
    // Ordinary tower-agent composition. Swap EchoService for a provider
    // service and nothing below this line changes.
    let service = ServiceBuilder::new()
        .layer(CatchPanicLayer::new())
        .layer(AdmissionLayer::new(4))
        .layer(DeadlineLayer::new())
        .layer(ValidateTurnLayer::new())
        .service(EchoService);

    // A continuation names a conversation, and the scope decides who may use
    // that name. FixedScope is correct here because stdio carries one client
    // for the life of the process. An HTTP server serving many callers must
    // supply a source that tells them apart, or every caller shares one scope.
    let tool = TurnTool::new(
        BoxTurnService::new(service),
        Arc::new(InMemoryContinuationStore::new()),
        Arc::new(FixedScope::stdio()),
    )
    .with_description("Run one finite agent turn, optionally continuing an earlier one")
    .build();

    let router = McpRouter::new()
        .server_info("tower-agent-mcp-example", "0.1.0")
        .tool(tool);

    // Constructing the router is the part worth showing. Serving it over
    // stdio is tower-mcp's concern and would block this example forever.
    println!("router built with the prompt tool; serve it with a tower-mcp transport");
    let _ = router;
}
