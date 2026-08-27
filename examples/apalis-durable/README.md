# apalis-durable-example

Restart recovery for a workflow host backed by persistent Apalis storage.

[#105](https://github.com/joshrotenberg/tower-agent/pull/105) proved the
workflow/Apalis boundary in memory and stopped at the process boundary. This
answers what came next: do frozen invocations, claim fencing, typed results,
deadlines, and cancellation survive a restart?

```bash
cargo run -p apalis-durable-example
```

The phases are also driven as separate processes by `tests/restart.rs`:

```bash
cargo test -p apalis-durable-example
```

## The shape

The boundary from #105 is unchanged. The workflow runner owns graph readiness.
Apalis transports only opaque versioned references. The host store owns
identity, claims, launch state, terminal results, and reconciliation. Provider
and cancellation objects are rebuilt locally, never serialized.

The store is an append-only log, because the questions after a crash are
historical: was this claimed before the worker died, did a result arrive after
its claim was fenced out, who decided this was safe to resume.

## What to read

[`docs/durable-host-report.md`](../../docs/durable-host-report.md) has the
version matrix, the fifteen proved properties, the storage-backend finding,
and the extraction decision.

The short version of the finding: `apalis-file-storage` 0.1.0-rc.9 does not
durably store enqueued work, and that is survivable only because the queue
carries readiness while the host store carries truth.
