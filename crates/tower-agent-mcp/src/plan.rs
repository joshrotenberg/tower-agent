use std::str::FromStr;
use std::sync::Arc;

use serde::Deserialize;
use tower::{Service, ServiceExt};
use tower_agent::{
    AgentError, AgentRequest, CallContext, CancellationToken, EffectState, ErrorKind,
    EventObserver, FailurePhase, SessionHandle, TurnOutcome,
};
use tower_agent_plan::{
    Answer, Layers, PartialTurn, Prepared, ProviderId, ReadyTurn, Requirement, ResumeBinding,
    ValueKind, prepare,
};
use tower_mcp::extract::{Context, Json, State};
use tower_mcp::protocol::{
    ElicitAction, ElicitFieldValue, ElicitFormParams, ElicitFormSchema, PrimitiveSchemaDefinition,
    SingleSelectEnumSchema,
};
use tower_mcp::{CallToolResult, Error, Tool, ToolBuilder};

use crate::{
    ContinuationId, ContinuationStore, ProgressEvents, Projection, Scope, ScopeSource,
    tool::refuse_with,
};

/// Most elicitation rounds before a plan is abandoned.
///
/// Each round must bind at least one requirement, so a well-behaved client
/// converges quickly. The bound exists for the case where it does not, which
/// would otherwise be an unbounded exchange inside one tool call.
pub const DEFAULT_MAX_ELICITATION_ROUNDS: usize = 4;

#[derive(Deserialize, tower_mcp::schemars::JsonSchema)]
#[schemars(crate = "tower_mcp::schemars")]
struct PlanInput {
    /// The prompt for this turn. Elicited when absent.
    #[serde(default)]
    prompt: Option<String>,
    /// Which provider to run on. Elicited when absent.
    #[serde(default)]
    provider: Option<String>,
    /// A continuation identifier from an earlier result. Pins the provider to
    /// the one that minted it, so neither is elicited.
    #[serde(default)]
    continuation: Option<String>,
}

struct PlanToolState<S> {
    service: S,
    store: Arc<dyn ContinuationStore>,
    scopes: Arc<dyn ScopeSource>,
    projection: Projection,
    defaults: PartialTurn,
    providers: Vec<ProviderId>,
    max_rounds: usize,
}

impl<S: Clone> Clone for PlanToolState<S> {
    fn clone(&self) -> Self {
        Self {
            service: self.service.clone(),
            store: Arc::clone(&self.store),
            scopes: Arc::clone(&self.scopes),
            projection: self.projection,
            defaults: self.defaults.clone(),
            providers: self.providers.clone(),
            max_rounds: self.max_rounds,
        }
    }
}

/// A turn that is planned before it is run, asking the client for what is
/// missing.
///
/// [`TurnTool`](crate::TurnTool) requires a complete turn. This one accepts a
/// fragment, resolves it against host defaults, and elicits whatever is still
/// unbound. Requirements are already structured data in `tower-agent-plan`, so
/// the mapping to an elicitation form is a rendering rather than a
/// translation.
///
/// # What this deliberately does not expose
///
/// The input is a small adapter-owned shape, not `PartialTurn`. Publishing the
/// whole planning vocabulary would make an MCP schema out of a type the
/// planning crate owns and evolves, and would commit this adapter to mirroring
/// it. Host defaults, profiles, and provider baselines stay on the host side,
/// where they were already meant to live: the client supplies the explicit
/// layer and nothing beneath it.
pub struct PlanTool<S> {
    state: PlanToolState<S>,
    name: String,
    description: String,
}

impl<S> PlanTool<S>
where
    S: Service<AgentRequest<ReadyTurn>, Response = TurnOutcome, Error = AgentError>
        + Clone
        + Send
        + Sync
        + 'static,
    S::Future: Send,
{
    /// A planning tool over a service that runs compiled turns, such as
    /// `RoutedTurnService`.
    #[must_use]
    pub fn new(
        service: S,
        store: Arc<dyn ContinuationStore>,
        scopes: Arc<dyn ScopeSource>,
    ) -> Self {
        Self {
            state: PlanToolState {
                service,
                store,
                scopes,
                projection: Projection::new(),
                defaults: PartialTurn::default(),
                providers: vec![ProviderId::Claude, ProviderId::Codex],
                max_rounds: DEFAULT_MAX_ELICITATION_ROUNDS,
            },
            name: "plan".to_string(),
            description: "Plan and run one agent turn, asking for anything missing".to_string(),
        }
    }

    /// Apply host defaults beneath whatever the client supplies.
    #[must_use]
    pub fn with_application_defaults(mut self, defaults: PartialTurn) -> Self {
        self.state.defaults = defaults;
        self
    }

    /// Offer only these providers when asking the client to choose.
    ///
    /// Narrow this to the providers actually registered on the service.
    /// Offering one that is not will produce a planner diagnostic rather than
    /// a run, which is correct but is a worse thing to show a user than a
    /// shorter list.
    #[must_use]
    pub fn with_providers(mut self, providers: Vec<ProviderId>) -> Self {
        self.state.providers = providers;
        self
    }

    /// Use a projection other than the redacting default.
    #[must_use]
    pub fn with_projection(mut self, projection: Projection) -> Self {
        self.state.projection = projection;
        self
    }

    /// Bound how many times the client may be asked.
    #[must_use]
    pub fn with_max_elicitation_rounds(mut self, rounds: usize) -> Self {
        self.state.max_rounds = rounds;
        self
    }

    /// Publish the tool under a different name.
    #[must_use]
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// Build the MCP tool.
    #[must_use]
    pub fn build(self) -> Tool {
        ToolBuilder::new(self.name)
            .description(self.description)
            .extractor_handler(
                self.state,
                |State(state): State<PlanToolState<S>>,
                 context: Context,
                 Json(input): Json<PlanInput>| async move {
                    Ok::<_, Error>(run(state, context, input).await)
                },
            )
            .build()
    }
}

