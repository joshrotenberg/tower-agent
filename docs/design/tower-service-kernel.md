# tower-agent: a Tower-native service kernel for agent work

Status: adopted kernel direction; initial service and middleware spike implemented

This document describes a new architectural direction for `tower-agent`. It is
intentionally independent of ongoing Roba work. It does not require Roba to
change, stop, or adopt this crate.

Architectural decision:

> The `tower-agent` crate contains no MCP implementation and has no normal,
> optional, or feature dependency on `tower-mcp` or any other MCP library. It
> exposes ordinary typed Tower services. An MCP server is one possible
> downstream consumer of those services. MCP libraries may appear as
> development dependencies for composition examples and integration tests.

The repository already contains a working agent-as-MCP experiment with a
`Backend` trait, sessions, schedules, a message bus, run records, budgets, and
Claude/Codex adapters. That implementation is valuable prior art. This proposal
does not silently describe it as though it already exists. It asks a narrower
question:

> What becomes possible if an agent turn, and eventually every agent-facing
> function, is a native Tower `Service` and agent behavior is assembled with
> Tower `Layer`s?

The answer should be proved with a small service kernel before deciding how
much of the existing server should migrate onto it.

## Summary

The atomic operation is a finite agent turn:

```text
owned request -> asynchronous work -> typed terminal result or typed failure
```

That is already the shape of `tower::Service`.

`tower-agent` should define the provider-neutral request, response, event,
error, cancellation, and readiness semantics for that operation. Concrete
Claude and Codex services should adapt `claude-wrapper` and `codex-wrapper`.
Tower middleware should add policy and mechanics such as authority checks,
single-flight admission, deadlines, observation, receipts, budgets, structured
output validation, and carefully constrained retry or fallback.

MCP is a downstream projection of those typed services, not the internal
execution model and not a feature required by the core crate. A consumer may
use `tower-mcp`, another Rust MCP library, or no MCP implementation at all. An
example can expose a service as a Tool, its state as Resources, and long calls
as MCP Tasks without duplicating the underlying lifecycle. Initially this
relationship is demonstrated only through examples and integration tests, not
through production MCP code or an adapter crate.

The intended stack is:

```text
operator or agent client
        |
        v
MCP / CLI / Rust adapter
        |
        v
hot logical agent host (optional session continuity and single-flight state)
        |
        v
Tower service stack
  observation / receipts
  ancestry and recursion policy
  authorization and authority narrowing
  admission and load shedding
  budgets and deadlines
  deterministic context assembly
  output validation
        |
        v
ClaudeService / CodexService
        |
        v
claude-wrapper / codex-wrapper
        |
        v
provider subprocess
```

The initial success criterion is deliberately smaller than a complete agent
server:

> A typed prompt service can be wrapped in representative Tower middleware,
> projected as one MCP Tool, cancelled and drained with a deterministic fake,
> observed while running, and tested without a live provider.

## Implementation checkpoint

The first spike is now executable:

- `crates/tower-agent` contains only the protocol-neutral service vocabulary,
  `EchoService`, boxed service helpers, and middleware.
- The pre-pivot MCP implementation is preserved in `tower-agent-server` while
  migration decisions remain reversible.
- `ValidateTurnLayer`, `AdmissionLayer`, `DeadlineLayer`, `ObserveLayer`,
  `CatchPanicLayer`, and `SuperviseLayer` demonstrate pre-effect validation,
  shared typed load shedding, cancellation plus drain, terminal receipts, panic
  normalization, and retained ownership after caller drop.
- `BackendService` provides a conservative bridge from the original `Backend`
  trait without claiming semantic equivalence.
- A dev-only `tower-mcp` example and in-process integration test prove ordinary
  Tool composition, the adapter-owned input schema, typed structured success
  and failure results, and unchanged middleware behavior. The example also
  demonstrates progress and request-cancellation wiring; those paths are not
  yet integration-tested.

The spike also found a hard provider boundary. The currently locked Claude and
Codex wrapper versions do not guarantee complete subprocess-tree cleanup when
their execution futures are dropped or their wrapper timeout fires. Native
services therefore configure no wrapper timeout, reject prelaunch
cancellation, and make no in-flight termination claim. Cancellation invariants
are proven only with cooperative fakes until the wrappers are upgraded or
equivalent process ownership mechanics are added.

The prompt-private Claude path is buffered so it can send the user prompt over stdin;
Claude system-prompt flags still expose those instructions in the child
argument vector. The locked Codex wrapper has no stdin prompt path, so its
native service explicitly documents full prompt exposure there.

## Relationship to the current repository

The current code has several useful concepts:

- `Backend` is the provider seam.
- `Params` is the resolved invocation.
- `Outcome` and `Event` normalize provider results and streaming observations.
- `Server` combines configuration, sessions, runs, budget, bus, path policy,
  and the MCP projection.
- `tower-agent-claude` and `tower-agent-codex` contain provider adapters.

The current `Backend` shape is close to a service but not yet one:

```rust
#[async_trait]
pub trait Backend: Send + Sync {
    async fn run(&self, params: &Params) -> Result<Outcome, BackendError>;

    async fn run_streaming(
        &self,
        params: &Params,
        events: UnboundedSender<Event>,
    ) -> Result<Outcome, BackendError>;
}
```

The important differences are:

- The request and event channel are borrowed or passed out of band rather than
  owned by one call value.
- There is no readiness or backpressure contract.
- Cross-cutting policy lives in `Server::run_with` instead of composable
  layers.
- Provider errors do not say whether execution began or effects may have
  occurred, so safe retry and fallback cannot be decided mechanically.
- Cancellation behavior is implicit in future dropping rather than part of the
  service contract.
- Capability metadata and schemas are coupled to the MCP/server layer.

The first implementation should introduce adapters between `Backend` and the
new service boundary. It should not begin with a large rewrite of sessions,
scheduling, the bus, or the CLI.

