# Architecture

## Execution model

The kernel is an owned Tower service for one finite agent operation:

```rust
Service<AgentRequest<Turn<ProviderOptions>>, Response = TurnOutcome, Error = AgentError>
```

The service future owns everything needed to finish or cancel that operation.
Provider selection is represented by the concrete service type. Provider
controls are represented by `ProviderOptions`; a universal flag bag would make
honor-or-refuse behavior impossible to reason about.

`AgentRequest` has two halves:

- `body`: potentially portable operation data;
- `context`: host-local identity, deadline, cancellation, and event sink.

Interfaces translate their wire data into a request and translate terminal
evidence back out. They do not own execution policy.

## Terminal contracts

`TurnOutcome` records output and any evidence the provider actually supplied:

- provider-private continuation handle;
- token buckets and reported total;
- monetary cost and currency;
- duration;
- provider turn count.

`AgentError` keeps four independent dimensions:

- `kind`: what class of failure occurred;
- `phase`: how far execution progressed;
- `effects`: whether external effects are absent, possible, or reported;
- `evidence`: partial terminal facts such as session, spend, usage, or turns.

Pre-launch rejections stay distinguishable without parsing messages. `Busy` is
this host's own capacity, `Unavailable` is the provider or a dependency not
serving, and `Limit` is a quota a caller has spent. A host holding a second
provider should act on those differently, which it cannot do if they collapse
into one kind.

`retry_after` carries what a limiter or provider said about waiting. It is
timing and never permission: whether an operation may be attempted again is
decided by effect state alone, so guidance to wait a minute says nothing about
whether the first attempt already spent money or wrote files. It is clamped to
`MAX_RETRY_AFTER`, because a host that sleeps on an unbounded value stalls a
worker indefinitely, and it stays absent rather than guessed when nobody
said.

An outer deadline or cancellation error keeps the provider settlement as its
cause and merges missing evidence upward. This lets accounting reconcile a
reservation and lets a host offer continuation even when the turn hit a cap.

A call may also settle successfully after its deadline elapsed or its
cancellation fired. That settlement is stronger evidence than a failed one:
`TerminalEvidence` projects the response into `FailureEvidence`, the outer
error carries the resulting session, usage, cost, duration, and turn count,
and its effect state becomes `Reported` because the turn demonstrably ran.

Phase records how far execution actually got, so a rejection before launch is
never reported as `Running`. A readiness panic and a request cancelled before
launch are both `Admission`, and a readiness panic carries `EffectState::None`
because no request was handed to the provider. Overstating either dimension
forbids recovery that is provably safe.

Missing evidence is `None`. It is never converted to zero. Provider-private
session values redact themselves in `Debug`, and interfaces expose only their
own safe projections.

## Events and receipts

Events are observations, not the terminal response. Sinks are synchronous but
must be nonblocking; a slow observer cannot hold provider execution hostage.
Dropped observations are acceptable. Dropped terminal settlement is not.

Counting events does not bound them. One event can carry an entire provider
output, so a channel bounded at sixteen items is unbounded in bytes.
`EventLimits` adds two ceilings: the largest single payload accepted, and the
largest total retained by events emitted but not yet consumed. Because
dropping an observation is acceptable, exceeding either drops the event and
reports `Full` rather than failing the turn. The aggregate ceiling is a
high-water mark, not a lifetime total: `BoundedEventReceiver` releases the
budget as each event is taken.

`LimitOutputLayer` bounds the terminal output instead, and there the opposite
rule applies. An oversized result is refused, never truncated, because
returning a partial value as success would misrepresent what the provider
produced. The failure is typed `Limit`, carries no provider content, and keeps
the accounting the turn established, since the turn ran and spent regardless
of whether its output is usable.

Neither ceiling bounds peak memory inside the provider wrappers, which read
child output to completion before the adapter sees it. That bound belongs
upstream, and `ClaudeService::with_output_limit` now exposes it: the wrapper
stops the child the way cancellation does and reports a typed failure
carrying no captured content, which this adapter maps to `Limit` in the
running phase with possible effects, because the turn was interrupted rather
than completed. It is off unless a host sets it. Codex has no equivalent
control yet, so a Codex turn's capture remains unbounded.

Receipts record operation identity and typed terminal state. Observation sits
outside panic normalization so a panic converted into `AgentError` is visible
as the actual terminal result.

