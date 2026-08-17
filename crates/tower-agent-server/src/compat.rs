//! Temporary bridge from the original [`Backend`] seam to the
//! Tower-native finite-turn contract.
//!
//! The old backend error is only a string and its provider futures do not yet
//! guarantee process cleanup on drop. The adapter therefore classifies backend
//! failures conservatively and does not claim cancellation safety. It exists to
//! compare composition paths while provider crates migrate to native services.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tower::Service;
use tower_agent::{
    AgentError, AgentEvent, AgentRequest, Cost, EffectState, ErrorKind, FailurePhase,
    SessionHandle, Turn, TurnOutcome,
};

use crate::{Backend, Event, Params};

/// Adapt an original tower-agent backend into a Tower service.
///
/// `Params` is retained as the temporary provider-options body. Common turn
/// fields overwrite the duplicate values in `Params` before execution.
#[derive(Clone)]
pub struct BackendService {
    backend: Arc<dyn Backend>,
}

impl BackendService {
    pub fn new(backend: Arc<dyn Backend>) -> Self {
        Self { backend }
    }
}

impl Service<AgentRequest<Turn<Params>>> for BackendService {
    type Response = TurnOutcome;
    type Error = AgentError;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: AgentRequest<Turn<Params>>) -> Self::Future {
        let backend = self.backend.clone();
        let provider = backend.name().to_string();
        let observer = request.context.events().clone();
        let turn = request.body;

        if let Some(session) = &turn.session
            && session.provider() != provider
        {
            let found = session.provider().to_string();
            return Box::pin(async move {
                Err(AgentError::new(
                    ErrorKind::Unsupported,
                    format!("cannot resume {found} session with {provider} service"),
                    FailurePhase::Validation,
                    EffectState::None,
                ))
            });
        }

        let mut params = turn.options;
        params.prompt = turn.prompt;
        params.cwd = turn
            .working_directory
            .map(|path| path.to_string_lossy().into_owned());
        params.session = turn.session.map(|session| session.value().to_string());

        Box::pin(async move {
            let _ = observer.try_emit(AgentEvent::Started);
            let (sender, mut events) = tokio::sync::mpsc::unbounded_channel();
            let run = backend.run_streaming(&params, sender);
            tokio::pin!(run);

            let result = loop {
                tokio::select! {
                    result = &mut run => break result,
                    Some(event) = events.recv() => {
                        let _ = observer.try_emit(map_event(event));
                    }
                }
            };
            while let Ok(event) = events.try_recv() {
                let _ = observer.try_emit(map_event(event));
            }

            let outcome = result.map_err(|error| {
                AgentError::new(
                    ErrorKind::Provider,
                    error.to_string(),
                    FailurePhase::Running,
                    EffectState::Possible,
                )
            })?;
            let mut adapted = TurnOutcome::new(outcome.reply);
            adapted.session = outcome
                .session
                .map(|value| SessionHandle::new(provider, value));
            adapted.cost = outcome.cost_usd.map(Cost::usd);
            Ok(adapted)
        })
    }
}

fn map_event(event: Event) -> AgentEvent {
    match event {
        Event::TextDelta(text) => AgentEvent::OutputDelta { text },
        Event::Thinking(text) => AgentEvent::ThinkingDelta { text },
        Event::ToolUse { name } => AgentEvent::ToolStarted { name },
        Event::Turn { n } => AgentEvent::TurnStarted { number: n },
        Event::Status(message) => AgentEvent::Status { message },
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use tower::ServiceExt;
    use tower_agent::{CallContext, EventObserver};

    use super::*;
    use crate::{BackendError, Outcome, Post};

    struct StreamingBackend;

    #[async_trait]
    impl Backend for StreamingBackend {
        fn name(&self) -> &str {
            "test"
        }

        async fn run(&self, _params: &Params) -> Result<Outcome, BackendError> {
            unreachable!("adapter uses streaming path")
        }

        async fn run_streaming(
            &self,
            params: &Params,
            events: tokio::sync::mpsc::UnboundedSender<Event>,
        ) -> Result<Outcome, BackendError> {
            let _ = events.send(Event::TextDelta("hel".into()));
            let _ = events.send(Event::TextDelta("lo".into()));
            Ok(Outcome {
                summary: "legacy summary is intentionally not projected".into(),
                reply: params.prompt.clone(),
                posts: vec![Post {
                    channel: "legacy-bus".into(),
                    body: "legacy post is intentionally not projected".into(),
                    to: None,
                    reply_to: None,
                }],
                session: Some("provider-session".into()),
                cost_usd: Some(0.25),
            })
        }
    }

    struct FailingBackend;

    #[async_trait]
    impl Backend for FailingBackend {
        fn name(&self) -> &str {
            "test"
        }

        async fn run(&self, _params: &Params) -> Result<Outcome, BackendError> {
            unreachable!("adapter uses streaming path")
        }

        async fn run_streaming(
            &self,
            _params: &Params,
            _events: tokio::sync::mpsc::UnboundedSender<Event>,
        ) -> Result<Outcome, BackendError> {
            Err(BackendError::new("legacy failure"))
        }
    }

    #[tokio::test]
    async fn adapts_owned_turn_events_outcome_and_session() {
        let (observer, mut events) = EventObserver::channel(3);
        let request = AgentRequest::with_context(
            Turn::new("hello").with_options(Params::default()),
            CallContext::new().with_events(observer),
        );
        let outcome = BackendService::new(Arc::new(StreamingBackend))
            .oneshot(request)
            .await
            .expect("backend succeeds");

        assert_eq!(outcome.output, "hello");
        assert_eq!(
            outcome.session.as_ref().map(SessionHandle::provider),
            Some("test")
        );
        assert_eq!(outcome.cost, Some(Cost::usd(0.25)));
        assert_eq!(outcome.output, "hello", "legacy summary is not promoted");
        assert_eq!(events.recv().await, Some(AgentEvent::Started));
        assert_eq!(
            events.recv().await,
            Some(AgentEvent::OutputDelta { text: "hel".into() })
        );
        assert_eq!(
            events.recv().await,
            Some(AgentEvent::OutputDelta { text: "lo".into() })
        );
    }

    #[tokio::test]
    async fn classifies_legacy_string_errors_conservatively() {
        let error = BackendService::new(Arc::new(FailingBackend))
            .oneshot(AgentRequest::new(
                Turn::new("hello").with_options(Params::default()),
            ))
            .await
            .expect_err("legacy backend fails");

        assert_eq!(error.kind, ErrorKind::Provider);
        assert_eq!(error.phase, FailurePhase::Running);
        assert_eq!(error.effects, EffectState::Possible);
        assert_eq!(error.message, "legacy failure");
    }
}
