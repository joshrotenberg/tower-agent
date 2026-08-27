//! One Tower service handling both agent and mechanical steps.
//!
//! This is where the three crates meet. The workflow crate decides which
//! steps are ready and hands each one here; the planning crate turns a
//! profile plus a prompt into a provider-committed turn; the router sends
//! that turn to the service registered for its provider.
//!
//! Notably the host does not use `AgentStepService`. That adapter binds one
//! `Turn<Options>` type, and a host routing across providers needs
//! `ReadyTurn`, whose whole purpose is to carry a turn whose provider is
//! already decided. The friction report records this.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use tower::{Service, ServiceExt};
use tower_agent::{AgentError, AgentRequest, EffectState, ErrorKind, FailurePhase};
use tower_agent_plan::{Layers, PartialTurn, Prepared, Profile, RoutedTurnService, prepare};
use tower_agent_workflow::StepCall;

use crate::job::{Job, MechanicalOp, ProfileCatalog};

/// What a step produces. Shared by both job kinds so dependencies can be
/// consumed uniformly.
#[derive(Clone, Debug, PartialEq)]
pub struct StepOutput {
    pub text: String,
}

/// Shared immutable input for a whole run.
#[derive(Clone, Debug)]
pub struct RunInput {
    pub repository: String,
    pub branch: String,
}

/// The host dispatcher.
#[derive(Clone)]
pub struct RepositoryWorker {
    router: RoutedTurnService,
    profiles: std::sync::Arc<ProfileCatalog>,
}

impl RepositoryWorker {
    pub fn new(router: RoutedTurnService, profiles: ProfileCatalog) -> Self {
        Self {
            router,
            profiles: std::sync::Arc::new(profiles),
        }
    }
}

type Call = StepCall<RunInput, Job, StepOutput>;

impl Service<Call> for RepositoryWorker {
    type Response = StepOutput;
    type Error = AgentError;
    type Future = Pin<Box<dyn Future<Output = Result<StepOutput, AgentError>> + Send + 'static>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), AgentError>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, call: Call) -> Self::Future {
        match call.job.clone() {
            Job::Mechanical { op, args } => {
                // Runs in process. No shell, no subprocess, no serialized
                // envelope: #106 has to settle that boundary before one
                // exists.
                let result = run_mechanical(op, &args, &call);
                Box::pin(async move { result })
            }
            Job::Agent {
                profile,
                prompt,
                provider,
            } => {
                let profiles = self.profiles.clone();
                let router = self.router.clone();
                // The context comes from the step, so the workflow's
                // cancellation token and deadline reach the provider rather
                // than being reinvented here.
                let context = call.agent_context();
                Box::pin(async move {
                    let ready = plan_turn(&profiles, &profile, &prompt, provider)?;
                    let outcome = router
                        .oneshot(AgentRequest::with_context(ready, context))
                        .await?;
                    Ok(StepOutput {
                        text: outcome.output,
                    })
                })
            }
        }
    }
}

/// Resolve a profile and a prompt into a provider-committed turn.
///
/// Compilation already proved this resolves, so a failure here is a host bug
/// rather than a configuration error, and is reported as internal.
fn plan_turn(
    profiles: &ProfileCatalog,
    profile: &str,
    prompt: &str,
    compiled_provider: tower_agent_plan::ProviderId,
) -> Result<tower_agent_plan::ReadyTurn, AgentError> {
    let saved: &Profile = profiles.get(profile).ok_or_else(|| {
        AgentError::new(
            ErrorKind::Internal,
            "compiled step names a profile the catalog no longer has",
            FailurePhase::Validation,
            EffectState::None,
        )
    })?;
    let explicit = PartialTurn {
        prompt: Some(prompt.to_string()),
        ..Default::default()
    };
    // Compilation recorded which provider this step resolved to. Re-resolving
    // must reach the same one, or the definition a person reviewed is not the
    // definition being run.
    if saved.turn.provider != Some(compiled_provider) {
        return Err(AgentError::new(
            ErrorKind::Internal,
            "profile now selects a different provider than compilation recorded",
            FailurePhase::Validation,
            EffectState::None,
        ));
    }
    match prepare(Layers::new(&explicit).with_profile(saved)) {
        Prepared::Ready(ready) => Ok(ready),
        Prepared::Missing { requirements, .. } => Err(AgentError::new(
            ErrorKind::Internal,
            format!(
                "compiled step is missing {} value(s) that compilation accepted",
                requirements.len()
            ),
            FailurePhase::Validation,
            EffectState::None,
        )),
        Prepared::Invalid { diagnostics } => Err(AgentError::new(
            ErrorKind::Internal,
            format!(
                "compiled step became invalid after compilation: {} diagnostic(s)",
                diagnostics.len()
            ),
            FailurePhase::Validation,
            EffectState::None,
        )),
    }
}

fn run_mechanical(
    op: MechanicalOp,
    args: &BTreeMap<String, String>,
    call: &Call,
) -> Result<StepOutput, AgentError> {
    let text = match op {
        MechanicalOp::ReadBranch => format!("{}@{}", call.input.repository, call.input.branch),
        MechanicalOp::CountFiles => {
            let pattern = args.get("pattern").map_or("*", String::as_str);
            format!("counted files matching {pattern}")
        }
        MechanicalOp::Collect => {
            // Direct dependency results only, which is the contract the
            // workflow crate offers and the thing worth exercising.
            let parts: Vec<&str> = call
                .dependencies
                .values()
                .map(|output| output.text.as_str())
                .collect();
            format!("collected [{}]", parts.join(" | "))
        }
    };
    Ok(StepOutput { text })
}
