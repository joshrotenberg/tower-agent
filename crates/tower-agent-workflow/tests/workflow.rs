use std::{
    convert::Infallible,
    future::{Ready, ready},
    num::NonZeroUsize,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
    time::Instant,
};

use tokio::sync::{Barrier, mpsc};
use tower::{Service, ServiceExt, service_fn};
use tower_agent::{
    AgentError, AgentEvent, AgentRequest, Cost, EffectState, ErrorKind, EventObserver,
    FailureEvidence, FailurePhase, FakeOptions, FakeService, FakeStep, SessionHandle, TokenUsage,
    Turn, TurnOutcome,
};
use tower_agent_workflow::{
    AgentStepService, BoxStepService, DagBuilder, PipelineBuilder, StepCall, StepId, StepSpec,
    WorkflowContext, WorkflowDefinition, WorkflowFailure, WorkflowRequest, WorkflowRunId,
    WorkflowService,
};

fn run_id(value: &str) -> WorkflowRunId {
    WorkflowRunId::new(value).expect("test run id is valid")
}

fn step_id(value: &str) -> StepId {
    StepId::new(value).expect("test step id is valid")
}

#[tokio::test]
async fn single_step_executes_a_typed_tower_agent_service() {
    let definition = WorkflowDefinition::single(
        "one-shot",
        "v1",
        StepSpec::new("call", "inspect the repository"),
    )
    .expect("single-step workflow is valid");
    let dispatcher = AgentStepService::new(
        tower_agent::EchoService,
        |call: StepCall<(), &'static str, TurnOutcome>| Ok(Turn::new(call.job)),
    );
    let service = WorkflowService::<_, TurnOutcome, AgentError>::new(dispatcher);

    let outcome = service
        .oneshot(WorkflowRequest::new(
            WorkflowContext::new(run_id("run-one")),
            definition,
            (),
        ))
        .await
        .expect("workflow succeeds");

    assert_eq!(
        outcome.outputs[&step_id("call")].output,
        "inspect the repository"
    );
    assert_eq!(outcome.leaf_outputs.len(), 1);
}

#[derive(Clone, Debug)]
enum TypedAgentJob {
    Echo,
    Fake,
}

#[tokio::test]
async fn one_dispatcher_routes_distinct_typed_provider_options() {
    let definition = PipelineBuilder::new("typed-providers", "v1")
        .then(StepSpec::new("echo", TypedAgentJob::Echo))
        .then(StepSpec::new("fake", TypedAgentJob::Fake))
        .build()
        .expect("pipeline is valid");
    let dispatcher = service_fn(
        |call: StepCall<&'static str, TypedAgentJob, TurnOutcome>| async move {
            match call.job {
                TypedAgentJob::Echo => {
                    tower_agent::EchoService
                        .oneshot(AgentRequest::with_context(
                            Turn::new(*call.input),
                            call.agent_context(),
                        ))
                        .await
                }
                TypedAgentJob::Fake => {
                    let prior = &call.dependencies[&step_id("echo")].output;
                    FakeService
                        .oneshot(AgentRequest::with_context(
                            Turn::new(format!("fake after {prior}")).with_options(FakeOptions {
                                simulated_tokens: Some(7),
                                ..FakeOptions::default()
                            }),
                            call.agent_context(),
                        ))
                        .await
                }
            }
        },
    );

    let outcome = WorkflowService::<_, TurnOutcome, AgentError>::new(dispatcher)
        .oneshot(WorkflowRequest::new(
            WorkflowContext::new(run_id("run-typed")),
            definition,
            "hello",
        ))
        .await
        .expect("typed provider pipeline succeeds");

    assert_eq!(outcome.outputs[&step_id("echo")].output, "hello");
    assert_eq!(
        outcome.outputs[&step_id("fake")].usage.unwrap().output,
        Some(7)
    );
}

#[derive(Clone, Debug)]
enum MetadataJob {
    Produce,
    Consume,
}

