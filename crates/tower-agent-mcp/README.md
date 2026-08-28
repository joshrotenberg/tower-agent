# tower-agent-mcp

MCP adapter vocabulary for `tower-agent`. The design record is
[`docs/mcp.md`](../../docs/mcp.md).

## Intention

A provider `SessionHandle` is private, provider-tagged, and never crosses a
protocol boundary. That rule is already enforced, and its consequence is that
a client is told a session exists and given no way to name it, so continuing a
conversation over a protocol is not merely unimplemented, it is
unrepresentable. An adapter has to mint a public name.

That name is a capability over conversation history, not a database key: a
resumed turn can read the prior context. So every store operation takes a
`Scope`, and the scope check, rather than the identifier, is the security
boundary.

- `Scope::Session` ends with the connection. Safe without configuration, and
  continuations do not survive a reconnect.
- `Scope::Principal` survives reconnects and requires an authenticated subject.
- The two are distinct variants because transport session ids and authenticated
  subjects are unrelated namespaces. Sharing one string type would let a
  session named like a principal resolve that principal's continuations.

An implementation that ignores the scope has built a bearer-token store, where
holding an identifier is enough to resume anyone's conversation. That is
reasonable for a single-user host and serious for a shared one, so it should be
chosen rather than inherited.

The adapter mints identity. The host owns persistence, which is the same split
`tower-agent-workflow` makes with its opaque jobs.

## The tool

`TurnTool` exposes one finite turn. It adds no execution policy: layers,
deadlines, and provider selection are composed into the service before it
arrives.

Constructing one requires naming a `ScopeSource`, with no default, because the
scope is what stops one caller continuing another's conversation and an adapter
that guessed would be guessing about that. `FixedScope::stdio()` is correct
where the transport carries one client per process and wrong everywhere else.

Three refusals never reach the provider: a request with no scope, a
continuation that does not parse, and a continuation that does not resolve in
this caller's scope. The last one matters most. An identifier that does not
resolve is refused rather than quietly starting a fresh conversation, because
a caller who asked to continue something would otherwise get a new conversation
that looks like success.

A store failure is treated differently. The turn has already run, so losing the
ability to name its session costs resumability and not the result: the
projection reports a session that is present with no continuation.

```sh
cargo run -p tower-agent-mcp --example stdio_server
```

## Planning, behind the `plan` feature

`PlanTool` accepts a fragment instead of a complete turn, resolves it against
host defaults, and asks the client for whatever is still unbound. Requirements
are already structured data, so the elicitation form is a rendering of them
rather than a translation. A provider requirement becomes a single-select
field, so a client cannot choose one no planner accepts.

The input is a small adapter-owned shape, not a `PartialTurn`. Defaults,
profiles, and provider baselines stay on the host, which is where the planning
crate's layering already put them.

Off by default, so the adapter's own surface does not depend on the planning
vocabulary. `plan-claude` and `plan-codex` forward to the planner features.

## Status

Experimental, at `0.1`, alongside the rest of the workspace. API stability is
not yet a goal.

The record is implemented: the continuation store, the projection, the turn
tool, progress reporting, and a planning tool behind the `plan` feature.

Progress indicates liveness, not content, and obeys the projection's redaction
policy rather than its own, because a notification reaches the same client the
result does.

`InMemoryContinuationStore` is the default for a host that has not chosen
otherwise. It is bounded and does not survive a restart.
