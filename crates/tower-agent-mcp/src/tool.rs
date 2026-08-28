use std::sync::Arc;

use serde::Deserialize;
use tower::ServiceExt;
use tower_agent::{
    AgentError, AgentRequest, BoxTurnService, CallContext, CancellationToken, EffectState,
    ErrorKind, FailurePhase, SessionHandle, Turn,
};
use tower_mcp::extract::{Context, Json, State};
use tower_mcp::{CallToolResult, Error, RequestContext, Tool, ToolBuilder};

use crate::{ContinuationId, ContinuationStore, Projection, Scope};

/// Why a request could not be given a continuation scope.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("no continuation scope for this request: {0}")]
pub struct ScopeUnavailable(pub String);

/// Decides which [`Scope`] a request's continuations belong to.
///
/// There is no default. A continuation is a capability over conversation
/// history, and the scope is what stops one caller spending another's, so an
/// adapter that guessed would be guessing about that. Naming a source is
/// therefore part of constructing the tool.
///
/// For a transport that carries one client per process, such as stdio,
/// [`FixedScope`] is correct. For a transport serving many clients it is not,
/// and a host supplies a source that reads whatever identifies them: an
/// authenticated subject where `tower-mcp` has bridged OAuth claims into the
/// request extensions, or a transport session identifier.
pub trait ScopeSource: Send + Sync + 'static {
    /// The scope for this request, or why there is none.
    fn scope(&self, context: &RequestContext) -> Result<Scope, ScopeUnavailable>;
}

impl<F> ScopeSource for F
where
    F: Fn(&RequestContext) -> Result<Scope, ScopeUnavailable> + Send + Sync + 'static,
{
    fn scope(&self, context: &RequestContext) -> Result<Scope, ScopeUnavailable> {
        self(context)
    }
}

/// One scope for every request.
///
/// Correct only where the transport carries a single client for the lifetime
/// of the process, which in practice means stdio. On a transport serving more
/// than one client this places every caller in one scope, and the scope check
/// then permits any of them to continue any other's conversation.
#[derive(Clone, Debug)]
pub struct FixedScope(Scope);

impl FixedScope {
    /// Place every request in `scope`.
    #[must_use]
    pub fn new(scope: Scope) -> Self {
        Self(scope)
    }

    /// The conventional single-client scope for a stdio server.
    #[must_use]
    pub fn stdio() -> Self {
        Self(Scope::session("stdio"))
    }
}

impl ScopeSource for FixedScope {
    fn scope(&self, _context: &RequestContext) -> Result<Scope, ScopeUnavailable> {
        Ok(self.0.clone())
    }
}

#[derive(Deserialize, tower_mcp::schemars::JsonSchema)]
#[schemars(crate = "tower_mcp::schemars")]
struct TurnInput {
    /// The prompt for this turn.
    prompt: String,
    /// A continuation identifier from an earlier result, to continue that
    /// conversation instead of starting one.
    #[serde(default)]
    continuation: Option<String>,
}

#[derive(Clone)]
struct TurnToolState {
    service: BoxTurnService,
    store: Arc<dyn ContinuationStore>,
    scopes: Arc<dyn ScopeSource>,
    projection: Projection,
}

/// One finite agent turn, exposed as an MCP tool.
///
/// The tool runs the service it is given and projects the terminal result. It
/// adds no execution policy: layers, deadlines, and provider selection are
/// composed into the service before it arrives here.
///
/// Continuation is the part that is not a projection. A settled turn's session
/// is recorded and named, and a later call carrying that name resumes the
/// conversation, subject to the scope check.
pub struct TurnTool {
    state: TurnToolState,
    name: String,
    description: String,
}

impl TurnTool {
    /// A tool over `service`, naming continuations in `store` under the scope
    /// `scopes` derives.
    #[must_use]
    pub fn new(
        service: BoxTurnService,
        store: Arc<dyn ContinuationStore>,
        scopes: Arc<dyn ScopeSource>,
    ) -> Self {
        Self {
            state: TurnToolState {
                service,
                store,
                scopes,
                projection: Projection::new(),
            },
            name: "prompt".to_string(),
            description: "Run one finite agent turn".to_string(),
        }
    }

    /// Use a projection other than the redacting default.
    #[must_use]
    pub fn with_projection(mut self, projection: Projection) -> Self {
        self.state.projection = projection;
        self
    }

    /// Publish the tool under a different name.
    #[must_use]
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// Publish a different description.
    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }

    /// Build the MCP tool.
    #[must_use]
    pub fn build(self) -> Tool {
        ToolBuilder::new(self.name)
            .description(self.description)
            .extractor_handler(
                self.state,
                |State(state): State<TurnToolState>,
                 context: Context,
                 Json(input): Json<TurnInput>| async move {
                    Ok::<_, Error>(run(state, context, input).await)
                },
            )
            .build()
    }
}

