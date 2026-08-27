# Resilience composition

Tower middleware composes mechanically. It does not always compose safely.
An agent turn can write files, call external services, and spend money before
it fails, so a policy that is routine for an idempotent RPC can duplicate real
work here. This is the guide to which policies are safe, and what each one
requires before it is.

The short version: **effect state decides, not error kind, and not timing.**

## Why `AgentRequest` is not `Clone`

Generic retry needs to issue the request again, so `tower::retry` requires a
`Policy` that can clone it. `AgentRequest` deliberately cannot be cloned, and
the missing impl is the point rather than an omission.

A request owns its `CallContext`: an operation id, a cancellation token, an
event sink, and possibly a host-preassigned session. Cloning it would produce
two operations claiming one identity, sharing a token so cancelling one
cancels the other, and emitting into one sink as though they were the same
call. Receipts would record one operation that settled twice.

So retry cannot be added by accident. A host that wants it has to reconstruct
the request explicitly, which is the moment to ask whether replaying is
sound.

## What every replay policy needs

Retry, hedging, and fallback all launch a second attempt. Each needs proof
that the first produced no external effect:

| `effects` | meaning | replay |
|---|---|---|
| `None` | nothing ran, or what ran provably had no effect | safe |
| `Possible` | the turn may have acted before failing | forbidden |
| `Reported` | the turn acted, and said so | forbidden |

Only `EffectState::None` permits an automatic replay, absent a
provider-backed idempotency contract. A deterministic error classification is
not a substitute: knowing *why* a turn failed says nothing about what it did
before failing. A turn that edited three files and then hit an authentication
error is an authentication failure that also changed the repository.

This is why pre-launch rejections matter so much. `Busy`, `Unavailable`, and
a validation refusal all carry `EffectState::None` because nothing was
launched, which is exactly what makes them the rejections a host *can* act on
automatically.

`retry_after` is timing, never permission. A provider asking a caller back in
sixty seconds says nothing about whether the first attempt spent money. Check
both, in that order: effect state for whether, guidance for when.

Hedging deserves a specific warning. Its whole premise is launching a second
attempt while the first may still be running, so it is not merely unsafe
after a failure but unsafe by construction here, unless the operation has an
idempotency contract the provider itself honors.

## Time limits are not interchangeable

`tower::timeout` and similar generic time limiters resolve by **dropping the
inner future**. For an agent turn that means abandoning a running provider
process: the child keeps working, the settlement never arrives, and evidence
of what it spent is lost.

`DeadlineLayer` does the opposite. It signals cancellation, keeps the future,
and waits for the provider to settle. The error it returns retains the
settlement as its cause and merges the evidence upward, so accounting can
still reconcile a turn that ran to completion a moment past its deadline.

Prefer `DeadlineLayer`. If a generic time limiter is used anyway, it must sit
*outside* supervision so a dropped caller future does not orphan provider
work.

## Recommended order

Outside to inside:

```text
Supervise            retains the call after the caller disappears
  Observe            sees normalized terminal results
    CatchPanic       converts panics into typed failures
      Resilience     circuit breaking, rate limiting, bulkheads
        Admission    immediate load shedding, no hidden queue
          Deadline   signals cancellation, waits for settlement
            Validate rejects invalid bodies before launch
              Authority   host filesystem ceiling, nearest the provider
                Provider
```

Resilience policies belong above `Admission` and below panic normalization.
Above admission, because a circuit that is open should refuse before a permit
is spent. Below `CatchPanic`, because a breaker counting failures should see
typed terminal results rather than unwinds.

Everything from `Deadline` inward runs per attempt. Everything outside
`Resilience` is per operation, and observation therefore records one receipt
per operation regardless of how many attempts a policy made.

## Circuit breaking

Open on typed provider and launch failures. Do not open on invalid requests
or rejected authority: those are the caller's fault and say nothing about
provider health, so counting them lets one malformed request trip the breaker
for everyone.

Half-open probes must be explicitly non-effectful or isolated. A probe that
runs a real turn to see whether the provider recovered is a turn that can
spend money and change a repository. It is also issued at the moment the
provider is least trusted, and it cannot be repeated freely, because each
attempt is another effect.

An effectful provider should therefore recover on an explicit signal instead.
A health operation is cheap, repeatable while the circuit is open, and risks
no work to answer, so it can be asked as often as needed. That makes the
circuit a switch the host controls rather than one that flips itself.

This needs manual circuit control, available from **`tower-resilience`
0.13.0**: `manual_mode` disables the sliding window, the half-open thresholds,
and the recovery timer, and `CircuitBreakerHandle::{force_open, force_closed,
reset}` are deterministic, so once one returns every clone of every service
from that layer observes the new state with no sleep or poll loop.

## Runnable examples

Each composes a real policy over the fake provider, so the behavior is
executable rather than described:

- [`circuit_breaker.rs`](../crates/tower-agent/examples/circuit_breaker.rs):
  opens on provider failures, reports `Unavailable` rather than `Busy`,
  because an open circuit is the provider being down and not this host being
  full. Recovers through an automatic half-open probe, which is safe only
  because the fake provider is scripted and non-effectful.
- [`health_gated_circuit.rs`](../crates/tower-agent/examples/health_gated_circuit.rs):
  the effectful case. A manual-mode circuit that no threshold or timer moves,
  recovered by a dedicated health operation rather than by an `AgentRequest`.
- [`rate_limiter.rs`](../crates/tower-agent/examples/rate_limiter.rs): a spent
  quota, reported as `Limit` with bounded `retry_after` guidance.
- [`bulkhead.rs`](../crates/tower-agent/examples/bulkhead.rs): capacity
  isolation, alongside `AdmissionLayer`.

## What is still unsafe

Buffering, caching, and request coalescing are not addressed by any of the
above. Each of them can serve one caller a result produced for another, which
for an operation that writes files and spends money is a different failure
than a stale cache entry. They stay out until typed effect evidence can prove
a particular composition sound.
