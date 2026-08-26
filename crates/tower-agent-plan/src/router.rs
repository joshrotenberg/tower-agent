//! Dispatch of provider-committed turns to configured provider services.

use std::task::{Context, Poll};

use tower::Service;
use tower::util::ServiceExt;
use tower_agent::{
    AgentError, AgentRequest, BoxTurnService, EffectState, ErrorKind, FailurePhase, TurnOutcome,
};

use crate::provider::ProviderId;
use crate::ready::ReadyTurn;

/// A finite-turn service over provider-committed turns.
///
/// The router holds one configured service per provider and dispatches a
/// [`ReadyTurn`] to the service matching its committed provider. Each inner
/// service is an ordinary Tower stack, so per-provider policy such as
/// authority, admission, and deadlines is composed by the host before
/// registration.
///
/// The router never retries and never falls back to another provider. A
/// failure from the selected service is returned unchanged, so a failure
/// whose effect state is [`EffectState::Possible`] can never be replayed
/// against a different provider. Provider selection happens during planning
/// for a fresh turn and is pinned by the session handle for a resumed one.
///
/// Cloning shares the registered services.
///
/// # Example
///
/// ```
/// # #[cfg(feature = "codex")]
/// # {
/// use tower_agent_plan::RoutedTurnService;
/// use tower_agent_codex::CodexService;
///
/// let router = RoutedTurnService::new().with_codex(CodexService::new());
/// # let _ = router;
/// # }
/// ```
#[derive(Clone, Default)]
pub struct RoutedTurnService {
    #[cfg(feature = "claude")]
    claude: Option<BoxTurnService<tower_agent_claude::ClaudeOptions>>,
    #[cfg(feature = "codex")]
    codex: Option<BoxTurnService<tower_agent_codex::CodexOptions>>,
}

impl RoutedTurnService {
    /// Create a router with no registered providers.
    ///
    /// Every provider is refused until it is registered.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether a provider has a registered service.
    pub fn handles(&self, provider: ProviderId) -> bool {
        match provider {
            #[cfg(feature = "claude")]
            ProviderId::Claude => self.claude.is_some(),
            #[cfg(feature = "codex")]
            ProviderId::Codex => self.codex.is_some(),
            #[allow(unreachable_patterns)]
            _ => false,
        }
    }

    /// Register the service that handles Claude turns.
    #[cfg(feature = "claude")]
    pub fn with_claude<S>(mut self, service: S) -> Self
    where
        S: Service<
                AgentRequest<tower_agent::Turn<tower_agent_claude::ClaudeOptions>>,
                Response = TurnOutcome,
                Error = AgentError,
            > + Clone
            + Send
            + Sync
            + 'static,
        S::Future: Send + 'static,
    {
        self.claude = Some(BoxTurnService::new(service));
        self
    }

    /// Register the service that handles Codex turns.
    #[cfg(feature = "codex")]
    pub fn with_codex<S>(mut self, service: S) -> Self
    where
        S: Service<
                AgentRequest<tower_agent::Turn<tower_agent_codex::CodexOptions>>,
                Response = TurnOutcome,
                Error = AgentError,
            > + Clone
            + Send
            + Sync
            + 'static,
        S::Future: Send + 'static,
    {
        self.codex = Some(BoxTurnService::new(service));
        self
    }
}

impl std::fmt::Debug for RoutedTurnService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut registered: Vec<&str> = Vec::new();
        #[cfg(feature = "claude")]
        if self.claude.is_some() {
            registered.push(ProviderId::Claude.as_str());
        }
        #[cfg(feature = "codex")]
        if self.codex.is_some() {
            registered.push(ProviderId::Codex.as_str());
        }
        formatter
            .debug_struct("RoutedTurnService")
            .field("registered", &registered)
            .finish()
    }
}

impl Service<AgentRequest<ReadyTurn>> for RoutedTurnService {
    type Response = TurnOutcome;
    type Error = AgentError;
    type Future = std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<TurnOutcome, AgentError>> + Send + 'static>,
    >;

    /// Always ready.
    ///
    /// The registered services are polled for readiness inside the returned
    /// future, because the provider is not known until the request arrives.
    /// A host that wants waiting backpressure composes it inside a
    /// registered service.
    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: AgentRequest<ReadyTurn>) -> Self::Future {
        let provider = request.body.provider();
        if let Err(error) = pinned_session_matches(&request.body) {
            return Box::pin(async move { Err(error) });
        }

        match request.body {
            #[cfg(feature = "claude")]
            ReadyTurn::Claude(turn) => {
                let Some(service) = self.claude.clone() else {
                    return Box::pin(async move { Err(unregistered(provider)) });
                };
                let request = AgentRequest::with_context(turn, request.context);
                Box::pin(async move { service.oneshot(request).await })
            }
            #[cfg(feature = "codex")]
            ReadyTurn::Codex(turn) => {
                let Some(service) = self.codex.clone() else {
                    return Box::pin(async move { Err(unregistered(provider)) });
                };
                let request = AgentRequest::with_context(turn, request.context);
                Box::pin(async move { service.oneshot(request).await })
            }
            #[allow(unreachable_patterns)]
            _ => Box::pin(async move { Err(unregistered(provider)) }),
        }
    }
}

/// Refuse a resumed turn whose session handle disagrees with its committed
/// provider.
///
/// Resolution already checks this for planned turns and the adapters check
/// it again at launch. The router repeats it because a host may construct a
/// [`ReadyTurn`] directly, and a resumed turn must never reach a provider
/// that did not mint its session.
fn pinned_session_matches(ready: &ReadyTurn) -> Result<(), AgentError> {
    let provider = ready.provider();
    let session = match ready {
        #[cfg(feature = "claude")]
        ReadyTurn::Claude(turn) => turn.session.as_ref(),
        #[cfg(feature = "codex")]
        ReadyTurn::Codex(turn) => turn.session.as_ref(),
        #[allow(unreachable_patterns)]
        _ => None,
    };
    match session {
        Some(session) if session.provider() != provider.as_str() => Err(AgentError::new(
            ErrorKind::Unsupported,
            format!(
                "cannot route a {} session to the {provider} provider",
                session.provider()
            ),
            FailurePhase::Validation,
            EffectState::None,
        )),
        _ => Ok(()),
    }
}

fn unregistered(provider: ProviderId) -> AgentError {
    AgentError::new(
        ErrorKind::Unsupported,
        format!("no service is registered for provider {provider}"),
        FailurePhase::Validation,
        EffectState::None,
    )
}
