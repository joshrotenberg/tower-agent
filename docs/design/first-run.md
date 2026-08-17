# First run: provoking tower-agent on a real repo

> Historical experiment for the preserved MCP-first host. Its bus, scheduler,
> and server backlog are not the `tower-agent` kernel roadmap. See
> [`../README.md`](../README.md) for the documentation authority map.

The stochastic-first method: point the fabric at a real repository, watch what it
does, and add only the mechanical layers the failures name. This is the findings
log from the first run (#15).

## Setup

- Target: the `git-spawn` crate (a real Rust wrapper over the git CLI) and its
  real open backlog of "add commands" issues.
- Agents: an `analyst` and a `critic` pointed at the repo (`cwd`), read-only
  (backend default permissions; no write tools granted).
- Backends: claude (sonnet, then haiku). One earlier probe used the scout on
  tower-agent itself.

## What happened

1. Scout on tower-agent itself, asked for the biggest risk: flagged that
   `Server::validate()` does not canonicalize or validate `cwd` / `add_dirs`
   before handing them to the backend. A grounded, correct observation about our
   own code.
2. Analyst on git-spawn issue #35 (porcelain commands): read the codebase and
   produced a concrete `git clean` plan, citing `src/command/rm.rs` as the
   analog, the `GitCommand` trait, `build_command_args()`, and the exact wiring
   in `command.rs` / `repo.rs` / `lib.rs`, with test placement. Genuinely useful,
   grounded output. Took ~50s (an earlier attempt ran past 120s).
3. Bus debate on a git-spawn design question (raw output vs a typed parser): the
   analyst reacted with an excellent code-grounded argument (cited the crate's
   documented "raw output by default, parsing opt-in" principle). But the
   conversation did not cascade: the feed stopped after the analyst. One haiku
   turn that read the codebase took ~84s.

## Findings

### F1. Grounded output is high quality (positive)

Agents did real, code-cited work on a real repo: an implementation plan and a
design argument that a maintainer could act on. The atom plus a backend, pointed
at a repo with read tools, is genuinely useful. No fabric change needed.

### F2. Latency and cost are unbounded

Grounded turns took 50 to over 120 seconds, even on haiku, because the agent
explores the codebase over many tool calls. Nothing bounds the turns or the
spend. Confirms the need for budget and turn caps (#20), and argues that a real
agent should almost always carry `max_turns`. Consider a per-agent default.

### F3. Cascade propagation is structural, and easy to misconfigure

A multi-agent conversation only propagates if the agents are wired for it:
subscriptions plus directed posts plus prompts that instruct posting. Here the
critic was not subscribed and the analyst was not told to post to it, so the
debate stopped after one hop. The operator's natural-language "critic, push back"
does not route; routing is `subscriptions` and `to`. This is a real ergonomics
gap: the happy path (a conversation) requires several things to line up. See
new issue.

### F4. In-flight observability is poor from the CLI

`agent broadcast` blocks until the bus is idle and only prints the feed at the
end. For real (slow) multi-agent work this is the wrong shape: you cannot watch
progress. The right model is `serve` plus polling the `feed` and `runs` tools
(the async surface we built), and a CLI that can tail the feed live. Motivates a
`agent watch` / live-feed command.

### F5. Trust-boundary validation gap (from F1's scout)

`cwd` and `add_dirs` flow from the `Call` to the backend without canonicalization
or containment. A buggy or hostile client could pass `../../etc`. See new issue.

### F6. No world-write gate (not provoked, by choice)

The agents stayed read-only only because the backend's default permission mode
denies writes. tower-agent itself has no gate: an agent granted `allowed_tools`
including `Write` would write unguarded. Not provoked here (writing to a real repo
was out of scope), but confirmed by reading `Server`: there is no gate. Confirms
#19.

## Actions

- New issues: #35 (cascade ergonomics, F3), #34 (path validation, F5).
- Confirmed existing: #20 (budget/turn caps, F2), #19 (world-write gate, F6).
- No fabric change was forced by this run; the failures name layers already on
  the backlog, plus two new ergonomics/safety gaps. That is the stochastic-first
  loop working: the mechanical layers are the residue of observed failures.