## Design principles

### One finite call is the atom

The kernel executes one finite request. It does not own a scheduler, workflow,
queue, fleet, or persistent session pool.

A hot logical agent may retain a provider session and submit many finite calls,
but that state is a host around the finite service. It is not a reason to make
the provider process itself permanent.

### Tower is the complete internal composition contract

The typed Rust execution boundary is `Service<Request>`. Middleware is
`Layer<Service>`. MCP, CLI commands, HTTP, tests, and other bindings are
downstream clients or projections of the same service.

No MCP dependency, protocol type, router, task store, request context, Tool
result, or MCP error belongs in the provider-neutral turn kernel. If an MCP
adapter needs a wire DTO or JSON Schema, the adapter owns it or derives it from
the public typed request.

### Requests own everything required to run

A service call must not borrow an event sink or mutable host state for the
lifetime of the provider future. The request owns or shares its prompt,
execution context, observer, and cancellation handle. This makes the future
`Send`, permits task execution, and gives cancellation a clear owner.

### Middleware must preserve agent semantics

Generic middleware is not automatically safe for agent work. An agent can
write files, call external services, spend money, or partially complete work
before failing.

Layers that retry, buffer, cache, coalesce, or fall back must have stronger
preconditions than they would for an ordinary read-only RPC.

### Backpressure is explicit; queuing is opt-in

`poll_ready` communicates capacity. The default single-agent host should load
shed with a typed `busy` result when one turn is active. It must not quietly
install `BufferLayer` and turn concurrent prompts into a hidden queue.

Hosts that want a queue may add one explicitly and document its ordering,
cancellation, capacity, and persistence semantics.

### Typed failures survive every layer

Provider, policy, timeout, budget, cancellation, and overload failures remain
distinguishable after composition and after MCP projection. Human text is not
an error ABI.

### Drop-based cancellation is a real contract

Dropping the future returned by `Service::call` must not orphan a provider
subprocess. Concrete services must terminate the entire owned process group or
transfer ownership to an explicitly observable task before the future can be
dropped safely.

### Discovery is separate from execution

Tower `Service` intentionally has no capability-introspection API. A service
therefore travels with a descriptor that identifies schemas, effects,
supported controls, and provider capabilities. The descriptor is not inferred
from runtime downcasts.

## Goals

- Define a small provider-neutral service vocabulary for one finite agent turn.
- Make Claude and Codex interchangeable at that boundary without claiming they
  support identical options.
- Enable reusable Tower middleware for agent-specific mechanics and policy.
- Preserve streaming events without making the terminal response itself a
  protocol-specific stream.
- Make direct cancellation and future-drop cancellation mechanically testable.
- Support an in-process Rust caller with no MCP dependency.
- Make it straightforward for a downstream crate to build a lossless MCP Tool
  adapter using `tower-mcp` or another Rust MCP implementation.
- Allow a hot single-agent host to retain session continuity above the finite
  service.
- Leave room for an agent to use a scoped MCP view of its own harness and, much
  later, call another agent service safely.

## Non-goals for the kernel

- A workflow or DAG engine.
- A built-in scheduler.
- A hidden job queue.
- A multi-agent bus or fleet controller.
- Durable task storage.
- Provider-private session mutation.
- A universal least-common-denominator list of every Claude and Codex flag.
- Automatic retry or fallback after arbitrary agent execution.
- Dynamic plugin loading.
- Replacing `claude-wrapper`, `codex-wrapper`, or `tower-mcp`.
- An MCP server, router, transport, task store, or protocol implementation in
  the `tower-agent` crate itself.

Existing higher-level experiments may continue to exist above the kernel. This
scope statement only says they should not determine the service atom.

## Layer model

The architecture has four independently testable layers.

### Provider mechanics

Wrapper crates build commands, validate provider versions, parse provider
events, classify provider-native errors, manage stdin/stdout/stderr, and kill
owned processes correctly.

This layer knows Claude or Codex and knows nothing about MCP.

### Agent service kernel

The kernel defines typed calls, results, errors, events, service descriptors,
and reusable layers. It knows Tower and provider-neutral agent semantics. It
does not know command-line parsing or MCP wire framing.

### Logical agent host

An optional stateful host retains a session handle, admits at most one active
turn, publishes current state, and creates a fresh finite service call for each
prompt. It may itself implement `Service<AgentRequest<Turn>>`.

The host remains hot while provider processes come and go.

### Downstream interface projections

MCP, CLI, HTTP, tests, or application code drive the same typed service. Each
projection owns presentation and protocol concerns such as JSON Schema,
structured protocol errors, stdout/stderr policy, authentication, and transport
lifetime. `tower-agent` does not select or wrap an MCP library.

## Core service contract

The exact names may change during the spike. The ownership and semantic shape
should not.

### Request envelope

Every operation uses a common local envelope around a typed body:

```rust
pub struct AgentRequest<T> {
    pub context: CallContext,
    pub body: T,
}
```

The first body is one finite turn:

```rust
pub struct Turn<O = ()> {
    pub prompt: String,
    pub working_directory: Option<PathBuf>,
    pub session: Option<SessionHandle>,
    pub options: O,
}
```

The concrete service identifies its provider. Routing among providers belongs
in another service above the finite turn. Provider-specific controls live in
the generic `O` options type rather than a universal flag list.

`Turn` is inspectable, but serialization is not part of the core contract.
Protocol adapters own portable DTOs that project only the fields they can
represent safely. Secrets, open channels, process handles, clocks, transport
endpoints, and provider-private session values do not belong in those DTOs.

`CallContext` is local launch state and is intentionally not serialized:

```rust
pub struct CallContext {
    pub operation_id: OperationId,
    pub deadline: Option<Instant>,
    pub cancellation: CancellationToken,
    pub events: EventObserver,
}
```

