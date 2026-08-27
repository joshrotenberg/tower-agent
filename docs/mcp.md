# MCP adapter

Decision record for a proposed `tower-agent-mcp` crate. Nothing here is
implemented. The record exists so the identity and redaction choices are made
before code rather than discovered by it, because both are hard to change once
a client depends on them.

The adapter is one interface over the existing kernel. It adds no execution
semantics: policy layers, provider adapters, cancellation, and settlement stay
exactly where they are. What it adds is a public vocabulary, and the whole
design question is which parts of the private vocabulary may become public.

## What the composition test already settles

`crates/tower-agent/tests/tower_mcp_composition.rs` composes a `BoxTurnService`
into an `McpRouter` today. It is a test rather than a shipped adapter, but it
already fixes two rules that this crate inherits rather than revisits.

A provider session value never crosses the wire. The projection emits
`{provider, present}` and the test asserts both that no `value` key exists and
that the serialized payload does not contain the handle. `SessionHandle` is
provider-tagged and redacts in `Debug`; the transport projection is the second
place that must hold, and it does.

Provider-authored strings are untrusted for publication. The test's own failure
cause contains a session value inside a human-readable message, which is why
`public_error_message` collapses every failure to `agent operation failed
(kind)`. A provider decides what goes in its own error text, so an adapter that
forwards it verbatim has delegated its redaction policy to the provider.

The consequence is the gap this crate exists to close. A client is told a
session exists and given no way to name it, so **continuation over MCP is not
unimplemented, it is unrepresentable**. Adding it means minting a public
identity, which is the decision below.

## Three identities

```text
SessionHandle        provider-private, provider-tagged, never on the wire
MCP session          tower-mcp transport state, one connection
continuation id      minted by the adapter, public, the thing a client holds
```

`CLAUDE.md` already assigns the third to the adapter: transport DTOs, schemas,
and public continuation ids belong there. No type mints one yet. Conflating any
two of these is the failure mode the rest of this record guards against.

The MCP session is not the conversation. It ends when the connection ends,
while an agent conversation outlives it. The provider handle is not public. So
the continuation id is a distinct third thing, and its lifetime is a policy
choice rather than a consequence of either of the others.

## What scopes a continuation id

A continuation id resumes a conversation, and a resumed turn can read that
conversation's prior context. The id is therefore a capability over history,
not a database key, and scoping it is a security decision.

Three shapes, with what each costs:

| scope | survives reconnect | needs auth | failure mode |
|---|---|---|---|
| MCP session | no | no | continuations die with the connection |
| authenticated principal | yes | yes | unavailable until auth is configured |
| unbound bearer token | yes | no | the id alone resumes anyone's conversation |

**Decision: session-bound by default, principal-bound when auth is configured,
never unbound implicitly.**

The unbound form is rejected as a default because it is the one that looks
easiest. An id that is itself the credential must be treated as a secret, and
the natural place to return it is `structured_content`, which is exactly the
field clients log, cache, and persist. That combination turns an ordinary tool
result into a durable credential for someone else's history, and it does so
silently. A host that genuinely wants bearer semantics can supply a store that
implements them, but it states that intent rather than inheriting it.

Session-bound is the default because it is safe without configuration. Its cost
is real: continuations do not survive a reconnect, which is precisely the case
`docs/durable-host-report.md` cares about. That is the correct trade for a
default, not for a deployment, and it is why the store is injectable.

## The store belongs to the host

The adapter mints identity. It does not own persistence.

```rust
trait ContinuationStore {
    fn mint(&self, session: &SessionHandle, scope: Scope) -> ContinuationId;
    fn resolve(&self, id: &ContinuationId, scope: Scope) -> Option<SessionHandle>;
}
```

The default implementation is in-memory and session-scoped. A durable host
supplies its own, and in doing so chooses the lifetime and the scope check
together.

