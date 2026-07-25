# tower-agent

An agent server exposed as an MCP surface. An agent and an MCP server are the
same shape: expose agentic work as an MCP contract and let any client drive it,
from a one-shot CLI to a hosted server.

Built bottom-up from one primitive and enhanced. The full design is in
[docs/design/spec.md](docs/design/spec.md); the roadmap is the
[milestones and issues](https://github.com/joshrotenberg/tower-agent/issues).

## The atom: the `prompt` tool

The core is one MCP tool. It requires a `prompt` and optionally takes any
parameter the backend takes, so the surface is a faithful projection of what the
backend can do:

```
prompt(
  prompt: string,          // required: the task/message
  agent: string?,          // select a configured agent's defaults
  system: string?,         // system prompt
  append_system: string?,  // appended to the system prompt
  model: string?,
  effort: string?,         // low | medium | high
  allowed_tools: [string]?,
  disallowed_tools: [string]?,
  add_dirs: [string]?,     // extra directories the agent may access
  max_turns: int?,
  cwd: string?,
  timeout: int?,           // per-run seconds
  session: string?,        // continue/resume a thread
)
```

Config supplies the defaults, so a call usually carries little. A named `agent`
that does not exist is an error, and an empty prompt is rejected.

## Agents

An **agent** is a named bundle of default parameters plus a base prompt. It is
config, not code. A call selects one with `agent` and may override any parameter.

```toml
[defaults]
model = "sonnet"

[agents.backlog]
system = "You groom the backlog for this repository."
cwd    = "../foo"                    # the repo this agent tends
schedule = "0 */6 * * *"             # fire on a cadence (see Scheduling)
schedule_prompt = "Review the open issues and pick the next one to work on."
```

Point an agent at a repo with `cwd`, give it a role in `system`, and either
prompt it directly or let its schedule tick it.

## Sessions

A session is a resumable thread. A fresh call mints a session id (`s1`, `s2`,
...) and returns it; pass it back to continue the thread with memory. The id is
ours; the backend's own resume token is an internal detail of the store, which
keeps sessions backend-portable.

```
$ agent run "My name for you is Orion. Acknowledge in one word."
{ "text": "Orion.", "session": "s1" }
$ agent run "What name did I give you?" --session s1
{ "text": "Orion.", "session": "s1" }
$ agent sessions
s1     agent=-        turns=2   Orion.
```

The `MemorySessionStore` is the default; the CLI uses a `FileSessionStore` so
threads resume across invocations.

## Scheduling

An agent with a `schedule` (cron, with optional 6-field seconds) fires its
`schedule_prompt` on cadence when the server runs. Ticks share a session, so a
scheduled agent accumulates memory. `agent tick <agent>` fires one run
immediately.

## Execution: sync, async, streaming

A prompt can run for minutes, so the caller chooses how to wait, on native MCP
mechanisms.

- **Sync**: call `prompt`; it blocks and returns the outcome.
- **Async**: the tool declares `taskSupport = optional`, so a client MAY call it
  as a task, getting a handle to poll, wait on, or cancel.
- **Streaming**: opt-in via the request progress token. Present, and assistant
  text streams as progress notifications; absent, and only the final outcome
  returns.

## The MCP surface

| tool | what |
|---|---|
| `prompt` | run a prompt (the atom); `agent` selects defaults, `session` continues a thread |
| `agents` | list configured agents |
| `sessions` | list threads, or one by id |

More tools (`broadcast`, `feed`) arrive with inter-agent communication.

## Backends

A `Backend` is the one seam where a model backend lives; the core names none.

- `tower-agent-claude`: runs prompts through the Claude Code CLI via
  `claude-wrapper`, with streaming.
- `StubBackend` (in the core): runs no model and echoes the resolved parameters,
  a dry run and the basis for tests.
- A codex backend is planned.

## Layout

```
crates/tower-agent          the atom, config, sessions, scheduling, Backend trait, MCP surface
crates/tower-agent-claude   the claude backend
crates/agent                the reference binary
```

## Use

```
just list                       # configured agents
just run "read the repo"        # one prompt (stub backend by default in the recipe)
just serve                      # serve over stdio (MCP), with the scheduler
just check                      # fmt, clippy, test

agent run "<prompt>" [--agent NAME] [--model M] [--session s1]
agent sessions                  # the session registry
agent tick <agent>              # fire a scheduled prompt once
agent serve                     # stdio MCP + scheduler
agent --backend stub ...        # run without a live model
```

## Status

M0 through M3 are done: the prompt tool, agents, sessions, scheduling, and the
async/streaming execution model. Inter-agent communication, observability, and a
codex backend are next; see the
[milestones](https://github.com/joshrotenberg/tower-agent/milestones).

## License

MIT OR Apache-2.0.