Not every field must ship in the first slice. The split is load-bearing:
portable intent is separate from host-local launch authority.

The initial implementation should include:

- operation identity;
- an optional deadline;
- a cancellation token;
- an owned event observer;
- typed turn intent.

Ancestry, credential injection, and extensible local values can be added when a
concrete layer needs them.

### Response

```rust
pub struct TurnOutcome {
    pub output: String,
    pub session: Option<SessionHandle>,
    pub usage: Option<TokenUsage>,
    pub cost: Option<Cost>,
    pub duration: Option<Duration>,
    pub provider_turns: Option<u32>,
}
```

Missing telemetry means unreported, never zero. A provider must not estimate
authoritative cost if it only reports tokens.

### Service shape

Conceptually:

```rust
impl Service<AgentRequest<Turn>> for ClaudeService {
    type Response = TurnOutcome;
    type Error = AgentError;
    type Future = ...;
}
```

The crate may provide a trait alias pattern and a boxed form:

```rust
pub type BoxTurnService = BoxCloneSyncService<
    AgentRequest<Turn>,
    TurnOutcome,
    AgentError,
>;
```

The concrete alias must require a `Send` future. Service clones used by a host
must share admission, budget, and circuit state where those policies are meant
to be global. A clone must not accidentally create an independent semaphore or
budget.

### Descriptor (deferred)

Execution and discovery travel together without changing Tower's trait:

```rust
pub struct ServiceDescriptor {
    pub id: ServiceId,
    pub operation: OperationDescriptor,
    pub effects: EffectClass,
    pub capabilities: Capabilities,
}

pub struct AgentEndpoint<S> {
    pub descriptor: ServiceDescriptor,
    pub service: S,
}
```

Provider capability absence must cause pre-execution refusal when requested.
It must not silently weaken authority, limits, or output requirements.

The core descriptor is protocol-neutral. A downstream adapter may generate
JSON Schema from the typed request and response, use adapter-owned DTOs, or
publish no schema if its protocol does not require one. JSON Schema support may
be offered behind a serialization-oriented feature, but it is not part of the
execution contract and must not pull an MCP library into the crate.

## Readiness, admission, and load shedding

Tower readiness is meaningful for agent work.

- A bare Tower concurrency limit uses `poll_ready = Pending` when it has no
  capacity, allowing a caller to wait.
- `AdmissionLayer` deliberately adds immediate load shedding above that limit:
  its own `poll_ready` reports ready and `call` returns typed `busy` when the
  inner limit is unavailable.
- A queue is a separate explicit layer.

The reference single-agent host should provide immediate `busy`, because a
second prompt is neither steering nor automatically the next conversation
turn.

Standard Tower `LoadShed` and `ConcurrencyLimit` may be usable internally, but
their errors must be normalized into `AgentError::Busy` rather than escaping as
opaque boxed errors. Middleware ordering and clone behavior must be covered by
concurrency tests.

## Events and streaming

A response future alone does not model incremental provider observations.
Making `Response = Stream<Item = Event>` is also insufficient because callers
still need one typed terminal outcome and drop behavior becomes ambiguous.

The request therefore owns an observer:

```rust
#[derive(Clone)]
pub struct EventObserver(Arc<dyn EventSink>);

pub trait EventSink: Send + Sync {
    fn try_emit(&self, event: AgentEvent) -> Result<(), EventSendError>;
}
```

The initial normalized event set should remain small:

```rust
pub enum AgentEvent {
    Started,
    OutputDelta { text: String },
    ThinkingDelta { text: String },
    ToolStarted { name: String },
    TurnStarted { number: u32 },
    Status { message: String },
    Usage { usage: TokenUsage },
    Warning { message: String },
}
```

Lifecycle owners, not providers, emit authoritative start, completion,
cancellation, and failure records. Provider adapters emit only observations
they actually receive.

An observer implementation may:

- discard events;
- send them to a bounded channel;
- append them to a journal;
- forward them as MCP progress notifications;
- broadcast them to several consumers;
- decorate them with timing or operation identity.

Backpressure policy is explicit. A provider parser must not deadlock because a
slow UI stopped reading events. The default channel observer should be bounded
and define whether it drops deltas, coalesces them, or records truncation.
Terminal settlement must never be dropped.

Event observation is best-effort by default. A host may install a mandatory
audit sink that fails admission if it cannot record, but it must choose that
policy explicitly.

## Cancellation and task ownership

There are three distinct cancellation paths:

1. The caller drops the service future.
2. The caller signals the request's cancellation token.
3. A higher-level control operation cancels an active task.

All three must converge on one provider cancellation mechanism.

For a supervised directly owned call:

- `SuperviseLayer` moves the complete inner future into an owned task;
- dropping the interface future cancels the token but does not abort that task;
- the task retains admission capacity and continues polling the provider
  through cleanup and settlement;
- `DeadlineLayer` cancels at the deadline and waits for the same settlement;
- the provider implementation remains responsible for terminating its complete
  owned process group when cancellation is signalled.

Supervision prevents accidental detachment of the Rust future. It cannot make a
provider that ignores cancellation safe, and it cannot replace correct process
ownership in a wrapper.

For a detached MCP Task, the task host explicitly owns the service future.
Dropping the original request handler does not cancel it. `tasks/cancel` signals
the same cancellation token, waits for provider settlement, and only then marks
the MCP task cancelled.

No layer may spawn work and discard the `JoinHandle` unless another observable
owner guarantees settlement. A panicking provider or coordinator must resolve
to a terminal failure rather than leave readiness permanently occupied.

## Error and effect model

Middleware needs more than a string and category to act safely.

```rust
pub struct AgentError {
    pub kind: ErrorKind,
    pub message: String,
    pub phase: FailurePhase,
    pub effects: EffectState,
    pub cause: Option<Box<AgentError>>,
}
```

