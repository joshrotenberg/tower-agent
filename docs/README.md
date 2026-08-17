# Documentation map

The repository contains both the adopted Tower-native kernel design and
historical material for the preserved MCP-first compatibility host. This index
is the authority map.

## Current direction

- [`../README.md`](../README.md): public overview, status, and workspace map.
- [`design/tower-service-kernel.md`](design/tower-service-kernel.md): adopted
  architecture, contracts, safety invariants, implementation status, and
  deferred work.
- [`../CONTRIBUTING.md`](../CONTRIBUTING.md): change discipline and validation.
- Rustdoc and tests under `crates/tower-agent`: executable API and middleware
  contract.

When prose and executable behavior disagree, treat it as a bug. Tests establish
implemented behavior; the service-kernel design distinguishes implemented,
deferred, and release-target properties.

## Historical compatibility-host material

- [`design/spec.md`](design/spec.md): original MCP-first implementation plan.
- [`design/agent-worker-mcp-spec.md`](design/agent-worker-mcp-spec.md): MCP Agent
  Worker north-star contract.
- [`design/first-run.md`](design/first-run.md): first live MCP-host experiment.
- [`design/bus-vs-workflow.md`](design/bus-vs-workflow.md): legacy bus and
  orchestration analysis.

These documents explain `tower-agent-server` history and may still inform a
downstream host. They do not define the core crate and should not generate core
backlog without a new, concrete kernel invariant.

## Decision rule

New core work should name at least one of:

- a finite-service contract or provider seam;
- a mechanically testable middleware invariant;
- evidence from a real provider failure;
- a protocol-neutral request, outcome, event, or error requirement.

Schedulers, fleets, durable stores, workflow engines, buses, and protocol
surfaces belong above the kernel until a concrete consumer proves otherwise.
