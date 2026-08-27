use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use tower::{Layer, Service};

use crate::{AgentError, EffectState, ErrorKind, FailurePhase, TerminalEvidence};

/// One injected fault, applied to one call.
///
/// Faults are described relative to launch, because that is the line every
/// invariant in this crate is drawn against: what may be claimed about
/// effects, which phase a failure belongs to, and whether a retry is safe all
/// depend on whether the provider was reached.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Fault {
    /// Stall before the inner service is called.
    ///
    /// The delay is not cancellation-aware on purpose: a caller that cancels
    /// during it should observe whatever the surrounding stack does with a
    /// service that has not yet handed the request on.
    DelayBeforeLaunch(Duration),
    /// Refuse before the inner service is called.
    ///
    /// Nothing runs, so this is the one injected failure that may honestly
    /// carry `EffectState::None`.
    /// The failure to raise, classified as a pre-launch refusal.
    RefuseBeforeLaunch {
        /// Kind to report. The phase and effect state are fixed.
        kind: ErrorKind,
        /// Message to report.
        message: String,
    },
    /// Let the call run, then hold its settled result before returning it.
    ///
    /// Terminal evidence arriving late is the shape that breaks deadline and
    /// supervision code, and it is invisible to a test whose provider
    /// answers instantly.
    DelaySettlement(Duration),
    /// Let the call run, then replace its result with a failure.
    ///
    /// The settlement becomes the cause, so its evidence and effect state
    /// survive. The injected failure never claims to know less about effects
    /// than the call it wrapped.
    /// The failure to raise, classified as a post-settlement failure.
    FailAfterSettlement {
        /// Kind to report. The phase and effect state are fixed.
        kind: ErrorKind,
        /// Message to report.
        message: String,
    },
}

/// Deterministic fault injection for testing cancellation and settlement.
///
/// Faults come from an explicit queue consumed in order, one per call, with
/// no randomness and no seed: a failing test names the exact fault that
/// produced it. An empty queue passes every call through unchanged, so the
/// layer is inert unless a test asks for something.
///
/// This is test machinery living in the public API on purpose. Adapter crates
/// need it from their own test suites, which cannot reach a `#[cfg(test)]`
/// item in another crate, and it exists precisely so those suites can cover
/// stalled and late-settling providers without live credentials or real
/// subprocesses.
#[derive(Clone, Debug, Default)]
pub struct FaultLayer {
    script: Arc<Mutex<VecDeque<Fault>>>,
}

impl FaultLayer {
    /// A layer that injects nothing.
    pub fn none() -> Self {
        Self::default()
    }

    /// A layer that applies these faults to successive calls, in order.
    pub fn scripted(faults: impl IntoIterator<Item = Fault>) -> Self {
        Self {
            script: Arc::new(Mutex::new(faults.into_iter().collect())),
        }
    }

    /// Faults not yet consumed. A test asserting this is empty has proved
    /// every fault it scripted was actually reached.
    pub fn remaining(&self) -> usize {
        self.script.lock().expect("fault script lock").len()
    }
}

impl<S> Layer<S> for FaultLayer {
    type Service = InjectFaults<S>;

    fn layer(&self, inner: S) -> Self::Service {
        InjectFaults {
            inner,
            script: Arc::clone(&self.script),
        }
    }
}

#[derive(Clone, Debug)]
/// The [`FaultLayer`] service. See that type for behavior.
pub struct InjectFaults<S> {
    inner: S,
    script: Arc<Mutex<VecDeque<Fault>>>,
}

impl<S, Request> Service<Request> for InjectFaults<S>
where
    S: Service<Request, Error = AgentError>,
    S::Future: Send + 'static,
    S::Response: Send + TerminalEvidence + 'static,
    Request: 'static,
{
    type Response = S::Response;
    type Error = AgentError;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: Request) -> Self::Future {
        let fault = self.script.lock().expect("fault script lock").pop_front();

        // Decided before the call so a pre-launch fault never reaches the
        // inner service: a refusal that claims nothing ran has to be true.
        match fault {
            Some(Fault::RefuseBeforeLaunch { kind, message }) => Box::pin(async move {
                Err(AgentError::new(
                    kind,
                    message,
                    FailurePhase::Admission,
                    EffectState::None,
                ))
            }),
            Some(Fault::DelayBeforeLaunch(delay)) => {
                let future = self.inner.call(request);
                Box::pin(async move {
                    tokio::time::sleep(delay).await;
                    future.await
                })
            }
            Some(Fault::DelaySettlement(delay)) => {
                let future = self.inner.call(request);
                Box::pin(async move {
                    let settled = future.await;
                    tokio::time::sleep(delay).await;
                    settled
                })
            }
            Some(Fault::FailAfterSettlement { kind, message }) => {
                let future = self.inner.call(request);
                Box::pin(async move {
                    let settled = future.await;
                    Err(fail_after(kind, message, settled))
                })
            }
            None => Box::pin(self.inner.call(request)),
        }
    }
}

