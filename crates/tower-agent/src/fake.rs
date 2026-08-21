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
    /// Exact event and delay sequence to run after `delay` and before the
    /// terminal result. When present, even if empty, this suppresses the
    /// automatic `Started` and `OutputDelta` events.
    pub script: Option<Vec<FakeStep>>,
    /// Exact terminal result. When present, this takes precedence over
    /// `fail`, `output`, and the simulated accounting fields.
    pub terminal: Option<FakeTerminal>,
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

impl FakeOptions {
    /// Return an exact successful provider outcome.
    pub fn succeed(outcome: TurnOutcome) -> Self {
        Self {
            terminal: Some(FakeTerminal::Success(outcome)),
            ..Self::default()
        }
    }

    /// Return an exact typed provider failure.
    pub fn fail_with(error: AgentError) -> Self {
        Self {
            terminal: Some(FakeTerminal::Failure(error)),
            ..Self::default()
        }
    }

    /// Replace automatic events with an exact event/delay sequence.
    pub fn with_script(mut self, script: impl IntoIterator<Item = FakeStep>) -> Self {
        self.script = Some(script.into_iter().collect());
        self
    }
}

/// Exact terminal behavior for one fake turn.
#[derive(Clone, Debug, PartialEq)]
pub enum FakeTerminal {
    Success(TurnOutcome),
    Failure(AgentError),
}

/// One deterministic observation or cancellation-aware pause before a fake
/// turn settles.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FakeStep {
    Emit(AgentEvent),
    Delay(Duration),
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

impl FakeService {
    /// Create an independently named fake provider for routing and
    /// provider-pinned session tests.
    pub fn named(provider: impl Into<String>) -> NamedFakeService {
        NamedFakeService::new(provider)
    }
}

/// A fake service with a caller-selected provider identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NamedFakeService {
    provider: String,
}

impl NamedFakeService {
    pub fn new(provider: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
        }
    }

    pub fn provider(&self) -> &str {
        &self.provider
    }
}

impl Service<AgentRequest<Turn<FakeOptions>>> for FakeService {
    type Response = TurnOutcome;
    type Error = AgentError;
    type Future = Pin<Box<dyn Future<Output = Result<TurnOutcome, AgentError>> + Send + 'static>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: AgentRequest<Turn<FakeOptions>>) -> Self::Future {
        call_fake(request, FAKE_PROVIDER.to_owned())
    }
}

impl Service<AgentRequest<Turn<FakeOptions>>> for NamedFakeService {
    type Response = TurnOutcome;
    type Error = AgentError;
    type Future = Pin<Box<dyn Future<Output = Result<TurnOutcome, AgentError>> + Send + 'static>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: AgentRequest<Turn<FakeOptions>>) -> Self::Future {
        call_fake(request, self.provider.clone())
    }
}