The typed cause chain lets a policy failure retain stronger settlement evidence.
For example, a deadline remains the outer error while the provider's terminal
failure and reported-effect state remain inspectable. Retry advice, partial
session evidence, and accounting may be added when a native provider
demonstrates how they can be populated honestly.

Suggested stable categories:

```rust
pub enum ErrorKind {
    InvalidRequest,
    Authentication,
    Unauthorized,
    Unsupported,
    Busy,
    DeadlineExceeded,
    Cancelled,
    Budget,
    Limit,
    Provider,
    Internal,
}
```

Suggested execution phases:

```rust
pub enum FailurePhase {
    Admission,
    Validation,
    Launch,
    Running,
    Settlement,
}
```

Suggested effect evidence:

```rust
pub enum EffectState {
    None,
    Possible,
    Reported,
}
```

The key rule is conservative:

> If execution may have reached the model or one of its tools, effects are
> `Possible` unless the provider supplies stronger evidence.

Retry middleware may retry automatically only when the error says effects are
`None` and retry advice permits it. An authentication failure, pre-spawn
connection refusal, or verified provider-overload response may qualify. A
timeout during a workspace-writing turn does not.

Fallback to another provider follows the same rule and additionally requires a
fresh or explicitly portable session. A Claude session token is not a Codex
session token.

## Authority model

The request declares desired authority, and the provider service must either
enforce or refuse it.

At minimum:

```rust
pub enum AuthorityRequest {
    ReadOnly,
    WorkspaceWrite,
    UnattendedWorkspaceWrite,
}
```

Additional external-write capabilities should be explicit and scoped rather
than folded into a single `dangerous` boolean.

An `AuthorityLayer` may narrow a request based on host policy, caller identity,
workspace, or operation ancestry. It must never broaden authority. Provider
adapters validate that their actual CLI flags satisfy the resulting posture
before spawning.

MCP Tool annotations are descriptive hints, not enforcement. Authorization and
authority checks remain service layers or transport policy.

## Middleware opportunities

The purpose of a Tower-native kernel is not merely to replace `async_trait`
with `Service`. It is to make these mechanics independently reusable,
orderable, and testable.

| Layer | Responsibility | Important constraint |
|---|---|---|
| `OperationLayer` | Mint or validate operation identity | Identity remains stable through retries/attempts; attempts get sub-ids |
| `AncestryLayer` | Propagate parent, depth, visited endpoints, delegated budget | Required before agent-to-agent calls; reject loops and depth overflow |
| `TraceLayer` | Structured spans and correlation | Redact prompts, credentials, and session tokens by default |
| `ReceiptLayer` | Record admission, terminal result, usage, cost, timing | Must record typed failure without delaying cancellation indefinitely |
| `AuthorityLayer` | Validate and narrow requested authority | Never broadens; unsupported enforcement fails before spawn |
| `AdmissionLayer` | Single-flight or bounded concurrency | Default agent host load sheds; no implicit queue |
| `RateLimitLayer` | Bound calls over time | Refusal is typed and includes retry timing when known |
| `BudgetLayer` | Reserve and account cost/tokens/turns | Reservation and terminal accounting must survive cancellation races |
| `DeadlineLayer` | Impose a wall-clock deadline | Timeout must cancel and drain the provider process group |
| `ContextLayer` | Deterministically inject instructions, aliases, project context | Preserve provenance and ordering; apply once per resumed turn |
| `OutputContractLayer` | Validate structured output | Never report success with invalid required output |
| `EventLayer` | Decorate, fan out, journal, or redact events | Slow observers cannot wedge the provider parser |
| `RetryLayer` | Retry safe pre-effect failures | Requires `EffectState::None`; never blindly retries a timed-out writer |
| `FallbackLayer` | Select another compatible provider | Requires no possible effects and portable/fresh session semantics |
| `CircuitBreakerLayer` | Stop sending calls to a failing provider | Provider health state is shared across service clones |
| `RedactionLayer` | Remove secrets from errors, events, and receipts | Raw command lines and environment must not escape |

### Reference ordering

Layer order changes semantics. A reference stack should document an explicit
outside-to-inside order:

```text
supervision / caller-drop ownership
  trace
    receipt observation
      panic normalization
        ancestry / loop prevention
          caller authorization
            admission / load shedding
              deadline and cancellation guard
                budget reservation
                  deterministic context assembly
                    authority enforcement
                      output contract
                        turn validation
                          event observation
                            provider service
```

This is a starting point, not an immutable universal ordering. Tests must state
which calls are observed and charged. For example:

- Receipt observation stays inside supervision so it records the retained
  terminal settlement after an interface caller disappears.
- Panic normalization stays inside receipt observation so call, readiness, and
  response-future panics become typed failures before the receipt is recorded.
- A receipt outside admission sees refused calls as well as executed calls.
- Admission before budget means a busy call does not reserve spend.
- Admission must wrap deadline short-circuiting so an already-expired request
  releases the readiness permit acquired by the concurrency limiter.
- Context assembly before provider validation lets the adapter validate the
  complete effective request.
- The deadline wraps output validation so malformed terminal output cannot
  evade the caller's deadline.

Retry and fallback are omitted from the default stack. A host installs them
only around a service whose error/effect contract makes them safe.

## Middleware that is unsafe by default

Several common Tower patterns require explicit rejection or adaptation:

### Buffering

`BufferLayer` creates a queue. That changes the meaning of a second prompt,
extends cancellation lifetime, and can make a stale request execute much later
under changed workspace state. Do not install it in the reference host.

### Retry

An agent call is not generally idempotent. A provider failure after file edits
cannot be retried safely merely because it has a transient error code.

### Fallback

Starting the same prompt on another provider can duplicate effects. Resuming
provider-private sessions across providers is invalid unless an explicit
portable context mechanism exists.

### Caching and coalescing

Prompt equality does not imply result equality. Workspace, time, ambient tools,
session state, and external services all affect execution. The default effect
class is non-cacheable and non-coalescible.