async fn run(state: TurnToolState, context: Context, input: TurnInput) -> CallToolResult {
    let scope = match state.scopes.scope(&context) {
        Ok(scope) => scope,
        // No scope means no safe way to name or resolve a continuation, so the
        // turn is refused rather than run without one.
        Err(unavailable) => {
            return refuse(
                &state.projection,
                AgentError::invalid_request(unavailable.to_string()),
            );
        }
    };

    let resumed = match resolve_continuation(&state, input.continuation.as_deref(), &scope).await {
        Ok(session) => session,
        Err(error) => return refuse(&state.projection, error),
    };

    let mut turn = Turn::new(input.prompt);
    if let Some(session) = resumed {
        turn = turn.resume(session);
    }

    // `tower_mcp::CancellationToken` wraps a `tokio_util` token but keeps it
    // private, so the two cannot be shared and a bridge is required.
    let cancellation = CancellationToken::new();
    let call = CallContext::new().with_cancellation(cancellation.clone());
    let operation_id = call.operation_id();
    let result = run_until_settled(
        state
            .service
            .clone()
            .oneshot(AgentRequest::with_context(turn, call)),
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
            // A failed turn can still be resumable, so evidence is consulted
            // for a session exactly as an outcome is.
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

/// Resolve a supplied continuation, or report why it cannot be honored.
///
/// An identifier that does not resolve is refused rather than quietly starting
/// a fresh conversation. The caller asked to continue something; running a new
/// turn instead would answer a different question and look like success.
async fn resolve_continuation(
    state: &TurnToolState,
    continuation: Option<&str>,
    scope: &Scope,
) -> Result<Option<SessionHandle>, AgentError> {
    let Some(raw) = continuation else {
        return Ok(None);
    };

    let id = ContinuationId::parse(raw)
        .map_err(|error| AgentError::invalid_request(error.to_string()))?;

    match state.store.resolve(id, scope.clone()).await {
        // Unknown, out of scope, and dropped are one answer here, as the store
        // intends. Reporting which would tell a caller that an identifier
        // exists somewhere it cannot reach.
        Ok(None) => Err(AgentError::invalid_request(
            "continuation is unknown or does not belong to this caller",
        )),
        Ok(Some(session)) => Ok(Some(session)),
        // The request is well formed and this host could not answer it. That
        // is a defect here rather than a caller error, and nothing launched.
        Err(error) => Err(AgentError::new(
            ErrorKind::Internal,
            format!("continuation store failed: {error}"),
            FailurePhase::Validation,
            EffectState::None,
        )),
    }
}

/// Forward an MCP cancellation into the turn, then wait for it to settle.
///
/// Cancelling signals and drains. The provider future is never dropped, so the
/// terminal result and its evidence still arrive, which is the same guarantee
/// `DeadlineLayer` gives. The cancellation branch is disabled after it fires
/// because an already-cancelled token completes immediately and would
/// otherwise spin.
async fn run_until_settled<F, T>(
    future: F,
    incoming: tower_mcp::context::CancellationToken,
    outgoing: CancellationToken,
) -> T
where
    F: std::future::Future<Output = T>,
{
    let mut future = std::pin::pin!(future);
    let mut forwarded = false;
    loop {
        tokio::select! {
            settled = &mut future => break settled,
            () = incoming.cancelled(), if !forwarded => {
                forwarded = true;
                outgoing.cancel();
            }
        }
    }
}

/// Name a session, or leave it unnamed if the store refuses.
///
/// A store failure loses resumability and does not fail the turn. The work is
/// already done, and discarding a settled result because bookkeeping failed
/// would be the worse outcome. The projection reports the session as present
/// with no continuation, which is exactly what happened.
async fn mint(
    state: &TurnToolState,
    session: Option<&SessionHandle>,
    scope: &Scope,
) -> Option<ContinuationId> {
    let session = session?;
    state.store.mint(session.clone(), scope.clone()).await.ok()
}

/// A refusal that never reached the provider.
fn refuse(projection: &Projection, error: AgentError) -> CallToolResult {
    let structured = projection.failure(&error, CallContext::new().operation_id(), None);
    let mut response = CallToolResult::error(projection.message(&error));
    response.structured_content = Some(structured);
    response
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn cancellation_is_forwarded_and_settlement_is_still_awaited() {
        let incoming = tower_mcp::context::CancellationToken::new();
        let outgoing = CancellationToken::new();
        let observed = outgoing.clone();
        let settled = Arc::new(AtomicBool::new(false));
        let reached = Arc::clone(&settled);

        // Stands in for a provider that observes cancellation and then
        // finishes, which is what the kernel's drain path produces.
        let turn = async move {
            observed.cancelled().await;
            reached.store(true, Ordering::SeqCst);
            "terminal result"
        };

        incoming.cancel();
        let outcome = tokio::time::timeout(
            Duration::from_secs(5),
            run_until_settled(turn, incoming, outgoing),
        )
        .await;

        assert_eq!(
            outcome.expect("cancellation was not forwarded, so nothing settled"),
            "terminal result"
        );
        // The future was awaited to completion rather than dropped, so its
        // terminal result and evidence still exist.
        assert!(settled.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn an_uncancelled_turn_settles_without_spinning() {
        let outcome = tokio::time::timeout(
            Duration::from_secs(5),
            run_until_settled(
                async { "terminal result" },
                tower_mcp::context::CancellationToken::new(),
                CancellationToken::new(),
            ),
        )
        .await;

        assert_eq!(outcome.expect("settled"), "terminal result");
    }
}
