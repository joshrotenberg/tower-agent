use std::future::{Future, Ready, ready};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use tower::Service;

use crate::{
    AgentError, AgentEvent, AgentRequest, EffectState, ErrorKind, FailurePhase, SessionHandle,
    TokenUsage, Turn, TurnOutcome,
};

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

/// Controls for one [`FakeService`] turn: latency, scripted failure, canned
/// output, session continuation, and simulated accounting.
///
/// The fake exists so hosts can exercise queueing, concurrency, deadlines,
/// cancellation, and session threading without a provider process. Absent
/// options behave like [`EchoService`] with a session.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FakeOptions {
    /// Sleep this long before settling; the sleep is cancellation-aware, so
    /// an interrupted fake turn reports a cancelled failure like a real
    /// provider would.
    pub delay: Option<Duration>,
    /// Fail after the delay with this message, as a provider failure with
    /// possible effects.
    pub fail: Option<String>,
    /// Terminal output; defaults to echoing the prompt.
    pub output: Option<String>,
    /// Simulated provider token total, reported as output tokens.
    pub simulated_tokens: Option<u64>,
    /// Simulated spend, reported as USD cost.
    pub simulated_cost_usd: Option<f64>,
}

/// Session-capable, latency-capable fake provider.
///
/// Continues any session tagged with its own provider name and mints
/// `fake-<counter>` handles for fresh turns, so session-threading logic can
/// be tested end to end. Rejects foreign-provider sessions exactly like the
/// real adapters.
#[derive(Clone, Debug, Default)]
pub struct FakeService;

const FAKE_PROVIDER: &str = "fake";

impl Service<AgentRequest<Turn<FakeOptions>>> for FakeService {
    type Response = TurnOutcome;
    type Error = AgentError;
    type Future = Pin<Box<dyn Future<Output = Result<TurnOutcome, AgentError>> + Send + 'static>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: AgentRequest<Turn<FakeOptions>>) -> Self::Future {
        Box::pin(async move {
            if request.context.cancellation().is_cancelled() {
                return Err(AgentError::cancelled(EffectState::None));
            }
            if request.body.prompt.trim().is_empty() {
                return Err(AgentError::invalid_request("prompt must not be empty"));
            }
            if let Some(session) = &request.body.session
                && session.provider() != FAKE_PROVIDER
            {
                return Err(AgentError::new(
                    ErrorKind::Unsupported,
                    format!(
                        "cannot resume {} session with the fake service",
                        session.provider()
                    ),
                    FailurePhase::Validation,
                    EffectState::None,
                ));
            }

            let started = Instant::now();
            let _ = request.context.events().try_emit(AgentEvent::Started);
            let options = &request.body.options;
            if let Some(delay) = options.delay {
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = request.context.cancellation().cancelled() => {
                        return Err(AgentError::cancelled(EffectState::Possible));
                    }
                }
            }
            if let Some(message) = &options.fail {
                return Err(AgentError::new(
                    ErrorKind::Provider,
                    message.clone(),
                    FailurePhase::Running,
                    EffectState::Possible,
                ));
            }

            let output = options
                .output
                .clone()
                .unwrap_or_else(|| request.body.prompt.clone());
            let _ = request.context.events().try_emit(AgentEvent::OutputDelta {
                text: output.clone(),
            });
            let mut outcome = TurnOutcome::new(output);
            outcome.session = Some(match &request.body.session {
                Some(session) => session.clone(),
                None => {
                    use std::sync::atomic::{AtomicU64, Ordering};
                    static COUNTER: AtomicU64 = AtomicU64::new(1);
                    SessionHandle::new(
                        FAKE_PROVIDER,
                        format!("fake-{}", COUNTER.fetch_add(1, Ordering::Relaxed)),
                    )
                }
            });
            outcome.usage = options.simulated_tokens.map(|total| TokenUsage {
                output: Some(total),
                ..TokenUsage::default()
            });
            outcome.cost = options.simulated_cost_usd.map(crate::Cost::usd);
            outcome.duration = Some(started.elapsed());
            outcome.provider_turns = Some(1);
            Ok(outcome)
        })
    }
}

#[cfg(test)]
mod tests {
    use tower::ServiceExt;

    use super::*;
    use crate::{CallContext, CancellationToken, ErrorKind, EventObserver};

    #[tokio::test]
    async fn fake_echoes_with_a_minted_session_and_duration() {
        let outcome = FakeService
            .oneshot(AgentRequest::new(
                Turn::new("hello").with_options(FakeOptions::default()),
            ))
            .await
            .expect("fake succeeds");
        assert_eq!(outcome.output, "hello");
        let session = outcome.session.expect("session minted");
        assert_eq!(session.provider(), "fake");
        assert!(outcome.duration.is_some());
    }

    #[tokio::test]
    async fn fake_continues_its_own_session_and_rejects_foreign() {
        let own = SessionHandle::new("fake", "fake-7");
        let outcome = FakeService
            .oneshot(AgentRequest::new(
                Turn::new("again")
                    .resume(own.clone())
                    .with_options(FakeOptions::default()),
            ))
            .await
            .expect("continuation succeeds");
        assert_eq!(
            outcome.session.as_ref().map(SessionHandle::value),
            Some("fake-7")
        );

        let error = FakeService
            .oneshot(AgentRequest::new(
                Turn::new("again")
                    .resume(SessionHandle::new("claude", "x"))
                    .with_options(FakeOptions::default()),
            ))
            .await
            .expect_err("foreign session refused");
        assert_eq!(error.kind, ErrorKind::Unsupported);
    }

    #[tokio::test]
    async fn fake_fails_on_script_and_cancels_mid_delay() {
        let error = FakeService
            .oneshot(AgentRequest::new(Turn::new("x").with_options(
                FakeOptions {
                    fail: Some("scripted failure".into()),
                    ..FakeOptions::default()
                },
            )))
            .await
            .expect_err("scripted failure");
        assert_eq!(error.kind, ErrorKind::Provider);
        assert_eq!(error.effects, EffectState::Possible);

        let cancellation = CancellationToken::new();
        let request = AgentRequest::with_context(
            Turn::new("slow").with_options(FakeOptions {
                delay: Some(Duration::from_secs(30)),
                ..FakeOptions::default()
            }),
            CallContext::new().with_cancellation(cancellation.clone()),
        );
        let call = tokio::spawn(FakeService.oneshot(request));
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancellation.cancel();
        let error = tokio::time::timeout(Duration::from_secs(2), call)
            .await
            .expect("settles promptly")
            .expect("no panic")
            .expect_err("cancelled");
        assert_eq!(error.kind, ErrorKind::Cancelled);
    }

    #[tokio::test]
    async fn fake_reports_simulated_accounting() {
        let outcome = FakeService
            .oneshot(AgentRequest::new(Turn::new("x").with_options(
                FakeOptions {
                    output: Some("canned".into()),
                    simulated_tokens: Some(123),
                    simulated_cost_usd: Some(0.05),
                    ..FakeOptions::default()
                },
            )))
            .await
            .expect("fake succeeds");
        assert_eq!(outcome.output, "canned");
        assert_eq!(outcome.usage.and_then(TokenUsage::total), Some(123));
        assert_eq!(outcome.cost, Some(crate::Cost::usd(0.05)));
    }
}

#[cfg(test)]
mod echo_tests {
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