async fn run<S>(state: PlanToolState<S>, context: Context, input: PlanInput) -> CallToolResult
where
    S: Service<AgentRequest<ReadyTurn>, Response = TurnOutcome, Error = AgentError>
        + Clone
        + Send
        + Sync
        + 'static,
    S::Future: Send,
{
    let scope = match state.scopes.scope(&context) {
        Ok(scope) => scope,
        Err(unavailable) => {
            return refuse_with(
                &state.projection,
                AgentError::invalid_request(unavailable.to_string()),
            );
        }
    };

    let mut explicit = PartialTurn {
        prompt: input.prompt,
        ..PartialTurn::default()
    };

    // A continuation names both a conversation and the provider that owns it,
    // so resuming settles the provider rather than asking about it.
    if let Some(raw) = input.continuation.as_deref() {
        match resolve_continuation(&state, raw, &scope).await {
            Ok(session) => match ProviderId::from_str(session.provider()) {
                Ok(provider) => {
                    explicit.provider = Some(provider);
                    explicit.context.resume =
                        Some(ResumeBinding::new(provider, session.value().to_string()));
                }
                Err(_) => {
                    return refuse_with(
                        &state.projection,
                        AgentError::unsupported(
                            "the continuation belongs to a provider this build cannot plan for",
                        ),
                    );
                }
            },
            Err(error) => return refuse_with(&state.projection, error),
        }
    } else if let Some(named) = input.provider.as_deref() {
        match ProviderId::from_str(named) {
            Ok(provider) => explicit.provider = Some(provider),
            // Left unbound rather than refused, so the planner reports it as a
            // diagnostic with a stable code alongside anything else wrong.
            Err(_) => explicit.provider = None,
        }
    }

    let mut answers: Vec<Answer> = Vec::new();
    for _ in 0..state.max_rounds.max(1) {
        let layers = Layers::new(&explicit)
            .with_application_defaults(&state.defaults)
            .with_answers(&answers);

        match prepare(layers) {
            Prepared::Ready(ready) => return execute(state, context, ready, scope).await,
            Prepared::Invalid { diagnostics } => {
                let detail = diagnostics
                    .iter()
                    .map(|diagnostic| diagnostic.code.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                return refuse_with(
                    &state.projection,
                    AgentError::invalid_request(format!("plan is invalid: {detail}")),
                );
            }
            Prepared::Missing { requirements, .. } => {
                match elicit(&state, &context, &requirements).await {
                    Ok(round) if round.is_empty() => {
                        // Nothing new was bound, so another round would ask the
                        // same question and get the same answer.
                        return refuse_with(
                            &state.projection,
                            AgentError::invalid_request(
                                "the plan is still incomplete and elicitation added nothing",
                            ),
                        );
                    }
                    Ok(round) => answers.extend(round),
                    Err(error) => return refuse_with(&state.projection, error),
                }
            }
        }
    }

    refuse_with(
        &state.projection,
        AgentError::invalid_request("the plan did not resolve within the allowed rounds"),
    )
}

/// Ask the client for the unbound values.
async fn elicit(
    state: &PlanToolState<impl Send + Sync>,
    context: &Context,
    requirements: &[Requirement],
) -> Result<Vec<Answer>, AgentError> {
    // Secrets are never requirements, and a requirement marked sensitive is a
    // planner telling this adapter not to put the value in a form. Refusing is
    // the only correct response, because the alternative is asking anyway.
    if let Some(sensitive) = requirements
        .iter()
        .find(|requirement| requirement.sensitive)
    {
        return Err(AgentError::unsupported(format!(
            "requirement {} is sensitive and cannot be elicited",
            sensitive.id
        )));
    }

    let mut schema = ElicitFormSchema::new();
    for requirement in requirements {
        match requirement.kind {
            // A finite set renders as a picker rather than free text, so a
            // client cannot answer with something no planner accepts.
            ValueKind::Provider => {
                schema.properties.insert(
                    requirement.id.clone(),
                    PrimitiveSchemaDefinition::SingleSelectEnum(SingleSelectEnumSchema {
                        schema_type: "string".to_string(),
                        title: Some(requirement.label.clone()),
                        description: Some(requirement.path.clone()),
                        enum_values: state
                            .providers
                            .iter()
                            .map(|provider| provider.as_str().to_string())
                            .collect(),
                        default: None,
                    }),
                );
                schema.required.push(requirement.id.clone());
            }
            ValueKind::Text => {
                schema =
                    schema.string_field(&requirement.id, Some(requirement.label.as_str()), true);
            }
        }
    }

    let result = context
        .elicit_form(ElicitFormParams {
            mode: None,
            message: "More information is needed to run this turn".to_string(),
            requested_schema: schema,
            meta: None,
        })
        .await
        .map_err(|error| {
            AgentError::new(
                ErrorKind::Internal,
                format!("elicitation failed: {error}"),
                FailurePhase::Validation,
                EffectState::None,
            )
        })?;

    match result.action {
        // Declining is an answer. Running anyway with host defaults would
        // ignore it, and nothing has launched, so refusing costs nothing.
        //
        // Classified as cancelled rather than invalid: the request was fine
        // and a person chose to stop. That also keeps it distinguishable from
        // a plan that simply failed to converge, which refuses for a different
        // reason and deserves a different answer.
        ElicitAction::Decline | ElicitAction::Cancel => Err(AgentError::new(
            ErrorKind::Cancelled,
            "the request for missing values was declined",
            FailurePhase::Validation,
            EffectState::None,
        )),
        ElicitAction::Accept => Ok(result
            .content
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(id, value)| {
                let value = match value {
                    ElicitFieldValue::String(value) => value,
                    // The form only ever asks for strings, so anything else is
                    // a client answering a question that was not posed.
                    _ => return None,
                };
                Some(Answer { id, value })
            })
            .collect()),
        // The enum is non-exhaustive. An action this build does not recognize
        // is not consent, so it refuses rather than assuming acceptance.
        _ => Err(AgentError::unsupported(
            "the client answered with an elicitation action this adapter does not understand",
        )),
    }
}

