use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
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
            inner,
            semaphore: self.semaphore.clone(),
            permit: None,
        }
    }
}

/// Shared capacity with immediate typed shedding.
///
/// The permit is acquired only after the inner service reports ready, so a
/// stalled inner service can never park capacity that a shed request would
/// then be denied. An acquired permit is moved into the response future and
/// released when that future settles, which keeps capacity occupied through
/// cleanup rather than freeing it when the caller disappears.
#[derive(Debug)]
pub struct Admission<S> {
    inner: S,
    semaphore: Arc<Semaphore>,
    permit: Option<OwnedSemaphorePermit>,
}

impl<S: Clone> Clone for Admission<S> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            semaphore: self.semaphore.clone(),
            permit: None,
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
        match self.inner.poll_ready(cx) {
            Poll::Ready(Err(error)) => {
                // A service that errors from readiness is finished. Release
                // any reservation and do not leave stale readiness behind for
                // a caller that retries instead of dropping the service.
                self.permit = None;
                return Poll::Ready(Err(error));
            }
            Poll::Ready(Ok(())) => {}
            Poll::Pending => {
                // The inner service is stalled, so this call will shed. Hold
                // no capacity while doing so: parking a permit here would
                // deny every other clone for as long as this one lives.
                self.permit = None;
                return Poll::Ready(Ok(()));
            }
        }

        if self.permit.is_none() {
            self.permit = Arc::clone(&self.semaphore).try_acquire_owned().ok();
        }

        // Load shedding is intentional: callers receive Busy instead of
        // waiting in a hidden queue for a permit.
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: Request) -> Self::Future {
        let Some(permit) = self.permit.take() else {
            return Box::pin(async { Err(AgentError::busy()) });
        };
        let future = self.inner.call(request);
        Box::pin(async move {
            // Capacity stays occupied until the inner call settles, including
            // every cleanup path.
            let _permit = permit;
            future.await
        })
    }
}

#[cfg(test)]
mod tests {
    use std::future::{Ready, ready};

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

    /// A service whose readiness the test drives directly.
    #[derive(Clone)]
    struct ControlledReadiness {
        readiness: Arc<std::sync::Mutex<Poll<Result<(), AgentError>>>>,
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl ControlledReadiness {
        fn new(initial: Poll<Result<(), AgentError>>) -> Self {
            Self {
                readiness: Arc::new(std::sync::Mutex::new(initial)),
                calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }
        }

        fn set(&self, readiness: Poll<Result<(), AgentError>>) {
            *self.readiness.lock().expect("readiness lock") = readiness;
        }

        fn calls(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::Relaxed)
        }
    }

    impl Service<AgentRequest<Turn>> for ControlledReadiness {
        type Response = TurnOutcome;
        type Error = AgentError;
        type Future = Ready<Result<TurnOutcome, AgentError>>;

        fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), AgentError>> {
            self.readiness.lock().expect("readiness lock").clone()
        }

        fn call(&mut self, request: AgentRequest<Turn>) -> Self::Future {
            self.calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            ready(Ok(TurnOutcome::new(request.body.prompt)))
        }
    }

    #[tokio::test]
    async fn shedding_a_stalled_inner_service_does_not_park_capacity() {
        let inner = ControlledReadiness::new(Poll::Pending);
        let layer = AdmissionLayer::single_flight();
        let mut stalled = layer.layer(inner.clone());
        let mut other = stalled.clone();

        // The stalled clone sheds without consuming the only permit.
        std::future::poll_fn(|cx| stalled.poll_ready(cx))
            .await
            .expect("admission reports ready");
        let error = stalled
            .call(AgentRequest::new(Turn::new("shed")))
            .await
            .expect_err("a stalled inner service sheds");
        assert_eq!(error.kind, ErrorKind::Busy);
        assert_eq!(inner.calls(), 0);

        // A second clone must still be admitted while the first one lives.
        inner.set(Poll::Ready(Ok(())));
        std::future::poll_fn(|cx| other.poll_ready(cx))
            .await
            .expect("admission reports ready");
        let outcome = other
            .call(AgentRequest::new(Turn::new("admitted")))
            .await
            .expect("capacity was never parked by the shed call");
        assert_eq!(outcome.output, "admitted");
    }

    #[tokio::test]
    async fn a_readiness_error_clears_reserved_capacity_and_readiness() {
        let inner = ControlledReadiness::new(Poll::Ready(Ok(())));
        let layer = AdmissionLayer::single_flight();
        let mut service = layer.layer(inner.clone());
        let mut other = service.clone();

        std::future::poll_fn(|cx| service.poll_ready(cx))
            .await
            .expect("first readiness succeeds");

        inner.set(Poll::Ready(Err(AgentError::new(
            ErrorKind::Internal,
            "provider is finished",
            crate::FailurePhase::Admission,
            crate::EffectState::None,
        ))));
        let error = std::future::poll_fn(|cx| service.poll_ready(cx))
            .await
            .expect_err("readiness reports the terminal error");
        assert_eq!(error.kind, ErrorKind::Internal);

        // A caller that retries instead of dropping the service must not be
        // routed into a service that already failed.
        let error = service
            .call(AgentRequest::new(Turn::new("after error")))
            .await
            .expect_err("stale readiness must not admit the request");
        assert_eq!(error.kind, ErrorKind::Busy);
        assert_eq!(inner.calls(), 0);

        // The reservation was released, so another clone can proceed.
        inner.set(Poll::Ready(Ok(())));
        std::future::poll_fn(|cx| other.poll_ready(cx))
            .await
            .expect("second clone is ready");
        other
            .call(AgentRequest::new(Turn::new("recovered")))
            .await
            .expect("capacity was released by the readiness error");
    }

    #[tokio::test]
    async fn capacity_admits_exactly_the_configured_concurrency() {
        let inner = ControlledReadiness::new(Poll::Ready(Ok(())));
        let layer = AdmissionLayer::new(2);
        let mut first = layer.layer(inner.clone());
        let mut second = first.clone();
        let mut third = first.clone();

        for service in [&mut first, &mut second, &mut third] {
            std::future::poll_fn(|cx| service.poll_ready(cx))
                .await
                .expect("admission reports ready");
        }

        // Two permits exist, so the third clone is shed while the first two
        // hold theirs.
        let error = third
            .call(AgentRequest::new(Turn::new("third")))
            .await
            .expect_err("capacity is exhausted");
        assert_eq!(error.kind, ErrorKind::Busy);
        first
            .call(AgentRequest::new(Turn::new("first")))
            .await
            .expect("first is admitted");
        second
            .call(AgentRequest::new(Turn::new("second")))
            .await
            .expect("second is admitted");
    }

    #[tokio::test]
    async fn separate_layers_do_not_share_capacity() {
        let inner = ControlledReadiness::new(Poll::Ready(Ok(())));
        let mut first = AdmissionLayer::single_flight().layer(inner.clone());
        let mut second = AdmissionLayer::single_flight().layer(inner.clone());

        for service in [&mut first, &mut second] {
            std::future::poll_fn(|cx| service.poll_ready(cx))
                .await
                .expect("admission reports ready");
        }
        first
            .call(AgentRequest::new(Turn::new("first")))
            .await
            .expect("first layer admits");
        second
            .call(AgentRequest::new(Turn::new("second")))
            .await
            .expect("a separate layer has its own capacity");
    }
}
