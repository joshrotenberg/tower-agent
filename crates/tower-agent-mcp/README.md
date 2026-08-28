# tower-agent-mcp

MCP adapter vocabulary for `tower-agent`. The design record is
[`docs/mcp.md`](../../docs/mcp.md).

## Intention

A provider `SessionHandle` is private, provider-tagged, and never crosses a
protocol boundary. That rule is already enforced, and its consequence is that
a client is told a session exists and given no way to name it, so continuing a
conversation over a protocol is not merely unimplemented, it is
unrepresentable. An adapter has to mint a public name.

That name is a capability over conversation history, not a database key: a
resumed turn can read the prior context. So every store operation takes a
`Scope`, and the scope check, rather than the identifier, is the security
boundary.

- `Scope::Session` ends with the connection. Safe without configuration, and
  continuations do not survive a reconnect.
- `Scope::Principal` survives reconnects and requires an authenticated subject.
- The two are distinct variants because transport session ids and authenticated
  subjects are unrelated namespaces. Sharing one string type would let a
  session named like a principal resolve that principal's continuations.

An implementation that ignores the scope has built a bearer-token store, where
holding an identifier is enough to resume anyone's conversation. That is
reasonable for a single-user host and serious for a shared one, so it should be
chosen rather than inherited.

The adapter mints identity. The host owns persistence, which is the same split
`tower-agent-workflow` makes with its opaque jobs.

## Status

Unpublished workspace crate, and the first step of the record only. There is no
transport here yet, deliberately: the session boundary is worth getting right
before MCP is in the picture. `InMemoryContinuationStore` is the default for a
host that has not chosen otherwise; it is bounded and does not survive a
restart.