#[tokio::test]
async fn exact_turn_outcome_metadata_survives_a_pipeline_dependency() {
    let definition = PipelineBuilder::new("metadata", "v1")
        .then(StepSpec::new("produce", MetadataJob::Produce))
        .then(StepSpec::new("consume", MetadataJob::Consume))
        .build()
        .expect("pipeline is valid");
    let expected = TurnOutcome {
        output: "structured research".into(),
        // No schema was requested, so the provider returned no structured
        // payload. Added when TurnOutcome gained the field.
        structured: None,
        session: Some(SessionHandle::new("fake-metadata", "session-42")),
        usage: Some(TokenUsage {
            input: Some(11),
            cached_input: Some(4),
            cache_write_input: Some(2),
            output: Some(7),
            reasoning_output: Some(3),
            provider_total: Some(27),
        }),
        cost: Some(Cost::usd(0.21)),
        duration: Some(Duration::from_secs(9)),
        provider_turns: Some(4),
    };
    let expected_for_dispatch = expected.clone();
    let dispatcher = service_fn(move |call: StepCall<(), MetadataJob, TurnOutcome>| {
        let expected = expected_for_dispatch.clone();
        async move {
            match call.job {
                MetadataJob::Produce => {
                    FakeService
                        .oneshot(AgentRequest::with_context(
                            Turn::new("produce metadata")
                                .with_options(FakeOptions::succeed(expected)),
                            call.agent_context(),
                        ))
                        .await
                }
                MetadataJob::Consume => {
                    assert_eq!(call.dependencies.len(), 1);
                    assert_eq!(call.dependencies[&step_id("produce")].as_ref(), &expected);
                    FakeService
                        .oneshot(AgentRequest::with_context(
                            Turn::new("metadata observed").with_options(FakeOptions::succeed(
                                TurnOutcome::new("metadata observed"),
                            )),
                            call.agent_context(),
                        ))
                        .await
                }
            }
        }
    });

    let outcome = WorkflowService::<_, TurnOutcome, AgentError>::new(dispatcher)
        .oneshot(WorkflowRequest::new(
            WorkflowContext::new(run_id("run-metadata")),
            definition,
            (),
        ))
        .await
        .expect("metadata pipeline succeeds");

    assert_eq!(outcome.outputs[&step_id("produce")].as_ref(), &expected);
    assert_eq!(
        outcome.outputs[&step_id("consume")].output,
        "metadata observed"
    );
}

#[derive(Clone, Debug)]
enum NamedSessionJob {
    Preassign,
    Resume,
    WrongProvider,
}

#[tokio::test]
async fn named_fake_threads_a_preassigned_session_and_rejects_wrong_provider_routing() {
    let definition = PipelineBuilder::new("named-session", "v1")
        .then(StepSpec::new("preassign", NamedSessionJob::Preassign))
        .then(StepSpec::new("resume", NamedSessionJob::Resume))
        .then(StepSpec::new(
            "wrong-provider",
            NamedSessionJob::WrongProvider,
        ))
        .build()
        .expect("pipeline is valid");
    let reserved = SessionHandle::new("fake-alpha", "host-reserved");
    let reserved_for_dispatch = reserved.clone();
    let alpha = FakeService::named("fake-alpha");
    let beta = FakeService::named("fake-beta");
    let dispatcher = service_fn(move |call: StepCall<(), NamedSessionJob, TurnOutcome>| {
        let reserved = reserved_for_dispatch.clone();
        let alpha = alpha.clone();
        let beta = beta.clone();
        async move {
            match call.job {
                NamedSessionJob::Preassign => {
                    alpha
                        .oneshot(AgentRequest::with_context(
                            Turn::new("reserve a session").with_options(FakeOptions::default()),
                            call.agent_context().with_preassigned_session(reserved),
                        ))
                        .await
                }
                NamedSessionJob::Resume => {
                    let session = call.dependencies[&step_id("preassign")]
                        .session
                        .clone()
                        .expect("preassigned session is returned");
                    alpha
                        .oneshot(AgentRequest::with_context(
                            Turn::new("resume reserved session")
                                .resume(session)
                                .with_options(FakeOptions::default()),
                            call.agent_context(),
                        ))
                        .await
                }
                NamedSessionJob::WrongProvider => {
                    let alpha_session = call.dependencies[&step_id("resume")]
                        .session
                        .clone()
                        .expect("resumed session is returned");
                    beta.oneshot(AgentRequest::with_context(
                        Turn::new("misrouted session")
                            .resume(alpha_session)
                            .with_options(FakeOptions::default()),
                        call.agent_context(),
                    ))
                    .await
                }
            }
        }
    });

    let failure = WorkflowService::<_, TurnOutcome, AgentError>::new(dispatcher)
        .oneshot(WorkflowRequest::new(
            WorkflowContext::new(run_id("run-named-session")),
            definition,
            (),
        ))
        .await
        .expect_err("the wrong provider must reject the alpha session");

    assert_eq!(failure.workflow_id().as_str(), "named-session");
    assert_eq!(failure.workflow_version().as_str(), "v1");
    assert_eq!(failure.run_id(), &run_id("run-named-session"));
    match failure {
        WorkflowFailure::StepsFailed {
            completed,
            failures,
            ..
        } => {
            assert_eq!(completed.len(), 2);
            assert_eq!(
                completed[&step_id("preassign")].session.as_ref(),
                Some(&reserved)
            );
            assert_eq!(
                completed[&step_id("resume")].session.as_ref(),
                Some(&reserved)
            );
            assert_eq!(failures.len(), 1);
            assert_eq!(failures[0].step_id, step_id("wrong-provider"));
            let error = &failures[0].error;
            assert_eq!(error.kind, ErrorKind::Unsupported);
            assert_eq!(error.phase, FailurePhase::Validation);
            assert_eq!(error.effects, EffectState::None);
            assert!(error.message.contains("fake-alpha"));
            assert!(error.message.contains("fake-beta"));
            assert!(error.evidence.is_none());
            assert!(error.cause.is_none());
        }
        other => panic!("unexpected failure: {other:?}"),
    }
}

