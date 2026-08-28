use std::sync::atomic::{AtomicU64, Ordering};

use tower_agent::{AgentEvent, EventSendError, EventSink};
use tower_mcp::RequestContext;

use crate::ProviderMessages;

/// Reports [`AgentEvent`]s as MCP progress notifications.
///
/// The two contracts already agree on the hard part. `EventSink::try_emit` is
/// synchronous and must not block, and `report_progress_sync` sends on a
/// bounded channel and discards the result, so neither can stall the turn it
/// observes. An observation that cannot be delivered is dropped, which is what
/// makes it safe to observe a call from inside it.
///
/// # Progress is liveness, not content
///
/// MCP progress notifications indicate that work is happening. They are not a
/// content channel, and this sink does not try to make them one: the terminal
/// result carries the output. So the progress value counts observations rather
/// than estimating completion, and `total` is always absent because an agent
/// turn has no known total.
///
/// Under [`ProviderMessages::Redacted`], which is the default, the message
/// says what kind of thing happened and not what it said. That is the same
/// rule the projection applies, and it matters more here: a status or warning
/// is provider-authored text that the terminal result would never publish, and
/// streamed output would escape the redaction that a later failure applies.
pub struct ProgressEvents {
    context: RequestContext,
    provider_messages: ProviderMessages,
    emitted: AtomicU64,
}

impl ProgressEvents {
    /// Report progress for the request `context` belongs to.
    #[must_use]
    pub fn new(context: RequestContext) -> Self {
        Self {
            context,
            provider_messages: ProviderMessages::Redacted,
            emitted: AtomicU64::new(0),
        }
    }

    /// Choose whether provider-authored text appears in progress messages.
    #[must_use]
    pub fn with_provider_messages(mut self, provider_messages: ProviderMessages) -> Self {
        self.provider_messages = provider_messages;
        self
    }

    fn message(&self, event: &AgentEvent) -> String {
        match self.provider_messages {
            ProviderMessages::Redacted => structural(event),
            ProviderMessages::Verbatim => verbatim(event),
        }
    }
}

/// What happened, never what it said.
fn structural(event: &AgentEvent) -> String {
    match event {
        AgentEvent::Started => "started".to_string(),
        AgentEvent::OutputDelta { .. } => "output".to_string(),
        AgentEvent::ThinkingDelta { .. } => "thinking".to_string(),
        AgentEvent::ToolStarted { .. } => "tool started".to_string(),
        // A turn number is the provider's own counter, not text it authored.
        AgentEvent::TurnStarted { number } => format!("turn {number}"),
        AgentEvent::Status { .. } => "status".to_string(),
        // Accounting is numbers. Publishing it early tells a caller what is
        // being spent while there is still time to stop.
        AgentEvent::Usage { usage } => match usage.total() {
            Some(total) => format!("usage {total}"),
            None => "usage".to_string(),
        },
        AgentEvent::Warning { .. } => "warning".to_string(),
        _ => "event".to_string(),
    }
}

fn verbatim(event: &AgentEvent) -> String {
    match event {
        AgentEvent::OutputDelta { text } => text.clone(),
        AgentEvent::ThinkingDelta { text } => text.clone(),
        AgentEvent::ToolStarted { name } => format!("tool {name}"),
        AgentEvent::Status { message } => message.clone(),
        AgentEvent::Warning { message } => format!("warning: {message}"),
        other => structural(other),
    }
}

