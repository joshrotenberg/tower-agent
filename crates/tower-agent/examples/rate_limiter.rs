//! Apply a provider/account quota before launching an agent turn.
//!
//! Scope each limiter to the credential or provider quota it represents. A
//! single global limiter can couple otherwise independent providers and
//! tenants. This example rejects immediately instead of queueing or retrying.

use std::time::Duration;

use tower::{Layer, ServiceBuilder, ServiceExt};
use tower_agent::layer::{CatchPanicLayer, DeadlineLayer, SuperviseLayer, ValidateTurnLayer};
use tower_agent::{
    AgentError, AgentRequest, EffectState, ErrorKind, FailurePhase, FakeOptions, FakeService, Turn,
};
use tower_resilience::ratelimiter::{RateLimiterLayer, RateLimiterServiceError};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let quota = RateLimiterLayer::builder()
        .name("fake-provider-account")
        .limit_for_period(1)
        .refresh_period(Duration::from_secs(60))
        .timeout_duration(Duration::ZERO)
        .build()?;

    let limited = quota.layer(FakeService).map_err(map_rate_limiter_error);

    // Validate before consuming quota. Supervise remains outermost so a
    // launched call stays owned through provider settlement if its caller
    // drops the returned future.
    let protected = ServiceBuilder::new()
        .layer(CatchPanicLayer::new())
        .layer(DeadlineLayer::new())
        .layer(ValidateTurnLayer::new())
        .service(limited);
    let service = ServiceBuilder::new()
        .layer(SuperviseLayer::new())
        .service(protected);

    let first = service.clone().oneshot(request("first call")).await?;
    println!("admitted: {}", first.output);

    let rejected = service
        .oneshot(request("second call"))
        .await
        .expect_err("the account quota is exhausted");
    assert_eq!(rejected.kind, ErrorKind::Limit);
    assert_eq!(rejected.phase, FailurePhase::Admission);
    assert_eq!(rejected.effects, EffectState::None);
    println!("quota rejection: {rejected}");

    Ok(())
}

fn request(prompt: &str) -> AgentRequest<Turn<FakeOptions>> {
    AgentRequest::new(Turn::new(prompt).with_options(FakeOptions::default()))
}

fn map_rate_limiter_error(error: RateLimiterServiceError<AgentError>) -> AgentError {
    match error {
        RateLimiterServiceError::Inner(error) => error,
        // A quota the caller has spent, so it stays Limit. Guidance says
        // when to come back; the effect state still decides whether the
        // operation may be tried again at all.
        RateLimiterServiceError::RateLimited => AgentError::new(
            ErrorKind::Limit,
            "agent provider quota is exhausted",
            FailurePhase::Admission,
            EffectState::None,
        )
        .with_retry_after(Duration::from_secs(1)),
    }
}
