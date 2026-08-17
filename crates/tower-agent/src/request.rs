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
    pub fn next() -> Self {
        Self(NEXT_OPERATION_ID.fetch_add(1, Ordering::Relaxed))
    }

    pub const fn from_u64(value: u64) -> Self {
        Self(value)
    }

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
}

impl CallContext {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn operation_id(&self) -> OperationId {
        self.operation_id
    }

    pub fn with_operation_id(mut self, operation_id: OperationId) -> Self {
        self.operation_id = operation_id;
        self
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    pub fn with_deadline(mut self, deadline: Instant) -> Self {
        self.deadline = Some(deadline);
        self
    }

    pub fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }

    pub fn with_cancellation(mut self, cancellation: CancellationToken) -> Self {
        self.cancellation = cancellation;
        self
    }

    pub fn events(&self) -> &EventObserver {
        &self.events
    }

    pub fn with_events(mut self, events: EventObserver) -> Self {
        self.events = events;
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
        }
    }
}

/// A common local envelope around a typed agent operation.
#[derive(Debug)]
pub struct AgentRequest<T> {
    pub context: CallContext,
    pub body: T,
}

impl<T> AgentRequest<T> {
    pub fn new(body: T) -> Self {
        Self {
            context: CallContext::default(),
            body,
        }
    }

    pub fn with_context(body: T, context: CallContext) -> Self {
        Self { context, body }
    }

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
    pub prompt: String,
    pub working_directory: Option<PathBuf>,
    pub session: Option<SessionHandle>,
    pub options: O,
}

impl Turn<()> {
    pub fn new(prompt: impl Into<String>) -> Self {
        Self {
            prompt: prompt.into(),
            working_directory: None,
            session: None,
            options: (),
        }
    }

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
    pub fn in_directory(mut self, path: impl Into<PathBuf>) -> Self {
        self.working_directory = Some(path.into());
        self
    }

    pub fn resume(mut self, session: SessionHandle) -> Self {
        self.session = Some(session);
        self
    }
}
