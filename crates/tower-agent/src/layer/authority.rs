use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use tower::{Layer, Service};

use crate::{AgentError, AgentRequest, AuthorityPolicy, RequestsFilesystemAuthority, Turn};

/// Rejects filesystem authority beyond a host-owned policy before provider work.
#[derive(Clone, Debug)]
pub struct AuthorityLayer {
    policy: AuthorityPolicy,
}

impl AuthorityLayer {
    pub const fn new(policy: AuthorityPolicy) -> Self {
        Self { policy }
    }

    pub const fn read_only() -> Self {
        Self::new(AuthorityPolicy::read_only())
    }

    pub const fn policy(&self) -> &AuthorityPolicy {
        &self.policy
    }
}

impl Default for AuthorityLayer {
    fn default() -> Self {
        Self::read_only()
    }
}

impl<S> Layer<S> for AuthorityLayer {
    type Service = EnforceAuthority<S>;

    fn layer(&self, inner: S) -> Self::Service {
        EnforceAuthority {
            inner,
            policy: self.policy.clone(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct EnforceAuthority<S> {
    inner: S,
    policy: AuthorityPolicy,
}

impl<S, O> Service<AgentRequest<Turn<O>>> for EnforceAuthority<S>
where
    S: Service<AgentRequest<Turn<O>>, Error = AgentError>,
    S::Future: Send + 'static,
    S::Response: Send + 'static,
    O: RequestsFilesystemAuthority,
{
    type Response = S::Response;
    type Error = AgentError;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(context)
    }

    fn call(&mut self, request: AgentRequest<Turn<O>>) -> Self::Future {
        match self.policy.authorize(&request.body) {
            Ok(()) => Box::pin(self.inner.call(request)),
            Err(error) => Box::pin(async move { Err(error) }),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tower::{ServiceExt, service_fn};

    use super::*;
    use crate::{EffectState, ErrorKind, FilesystemAuthority, TurnOutcome};

    #[derive(Clone, Copy)]
    struct Options(FilesystemAuthority);

    impl RequestsFilesystemAuthority for Options {
        fn filesystem_authority(&self) -> FilesystemAuthority {
            self.0
        }
    }

    #[tokio::test]
    async fn excessive_authority_never_reaches_the_provider() {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider_calls = calls.clone();
        let provider = service_fn(move |_request: AgentRequest<Turn<Options>>| {
            provider_calls.fetch_add(1, Ordering::SeqCst);
            async { Ok::<_, AgentError>(TurnOutcome::new("unexpected")) }
        });
        let service = AuthorityLayer::read_only().layer(provider);
        let request = AgentRequest::new(
            Turn::new("write a file").with_options(Options(FilesystemAuthority::WorkspaceWrite)),
        );

        let error = service
            .oneshot(request)
            .await
            .expect_err("host ceiling must reject the request");

        assert_eq!(error.kind, ErrorKind::Unauthorized);
        assert_eq!(error.effects, EffectState::None);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
}
