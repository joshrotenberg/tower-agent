# tower-agent

Tower-native services and middleware for finite agent operations.

`tower-agent` is an execution library, not an agent server and not an MCP
implementation. A provider implements `tower::Service` for an owned agent
request. Applications compose that service with middleware and may project it
onto MCP, a CLI, HTTP, or another interface.

```text
application
├── tower-agent
├── provider service
└── tower-mcp                 optional downstream projection
```

The core crate has no normal dependency on `tower-mcp`.

## The service atom

One finite turn is the initial operation:

```rust
use tower::ServiceExt;
use tower_agent::{AgentRequest, EchoService, Turn};

# async fn example() -> Result<(), tower_agent::AgentError> {
let outcome = EchoService
    .oneshot(AgentRequest::new(Turn::new("inspect this repository")))
    .await?;

assert_eq!(outcome.output, "inspect this repository");
# Ok(())
# }
```

`AgentRequest<T>` separates a typed, potentially portable body from local call
state such as operation identity, cancellation, deadline, and event observation.
`Turn<O>` carries common turn data while leaving provider-specific controls in
the generic `O` options type. Provider selection is a routing concern above a
concrete service, so it is not a field on `Turn`.

The terminal contract is typed:

- `TurnOutcome` carries output and optional session, usage, cost, and timing
  evidence.
- `AgentError` preserves error kind, failure phase, and whether effects are
  absent, possible, or reported.
- `AgentEvent` provides nonblocking incremental observations without changing
  the terminal response into a stream.

## Middleware

The first spike implements layers where agent semantics differ materially from
ordinary request/response RPCs:

| Layer | Behavior |
|---|---|
| `ValidateTurnLayer` | Rejects invalid prompts before provider execution. |
| `AdmissionLayer` | Shares capacity across clones and returns typed `Busy` without a hidden queue. |
| `DeadlineLayer` | Drains explicit cancellation or a deadline before returning typed terminal evidence. |
| `ObserveLayer` | Records a typed terminal receipt with stable operation identity. |
| `SuperviseLayer` | Keeps polling an owned inner call after caller drop while signalling cancellation. |
| `CatchPanicLayer` | Converts a provider panic into typed terminal failure evidence. |

A representative outside-to-inside stack is:

```rust
use tower::ServiceBuilder;
use tower_agent::EchoService;
use tower_agent::layer::{
    AdmissionLayer, CatchPanicLayer, DeadlineLayer, ObserveLayer,
    ReceiptObserver, SuperviseLayer, ValidateTurnLayer,
};

let service = ServiceBuilder::new()
    .layer(SuperviseLayer::new())
    .layer(ObserveLayer::new(ReceiptObserver::default()))
    .layer(CatchPanicLayer::new())
    .layer(AdmissionLayer::single_flight())
    .layer(DeadlineLayer::new())
    .layer(ValidateTurnLayer::new())
    .service(EchoService);
```

This ordering is semantic. Supervision owns the call after the interface caller
goes away. Panic normalization sits inside observation so receipts see the
typed terminal failure. Admission wraps deadline short-circuiting, ensuring its
readiness permit is released even when a request is already expired, and its
capacity remains occupied until cancellation cleanup actually finishes.

Promising next layers include authority narrowing, deterministic context
assembly, budget reservation and accounting, output-contract validation, event
fanout/redaction, and circuit breaking. Retry, fallback, buffering, caching, and
coalescing are unsafe by default for effectful agent work.

## Optional MCP composition

MCP lives one level above the kernel. The
[`tower_mcp_prompt` example](crates/tower-agent/examples/tower_mcp_prompt.rs)
shows a downstream adapter that owns:

- its wire DTO and JSON Schema;
- MCP cancellation and progress mapping;
- text plus structured result encoding;
- typed domain-error projection.

The corresponding integration test proves that the same middleware still wraps
the call through MCP. `tower-mcp` is only a development dependency of the core
crate.

## Current migration state

The previous MCP-first implementation is preserved as `tower-agent-server` so
the pivot can be evaluated without deleting working behavior. It still contains
configuration, sessions, scheduling, the bus, runs, budget, and the existing MCP
surface. `BackendService` temporarily adapts its original `Backend` trait to the
new owned Tower contract.

The Claude and Codex crates now expose Tower-native services by default. Their
original `Backend` implementations exist only behind a `legacy-server` feature,
which the preserved `agent` binary enables explicitly. Default provider
dependency trees therefore do not pull in the MCP server.

The native services intentionally configure no wrapper timeout. In the locked
wrapper versions, timeout or future drop can return while provider descendants
remain alive. They reject cancellation before launch, but do not claim safe
in-flight termination. The kernel proves cancellation and drain semantics with
cooperative fakes until wrapper process ownership is upgraded.

The locked Claude wrapper can also return an I/O failure after a buffered-stdin
write fails without proving that the direct child was killed and reaped.
Recoverable Claude rail-stop errors carry session and accounting evidence that
the current `AgentError` ABI cannot yet retain. Both are explicit provider
follow-up work, not kernel guarantees.

The native Claude path uses buffered JSON so it can send the user prompt over
stdin; Claude system-prompt flags still remain in the child argument vector.
The locked Codex wrapper has no stdin prompt path, so its native service
documents that the entire prompt is visible there.

## Workspace

```text
crates/tower-agent          protocol-neutral service types, fakes, and middleware
crates/tower-agent-server   preserved MCP-first server and compatibility adapter
crates/tower-agent-claude   Tower-native Claude service; optional legacy backend
crates/tower-agent-codex    Tower-native Codex service; optional legacy backend
crates/agent                preserved reference CLI/server binary
```

Run the complete check suite with:

```text
just check
```

The adopted design and the remaining research questions are in
[`docs/design/tower-service-kernel.md`](docs/design/tower-service-kernel.md).

## License

MIT OR Apache-2.0.
