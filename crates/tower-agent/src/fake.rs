use std::future::{Ready, ready};
use std::task::{Context, Poll};

use tower::Service;

use crate::{AgentError, AgentEvent, AgentRequest, EffectState, Turn, TurnOutcome};

/// A deterministic service that returns the prompt as its output.
#[derive(Clone, Copy, Debug, Default)]
pub struct EchoService;

impl Service<AgentRequest<Turn>> for EchoService {
    type Response = TurnOutcome;
    type Error = AgentError;
    type Future = Ready<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: AgentRequest<Turn>) -> Self::Future {
        if request.context.cancellation().is_cancelled() {
            return ready(Err(AgentError::cancelled(EffectState::None)));
        }
        if request.body.prompt.trim().is_empty() {
            return ready(Err(AgentError::invalid_request("prompt must not be empty")));
        }
        if request.body.session.is_some() {
            return ready(Err(AgentError::unsupported(
                "EchoService does not support session continuation",
            )));
        }
        if request.body.working_directory.is_some() {
            return ready(Err(AgentError::unsupported(
                "EchoService does not use a working directory",
            )));
        }

        let _ = request.context.events().try_emit(AgentEvent::Started);
        let _ = request.context.events().try_emit(AgentEvent::OutputDelta {
            text: request.body.prompt.clone(),
        });
        ready(Ok(TurnOutcome::new(request.body.prompt)))
    }
}

#[cfg(test)]
mod tests {
    use tower::ServiceExt;

    use super::*;
    use crate::{CallContext, ErrorKind, EventObserver};

    #[tokio::test]
    async fn echoes_a_turn_and_emits_observations() {
        let (events, mut receiver) = EventObserver::channel(2);
        let request =
            AgentRequest::with_context(Turn::new("hello"), CallContext::new().with_events(events));

        let outcome = EchoService.oneshot(request).await.expect("echo succeeds");
        assert_eq!(outcome.output, "hello");
        assert_eq!(receiver.recv().await, Some(AgentEvent::Started));
        assert_eq!(
            receiver.recv().await,
            Some(AgentEvent::OutputDelta {
                text: "hello".into()
            })
        );
    }

    #[tokio::test]
    async fn refuses_an_empty_prompt_before_effects() {
        let error = EchoService
            .oneshot(AgentRequest::new(Turn::new("  ")))
            .await
            .expect_err("empty prompt must fail");
        assert_eq!(error.kind, ErrorKind::InvalidRequest);
        assert_eq!(error.effects, EffectState::None);
    }

    #[tokio::test]
    async fn a_full_event_observer_does_not_block_completion() {
        let (events, mut receiver) = EventObserver::channel(1);
        let request =
            AgentRequest::with_context(Turn::new("hello"), CallContext::new().with_events(events));

        let outcome = EchoService.oneshot(request).await.expect("echo succeeds");
        assert_eq!(outcome.output, "hello");
        assert_eq!(receiver.recv().await, Some(AgentEvent::Started));
        assert!(receiver.try_recv().is_err(), "the full sink drops a delta");
    }
}
