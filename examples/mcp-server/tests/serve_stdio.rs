//! Drives the server the way a client does, over the real transport.
//!
//! A stdio server blocks on its input, which is why `just check` could not run
//! it as an ordinary example. `run_with_streams` takes any reader and writer,
//! so the exchange happens over an in-memory pipe instead: scripted JSON-RPC
//! in, assertions out, and the loop terminates when the reader hits EOF.
//!
//! Nothing here needs credentials. The default provider is the fake one, which
//! is also what makes the continuation round trip meaningful: it mints and
//! continues real session handles.

use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::AsyncReadExt;
use tower_mcp::transport::stdio::BidirectionalStdioTransport;

use mcp_server_example as server;

/// A live server, driven one request at a time over a pipe.
///
/// Interleaved rather than write-everything-then-read, so a later request can
/// carry a value the server produced in an earlier response. That is what a
/// client does, and it is the only way to test continuation against a single
/// server rather than two.
struct Client {
    writer: tokio::io::WriteHalf<tokio::io::DuplexStream>,
    reader: tokio::io::BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
    served: tokio::task::JoinHandle<()>,
}

impl Client {
    fn start(settings: server::Settings) -> Self {
        let mut transport = BidirectionalStdioTransport::new(server::router(&settings));
        let (client, server_side) = tokio::io::duplex(64 * 1024);
        let (server_reader, server_writer) = tokio::io::split(server_side);
        let (reader, writer) = tokio::io::split(client);

        let served = tokio::spawn(async move {
            transport
                .run_with_streams(server_reader, server_writer)
                .await
                .expect("transport ran");
        });

        Self {
            writer,
            reader: tokio::io::BufReader::new(reader),
            served,
        }
    }

    /// Send one request and return the response to it.
    async fn call(&mut self, request: Value) -> Value {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

        let mut line = serde_json::to_string(&request).expect("serialize");
        line.push('\n');
        self.writer.write_all(line.as_bytes()).await.expect("write");
        self.writer.flush().await.expect("flush");

        loop {
            let mut response = String::new();
            let read = self.reader.read_line(&mut response).await.expect("read");
            assert!(read > 0, "server closed before answering {request}");
            if response.trim().is_empty() {
                continue;
            }
            let value: Value = serde_json::from_str(&response).expect("valid json-rpc");
            // Notifications carry no id and are not the answer to anything.
            if value.get("id").is_some() {
                return value;
            }
        }
    }

    /// Close the input, which is how a client shuts a stdio server down.
    async fn shutdown(mut self) {
        use tokio::io::AsyncWriteExt;
        self.writer.shutdown().await.expect("shutdown");
        self.served.await.expect("server exited cleanly");
    }
}

fn fake_settings() -> server::Settings {
    server::Settings {
        provider: server::Provider::Fake,
        messages: tower_agent_mcp::ProviderMessages::Redacted,
        concurrency: 2,
        timeout: Some(Duration::from_secs(30)),
    }
}

/// Feed `requests` to the server and collect every response it writes.
async fn exchange(requests: &[Value]) -> Vec<Value> {
    let settings = server::Settings {
        provider: server::Provider::Fake,
        messages: tower_agent_mcp::ProviderMessages::Redacted,
        concurrency: 2,
        timeout: Some(Duration::from_secs(30)),
    };
    let mut transport = BidirectionalStdioTransport::new(server::router(&settings));

    let (client, server_side) = tokio::io::duplex(64 * 1024);
    let (server_reader, server_writer) = tokio::io::split(server_side);
    let (mut client_reader, mut client_writer) = tokio::io::split(client);

    let mut input = String::new();
    for request in requests {
        input.push_str(&serde_json::to_string(request).expect("serialize"));
        input.push('\n');
    }

    let writer = tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        client_writer
            .write_all(input.as_bytes())
            .await
            .expect("write");
        client_writer.shutdown().await.expect("shutdown");
    });

    let served = tokio::spawn(async move {
        transport
            .run_with_streams(server_reader, server_writer)
            .await
            .expect("transport ran");
    });

    let mut raw = String::new();
    client_reader.read_to_string(&mut raw).await.expect("read");
    writer.await.expect("writer");
    served.await.expect("server");

    raw.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("valid json-rpc"))
        .collect()
}

