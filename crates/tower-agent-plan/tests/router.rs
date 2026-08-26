//! Routing of provider-committed turns.
//!
//! The fakes below stand in for configured provider stacks so dispatch,
//! pinning, and failure passthrough are observable without launching a CLI.

#![cfg(all(feature = "claude", feature = "codex"))]

use std::future::{Ready, ready};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};

use tower::{Service, ServiceExt};
use tower_agent::{
    AgentError, AgentRequest, EffectState, ErrorKind, FailurePhase, SessionHandle, Turn,
    TurnOutcome,
};
use tower_agent_claude::ClaudeOptions;
use tower_agent_codex::CodexOptions;
use tower_agent_plan::{ProviderId, ReadyTurn, RoutedTurnService};

/// Records every call and returns a configured terminal result.
#[derive(Clone)]
struct RecordingService {
    label: &'static str,
    calls: Arc<AtomicUsize>,
    outcome: Result<&'static str, ErrorKind>,
}

impl RecordingService {
    fn ok(label: &'static str) -> Self {
        Self {
            label,
            calls: Arc::new(AtomicUsize::new(0)),
            outcome: Ok(label),
        }
    }

    fn failing(label: &'static str, kind: ErrorKind) -> Self {
        Self {
            label,
            calls: Arc::new(AtomicUsize::new(0)),
            outcome: Err(kind),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }
}

impl<O> Service<AgentRequest<Turn<O>>> for RecordingService {
    type Response = TurnOutcome;
    type Error = AgentError;
    type Future = Ready<Result<TurnOutcome, AgentError>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _request: AgentRequest<Turn<O>>) -> Self::Future {
        self.calls.fetch_add(1, Ordering::Relaxed);
        ready(match self.outcome {
            Ok(output) => Ok(TurnOutcome::new(output)),
            Err(kind) => Err(AgentError::new(
                kind,
                format!("{} failed", self.label),
                FailurePhase::Running,
                EffectState::Possible,
            )),
        })
    }
}

fn claude_ready() -> ReadyTurn {
    ReadyTurn::Claude(Turn::new("inspect this repository").with_options(ClaudeOptions::default()))
}

fn codex_ready() -> ReadyTurn {
    ReadyTurn::Codex(Turn::new("inspect this repository").with_options(CodexOptions::default()))
}

#[tokio::test]
async fn fresh_turns_reach_the_service_for_their_provider() {
    let claude = RecordingService::ok("claude");
    let codex = RecordingService::ok("codex");
    let router = RoutedTurnService::new()
        .with_claude(claude.clone())
        .with_codex(codex.clone());

    let outcome = router
        .clone()
        .oneshot(AgentRequest::new(claude_ready()))
        .await
        .expect("claude turn succeeds");
    assert_eq!(outcome.output, "claude");
    assert_eq!(claude.calls(), 1);
    assert_eq!(codex.calls(), 0);

    let outcome = router
        .oneshot(AgentRequest::new(codex_ready()))
        .await
        .expect("codex turn succeeds");
    assert_eq!(outcome.output, "codex");
    assert_eq!(claude.calls(), 1);
    assert_eq!(codex.calls(), 1);
}

#[tokio::test]
async fn resumed_turns_stay_pinned_to_their_session_provider() {
    let claude = RecordingService::ok("claude");
    let codex = RecordingService::ok("codex");
    let router = RoutedTurnService::new()
        .with_claude(claude.clone())
        .with_codex(codex.clone());

    let mismatched = ReadyTurn::Codex(
        Turn::new("continue the review")
            .with_options(CodexOptions::default())
            .resume(SessionHandle::new("claude", "abc")),
    );
    let error = router
        .oneshot(AgentRequest::new(mismatched))
        .await
        .expect_err("a claude session cannot route to codex");
    assert_eq!(error.kind, ErrorKind::Unsupported);
    assert_eq!(error.phase, FailurePhase::Validation);
    assert_eq!(error.effects, EffectState::None);
    assert_eq!(claude.calls(), 0);
    assert_eq!(codex.calls(), 0);
}

#[tokio::test]
async fn matching_resumed_turns_dispatch_normally() {
    let codex = RecordingService::ok("codex");
    let router = RoutedTurnService::new().with_codex(codex.clone());

    let resumed = ReadyTurn::Codex(
        Turn::new("continue the review")
            .with_options(CodexOptions::default())
            .resume(SessionHandle::new("codex", "abc")),
    );
    router
        .oneshot(AgentRequest::new(resumed))
        .await
        .expect("matching session dispatches");
    assert_eq!(codex.calls(), 1);
}

#[tokio::test]
async fn unregistered_providers_are_refused_before_dispatch() {
    let codex = RecordingService::ok("codex");
    let router = RoutedTurnService::new().with_codex(codex.clone());
    assert!(router.handles(ProviderId::Codex));
    assert!(!router.handles(ProviderId::Claude));

    let error = router
        .oneshot(AgentRequest::new(claude_ready()))
        .await
        .expect_err("claude is unregistered");
    assert_eq!(error.kind, ErrorKind::Unsupported);
    assert_eq!(error.phase, FailurePhase::Validation);
    assert_eq!(error.effects, EffectState::None);
    assert_eq!(codex.calls(), 0);
}

#[tokio::test]
async fn effectful_failures_never_reach_another_provider() {
    let claude = RecordingService::ok("claude");
    let codex = RecordingService::failing("codex", ErrorKind::Provider);
    let router = RoutedTurnService::new()
        .with_claude(claude.clone())
        .with_codex(codex.clone());

    let error = router
        .oneshot(AgentRequest::new(codex_ready()))
        .await
        .expect_err("codex fails");
    assert_eq!(error.kind, ErrorKind::Provider);
    assert_eq!(error.effects, EffectState::Possible);
    assert_eq!(codex.calls(), 1);
    assert_eq!(claude.calls(), 0);
}

#[tokio::test]
async fn clones_share_registered_services() {
    let codex = RecordingService::ok("codex");
    let router = RoutedTurnService::new().with_codex(codex.clone());

    for _ in 0..3 {
        router
            .clone()
            .oneshot(AgentRequest::new(codex_ready()))
            .await
            .expect("clone dispatches");
    }
    assert_eq!(codex.calls(), 3);
}

#[tokio::test]
async fn planned_turns_route_end_to_end() {
    use tower_agent_plan::{Layers, PartialTurn, Prepared, prepare};

    let codex = RecordingService::ok("codex");
    let router = RoutedTurnService::new().with_codex(codex.clone());

    let explicit = PartialTurn {
        provider: Some(ProviderId::Codex),
        prompt: Some("review the current branch".to_string()),
        ..Default::default()
    };
    let Prepared::Ready(ready) = prepare(Layers::new(&explicit)) else {
        panic!("expected a ready turn");
    };
    assert_eq!(ready.provider(), ProviderId::Codex);

    let outcome = router
        .oneshot(AgentRequest::new(ready))
        .await
        .expect("planned turn dispatches");
    assert_eq!(outcome.output, "codex");
    assert_eq!(codex.calls(), 1);
}