### Generic timeout

A timeout that only drops an outer future is incorrect if the wrapper leaves a
child or descendant process running. Use an agent-aware deadline layer backed
by a tested provider cancellation contract.

## Provider services

Initial concrete services live outside the core crate:

```text
tower-agent-claude  -> claude-wrapper
tower-agent-codex   -> codex-wrapper
```

Their release-target responsibilities are deliberately narrow:

- map provider-neutral intent to supported provider flags;
- reject unsupported or unenforceable settings before spawn;
- use stdin for prompts where the provider supports it;
- parse streaming events into normalized observations;
- retain session evidence on success and recoverable failure;
- classify provider-native failures conservatively;
- kill the owned process group on timeout, cancellation, or future drop;
- report capability metadata honestly.

Wrapper crates remain the best home for reusable provider-process mechanics.
The initial service adapters should consume wrapper APIs. Only after the service
contract is proven should wrapper crates consider optional native Tower
implementations.

### Backend compatibility adapter

To compare architectures without rewriting the repository, preserve one
temporary migration direction:

```text
existing Backend -> BackendService adapter
```

This is deliberately conservative and lossy. It preserves the owned prompt,
normalized streaming observations, terminal reply, cost, and provider-tagged
session handle. The legacy error is only a string, so the bridge can only add
conservative phase/effect evidence. It drops legacy summary and bus-post data,
and it cannot add cancellation or process ownership that the old provider
future does not implement. Its tests document that limited contract. A reverse
adapter is unnecessary unless a concrete legacy host needs one.

The comparison should measure whether policy can move out of `Server::run_with`
cleanly, not claim that the two contracts are equivalent.

## Hot logical agents and sessions

The finite service does not itself imply a persistent process or session pool.

A hot `AgentInstance<S>` may wrap a turn service and own:

- one immutable configuration/template;
- one retained provider session handle;
- zero or one active call;
- monotonically increasing operation identity;
- latest terminal result;
- optional bounded event replay;
- `Idle`, `Running`, `Stopping`, and `Stopped` state.

Each prompt creates a new finite call. On settlement, the instance retains
session evidence from success or failure. A failed turn returns the logical
agent to idle. Only explicit shutdown permanently stops it.

`AgentInstance<S>` can implement `Service<AgentRequest<Turn>>`, but its clones
must share one state machine. `poll_ready` and load shedding expose its
single-flight policy.

Control operations such as status, steer, interrupt, and shutdown are separate
typed services or host methods. A second prompt is never silently interpreted
as steering.

Session continuity is provider-specific evidence held behind a
provider-neutral handle. Missing session evidence on a resumed call preserves
the already-known handle; a handle from a different provider is never
installed.

## Downstream MCP projection

This section is non-core integration guidance. It does not assign MCP behavior
or dependencies to `tower-agent`.

`tower-mcp` already represents Tools and Resources as Tower services, so one
possible downstream adapter should be thin:

```text
JSON/schema extraction
  -> construct local CallContext
  -> call typed Agent service
  -> map terminal outcome to text + structuredContent
  -> map AgentError to isError + typed structuredContent
```

JSON-RPC errors are reserved for malformed protocol input, authorization at the
transport boundary, missing protocol capabilities, and server faults. Provider
and domain failures are successful MCP calls whose Tool result has
`isError: true` and a typed structured body.

The downstream adapter owns JSON Schema and all MCP wire behavior. Core service
types may optionally derive serialization/schema traits behind independent
features, but the kernel must not depend on MCP wire types or an MCP crate.

Nothing in the service contract requires `tower-mcp` specifically. Another MCP
library can perform the same mapping by calling the public Tower service.

### Composition proof

The intended `tower-agent` plus `tower-mcp` composition is close to mechanical.
Tower MCP's public bridge for typed state is an extractor handler:

```rust
let agent = BoxTurnService::new(
    ServiceBuilder::new()
        .layer(AdmissionLayer::single_flight())
        .layer(DeadlineLayer::new())
        .service(EchoService),
);

let prompt = ToolBuilder::new("prompt")
    .extractor_handler(
        agent,
        |State(agent): State<BoxTurnService>,
         context: Context,
         Json(input): Json<PromptInput>| async move {
            let (request, operation_id) = adapt_request(input, context);
            let result = agent.oneshot(request).await;
            Ok::<_, tower_mcp::Error>(adapt_result(result, operation_id))
        },
    )
    .build();
```

`adapt_request` and `adapt_result` belong to the example or consumer. They are
not `tower-agent` APIs tied to MCP.

The adapter has exactly four responsibilities:

1. Decode protocol input into the typed request body.
2. Translate MCP request cancellation and progress into local `CallContext`.
3. Await the service future, or transfer its ownership to an MCP Task.
4. Encode the typed outcome or failure as MCP content and structured content.

It must not reimplement admission, session continuity, provider cancellation,
deadlines, event ordering, or terminal settlement.

The composition is a design test. A prompt Tool adapter should remain small,
obvious, and free of lifecycle state. If the example needs a substantial server
abstraction, the `tower-agent` contract is missing a general primitive.

### Ordinary calls

An ordinary Tool call awaits the finite service response. Its text content is
the assistant answer. The example's structured content contains operation
identity, normalized output, usage, cost, timing, provider turn count, and
redacted session presence. Provider-private session values stay local; a host
that supports continuation must mint and authorize its own public handle.

### MCP Tasks

The target Task-aware call transfers ownership of the service future to the
task host. Task cancellation must signal the call token and wait for provider
settlement. The task store should not create a second run registry unless
durable task recovery is explicitly required. This path is deferred because
the currently locked Tower MCP Task implementation does not connect Task
cancellation to the running Tool request.

### Progress

An MCP event observer converts output deltas and selected status events into
progress notifications. Progress is an observation of the same service call,
not a different streaming execution path.