/// Build a post-settlement failure without discarding what settled.
///
/// The call reached the provider, so the injected failure must not claim less
/// about effects than the settlement did. A successful settlement proves the
/// turn ran, and a failed one already carries its own reading; either way the
/// answer comes from the call rather than from the fault.
fn fail_after<T>(kind: ErrorKind, message: String, settled: Result<T, AgentError>) -> AgentError
where
    T: TerminalEvidence,
{
    match settled {
        Ok(response) => AgentError::new(
            kind,
            message,
            FailurePhase::Settlement,
            EffectState::Reported,
        )
        .with_evidence(response.terminal_evidence()),
        Err(cause) => AgentError::new(kind, message, FailurePhase::Settlement, cause.effects)
            .with_cause(cause),
    }
}

#[cfg(test)]
mod tests {
    use tower::{ServiceBuilder, ServiceExt, service_fn};

    use super::*;
    use crate::layer::DeadlineLayer;
    use crate::{AgentRequest, CallContext, CancellationToken, Cost, Turn, TurnOutcome};

    fn settled() -> TurnOutcome {
        TurnOutcome {
            cost: Some(Cost::usd(0.11)),
            provider_turns: Some(2),
            ..TurnOutcome::new("done")
        }
    }

    fn provider() -> impl Service<
        AgentRequest<Turn>,
        Response = TurnOutcome,
        Error = AgentError,
        Future: Send + 'static,
    > + Clone {
        service_fn(|_: AgentRequest<Turn>| async { Ok::<_, AgentError>(settled()) })
    }

    #[tokio::test]
    async fn an_empty_script_changes_nothing() {
        let layer = FaultLayer::none();
        let outcome = ServiceBuilder::new()
            .layer(layer.clone())
            .service(provider())
            .oneshot(AgentRequest::new(Turn::new("hello")))
            .await
            .expect("no fault was scripted");
        assert_eq!(outcome.output, "done");
        assert_eq!(layer.remaining(), 0);
    }

    #[tokio::test]
    async fn faults_apply_in_order_one_per_call() {
        let layer = FaultLayer::scripted([
            Fault::RefuseBeforeLaunch {
                kind: ErrorKind::Busy,
                message: "first".to_string(),
            },
            Fault::FailAfterSettlement {
                kind: ErrorKind::Provider,
                message: "second".to_string(),
            },
        ]);
        let service = ServiceBuilder::new()
            .layer(layer.clone())
            .service(provider());

        assert_eq!(layer.remaining(), 2);
        let first = service
            .clone()
            .oneshot(AgentRequest::new(Turn::new("a")))
            .await
            .expect_err("first fault");
        assert_eq!(first.message, "first");

        let second = service
            .clone()
            .oneshot(AgentRequest::new(Turn::new("b")))
            .await
            .expect_err("second fault");
        assert_eq!(second.message, "second");

        // The script is spent, so the third call runs clean.
        assert_eq!(layer.remaining(), 0);
        service
            .oneshot(AgentRequest::new(Turn::new("c")))
            .await
            .expect("script exhausted");
    }

