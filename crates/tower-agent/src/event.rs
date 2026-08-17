use std::fmt;
use std::sync::Arc;

use tokio::sync::mpsc;

use crate::TokenUsage;

#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum AgentEvent {
    /// The service accepted the operation and began an attempt. This does not
    /// prove that a provider subprocess has spawned successfully.
    Started,
    OutputDelta {
        text: String,
    },
    ThinkingDelta {
        text: String,
    },
    ToolStarted {
        name: String,
    },
    TurnStarted {
        number: u32,
    },
    Status {
        message: String,
    },
    Usage {
        usage: TokenUsage,
    },
    Warning {
        message: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum EventSendError {
    #[error("event observer is full")]
    Full,
    #[error("event observer is closed")]
    Closed,
}

/// A nonblocking destination for incremental provider observations.
pub trait EventSink: Send + Sync + 'static {
    fn try_emit(&self, event: AgentEvent) -> Result<(), EventSendError>;
}

#[derive(Clone)]
pub struct EventObserver(Arc<dyn EventSink>);

impl EventObserver {
    pub fn new(sink: impl EventSink) -> Self {
        Self(Arc::new(sink))
    }

    pub fn channel(capacity: usize) -> (Self, mpsc::Receiver<AgentEvent>) {
        let (sender, receiver) = mpsc::channel(capacity);
        (Self::new(ChannelEventSink(sender)), receiver)
    }

    pub fn try_emit(&self, event: AgentEvent) -> Result<(), EventSendError> {
        self.0.try_emit(event)
    }
}

impl Default for EventObserver {
    fn default() -> Self {
        Self::new(NoopEventSink)
    }
}

impl fmt::Debug for EventObserver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("EventObserver").field(&"..").finish()
    }
}

struct NoopEventSink;

impl EventSink for NoopEventSink {
    fn try_emit(&self, _event: AgentEvent) -> Result<(), EventSendError> {
        Ok(())
    }
}

struct ChannelEventSink(mpsc::Sender<AgentEvent>);

impl EventSink for ChannelEventSink {
    fn try_emit(&self, event: AgentEvent) -> Result<(), EventSendError> {
        self.0.try_send(event).map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => EventSendError::Full,
            mpsc::error::TrySendError::Closed(_) => EventSendError::Closed,
        })
    }
}