## Reference stack

Outside to inside:

```text
Supervise
  Observe
    CatchPanic
      Admission
        Deadline
          ValidateTurn
            Authority (providers carrying a filesystem-authority request)
              Provider
```

The order encodes policy:

1. `Supervise` retains ownership after an interface caller disappears.
2. `Observe` sees normalized terminal results.
3. `CatchPanic` converts panics raised by `call` or its future.
4. `Admission` holds capacity until every inner cleanup path settles.
5. `Deadline` signals cancellation and drains the provider future.
6. `ValidateTurn` rejects invalid bodies before launch.
7. `Authority` rejects excessive filesystem authority closest to the provider,
   where the host ceiling is known.

`AdmissionLayer` is immediate load shedding, not waiting backpressure. Its
`poll_ready` reports ready; unavailable capacity becomes typed `Busy` from
`call`. Use Tower's concurrency limit when waiting is the desired policy.

## Cancellation and process ownership

The request cancellation token is cooperative at the kernel boundary. Provider
services must observe it while work is in flight. `DeadlineLayer` never drops
the provider future itself: it signals cancellation and waits for settlement.

Claude wrapper 0.14.2 and the pinned Codex wrapper revision put each invocation
in its own process group on Unix. The adapters pass request cancellation into
the wrappers' high-level JSON paths. Explicit cancellation, timeout, and stdin
failures await group termination and direct-child reaping before returning
terminal settlement. On platforms without process groups, cleanup awaits the
direct child but cannot guarantee ownership of descendants. Dropping a wrapper
future remains an immediate kill path because destructors cannot await;
`SuperviseLayer` retains provider work after caller drop so normal Tower
composition reaches the awaited settlement path.

Cancellation ownership does not cover abrupt worker death because `SIGKILL`, a
container crash, and similar failures run no Rust destructor. Both provider
services therefore expose two host-owned controls:

- `with_die_with_parent(true)` asks Linux to apply `PR_SET_PDEATHSIG` with a
  post-arm parent check, so the kernel kills the direct provider child when its
  immediate worker dies;
- `with_spawn_observer` reports a `SpawnReceipt` before provider output, with
  provider and operation identity plus the direct child pid and its owned
  process-group id, so a durable host can register external cleanup against
  the correct lease or job.

`ClaudeService::die_with_parent_supported` and
`CodexService::die_with_parent_supported` report the platform guarantee. It is
true only on Linux. macOS, Windows, and other targets require a watchdog that
persists and reconciles spawn receipts. The observer itself must not block; use
a nonblocking channel or bounded local write from the callback.

Parent-death signaling covers the direct provider child, and process-group
identity gives a watchdog the wider run boundary on Unix. Neither mechanism
makes the provider operation exactly once. A crash can happen after an external
effect but before terminal settlement or queue acknowledgement, so durable
hosts must still assume at-least-once delivery and prevent automatic replay
when effect state is possible.

## Provider boundaries

Provider controls are honor-or-refuse. A service either maps a requested model,
sandbox, directory, tool, or resume control exactly, or rejects the request
before work starts.

Current prompt placement:

| Provider path | User prompt | Other instruction data |
|---|---|---|
| Claude | stdin | system-prompt flags in argv |
| Codex fresh | stdin | composed prompt in stdin |
| Codex resume | stdin | composed prompt in stdin |

The host owns launch configuration such as provider home and configuration
directories. Portable request bodies do not carry ambient credentials or
provider-private launch context.

Session handles remain provider-tagged opaque values, but both native adapters
reject a resumed handle beginning with `-` before launch. This ensures a raw
handle, whether supplied by a caller or retained from provider evidence, cannot
be reinterpreted as a Claude or Codex CLI option.

### Host-preassigned sessions

`CallContext::with_preassigned_session` lets a durable host reserve a
provider-tagged session handle before a fresh launch. It is deliberately local
context rather than a portable `Turn` option: an interface caller should not be
able to choose provider persistence identities.