fn call_fake(
    request: AgentRequest<Turn<FakeOptions>>,
    provider: String,
) -> Pin<Box<dyn Future<Output = Result<TurnOutcome, AgentError>> + Send + 'static>> {
    Box::pin(async move {
        if request.context.cancellation().is_cancelled() {
            return Err(AgentError::cancelled(EffectState::None));
        }
        if provider.trim().is_empty() {
            return Err(AgentError::invalid_request(
                "fake provider name must not be empty",
            ));
        }
        if request.body.prompt.trim().is_empty() {
            return Err(AgentError::invalid_request("prompt must not be empty"));
        }

        let resumed_session = request.body.session.as_ref();
        let preassigned_session = request.context.preassigned_session();
        if resumed_session.is_some() && preassigned_session.is_some() {
            return Err(AgentError::invalid_request(
                "a resumed fake turn cannot also preassign a session",
            ));
        }
        for session in resumed_session.into_iter().chain(preassigned_session) {
            if session.provider() != provider {
                return Err(AgentError::new(
                    ErrorKind::Unsupported,
                    format!(
                        "cannot use {} session with the {provider} fake service",
                        session.provider()
                    ),
                    FailurePhase::Validation,
                    EffectState::None,
                ));
            }
        }

        let started = Instant::now();
        let scripted = request.body.options.script.is_some();
        if !scripted {
            let _ = request.context.events().try_emit(AgentEvent::Started);
        }

        if let Some(delay) = request.body.options.delay {
            wait_or_cancel(delay, &request).await?;
        }
        if let Some(script) = &request.body.options.script {
            for step in script {
                match step {
                    FakeStep::Emit(event) => {
                        let _ = request.context.events().try_emit(event.clone());
                    }
                    FakeStep::Delay(delay) => wait_or_cancel(*delay, &request).await?,
                }
            }
        }

        if let Some(terminal) = &request.body.options.terminal {
            return match terminal {
                FakeTerminal::Success(outcome) => {
                    if !scripted {
                        let _ = request.context.events().try_emit(AgentEvent::OutputDelta {
                            text: outcome.output.clone(),
                        });
                    }
                    Ok(outcome.clone())
                }
                FakeTerminal::Failure(error) => Err(error.clone()),
            };
        }
        if let Some(message) = &request.body.options.fail {
            return Err(AgentError::new(
                ErrorKind::Provider,
                message.clone(),
                FailurePhase::Running,
                EffectState::Possible,
            ));
        }

        let output = request
            .body
            .options
            .output
            .clone()
            .unwrap_or_else(|| request.body.prompt.clone());
        if !scripted {
            let _ = request.context.events().try_emit(AgentEvent::OutputDelta {
                text: output.clone(),
            });
        }
        let mut outcome = TurnOutcome::new(output);
        outcome.session = Some(
            request
                .body
                .session
                .clone()
                .or_else(|| request.context.preassigned_session().cloned())
                .unwrap_or_else(|| mint_fake_session(&provider)),
        );
        outcome.usage = request
            .body
            .options
            .simulated_tokens
            .map(|total| TokenUsage {
                output: Some(total),
                ..TokenUsage::default()
            });
        outcome.cost = request
            .body
            .options
            .simulated_cost_usd
            .map(crate::Cost::usd);
        outcome.duration = Some(started.elapsed());
        outcome.provider_turns = Some(1);
        Ok(outcome)
    })
}

async fn wait_or_cancel(
    delay: Duration,
    request: &AgentRequest<Turn<FakeOptions>>,
) -> Result<(), AgentError> {
    tokio::select! {
        _ = tokio::time::sleep(delay) => Ok(()),
        _ = request.context.cancellation().cancelled() => {
            Err(AgentError::cancelled(EffectState::Possible))
        }
    }
}

fn mint_fake_session(provider: &str) -> SessionHandle {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    SessionHandle::new(
        provider,
        format!("fake-{}", COUNTER.fetch_add(1, Ordering::Relaxed)),
    )
}

#[cfg(test)]
mod tests {
    use tower::ServiceExt;

    use super::*;
    use crate::{CallContext, CancellationToken, Cost, ErrorKind, FailureEvidence, TokenUsage};

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

    #[tokio::test]
    async fn fake_returns_an_exact_outcome_and_scripted_events() {
        let usage = TokenUsage {
            input: Some(10),
            cached_input: Some(3),
            output: Some(7),
            reasoning_output: Some(2),
            provider_total: Some(22),
            ..TokenUsage::default()
        };
        let expected = TurnOutcome {
            output: "structured answer".into(),
            session: Some(SessionHandle::new("fake", "exact-session")),
            usage: Some(usage),
            cost: Some(Cost::usd(0.42)),
            duration: Some(Duration::from_secs(7)),
            provider_turns: Some(3),
        };
        let script = [
            FakeStep::Emit(AgentEvent::Started),
            FakeStep::Emit(AgentEvent::ThinkingDelta {
                text: "considering".into(),
            }),
            FakeStep::Delay(Duration::from_millis(1)),
            FakeStep::Emit(AgentEvent::ToolStarted {
                name: "search".into(),
            }),
            FakeStep::Emit(AgentEvent::Usage { usage }),
        ];
        let (events, mut receiver) = crate::EventObserver::channel(8);
        let request = AgentRequest::with_context(
            Turn::new("ignored")
                .with_options(FakeOptions::succeed(expected.clone()).with_script(script.clone())),
            CallContext::new().with_events(events),
        );

        let actual = FakeService.oneshot(request).await.expect("exact success");
        assert_eq!(actual, expected);
        for step in script {
            if let FakeStep::Emit(expected_event) = step {
                assert_eq!(receiver.recv().await, Some(expected_event));
            }
        }
        assert!(
            receiver.try_recv().is_err(),
            "a custom script suppresses automatic events"
        );
    }

