# The bus vs the workflow methodology

> Historical design note for the preserved MCP-first host. The bus is not part
> of the `tower-agent` kernel contract or current core backlog. See
> [`../README.md`](../README.md) for the documentation authority map.

Two ways to get many agents working together. tower-agent implements one; the
other is a solved problem it should compose with, not rebuild. This note records
why.

## The two models

**The bus (what tower-agent is).** Persistent agents react to triggers (invoke,
schedule, subscribe), post to channels, and converse. Coordination is emergent:
there is no central plan, structure comes from subscriptions and directed posts.
Stochastic and reactive. The depth bound keeps it from storming; convergence is
not guaranteed.

**The workflow methodology (fan-out / synthesize).** A controller decomposes one
task into independent pieces, fans out N agents (often in parallel), and joins
their results in a synthesis step. Deterministic and scripted (barriers,
pipelines, judge panels). Ephemeral: it runs and exits. Convergence is
structural, the join guarantees it.

| | bus | workflow |
|---|---|---|
| topology | peer graph, emergent | controller / DAG, prescribed |
| lifetime | persistent roles | ephemeral tasks |
| control | stochastic, reactive | deterministic, scripted |
| coordination | posts + subscriptions | barriers, pipelines, joins |
| convergence | not guaranteed (a role must decide) | structural (the synthesis step) |
| best for | ongoing, open-ended, event-driven work | bounded decompose-and-cover tasks |
| failure mode | storms, non-convergence | rigidity, over-parallelization cost |

## They compose

The `Backend` is "run a prompt, get a result," and the `prompt` tool is async
over MCP tasks. That is exactly the atom a workflow orchestrates. So:

- A workflow can call a tower-agent agent as a step (over the MCP `prompt` tool);
  configured specialists become callable units inside a fan-out.
- A workflow's synthesis step maps onto a tower-agent **decider role**: a role
  that reads the thread and writes a durable decision. That is the mechanical
  convergence the stochastic bus lacks.
- The bus can host a crude fan-out (a planner broadcasts subtasks, workers
  subscribe and post results), but that fights the substrate: no deterministic
  barrier, no guaranteed join, storm risk. Keep heavy fan-out off the bus.

## Decision: do not build a workflow engine

tower-agent stays a substrate whose atom any orchestrator can drive.

1. Deterministic DAG orchestration is a solved problem (external orchestrators,
   script runners). Rebuilding it would duplicate that and dilute the fabric.
2. tower-agent's value is what workflows do not do: persistent roles, trigger
   driven reactivity, emergent conversation, memory, and observability of ongoing
   traffic. Lean into that.
3. Keeping the `prompt` tool a clean, callable atom keeps tower-agent composable
   into any orchestrator. The fabric provides agents and execution; a workflow
   provides the plan; they meet at the MCP `prompt` tool.

If fan-out ever earns a place as a first-class feature, the shape is a
**coordinator role** (config/markdown, using `invoke` to dispatch and a decider
to synthesize), not a bespoke engine. See issue #28.
