use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use tower::{Layer, Service};

use crate::{AgentError, AgentRequest, Turn};

#[derive(Clone, Copy, Debug, Default)]
pub struct ValidateTurnLayer {
    max_prompt_bytes: Option<usize>,
}

impl ValidateTurnLayer {
    pub const fn new() -> Self {
        Self {
            max_prompt_bytes: None,
        }
    }

    pub const fn with_max_prompt_bytes(max_prompt_bytes: usize) -> Self {
        Self {
            max_prompt_bytes: Some(max_prompt_bytes),
        }
    }
}

impl<S> Layer<S> for ValidateTurnLayer {
    type Service = ValidateTurn<S>;

    fn layer(&self, inner: S) -> Self::Service {
        ValidateTurn {
            inner,
            max_prompt_bytes: self.max_prompt_bytes,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ValidateTurn<S> {
    inner: S,
    max_prompt_bytes: Option<usize>,
}

impl<S, O> Service<AgentRequest<Turn<O>>> for ValidateTurn<S>
where
    S: Service<AgentRequest<Turn<O>>, Error = AgentError>,
    S::Future: Send + 'static,
    S::Response: Send + 'static,
{
    type Response = S::Response;
    type Error = AgentError;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: AgentRequest<Turn<O>>) -> Self::Future {
        let prompt = &request.body.prompt;
        let error = if prompt.trim().is_empty() {
            Some(AgentError::invalid_request("prompt must not be empty"))
        } else if self
            .max_prompt_bytes
            .is_some_and(|maximum| prompt.len() > maximum)
        {
            Some(AgentError::invalid_request(
                "prompt exceeds the configured limit",
            ))
        } else {
            None
        };

        match error {
            Some(error) => Box::pin(async move { Err(error) }),
            None => Box::pin(self.inner.call(request)),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tower::{ServiceBuilder, ServiceExt, service_fn};

    use super::*;
    use crate::{ErrorKind, TurnOutcome};

    #[tokio::test]
    async fn invalid_requests_do_not_reach_the_provider() {
        let calls = Arc::new(AtomicUsize::new(0));
        let service_calls = calls.clone();
        let provider = service_fn(move |_request: AgentRequest<Turn>| {
            service_calls.fetch_add(1, Ordering::SeqCst);
            async { Ok::<_, AgentError>(TurnOutcome::new("unexpected")) }
        });
        let service = ServiceBuilder::new()
            .layer(ValidateTurnLayer::new())
            .service(provider);

        let error = service
            .oneshot(AgentRequest::new(Turn::new("   ")))
            .await
            .expect_err("empty prompt is rejected");
        assert_eq!(error.kind, ErrorKind::InvalidRequest);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
}
