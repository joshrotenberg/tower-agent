//! Tower-native services and middleware for finite agent operations.
//!
//! The crate defines a protocol-neutral request, response, error, event, and
//! middleware vocabulary. It does not contain an MCP server, transport, task
//! store, scheduler, or provider implementation. Applications may project the
//! services onto MCP, a CLI, HTTP, or call them directly.
//!
//! # One finite turn
//!
//! ```
//! use tower::ServiceExt;
//! use tower_agent::{AgentRequest, EchoService, Turn};
//!
//! # async fn example() -> Result<(), tower_agent::AgentError> {
//! let outcome = EchoService
//!     .oneshot(AgentRequest::new(Turn::new("inspect this repository")))
//!     .await?;
//!
//! assert_eq!(outcome.output, "inspect this repository");
//! // Evidence a provider did not supply stays absent rather than zero.
//! assert_eq!(outcome.cost, None);
//! # Ok(())
//! # }
//! ```
//!
//! # Composing execution policy
//!
//! Ordering is semantic: supervision owns the call after the caller drops it,
//! panic normalization sits inside observation so receipts see a typed
//! terminal failure, and admission wraps deadline handling so capacity stays
//! occupied through cleanup.
//!
//! ```
//! use tower::ServiceBuilder;
//! use tower_agent::EchoService;
//! use tower_agent::layer::{
//!     AdmissionLayer, CatchPanicLayer, DeadlineLayer, ObserveLayer, ReceiptObserver,
//!     SuperviseLayer, ValidateTurnLayer,
//! };
//!
//! let service = ServiceBuilder::new()
//!     .layer(SuperviseLayer::new())
//!     .layer(ObserveLayer::new(ReceiptObserver::default()))
//!     .layer(CatchPanicLayer::new())
//!     .layer(AdmissionLayer::single_flight())
//!     .layer(DeadlineLayer::new())
//!     .layer(ValidateTurnLayer::new())
//!     .service(EchoService);
//! # let _ = service;
//! ```
//!
//! # Reading a failure
//!
//! A failure carries four independent dimensions, so a caller can decide what
//! is safe to do next rather than guessing from a message.
//!
//! ```
//! use tower_agent::{AgentError, EffectState, ErrorKind, FailurePhase};
//!
//! let error = AgentError::busy();
//! assert_eq!(error.kind, ErrorKind::Busy);
//! // Nothing was launched, so a caller may retry this one safely.
//! assert_eq!(error.phase, FailurePhase::Admission);
//! assert_eq!(error.effects, EffectState::None);
//! ```

mod authority;
mod environment;
mod error;
mod event;
mod fake;
pub mod layer;
mod process;
mod request;
mod response;
mod service;
mod session;

pub use authority::{AuthorityPolicy, FilesystemAuthority, RequestsFilesystemAuthority};
pub use environment::{ChildEnvironmentError, ChildEnvironmentPolicy, ResolvedChildEnvironment};
pub use error::{AgentError, EffectState, ErrorKind, FailurePhase, MAX_RETRY_AFTER};
pub use event::{
    AgentEvent, BoundedEventReceiver, EventLimits, EventObserver, EventSendError, EventSink,
};
pub use fake::{EchoService, FakeOptions, FakeService, FakeStep, FakeTerminal, NamedFakeService};
pub use process::{SpawnObserver, SpawnReceipt};
pub use request::{AgentRequest, CallContext, OperationId, Turn};
pub use response::{Cost, FailureEvidence, TerminalEvidence, TokenUsage, TurnOutcome};
pub use service::{BoxTurnService, TurnRequest};
pub use session::SessionHandle;
pub use tokio_util::sync::CancellationToken;