### Resources

A hot host may expose state and replay through Resources. Static service
descriptors may also be resources. Resource handlers read shared service state;
they do not mutate the provider.

## Other agent functions

Prompt is the first and most important body type, not a permanent restriction.
Other functions can use the same envelope:

```rust
Service<AgentRequest<InspectRepository>>
Service<AgentRequest<ReviewPatch>>
Service<AgentRequest<Status>>
Service<AgentRequest<Interrupt>>
```

Generic middleware can operate on `AgentRequest<T>` when it only needs common
context. Function-specific middleware remains typed over `T`.

Do not force every function into one giant `AgentCall` enum merely to store
heterogeneous services in a vector. Type erasure belongs at a registry or
protocol projection boundary. Each MCP Tool can retain its own typed service
internally.

An extension package may export an `AgentEndpoint<S>` or a Tower MCP router
fragment. Composition must fail on name collisions. Last-writer-wins tool
replacement is unsafe for host or security capabilities.

## The agent as a client

A provider model may receive a role-scoped MCP view of the same harness. This is
an application of the service architecture, not a special provider protocol.

The provider-facing projection should normally include:

- inspectable context and resources;
- extension tools such as Git or GitHub operations;
- event/status views that are safe for the model;
- explicitly delegated control tools.

Calling the same instance's prompt service while that instance is already
running should return `busy`; it must not recurse accidentally. The model-facing
projection therefore need not expose self-prompt by default.

Calling another agent service is different. It requires explicit delegated
authority and call ancestry.

## Future agent-to-agent calls

Roba-to-Roba or tower-agent-to-tower-agent communication is not required for
the first implementation, but it reveals useful context fields:

- stable caller and callee endpoint identity;
- parent operation id;
- depth and visited endpoint set;
- delegated authority;
- remaining deadline and budget;
- trace and receipt correlation;
- explicit session ownership.

An ancestry layer can reject loops, enforce maximum depth, and prevent a child
from receiving more authority or budget than its parent. These values should be
designed as local context even if federation is deferred.

Do not add a multi-agent scheduler or bus to the kernel to enable this. One
service can simply be the typed or MCP client of another service.

## Proposed crate boundary

The `tower-agent` crate is useful on its own. Its required dependency direction
is:

```text
application / CLI / MCP example or consumer
                 |
                 v
            tower-agent

provider adapters ----> tower-agent
```

There is no arrow from `tower-agent` to an MCP crate.

The repository already has a workspace outline, but the responsibilities should
become sharper:

```text
crates/tower-agent
    request/response/error/event vocabulary
    boxed service types
    core middleware layers
    fake/echo service for deterministic tests

crates/tower-agent-server
    preserved pre-pivot server and temporary BackendService adapter

crates/tower-agent-claude
    ClaudeService over claude-wrapper

crates/tower-agent-codex
    CodexService over codex-wrapper

crates/agent
    reference CLI composition
```

The initial repository contains no production `tower-agent-mcp` companion.
Instead:

```text
crates/tower-agent/examples/tower_mcp_prompt.rs
    copyable guide showing one ordinary prompt Tool

crates/tower-agent/tests/tower_mcp_composition.rs
    executable confirmation of the input schema, results, typed errors, and
    middleware receipts through an ordinary Tool call
```

`tower-mcp` may be a development dependency used by these examples and tests.
It must not be a normal, optional, or feature-activated dependency of
`tower-agent`. Examples and tests are the integration guide and validation
surface, not reusable production glue.

If several real consumers later duplicate meaningful adapter logic, that is
evidence for a separate companion crate. Do not predict that need now.

## Target safety invariants

These are release targets for native provider services. The deterministic fake
currently proves the service and middleware portions. Invariant 3 is not yet
claimed for the locked real-provider wrappers. Invariant 11 is also not yet
claimed for arbitrary provider diagnostics; the native adapters redact known
command and authentication failures, but complete diagnostic redaction still
needs adversarial fake-binary tests.

The following are release-blocking invariants for the service kernel:

1. No provider work starts before complete validation and authority checks.
2. A provider setting is honored or refused, never silently weakened.
3. Dropping a directly owned call cannot orphan provider descendants.
4. An explicit cancellation settles before capacity is reported available.
5. Service clones share state for policies documented as global.
6. The default single-agent composition has no hidden queue.
7. A typed failure category survives every layer and protocol projection.
8. Missing telemetry remains absent rather than becoming zero.
9. Retry and fallback cannot run when effects may have occurred.
10. Slow or disconnected event consumers cannot wedge terminal settlement.
11. Credentials, provider-private session values, and provider-private launch
    context are not serialized into wire requests, events, schemas, receipts,
    or errors.
12. Operation identity and ancestry cannot be replaced by an inner service.
13. Middleware cannot broaden delegated authority.
14. A panic or aborted coordinator releases admission capacity and produces a
    terminal failure.

## Test strategy

The project should treat middleware behavior as a conformance contract, not
just implementation detail.

### Deterministic fake service

Build a controllable fake that can:

- record full owned requests;
- block at admission, launch, running, and settlement barriers;
- emit events;
- return success with or without a session;
- fail in each phase with configurable effect evidence;
- rotate a session handle;
- panic;
- expose a drop probe confirming cancellation;
- report usage and cost.

Use semaphores or barriers rather than sleeps for race tests.

### Service law tests

- A bare concurrency limit's `poll_ready` capacity matches actual admission.
- The immediate load-shed wrapper deliberately reports ready and returns typed
  `Busy` from `call` while that underlying capacity is occupied.
- Calling without readiness is either supported deliberately or fails loudly.
- Clones share single-flight, rate, budget, and circuit state as documented.
- A completed or cancelled call releases capacity exactly once.
- Dropping the returned future triggers cancellation and cleanup.
- A panicking inner service does not wedge capacity.

### Middleware ordering tests