#[derive(Clone, Debug)]
enum PipelineJob {
    Research,
    Implement,
    Review,
}

#[tokio::test]
async fn pipeline_passes_only_direct_dependency_outputs() {
    let definition = PipelineBuilder::new("review", "v1")
        .then(StepSpec::new("research", PipelineJob::Research))
        .then(StepSpec::new("implement", PipelineJob::Implement))
        .then(StepSpec::new("review", PipelineJob::Review))
        .build()
        .expect("pipeline is valid");
    let dispatcher = service_fn(
        |call: StepCall<&'static str, PipelineJob, String>| async move {
            let output = match call.job {
                PipelineJob::Research => {
                    assert!(call.dependencies.is_empty());
                    format!("research({})", call.input)
                }
                PipelineJob::Implement => {
                    assert_eq!(call.dependencies.len(), 1);
                    format!("implement({})", call.dependencies[&step_id("research")])
                }
                PipelineJob::Review => {
                    assert_eq!(call.dependencies.len(), 1);
                    assert!(!call.dependencies.contains_key(&step_id("research")));
                    format!("review({})", call.dependencies[&step_id("implement")])
                }
            };
            Ok::<_, Infallible>(output)
        },
    );

    let outcome = WorkflowService::<_, String, Infallible>::new(dispatcher)
        .oneshot(WorkflowRequest::new(
            WorkflowContext::new(run_id("run-pipeline")),
            definition,
            "issue-42",
        ))
        .await
        .expect("pipeline succeeds");

    assert_eq!(
        outcome.outputs[&step_id("review")].as_str(),
        "review(implement(research(issue-42)))"
    );
}

