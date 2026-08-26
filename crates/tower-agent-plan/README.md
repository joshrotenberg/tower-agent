# tower-agent-plan

The pure front half of one agent call: turn partially specified turn data
into a complete typed turn body, a structured list of missing requirements,
or structured diagnostics. The kernel and the provider adapters remain the
effectful back half; this crate launches nothing.

## Intention

- Layered resolution with one documented precedence: provider baseline
  defaults, application defaults, one selected profile, the explicit
  request, elicited answers for still-unbound paths.
- Profiles as saved partial turns. The requirements remaining after
  resolution are a profile's effective callable signature, so a resolved
  profile behaves like a partially applied service.
- Requirements and diagnostics as adapter-neutral data. A CLI renders
  requirements as flags, an MCP adapter as elicitation, a UI as form
  fields; none of those choices lives here.
- Provider planners behind cargo features (`claude`, `codex`) fold a
  complete resolution into the adapter's concrete `Turn<O>`,
  honor-or-refuse. `ReadyTurn` is the provider-committed compile target.
- Per-provider `preflight` helpers check a folded turn against one
  configured service, so planner `Ready` means that service will not refuse
  the turn during its validation phase.
- `RoutedTurnService` dispatches a `ReadyTurn` to the service registered for
  its provider. Resumed turns stay pinned to the provider that minted their
  session, and a failure is never retried or replayed against another
  provider.

The compile target is always the typed portable turn body, never a process
specification. Argv construction, environment policy, execution,
cancellation, and settlement belong to the provider adapters and the kernel
middleware. The decision record, including what was deliberately cut from
the originating design, is [`docs/plan.md`](../../docs/plan.md).

## Status

Unpublished workspace crate. The vocabulary and merge laws are specified by
the JSON fixture corpus under `tests/fixtures/`; the provider folds are
specified by the typed planner tests.
