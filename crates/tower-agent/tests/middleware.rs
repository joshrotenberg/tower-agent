use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{Mutex, Notify, mpsc, oneshot};
use tower::{Service, ServiceBuilder, ServiceExt, service_fn};
use tower_agent::layer::{
    AdmissionLayer, CatchPanicLayer, DeadlineLayer, ObserveLayer, ReceiptObserver, ReceiptStatus,
    SuperviseLayer, ValidateTurnLayer,
};
use tower_agent::{
    AgentError, AgentRequest, CallContext, CancellationToken, Cost, EffectState, ErrorKind,
    FailureEvidence, SessionHandle, Turn, TurnOutcome,
};

struct Harness {
    service: tower_agent::BoxTurnService,
    started: mpsc::Receiver<String>,
    settled: mpsc::Receiver<String>,
    cleanup: Arc<Notify>,
    receipts: mpsc::Receiver<tower_agent::layer::Receipt>,
}

fn cancellation_harness() -> Harness {
    let (started_tx, started) = mpsc::channel(4);
    let (settled_tx, settled) = mpsc::channel(4);
    let cleanup = Arc::new(Notify::new());
    let provider_cleanup = cleanup.clone();
    let (observer, receipts) = ReceiptObserver::channel(8);

    let provider = service_fn(move |request: AgentRequest<Turn>| {
        let started = started_tx.clone();
        let settled = settled_tx.clone();
        let cleanup = provider_cleanup.clone();
        async move {
            let prompt = request.body.prompt;
            started
                .send(prompt.clone())
                .await
                .expect("started receiver");
            if prompt == "immediate" {
                settled
                    .send(prompt.clone())
                    .await
                    .expect("settled receiver");
                return Ok::<_, AgentError>(TurnOutcome::new(prompt));
            }

            request.context.cancellation().cancelled().await;
            cleanup.notified().await;
            settled.send(prompt).await.expect("settled receiver");
            Err(AgentError::cancelled(EffectState::Possible))
        }
    });

    let service = ServiceBuilder::new()
        .layer(SuperviseLayer::new())
        .layer(ObserveLayer::new(observer))
        .layer(CatchPanicLayer::new())
        .layer(AdmissionLayer::single_flight())
        .layer(DeadlineLayer::new())
        .layer(ValidateTurnLayer::new())
        .service(provider);

    Harness {
        service: tower_agent::BoxTurnService::new(service),
        started,
        settled,
        cleanup,
        receipts,
    }
}

#[tokio::test]
async fn dropping_an_unpolled_supervisor_future_still_cancels() {
    let (settled_tx, settled_rx) = oneshot::channel();
    let settled = Arc::new(std::sync::Mutex::new(Some(settled_tx)));
    let provider_settled = settled.clone();
    let provider = service_fn(move |request: AgentRequest<Turn>| {
        let settled = provider_settled.clone();
        async move {
            request.context.cancellation().cancelled().await;
            if let Some(sender) = settled.lock().expect("settled lock").take() {
                let _ = sender.send(());
            }
            Err::<TurnOutcome, _>(AgentError::cancelled(EffectState::Possible))
        }
    });
    let mut service = ServiceBuilder::new()
        .layer(SuperviseLayer::new())
        .service(provider);
    service.ready().await.expect("service ready");
    let cancellation = CancellationToken::new();
    let request = AgentRequest::with_context(
        Turn::new("drop before poll"),
        CallContext::new().with_cancellation(cancellation.clone()),
    );

    let future = service.call(request);
    drop(future);

    cancellation.cancelled().await;
    tokio::time::timeout(Duration::from_secs(1), settled_rx)
        .await
        .expect("inner call remains supervised")
        .expect("settlement signal");
}

#[tokio::test]
async fn caller_drop_cancels_but_supervisor_retains_work_and_capacity() {
    let mut harness = cancellation_harness();
    let cancellation = CancellationToken::new();
    let request = AgentRequest::with_context(
        Turn::new("held"),
        CallContext::new().with_cancellation(cancellation.clone()),
    );
    let caller = tokio::spawn(harness.service.clone().oneshot(request));
    assert_eq!(harness.started.recv().await.as_deref(), Some("held"));

    caller.abort();
    cancellation.cancelled().await;

    let busy = harness
        .service
        .clone()
        .oneshot(AgentRequest::new(Turn::new("busy")))
        .await
        .expect_err("capacity stays occupied during cleanup");
    assert_eq!(busy.kind, ErrorKind::Busy);

    harness.cleanup.notify_waiters();
    assert_eq!(harness.settled.recv().await.as_deref(), Some("held"));

    let outcome = harness
        .service
        .clone()
        .oneshot(AgentRequest::new(Turn::new("immediate")))
        .await
        .expect("capacity reopens after cleanup");
    assert_eq!(outcome.output, "immediate");

    let mut statuses = Vec::new();
    while statuses.len() < 3 {
        statuses.push(
            harness
                .receipts
                .recv()
                .await
                .expect("terminal receipt")
                .status,
        );
    }
    assert!(statuses.contains(&ReceiptStatus::Failed {
        kind: ErrorKind::Cancelled,
        phase: tower_agent::FailurePhase::Running,
        effects: EffectState::Possible,
    }));
    assert!(statuses.contains(&ReceiptStatus::Failed {
        kind: ErrorKind::Busy,
        phase: tower_agent::FailurePhase::Admission,
        effects: EffectState::None,
    }));
    assert!(statuses.contains(&ReceiptStatus::Succeeded));
}