#[derive(Clone, Debug)]
enum DagJob {
    Root(&'static str),
    Join,
}

#[tokio::test]
async fn dag_runs_ready_roots_concurrently_and_joins_them() {
    let definition = DagBuilder::new("parallel-review", "v1")
        .step(StepSpec::new("architecture", DagJob::Root("architecture")))
        .step(StepSpec::new("tests", DagJob::Root("tests")))
        .step(StepSpec::new("synthesize", DagJob::Join).needs(["architecture", "tests"]))
        .build()
        .expect("dag is valid");
    let roots_started = Arc::new(Barrier::new(2));
    let dispatcher = service_fn({
        let roots_started = Arc::clone(&roots_started);
        move |call: StepCall<(), DagJob, String>| {
            let roots_started = Arc::clone(&roots_started);
            async move {
                let output = match call.job {
                    DagJob::Root(name) => {
                        assert!(call.dependencies.is_empty());
                        roots_started.wait().await;
                        name.to_string()
                    }
                    DagJob::Join => {
                        assert_eq!(
                            call.dependencies.keys().collect::<Vec<_>>(),
                            vec![&step_id("architecture"), &step_id("tests")]
                        );
                        format!(
                            "{}+{}",
                            call.dependencies[&step_id("architecture")],
                            call.dependencies[&step_id("tests")]
                        )
                    }
                };
                Ok::<_, Infallible>(output)
            }
        }
    });
    let service = WorkflowService::<_, String, Infallible>::new(dispatcher)
        .with_max_concurrency(NonZeroUsize::new(2).expect("nonzero"));

    let outcome = tokio::time::timeout(
        Duration::from_secs(2),
        service.oneshot(WorkflowRequest::new(
            WorkflowContext::new(run_id("run-dag")),
            definition,
            (),
        )),
    )
    .await
    .expect("independent roots should not deadlock")
    .expect("dag succeeds");

    assert_eq!(
        outcome.outputs[&step_id("synthesize")].as_str(),
        "architecture+tests"
    );
}

#[derive(Clone, Debug)]
enum FailureJob {
    Succeed,
    Fail,
    MustNotRun,
}

#[tokio::test]
async fn failure_is_not_retried_and_descendants_do_not_run() {
    let definition = PipelineBuilder::new("failure", "v1")
        .then(StepSpec::new("first", FailureJob::Succeed))
        .then(StepSpec::new("second", FailureJob::Fail))
        .then(StepSpec::new("third", FailureJob::MustNotRun))
        .build()
        .expect("pipeline is valid");
    let calls = Arc::new(AtomicUsize::new(0));
    let dispatcher = service_fn({
        let calls = Arc::clone(&calls);
        move |call: StepCall<(), FailureJob, String>| {
            calls.fetch_add(1, Ordering::SeqCst);
            async move {
                match call.job {
                    FailureJob::Succeed => Ok("done".to_string()),
                    FailureJob::Fail => Err(AgentError::new(
                        ErrorKind::Provider,
                        "scripted provider failure",
                        FailurePhase::Running,
                        EffectState::Possible,
                    )),
                    FailureJob::MustNotRun => panic!("descendant ran after failure"),
                }
            }
        }
    });

    let failure = WorkflowService::<_, String, AgentError>::new(dispatcher)
        .oneshot(WorkflowRequest::new(
            WorkflowContext::new(run_id("run-failure")),
            definition,
            (),
        ))
        .await
        .expect_err("workflow should fail");

    assert_eq!(calls.load(Ordering::SeqCst), 2);
    match failure {
        WorkflowFailure::StepsFailed {
            completed,
            failures,
            ..
        } => {
            assert_eq!(completed.len(), 1);
            assert_eq!(failures.len(), 1);
            assert_eq!(failures[0].step_id, step_id("second"));
            assert_eq!(failures[0].error.effects, EffectState::Possible);
        }
        other => panic!("unexpected failure: {other:?}"),
    }
}

#[tokio::test]
async fn cancellation_before_scheduling_launches_nothing() {
    let definition = WorkflowDefinition::single("pre-cancel", "v1", StepSpec::new("call", ()))
        .expect("single-step workflow is valid");
    let calls = Arc::new(AtomicUsize::new(0));
    let dispatcher = service_fn({
        let calls = Arc::clone(&calls);
        move |_call: StepCall<(), (), ()>| {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Ok::<_, Infallible>(()) }
        }
    });
    let cancellation = tower_agent::CancellationToken::new();
    cancellation.cancel();

    let failure = WorkflowService::<_, (), Infallible>::new(dispatcher)
        .oneshot(WorkflowRequest::new(
            WorkflowContext::new(run_id("run-pre-cancel")).with_cancellation(cancellation),
            definition,
            (),
        ))
        .await
        .expect_err("pre-cancelled workflow should stop");

    assert!(matches!(failure, WorkflowFailure::Cancelled { .. }));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn expired_deadline_launches_nothing() {
    let definition = WorkflowDefinition::single("expired", "v1", StepSpec::new("call", ()))
        .expect("single-step workflow is valid");
    let calls = Arc::new(AtomicUsize::new(0));
    let dispatcher = service_fn({
        let calls = Arc::clone(&calls);
        move |_call: StepCall<(), (), ()>| {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Ok::<_, Infallible>(()) }
        }
    });

    let failure = WorkflowService::<_, (), Infallible>::new(dispatcher)
        .oneshot(WorkflowRequest::new(
            WorkflowContext::new(run_id("run-expired"))
                .with_deadline(Instant::now() - Duration::from_secs(1)),
            definition,
            (),
        ))
        .await
        .expect_err("expired workflow should stop");