- Busy calls are observed but do not reserve budget in the reference stack.
- Invalid authority fails before provider launch.
- Context is injected exactly once per call and preserves provenance.
- Timeout cancels and drains before returning.
- Receipts contain terminal typed failure and session evidence.
- Output validation can convert an otherwise successful provider result into a
  typed settlement failure.
- Event decoration retains ordering and operation identity.

### Retry and effect tests

- A pre-launch transient failure with `EffectState::None` may retry.
- A running timeout with `EffectState::Possible` never retries.
- Fallback refuses a provider-private resume session.
- Attempt ids differ while the parent operation id remains stable.

### Provider adapter tests

Use fake binaries to assert:

- exact fresh and resume arguments;
- prompts use stdin where supported;
- authority posture;
- unsupported controls fail before spawn;
- event normalization;
- success and failure session evidence;
- timeout and cancellation kill the process group;
- secrets do not appear in arguments or rendered errors.

Live tests are ignored, opt-in, inexpensive, and assert mechanics rather than
model compliance.

### Tower MCP composition tests

Use the dev-only `tower-mcp` dependency and its in-process test client. The
current integration test exercises public `tower-agent` APIs exactly as an
external consumer would:

- The adapter-owned Tool input schema requires `prompt`.
- Success returns answer text plus structured output, telemetry, provider turn
  count, and redacted session presence without the private session value.
- Typed domain failure returns `isError: true` plus stable kind, phase, effect,
  message, and recursive cause evidence in structured content.
- No client parses human error text.
- Existing Tower middleware still surrounds the call when invoked through MCP.
- The adapter contains no lifecycle state or provider-specific behavior.

The copyable example additionally shows how an ordinary call maps local events
to progress and maps request cancellation into the call token while retaining
the service future until settlement. Future test increments may exercise those
paths, output schemas, disconnect behavior, Resources, and MCP Tasks. None of
those are part of the current executable proof. In particular, the locked
Tower MCP Task implementation does not connect Task cancellation to the
running Tool request.

The corresponding example should be intentionally readable and copyable. It
is not a hidden test harness and does not depend on private crate APIs.

## Phased implementation plan

These are evidence workstreams rather than a strict waterfall. Small downstream
spikes may run early when they test an architectural seam, but no workstream is
complete until its own deliverables and exit evidence are explicit. Each
implemented increment ends with formatting, clippy with warnings denied, unit
tests, integration tests, documentation tests, and a clean diff.

### Phase 0: inventory and decision record (complete)

Deliverables:

- Record the current `Backend`, `Server::run_with`, provider adapter, event,
  cancellation, and MCP behavior.
- Decide initial request, response, event, and error names.
- Decide the minimum supported Rust/Tower/Tower MCP versions. The first spike
  records Rust 1.90, Tower 0.5, and Tower MCP 0.13 for its dev-only ordinary
  Tool proof; MCP Task support has no selected baseline yet.
- Mark this service-kernel design as proposed or adopted.

Exit criterion:

> The new kernel has a precise compatibility boundary with the existing
> implementation and no behavior has changed accidentally.

### Phase 1: the service atom (implemented subset; descriptor deferred)

Deliverables:

- `AgentRequest<Turn>` and local `CallContext`.
- `TurnOutcome`, `AgentError`, effect/phase evidence, and normalized events.
- boxed service type;
- service descriptor (deferred until discovery has a concrete consumer).
- deterministic fake or echo service.
- adapter from the existing `Backend` trait to the new service.

Required proof:

- owned request and observer;
- readiness exercised through `ServiceExt`;
- success, failure, events, and session evidence round-trip;
- no MCP dependency in the kernel path;
- dropped call has explicit tested behavior.

Exit criterion:

> One finite fake agent turn runs entirely through the Tower service contract.

### Phase 2: representative middleware (core subset complete)

Deliverables:

- typed single-flight admission/load shedding;
- observation/receipt layer;
- deadline/cancellation layer;
- authority validation layer (deferred until an authority request type is
  justified);
- explicit reference stack builder (deferred; the current reference ordering
  is documented and assembled directly with `ServiceBuilder`).

Required proof:

- exact middleware order;
- concurrent call returns typed busy without queuing;
- timeout drains the fake provider before admission capacity becomes reusable;
- cancellation and completion races settle once (simultaneous-readiness proof
  deferred; current tests exercise each path deterministically);
- clones share intended state;
- no typed error information is erased.

Exit criterion:

> Tower composition provides measurable clarity over `Server::run_with`, not
> merely different syntax.

### Phase 3: provider adapters (native call subset complete; cleanup blocked)

Deliverables:

- `ClaudeService` over the current supported `claude-wrapper`.
- `CodexService` over the current supported `codex-wrapper`.
- preflight validation and conservative typed failures;
- host-owned provider configuration;
- honest descriptors (deferred);
- complete fake-binary process suites (deferred).

Exit targets, including blocked cleanup work:

- fresh/resume, events, permissions, limits, and terminal evidence;
- cancellation kills process groups;
- each provider rejects unsupported controls;
- provider errors populate phase and effect evidence conservatively.

The locked Claude wrapper can return from a failed buffered-stdin write without
proving that its direct child was killed and reaped. The current failure ABI
also has no place for the session, spend, and turn count carried by Claude's
recoverable max-turn and max-budget failures. Process settlement and partial
failure evidence are therefore explicit blocked work, not properties of the
native subset.

Exit criterion:

> Claude and Codex satisfy the same service contract without pretending to
> have identical capabilities.

### Phase 4: downstream MCP proof (ordinary Tool subset complete)

Deliverables:

- a copyable `tower_mcp_prompt` example using only public APIs;
- a `tower_mcp_composition` integration test using an equivalent public-API
  mapping (sharing one adapter implementation is deferred);
