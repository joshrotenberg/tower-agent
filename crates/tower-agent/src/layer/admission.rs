use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::sync::Semaphore;
use tower::limit::ConcurrencyLimit;
use tower::{Layer, Service};

use crate::AgentError;

/// Shared concurrency admission followed by immediate typed load shedding.
#[derive(Clone, Debug)]
pub struct AdmissionLayer {
    semaphore: Arc<Semaphore>,
}

impl AdmissionLayer {
    pub fn new(max_concurrency: usize) -> Self {
        assert!(max_concurrency > 0, "admission limit must be nonzero");
        Self {
            semaphore: Arc::new(Semaphore::new(max_concurrency)),
        }
    }

    pub fn single_flight() -> Self {
        Self::new(1)
    }
}

impl<S> Layer<S> for AdmissionLayer {
    type Service = Admission<S>;

    fn layer(&self, inner: S) -> Self::Service {
        Admission {
            inner: ConcurrencyLimit::with_semaphore(inner, self.semaphore.clone()),
            is_ready: false,
        }
    }
}

#[derive(Debug)]
pub struct Admission<S> {
    inner: ConcurrencyLimit<S>,
    is_ready: bool,
}

impl<S: Clone> Clone for Admission<S> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            is_ready: false,
        }
    }
}

impl<S, Request> Service<Request> for Admission<S>
where
    S: Service<Request, Error = AgentError>,
    S::Future: Send + 'static,
    S::Response: Send + 'static,
    Request: 'static,
{
    type Response = S::Response;
    type Error = AgentError;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.is_ready = match self.inner.poll_ready(cx) {
            Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
            readiness => readiness.is_ready(),
        };

        // Load shedding is intentional: callers receive Busy instead of
        // waiting in a hidden queue for a permit.
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: Request) -> Self::Future {
        if self.is_ready {
            self.is_ready = false;
            Box::pin(self.inner.call(request))
        } else {
            Box::pin(async { Err(AgentError::busy()) })
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::sync::{Notify, oneshot};
    use tower::{ServiceBuilder, ServiceExt, service_fn};

    use super::*;
    use crate::{AgentRequest, ErrorKind, Turn, TurnOutcome};

    #[tokio::test]
    async fn clones_share_capacity_and_shed_without_queueing() {
        let release = Arc::new(Notify::new());
        let provider_release = release.clone();
        let (started_tx, started_rx) = oneshot::channel::<()>();
        let started = Arc::new(std::sync::Mutex::new(Some(started_tx)));
        let provider_started = started.clone();
        let provider = service_fn(move |request: AgentRequest<Turn>| {
            let release = provider_release.clone();
            let started = provider_started.clone();
            async move {
                if let Some(sender) = started.lock().expect("started lock").take() {
                    let _ = sender.send(());
                }
                release.notified().await;
                Ok::<_, AgentError>(TurnOutcome::new(request.body.prompt))
            }
        });
        let service = ServiceBuilder::new()
            .layer(AdmissionLayer::single_flight())
            .service(provider);

        let first_service = service.clone();
        let first = tokio::spawn(async move {
            first_service
                .oneshot(AgentRequest::new(Turn::new("first")))
                .await
        });
        started_rx.await.expect("first call starts");

        let error = service
            .clone()
            .oneshot(AgentRequest::new(Turn::new("second")))
            .await
            .expect_err("second call is shed");
        assert_eq!(error.kind, ErrorKind::Busy);

        release.notify_waiters();
        assert_eq!(
            first
                .await
                .expect("first task joins")
                .expect("first succeeds")
                .output,
            "first"
        );
    }
}
