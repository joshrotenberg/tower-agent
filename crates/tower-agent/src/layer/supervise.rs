use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use tower::{Layer, Service};

use crate::{AgentError, AgentRequest, EffectState, ErrorKind, FailurePhase};

/// Keeps an inner call owned and polled after its caller goes away.
///
/// Dropping the returned future signals cancellation but detaches the spawned
/// supervisor task instead of aborting it. The task retains the inner future,
/// including admission permits, until the provider settles.
#[derive(Clone, Copy, Debug, Default)]
pub struct SuperviseLayer;

impl SuperviseLayer {
    /// Retain dropped calls until they settle.
    pub const fn new() -> Self {
        Self
    }
}

impl<S> Layer<S> for SuperviseLayer {
    type Service = Supervise<S>;

    fn layer(&self, inner: S) -> Self::Service {
        Supervise { inner }
    }
}

#[derive(Clone, Debug)]
/// The [`SuperviseLayer`] service. See that type for behavior.
pub struct Supervise<S> {
    inner: S,
}

impl<S, T> Service<AgentRequest<T>> for Supervise<S>
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
        let cancellation = request.context.cancellation().clone();
        let future = self.inner.call(request);
        let task = tokio::spawn(future);
        let mut guard = CancelOnDrop::new(cancellation);

        Box::pin(async move {
            let result = match task.await {
                Ok(result) => result,
                Err(error) => Err(AgentError::new(
                    ErrorKind::Internal,
                    format!("agent supervisor task failed: {error}"),
                    FailurePhase::Settlement,
                    EffectState::Possible,
                )),
            };
            guard.disarm();
            result
        })
    }
}

struct CancelOnDrop {
    cancellation: tokio_util::sync::CancellationToken,
    armed: bool,
}

impl CancelOnDrop {
    fn new(cancellation: tokio_util::sync::CancellationToken) -> Self {
        Self {
            cancellation,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.cancellation.cancel();
        }
    }
}