Claude accepts this control for fresh turns because its CLI can honor a
specific `--session-id`. The adapter requires a Claude-tagged canonical
lowercase UUID and rejects any combination with `Turn::resume` before launch.
Success, result-shaped failure, cancellation, process failure, and launch
failure all retain the same assigned handle as terminal evidence.
Provider-generated fresh IDs and caller-facing resume handles keep their
existing paths when no host assignment is present. Providers without an exact
fresh-session primitive must reject this context rather than pretend to honor
it. If Claude reports a different session than the assignment, settlement
fails instead of silently adopting it. The failure retains safe accounting
evidence but omits session evidence entirely; neither disputed handle is
advertised as a verified continuation.

### Child process environment

`ChildEnvironmentPolicy` is host-owned and shared by both provider services.
Its compatibility default inherits the worker's complete environment. The
clear mode removes every ambient variable, then copies only named allowlisted
variables and applies explicit entries. Explicit values are redacted from
`Debug`, errors, and events; invalid keys, NUL-containing values, and
non-Unicode allowlisted values are refused before launch.

A queued or server host should begin with `ChildEnvironmentPolicy::clear()` and
add only what its deployment proves necessary:

- `PATH` when the provider or its tools resolve executables by name;
- locale variables such as `LANG` or `LC_ALL` when their behavior matters;
- exactly one intended authentication path: for example
  `ANTHROPIC_API_KEY` or `CLAUDE_CODE_OAUTH_TOKEN` for Claude, or one of
  `OPENAI_API_KEY`, `CODEX_API_KEY`, or `CODEX_ACCESS_TOKEN` for Codex;
- cloud-provider variables only when intentionally using Bedrock or Vertex;
- an isolated provider configuration directory when using stored login state.

`ClaudeService::with_config_directory` and `CodexService::with_codex_home` are
applied after the policy and therefore remain explicit under a cleared
environment. Nothing automatically preserves `HOME`; retain it only when the
chosen authentication or tool path truly requires the ambient home. This is a
direct-child environment boundary, not an OS sandbox: a same-UID child may
still inspect files, sockets, or process metadata allowed by the platform.

### Provider ambient context

Ambient-context policy is host-owned even when a turn may request a stronger
mode. It is not a portable boolean: the providers expose different mechanisms
and leave different residual inputs.

`CodexAmbientContextPolicy::Automation` applies `--ignore-user-config`,
`--ignore-rules`, `--strict-config`, and `project_doc_max_bytes=0` to both fresh
and resumed execution. The rules flag concerns execpolicy `.rules`; it does not
disable project instructions by itself. The project-document override is what
suppresses `AGENTS.md`. This profile still admits Codex/provider built-ins,
managed host instructions, discovered skill inventory, the explicit prompt,
workspace contents, and the configured child environment. It is a predictable
automation profile, not a claim of hermetic execution.

`CodexOptions::ephemeral` is independent of context policy. It prevents rollout
persistence for fresh and resumed calls. The adapter omits `SessionHandle` from
an ephemeral outcome even if the CLI emits a transient thread id, because that
id does not prove the completed turn can be resumed.

`CodexOptions::output_schema` carries parsed JSON rather than a caller-selected
filesystem path. The adapter validates it against the JSON Schema Draft 2020-12
meta-schema and enforces a 1 MiB serialized limit before launch. For both fresh
and resumed calls it writes the schema to an owner-only temporary file, passes
that path to Codex, retains the file through process settlement, and removes it
on every return or dropped future. Option `Debug` output redacts the schema, and
adapter errors never include its contents or temporary path.

Claude exposes mutually exclusive `ClaudeAmbientContext` modes:

| Mode | Provider behavior | Important residuals |
|---|---|---|
| `Inherit` | Normal Claude loading | User/project/local settings, customizations, memory, managed policy |
| `Hermetic(Project)` | Keep user settings; seal project/local sources and MCP | User settings, managed policy, provider built-ins, dynamic sections moved into the first user message |
| `Hermetic(Full)` | Seal user/project/local setting sources and MCP | Managed policy, provider built-ins, explicit prompts, and some state outside setting sources |
| `Safe` | Disable CLAUDE.md, skills, plugins, hooks, MCP, custom agents/commands, and auto-memory | Managed policy, normal OAuth/keychain auth, model selection, built-in tools and permissions |
| `Bare` | Minimal scripted mode with additional services disabled | API-key or explicit-helper auth, provider built-ins, explicit prompts/tools, workspace and environment visibility |

