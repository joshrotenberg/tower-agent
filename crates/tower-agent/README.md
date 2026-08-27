# tower-agent

Tower service vocabulary and middleware for one finite agent operation. This
crate is the execution kernel: it defines the request, outcome, failure,
event, and layer types, and contains no provider, transport, or scheduler.

## Intention

One finite turn is the execution atom:

```text
Service<AgentRequest<Turn<O>>, Response = TurnOutcome, Error = AgentError>
```

The concrete service identifies the provider, and provider controls live in
the generic options type `O`, so no layer can silently rewrite a control it
cannot see.

What the crate is responsible for:

- **Terminal contracts.** `TurnOutcome` and `AgentError` record what actually
  happened, across four independent dimensions for failure: kind, phase,
  effect state, and partial evidence. Missing evidence stays absent and is
  never synthesized as zero.
- **Execution-lifetime middleware.** A concern belongs in the stack when it
  must own, gate, or observe the in-flight future: supervision, observation,
  panic normalization, admission, deadlines, validation, and filesystem
  authority. Everything else composes around the call.
- **Redaction.** Provider session values redact in `Debug`, and an interface
  mints its own public continuation identifier rather than exposing one.

What it deliberately excludes: retry, fallback, buffering, caching, and
coalescing. Those are unsafe until typed effect evidence proves an operation
produced no external effect; see the retry and fallback notes in
[`docs/architecture.md`](../../docs/architecture.md).

## Status

Experimental `0.1`. The contracts are typed and tested; API stability is not
yet a goal.
