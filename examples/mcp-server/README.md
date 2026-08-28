# mcp-server-example

Serves `tower-agent` over MCP on stdio. The first thing in this workspace an
MCP client can actually talk to.

Everything it composes is ordinary library code: the tools come from
`tower-agent-mcp`, the policy stack is the order documented in
[`docs/resilience.md`](../../docs/resilience.md), and the providers are the
same services any other host builds.

## Running it

```sh
cargo build -p mcp-server-example
```

Then point a client at the binary:

```json
{
  "command": "target/debug/agent-mcp",
  "env": { "AGENT_MCP_PROVIDER": "claude" }
}
```

An MCP client launches a server with an environment block and no control over
argv, so configuration is environment variables.

| Variable | Default | Effect |
|---|---|---|
| `AGENT_MCP_PROVIDER` | `fake` | `fake`, `claude`, or `codex` |
| `AGENT_MCP_VERBOSE` | unset | any value publishes provider-authored text |
| `AGENT_MCP_CONCURRENCY` | `2` | turns admitted at once |
| `AGENT_MCP_TIMEOUT_SECS` | unset | deadline applied to every turn |

The default provider is the fake one. A server that spends money the first time
someone runs it is a bad default, so the real providers are opt-in.

## Two choices worth knowing

**The transport is bidirectional.** `BidirectionalStdioTransport`, not
`StdioTransport`, because the planning tool elicits missing values and that is
a server-to-client request. Only the bidirectional transport wires a client
requester into the request context; with the plain one the planning tool
compiles and fails at runtime.

**The scope is fixed.** stdio carries exactly one client for the life of the
process, so `FixedScope::stdio()` is correct here. Over HTTP it would not be:
every caller would land in one scope, and the scope check would then let any of
them continue any other's conversation.

## A transport bug this example found

Piping a batch of requests and closing stdin immediately loses the responses:

```sh
# prints nothing
echo '{"jsonrpc":"2.0","id":1,"method":"initialize",...}' | agent-mcp

# prints the response
( echo '{"jsonrpc":"2.0","id":1,"method":"initialize",...}'; sleep 2 ) | agent-mcp
```

`BidirectionalStdioTransport` dispatches each request on a spawned task so the
run loop stays free to service elicitation. On EOF it breaks out of that loop
and returns, without awaiting tasks still in flight, so the runtime shuts down
underneath them and their responses are never written. With more than one
request outstanding the shutdown can also panic with `JoinHandle polled after
completion`.

Real MCP clients keep stdin open for the life of the session, so this does not
affect normal use. It does affect scripted testing, and it is why the test
below reads each response before closing its side of the pipe.

## Tests

A stdio server blocks on its input, so it cannot be run to completion by the
example gate. `tests/serve_stdio.rs` drives the real transport over an
in-memory pipe instead, one request at a time, terminating when the client half
closes. No credentials, no client, and it runs in CI like any other test.
