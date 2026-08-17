# Contributing

`tower-agent` is an experimental Tower-native execution kernel. Changes should
keep provider execution, policy middleware, and protocol projection as separate
layers.

## Setup

Install Rust 1.90 or newer with `rustfmt` and Clippy. The repository uses
[`just`](https://github.com/casey/just) for its local check entrypoint and can
use [Lefthook](https://github.com/evilmartians/lefthook) for the pre-push hook.

```text
rustup toolchain install 1.90.0 --profile minimal --component rustfmt,clippy
lefthook install
just check
```

`just check` is the local equivalent of GitHub Actions. It verifies formatting,
the core dependency boundary, Clippy with all features, all workspace tests,
and warning-free rustdoc.

## Change discipline

- Keep `tower-agent` protocol-neutral. `tower-mcp` may be a development
  dependency for examples and tests, never a normal or optional core
  dependency.
- State middleware ordering, clone-sharing, readiness, cancellation, and error
  preservation semantics explicitly.
- Treat agent work as effectful. Retry, fallback, buffering, caching, and
  coalescing require evidence that duplicate effects are impossible or safe.
- Provider controls are honor-or-refuse. Do not silently weaken authority,
  sandbox, limits, working-directory, or output requirements.
- Use deterministic fakes for service laws. Use fake binaries for process
  ownership and cleanup claims.
- Keep historical compatibility-host ideas out of the core backlog unless a
  current consumer proves a protocol-neutral requirement.

## Pull requests

Use a feature branch and a conventional-commit prefix. Describe the invariant
or boundary changed, the middleware ordering impact, and the checks run. Do not
add AI attribution footers.
