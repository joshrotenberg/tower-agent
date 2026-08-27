# Workflow host friction report

What `tower-agent-workflow` felt like from a host that builds definitions from
configuration rather than Rust literals. The host is
[`examples/repository-worker`](../examples/repository-worker): an application
schema, a compiler, a profile catalog, and one dispatcher, running four shapes
against in-process providers.

Recorded for #107, whose question was where the library boundary helps and
where it gets in the way.

## Verdict

**No workflow-library changes needed.** Everything the host wanted, the
library already expressed, and each boundary it enforced turned out to be
load-bearing rather than merely tidy.

## What the library expressed

| the host wanted | the library gave |
|---|---|
| one agent call | `WorkflowDefinition::single` |
| a linear pipeline | `PipelineBuilder`, though the host used `DagBuilder` uniformly |
| fan-out and join | `DagBuilder` with explicit `needs` |
| phases | nothing, and correctly so; see below |
| stable identities | `WorkflowId`, `WorkflowVersion`, `StepId`, validated on construction |
| duplicate and cycle refusal | `WorkflowDefinitionError`, before any step launches |
| direct-dependency results | `StepCall::dependencies`, only direct ones |
| the run's cancellation and deadline | `StepCall::agent_context()` |

## Phases are sugar, and belong to the host

The host offers a phase-oriented form. Compiling it is nine lines: every step
in a phase depends on every step of the previous phase, which is a fan-in
followed by a fan-out. It normalizes to a definition byte-identical to the
hand-written DAG, and a test asserts the two produce the same run rather than
merely the same shape.

That is the argument against a phase facade in the library. The sugar is
cheap, application-shaped, and different hosts will want different sugar; a
library phase type would have to pick one and would then be wrong for the
next consumer. **A reusable phase or planner facade is not justified yet.**

## The one friction point

`AgentStepService` binds a single `Turn<Options>` type. A host routing across
providers cannot use it, because provider choice is per step: this host's
`reviewer` profile resolves to Claude and `implementer` to Codex, and the
turns those produce have different option types.

The host therefore wrote its own dispatcher and used `RoutedTurnService` over
`ReadyTurn`, which is the type built for exactly this. That is not a defect,
but the adapter's doc comment could say plainly that it is the single-provider
convenience and that a heterogeneous host should route `ReadyTurn` instead.
The friction was ten minutes of reading, not a redesign.

## What stayed ordinary host glue, as intended

Schema and migrations, the profile catalog, prompt text, mechanical
operations, the dispatcher, and error formatting with file locations. None of
it wanted to move into the library, and the library never asked to see any of
it.

Worth noting what configuration cannot express here, by construction: a
provider option, a credential, a session handle, a cancellation token, or a
deadline. A step names a host-owned profile and supplies a prompt. Those are
the values that are either secret or process-local, and a file that could
carry them is a file that leaks or goes stale.

## Where the planning crate carried weight

The compiler resolves each agent step's profile at compile time, so a
misspelled profile or an unsatisfiable one fails before any step launches,
with a location a person can open. That works because the planning crate
answers with requirements and diagnostics as data rather than by prompting or
launching, so a compiler can ask "would this run?" without running it.

The host also keeps the provider that compilation resolved to, and the
dispatcher refuses if re-resolving would now pick a different one. A profile
edited between compile and run should not silently move a step to another
provider.

## Publication readiness

**Keep `tower-agent-workflow` unpublished for now.** Nothing here argues
against the design, and one host is thin evidence for an API that would then
be stable. The concrete gap is durability: this host runs in process, and
#108 asks whether frozen invocations, claim fencing, and typed results survive
a restart. That question can still change the shape of what a durable host
needs from the definition types, and it is cheaper to answer before
publishing than after.

## Follow-ups this produced

- Say in `AgentStepService`'s docs that it is the single-provider convenience,
  and point a heterogeneous host at `RoutedTurnService`.
- #106 can now use this host's typed in-process mechanical service as its
  evidence, which was its stated precondition.