    assert!(matches!(failure, WorkflowFailure::DeadlineExceeded { .. }));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn cancellation_is_bridged_to_the_agent_and_awaits_settlement() {
    let definition = WorkflowDefinition::single("cancel", "v1", StepSpec::new("slow", ()))
        .expect("single-step workflow is valid");
    let cancellation = tower_agent::CancellationToken::new();
    let (observer, mut events) = EventObserver::channel(4);
    let dispatcher = service_fn(move |call: StepCall<(), (), TurnOutcome>| {
        let observer = observer.clone();
        async move {
            let context = call.agent_context().with_events(observer.clone());
            FakeService
                .oneshot(AgentRequest::with_context(
                    Turn::new("slow agent").with_options(FakeOptions {
                        delay: Some(Duration::from_secs(60)),
                        ..FakeOptions::default()
                    }),
                    context,
                ))
                .await
        }
    });
    let request = WorkflowRequest::new(
        WorkflowContext::new(run_id("run-cancel")).with_cancellation(cancellation.clone()),
        definition,
        (),
    );
    let execution = tokio::spawn(async move {
        WorkflowService::<_, TurnOutcome, AgentError>::new(dispatcher)
            .oneshot(request)
            .await
    });

    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .expect("agent should start"),
        Some(AgentEvent::Started)
    );
    cancellation.cancel();
    let failure = tokio::time::timeout(Duration::from_secs(2), execution)
        .await
        .expect("cooperative cancellation should settle")
        .expect("workflow task should not panic")
        .expect_err("cancelled workflow should fail");

    match failure {
        WorkflowFailure::Cancelled {
            completed,
            settled_failures,
            ..
        } => {
            assert!(completed.is_empty());
            assert_eq!(settled_failures.len(), 1);
            assert_eq!(settled_failures[0].error.kind, ErrorKind::Cancelled);
            assert_eq!(settled_failures[0].error.effects, EffectState::Possible);
        }
        other => panic!("unexpected failure: {other:?}"),
    }
}

#[tokio::test]
async fn type_erased_dispatcher_runs_the_same_contract() {
    let definition = WorkflowDefinition::single("boxed", "v1", StepSpec::new("call", "boxed job"))
        .expect("single-step workflow is valid");
    let dispatcher: BoxStepService<(), &'static str, String, Infallible> = BoxStepService::new(
        service_fn(|call: StepCall<(), &'static str, String>| async move {
            Ok::<_, Infallible>(format!("{}:{}", call.step_id, call.job))
        }),
    );

    let outcome = WorkflowService::<_, String, Infallible>::new(dispatcher)
        .oneshot(WorkflowRequest::new(
            WorkflowContext::new(run_id("run-boxed")),
            definition,
            (),
        ))
        .await
        .expect("boxed dispatcher succeeds");

    assert_eq!(outcome.outputs[&step_id("call")].as_str(), "call:boxed job");
}

#[tokio::test]
async fn max_concurrency_is_hard_without_wave_blocking_newly_ready_work() {
    let definition = DagBuilder::new("bounded", "v1")
        .step(StepSpec::new("a-fast-root", ()))
        .step(StepSpec::new("b-slow-root", ()))
        .step(StepSpec::new("c-fast-child", ()).needs(["a-fast-root"]))
        .build()
        .expect("dag is valid");
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();
    let release_fast_root = tower_agent::CancellationToken::new();
    let release_slow_root = tower_agent::CancellationToken::new();
    let dispatcher = service_fn({
        let release_fast_root = release_fast_root.clone();
        let release_slow_root = release_slow_root.clone();
        move |call: StepCall<(), (), ()>| {
            let started_tx = started_tx.clone();
            let release_fast_root = release_fast_root.clone();
            let release_slow_root = release_slow_root.clone();
            async move {
                let step_id = call.step_id;
                started_tx.send(step_id.clone()).expect("receiver is alive");
                match step_id.as_str() {
                    "a-fast-root" => release_fast_root.cancelled().await,
                    "b-slow-root" => release_slow_root.cancelled().await,
                    "c-fast-child" => {}
                    other => panic!("unexpected step: {other}"),
                }
                Ok::<_, Infallible>(())
            }
        }
    });
    let execution = tokio::spawn(async move {
        WorkflowService::<_, (), Infallible>::new(dispatcher)
            .with_max_concurrency(NonZeroUsize::new(2).expect("nonzero"))
            .oneshot(WorkflowRequest::new(
                WorkflowContext::new(run_id("run-bounded")),
                definition,
                (),
            ))
            .await
    });

    let first = tokio::time::timeout(Duration::from_secs(2), started_rx.recv())
        .await
        .expect("first step should start")
        .expect("sender is alive");
    let second = tokio::time::timeout(Duration::from_secs(2), started_rx.recv())
        .await
        .expect("second step should start")
        .expect("sender is alive");
    assert_eq!(
        [first, second]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>(),
        [step_id("a-fast-root"), step_id("b-slow-root")]
            .into_iter()
            .collect()
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(50), started_rx.recv())
            .await
            .is_err(),
        "a third step crossed the concurrency boundary"
    );

    // Release only the fast root. Its dependent child must use the newly
    // available slot without waiting for the unrelated slow root.
    release_fast_root.cancel();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), started_rx.recv())
            .await
            .expect("newly available capacity should be used")
            .expect("sender is alive"),
        step_id("c-fast-child")
    );
    release_slow_root.cancel();
    tokio::time::timeout(Duration::from_secs(2), execution)
        .await
        .expect("workflow should finish")
        .expect("workflow task should not panic")
        .expect("workflow succeeds");
}

