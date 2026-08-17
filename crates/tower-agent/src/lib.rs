//! Tower-native services and middleware for finite agent operations.
//!
//! The crate defines a protocol-neutral request, response, error, event, and
//! middleware vocabulary. It does not contain an MCP server, transport, task
//! store, scheduler, or provider implementation. Applications may project the
//! services onto MCP, a CLI, HTTP, or call them directly.

mod error;
mod event;
mod fake;
pub mod layer;
mod request;
mod response;
mod service;
mod session;

pub use error::{AgentError, EffectState, ErrorKind, FailurePhase};
pub use event::{AgentEvent, EventObserver, EventSendError, EventSink};
pub use fake::EchoService;
pub use request::{AgentRequest, CallContext, OperationId, Turn};
pub use response::{Cost, TokenUsage, TurnOutcome};
pub use service::{BoxTurnService, TurnRequest};
pub use session::SessionHandle;
pub use tokio_util::sync::CancellationToken;
