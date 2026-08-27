use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tower::{Layer, Service};

use crate::{AgentError, AgentRequest, EffectState, ErrorKind, FailurePhase, OperationId};

#[derive(Clone, Debug, PartialEq, Eq)]
/// A terminal record of one call, emitted exactly once.
///
/// Recorded even when the caller vanishes, so a host can account for
/// work that no longer has anyone waiting on it.
pub struct Receipt {
    /// Identity of the call this record describes.
    pub operation_id: OperationId,
    /// Wall-clock time from entering the layer to settling.
    pub elapsed: Duration,
    /// How the call ended.
    pub status: ReceiptStatus,
}

#[derive(Clone, Debug, PartialEq, Eq)]
/// How a call ended, from the observing layer's point of view.
pub enum ReceiptStatus {
    /// The call returned a successful response.
    Succeeded,
    /// The call returned a typed failure.
    ///
    /// Carries the classification rather than the message, so a consumer
    /// aggregates on stable axes.
    Failed {
        /// What kind of failure it was.
        kind: ErrorKind,
        /// How far the call got.
        phase: FailurePhase,
        /// What is known about external effects.
        effects: EffectState,
    },
    /// The call was dropped before it settled.
    ///
    /// `effects` distinguishes a future dropped before its first poll, where
    /// nothing ran, from one dropped after the provider began work. Without
    /// it a receipt consumer would have to guess, and would be wrong whenever
    /// it guessed the other way.
    Abandoned {
        /// What is known about external effects at the moment of the drop.
        effects: EffectState,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
/// Why a receipt could not be delivered.
///
/// A dropped receipt is lost accounting, unlike a dropped event.
/// Size the sink so this does not happen.
pub enum ReceiptSendError {
    #[error("receipt observer is full")]
    /// The sink is at capacity. This receipt was discarded.
    Full,
    #[error("receipt observer is closed")]
    /// The consumer is gone. Later receipts will be discarded too.
    Closed,
}

/// A nonblocking terminal receipt destination.
pub trait ReceiptSink: Send + Sync + 'static {
    /// Record one terminal receipt without blocking.
    ///
    /// Called from the call's own path and from `Drop`, so an
    /// implementation that blocks stalls a settling turn.
    fn try_record(&self, receipt: Receipt) -> Result<(), ReceiptSendError>;
}

#[derive(Clone)]
/// A cloneable handle to a [`ReceiptSink`].
pub struct ReceiptObserver(Arc<dyn ReceiptSink>);

impl ReceiptObserver {
    /// Record through a caller-supplied sink.
    pub fn new(sink: impl ReceiptSink) -> Self {
        Self(Arc::new(sink))
    }

    /// A channel sink with a fixed capacity.
    ///
    /// Receipts are dropped once `capacity` are outstanding, so a consumer
    /// that stops draining loses accounting.
    pub fn channel(capacity: usize) -> (Self, mpsc::Receiver<Receipt>) {
        let (sender, receiver) = mpsc::channel(capacity);
        (Self::new(ChannelReceiptSink(sender)), receiver)
    }

    /// Record one receipt, discarding it if the sink refuses.
    pub fn try_record(&self, receipt: Receipt) -> Result<(), ReceiptSendError> {
        self.0.try_record(receipt)
    }
}

impl Default for ReceiptObserver {
    fn default() -> Self {
        Self::new(NoopReceiptSink)
    }
}

impl fmt::Debug for ReceiptObserver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ReceiptObserver").field(&"..").finish()
    }
}

#[derive(Clone, Debug)]
/// Emits one [`Receipt`] per call, including abandoned ones.
pub struct ObserveLayer {
    observer: ReceiptObserver,
}

impl ObserveLayer {
    /// Emit receipts to `observer`.
    pub fn new(observer: ReceiptObserver) -> Self {
        Self { observer }
    }
}

impl<S> Layer<S> for ObserveLayer {
    type Service = Observe<S>;

    fn layer(&self, inner: S) -> Self::Service {
        Observe {
            inner,
            observer: self.observer.clone(),
        }
    }
}

#[derive(Clone, Debug)]
/// The [`ObserveLayer`] service. See that type for behavior.
pub struct Observe<S> {
    inner: S,
    observer: ReceiptObserver,
}

impl<S, T> Service<AgentRequest<T>> for Observe<S>
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
        let operation_id = request.context.operation_id();
        let future = self.inner.call(request);
        let mut guard = ReceiptGuard::new(self.observer.clone(), operation_id);

        Box::pin(async move {
            // Reached only once this future is polled, which is also when the
            // inner call is first polled, so an abandonment recorded after
            // this point may have produced effects.
            guard.mark_launched();
            let result = future.await;
            let status = match &result {
                Ok(_) => ReceiptStatus::Succeeded,
                Err(error) => ReceiptStatus::Failed {
                    kind: error.kind,
                    phase: error.phase,
                    effects: error.effects,
                },
            };
            guard.finish(status);
            result
        })
    }
}

