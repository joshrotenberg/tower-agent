use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use tower::{Layer, Service};

use crate::{AgentError, AgentRequest, EffectState};

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
    S::Response: Send + 'static,
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
            return Box::pin(async { Err(AgentError::cancelled(EffectState::None)) });
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

fn preserve_settlement<T>(mut error: AgentError, settlement: Result<T, AgentError>) -> AgentError {
    if let Err(cause) = settlement {
        error = error.with_cause(cause);
    }
    error
}