This mirrors two decisions already made elsewhere in the workspace.
`tower-agent-workflow` refuses to own persistence and moves an opaque host job
instead. `docs/durable-host-report.md` concluded that the queue carries
readiness while the store carries truth. Here the adapter carries naming while
the store carries the mapping.

`resolve` takes the scope rather than trusting the id, so a forged or leaked id
from another scope fails to resolve rather than resolving into a stranger's
conversation. Two further checks sit behind it and are not a substitute for it:
`ResumeBinding` in `tower-agent-plan` is provider-tagged, and the provider
adapters already refuse a foreign-provider handle at validation.

## Continuation on failure, and what it is not

`FailureEvidence` carries a session, and the existing projection already emits
it. A turn that failed may therefore still be resumable, so the adapter mints a
continuation id for failures as well as successes. Most tool servers cannot
offer this; the evidence discipline is what makes it available.

It also creates a hazard. A failure payload carries `effects` and a
continuation id side by side, and a client that reads them together will infer
that the id is how you try again.

**Continuing a conversation is not retrying a turn.** Resuming does not re-run
the failed call, and it does not make a call with `effects: possible` safe to
repeat. `docs/resilience.md` governs replay and nothing here changes it. The
schema states this in words rather than relying on the field names, because the
inference is natural and wrong.

## Redaction

Collapsing provider messages is safe and costs the user the actual error. The
right default depends on who can read the output, which the adapter cannot
infer.

**Decision: redacted by default, verbosity opt-in.** A single-user local server
may enable provider text. A shared one must not, and must not have to remember
to turn it off.

## What maps without adaptation

Three things fit the existing kernel more exactly than a projection usually
does, which is the main evidence that MCP is a reasonable first interface.

**Cancellation is the same type.** `tower_mcp::Context::cancellation_token`
and `CallContext::with_cancellation` are both
`tokio_util::sync::CancellationToken`. An MCP `notifications/cancelled` drives
the existing deadline-and-drain path with no bridging, including the guarantee
that the adapter waits for settlement rather than dropping the provider future.

**Events fit progress.** `AgentEvent` maps to
`Context::report_progress_sync`. Both are nonblocking by contract and both drop
rather than stall the call they observe, so an `EventSink` wrapping `Context`
preserves the rule that observation never delays settlement.

**Elicitation is what requirements were shaped for.** `Context::elicit_form`
takes what `Requirement` already carries, and `ElicitResult` becomes `Answer`.
`tower-agent-plan` documents this use case explicitly and has had no consumer
that exercises it. `Requirement.sensitive`, plus the rule that secrets are
never requirements, means nothing dangerous is elicitable by construction.

## Shape

```text
tower-agent-mcp
  ContinuationStore    trait, plus a session-scoped in-memory default
  projection           TurnOutcome | AgentError -> structured content
  ProgressEvents       EventSink -> Context::report_progress_sync
  TurnTool             BoxTurnService -> one tool, optional continuation input
  PlanTool  (feature)  resolve -> elicit -> compile -> run
```

The composition test becomes the projection module's specification with little
change. It already asserts the redaction rules, the evidence projection, and
that middleware survives the boundary.

## Series

Unfiled. The intended order:

1. `ContinuationStore`, the id type, and the scope check, with the session
   boundary tested before any transport exists.
2. The projection module, extracted from the composition test.
3. `TurnTool`, including continuation input and continuation-on-failure.
4. Progress events.
5. `PlanTool` behind a feature, once the turn projection is stable.

`PlanTool` is deliberately last. The adapter's first version should not depend
on `tower-agent-plan`, so that the planning crate gains a consumer without the
interface inheriting a second crate's API surface before its own is settled.

## Open

- Whether planning belongs at the MCP surface at all, or whether elicitation is
  a host concern and the adapter stays a thin turn projection.
- Whether one tool with a mode or two tools is the better public shape.
- Whether the adapter ships an authenticated store or only the trait, which
  depends on how `tower-mcp` auth is configured in practice.
