# Contributing

`tower-agent` is an experimental Tower-native execution library. Changes keep
provider execution, policy middleware, and interface adaptation separate.

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
warning-free rustdoc, and package assembly for the publishable crates.

## Change discipline

- Keep `tower-agent` protocol-neutral. Interface libraries may be development
  dependencies for examples and tests, never normal or optional core
  dependencies.
- State middleware ordering, clone-sharing, readiness, cancellation, and error
  preservation semantics explicitly.
- Treat agent work as effectful. Retry, fallback, buffering, caching, and
  coalescing require evidence that duplicate effects are impossible or safe.
- Provider controls are honor-or-refuse. Do not silently weaken authority,
  sandbox, limits, working-directory, or output requirements.
- Use deterministic fakes for service laws. Use fake binaries for process
  ownership and cleanup claims.
- Add core backlog only when a current consumer or provider proves a
  protocol-neutral requirement.

## Pull requests

Use a feature branch and a conventional-commit prefix. Describe the invariant
or boundary changed, the middleware ordering impact, and the checks run. Do not
add AI attribution footers.

## Releases

The `Release PR` workflow creates or updates a draft release PR after changes
land on `main`. It keeps `tower-agent`, `tower-agent-claude`, and
`tower-agent-codex` on one version and maintains the repository changelog.

The workflow only runs `release-plz release-pr`. It cannot publish crates,
create tags, or create GitHub releases. When publishing is enabled later, add a
separate `release-plz release` job after configuring crates.io trusted
publishing. `release_always = false` ensures that job releases only after a
release PR is merged.

Release PR preparation is staged but disabled by default. To enable it:

1. Add a fine-grained token limited to this repository, with Contents and Pull
   requests read/write access, as the `RELEASE_PLZ_TOKEN` repository secret.
2. Set the `RELEASE_PLZ_ENABLED` repository variable to `true`.
3. Run `Release PR` manually, or let the next push to `main` run it.

Using the dedicated token also lets the generated release PR trigger CI. The
broader repository setting that allows every GitHub Actions workflow to create
and approve pull requests can remain disabled.
