use std::{
    future::Future,
    marker::PhantomData,
    pin::Pin,
    task::{Context, Poll},
};

use tower::{Service, ServiceExt};
use tower_agent::{AgentError, AgentRequest, EffectState, Turn, TurnOutcome};

use crate::StepCall;

/// Adapt one fully composed, typed Tower Agent service into a workflow step.
///
/// The synchronous preparer constructs only the typed [`Turn`]. The adapter
/// always constructs the [`AgentRequest`] with the workflow step's cancellation
/// token and absolute deadline, so a preparer cannot accidentally discard the
/// cooperative settlement contract. Applications that need per-step event
/// observers or preassigned sessions can build an `AgentRequest` directly in
/// their dispatcher instead.
///
/// This convenience adapter adds no retry, deadline layer, supervision, or
/// persistence. In particular, durable job resolution should be modeled as an
/// asynchronous dispatcher service with its own error type, not hidden in this
/// synchronous preparer.
pub struct AgentStepService<S, P, Options> {
    inner: S,
    prepare: P,
    options: PhantomData<fn() -> Options>,
}

impl<S, P, Options> AgentStepService<S, P, Options> {
    /// Construct an adapter around a fully composed agent service and a turn preparer.
    ///
    /// `prepare` runs synchronously before the inner service is asked for
    /// readiness. It receives the complete workflow step call and must return
    /// only the typed turn body; this adapter attaches the host-local context.
    pub fn new(inner: S, prepare: P) -> Self {
        Self {
            inner,
            prepare,
            options: PhantomData,
        }
    }

    /// Borrow the fully composed agent service wrapped by this adapter.
    pub fn inner(&self) -> &S {
        &self.inner
    }
}

impl<S, P, Options> Clone for AgentStepService<S, P, Options>
where
    S: Clone,
    P: Clone,
{
    fn clone(&self) -> Self {
        Self::new(self.inner.clone(), self.prepare.clone())
    }
}

impl<S, P, Options, I, J> Service<StepCall<I, J, TurnOutcome>> for AgentStepService<S, P, Options>
where
    S: Service<AgentRequest<Turn<Options>>, Response = TurnOutcome, Error = AgentError>
        + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
    P: Fn(StepCall<I, J, TurnOutcome>) -> Result<Turn<Options>, AgentError> + Send + Sync + 'static,
    Options: Send + 'static,
{
    type Response = TurnOutcome;
    type Error = AgentError;
    type Future = Pin<Box<dyn Future<Output = Result<TurnOutcome, AgentError>> + Send>>;

    fn poll_ready(&mut self, _context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // Preparation can fail before an inner call, so reserving inner
        // readiness here could leave a permit unconsumed. Preparation happens
        // first; the returned future then drives readiness and call on the
        // same clone while remaining cancellation-aware before `call`.
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, call: StepCall<I, J, TurnOutcome>) -> Self::Future {
        let context = call.agent_context();
        let turn = match (self.prepare)(call) {
            Ok(turn) => turn,
            Err(error) => return Box::pin(async move { Err(error) }),
        };
        let request = AgentRequest::with_context(turn, context);
        let cancellation = request.context.cancellation().clone();
        let mut service = self.inner.clone();
        Box::pin(async move {
            let ready = tokio::select! {
                biased;
                () = cancellation.cancelled() => {
                    return Err(AgentError::cancelled(EffectState::None));
                }
                ready = service.ready() => ready?,
            };
            if cancellation.is_cancelled() {
                return Err(AgentError::cancelled(EffectState::None));
            }
            ready.call(request).await
        })
    }
}