#[tokio::test]
async fn deadline_cancels_and_drains_before_returning() {
    let mut harness = cancellation_harness();
    let cancellation = CancellationToken::new();
    let request = AgentRequest::with_context(
        Turn::new("deadline"),
        CallContext::new()
            .with_cancellation(cancellation.clone())
            .with_deadline(Instant::now() + Duration::from_millis(20)),
    );
    let caller = tokio::spawn(harness.service.clone().oneshot(request));
    assert_eq!(harness.started.recv().await.as_deref(), Some("deadline"));

    tokio::time::timeout(Duration::from_secs(1), cancellation.cancelled())
        .await
        .expect("deadline signals cancellation");
    assert!(!caller.is_finished(), "deadline waits for provider cleanup");

    let busy = harness
        .service
        .clone()
        .oneshot(AgentRequest::new(Turn::new("busy")))
        .await
        .expect_err("capacity remains occupied while deadline drains");
    assert_eq!(busy.kind, ErrorKind::Busy);

    harness.cleanup.notify_waiters();
    let error = caller
        .await
        .expect("caller task joins")
        .expect_err("deadline wins over provider cancellation result");
    assert_eq!(error.kind, ErrorKind::DeadlineExceeded);
    assert_eq!(error.effects, EffectState::Possible);
    assert_eq!(
        error.cause.as_ref().map(|cause| cause.kind),
        Some(ErrorKind::Cancelled)
    );

    let receipts = Arc::new(Mutex::new(Vec::new()));
    while receipts.lock().await.len() < 2 {
        let receipt = harness.receipts.recv().await.expect("receipt");
        receipts.lock().await.push(receipt.status);
    }
    let receipts = receipts.lock().await;
    assert!(receipts.contains(&ReceiptStatus::Failed {
        kind: ErrorKind::DeadlineExceeded,
        phase: tower_agent::FailurePhase::Running,
        effects: EffectState::Possible,
    }));
}

#[tokio::test]
async fn explicit_cancellation_cancels_and_drains_before_returning() {
    let mut harness = cancellation_harness();
    let cancellation = CancellationToken::new();
    let request = AgentRequest::with_context(
        Turn::new("cancelled"),
        CallContext::new().with_cancellation(cancellation.clone()),
    );
    let call = tokio::spawn(harness.service.clone().oneshot(request));
    assert_eq!(harness.started.recv().await.as_deref(), Some("cancelled"));

    cancellation.cancel();
    tokio::task::yield_now().await;
    assert!(!call.is_finished(), "cancellation waits for settlement");

    let busy = harness
        .service
        .clone()
        .oneshot(AgentRequest::new(Turn::new("busy")))
        .await
        .expect_err("capacity stays occupied during cancellation cleanup");
    assert_eq!(busy.kind, ErrorKind::Busy);

    harness.cleanup.notify_waiters();
    assert_eq!(harness.settled.recv().await.as_deref(), Some("cancelled"));
    let error = call
        .await
        .expect("call joins")
        .expect_err("explicit cancellation is terminal");
    assert_eq!(error.kind, ErrorKind::Cancelled);
    assert_eq!(
        error.cause.as_deref().map(|cause| cause.kind),
        Some(ErrorKind::Cancelled)
    );

    let outcome = harness
        .service
        .oneshot(AgentRequest::new(Turn::new("immediate")))
        .await
        .expect("capacity releases after settlement");
    assert_eq!(outcome.output, "immediate");
}

