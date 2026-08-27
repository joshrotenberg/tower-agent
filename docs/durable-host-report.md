# Durable host report

What survives a restart, what does not, and one finding that changes where
durability has to live. Recorded for #108. The host is
[`examples/apalis-durable`](../examples/apalis-durable).

## Version matrix

| crate | version | note |
|---|---|---|
| `apalis` | `=1.0.0-rc.9` | pinned by #105 |
| `apalis-file-storage` | `=0.1.0-rc.9` | the **only** backend published against rc.9 |

`apalis-sqlite`, `apalis-postgres`, `apalis-mysql`, and `apalis-redis` all cap
at `1.0.0-rc.8`. Using any of them means moving the whole example back a
release candidate. That is the trade recorded here rather than made silently.

## The finding

**`apalis-file-storage` 0.1.0-rc.9 does not durably store enqueued work.**

Its `insert` touches only the in-memory map. `persist_to_disk` runs from
`remove`, from the acknowledge path, and when a task is polled out. A task
that is pushed and never polled is lost with the process, and pushing does not
write at all until the sink is flushed.

A test asserts this, so it cannot regress quietly and so a future backend that
fixes it fails the assertion and prompts an update.

## Why the finding is survivable

Because the design #105 established already refuses to trust the queue. Its
non-goals say plainly that acknowledgements are not the authoritative result,
and the host store commits before the queue hears anything.

So the queue carries **readiness**, and the store carries **truth**. Losing the
queue costs liveness, not correctness: a coordinator re-derives which steps
settled and which became ready from the store alone, and enqueues again
without calling a provider twice. That is proved rather than asserted.

## What was proved

Fifteen tests. Two of them run the binary as a **separate process** twice,
sharing nothing but a file path, because reconstructing a store inside one
process still shares an allocator and a page cache.

| Property | How |
|---|---|
| Settled roots survive; only the newly ready step is admitted | reconstruction and real subprocess |
| Work lost while claimed returns under a strictly higher epoch | reconstruction and real subprocess |
| Work lost after launch becomes uncertain and is never relaunched | reconstruction |
| A result from a fenced-out claim is logged but never becomes the answer | reconstruction |
| Terminal commit before acknowledgement survives redelivery: three deliveries, one provider call | reconstruction |
| A redelivery describing different work is refused | fingerprint mismatch |
| An expired deadline admits nothing after restart | wall-clock reconstruction |
| Cancellation intent survives restart | durable record |
| Uncertain work resolves only by a recorded, attributed decision | three decision paths |
| A torn final line does not prevent recovery | partial write |
| Recorded output is bounded | truncation |

## Two decisions inside the design

**Deadlines persist as UTC milliseconds, never as `Instant`.** An `Instant` is
meaningless in the process that reads it back. The deadline is reconstructed
against the current clock, and an expired one admits nothing, because treating
it as "plenty of time" is how a restart silently doubles a budget.

**Reconciliation is a recorded decision, not a state transition.** Uncertain
work resolves three ways, each written to the log with its author: adopt a
result that provider evidence shows completed, prove no effects and become
runnable again, or abandon and leave descendants blocked behind a typed
failure. Only the middle one makes work runnable, and it takes a person or a
policy saying so. That is what "auditable" has to mean for an operation that
may have opened a pull request.

The store is append-only for the same reason. The questions after a crash are
historical: was this claimed before the worker died, did a result arrive after
its claim was fenced out, who decided this was safe. A log answers those; an
overwritten row does not.

## Extraction decision

**Keep it in the host. Do not extract `tower-agent-apalis`.**

The transport glue is small: one opaque versioned reference, an enqueue, and a
drain. What is substantial is the *host store*, and that is where the design
decisions live: epochs, fingerprints, the commit-before-ack ordering,
wall-clock deadlines, and the reconciliation vocabulary. None of that is
Apalis-specific, and an adapter crate would either exclude it, leaving
something too thin to be worth a crate, or include it, and then it is not an
Apalis adapter at all.

Extraction becomes worth revisiting when a second transport appears and the
same glue is written twice. One transport is not evidence.

## What this says about publishing the workflow crate

#107 recommended waiting for this answer. It arrived without asking the
workflow crate to change: `WorkflowDefinition`, `StepCall`, and the identity
types were sufficient, and the durable host needed nothing added to them.

That removes the objection #107 raised. The remaining reason to wait is
ordinary caution about a `0.1` API with two consumers, not a known gap.
