# tower-agent: spec

A clean-room spec for the agent server, built bottom-up from one primitive, in
standard terms (agent, tool, prompt, session, schedule, channel). Everything here
is validated by a stochastic-first experiment in the `repol` repo (its
`docs/design/agent-as-mcp.md` and `agent-first-run.md`); this rebuild is ordering
and factoring, not new guesses.

## Thesis

An agent and an MCP server are the same shape. Expose agentic work as an MCP
contract and let any client drive it. This is the Claude Code desktop model as
client/server. We do not start from that whole model; we start from its atom and
enhance.

## The atom: the `prompt` tool

The core is one MCP tool. It **requires a `prompt`** and **optionally takes any
parameter the backend takes**. Its input mirrors the model wrapper's call
(`claude-wrapper`'s `QueryCommand`), so the surface is a faithful, minimal
projection of what the backend can do:

```
prompt(
  prompt: string,          // required: the task/message
  agent: string?,          // select a configured agent's defaults (see Config)
  system: string?,         // system prompt
  append_system: string?,  // appended to the system prompt
  model: string?,
  effort: string?,         // low | medium | high
  allowed_tools: [string]?,
  disallowed_tools: [string]?,
  add_dirs: [string]?,     // extra directories the agent may access
  max_turns: int?,         // bound the agentic turns
  cwd: string?,
  timeout: int?,           // per-run seconds, overriding the backend default
  session: string?,        // continue/resume a thread
  ...                      // any other wrapper parameter, all optional
)
```

A named `agent` that does not exist is an error, and an empty prompt is
rejected, so a typo fails loudly rather than running with the wrong config.

### Execution: sync, async, streaming

A prompt can run for minutes, so the caller chooses how to wait, and blocking is
never forced. Both use native MCP mechanisms, not a bespoke run registry.

- **Sync**: call `prompt` normally; it blocks and returns the outcome. Fine for
  short prompts and simple clients.
- **Async**: the tool declares `taskSupport = optional`, so a client MAY call it
  as a task. It returns a task handle immediately; the client polls, waits, or
  cancels through the standard MCP task methods. The runtime spawns and stores
  the run; no run registry of our own yet (that arrives with sessions and
  observability, with the run id as the spine).
- **Streaming**: opt-in via the request's progress token. When present, assistant
  text is forwarded as progress notifications as it is produced; when absent,
  nothing streams and only the final outcome returns. The backend seam has a
  streaming path (emit events, return the final outcome) with a non-streaming
  default, so a backend that cannot stream still conforms. The event set starts
  at text deltas and status, and will grow to tool-use and turn boundaries.

That is the whole MVP: an MCP server (or one-shot CLI) that runs a prompt through
a backend, with defaults from config. The result is the backend's output.
Structure on the result (`reply`/`posts`) arrives with inter-agent communication,
not in the atom. Everything below is built up from this one tool.

## Agents

An **agent** is the standard primitive: a named bundle of default parameters plus
a base prompt. It is config, not code. Selecting one (`prompt(agent="tester",
...)`) pre-fills the parameters; the call can override any of them.

```toml
[agents.tester]
system = "You run the tests and report failures with the exact output."
model  = "haiku"
allowed_tools = ["Bash(cargo test:*)"]
```

"Define agents better than the app's implicit sessions" reduces to this: an agent
is a typed, versioned config profile, not whatever you happened to type.

## Config and defaults

Parameter precedence, most specific wins: (1) the call's explicit params, (2) the
selected agent's defaults, (3) server defaults, (4) the backend's defaults.

```toml
[defaults]
model  = "sonnet"
effort = "medium"
cwd    = "."
```

### Environment per agent (the config-dir piece)

Each agent (or server instance) can run in its own environment instead of a
stripped-down "hermetic" one, by giving the backend its own `CLAUDE_CONFIG_DIR`.
That directory carries a curated, isolated environment: its own agents, MCP
servers, permissions, instructions, and its own session history. This is a
first-class config concern, not an afterthought:

```toml
[agents.tester]
config_dir = ".agent/env/tester"   # its own ~/.claude
```

Empirically confirmed: the config dir and its sessions relocate, but auth does
not inherit even on macOS, so each environment is given a token
(`CLAUDE_CODE_OAUTH_TOKEN` / `ANTHROPIC_API_KEY`) or a one-time login. Provisioned
auth is a feature for a controlled server, not just a cost: an agent starts with
exactly the environment you handed it.

## Backend seam

The one place a model backend lives. It takes a resolved parameter set and runs
it.

```rust
#[async_trait]
trait Backend: Send + Sync {
    async fn run(&self, params: &Params) -> Result<Outcome, BackendError>;
}
```

Reference impls in their own crates so the core has no backend dependency:
**claude** (`claude-wrapper`) and, later, **codex** (`codex-wrapper`). A `Backend`
is a drop-in; the core never names one. A `StubBackend` in the core runs no model
(it echoes the resolved parameters), so the whole server is testable without a
live model. Session continuity is the backend's job (claude's `--resume`, keyed
per session).

