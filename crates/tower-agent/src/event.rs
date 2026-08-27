use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::sync::mpsc;

use crate::TokenUsage;

#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
/// An incremental observation of a call in progress.
///
/// Events are advisory. They are delivered on a nonblocking path that
/// drops rather than stalls a turn, so no event is proof of anything:
/// terminal facts come from the response or the failure evidence.
///
/// Non-exhaustive: providers report observations this set does not name
/// yet, and adding one must not break a consumer.
pub enum AgentEvent {
    /// The service accepted the operation and began an attempt. This does not
    /// prove that a provider subprocess has spawned successfully.
    Started,
    /// A fragment of the turn's output text.
    OutputDelta {
        /// The fragment, not the accumulated output.
        text: String,
    },
    /// A fragment of the provider's reasoning, when it exposes any.
    ThinkingDelta {
        /// The fragment, not the accumulated reasoning.
        text: String,
    },
    /// The provider began invoking a tool.
    ToolStarted {
        /// Provider-reported tool name, not validated by this crate.
        name: String,
    },
    /// The provider began a turn inside this call.
    TurnStarted {
        /// Provider-side turn number within this one call.
        number: u32,
    },
    /// Provider progress text with no defined structure.
    Status {
        /// The status text as the provider phrased it.
        message: String,
    },
    /// Token accounting reported partway through the call.
    ///
    /// Superseded by the accounting on the terminal result.
    Usage {
        /// Accounting as of this observation, not a running total.
        usage: TokenUsage,
    },
    /// The provider reported a condition it did not treat as fatal.
    Warning {
        /// The warning as the provider phrased it.
        message: String,
    },
}

impl AgentEvent {
    /// Bytes of caller-visible payload this event carries.
    ///
    /// Only the variable-length parts count. Discriminants and fixed-width
    /// numbers cannot grow with provider output, which is the thing worth
    /// bounding.
    pub fn payload_bytes(&self) -> usize {
        match self {
            Self::OutputDelta { text } | Self::ThinkingDelta { text } => text.len(),
            Self::ToolStarted { name } => name.len(),
            Self::Status { message } | Self::Warning { message } => message.len(),
            _ => 0,
        }
    }
}

/// Host-owned byte ceilings for incremental observation.
///
/// Counting events is not enough: one event can carry an entire provider
/// output, so a channel bounded at sixteen items is still unbounded in bytes.
/// Dropping an observation is acceptable where dropping terminal settlement
/// is not, so exceeding either ceiling drops the event and reports `Full`
/// rather than failing the turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EventLimits {
    /// Largest single payload accepted.
    pub max_event_bytes: usize,
    /// Largest total payload retained by events that have been emitted but
    /// not yet consumed.
    pub max_queued_bytes: usize,
}

impl EventLimits {
    /// Ceilings for one event and for the unconsumed backlog.
    pub const fn new(max_event_bytes: usize, max_queued_bytes: usize) -> Self {
        Self {
            max_event_bytes,
            max_queued_bytes,
        }
    }
}

impl Default for EventLimits {
    fn default() -> Self {
        // Large enough that ordinary streaming is untouched, small enough
        // that a runaway provider cannot retain unbounded memory.
        Self::new(256 * 1024, 4 * 1024 * 1024)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
/// Why an event could not be delivered.
///
/// Both variants mean the observation was dropped. Neither fails the
/// turn: observation is advisory.
pub enum EventSendError {
    #[error("event observer is full")]
    /// A ceiling was reached. The turn continues without this event.
    Full,
    #[error("event observer is closed")]
    /// The consumer is gone. Later events will be dropped too.
    Closed,
}

/// A nonblocking destination for incremental provider observations.
pub trait EventSink: Send + Sync + 'static {
    /// Deliver one event without blocking.
    ///
    /// Called on the turn's own path, so an implementation that blocks
    /// stalls the call it is observing. Drop instead.
    fn try_emit(&self, event: AgentEvent) -> Result<(), EventSendError>;
}

#[derive(Clone)]
/// A cloneable handle to an [`EventSink`].
///
/// Cloning shares one sink, so every clone observes the same call.
pub struct EventObserver(Arc<dyn EventSink>);

impl EventObserver {
    /// Observe through a caller-supplied sink.
    pub fn new(sink: impl EventSink) -> Self {
        Self(Arc::new(sink))
    }

    /// A channel bounded by item count only.
    ///
    /// One event can carry an entire provider output, so this bounds items
    /// but not memory. Use [`bounded_channel`](Self::bounded_channel) when
    /// the payload size is not already known to be small.
    pub fn channel(capacity: usize) -> (Self, mpsc::Receiver<AgentEvent>) {
        let (sender, receiver) = mpsc::channel(capacity);
        (Self::new(ChannelEventSink(sender)), receiver)
    }

    /// A channel that bounds retained bytes as well as item count.
    ///
    /// The returned receiver must be used to consume events: it releases the
    /// byte budget as each one is taken, which is what makes the aggregate
    /// ceiling a high-water mark rather than a lifetime total.
    pub fn bounded_channel(capacity: usize, limits: EventLimits) -> (Self, BoundedEventReceiver) {
        let (sender, receiver) = mpsc::channel(capacity);
        let queued = Arc::new(AtomicUsize::new(0));
        let sink = BoundedChannelEventSink {
            sender,
            limits,
            queued: Arc::clone(&queued),
        };
        (Self::new(sink), BoundedEventReceiver { receiver, queued })
    }

