# tower-agent

An agent server exposed as an MCP surface. An agent and an MCP server are the
same shape: expose agentic work as an MCP contract and let any client drive it,
from a one-shot CLI to a hosted server.

Built bottom-up from one primitive and enhanced. See
[the spec](docs/design/spec.md) for the full design.

## The atom: the `prompt` tool

The core is one MCP tool. It requires a `prompt` and optionally takes any
parameter the backend takes, so the surface is a faithful projection of what the
backend can do:

```
prompt(
  prompt: string,       // required: the task/message
  agent: string?,       // select a configured agent's defaults
  system: string?,      // system prompt
  model: string?,
  effort: string?,      // low | medium | high
  allowed_tools: [string]?,
  cwd: string?,
  session: string?,     // continue/resume a thread
  ...
)
```

Config supplies the defaults, so a call usually carries little.

## Agents

An **agent** is a named bundle of default parameters plus a base prompt. It is
config, not code. A call selects one with `agent` and may override any of its
parameters.

```toml
[defaults]
model = "sonnet"

[agents.tester]
system = "You run the tests and report failures with the exact output."
allowed_tools = ["Bash(cargo test:*)"]
# config_dir = ".agent/env/tester"   # this agent's own CLAUDE_CONFIG_DIR
```

## Backends

A `Backend` is the one seam where a model backend lives; the core names none.
Reference backends live in their own crates: claude (`claude-wrapper`) and, later,
codex (`codex-wrapper`). The `StubBackend` runs no model, echoing the resolved
parameters, so the whole server works without a live model.

## Layout

```
crates/tower-agent     the atom, config, Backend trait, MCP projection (no backend dep)
crates/agent           the reference binary
```

Enhancements build up on the atom: sessions, scheduling, inter-agent
communication, observability. See the spec.

## Use

```
just list                       # configured agents
just run "read the repo"        # one prompt through the stub backend
just serve                      # serve over stdio (MCP)
just check                      # fmt, clippy, test
```

## License

MIT OR Apache-2.0.
