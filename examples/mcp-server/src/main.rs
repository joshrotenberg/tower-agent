//! Binary entry point. The server itself is the library beside this file, so
//! the transport test can drive it without going through `main`.

use mcp_server_example::{Settings, router};
use tower_mcp::transport::stdio::BidirectionalStdioTransport;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let settings = Settings::from_env()?;

    // Bidirectional rather than the plain stdio transport: the planning tool
    // elicits missing values, which is a server-to-client request, and only
    // this transport wires a client requester into the request context.
    let mut transport = BidirectionalStdioTransport::new(router(&settings));
    transport
        .run()
        .await
        .map_err(|error| anyhow::anyhow!("{error}"))
}