struct ReceiptGuard {
    observer: ReceiptObserver,
    operation_id: OperationId,
    started: Instant,
    finished: bool,
    launched: bool,
}

impl ReceiptGuard {
    fn new(observer: ReceiptObserver, operation_id: OperationId) -> Self {
        Self {
            observer,
            operation_id,
            started: Instant::now(),
            finished: false,
            launched: false,
        }
    }

    fn mark_launched(&mut self) {
        self.launched = true;
    }

    fn finish(&mut self, status: ReceiptStatus) {
        self.finished = true;
        let _ = self.observer.try_record(Receipt {
            operation_id: self.operation_id,
            elapsed: self.started.elapsed(),
            status,
        });
    }
}

impl Drop for ReceiptGuard {
    fn drop(&mut self) {
        if !self.finished {
            let effects = if self.launched {
                EffectState::Possible
            } else {
                EffectState::None
            };
            let _ = self.observer.try_record(Receipt {
                operation_id: self.operation_id,
                elapsed: self.started.elapsed(),
                status: ReceiptStatus::Abandoned { effects },
            });
        }
    }
}

struct NoopReceiptSink;

impl ReceiptSink for NoopReceiptSink {
    fn try_record(&self, _receipt: Receipt) -> Result<(), ReceiptSendError> {
        Ok(())
    }
}

struct ChannelReceiptSink(mpsc::Sender<Receipt>);

impl ReceiptSink for ChannelReceiptSink {
    fn try_record(&self, receipt: Receipt) -> Result<(), ReceiptSendError> {
        self.0.try_send(receipt).map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => ReceiptSendError::Full,
            mpsc::error::TrySendError::Closed(_) => ReceiptSendError::Closed,
        })
    }
}

#[cfg(test)]
mod tests {
    use tower::{Service, ServiceBuilder, ServiceExt, service_fn};

    use super::*;
    use crate::{AgentRequest, Turn, TurnOutcome};

    #[tokio::test]
    async fn records_typed_terminal_status_with_operation_identity() {
        let (observer, mut receipts) = ReceiptObserver::channel(1);
        let provider = service_fn(|_request: AgentRequest<Turn>| async {
            Err::<TurnOutcome, _>(AgentError::invalid_request("bad input"))
        });
        let service = ServiceBuilder::new()
            .layer(ObserveLayer::new(observer))
            .service(provider);
        let operation_id = OperationId::from_u64(42);
        let request = AgentRequest::with_context(
            Turn::new("hello"),
            crate::CallContext::new().with_operation_id(operation_id),
        );

        let _ = service.oneshot(request).await;
        let receipt = receipts.recv().await.expect("receipt");
        assert_eq!(receipt.operation_id, operation_id);
        assert_eq!(
            receipt.status,
            ReceiptStatus::Failed {
                kind: ErrorKind::InvalidRequest,
                phase: FailurePhase::Validation,
                effects: EffectState::None,
            }
        );
    }

    #[tokio::test]
    async fn dropping_before_first_poll_records_abandonment() {
        let (observer, mut receipts) = ReceiptObserver::channel(1);
        let provider = service_fn(|_request: AgentRequest<Turn>| async {
            std::future::pending::<Result<TurnOutcome, AgentError>>().await
        });
        let mut service = ServiceBuilder::new()
            .layer(ObserveLayer::new(observer))
            .service(provider);
        service.ready().await.expect("service ready");
        let operation_id = OperationId::from_u64(7);
        let request = AgentRequest::with_context(
            Turn::new("hello"),
            crate::CallContext::new().with_operation_id(operation_id),
        );

        let future = service.call(request);
        drop(future);

        let receipt = receipts.recv().await.expect("abandoned receipt");
        assert_eq!(receipt.operation_id, operation_id);
        // Never polled, so the provider never ran and no effect is possible.
        assert_eq!(
            receipt.status,
            ReceiptStatus::Abandoned {
                effects: EffectState::None
            }
        );
    }

    #[tokio::test]
    async fn dropping_after_the_call_starts_records_possible_effects() {
        let (observer, mut receipts) = ReceiptObserver::channel(1);
        let entered = std::sync::Arc::new(tokio::sync::Notify::new());
        let provider_entered = entered.clone();
        let provider = service_fn(move |_request: AgentRequest<Turn>| {
            let entered = provider_entered.clone();
            async move {
                entered.notify_one();
                std::future::pending::<Result<TurnOutcome, AgentError>>().await
            }
        });
        let mut service = ServiceBuilder::new()
            .layer(ObserveLayer::new(observer))
            .service(provider);
        service.ready().await.expect("service ready");

        let call = tokio::spawn(service.call(AgentRequest::new(Turn::new("hello"))));
        entered.notified().await;
        call.abort();

        let receipt = receipts.recv().await.expect("abandoned receipt");
        // The provider began work before the caller vanished, so a consumer
        // must not treat this as effect-free.
        assert_eq!(
            receipt.status,
            ReceiptStatus::Abandoned {
                effects: EffectState::Possible
            }
        );
    }
}
