# Local Apalis transport proof

This application is a deterministic, in-memory proof of the boundary between
`tower-agent-workflow`, Apalis, and a host-owned run store. It is intentionally
an application-level example rather than an Apalis dependency in the workflow
crate.

For each ready workflow step, the dispatcher registers the full host-local
`StepCall`, sends only a versioned `StepJobRef` through Apalis, and waits for a
typed terminal result. Every reference is deliberately delivered twice. The
Apalis worker claims the host record, calls a hardened fake provider service,
and commits the typed result before returning queue-level success. The workflow
runner remains responsible for DAG readiness; Apalis remains responsible only
for transporting ready work to a worker.

The executable drops its first coordinator after both fan-out roots have been
launched. The workers finish independently, and a second coordinator using the
same logical run identity reuses both settled roots and schedules only the join.
The final counters show six queue deliveries for three logical steps, with one
provider call and one terminal record per step.

## Run it

From the workspace root:

```text
cargo run -p apalis-local-example
```

Run the focused recovery tests with:

```text
cargo test -p apalis-local-example
```

The tests cover coordinator replay, frozen invocation identity, graceful worker
shutdown and settlement, stale-worker fencing, safe reclamation of work lost
before launch, and fail-closed settlement of work lost after launch.
Synchronization gates make those transitions deterministic rather than relying
on sleep timing.

## Host record states

```text
Pending -> Claimed(epoch) -> Launched(epoch) -> Terminal(epoch)
                |                  |
        worker loss          worker loss
                |                  |
                v                  v
             Pending        Uncertain(epoch)
```

- `Pending`: The host still owns the complete call. Coordinator replay may
  enqueue its opaque reference again.
- `Claimed(epoch)`: One worker holds a fenced claim, but the host record still
  retains the call. If that worker is known to be gone, the record can safely
  return to `Pending`; the stale epoch can no longer launch or commit it.
- `Launched(epoch)`: The call has crossed the launch boundary and the worker
  owns it. External effects may have happened, so loss cannot safely return the
  record to `Pending`.
- `Terminal(epoch)`: The host has stored the exact typed success or failure.
  Duplicate delivery and coordinator replay observe that result without
  another provider call.
- `Uncertain(epoch)`: A launched worker disappeared before committing a typed
  result. The host publishes an `Internal / Settlement / Possible` failure,
  rejects stale completion, and never automatically relaunches the call.

Only the matching claim epoch may launch or complete a record. Deliveries that
find a record already claimed, launched, terminal, or uncertain are skipped.
That host-owned state, rather than an Apalis idempotency key or acknowledgment,
is the authority for execution and result reuse.

## Non-goals

This proof does not provide or validate:

- a persistent run/result store or recovery after process restart;
- lease expiry, queue-lock expiry, worker ownership discovery, or automatic
  abandoned-job recovery;
- reconstruction of a persisted wall-clock deadline into a worker-local
  `Instant`;
- Claude, Codex, or any other real provider;
- automatic Apalis or agent retry;
- Apalis `last_result` as an authoritative typed result store.

The in-memory store rejects a replay that changes the workflow job, input, or
dependency outputs associated with an existing logical identity. A production
host must enforce that frozen-invocation invariant durably, persist claims and
results, fence stale workers, and define an explicit reconciliation policy for
uncertain effects.
