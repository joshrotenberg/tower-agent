# tower-agent-workflow

Backend-neutral workflows over finite Tower services. One definition owns
stable identities, dependency topology, and opaque host jobs; one runner calls
a host-supplied service for each ready step.

## Intention

- **Definitions are immutable and validated on construction.** Duplicate ids,
  missing dependencies, and cycles are refused before any step launches, which
  is the only point at which refusing is free.
- **Jobs are opaque.** The crate moves a host-defined value from definition to
  dispatcher without interpreting it, so agent work and ordinary mechanical
  work travel the same path and the host decides what each means.
- **Only direct dependencies are handed to a step.** A step sees what it
  declared it needed, not the whole run.
- **The runner is deliberately non-durable.** It calls one service per ready
  step and never retries. Retry over an agent turn requires proof that no
  external effect occurred, which this crate cannot establish; see
  [the resilience guide](https://github.com/joshrotenberg/tower-agent/blob/main/docs/resilience.md).

What it does not own: configuration parsing, provider options, persistence,
sessions, queue policy, and scheduling. Those belong to a host, and two
worked examples in the repository show what that division looks like in
practice, including a durable host that survives process restart.

## Status

Experimental, at `0.1`, alongside the rest of the workspace. The execution
semantics are typed and tested; API stability is not yet a goal.

Two consumers have exercised it: a configuration-driven repository worker and
a restart-recovering durable host. Neither required a change to this crate,
which is the evidence behind publishing it.