#[tokio::test]
async fn panicking_provider_becomes_typed_internal_failure_and_releases_capacity() {
    let (first_started_tx, first_started_rx) = oneshot::channel();
    let first_started = Arc::new(std::sync::Mutex::new(Some(first_started_tx)));
    let provider_started = first_started.clone();
    let provider = service_fn(move |request: AgentRequest<Turn>| {
        let started = provider_started.clone();
        async move {
            if request.body.prompt == "panic" {
                if let Some(sender) = started.lock().expect("started lock").take() {
                    let _ = sender.send(());
                }
                panic!("provider panic");
            }
            Ok::<_, AgentError>(TurnOutcome::new(request.body.prompt))
        }
    });
    let (observer, mut receipts) = ReceiptObserver::channel(1);
    let service = ServiceBuilder::new()
        .layer(SuperviseLayer::new())
        .layer(ObserveLayer::new(observer))
        .layer(CatchPanicLayer::new())
        .layer(AdmissionLayer::single_flight())
        .service(provider);
    let service = tower_agent::BoxTurnService::new(service);

    let operation_id = tower_agent::OperationId::from_u64(4242);
    let error = service
        .clone()
        .oneshot(AgentRequest::with_context(
            Turn::new("panic"),
            CallContext::new().with_operation_id(operation_id),
        ))
        .await
        .expect_err("panic is normalized");
    first_started_rx.await.expect("panic call started");
    assert_eq!(error.kind, ErrorKind::Internal);
    let receipt = receipts.recv().await.expect("panic receipt");
    // Operation identity must survive panic normalization, not just the
    // typed status.
    assert_eq!(receipt.operation_id, operation_id);
    assert_eq!(
        receipt.status,
        ReceiptStatus::Failed {
            kind: ErrorKind::Internal,
            phase: tower_agent::FailurePhase::Settlement,
            effects: EffectState::Possible,
        }
    );

    let outcome = service
        .oneshot(AgentRequest::new(Turn::new("after")))
        .await
        .expect("capacity is released after panic");
    assert_eq!(outcome.output, "after");
}

#[tokio::test]
async fn expired_deadline_does_not_strand_admission_capacity() {
    let provider = service_fn(|request: AgentRequest<Turn>| async move {
        Ok::<_, AgentError>(TurnOutcome::new(request.body.prompt))
    });
    let service = ServiceBuilder::new()
        .layer(AdmissionLayer::single_flight())
        .layer(DeadlineLayer::new())
        .service(provider);
    let mut first = service.clone();
    first.ready().await.expect("first clone ready");
    let expired = AgentRequest::with_context(
        Turn::new("expired"),
        CallContext::new().with_deadline(Instant::now() - Duration::from_millis(1)),
    );

    let error = first.call(expired).await.expect_err("deadline is expired");
    assert_eq!(error.kind, ErrorKind::DeadlineExceeded);
    assert_eq!(error.effects, EffectState::None);

    let outcome = service
        .oneshot(AgentRequest::new(Turn::new("after")))
        .await
        .expect("admission permit was released");
    assert_eq!(outcome.output, "after");
}

#[tokio::test]
async fn deadline_preserves_stronger_settlement_evidence() {
    let provider = service_fn(|request: AgentRequest<Turn>| async move {
        request.context.cancellation().cancelled().await;
        Err::<TurnOutcome, _>(
            AgentError::new(
                ErrorKind::Provider,
                "provider reported partial writes during cleanup",
                tower_agent::FailurePhase::Settlement,
                EffectState::Reported,
            )
            .with_evidence(FailureEvidence {
                session: Some(SessionHandle::new("fake", "private-session")),
                cost: Some(Cost::usd(0.25)),
                provider_turns: Some(3),
                ..FailureEvidence::default()
            }),
        )
    });
    let service = ServiceBuilder::new()
        .layer(SuperviseLayer::new())
        .layer(AdmissionLayer::single_flight())
        .layer(DeadlineLayer::new())
        .service(provider);
    let request = AgentRequest::with_context(
        Turn::new("deadline"),
        CallContext::new().with_deadline(Instant::now() + Duration::from_millis(10)),
    );

    let error = service
        .oneshot(request)
        .await
        .expect_err("deadline remains the primary failure");
    let evidence = error.evidence.as_deref().expect("settlement evidence");
    assert_eq!(error.kind, ErrorKind::DeadlineExceeded);
    assert_eq!(error.effects, EffectState::Reported);
    assert_eq!(evidence.cost, Some(Cost::usd(0.25)));
    assert_eq!(evidence.provider_turns, Some(3));
    assert_eq!(
        evidence.session.as_ref().map(SessionHandle::value),
        Some("private-session")
    );
    let cause = error.cause.expect("settlement cause is retained");
    assert_eq!(cause.kind, ErrorKind::Provider);
    assert_eq!(cause.phase, tower_agent::FailurePhase::Settlement);
    assert_eq!(cause.effects, EffectState::Reported);
}

/// A provider that completes successfully after observing cancellation, the
/// case where a turn finishes while racing its own deadline.
fn late_success_service(
    outcome: TurnOutcome,
) -> impl Service<AgentRequest<Turn>, Response = TurnOutcome, Error = AgentError, Future: Send + 'static>
+ Clone {
    let outcome = Arc::new(outcome);
    service_fn(move |request: AgentRequest<Turn>| {
        let outcome = outcome.clone();
        async move {
            request.context.cancellation().cancelled().await;
            Ok::<_, AgentError>((*outcome).clone())
        }
    })
}

