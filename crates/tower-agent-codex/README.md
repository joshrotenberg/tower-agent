# tower-agent-codex

The Codex provider service for [`tower-agent`](../tower-agent). It implements
the same owned Tower contract as every other provider, with Codex-specific
controls in `CodexOptions`.

## Intention

- **Honor or refuse.** A control is either mapped exactly or the request is
  rejected before work starts. `CodexService::preflight` exposes those
  refusal decisions as a pure check, so a caller can find out without
  launching.
- **Filesystem authority is enforced twice.** `AuthorityLayer` rejects an
  excessive request before provider work, and the service repeats the check
  at its own launch boundary, so omitting or reordering middleware cannot
  broaden authority. Read-only is the default ceiling.
- **Terminal evidence must be coherent.** Settlement requires exactly one
  terminal event, and it must be the last one parsed. Missing, repeated, or
  contradictory terminal sequences fail rather than promoting partial
  assistant text to a successful output.
- **Ambient context and skills are host-owned.** The automation profile
  suppresses user config, execpolicy rules, and project instructions, and an
  exact skill policy can disable named skill folders. Neither claims hermetic
  execution: provider built-ins, managed instructions, workspace contents,
  and the child environment remain.

Fresh and resumed prompts both travel over stdin, including any system prompt,
which is composed into the same string.

## Status

Experimental `0.1`, tracking a pinned `codex-wrapper` revision.
