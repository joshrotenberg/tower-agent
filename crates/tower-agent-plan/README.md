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

## Example

[`examples/elicitation_loop.rs`](examples/elicitation_loop.rs) runs the whole
shape in one file: layered defaults, a fragment a user supplied, requirements
reported as data rather than prompted for, answers folded back in, and a typed
`Turn<ClaudeOptions>` at the end. It also shows a refusal, because diagnostics
are data on the same footing as requirements.

```sh
cargo run -p tower-agent-plan --features claude --example elicitation_loop
```

## Status

Experimental, at `0.1`, alongside the rest of the workspace. API stability is
not yet a goal.

The vocabulary and merge laws are specified by the JSON fixture corpus under
`tests/fixtures/`; the provider folds are specified by the typed planner tests.

Provider support is feature-gated. With neither `claude` nor `codex` enabled
the crate still resolves and reports requirements; it just has no planner to
compile a turn with.