## Enhancements

Layered on the atom, in order, each earning its place.

### Sessions

A session is a resumable thread with an agent. The `session` param plus a
registry: a fresh call mints one, passing it continues it (the backend resumes),
and the registry (id, agent, turns, last summary) makes threads listable and
re-enterable. repol-validated: real memory across turns.

### Scheduling

An agent can carry a cron schedule; the server fires its prompt on cadence. A
trigger that calls the atom, nothing more. `[agents.tester] schedule = "0 */6 * * *"`.

### Inter-agent communication

Agents talk over channels. For this the result gains structure,
`{summary, reply, posts}`: `posts` are messages to channels, with directed `to`
(reaches an agent regardless of subscription) and threaded `reply_to`. An agent
subscribes to channels; a message on one is another trigger that calls the atom.
A depth bound stops runaway cascades (a storm becomes a log, not a hang). Turns
get recent channel history as context so an agent sees the thread, not just the
last message. All repol-validated as findings.

### Observability

Everything the server is doing, readable over the same MCP surface: a **feed** of
channel messages (agent-to-agent traffic), the **sessions** registry (threads),
the **agents** list, and idle state (whether work is in flight). Observation is
separate from delivery: a directed message still lands in the feed even though
only its addressee reacts.

## The MCP surface

| tool | what |
|---|---|
| `prompt` | run a prompt (the atom); `agent` selects defaults, `session` continues a thread |
| `agents` | list configured agents |
| `sessions` | list threads, or one by id |
| `broadcast` | post to a channel |
| `feed` | recent channel messages |

Later, as MCP tiers land: channels and sessions as subscribable resources,
prompts as invocation templates, and prompt runs as tasks with `input_required`
gates.

## The reference binary

`agent`: loads config (defaults and named agents), picks a backend, points at a
repo. `list`, `run <prompt> [--agent] [--session]`, `serve` (stdio MCP +
scheduler). A new capability is a new agent in config.

## Non-goals for v1 (deferred on purpose)

Add each only when the stochastic version fails at it, never pre-built: gate /
approval / budget / isolation (`repol` has these; it stays separate as the
reference for what we already learned we need); concurrency across agents (serial
until it limits); persistence (in-memory until a store is genuinely needed);
fleet / remote transport (server-per-repo over a socket) after the single-server
surface.

## Crate structure

```
tower-agent            the atom, config, Backend trait, MCP projection (no backend dep)
tower-agent-claude     the claude Backend
tower-agent-codex      the codex Backend (later)
agent                  the reference binary
```

## Build plan

Bottom-up; the atom first, exercised with a stub backend so the core never needs
a live model.

- **M0** the `prompt` tool over a `Backend` trait; a stub backend; config with
  defaults and agent profiles; a one-shot CLI. Useful on its own. (done)
- **M1** the claude backend; `serve` (stdio MCP); real prompts on a repo;
  `config_dir`-per-agent with token auth.
- **M2** sessions: our minted id decoupled from the backend token, the registry
  (`SessionStore`, memory + a JSON file store), backend resume, the `sessions`
  tool, and `agent sessions`. (done)
- **M3** scheduling: agents with a cron `schedule` + `schedule_prompt`, a
  per-agent scheduler task (croner, optional seconds), ticks that share a session
  for memory, `serve` starting the scheduler, and `agent tick`. (done)
- **M4** inter-agent communication: structured result (done, #6), channels +
  the subscribe trigger (done, #7, one-hop), then directed threads (#8), the
  cascade + depth bound (#9), turn context (#10), and the feed/broadcast tools
  (#11, started).
- **M5** observability rounded out (feed/idle over MCP); the codex backend.
- **M6** provoke on a real repo; add only the mechanical layers failures name.

## Open questions

- Resolved: the atom's result is `{summary, reply, posts}` plus the session.
  `reply` is the answer (the model's text for a plain prompt), `summary` a log
  line, `posts` empty until an agent participates in the bus. Structured
  production via a JSON schema is deferred to channels, to keep the streaming
  path clean (a schema would stream JSON, not prose).
- How much shared scaffolding should dictate versus leave to the agent's prompt
  (the directed-reply instruction once overrode an agent's own prompt).
- Where the gate re-enters, if it does: an agent (stochastic) or a server
  primitive (mechanical), once a real world-writing agent runs here.