#[derive(Clone)]
struct NeverReadyStep {
    readiness_polled: mpsc::UnboundedSender<()>,
    calls: Arc<AtomicUsize>,
}

impl Service<StepCall<(), (), ()>> for NeverReadyStep {
    type Response = ();
    type Error = Infallible;
    type Future = Ready<Result<(), Infallible>>;

    fn poll_ready(&mut self, _context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let _ = self.readiness_polled.send(());
        Poll::Pending
    }

    fn call(&mut self, _request: StepCall<(), (), ()>) -> Self::Future {
        self.calls.fetch_add(1, Ordering::SeqCst);
        ready(Ok(()))
    }
}

#[tokio::test]
async fn cancellation_interrupts_pending_dispatcher_readiness_without_calling() {
    let definition = WorkflowDefinition::single("pending-ready", "v1", StepSpec::new("call", ()))
        .expect("single-step workflow is valid");
    let cancellation = tower_agent::CancellationToken::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let (readiness_tx, mut readiness_rx) = mpsc::unbounded_channel();
    let dispatcher = NeverReadyStep {
        readiness_polled: readiness_tx,
        calls: Arc::clone(&calls),
    };
    let request = WorkflowRequest::new(
        WorkflowContext::new(run_id("run-pending-ready")).with_cancellation(cancellation.clone()),
        definition,
        (),
    );
    let execution = tokio::spawn(async move {
        WorkflowService::<_, (), Infallible>::new(dispatcher)
            .oneshot(request)
            .await
    });

    tokio::time::timeout(Duration::from_secs(2), readiness_rx.recv())
        .await
        .expect("dispatcher readiness should be polled")
        .expect("sender is alive");
    cancellation.cancel();
    let failure = tokio::time::timeout(Duration::from_secs(2), execution)
        .await
        .expect("readiness cancellation should settle")
        .expect("workflow task should not panic")
        .expect_err("workflow should be cancelled");

    assert!(matches!(failure, WorkflowFailure::Cancelled { .. }));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[derive(Clone)]
struct NeverReadyAgent {
    readiness_polled: mpsc::UnboundedSender<()>,
    calls: Arc<AtomicUsize>,
}

impl Service<AgentRequest<Turn>> for NeverReadyAgent {
    type Response = TurnOutcome;
    type Error = AgentError;
    type Future = Ready<Result<TurnOutcome, AgentError>>;

    fn poll_ready(&mut self, _context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let _ = self.readiness_polled.send(());
        Poll::Pending
    }

    fn call(&mut self, request: AgentRequest<Turn>) -> Self::Future {
        self.calls.fetch_add(1, Ordering::SeqCst);
        ready(Ok(TurnOutcome::new(request.body.prompt)))
    }
}

#[tokio::test]
async fn agent_adapter_preserves_context_and_interrupts_inner_readiness() {
    let definition = WorkflowDefinition::single(
        "pending-agent-ready",
        "v1",
        StepSpec::new("call", "prompt".to_string()),
    )
    .expect("single-step workflow is valid");
    let cancellation = tower_agent::CancellationToken::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let (readiness_tx, mut readiness_rx) = mpsc::unbounded_channel();
    let dispatcher = AgentStepService::new(
        NeverReadyAgent {
            readiness_polled: readiness_tx,
            calls: Arc::clone(&calls),
        },
        |call: StepCall<(), String, TurnOutcome>| Ok(Turn::new(call.job)),
    );
    let request = WorkflowRequest::new(
        WorkflowContext::new(run_id("run-pending-agent-ready"))
            .with_cancellation(cancellation.clone()),
        definition,
        (),
    );
    let execution = tokio::spawn(async move {
        WorkflowService::<_, TurnOutcome, AgentError>::new(dispatcher)
            .oneshot(request)
            .await
    });

    tokio::time::timeout(Duration::from_secs(2), readiness_rx.recv())
        .await
        .expect("inner readiness should be polled")
        .expect("sender is alive");
    cancellation.cancel();
    let failure = tokio::time::timeout(Duration::from_secs(2), execution)
        .await
        .expect("inner readiness cancellation should settle")
        .expect("workflow task should not panic")
        .expect_err("workflow should be cancelled");

    match failure {
        WorkflowFailure::Cancelled {
            settled_failures, ..
        } => {
            assert_eq!(settled_failures.len(), 1);
            assert_eq!(settled_failures[0].error.kind, ErrorKind::Cancelled);
            assert_eq!(settled_failures[0].error.effects, EffectState::None);
        }
        other => panic!("unexpected failure: {other:?}"),
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[derive(Clone, Debug)]
enum ParallelFailureJob {
    Fail,
    AwaitCancellation,
    NotAdmittedAfterFailure,
}

#[tokio::test(start_paused = true)]
async fn exact_failure_evidence_survives_while_a_scripted_sibling_is_drained() {
    let definition = DagBuilder::new("parallel-failure", "v1")
        .step(StepSpec::new("a-fail", ParallelFailureJob::Fail))
        .step(StepSpec::new(
            "b-await-cancellation",
            ParallelFailureJob::AwaitCancellation,
        ))
        .step(StepSpec::new(
            "z-not-admitted",
            ParallelFailureJob::NotAdmittedAfterFailure,
        ))
        .build()
        .expect("dag is valid");
    let evidence = FailureEvidence {
        session: Some(SessionHandle::new("fake", "reserved-session")),
        usage: Some(TokenUsage {
            input: Some(13),
            output: Some(5),
            provider_total: Some(18),
            ..TokenUsage::default()
        }),
        cost: Some(Cost::usd(0.08)),
        duration: Some(Duration::from_secs(3)),
        provider_turns: Some(2),
    };
    let expected_failure = AgentError::new(
        ErrorKind::Budget,
        "scripted budget rail",
        FailurePhase::Settlement,
        EffectState::Reported,
    )
    .with_evidence(evidence);
    let expected_for_dispatch = expected_failure.clone();
    let fail_calls = Arc::new(AtomicUsize::new(0));
    let sibling_calls = Arc::new(AtomicUsize::new(0));
    let calls_after_failure = Arc::new(AtomicUsize::new(0));
    let (observer, mut events) = EventObserver::channel(4);
    let (cleanup_started_tx, mut cleanup_started_rx) = mpsc::unbounded_channel();
    let dispatcher = service_fn({
        let fail_calls = Arc::clone(&fail_calls);
        let sibling_calls = Arc::clone(&sibling_calls);
        let calls_after_failure = Arc::clone(&calls_after_failure);
        move |call: StepCall<(), ParallelFailureJob, TurnOutcome>| {
            let expected_failure = expected_for_dispatch.clone();
            let fail_calls = Arc::clone(&fail_calls);
            let sibling_calls = Arc::clone(&sibling_calls);
            let calls_after_failure = Arc::clone(&calls_after_failure);
            let observer = observer.clone();
            let cleanup_started_tx = cleanup_started_tx.clone();
            async move {
                match call.job {
                    ParallelFailureJob::Fail => {
                        fail_calls.fetch_add(1, Ordering::SeqCst);
                        FakeService
                            .oneshot(AgentRequest::with_context(
                                Turn::new("fail after sibling starts").with_options(
                                    FakeOptions::fail_with(expected_failure)
                                        .with_script([FakeStep::Delay(Duration::from_secs(1))]),
                                ),
                                call.agent_context(),
                            ))
                            .await
                    }
                    ParallelFailureJob::AwaitCancellation => {
                        sibling_calls.fetch_add(1, Ordering::SeqCst);
                        let result = FakeService
                            .oneshot(AgentRequest::with_context(
                                Turn::new("drain on cancellation").with_options(
                                    FakeOptions::succeed(TurnOutcome::new("too late")).with_script(
                                        [
                                            FakeStep::Emit(AgentEvent::Started),
                                            FakeStep::Delay(Duration::from_secs(30)),
                                            FakeStep::Emit(AgentEvent::OutputDelta {
                                                text: "must not be emitted".into(),
                                            }),
                                        ],
                                    ),
                                ),
                                call.agent_context().with_events(observer),
                            ))
                            .await;
                        if result
                            .as_ref()
                            .is_err_and(|error| error.kind == ErrorKind::Cancelled)
                        {
                            cleanup_started_tx.send(()).expect("receiver is alive");
                            // Model settlement cleanup that outlives the
                            // workflow deadline after a step failure was latched.
                            tokio::time::sleep(Duration::from_secs(2)).await;
                        }
                        result
                    }
                    ParallelFailureJob::NotAdmittedAfterFailure => {
                        calls_after_failure.fetch_add(1, Ordering::SeqCst);
                        panic!("a new call was admitted after a sibling failure")
                    }
                }
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    let request = WorkflowRequest::new(
        WorkflowContext::new(run_id("run-parallel-failure")).with_deadline(deadline),
        definition,
        (),
    );
    let execution = tokio::spawn(async move {
        WorkflowService::<_, TurnOutcome, AgentError>::new(dispatcher)
            .with_max_concurrency(NonZeroUsize::new(2).expect("nonzero"))
            .oneshot(request)
            .await
    });

    assert_eq!(
        tokio::time::timeout(Duration::from_millis(10), events.recv())
            .await
            .expect("scripted sibling should start"),
        Some(AgentEvent::Started)
    );
    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::time::timeout(Duration::from_millis(10), cleanup_started_rx.recv())
        .await
        .expect("sibling should enter settlement cleanup")
        .expect("sender is alive");
    tokio::time::advance(Duration::from_secs(2)).await;
    let failure = tokio::time::timeout(Duration::from_millis(10), execution)
        .await
        .expect("called sibling should settle after cancellation")
        .expect("workflow task should not panic")
        .expect_err("workflow should fail");

    assert_eq!(fail_calls.load(Ordering::SeqCst), 1, "failure retried");
    assert_eq!(sibling_calls.load(Ordering::SeqCst), 1);
    assert_eq!(calls_after_failure.load(Ordering::SeqCst), 0);
    assert!(
        events.try_recv().is_err(),
        "event after the cancelled scripted delay was emitted"
    );
    match failure {
        WorkflowFailure::StepsFailed { failures, .. } => {
            assert_eq!(failures.len(), 2);
            assert_eq!(failures[0].step_id, step_id("a-fail"));
            assert_eq!(failures[0].error, expected_failure);
            assert_eq!(failures[1].step_id, step_id("b-await-cancellation"));
            assert_eq!(failures[1].error.kind, ErrorKind::Cancelled);
            assert_eq!(failures[1].error.effects, EffectState::Possible);
        }
        other => panic!("unexpected failure: {other:?}"),
    }
}

#[tokio::test(start_paused = true)]
async fn deadline_signals_in_flight_cancellation_and_awaits_settlement() {
    let definition = WorkflowDefinition::single("deadline", "v1", StepSpec::new("call", ()))
        .expect("single-step workflow is valid");
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();
    let deadline = Instant::now() + Duration::from_millis(50);
    let dispatcher = service_fn(move |call: StepCall<(), (), ()>| {
        let started_tx = started_tx.clone();
        async move {
            assert_eq!(call.deadline, Some(deadline));
            started_tx.send(()).expect("receiver is alive");
            call.cancellation.cancelled().await;
            Err::<(), _>(AgentError::cancelled(EffectState::Possible))
        }
    });
    let request = WorkflowRequest::new(
        WorkflowContext::new(run_id("run-deadline")).with_deadline(deadline),
        definition,
        (),
    );
    let execution = tokio::spawn(async move {
        WorkflowService::<_, (), AgentError>::new(dispatcher)
            .oneshot(request)
            .await
    });

    tokio::time::timeout(Duration::from_millis(10), started_rx.recv())
        .await
        .expect("step should start before the deadline")
        .expect("sender is alive");
    tokio::time::advance(Duration::from_millis(50)).await;
    let failure = tokio::time::timeout(Duration::from_millis(10), execution)
        .await
        .expect("deadline cleanup should settle")
        .expect("workflow task should not panic")
        .expect_err("workflow should exceed its deadline");

    match failure {
        WorkflowFailure::DeadlineExceeded {
            settled_failures, ..
        } => {
            assert_eq!(settled_failures.len(), 1);
            assert_eq!(settled_failures[0].error.kind, ErrorKind::Cancelled);
            assert_eq!(settled_failures[0].error.effects, EffectState::Possible);
        }
        other => panic!("unexpected failure: {other:?}"),
    }
}