- ordinary call and structured error mapping;
- in-process client path;
- progress event bridge (example only; integration test deferred);
- optional MCP Task path with correct cancellation (deferred; the currently
  locked Tower MCP Task path does not propagate cancellation into running tool
  work).

Exit targets, including deferred work:

- actual client schema and structured result tests;
- byte-clean display/result separation;
- typed failures survive the wire projection;
- ordinary and Task calls own cancellation correctly;
- middleware behavior is unchanged through the MCP adapter;
- `tower-mcp` is present only as a development dependency;
- no reusable MCP adapter is added to production code.

Exit criterion:

> The same service is directly callable from Rust and through MCP with no
> semantic fork, while `tower-agent` itself has no MCP dependency.

### Phase 5: hot single-agent host (deferred)

Deliverables:

- optional `AgentInstance<S>` retaining one session;
- single-flight readiness and typed busy;
- repeated finite turns;
- status, interrupt, and shutdown services/resources;
- bounded cross-turn event observation if demanded by a client.

Required proof:

- construction starts no provider work;
- turn two resumes turn one's session;
- failure does not kill the host;
- interruption leaves it reusable;
- shutdown never reopens after a completion race;
- dropped interface requests do not wedge the host.

Exit criterion:

> One hot logical agent serves repeated prompts while every provider operation
> remains a finite Tower service call.

### Phase 6: evaluate migration and advanced layers (deferred)

Only after the previous proof, decide whether to migrate or rebuild existing
sessions, budgets, scheduling, bus, and observability features.

Candidates:

- deterministic context and alias layers;
- structured output contracts;
- provider health/circuit breaking;
- Git/GitHub MCP extension packages;
- provider-facing scoped MCP projection;
- explicit agent-to-agent calls with ancestry and delegated budgets.

Retry, fallback, queues, and fleet behavior require separate safety evidence.

## Initial research questions

The first implementation session should resolve these through small executable
tests rather than prolonged abstraction design:

1. Should `EventObserver` be a synchronous `Arc<dyn EventSink>`, a bounded
   sender, or a small enum supporting both? The provider parser must never
   block indefinitely.
2. Should cancellation use `tokio_util::sync::CancellationToken`, a crate-local
   abstraction, or both? Verify future-drop cleanup before choosing.
3. Can standard Tower concurrency/load-shed layers preserve the desired shared
   clone state and typed errors, or is a small agent-aware layer clearer?
4. What is the smallest error effect model that safely controls retry and
   fallback?
5. Does the service descriptor belong beside each service value or on a
   factory that builds the fully layered endpoint?
6. Is `AgentInstance` best represented directly as a Service or as a host that
   vends a service handle?
7. Which existing `Server::run_with` responsibilities become layers, which
   remain host state, and which should stay outside the new kernel entirely?
8. Can `Backend` compatibility adapters preserve streaming and cancellation
   well enough for an incremental migration?
9. Should JSON Schema support be an optional serialization feature, or should
   the Tower MCP example own wire DTOs entirely?
10. Which Tower MCP release and Task semantics are the supported baseline?

### Answers from the first spike

- `EventObserver` wraps a nonblocking `EventSink::try_emit`; a bounded channel
  implementation reports full or closed without blocking provider parsing.
- Cancellation uses `tokio_util::sync::CancellationToken` and
  `SuperviseLayer` retains ownership after caller drop.
- `AdmissionLayer` uses Tower's shared concurrency primitive with an
  agent-aware typed load-shed boundary.
- The minimal implemented error evidence is kind, phase, and effect state.
  Retry advice remains deferred with retry itself.
- Descriptors are deferred until discovery has a concrete consumer.
- The one-way compatibility adapter preserves the owned prompt, normalized
  events, terminal reply, cost, and provider-tagged session evidence. It drops
  legacy summary and bus-post data, and it cannot add cancellation or precise
  effect evidence that the old backend lacks.
- The MCP example owns its DTO and schema entirely.
- The current ordinary Tool projection is supported. MCP Tasks remain deferred
  until task cancellation is connected to the running call.

## Recommended first spike

Keep the first code change intentionally small:

1. Add the owned request envelope, typed outcome/error, observer, and
   `EchoService`.
2. Implement one `BackendService<B>` adapter around the existing `Backend`.
3. Add an agent-aware single-flight layer and deadline/cancellation layer.
4. Exercise them through direct Tower calls only.
5. Add one dev-only Tower MCP example and integration test after the direct
   service tests are green. Add no production adapter.

This initial spike is complete. It demonstrates the intended dependency
inversion and shows concrete middleware value. Adoption remains provisional
for native providers until subprocess cancellation is proved against fake
binaries.

The spike succeeds if it demonstrates all of the following:

- middleware can observe and constrain the same provider call without
  duplicating lifecycle code;
- a second call is typed busy rather than queued;
- dropping or timing out a cooperatively cancellable fake call provably
  releases its work and capacity;
- bounded event observation remains available, while provider-level
  incremental streaming is deferred where the safe wrapper path is buffered;
- errors remain typed;
- the MCP adapter is materially thin;
- the example is sufficient documentation for an external consumer;
- `cargo tree -p tower-agent --edges normal` contains no MCP implementation
  crate.

If it cannot demonstrate those properties with less coupling than the current
`Backend` plus `Server::run_with` design, stop and record why. The point is not
to make Tower mandatory. The point is to discover whether Tower gives agent
work a better reusable execution substrate.

## Adoption criteria

Adopt the Tower-native kernel only if the spike shows:

- less duplicated lifecycle and policy code across direct, CLI, and MCP calls;
- clearer ownership of cancellation and subprocess cleanup;
- meaningful reusable middleware with explicit ordering;
- no loss of provider capability honesty;
- no erosion of typed error or session evidence;
- deterministic tests for concurrency and cancellation;
- a simpler provider adapter story for both Claude and Codex.

Do not adopt it solely because `Service` is aesthetically appealing. Its value
must appear in correctness, composition, and testability.