fn initialize() -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "serve-stdio-test", "version": "0.1.0" }
        }
    })
}

/// MCP omits `isError` entirely on success rather than sending `false`, so a
/// successful result is the absence of the flag, not a falsy one.
fn succeeded(result: &Value) -> bool {
    result["isError"] != json!(true)
}

fn result_for(responses: &[Value], id: i64) -> &Value {
    responses
        .iter()
        .find(|response| response["id"] == id)
        .map(|response| &response["result"])
        .unwrap_or_else(|| panic!("no response with id {id} in {responses:#?}"))
}

#[tokio::test]
async fn the_server_completes_a_real_client_handshake() {
    let responses = exchange(&[
        initialize(),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
    ])
    .await;

    let init = result_for(&responses, 1);
    assert_eq!(init["serverInfo"]["name"], server::SERVER_NAME);

    let names: Vec<&str> = result_for(&responses, 2)["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|tool| tool["name"].as_str().expect("name"))
        .collect();

    // Both tools are published, and the planning one is only present because
    // the transport can carry a server-to-client request.
    assert!(names.contains(&"prompt"), "{names:?}");
    assert!(names.contains(&"plan"), "{names:?}");
}

#[tokio::test]
async fn a_turn_runs_and_its_continuation_is_honored_over_the_wire() {
    let mut client = Client::start(fake_settings());
    client.call(initialize()).await;

    let first = client
        .call(json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": { "name": "prompt", "arguments": { "prompt": "first" } }
        }))
        .await;

    let structured = &first["result"]["structuredContent"];
    assert!(succeeded(&first["result"]), "{first:#?}");
    assert_eq!(structured["output"], "first");
    assert_eq!(structured["session"]["provider"], "fake");

    let continuation = structured["session"]["continuation"]
        .as_str()
        .expect("a continuation was minted")
        .to_string();

    // The private session handle never crossed the wire, only this name for it.
    assert!(
        !first.to_string().contains("fake-"),
        "handle leaked: {first}"
    );

    let second = client
        .call(json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": {
                "name": "prompt",
                "arguments": { "prompt": "second", "continuation": continuation }
            }
        }))
        .await;

    // Same server, same store: the name resolved and the turn was admitted,
    // where an unknown or out-of-scope name is refused before launch.
    //
    // That the resolved handle actually reaches the provider is asserted in
    // `tower-agent-mcp`'s own tests with a recording provider. It cannot be
    // observed from out here, because the fake's output is identical whether
    // it continued a conversation or started one, and the handle is redacted
    // by design. Asserting it here would be a test that cannot fail.
    assert!(succeeded(&second["result"]), "{second:#?}");
    assert_eq!(second["result"]["structuredContent"]["output"], "second");

    client.shutdown().await;
}

#[tokio::test]
async fn a_refusal_arrives_with_its_classification_intact() {
    let responses = exchange(&[
        initialize(),
        json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": { "name": "prompt", "arguments": { "prompt": "   " } }
        }),
    ])
    .await;

    let refused = result_for(&responses, 2);
    assert!(!succeeded(refused), "{refused:#?}");

    // ValidateTurnLayer refused this before launch, and the classification
    // survived the whole way out to the wire.
    let structured = &refused["structuredContent"];
    assert_eq!(structured["kind"], "invalid_request");
    assert_eq!(structured["phase"], "validation");
    assert_eq!(structured["effects"], "none");
    assert_eq!(structured["replaySafe"], true);
}

#[tokio::test]
async fn an_unknown_continuation_is_refused_rather_than_started_fresh() {
    let responses = exchange(&[
        initialize(),
        json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": {
                "name": "prompt",
                "arguments": { "prompt": "go", "continuation": "never-minted" }
            }
        }),
    ])
    .await;

    assert!(!succeeded(result_for(&responses, 2)));
}