    #[tokio::test]
    async fn a_pre_launch_refusal_never_reaches_the_provider() {
        let calls = Arc::new(Mutex::new(0usize));
        let seen = Arc::clone(&calls);
        let inner = service_fn(move |_: AgentRequest<Turn>| {
            *seen.lock().unwrap() += 1;
            async { Ok::<_, AgentError>(settled()) }
        });
        let error = ServiceBuilder::new()
            .layer(FaultLayer::scripted([Fault::RefuseBeforeLaunch {
                kind: ErrorKind::Unavailable,
                message: "circuit open".to_string(),
            }]))
            .service(inner)
            .oneshot(AgentRequest::new(Turn::new("hello")))
            .await
            .expect_err("refused");

        assert_eq!(*calls.lock().unwrap(), 0, "the provider was reached anyway");
        assert_eq!(error.effects, EffectState::None);
        assert_eq!(error.phase, FailurePhase::Admission);
    }

    /// The invariant this layer exists to respect: once the call has run, an
    /// injected failure may not claim nothing happened.
    #[tokio::test]
    async fn a_post_settlement_failure_never_claims_no_effects() {
        let error = ServiceBuilder::new()
            .layer(FaultLayer::scripted([Fault::FailAfterSettlement {
                kind: ErrorKind::Internal,
                message: "injected".to_string(),
            }]))
            .service(provider())
            .oneshot(AgentRequest::new(Turn::new("hello")))
            .await
            .expect_err("injected after settlement");

        assert_ne!(error.effects, EffectState::None);
        assert_eq!(error.effects, EffectState::Reported);
        // The settlement's accounting survives the injected failure.
        let evidence = error.evidence.as_deref().expect("evidence");
        assert_eq!(evidence.cost, Some(Cost::usd(0.11)));
        assert_eq!(evidence.provider_turns, Some(2));
    }

    #[tokio::test]
    async fn a_post_settlement_failure_keeps_a_failed_call_as_its_cause() {
        let inner = service_fn(|_: AgentRequest<Turn>| async {
            Err::<TurnOutcome, _>(AgentError::new(
                ErrorKind::Provider,
                "provider failed",
                FailurePhase::Running,
                EffectState::Possible,
            ))
        });
        let error = ServiceBuilder::new()
            .layer(FaultLayer::scripted([Fault::FailAfterSettlement {
                kind: ErrorKind::Internal,
                message: "injected".to_string(),
            }]))
            .service(inner)
            .oneshot(AgentRequest::new(Turn::new("hello")))
            .await
            .expect_err("injected after a failed call");

        assert_eq!(error.effects, EffectState::Possible);
        assert_eq!(
            error.cause.as_ref().map(|cause| cause.kind),
            Some(ErrorKind::Provider)
        );
    }

    /// Late terminal evidence is the case that breaks deadline handling, and
    /// it cannot be reproduced with a provider that answers instantly.
    #[tokio::test(start_paused = true)]
    async fn a_deadline_drains_a_late_settling_call_and_keeps_its_evidence() {
        let service = ServiceBuilder::new()
            .layer(DeadlineLayer::new())
            .layer(FaultLayer::scripted([Fault::DelaySettlement(
                Duration::from_secs(30),
            )]))
            .service(provider());

        let request = AgentRequest::with_context(
            Turn::new("late"),
            CallContext::new().with_deadline(std::time::Instant::now() + Duration::from_millis(50)),
        );
        let error = service
            .oneshot(request)
            .await
            .expect_err("the deadline elapsed first");

        assert_eq!(error.kind, ErrorKind::DeadlineExceeded);
        // The call still settled, so its accounting reaches the caller.
        assert_eq!(error.effects, EffectState::Reported);
        assert_eq!(
            error.evidence.as_deref().and_then(|e| e.cost.clone()),
            Some(Cost::usd(0.11))
        );
    }

    /// A stall before launch, cancelled while it stalls.
    #[tokio::test(start_paused = true)]
    async fn cancellation_during_a_pre_launch_stall_settles() {
        let cancellation = CancellationToken::new();
        let service = ServiceBuilder::new()
            .layer(DeadlineLayer::new())
            .layer(FaultLayer::scripted([Fault::DelayBeforeLaunch(
                Duration::from_secs(30),
            )]))
            .service(provider());

        let request = AgentRequest::with_context(
            Turn::new("stalled"),
            CallContext::new().with_cancellation(cancellation.clone()),
        );
        let call = tokio::spawn(service.oneshot(request));
        tokio::task::yield_now().await;
        cancellation.cancel();

        let error = call
            .await
            .expect("call task")
            .expect_err("cancelled while stalled");
        assert_eq!(error.kind, ErrorKind::Cancelled);
    }
}
