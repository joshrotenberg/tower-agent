use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::task::{Context, Poll};

use futures_util::FutureExt;
use tower::{Layer, Service};

use crate::{AgentError, EffectState, ErrorKind, FailurePhase};

/// Converts a panic while polling an inner call into a typed terminal failure.
#[derive(Clone, Copy, Debug, Default)]
pub struct CatchPanicLayer;

impl CatchPanicLayer {
    pub const fn new() -> Self {
        Self
    }
}

impl<S> Layer<S> for CatchPanicLayer {
    type Service = CatchPanic<S>;

    fn layer(&self, inner: S) -> Self::Service {
        CatchPanic { inner }
    }
}

#[derive(Clone, Debug)]
pub struct CatchPanic<S> {
    inner: S,
}

impl<S, Request> Service<Request> for CatchPanic<S>
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
        match catch_unwind(AssertUnwindSafe(|| self.inner.poll_ready(cx))) {
            Ok(readiness) => readiness,
            Err(payload) => Poll::Ready(Err(panic_error(payload))),
        }
    }

    fn call(&mut self, request: Request) -> Self::Future {
        let future = match catch_unwind(AssertUnwindSafe(|| self.inner.call(request))) {
            Ok(future) => future,
            Err(payload) => return Box::pin(async move { Err(panic_error(payload)) }),
        };
        let future = AssertUnwindSafe(future).catch_unwind();
        Box::pin(async move {
            match future.await {
                Ok(result) => result,
                Err(payload) => Err(panic_error(payload)),
            }
        })
    }
}

fn panic_error(payload: Box<dyn std::any::Any + Send>) -> AgentError {
    AgentError::new(
        ErrorKind::Internal,
        panic_message(payload),
        FailurePhase::Settlement,
        EffectState::Possible,
    )
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    let detail = payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("unknown panic payload");
    format!("agent service panicked: {detail}")
}

#[cfg(test)]
mod tests {
    use std::future::Ready;

    use tower::ServiceExt;

    use super::*;
    use crate::{AgentRequest, ErrorKind, Turn, TurnOutcome};

    #[derive(Clone)]
    struct PanicInCall;

    impl Service<AgentRequest<Turn>> for PanicInCall {
        type Response = TurnOutcome;
        type Error = AgentError;
        type Future = Ready<Result<TurnOutcome, AgentError>>;

        fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), AgentError>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _request: AgentRequest<Turn>) -> Self::Future {
            panic!("synchronous call panic")
        }
    }

    #[tokio::test]
    async fn normalizes_a_synchronous_call_panic() {
        let error = CatchPanicLayer::new()
            .layer(PanicInCall)
            .oneshot(AgentRequest::new(Turn::new("hello")))
            .await
            .expect_err("panic becomes a typed error");

        assert_eq!(error.kind, ErrorKind::Internal);
        assert!(error.message.contains("synchronous call panic"));
    }

    #[derive(Clone)]
    struct PanicInReadiness;

    impl Service<AgentRequest<Turn>> for PanicInReadiness {
        type Response = TurnOutcome;
        type Error = AgentError;
        type Future = Ready<Result<TurnOutcome, AgentError>>;

        fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), AgentError>> {
            panic!("readiness panic")
        }

        fn call(&mut self, _request: AgentRequest<Turn>) -> Self::Future {
            unreachable!("readiness failed")
        }
    }

    #[tokio::test]
    async fn normalizes_a_readiness_panic() {
        let error = CatchPanicLayer::new()
            .layer(PanicInReadiness)
            .oneshot(AgentRequest::new(Turn::new("hello")))
            .await
            .expect_err("readiness panic becomes a typed error");

        assert_eq!(error.kind, ErrorKind::Internal);
        assert!(error.message.contains("readiness panic"));
    }
}
