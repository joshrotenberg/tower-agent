# tower-agent-claude

The Claude Code provider service for [`tower-agent`](../tower-agent). It
implements the same owned Tower contract as every other provider, with
Claude-specific controls in `ClaudeOptions`.

## Intention

- **Honor or refuse.** A control is either mapped exactly or the request is
  rejected before work starts. `ClaudeService::preflight` exposes those
  refusal decisions as a pure check, so a caller can find out without
  launching.
- **Ambient context is host-owned.** The service combines a host baseline
  with the mode a turn requests. A turn may strengthen the posture and never
  weaken it; conflicts fail during validation.
- **Structured output is validated on both sides.** A requested JSON schema
  is checked as draft-07, size-bounded, and required to be an object before
  launch, and the validated payload comes back as `TurnOutcome::structured`
  rather than serialized into the prose.
- **Errors say nothing about the provider's internals.** Messages are fixed
  category text; result text, stderr, arguments, working directories, and
  session values never reach the public error surface.

The prompt travels over stdin; system-prompt flags remain in argv, so secrets
do not belong there.

## Boundaries

This adapter does not claim the portable filesystem-authority contract. Tool
allowlists are provider-specific controls, not a sandbox, and must not be
presented as enforcement of that contract.

## Status

Experimental `0.1`, tracking `claude-wrapper` 0.14.
