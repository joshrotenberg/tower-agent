//! Circuit-break a non-effectful agent service without retrying calls.
//!
//! This example uses `FakeService`, whose scripted failures do not perform
//! external effects. An automatic half-open probe is only appropriate when a
//! real provider is configured with an equally strong non-effectful contract.
//! Effectful turns need externally controlled recovery instead.

use std::time::Duration;

use tower::{Layer, ServiceBuilder, ServiceExt};
use tower_agent::layer::{
    AdmissionLayer, CatchPanicLayer, DeadlineLayer, SuperviseLayer, ValidateTurnLayer,
};
use tower_agent::{
    AgentError, AgentRequest, EffectState, ErrorKind, FailurePhase, FakeOptions, FakeService, Turn,
    TurnOutcome,
};
use tower_resilience::circuitbreaker::{CircuitBreakerError, CircuitBreakerLayer};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (breaker_layer, breaker) = CircuitBreakerLayer::builder()
        .name("fake-agent")
        .consecutive_failures(2)
        .wait_duration_in_open(Duration::from_millis(50))
        .permitted_calls_in_half_open(1)
        .failure_classifier(classify_provider_health)
        .build_with_handle();

    // Keep Supervise outside the breaker. If the caller drops its future,
    // Supervise retains the breaker and provider call through settlement, so
    // the breaker records the actual terminal result.
    let provider = ServiceBuilder::new()
        .layer(CatchPanicLayer::new())
        .layer(AdmissionLayer::single_flight())
        .layer(DeadlineLayer::new())
        .layer(ValidateTurnLayer::new())
        .service(FakeService);
    let protected = breaker_layer.layer(provider).map_err(map_breaker_error);
    let service = ServiceBuilder::new()
        .layer(SuperviseLayer::new())
        .service(protected);

    // Invalid requests are caller failures, so they do not count toward the
    // provider-health circuit.
    let invalid = service
        .clone()
        .oneshot(request("", None))
        .await
        .expect_err("empty prompt is invalid");
    println!(
        "invalid request: {} (circuit {:?})",
        invalid.kind,
        breaker.state()
    );

    for attempt in 1..=2 {
        let error = service
            .clone()
            .oneshot(request("run", Some("provider unavailable")))
            .await
            .expect_err("scripted provider failure");
        println!(
            "provider failure {attempt}: {} (circuit {:?})",
            error.kind,
            breaker.state()
        );
    }

    // The open circuit rejects before provider launch and maps back into the
    // kernel's typed admission vocabulary.
    let rejected = service
        .clone()
        .oneshot(request("healthy call", None))
        .await
        .expect_err("open circuit rejects the call");
    assert_eq!(rejected.kind, ErrorKind::Busy);
    assert_eq!(rejected.phase, FailurePhase::Admission);
    assert_eq!(rejected.effects, EffectState::None);
    println!("open-circuit rejection: {rejected}");

    // The automatic half-open probe is safe here only because FakeService is
    // non-effectful. Do not let an arbitrary effectful turn become a probe.
    tokio::time::sleep(Duration::from_millis(75)).await;
    let outcome = service.oneshot(request("healthy call", None)).await?;
    println!("half-open probe recovered: {}", outcome.output);

    Ok(())
}

fn request(prompt: &str, failure: Option<&str>) -> AgentRequest<Turn<FakeOptions>> {
    AgentRequest::new(Turn::new(prompt).with_options(FakeOptions {
        fail: failure.map(str::to_owned),
        ..FakeOptions::default()
    }))
}

fn classify_provider_health(result: &Result<TurnOutcome, AgentError>) -> bool {
    matches!(
        result,
        Err(error) if matches!(error.kind, ErrorKind::Provider | ErrorKind::Internal)
    )
}

fn map_breaker_error(error: CircuitBreakerError<AgentError>) -> AgentError {
    match error {
        CircuitBreakerError::Inner(error) => error,
        // Unavailable, not Busy: this host has capacity, the provider is
        // the thing that is down. A caller that can reach another provider
        // should, and one that cannot should wait rather than retry now.
        CircuitBreakerError::OpenCircuit => {
            AgentError::unavailable("agent provider circuit is open")
        }
    }
}
