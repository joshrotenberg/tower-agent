//! Recover an effectful provider circuit from an external health signal.
//!
//! An automatic half-open probe decides whether a provider recovered by
//! running a real call through it. When that call is an agent turn, the probe
//! writes files, spends money, or performs some other external effect purely
//! to answer a monitoring question, and it does so at the moment the provider
//! is least trusted. [`circuit_breaker.rs`](./circuit_breaker.rs) may rely on
//! automatic probing only because `FakeService` is scripted and non-effectful.
//! An effectful provider needs the recovery decision made somewhere else.
//!
//! Here the circuit runs in manual mode: it changes state only when the host
//! says so, and the thing that says so is a dedicated health operation that is
//! not an [`AgentRequest`]. Nothing this example does can turn a user's turn
//! into a probe, because no code path transitions the circuit on the result of
//! one.
//!
//! Requires `tower-resilience` 0.13.0 or later, which is where
//! `manual_mode` and the `CircuitBreakerHandle` control API
//! (`force_open`, `force_closed`, `reset`) first appear in a published
//! release.
//!
//! Run with `cargo run -p tower-agent --example health_gated_circuit`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use tower::{Layer, ServiceBuilder, ServiceExt, service_fn};
use tower_agent::layer::{
    AdmissionLayer, CatchPanicLayer, DeadlineLayer, SuperviseLayer, ValidateTurnLayer,
};
use tower_agent::{
    AgentError, AgentRequest, EffectState, ErrorKind, FailurePhase, Turn, TurnOutcome,
};
use tower_resilience::circuitbreaker::{CircuitBreakerError, CircuitBreakerLayer};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Stands in for the real provider's health, which a host learns from a
    // status endpoint, a credential check, or an operator. Never from a turn.
    let provider_is_healthy = Arc::new(AtomicBool::new(false));
    // Counts turns that actually reached the provider, so the example can
    // prove an open circuit costs nothing rather than merely claiming it.
    let launches = Arc::new(AtomicUsize::new(0));

    let (breaker_layer, circuit) = CircuitBreakerLayer::builder()
        .name("effectful-provider")
        .manual_mode()
        .build_with_handle()?;

    let provider = effectful_provider(Arc::clone(&launches), Arc::clone(&provider_is_healthy));
    let stack = ServiceBuilder::new()
        .layer(AdmissionLayer::single_flight())
        .layer(DeadlineLayer::new())
        .layer(ValidateTurnLayer::new())
        .service(provider);
    // Resilience sits above admission so an open circuit refuses before a
    // permit is spent, and below panic normalization so it sees typed results.
    let protected = breaker_layer.layer(stack).map_err(map_breaker_error);
    let service = ServiceBuilder::new()
        .layer(SuperviseLayer::new())
        .layer(CatchPanicLayer::new())
        .service(protected);

    // 1. The provider is down. The turn fails after it may already have acted,
    //    which is exactly why replaying it automatically is not an option.
    let failure = service
        .clone()
        .oneshot(request("edit the changelog"))
        .await
        .expect_err("provider is unhealthy");
    assert_eq!(failure.kind, ErrorKind::Provider);
    assert_eq!(failure.effects, EffectState::Possible);
    println!(
        "1. turn failed: {} (effects {}, circuit {:?})",
        failure.kind,
        failure.effects,
        circuit.state()
    );

    // 2. The host, not the breaker, decides to stop sending work. In manual
    //    mode nothing opens the circuit on its own, so this is the only way it
    //    opens, and it is an explicit operational act.
    circuit.force_open().await;
    assert!(circuit.is_open());
    println!("2. host opened the circuit: {:?}", circuit.state());

    // 3. While open, turns are refused before launch. Admission phase, no
    //    effects, and the provider is never reached, so a refused turn cannot
    //    have written anything.
    let launched_before = launches.load(Ordering::SeqCst);
    for attempt in 1..=3 {
        let rejected = service
            .clone()
            .oneshot(request("edit the changelog"))
            .await
            .expect_err("open circuit refuses the turn");
        assert_eq!(rejected.kind, ErrorKind::Unavailable);
        assert_eq!(rejected.phase, FailurePhase::Admission);
        assert_eq!(rejected.effects, EffectState::None);
        println!("3.{attempt} refused before launch: {rejected}");
    }
    assert_eq!(launches.load(Ordering::SeqCst), launched_before);

    // 4. The health operation is an ordinary function, not a turn. It runs
    //    while the circuit is open, which an automatic probe could not do
    //    without spending a real call, and it reports that the provider is
    //    still down. The circuit stays open, and no turn was risked to learn
    //    that.
    assert!(!check_provider_health(&provider_is_healthy).await);
    assert!(circuit.is_open());
    println!("4. health check says unhealthy, circuit stays open");

    // 5. The provider recovers. Only now does the health signal permit work,
    //    and closing is again an explicit act rather than a timer expiring.
    provider_is_healthy.store(true, Ordering::SeqCst);
    if check_provider_health(&provider_is_healthy).await {
        circuit.force_closed().await;
    }
    assert!(!circuit.is_open());
    println!("5. health check says healthy, host closed the circuit");

    // 6. Re-admitted. The first turn after recovery is a real unit of work the
    //    caller asked for, not a probe issued to answer a health question.
    let outcome = service.oneshot(request("edit the changelog")).await?;
    assert_eq!(launches.load(Ordering::SeqCst), launched_before + 1);
    println!("6. re-admitted and settled: {}", outcome.output);

    Ok(())
}

fn request(prompt: &str) -> AgentRequest<Turn> {
    AgentRequest::new(Turn::new(prompt))
}

/// A provider whose failures may already have changed the world.
///
/// The `Possible` effect state is the whole reason this example exists. A
/// failure that provably did nothing could be retried, and a circuit around it
/// could probe freely.
fn effectful_provider(
    launches: Arc<AtomicUsize>,
    healthy: Arc<AtomicBool>,
) -> impl tower::Service<
    AgentRequest<Turn>,
    Response = TurnOutcome,
    Error = AgentError,
    Future = impl Future<Output = Result<TurnOutcome, AgentError>> + Send,
> + Clone {
    service_fn(move |_request: AgentRequest<Turn>| {
        let launches = Arc::clone(&launches);
        let healthy = Arc::clone(&healthy);
        async move {
            launches.fetch_add(1, Ordering::SeqCst);
            if healthy.load(Ordering::SeqCst) {
                Ok(TurnOutcome::new("changelog edited"))
            } else {
                Err(AgentError::new(
                    ErrorKind::Provider,
                    "provider failed partway through the turn",
                    FailurePhase::Running,
                    EffectState::Possible,
                ))
            }
        }
    })
}

/// The recovery signal: a cheap, non-effectful question about the provider.
///
/// A real host asks a status endpoint, revalidates credentials, or reads an
/// operator's decision. What matters is that answering it costs no agent turn,
/// so it can be asked as often as needed while the circuit is open.
async fn check_provider_health(healthy: &AtomicBool) -> bool {
    healthy.load(Ordering::SeqCst)
}

fn map_breaker_error(error: CircuitBreakerError<AgentError>) -> AgentError {
    match error {
        CircuitBreakerError::Inner(error) => error,
        // Unavailable rather than Busy: this host has capacity, the provider
        // is the thing that is down. Admission phase with no effects, so a
        // caller knows the turn never ran and may safely send it elsewhere.
        CircuitBreakerError::OpenCircuit => {
            AgentError::unavailable("agent provider circuit is open pending a health signal")
        }
    }
}