fn settled_outcome() -> TurnOutcome {
    TurnOutcome {
        session: Some(SessionHandle::new("fake", "resume-me")),
        usage: Some(tower_agent::TokenUsage {
            input: Some(41_000),
            output: Some(2_300),
            ..tower_agent::TokenUsage::default()
        }),
        cost: Some(Cost::usd(0.19)),
        duration: Some(Duration::from_secs(31)),
        provider_turns: Some(4),
        ..TurnOutcome::new("finished just too late")
    }
}

#[tokio::test]
async fn a_deadline_retains_evidence_from_a_successful_late_settlement() {
    let service = ServiceBuilder::new()
        .layer(DeadlineLayer::new())
        .service(late_success_service(settled_outcome()));

    let request = AgentRequest::with_context(
        Turn::new("late"),
        CallContext::new().with_deadline(Instant::now() + Duration::from_millis(20)),
    );
    let error = service
        .oneshot(request)
        .await
        .expect_err("the deadline elapsed");

    assert_eq!(error.kind, ErrorKind::DeadlineExceeded);
    // The turn ran to completion, which is stronger than "effects possible".
    assert_eq!(error.effects, EffectState::Reported);

    let evidence = error.evidence.as_deref().expect("settlement evidence");
    let expected = settled_outcome();
    assert_eq!(evidence.session, expected.session);
    assert_eq!(evidence.usage, expected.usage);
    assert_eq!(evidence.cost, expected.cost);
    assert_eq!(evidence.duration, expected.duration);
    assert_eq!(evidence.provider_turns, expected.provider_turns);
}

#[tokio::test]
async fn explicit_cancellation_retains_evidence_from_a_successful_late_settlement() {
    let cancellation = CancellationToken::new();
    let entered = Arc::new(Notify::new());
    let provider_entered = entered.clone();
    let outcome = settled_outcome();
    let service = ServiceBuilder::new()
        .layer(DeadlineLayer::new())
        .service(service_fn(move |request: AgentRequest<Turn>| {
            let entered = provider_entered.clone();
            let outcome = outcome.clone();
            async move {
                entered.notify_one();
                request.context.cancellation().cancelled().await;
                Ok::<_, AgentError>(outcome)
            }
        }));

    let request = AgentRequest::with_context(
        Turn::new("late"),
        CallContext::new().with_cancellation(cancellation.clone()),
    );
    let call = tokio::spawn(service.oneshot(request));
    // Cancel only once the provider is genuinely in flight, so this exercises
    // the drain path rather than the pre-launch rejection.
    entered.notified().await;
    cancellation.cancel();
    let error = call
        .await
        .expect("call task")
        .expect_err("the call was cancelled");

    assert_eq!(error.kind, ErrorKind::Cancelled);
    assert_eq!(error.effects, EffectState::Reported);
    assert_eq!(
        error
            .evidence
            .as_deref()
            .and_then(|evidence| evidence.cost.clone()),
        Some(Cost::usd(0.19))
    );
}

#[tokio::test]
async fn a_late_settlement_never_overwrites_evidence_the_outer_error_already_carries() {
    let service = ServiceBuilder::new()
        .layer(DeadlineLayer::new())
        .service(late_success_service(settled_outcome()));

    let request = AgentRequest::with_context(
        Turn::new("late"),
        CallContext::new().with_deadline(Instant::now() + Duration::from_millis(20)),
    );
    let error = service
        .oneshot(request)
        .await
        .expect_err("the deadline elapsed");
    let evidence = error.evidence.as_deref().expect("settlement evidence");

    // Absent fields are filled from the settlement; nothing is synthesized.
    assert!(evidence.usage.is_some());
    assert_eq!(
        evidence.usage.and_then(tower_agent::TokenUsage::total),
        Some(43_300)
    );
}

#[tokio::test]
async fn a_request_cancelled_before_launch_is_rejected_at_admission() {
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let service = ServiceBuilder::new()
        .layer(DeadlineLayer::new())
        .service(service_fn(|_: AgentRequest<Turn>| async {
            panic!("the provider must never be called");
            #[allow(unreachable_code)]
            Ok::<TurnOutcome, AgentError>(TurnOutcome::new(""))
        }));

    let request = AgentRequest::with_context(
        Turn::new("never runs"),
        CallContext::new().with_cancellation(cancellation),
    );
    let error = service.oneshot(request).await.expect_err("pre-cancelled");

    assert_eq!(error.kind, ErrorKind::Cancelled);
    // Nothing was launched, so the phase matches the elapsed-deadline
    // rejection rather than claiming the provider was reached.
    assert_eq!(error.phase, tower_agent::FailurePhase::Admission);
    assert_eq!(error.effects, EffectState::None);
}
