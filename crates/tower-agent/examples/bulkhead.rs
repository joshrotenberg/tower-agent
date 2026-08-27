//! Isolate long-lived agent calls with a fail-fast concurrency bulkhead.
//!
//! `AdmissionLayer` is the kernel's small, typed load-shedding primitive. A
//! `tower-resilience` bulkhead is useful when independently shared capacity,
//! metrics, events, or runtime controls justify a named provider partition.
//! Avoid stacking identical limits unless the scopes are intentionally
//! different (for example, a host limit outside a provider/account limit).

use std::time::Duration;

use tower::{Layer, ServiceBuilder, ServiceExt};
use tower_agent::layer::{CatchPanicLayer, DeadlineLayer, SuperviseLayer, ValidateTurnLayer};
use tower_agent::{
    AgentError, AgentEvent, AgentRequest, CallContext, EffectState, ErrorKind, EventObserver,
    FailurePhase, FakeOptions, FakeService, Turn,
};
use tower_resilience::bulkhead::{BulkheadError, BulkheadLayer, BulkheadServiceError};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let provider_partition = BulkheadLayer::builder()
        .name("fake-provider-account")
        .max_concurrent_calls(1)
        .reject_when_full()
        .build()?;
    let partitioned = provider_partition
        .layer(FakeService)
        .map_err(map_bulkhead_error);

    let protected = ServiceBuilder::new()
        .layer(CatchPanicLayer::new())
        .layer(DeadlineLayer::new())
        .layer(ValidateTurnLayer::new())
        .service(partitioned);

    // Supervise must remain outside the bulkhead. Dropping a caller then
    // signals cancellation while the supervisor retains the inner future and
    // its permit until the provider has actually settled.
    let service = ServiceBuilder::new()
        .layer(SuperviseLayer::new())
        .service(protected);

    let (events, mut event_rx) = EventObserver::channel(1);
    let slow_request = AgentRequest::with_context(
        Turn::new("slow call").with_options(FakeOptions {
            delay: Some(Duration::from_millis(100)),
            ..FakeOptions::default()
        }),
        CallContext::new().with_events(events),
    );
    let first_service = service.clone();
    let first = tokio::spawn(async move { first_service.oneshot(slow_request).await });

    assert!(matches!(event_rx.recv().await, Some(AgentEvent::Started)));
    let rejected = service
        .clone()
        .oneshot(request("concurrent call"))
        .await
        .expect_err("the provider partition is full");
    assert_eq!(rejected.kind, ErrorKind::Busy);
    assert_eq!(rejected.phase, FailurePhase::Admission);
    assert_eq!(rejected.effects, EffectState::None);
    println!("bulkhead rejection: {rejected}");

    let outcome = first.await??;
    println!("settled: {}", outcome.output);

    let next = service.oneshot(request("after settlement")).await?;
    println!("permit released: {}", next.output);

    Ok(())
}

fn request(prompt: &str) -> AgentRequest<Turn<FakeOptions>> {
    AgentRequest::new(Turn::new(prompt).with_options(FakeOptions::default()))
}

fn map_bulkhead_error(error: BulkheadServiceError<AgentError>) -> AgentError {
    match error {
        BulkheadServiceError::Inner(error) => error,
        BulkheadServiceError::Bulkhead(
            BulkheadError::BulkheadFull { .. } | BulkheadError::Timeout,
        ) => AgentError::new(
            ErrorKind::Busy,
            "agent provider partition is at capacity",
            FailurePhase::Admission,
            EffectState::None,
        ),
        // A closed partition is not a full one. Capacity will not come back,
        // so a caller that waits here waits forever, while one that can reach
        // another host may still succeed.
        BulkheadServiceError::Bulkhead(BulkheadError::Closed) => AgentError::new(
            ErrorKind::Unavailable,
            "agent provider partition is closed",
            FailurePhase::Admission,
            EffectState::None,
        ),
        // Reported when `call` runs without a successful `poll_ready`
        // reservation. That is this host breaking the Service contract, not
        // the provider misbehaving, so it must not read as backpressure.
        BulkheadServiceError::Bulkhead(BulkheadError::NotReady) => AgentError::new(
            ErrorKind::Internal,
            "bulkhead called without a readiness reservation",
            FailurePhase::Admission,
            EffectState::None,
        ),
    }
}
