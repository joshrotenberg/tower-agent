//! MCP adapter vocabulary for `tower-agent`.
//!
//! The design record is [`docs/mcp.md`][record]. This crate currently holds
//! only the first step of it: public continuation identity and the scope check
//! that governs it. There is no transport here yet, deliberately, because the
//! session boundary is the part worth getting right in isolation.
//!
//! A provider [`SessionHandle`](tower_agent::SessionHandle) is private,
//! provider-tagged, and never crosses a protocol boundary. A client that is
//! told a session exists and given no way to name it cannot continue a
//! conversation at all, so an adapter has to mint a public name. That name is
//! a capability over conversation history rather than a database key, which is
//! why every operation takes a [`Scope`].
//!
//! [record]: https://github.com/joshrotenberg/tower-agent/blob/main/docs/mcp.md
//!
//! # Example
//!
//! ```
//! use tower_agent::SessionHandle;
//! use tower_agent_mcp::{ContinuationStore, InMemoryContinuationStore, Scope};
//!
//! # #[tokio::main] async fn main() {
//! let store = InMemoryContinuationStore::new();
//!
//! // A turn settled and produced a resumable session.
//! let id = store
//!     .mint(SessionHandle::new("claude", "provider-private"), Scope::session("conn-1"))
//!     .await
//!     .expect("mint");
//!
//! // The connection that minted it can resume.
//! let resumed = store
//!     .resolve(id.clone(), Scope::session("conn-1"))
//!     .await
//!     .expect("resolve");
//! assert!(resumed.is_some());
//!
//! // Another connection holding the same identifier cannot.
//! let other = store
//!     .resolve(id, Scope::session("conn-2"))
//!     .await
//!     .expect("resolve");
//! assert!(other.is_none());
//! # }
//! ```

#![deny(missing_docs)]

mod continuation;

pub use continuation::{
    ContinuationError, ContinuationId, ContinuationStore, DEFAULT_CAPACITY,
    InMemoryContinuationStore, InvalidContinuationId, Scope, StoreFuture,
};
