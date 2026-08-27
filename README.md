# tower-agent

[![CI](https://github.com/joshrotenberg/tower-agent/actions/workflows/ci.yml/badge.svg)](https://github.com/joshrotenberg/tower-agent/actions/workflows/ci.yml)
[![Rust 1.90+](https://img.shields.io/badge/rust-1.90%2B-93450a.svg)](https://www.rust-lang.org)

Tower-native services and middleware for finite agent operations.

A provider implements `tower::Service` for an owned agent request. Applications
compose execution policy with ordinary Tower layers, then call the service from
whatever interface they need.

```text
interface
    │
policy layers
    │
provider service
```

## Status

This is an experimental `0.1` workspace. The request, outcome, failure, event,
and middleware contracts are typed and tested. API stability is not yet a goal.

## The service atom

One finite turn is the initial operation:

```rust
use tower::ServiceExt;
use tower_agent::{AgentRequest, EchoService, Turn};

let outcome = EchoService
    .oneshot(AgentRequest::new(Turn::new("inspect this repository")))
    .await?;

assert_eq!(outcome.output, "inspect this repository");
```

`AgentRequest<T>` separates the operation body from local call state such as
identity, cancellation, deadline, and event observation. `Turn<O>` carries
common turn data while leaving provider controls in the generic `O` options
type. The concrete service identifies the provider.

Terminal state is explicit:

- `TurnOutcome` carries output plus optional session, token, cost, duration,
  and provider-turn evidence.
- `AgentError` preserves failure kind, execution phase, effect state, causal
  settlement, and partial evidence from failed provider calls.
- `AgentEvent` carries nonblocking incremental observations without changing
  the terminal response into a stream.

Missing evidence stays absent. Provider session handles redact their values in
`Debug`; an interface must mint its own public continuation identifier before
exposing one.

## Middleware

The current layers cover the places where agent calls differ materially from
ordinary request/response RPCs:

| Layer | Behavior |
|---|---|
| `ValidateTurnLayer` | Rejects invalid prompts before provider execution. |
| `AdmissionLayer` | Shares capacity across clones and returns typed `Busy` without a hidden queue. |
| `DeadlineLayer` | Signals cancellation, retains the call, and waits for provider settlement. |
| `ObserveLayer` | Records typed terminal receipts with stable operation identity. |
| `SuperviseLayer` | Keeps polling an owned call after its interface caller disappears. |
| `CatchPanicLayer` | Converts provider call panics into typed terminal failure. |
| `AuthorityLayer` | Rejects a turn requesting more filesystem authority than the host ceiling allows. |

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

Ordering is semantic. Supervision owns the call after caller drop. Panic
normalization sits inside observation so receipts see typed terminal failure.
Admission wraps deadline handling so capacity stays occupied through cleanup.

`AuthorityLayer` is host policy rather than call lifetime, so it goes inside
`ValidateTurn`, closest to the provider, and only for providers whose options
carry a portable filesystem-authority request. The Codex service repeats the
check at its own launch boundary, so omitting the layer cannot broaden
authority.

The next useful middleware seams are deterministic context assembly, budget
reservation and reconciliation, output-contract validation, event redaction
and fanout, and circuit breaking. Retry, fallback, buffering, caching, and
coalescing require stronger effect guarantees before they are safe;
[docs/resilience.md](docs/resilience.md) sets out what each policy needs
first and why `AgentRequest` is deliberately not `Clone`.

## Provider services

The workspace targets Claude and Codex, deliberately: two adapters that are
correct about cancellation, evidence, and bounded capture are worth more than
several that are approximately right. Nothing in the kernel assumes two, and
further providers are deferred rather than ruled out.

`ClaudeService` and `CodexService` implement the same owned Tower contract with
provider-specific option types. Both bridge request cancellation into the
wrapper execution future. Their wrappers own a process group on Unix and kill
the direct child on platforms without process groups.

Claude sends the user prompt over stdin; its system-prompt flags remain in argv.
Codex sends fresh and resumed prompts over stdin. Provider controls are
honor-or-refuse:
unsupported combinations fail before work starts.

`CodexAmbientContextPolicy::Automation` suppresses user config, execpolicy
rules, and project instructions for queued execution. A host may additionally
use `CodexSkillPolicy::DisableExact` to disable canonical skill folders for
fresh and resumed turns. Codex does not currently document a global skill
disable, so unlisted discovered skills, provider built-ins, and managed
instructions may remain.

Run the executable composition example with:

```text
cargo run -p agent-example -- --provider codex "inspect this repository"
```

Codex defaults to a read-only sandbox. `--workspace-write` is an explicit host
choice and is refused for providers that cannot enforce that exact request.
`AuthorityPolicy` is host-owned: `AuthorityLayer` rejects excessive requests
before provider work, and `CodexService` repeats the check at launch so layer
omission or ordering cannot broaden authority. Explicit writable roots must be
approved by the policy. Full filesystem access requires an explicit full-access
ceiling and is never enabled by default.

Claude tool allowlists remain provider-specific controls, not a portable
filesystem sandbox. The Claude service therefore does not claim conformance to
the filesystem-authority contract.

The [transport example](crates/tower-agent/examples/tower_mcp_prompt.rs) shows an
MCP adapter as downstream composition over the same typed service.

## Workspace

```text
crates/tower-agent          service types, fakes, and middleware
crates/tower-agent-claude   Claude provider service
crates/tower-agent-codex    Codex provider service
crates/tower-agent-plan     planning vocabulary and layered resolver (unpublished)
examples/agent              executable provider composition
```

Run the complete check suite with:

```text
just check
```

Each crate has its own README stating what that crate is responsible for.
The [architecture notes](docs/architecture.md) describe the service laws,
layer ordering, process lifecycle, and middleware roadmap, and
[docs/plan.md](docs/plan.md) records the planning decisions. See
[CONTRIBUTING.md](CONTRIBUTING.md) for local checks and change discipline.

The examples above are kept honest by the crate-level rustdoc, which compiles
equivalent code as doctests on every run.

## License

Licensed under either [MIT](LICENSE-MIT) or [Apache 2.0](LICENSE-APACHE), at
your option.
