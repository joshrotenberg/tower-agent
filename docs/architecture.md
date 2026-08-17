# Architecture

## Execution model

The kernel is an owned Tower service for one finite agent operation:

```rust
Service<AgentRequest<Turn<ProviderOptions>>, Response = TurnOutcome, Error = AgentError>
```

The service future owns everything needed to finish or cancel that operation.
Provider selection is represented by the concrete service type. Provider
controls are represented by `ProviderOptions`; a universal flag bag would make
honor-or-refuse behavior impossible to reason about.

`AgentRequest` has two halves:

- `body`: potentially portable operation data;
- `context`: host-local identity, deadline, cancellation, and event sink.

Interfaces translate their wire data into a request and translate terminal
evidence back out. They do not own execution policy.

## Terminal contracts

`TurnOutcome` records output and any evidence the provider actually supplied:

- provider-private continuation handle;
- token buckets and reported total;
- monetary cost and currency;
- duration;
- provider turn count.

`AgentError` keeps four independent dimensions:

- `kind`: what class of failure occurred;
- `phase`: how far execution progressed;
- `effects`: whether external effects are absent, possible, or reported;
- `evidence`: partial terminal facts such as session, spend, usage, or turns.

An outer deadline or cancellation error keeps the provider settlement as its
cause and merges missing evidence upward. This lets accounting reconcile a
reservation and lets a host offer continuation even when the turn hit a cap.

Missing evidence is `None`. It is never converted to zero. Provider-private
session values redact themselves in `Debug`, and interfaces expose only their
own safe projections.

## Events and receipts

Events are observations, not the terminal response. Sinks are synchronous but
must be nonblocking; a slow observer cannot hold provider execution hostage.
Dropped observations are acceptable. Dropped terminal settlement is not.

Receipts record operation identity and typed terminal state. Observation sits
outside panic normalization so a panic converted into `AgentError` is visible
as the actual terminal result.

## Reference stack

Outside to inside:

```text
Supervise
  Observe
    CatchPanic
      Admission
        Deadline
          ValidateTurn
            Provider
```

The order encodes policy:

1. `Supervise` retains ownership after an interface caller disappears.
2. `Observe` sees normalized terminal results.
3. `CatchPanic` converts panics raised by `call` or its future.
4. `Admission` holds capacity until every inner cleanup path settles.
5. `Deadline` signals cancellation and drains the provider future.
6. `ValidateTurn` rejects invalid bodies before launch.

`AdmissionLayer` is immediate load shedding, not waiting backpressure. Its
`poll_ready` reports ready; unavailable capacity becomes typed `Busy` from
`call`. Use Tower's concurrency limit when waiting is the desired policy.

## Cancellation and process ownership

The request cancellation token is cooperative at the kernel boundary. Provider
services must observe it while work is in flight. `DeadlineLayer` never drops
the provider future itself: it signals cancellation and waits for settlement.

Claude wrapper 0.14 and Codex wrapper 0.3 put each invocation in its own process
group on Unix. Dropping their execution future kills that group; timeout and
stdin setup failures use the same ownership machinery. On platforms without
process groups, cleanup reaches the direct child.

Drop cannot synchronously wait for process reaping. The current adapters can
prove that cancellation reaches the wrapper cleanup path and that the service
settles promptly, but not a portable "all descendants reaped before return"
property. A future wrapper API that accepts cancellation in its high-level JSON
and stdin paths can close that final proof by awaiting termination and reap.

## Provider boundaries

Provider controls are honor-or-refuse. A service either maps a requested model,
sandbox, directory, tool, or resume control exactly, or rejects the request
before work starts.

Current prompt placement:

| Provider path | User prompt | Other instruction data |
|---|---|---|
| Claude | stdin | system-prompt flags in argv |
| Codex fresh | stdin | composed prompt in stdin |
| Codex resume | argv | composed prompt in argv |

The host owns launch configuration such as provider home and configuration
directories. Portable request bodies do not carry ambient credentials or
provider-private launch context.

## Middleware opportunities

### Authority narrowing

Represent requested filesystem, network, tool, and subprocess authority and
intersect it with host policy before provider launch. The result must never be
broader than either request or policy. A provider that cannot enforce the
intersection must refuse it. Approval is a host concern above the enforcement
boundary.

The immediate concrete case is Codex read-only versus workspace-write sandbox.
Claude does not currently expose an equivalent filesystem sandbox through this
adapter, so a generic write request cannot yet be enforced uniformly.

### Context assembly

Build prompt context deterministically from named sources. Record source ids,
ordering, byte or token budgets, and truncation. This layer should make the
effective input reproducible without placing provider-private data in the
portable turn.

### Budget reservation and reconciliation

Reserve an estimated budget at admission, then reconcile with terminal success
or failure evidence. Failure accounting is first-class because a capped or
failed provider call may still report spend and token use.

### Output contracts

Validate structured provider output before it becomes a successful outcome.
Distinguish provider execution failure from settlement failure and retain the
raw provider evidence only behind a host-private diagnostic boundary.

### Event policy

Fan out events to metrics, logs, progress, and receipts while applying per-sink
redaction and overflow policy. Terminal settlement remains independent of
event delivery.

### Circuit breaking

Open on typed provider or launch failures, not invalid requests or rejected
authority. Half-open probes must be explicitly non-effectful or isolated.

### Retry and fallback

These are unsafe by default. A retry or alternate provider is admissible only
when effect evidence proves no external effect occurred, or when the operation
has a provider-backed idempotency contract. `EffectState::Possible` forbids an
automatic replay.

## Service laws

Tests should preserve these properties:

1. Invalid input and unsupported controls fail before provider launch.
2. Cloned admission services share one capacity boundary.
3. Cancellation and deadline return only after the inner service settles.
4. Caller drop does not abandon an owned provider future.
5. Panic normalization preserves operation identity and effect conservatism.
6. Outer errors retain the strongest available cause and partial evidence.
7. Missing usage, cost, duration, and turn counts remain absent.
8. Provider session values never appear in `Debug` or adapter DTOs by default.
9. Middleware behavior is unchanged when called through an interface adapter.
10. The core dependency graph contains no interface implementation.
