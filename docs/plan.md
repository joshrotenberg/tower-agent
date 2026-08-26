# Planning

Decision record for the `tower-agent-plan` crate and the series that builds
it (#112 through #116). The crate is the pure front half of one agent call:
it turns partially specified turn data into a complete typed turn body,
structured missing requirements, or structured diagnostics. The kernel and
the provider adapters remain the effectful back half.

## Origin and retargeting

The design descends from an external agent-run specification whose pipeline
was:

```text
PartialRequest -> resolve -> CompleteRequest -> compile -> AgentPlan
    (program + argv + cwd + env + stdin) -> execute once -> ExecutionResult
```

That specification compiles provider semantics away before execution so a
generic process runner can execute the plan. tower-agent exists to reject
exactly that split: the provider adapters are the settlement boundary, and
compiling to argv outside them would duplicate the least interesting part of
each adapter while forfeiting typed evidence, effect states, cancellation
with awaited settlement, redaction, and honor-or-refuse validation.

The retargeted pipeline keeps the front half and replaces the back half with
the existing kernel:

```text
PartialTurn layers
    -> resolve                      pure, tower-agent-plan
Complete | Missing | Invalid
    -> provider planner fold        pure, tower-agent-plan provider features
    -> adapter preflight            pure, per configured service
ReadyTurn: Turn<ClaudeOptions> | Turn<CodexOptions>
    -> RoutedTurnService            tower-agent middleware and adapters
TurnOutcome | AgentError
```

Routing is the last planning-side step and the first execution-side one. It
holds one configured service per provider, refuses an unregistered provider
and a session whose tag disagrees with its committed provider, and never
retries or falls back: an effectful failure must not reach a second
provider.

The compile target of planning is the typed portable turn body, never a
process specification.

## What replaces each cut concept

| Specification concept | Replacement here |
|---|---|
| `AgentPlan` (program, argv, stdin) | `Turn<O>` plus the provider adapter |
| Generic process Executor | Provider services, `DeadlineLayer`, `CallContext` |
| `ExecutionResult` (exit code, raw bytes) | `TurnOutcome` and `AgentError` evidence |
| `EnvironmentPlan` (inherit, set, remove) | `ChildEnvironmentPolicy` |
| `raw_args` escape hatch | Removed; incompatible with honor-or-refuse |
| Result Consumption surface | `ObserveLayer`, receipts, and event sinks |
| Timeout and cancellation as executor options | `CallContext` deadline and cancellation |
| MCP adapter and elicitation protocol | Downstream composition over requirement data |

Raw argv passthrough is removed rather than deferred. An ordered token escape
hatch would tunnel under the adapter guarantees (stdin prompt policy, session
handle checks, redaction). A host that needs unmodeled CLIs needs a separate
dumb execution path with the weaker result type, outside this workspace.

## Resolution

Layers merge from lowest to highest precedence: provider baseline defaults,
application defaults, one selected profile, the explicit request, and
elicited answers for paths still unbound.

- A later bound scalar replaces an earlier scalar.
- Nested groups merge by field, not by group replacement.
- A bound list replaces lower layers whole; a bound empty list is a real
  value.
- Omission means no binding from that layer. Empty strings, empty lists, and
  `false` are bindings.
- Answers fill only paths still unbound after every layer. Answering a bound
  path or an unknown requirement is invalid.
- A profile/explicit provider mismatch is invalid, never an implicit
  conversion. The provider is not inferred from a resume tag.
- A provider baseline cannot select or change the provider; its `provider`
  field is ignored during merge.
- Invalid bound values take priority over eliciting more values.

Requirements are adapter-neutral data with stable ids. The requirements
remaining after resolution are the effective callable signature of a partial
turn or profile: a resolved profile is a partially applied service whose
request type is the answers to its remaining requirements.

The shared vocabulary is deliberately small: prompt, model name, working
directory, additional directories, resume, and filesystem authority. A
setting enters the shared vocabulary only when its semantics are genuinely
shared; everything else stays in the provider option mirrors.

A provider option mirror carries only what the shared vocabulary cannot
express. A setting the shared vocabulary already carries never appears in a
mirror, so no concrete option field is reachable from two planning paths
with undefined precedence between them.

The two-provider pressure test settled three candidate promotions:

- `system_prompt` stays provider-typed. Claude has a real system-prompt
  control with replace and append forms; Codex simulates one by prepending
  to the stdin prompt. A shared field would promise semantics only one
  provider honors.
- `effort` stays provider-typed. The Codex adapter does not expose an
  effort control, so a shared field would be Claude-only in practice.
- Tool allow/deny lists stay provider-typed. They are Claude-specific
  patterns, and the architecture already forbids presenting them as a
  portable sandbox; `permissions.filesystem` is refused for Claude for the
  same reason.

## Alternate providers

`ReadyTurn` is non-exhaustive and grows one feature-gated variant per
provider planner. Nothing in the pipeline assumes a CLI-backed provider: a
REST-backed provider adds an options type, a service implementing the same
kernel contract, a planner fold, and a `ReadyTurn` variant. The seam to
widen when a third provider lands is `ProviderId`, currently a closed enum;
whether it stays an enum with more variants or becomes an open identifier is
a decision for that provider's series, recorded here so it is not made by
accident.

## Resume representation

Planning layers carry `ResumeBinding`, a provider-tagged raw resume value, in
their serde surface. This is a v1 decision:

- resolution checks the tag against the resolved provider and rejects blank
  or hyphen-leading values, and the adapters re-validate at launch, so the
  wire value is defense in depth rather than a trusted input;
- `Debug` redacts the value, matching `SessionHandle` discipline;
- the intended refinement is a host-minted continuation reference translated
  to a `SessionHandle` outside the planner, in the direction of #92.

## Fixtures

The JSON corpus under `crates/tower-agent-plan/tests/fixtures/planning` is
the executable specification for the merge laws. Each file is one case:
layers in, expected outcome out, with exact requirement ids and order for
missing cases and exact diagnostic codes and order for invalid cases.
Provider planners extend the corpus rather than replacing it.

## Series

1. #112 vocabulary and layered resolver (this record).
2. #113 Codex provider planner behind a `codex` feature.
3. #114 Claude provider planner behind a `claude` feature.
4. #115 pure preflight validation exposed by both adapters, wired into
   compiler validation.
5. #116 routed turn service over `ReadyTurn`, covering the scope of #95.
