//! Fan out a repository review to two typed fake providers, then synthesize it.
//!
//! Run with:
//! `cargo run -p tower-agent-workflow --example fake_fanout`

use std::{error::Error, num::NonZeroUsize, sync::Arc, time::Duration};

use tower::{ServiceBuilder, ServiceExt, service_fn};
use tower_agent::{
    AgentError, AgentRequest, BoxTurnService, Cost, FakeOptions, FakeService, FakeStep,
    SessionHandle, TokenUsage, Turn, TurnOutcome,
    layer::{AdmissionLayer, CatchPanicLayer, DeadlineLayer, SuperviseLayer, ValidateTurnLayer},
};
use tower_agent_workflow::{
    DagBuilder, StepCall, StepId, StepSpec, WorkflowContext, WorkflowRequest, WorkflowRunId,
    WorkflowService,
};

const ARCHITECT_PROVIDER: &str = "fake-architect";
const VERIFIER_PROVIDER: &str = "fake-verifier";

#[derive(Debug)]
struct ReviewRequest {
    repository: String,
    objective: String,
}

/// Host-owned jobs stay typed. A future application layer could create these
/// from configuration without teaching the workflow crate about providers.
#[derive(Clone, Debug)]
enum ReviewJob {
    Architecture(FakeOptions),
    Verification(FakeOptions),
    Synthesize,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let definition = DagBuilder::new("repository-review", "v1")
        .step(StepSpec::new(
            "architecture",
            ReviewJob::Architecture(
                FakeOptions::succeed(review_outcome(
                    ARCHITECT_PROVIDER,
                    "architecture-session",
                    "The worker boundary is clean; keep orchestration outside provider adapters.",
                    240,
                    0.012,
                ))
                .with_script([FakeStep::Delay(Duration::from_millis(30))]),
            ),
        ))
        .step(StepSpec::new(
            "verification",
            ReviewJob::Verification(
                FakeOptions::succeed(review_outcome(
                    VERIFIER_PROVIDER,
                    "verification-session",
                    "Add cancellation, deadline, dependency, and typed-routing conformance tests.",
                    180,
                    0.009,
                ))
                .with_script([FakeStep::Delay(Duration::from_millis(20))]),
            ),
        ))
        .step(
            StepSpec::new("synthesize", ReviewJob::Synthesize)
                .needs(["architecture", "verification"]),
        )
        .build()?;

    let architect = fake_provider(ARCHITECT_PROVIDER);
    let verifier = fake_provider(VERIFIER_PROVIDER);
    let dispatcher = service_fn(
        move |call: StepCall<ReviewRequest, ReviewJob, TurnOutcome>| {
            let architect = architect.clone();
            let verifier = verifier.clone();

            async move {
                let context = call.agent_context();
                let (provider, turn) = match &call.job {
                    ReviewJob::Architecture(options) => {
                        let prompt = format!(
                            "Review the architecture of {} for: {}",
                            call.input.repository, call.input.objective
                        );
                        (architect, Turn::new(prompt).with_options(options.clone()))
                    }
                    ReviewJob::Verification(options) => {
                        let prompt = format!(
                            "Design a verification plan for {} covering: {}",
                            call.input.repository, call.input.objective
                        );
                        (verifier, Turn::new(prompt).with_options(options.clone()))
                    }
                    ReviewJob::Synthesize => {
                        let architecture = dependency(&call, "architecture")?;
                        let verification = dependency(&call, "verification")?;
                        let session = architecture.session.clone().ok_or_else(|| {
                            AgentError::invalid_request(
                                "architecture output did not include a session",
                            )
                        })?;
                        let prompt = format!(
                            "Synthesize the two reviews.\n\nArchitecture:\n{}\n\nVerification:\n{}",
                            architecture.output, verification.output
                        );
                        let options = FakeOptions {
                            output: Some(format!(
                                "Recommendation: incubate the workflow crate in-tree.\n\n{}\n{}",
                                architecture.output, verification.output
                            )),
                            simulated_tokens: Some(90),
                            simulated_cost_usd: Some(0.004),
                            ..FakeOptions::default()
                        };
                        // The synthesizer resumes the architect's provider-pinned
                        // session while consuming both direct dependency outputs.
                        (
                            architect,
                            Turn::new(prompt).with_options(options).resume(session),
                        )
                    }
                };

                provider
                    .oneshot(AgentRequest::with_context(turn, context))
                    .await
            }
        },
    );

    let workflow = WorkflowService::new(dispatcher)
        .with_max_concurrency(NonZeroUsize::new(2).expect("two is nonzero"));
    let outcome = workflow
        .oneshot(WorkflowRequest::new(
            WorkflowContext::new(WorkflowRunId::new("review-run-1")?),
            definition,
            ReviewRequest {
                repository: "tower-agent".to_owned(),
                objective: "generic multi-stage agent composition".to_owned(),
            },
        ))
        .await?;

    let synthesis = &outcome.outputs[&step_id("synthesize")];
    println!("{}", synthesis.output);
    if let Some(session) = &synthesis.session {
        println!("\nresumed provider: {}", session.provider());
    }

    Ok(())
}

fn dependency(
    call: &StepCall<ReviewRequest, ReviewJob, TurnOutcome>,
    id: &str,
) -> Result<Arc<TurnOutcome>, AgentError> {
    call.dependencies
        .get(&step_id(id))
        .cloned()
        .ok_or_else(|| AgentError::invalid_request(format!("missing `{id}` dependency output")))
}

fn step_id(value: &str) -> StepId {
    StepId::new(value).expect("example uses valid static step ids")
}

fn fake_provider(name: &str) -> BoxTurnService<FakeOptions> {
    BoxTurnService::new(
        ServiceBuilder::new()
            .layer(SuperviseLayer::new())
            .layer(CatchPanicLayer::new())
            .layer(AdmissionLayer::single_flight())
            .layer(DeadlineLayer::new())
            .layer(ValidateTurnLayer::new())
            .service(FakeService::named(name)),
    )
}

fn review_outcome(
    provider: &str,
    session: &str,
    output: &str,
    tokens: u64,
    cost_usd: f64,
) -> TurnOutcome {
    let mut outcome = TurnOutcome::new(output);
    outcome.session = Some(SessionHandle::new(provider, session));
    outcome.usage = Some(TokenUsage {
        output: Some(tokens),
        ..TokenUsage::default()
    });
    outcome.cost = Some(Cost::usd(cost_usd));
    outcome.provider_turns = Some(1);
    outcome
}