The Claude service combines a host baseline with the requested turn mode.
Inherited requests keep the host baseline; project hermetic can strengthen to
full hermetic; safe and bare cannot replace one another or a host-required
hermetic posture. Conflicts fail during validation. None of these modes hides
workspace files or child environment variables; compose them with filesystem
authority and `ChildEnvironmentPolicy` where the provider supports those
boundaries.

Claude adapter errors expose fixed category-based messages. Provider result
text, stderr, wrapper I/O details, command arguments, working directories, and
session handles are not copied into `AgentError.message` or tracing. Raw child
diagnostics are discarded by default; any future diagnostic observer must be
an explicit host-private, sensitive-data policy rather than part of the
portable error surface.

## Filesystem authority

`FilesystemAuthority` represents read-only, workspace-write, and full-access
requests without leaking provider wrapper flags into portable request DTOs.
`AuthorityPolicy` supplies the host ceiling and approved writable roots.
`AuthorityLayer` rejects excessive authority before provider work, while the
Codex provider repeats the policy check at its launch boundary so middleware
ordering or omission cannot bypass it. Requests are refused rather than silently
narrowed because changing write semantics can change task behavior.

Codex maps the authorized portable level to its concrete sandbox. Read-only is
the default ceiling; workspace-write requires explicit host policy and explicit
request roots must remain beneath host-approved roots. Full access is never
path-contained and therefore requires an explicit full-access ceiling.

Claude does not expose an equivalent filesystem sandbox through this adapter.
Its tool patterns and extra directories remain provider-specific controls and
must not be presented as enforcement of the portable filesystem contract.
Network, tool, subprocess, and interactive approval policy should be added only
when concrete provider mechanisms can enforce their semantics.

## Middleware opportunities

The sections above describe shipped behavior. The sections below are open
seams, except where a subsection states that a provider adapter already
implements the contract.

### Context assembly

Build prompt context deterministically from named sources. Record source ids,
ordering, byte or token budgets, and truncation. This layer should make the
effective input reproducible without placing provider-private data in the
portable turn.

### Budget reservation and reconciliation

Reserve an estimated budget at admission, then reconcile with terminal success
or failure evidence. Failure accounting is first-class because a capped or
failed provider call may still report spend and token use.

### Output contracts

Validate structured provider output before it becomes a successful outcome.
Distinguish provider execution failure from settlement failure and retain the
raw provider evidence only behind a host-private diagnostic boundary.

Both provider adapters already implement part of this contract. Claude
validates a requested JSON schema as draft-07 before launch, returns the
validated payload as `TurnOutcome::structured` separate from the prose, and
fails settlement when a schema was requested and no payload arrived.

The Codex adapter requires exactly one terminal event and requires it to be the
last parsed event. Only `turn.completed` produces `TurnOutcome`; `turn.failed`
becomes an effectful typed failure, including rollout-budget classification and
validated session evidence. Missing, repeated, contradictory, or nonterminal
terminal sequences fail settlement without promoting partial assistant text to
successful output or advertising uncertain continuation evidence.

### Event policy

Fan out events to metrics, logs, progress, and receipts while applying per-sink
redaction and overflow policy. Terminal settlement remains independent of
event delivery.

### Circuit breaking

Open on typed provider or launch failures, not invalid requests or rejected
authority. Half-open probes must be explicitly non-effectful or isolated.

### Retry and fallback

These are unsafe by default. A retry or alternate provider is admissible only
when effect evidence proves no external effect occurred, or when the operation
has a provider-backed idempotency contract. `EffectState::Possible` forbids an
automatic replay. Without an idempotency contract, only `EffectState::None`
permits an automatic retry; a deterministic error classification alone does
not prove that an earlier step in the turn produced no effects.

## Service laws

Tests should preserve these properties:

1. Invalid input and unsupported controls fail before provider launch.
2. Cloned admission services share one capacity boundary.
3. Cancellation and deadline return only after the inner service settles.
4. Caller drop does not abandon an owned provider future.
5. Panic normalization preserves operation identity and effect conservatism.
6. Outer errors retain the strongest available cause and partial evidence.
7. Missing usage, cost, duration, and turn counts remain absent.
8. Provider session values never appear in `Debug` or adapter DTOs by default.
9. Middleware behavior is unchanged when called through an interface adapter.
10. The core dependency graph contains no interface implementation.