async fn execute<S>(
    state: PlanToolState<S>,
    context: Context,
    ready: ReadyTurn,
    scope: Scope,
) -> CallToolResult
where
    S: Service<AgentRequest<ReadyTurn>, Response = TurnOutcome, Error = AgentError>
        + Clone
        + Send
        + Sync
        + 'static,
    S::Future: Send,
{
    let cancellation = CancellationToken::new();
    let progress = ProgressEvents::new(context.clone().into_inner())
        .with_provider_messages(state.projection.provider_messages());
    let call = CallContext::new()
        .with_cancellation(cancellation.clone())
        .with_events(EventObserver::new(progress));
    let operation_id = call.operation_id();

    let result = crate::tool::run_until_settled(
        state
            .service
            .clone()
            .oneshot(AgentRequest::with_context(ready, call)),
        context.cancellation_token(),
        cancellation,
    )
    .await;

    match result {
        Ok(outcome) => {
            let continuation = mint(&state, outcome.session.as_ref(), &scope).await;
            let structured =
                state
                    .projection
                    .outcome(&outcome, operation_id, continuation.as_ref());
            let mut response = CallToolResult::text(outcome.output.clone());
            response.structured_content = Some(structured);
            response
        }
        Err(error) => {
            let session = error
                .evidence
                .as_deref()
                .and_then(|evidence| evidence.session.clone());
            let continuation = mint(&state, session.as_ref(), &scope).await;
            let structured = state
                .projection
                .failure(&error, operation_id, continuation.as_ref());
            let mut response = CallToolResult::error(state.projection.message(&error));
            response.structured_content = Some(structured);
            response
        }
    }
}

async fn resolve_continuation<S>(
    state: &PlanToolState<S>,
    raw: &str,
    scope: &Scope,
) -> Result<SessionHandle, AgentError> {
    let id = ContinuationId::parse(raw)
        .map_err(|error| AgentError::invalid_request(error.to_string()))?;

    match state.store.resolve(id, scope.clone()).await {
        Ok(Some(session)) => Ok(session),
        Ok(None) => Err(AgentError::invalid_request(
            "continuation is unknown or does not belong to this caller",
        )),
        Err(error) => Err(AgentError::new(
            ErrorKind::Internal,
            format!("continuation store failed: {error}"),
            FailurePhase::Validation,
            EffectState::None,
        )),
    }
}

async fn mint<S>(
    state: &PlanToolState<S>,
    session: Option<&SessionHandle>,
    scope: &Scope,
) -> Option<ContinuationId> {
    let session = session?;
    state.store.mint(session.clone(), scope.clone()).await.ok()
}