impl EventSink for ProgressEvents {
    fn try_emit(&self, event: AgentEvent) -> Result<(), EventSendError> {
        // No progress token means the client did not ask for progress. There
        // was never a consumer and there will not be one, so reporting the
        // channel closed lets a caller stop producing observations.
        if self.context.progress_token().is_none() {
            return Err(EventSendError::Closed);
        }

        // MCP requires progress to increase. A turn has no known total, so
        // this counts what has been observed rather than estimating how much
        // remains.
        let progress = self.emitted.fetch_add(1, Ordering::Relaxed) + 1;
        let message = self.message(&event);
        self.context
            .report_progress_sync(progress as f64, None, Some(&message));

        // `report_progress_sync` discards the send result, so a notification
        // dropped for a full channel is not observable here and cannot be
        // reported as Full. Dropping an observation is the intended outcome
        // either way.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use tower_agent::TokenUsage;
    use tower_mcp::ServerNotification;
    use tower_mcp::context::notification_channel;
    use tower_mcp::protocol::{ProgressToken, RequestId};

    use super::*;

    /// A context that actually delivers, so tests assert what a client would
    /// receive rather than how a string was built.
    fn observing() -> (
        RequestContext,
        tokio::sync::mpsc::Receiver<ServerNotification>,
    ) {
        let (tx, rx) = notification_channel(16);
        let context = RequestContext::new(RequestId::Number(1))
            .with_progress_token(ProgressToken::Number(42))
            .with_notification_sender(tx);
        (context, rx)
    }

    fn delivered(
        rx: &mut tokio::sync::mpsc::Receiver<ServerNotification>,
    ) -> (f64, Option<f64>, String) {
        match rx.try_recv().expect("a progress notification") {
            ServerNotification::Progress(params) => (
                params.progress,
                params.total,
                params.message.unwrap_or_default(),
            ),
            other => panic!("expected progress, got {other:?}"),
        }
    }

    #[test]
    fn redacted_progress_says_what_happened_and_not_what_it_said() {
        let secret = "host-private-session";
        let (context, mut rx) = observing();
        let sink = ProgressEvents::new(context);

        for event in [
            AgentEvent::OutputDelta {
                text: secret.to_string(),
            },
            AgentEvent::ThinkingDelta {
                text: secret.to_string(),
            },
            AgentEvent::ToolStarted {
                name: secret.to_string(),
            },
            AgentEvent::Status {
                message: secret.to_string(),
            },
            AgentEvent::Warning {
                message: secret.to_string(),
            },
        ] {
            assert_eq!(sink.try_emit(event), Ok(()));
            let (_, _, message) = delivered(&mut rx);
            assert!(!message.contains(secret), "leaked: {message}");
            assert!(!message.is_empty());
        }
    }

    #[test]
    fn structural_facts_survive_redaction() {
        let (context, mut rx) = observing();
        let sink = ProgressEvents::new(context);

        // Numbers are not provider-authored text, and they are the part a
        // progress indicator actually needs.
        sink.try_emit(AgentEvent::TurnStarted { number: 3 })
            .expect("emit");
        assert_eq!(delivered(&mut rx).2, "turn 3");

        sink.try_emit(AgentEvent::Usage {
            usage: TokenUsage {
                input: Some(20),
                output: Some(22),
                ..TokenUsage::default()
            },
        })
        .expect("emit");
        assert_eq!(delivered(&mut rx).2, "usage 42");
    }

    #[test]
    fn verbatim_publishes_provider_text() {
        let (context, mut rx) = observing();
        let sink = ProgressEvents::new(context).with_provider_messages(ProviderMessages::Verbatim);

        sink.try_emit(AgentEvent::Status {
            message: "reticulating".to_string(),
        })
        .expect("emit");
        assert_eq!(delivered(&mut rx).2, "reticulating");
    }

    #[test]
    fn progress_increases_and_never_claims_a_total() {
        let (context, mut rx) = observing();
        let sink = ProgressEvents::new(context);

        for expected in 1..=3 {
            sink.try_emit(AgentEvent::Started).expect("emit");
            let (progress, total, _) = delivered(&mut rx);
            // MCP requires a strictly increasing value, and an agent turn has
            // no known total to report against.
            assert_eq!(progress, f64::from(expected));
            assert_eq!(total, None);
        }
    }

    #[test]
    fn a_request_without_a_progress_token_reports_closed() {
        let (tx, mut rx) = notification_channel(4);
        let context = RequestContext::new(RequestId::Number(1)).with_notification_sender(tx);
        let sink = ProgressEvents::new(context);

        // Nothing will ever be delivered, and saying so lets a producer stop.
        assert_eq!(
            sink.try_emit(AgentEvent::Started),
            Err(EventSendError::Closed)
        );
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn a_full_channel_drops_the_observation_without_failing_the_turn() {
        let (tx, rx) = notification_channel(1);
        let context = RequestContext::new(RequestId::Number(1))
            .with_progress_token(ProgressToken::Number(42))
            .with_notification_sender(tx);
        let sink = ProgressEvents::new(context);

        // Fill the channel, then keep emitting. Observation is advisory, so a
        // backed-up consumer must cost observations and never the turn.
        for _ in 0..8 {
            assert_eq!(sink.try_emit(AgentEvent::Started), Ok(()));
        }
        drop(rx);
    }
}
