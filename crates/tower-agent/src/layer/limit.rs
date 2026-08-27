use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use tower::{Layer, Service};

use crate::{AgentError, EffectState, ErrorKind, FailurePhase, TerminalEvidence};

/// Refuses a terminal output larger than the host allows.
///
/// A provider can emit an arbitrarily large result, and the adapters hand it
/// back as an owned `String`. This bounds what a host is willing to accept
/// before that value travels any further.
///
/// The oversized output is dropped rather than truncated. Returning a partial
/// result as success would be a lie about what the provider produced, and the
/// crate's whole contract is that terminal evidence is trustworthy. The
/// failure is typed `Limit`, carries no provider content, and keeps the
/// accounting the turn established, because the turn did run and did cost
/// money even though its output is unusable.
#[derive(Clone, Copy, Debug)]
pub struct LimitOutputLayer {
    max_bytes: usize,
}

impl LimitOutputLayer {
    /// Refuse responses whose payload exceeds `max_bytes`.
    pub const fn new(max_bytes: usize) -> Self {
        Self { max_bytes }
    }
}

impl<S> Layer<S> for LimitOutputLayer {
    type Service = LimitOutput<S>;

    fn layer(&self, inner: S) -> Self::Service {
        LimitOutput {
            inner,
            max_bytes: self.max_bytes,
        }
    }
}

#[derive(Clone, Debug)]
/// The [`LimitOutputLayer`] service. See that type for behavior.
pub struct LimitOutput<S> {
    inner: S,
    max_bytes: usize,
}

impl<S, Request> Service<Request> for LimitOutput<S>
where
    S: Service<Request, Error = AgentError>,
    S::Future: Send + 'static,
    S::Response: Send + BoundedOutput + TerminalEvidence + 'static,
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
        let future = self.inner.call(request);
        let max_bytes = self.max_bytes;
        Box::pin(async move {
            let response = future.await?;
            let bytes = response.output_bytes();
            if bytes > max_bytes {
                let evidence = response.terminal_evidence();
                drop(response);
                return Err(AgentError::new(
                    ErrorKind::Limit,
                    format!("provider output exceeded the {max_bytes}-byte host limit"),
                    FailurePhase::Settlement,
                    // The turn ran to completion; only its output is unusable.
                    EffectState::Reported,
                )
                .with_evidence(evidence));
            }
            Ok(response)
        })
    }
}

/// A response whose payload size a host may bound.
pub trait BoundedOutput {
    /// Payload bytes a caller would receive from this response.
    fn output_bytes(&self) -> usize;
}

impl BoundedOutput for crate::TurnOutcome {
    fn output_bytes(&self) -> usize {
        // Structured output counts: a schema-constrained payload is returned
        // to the caller exactly like prose and is just as unbounded.
        self.output.len()
            + self
                .structured
                .as_ref()
                .map(|value| value.to_string().len())
                .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use tower::{ServiceBuilder, ServiceExt, service_fn};

    use super::*;
    use crate::{AgentRequest, Cost, Turn, TurnOutcome};

    fn outcome(size: usize) -> TurnOutcome {
        TurnOutcome {
            cost: Some(Cost::usd(0.42)),
            provider_turns: Some(3),
            ..TurnOutcome::new("x".repeat(size))
        }
    }

    fn service(
        size: usize,
        max: usize,
    ) -> impl Service<AgentRequest<Turn>, Response = TurnOutcome, Error = AgentError> {
        ServiceBuilder::new()
            .layer(LimitOutputLayer::new(max))
            .service(service_fn(move |_: AgentRequest<Turn>| async move {
                Ok::<_, AgentError>(outcome(size))
            }))
    }

    #[tokio::test]
    async fn output_within_the_limit_passes_through() {
        let result = service(64, 1024)
            .oneshot(AgentRequest::new(Turn::new("hello")))
            .await
            .expect("within the limit");
        assert_eq!(result.output.len(), 64);
    }

    #[tokio::test]
    async fn oversized_output_fails_typed_and_keeps_accounting() {
        let error = service(2048, 1024)
            .oneshot(AgentRequest::new(Turn::new("hello")))
            .await
            .expect_err("over the limit");

        assert_eq!(error.kind, ErrorKind::Limit);
        assert_eq!(error.phase, FailurePhase::Settlement);
        // The turn ran and spent, so this is not a retryable no-op.
        assert_eq!(error.effects, EffectState::Reported);

        let evidence = error.evidence.as_deref().expect("accounting survives");
        assert_eq!(evidence.cost, Some(Cost::usd(0.42)));
        assert_eq!(evidence.provider_turns, Some(3));
    }

    #[tokio::test]
    async fn the_failure_carries_no_provider_content() {
        let error = service(2048, 1024)
            .oneshot(AgentRequest::new(Turn::new("hello")))
            .await
            .expect_err("over the limit");
        assert!(!error.message.contains("xxx"), "{}", error.message);
        assert!(!format!("{error:?}").contains("xxx"));
    }

    #[tokio::test]
    async fn structured_output_counts_toward_the_limit() {
        let big = serde_json::json!({ "blob": "y".repeat(2048) });
        let inner = service_fn(move |_: AgentRequest<Turn>| {
            let big = big.clone();
            async move {
                Ok::<_, AgentError>(TurnOutcome {
                    structured: Some(big),
                    ..TurnOutcome::new("short")
                })
            }
        });
        let error = ServiceBuilder::new()
            .layer(LimitOutputLayer::new(1024))
            .service(inner)
            .oneshot(AgentRequest::new(Turn::new("hello")))
            .await
            .expect_err("structured payloads are bounded too");
        assert_eq!(error.kind, ErrorKind::Limit);
    }
}