    #[tokio::test]
    async fn fake_returns_an_exact_typed_failure_with_evidence() {
        let evidence = FailureEvidence {
            session: Some(SessionHandle::new("fake", "reserved-session")),
            usage: Some(TokenUsage {
                input: Some(11),
                provider_total: Some(11),
                ..TokenUsage::default()
            }),
            cost: Some(Cost::usd(0.03)),
            duration: Some(Duration::from_secs(2)),
            provider_turns: Some(1),
        };
        let expected = AgentError::new(
            ErrorKind::Authentication,
            "credential rejected",
            FailurePhase::Launch,
            EffectState::None,
        )
        .with_evidence(evidence);

        let actual = FakeService
            .oneshot(AgentRequest::new(
                Turn::new("x").with_options(FakeOptions::fail_with(expected.clone())),
            ))
            .await
            .expect_err("exact failure");
        assert_eq!(actual, expected);
    }

    #[tokio::test]
    async fn named_fakes_mint_resume_and_honor_their_own_sessions() {
        let service = FakeService::named("fake-alpha");
        assert_eq!(service.provider(), "fake-alpha");

        let fresh = service
            .clone()
            .oneshot(AgentRequest::new(
                Turn::new("fresh").with_options(FakeOptions::default()),
            ))
            .await
            .expect("fresh named fake succeeds");
        let minted = fresh.session.expect("named session minted");
        assert_eq!(minted.provider(), "fake-alpha");

        let resumed = service
            .clone()
            .oneshot(AgentRequest::new(
                Turn::new("resume")
                    .resume(minted.clone())
                    .with_options(FakeOptions::default()),
            ))
            .await
            .expect("own session resumes");
        assert_eq!(resumed.session, Some(minted));

        let reserved = SessionHandle::new("fake-alpha", "host-reserved");
        let preassigned = service
            .clone()
            .oneshot(AgentRequest::with_context(
                Turn::new("fresh reserved").with_options(FakeOptions::default()),
                CallContext::new().with_preassigned_session(reserved.clone()),
            ))
            .await
            .expect("compatible preassignment succeeds");
        assert_eq!(preassigned.session, Some(reserved));

        let foreign = service
            .clone()
            .oneshot(AgentRequest::new(
                Turn::new("wrong provider")
                    .resume(SessionHandle::new("fake-beta", "session"))
                    .with_options(FakeOptions::default()),
            ))
            .await
            .expect_err("foreign session is rejected");
        assert_eq!(foreign.kind, ErrorKind::Unsupported);
        assert_eq!(foreign.phase, FailurePhase::Validation);
        assert_eq!(foreign.effects, EffectState::None);

        let conflict = service
            .oneshot(AgentRequest::with_context(
                Turn::new("conflict")
                    .resume(SessionHandle::new("fake-alpha", "resume"))
                    .with_options(FakeOptions::default()),
                CallContext::new()
                    .with_preassigned_session(SessionHandle::new("fake-alpha", "reserved")),
            ))
            .await
            .expect_err("resume and preassignment conflict");
        assert_eq!(conflict.kind, ErrorKind::InvalidRequest);
        assert_eq!(conflict.effects, EffectState::None);
    }

    #[tokio::test]
    async fn scripted_delays_are_cancellation_aware() {
        let cancellation = CancellationToken::new();
        let (events, mut receiver) = crate::EventObserver::channel(2);
        let options = FakeOptions::succeed(TurnOutcome::new("too late")).with_script([
            FakeStep::Emit(AgentEvent::Started),
            FakeStep::Delay(Duration::from_secs(30)),
            FakeStep::Emit(AgentEvent::OutputDelta {
                text: "too late".into(),
            }),
        ]);
        let request = AgentRequest::with_context(
            Turn::new("slow").with_options(options),
            CallContext::new()
                .with_cancellation(cancellation.clone())
                .with_events(events),
        );
        let call = tokio::spawn(FakeService.oneshot(request));

        assert_eq!(receiver.recv().await, Some(AgentEvent::Started));
        cancellation.cancel();
        let error = tokio::time::timeout(Duration::from_secs(2), call)
            .await
            .expect("settles promptly")
            .expect("no panic")
            .expect_err("cancelled");
        assert_eq!(error.kind, ErrorKind::Cancelled);
        assert_eq!(error.effects, EffectState::Possible);
        assert!(
            receiver.try_recv().is_err(),
            "events after the cancelled delay are not emitted"
        );
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
