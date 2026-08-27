use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use tokio_util::sync::CancellationToken;

use crate::{EventObserver, SessionHandle};

static NEXT_OPERATION_ID: AtomicU64 = AtomicU64::new(1);

/// Process-local identity for one service call.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct OperationId(u64);

impl OperationId {
    /// A process-unique identifier for one call.
    ///
    /// Unique within this process only. A host that correlates across
    /// processes must carry its own identity.
    pub fn next() -> Self {
        Self(NEXT_OPERATION_ID.fetch_add(1, Ordering::Relaxed))
    }

    /// Rebuild an identifier a host already assigned.
    pub const fn from_u64(value: u64) -> Self {
        Self(value)
    }

    /// The underlying value, for correlation outside this crate.
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl Default for OperationId {
    fn default() -> Self {
        Self::next()
    }
}

impl fmt::Display for OperationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Host-local state that accompanies a portable operation body.
#[derive(Debug)]
pub struct CallContext {
    operation_id: OperationId,
    deadline: Option<Instant>,
    cancellation: CancellationToken,
    events: EventObserver,
    preassigned_session: Option<SessionHandle>,
}

impl CallContext {
    /// A context with a fresh operation id and no other constraints.
    pub fn new() -> Self {
        Self::default()
    }

    /// The identifier correlating every observation of this call.
    pub fn operation_id(&self) -> OperationId {
        self.operation_id
    }

    /// Use a host-assigned identifier instead of the generated one.
    pub fn with_operation_id(mut self, operation_id: OperationId) -> Self {
        self.operation_id = operation_id;
        self
    }

    /// The instant this call must finish by, if a host set one.
    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    /// Require the call to finish by `deadline`.
    ///
    /// Enforcement belongs to a layer; setting this alone does not cancel.
    pub fn with_deadline(mut self, deadline: Instant) -> Self {
        self.deadline = Some(deadline);
        self
    }

    /// The token a caller can use to cancel this call.
    pub fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }

    /// Attach a caller-owned cancellation token.
    pub fn with_cancellation(mut self, cancellation: CancellationToken) -> Self {
        self.cancellation = cancellation;
        self
    }

    /// The sink receiving incremental observations of this call.
    pub fn events(&self) -> &EventObserver {
        &self.events
    }

    /// Attach an observer for incremental events.
    ///
    /// The sink is called synchronously and must not block.
    pub fn with_events(mut self, events: EventObserver) -> Self {
        self.events = events;
        self
    }

    /// A host-owned provider session identity reserved before a fresh launch.
    ///
    /// Provider adapters must either honor this exactly or reject it before
    /// launch. It is local execution context, not portable caller input, and
    /// conflicts with a turn that already requests resume.
    pub fn preassigned_session(&self) -> Option<&SessionHandle> {
        self.preassigned_session.as_ref()
    }

    /// Record a session handle assigned before the provider ran.
    ///
    /// A host that mints continuation identity itself sets this so a
    /// failure before settlement still reports a resumable session.
    pub fn with_preassigned_session(mut self, session: SessionHandle) -> Self {
        self.preassigned_session = Some(session);
        self
    }
}

impl Default for CallContext {
    fn default() -> Self {
        Self {
            operation_id: OperationId::next(),
            deadline: None,
            cancellation: CancellationToken::new(),
            events: EventObserver::default(),
            preassigned_session: None,
        }
    }
}

/// A common local envelope around a typed agent operation.
#[derive(Debug)]
pub struct AgentRequest<T> {
    /// Host-owned execution constraints that every layer can read.
    pub context: CallContext,
    /// The provider-facing payload.
    pub body: T,
}

impl<T> AgentRequest<T> {
    /// A request with a fresh context.
    pub fn new(body: T) -> Self {
        Self {
            context: CallContext::default(),
            body,
        }
    }

    /// A request reusing a context a host already built.
    pub fn with_context(body: T, context: CallContext) -> Self {
        Self { context, body }
    }

    /// Replace the body while preserving the context.
    ///
    /// The operation id, deadline, and cancellation survive, so an adapter
    /// can translate the payload without losing the call's identity.
    pub fn map_body<U>(self, map: impl FnOnce(T) -> U) -> AgentRequest<U> {
        AgentRequest {
            context: self.context,
            body: map(self.body),
        }
    }
}

/// One finite agent turn.
///
/// Provider-specific controls belong in `O`. A concrete provider service
/// chooses its own options type instead of forcing every provider through a
/// least-common-denominator set of flags.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Turn<O = ()> {
    /// The user prompt for this turn.
    pub prompt: String,
    /// Directory the provider runs in. `None` accepts the host default.
    pub working_directory: Option<PathBuf>,
    /// Session to resume. `None` starts a fresh conversation.
    pub session: Option<SessionHandle>,
    /// Provider-specific controls, or `()` when the turn is portable.
    pub options: O,
}

impl Turn<()> {
    /// A portable turn carrying only a prompt.
    pub fn new(prompt: impl Into<String>) -> Self {
        Self {
            prompt: prompt.into(),
            working_directory: None,
            session: None,
            options: (),
        }
    }

    /// Attach provider-specific options, changing the turn's type.
    ///
    /// This is how a portable `Turn` becomes one only a particular
    /// provider can run.
    pub fn with_options<O>(self, options: O) -> Turn<O> {
        Turn {
            prompt: self.prompt,
            working_directory: self.working_directory,
            session: self.session,
            options,
        }
    }
}

impl<O> Turn<O> {
    /// Run the turn in `path`.
    ///
    /// Subject to the host filesystem authority, which is checked before
    /// launch and can refuse this directory.
    pub fn in_directory(mut self, path: impl Into<PathBuf>) -> Self {
        self.working_directory = Some(path.into());
        self
    }

    /// Continue an existing conversation.
    ///
    /// The handle must come from the same provider; another provider's
    /// handle fails validation.
    pub fn resume(mut self, session: SessionHandle) -> Self {
        self.session = Some(session);
        self
    }
}