    /// Deliver one event, dropping it if a ceiling or the consumer refuses.
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

/// Receiver half of [`EventObserver::bounded_channel`].
pub struct BoundedEventReceiver {
    receiver: mpsc::Receiver<AgentEvent>,
    queued: Arc<AtomicUsize>,
}

impl BoundedEventReceiver {
    /// Await the next event, releasing its bytes from the backlog budget.
    pub async fn recv(&mut self) -> Option<AgentEvent> {
        let event = self.receiver.recv().await?;
        self.queued
            .fetch_sub(event.payload_bytes(), Ordering::Relaxed);
        Some(event)
    }

    /// Take a ready event without waiting, releasing its bytes.
    pub fn try_recv(&mut self) -> Option<AgentEvent> {
        let event = self.receiver.try_recv().ok()?;
        self.queued
            .fetch_sub(event.payload_bytes(), Ordering::Relaxed);
        Some(event)
    }

    /// Payload bytes currently retained by unconsumed events.
    pub fn queued_bytes(&self) -> usize {
        self.queued.load(Ordering::Relaxed)
    }
}

impl fmt::Debug for BoundedEventReceiver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BoundedEventReceiver")
            .field("queued_bytes", &self.queued_bytes())
            .finish()
    }
}

struct BoundedChannelEventSink {
    sender: mpsc::Sender<AgentEvent>,
    limits: EventLimits,
    queued: Arc<AtomicUsize>,
}

impl EventSink for BoundedChannelEventSink {
    fn try_emit(&self, event: AgentEvent) -> Result<(), EventSendError> {
        let bytes = event.payload_bytes();
        if bytes > self.limits.max_event_bytes {
            return Err(EventSendError::Full);
        }
        // Reserve before sending so two concurrent emitters cannot both see
        // room and jointly exceed the ceiling.
        let mut current = self.queued.load(Ordering::Relaxed);
        loop {
            if current + bytes > self.limits.max_queued_bytes {
                return Err(EventSendError::Full);
            }
            match self.queued.compare_exchange_weak(
                current,
                current + bytes,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }
        self.sender.try_send(event).map_err(|error| {
            self.queued.fetch_sub(bytes, Ordering::Relaxed);
            match error {
                mpsc::error::TrySendError::Full(_) => EventSendError::Full,
                mpsc::error::TrySendError::Closed(_) => EventSendError::Closed,
            }
        })
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

#[cfg(test)]
mod tests {
    use super::*;

    fn delta(size: usize) -> AgentEvent {
        AgentEvent::OutputDelta {
            text: "x".repeat(size),
        }
    }

    #[test]
    fn only_variable_payload_counts() {
        assert_eq!(delta(100).payload_bytes(), 100);
        assert_eq!(AgentEvent::Started.payload_bytes(), 0);
        assert_eq!(AgentEvent::TurnStarted { number: 7 }.payload_bytes(), 0);
    }

    #[tokio::test]
    async fn an_oversized_event_is_dropped_not_delivered() {
        let (observer, mut events) = EventObserver::bounded_channel(8, EventLimits::new(64, 4096));
        assert_eq!(observer.try_emit(delta(65)), Err(EventSendError::Full));
        assert_eq!(observer.try_emit(delta(64)), Ok(()));
        assert_eq!(events.try_recv().map(|e| e.payload_bytes()), Some(64));
        assert_eq!(events.queued_bytes(), 0);
    }

    #[tokio::test]
    async fn retained_bytes_are_bounded_even_when_the_item_count_is_not() {
        // Sixteen slots, but only room for four events worth of bytes: a
        // count-bounded channel alone would retain four times as much.
        let (observer, mut events) =
            EventObserver::bounded_channel(16, EventLimits::new(1024, 400));
        for _ in 0..4 {
            assert_eq!(observer.try_emit(delta(100)), Ok(()));
        }
        assert_eq!(events.queued_bytes(), 400);
        assert_eq!(observer.try_emit(delta(100)), Err(EventSendError::Full));

        // Consuming releases the budget, so the ceiling is a high-water mark
        // rather than a lifetime total.
        assert!(events.recv().await.is_some());
        assert_eq!(events.queued_bytes(), 300);
        assert_eq!(observer.try_emit(delta(100)), Ok(()));
    }

    #[tokio::test]
    async fn a_rejected_send_releases_its_reservation() {
        let (observer, events) = EventObserver::bounded_channel(1, EventLimits::new(1024, 4096));
        assert_eq!(observer.try_emit(delta(10)), Ok(()));
        // The channel is full by item count, not by bytes.
        assert_eq!(observer.try_emit(delta(10)), Err(EventSendError::Full));
        assert_eq!(
            events.queued_bytes(),
            10,
            "the refused send left no reservation"
        );
    }

    #[test]
    fn the_default_leaves_ordinary_streaming_alone() {
        let limits = EventLimits::default();
        assert!(limits.max_event_bytes >= 64 * 1024);
        assert!(limits.max_queued_bytes > limits.max_event_bytes);
    }
}
