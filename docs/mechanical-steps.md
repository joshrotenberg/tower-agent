# Mechanical steps

A workflow mixes agent turns with ordinary work: read a branch, count files,
open a pull request. `tower-agent-workflow` already carries an opaque
host-owned job through one dispatcher, so an application can do both today
without the library knowing which is which.

This note records the boundary before anyone adds a subprocess runner, and
ends with a decision. It authorizes nothing.

## What the evidence says

[`examples/repository-worker`](../examples/repository-worker) routes agent and
mechanical steps through a single `Service<StepCall<..>>`. The mechanical half
is a closed enum and a match, and the whole of it is about twenty lines:

```rust
match op {
    MechanicalOp::ReadBranch => format!("{}@{}", input.repository, input.branch),
    MechanicalOp::CountFiles => /* ... */,
    MechanicalOp::Collect    => /* joins direct-dependency results */,
}
```

The operation set is closed on purpose. An open string would let
configuration name work the host has not implemented, turning a compile-time
error into a run-time one halfway through a workflow that has already spent
money.

Nothing in the library needed changing to support this, and no
workflow-specific mechanical trait was required. That is the baseline any
subprocess proposal has to beat.

## Why a subprocess runner is not a small addition

Running an arbitrary command well requires every hazard the provider adapters
already handle, and this repository has spent real effort getting each one
right for two well-known CLIs. A general runner faces all of them at once, for
commands nobody has audited.

| Concern | What it takes to get right | Where this repo learned it |
|---|---|---|
| Executable identity | Direct argv, never `sh -c`. A shell string turns any interpolated value into code | Provider adapters compile argv, never a command string |
| Argument construction | Fixed host-owned registry; configuration selects a command, never composes one | The planning crate refuses a raw-argv escape hatch for the same reason |
| Working directory | Explicit, contained, never inherited by accident | `AuthorityPolicy` writable roots |
| Environment | Cleared by default, allowlisted additions, explicit values redacted | `ChildEnvironmentPolicy` |
| Filesystem authority | Host ceiling checked before launch and again at the launch boundary | `AuthorityLayer` plus the Codex service's own repeat check |
| Output ceilings | Bounded capture inside the runner; a downstream cap cannot bound peak memory | #99, which needed changes in both wrapper crates before the adapters could offer it |
| Malformed output | A parse failure is a settlement failure, never a partial success | Codex terminal-event validation |
| Process groups | Own the group on Unix; a direct child is not the whole tree | Both wrappers |
| Cancellation | Signal and await settlement; dropping the future abandons a live process | `DeadlineLayer` |
| Reaping | Await termination before reporting settlement | Both adapters |
| Abrupt worker death | `PR_SET_PDEATHSIG` on Linux, spawn receipts and an external watchdog elsewhere | `SpawnReceipt`, `die_with_parent` |
| Effect state | `None` only before launch; `Possible` once the process exists | #100, #125, #136, each a bug from getting this wrong |
| Idempotency | An effectful Git or GitHub step cannot be replayed without a provider-backed contract | [`resilience.md`](resilience.md) |
| Redaction | Fixed category messages; child output never enters an error | Claude adapter diagnostics policy |

The table is the argument. Every row is a place this codebase has already been
wrong at least once, for CLIs whose behavior was known and captured.

## A candidate envelope, not a commitment

If a subprocess adapter is ever built, this is the shape to start from. It is
recorded so a future attempt does not reinvent it, and is **not stabilized**.

On stdin, one bounded document:

```json
{
  "schema_version": 1,
  "run": { "workflow_id": "review-fanout", "workflow_version": "v1",
           "run_id": "local-run", "step_id": "join" },
  "input": { "repository": "owner/name", "branch": "main" },
  "dependencies": { "architecture": { "text": "..." }, "tests": { "text": "..." } }
}
```

On stdout, one bounded document:

```json
{ "schema_version": 1, "status": "ok", "output": { "text": "..." } }
```

or

```json
{ "schema_version": 1, "status": "failed", "code": "missing_input",
  "message": "fixed category text" }
```

Four properties matter more than the field names:

- **Identity travels, capability does not.** The envelope names the run and
  step. It carries no credential, no provider session, no cancellation token,
  and no `Instant`: the first two are secrets and the last two are meaningless
  outside the process that made them.
- **Both directions are versioned and bounded.** Unbounded input is a way to
  hang a child; unbounded output is a way to exhaust the host.
- **Only direct-dependency results are projected**, matching what
  `StepCall::dependencies` already provides.
- **Cancellation is out of band**, through the process group, because a token
  cannot be serialized and a child cannot be trusted to poll one.

Diagnostics belong on stderr, bounded and redacted, and never in the returned
failure.

## Decision

**Defer, and implement in the consuming application if the need arrives.**

The in-process typed service costs about twenty lines, needed nothing from
the library, and covers everything the demonstrated use case asked for. A
subprocess runner would be a second provider-adapter-shaped component carrying
every hazard in the table above, for arbitrary commands rather than two
audited CLIs.

Two conditions would change this. If at least two consumers independently
need to run out-of-process work and converge on a similar contract, the
envelope above becomes worth extracting. If a single consumer needs it,
building it inside that application is correct: it can hardcode its command
registry, its authority, and its ceilings, none of which generalize.

Until then `tower-agent-workflow` keeps no mechanical trait, no shell
adapter, and no envelope. Its job stays typed definitions and finite
execution.
