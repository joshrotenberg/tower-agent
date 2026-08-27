use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use tower::{Layer, Service};

use crate::{AgentError, AgentRequest, EffectState, TerminalEvidence};

#[derive(Clone, Copy, Debug, Default)]
pub struct DeadlineLayer;

impl DeadlineLayer {
    pub const fn new() -> Self {
        Self
    }
}

impl<S> Layer<S> for DeadlineLayer {
    type Service = Deadline<S>;

    fn layer(&self, inner: S) -> Self::Service {
        Deadline { inner }
    }
}

#[derive(Clone, Debug)]
pub struct Deadline<S> {
    inner: S,
}

impl<S, T> Service<AgentRequest<T>> for Deadline<S>
where
    S: Service<AgentRequest<T>, Error = AgentError>,
    S::Future: Send + 'static,
    S::Response: Send + TerminalEvidence + 'static,
    T: 'static,
{
    type Response = S::Response;
    type Error = AgentError;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: AgentRequest<T>) -> Self::Future {
        let deadline = request.context.deadline();
        let cancellation = request.context.cancellation().clone();

        if cancellation.is_cancelled() {
            // Nothing is launched on this path, so the phase matches the
            // elapsed-deadline rejection below rather than claiming the
            // operation reached the provider.
            return Box::pin(async {
                Err(AgentError::new(
                    crate::ErrorKind::Cancelled,
                    "agent operation was cancelled before execution",
                    crate::FailurePhase::Admission,
                    EffectState::None,
                ))
            });
        }

        if deadline.is_some_and(|deadline| deadline <= std::time::Instant::now()) {
            cancellation.cancel();
            return Box::pin(async {
                Err(AgentError::new(
                    crate::ErrorKind::DeadlineExceeded,
                    "agent operation deadline elapsed before execution",
                    crate::FailurePhase::Admission,
                    EffectState::None,
                ))
            });
        }

        let future = self.inner.call(request);
        Box::pin(async move {
            let future = future;
            tokio::pin!(future);

            match deadline {
                Some(deadline) => tokio::select! {
                    result = &mut future => result,
                    () = tokio::time::sleep_until(deadline.into()) => {
                        cancellation.cancel();
                        // Cancellation is a cleanup request, not permission to
                        // drop the inner future. Wait for settlement first.
                        let settlement = future.await;
                        Err(preserve_settlement(
                            AgentError::deadline_exceeded(EffectState::Possible),
                            settlement,
                        ))
                    }
                    () = cancellation.cancelled() => {
                        let settlement = future.await;
                        Err(preserve_settlement(
                            AgentError::cancelled(EffectState::Possible),
                            settlement,
                        ))
                    }
                },
                None => tokio::select! {
                    result = &mut future => result,
                    () = cancellation.cancelled() => {
                        let settlement = future.await;
                        Err(preserve_settlement(
                            AgentError::cancelled(EffectState::Possible),
                            settlement,
                        ))
                    }
                },
            }
        })
    }
}

/// Merge whatever the inner call established into the outer failure.
///
/// A failed settlement contributes its cause, effect state, and partial
/// evidence. A successful settlement is stronger still: it proves the turn ran
/// to completion, so its effects are reported rather than merely possible and
/// its terminal facts are the accounting a host needs to reconcile a
/// reservation and offer continuation. Discarding them would report a spend of
/// nothing for a turn that actually spent.
fn preserve_settlement<T>(mut error: AgentError, settlement: Result<T, AgentError>) -> AgentError
where
    T: TerminalEvidence,
{
    match settlement {
        Err(cause) => error.with_cause(cause),
        Ok(response) => {
            error.effects = error.effects.combine(EffectState::Reported);
            let settled = response.terminal_evidence();
            match error.evidence.as_deref_mut() {
                Some(evidence) => evidence.merge_missing(&settled),
                None => error.evidence = Some(Box::new(settled)),
            }
            error
        }
    }
}
