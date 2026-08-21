//! Local-only proof that `WorkflowService` can dispatch ready steps through
//! Apalis without giving Apalis ownership of graph or result semantics.
//!
//! This is an in-process semantics test, not a persistent-store recipe. See
//! `examples/apalis-local/README.md` for the state model and explicit non-goals.

use std::{
    collections::BTreeMap,
    future::Future,
    io,
    num::NonZeroUsize,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, ensure};
use apalis::prelude::{
    Extensions, MemorySink, MemoryStorage, RandomId, Task, WorkerBuilder, WorkerError,
};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::{Barrier, Mutex, watch};
use tower::{Service, ServiceBuilder, ServiceExt, limit::ConcurrencyLimitLayer};
use tower_agent::{
    AgentError, AgentRequest, BoxTurnService, CancellationToken, EffectState, ErrorKind,
    FailurePhase, FakeOptions, FakeService, Turn, TurnOutcome,
    layer::{AdmissionLayer, CatchPanicLayer, DeadlineLayer, SuperviseLayer, ValidateTurnLayer},
};
use tower_agent_workflow::{
    DagBuilder, StepCall, StepId, StepSpec, WorkflowContext, WorkflowDefinition, WorkflowFailure,
    WorkflowOutcome, WorkflowRequest, WorkflowRunId, WorkflowService,
};

const ARCHITECT_PROVIDER: &str = "fake-architect";
const VERIFIER_PROVIDER: &str = "fake-verifier";
const JOB_REF_SCHEMA_VERSION: u16 = 1;
const EXPECTED_LOGICAL_STEPS: usize = 3;
const EXPECTED_DELIVERIES: usize = EXPECTED_LOGICAL_STEPS * 2;

type LocalStepCall = StepCall<ReviewRequest, ReviewJob, TurnOutcome>;
type TerminalResult = Result<TurnOutcome, AgentError>;
type ApalisTask = Task<StepJobRef, Extensions, RandomId>;

#[derive(Debug, PartialEq, Eq)]
struct ReviewRequest {
    repository: String,
    objective: String,
}

#[derive(Clone, Debug, PartialEq)]
enum ReviewJob {
    Architecture(FakeOptions),
    Verification(FakeOptions),
    Synthesize,
}

/// The only application payload that crosses the Apalis boundary.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
struct StepJobRef {
    schema_version: u16,
    run_id: String,
    workflow_id: String,
    workflow_version: String,
    step_id: String,
}

impl StepJobRef {
    fn for_call(call: &LocalStepCall) -> Self {
        Self {
            schema_version: JOB_REF_SCHEMA_VERSION,
            run_id: call.run_id.to_string(),
            workflow_id: call.workflow_id.to_string(),
            workflow_version: call.workflow_version.to_string(),
            step_id: call.step_id.to_string(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ClaimEpoch(u64);

#[derive(Clone, Debug, PartialEq, Eq)]
struct StepClaim {
    reference: StepJobRef,
    epoch: ClaimEpoch,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RecordState {
    Pending,
    Claimed(ClaimEpoch),
    Launched(ClaimEpoch),
    Terminal(ClaimEpoch),
    #[allow(dead_code, reason = "constructed by the worker-loss proof test")]
    Uncertain(ClaimEpoch),
}

struct StepRecord {
    call: Option<LocalStepCall>,
    frozen: FrozenStepCall,
    state: RecordState,
    provider_call_recorded: bool,
    terminal: watch::Sender<Option<TerminalResult>>,
}

/// Definition/input data that one logical identity permanently names.
///
/// Runtime cancellation and `Instant` deadlines are deliberately retained
/// from the first registration rather than compared with a replay's new local
/// controls.
#[derive(Clone, Debug, PartialEq)]
struct FrozenStepCall {
    input: Arc<ReviewRequest>,
    job: ReviewJob,
    dependencies: BTreeMap<StepId, Arc<TurnOutcome>>,
}

impl FrozenStepCall {
    fn for_call(call: &LocalStepCall) -> Self {
        Self {
            input: Arc::clone(&call.input),
            job: call.job.clone(),
            dependencies: call.dependencies.clone(),
        }
    }
}

#[derive(Debug, Default)]
#[allow(dead_code, reason = "constructed by the worker-loss proof test")]
struct WorkerLossRecovery {
    retryable: Vec<StepJobRef>,
    uncertain: Vec<StepClaim>,
}

#[derive(Clone, Debug, Default)]
struct StoreStats {
    registered_records: usize,
    deliveries: usize,
    claims: usize,
    launches: usize,
    duplicate_skips: usize,
    terminal_records: usize,
    uncertain_records: usize,
    provider_calls: BTreeMap<StepJobRef, usize>,
}

#[derive(Default)]
struct StoreInner {
    records: BTreeMap<StepJobRef, StepRecord>,
    stats: StoreStats,
    next_epoch: u64,
}

/// In production this role belongs to the application's durable run store.
#[derive(Clone)]
struct LocalRunStore {
    inner: Arc<Mutex<StoreInner>>,
    revision: watch::Sender<u64>,
}

impl LocalRunStore {
    fn new() -> Self {
        let (revision, _) = watch::channel(0);
        Self {
            inner: Arc::new(Mutex::new(StoreInner::default())),
            revision,
        }
    }

    async fn register(
        &self,
        reference: StepJobRef,
        call: LocalStepCall,
    ) -> io::Result<(bool, watch::Receiver<Option<TerminalResult>>)> {
        let mut inner = self.inner.lock().await;
        let frozen = FrozenStepCall::for_call(&call);
        if let Some(record) = inner.records.get(&reference) {
            if record.frozen != frozen {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "logical identity collision for workflow step `{}`",
                        reference.step_id
                    ),
                ));
            }
            // If a coordinator disappeared after registration but before its
            // enqueue completed, replaying Pending work is safe: claim epochs
            // still admit only one worker. Every later state is already owned
            // or settled and must only be observed.
            let should_enqueue = record.state == RecordState::Pending;
            return Ok((should_enqueue, record.terminal.subscribe()));
        }

        let (terminal, receiver) = watch::channel(None);
        inner.records.insert(
            reference,
            StepRecord {
                call: Some(call),
                frozen,
                state: RecordState::Pending,
                provider_call_recorded: false,
                terminal,
            },
        );
        inner.stats.registered_records += 1;
        drop(inner);
        self.notify();
        Ok((true, receiver))
    }

    async fn claim(&self, reference: &StepJobRef) -> io::Result<Option<StepClaim>> {
        let mut inner = self.inner.lock().await;
        let state = inner
            .records
            .get(reference)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("missing host record for {reference:?}"),
                )
            })?
            .state;
        inner.stats.deliveries += 1;
        let claim = if state == RecordState::Pending {
            inner.next_epoch = inner
                .next_epoch
                .checked_add(1)
                .ok_or_else(|| io::Error::other("host claim epoch overflow"))?;
            let epoch = ClaimEpoch(inner.next_epoch);
            let record = inner
                .records
                .get_mut(reference)
                .expect("record existence was checked above");
            record.state = RecordState::Claimed(epoch);
            inner.stats.claims += 1;
            Some(StepClaim {
                reference: reference.clone(),
                epoch,
            })
        } else {
            inner.stats.duplicate_skips += 1;
            None
        };
        drop(inner);
        self.notify();
        Ok(claim)
    }

    async fn launch(&self, claim: &StepClaim) -> io::Result<LocalStepCall> {
        let mut inner = self.inner.lock().await;
        let call = {
            let record = inner.records.get_mut(&claim.reference).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("missing host record for {:?}", claim.reference),
                )
            })?;
            if record.state != RecordState::Claimed(claim.epoch) {
                return Err(io::Error::other(format!(
                    "cannot launch {:?} at epoch {:?} from {:?}",
                    claim.reference, claim.epoch, record.state
                )));
            }
            let call = record.call.take().ok_or_else(|| {
                io::Error::other("claimed record did not retain its owned StepCall")
            })?;
            record.state = RecordState::Launched(claim.epoch);
            call
        };
        inner.stats.launches += 1;
        drop(inner);
        self.notify();
        Ok(call)
    }

    async fn note_provider_call(&self, claim: &StepClaim) -> io::Result<()> {
        let mut inner = self.inner.lock().await;
        {
            let record = inner.records.get_mut(&claim.reference).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("missing host record for {:?}", claim.reference),
                )
            })?;
            if record.state != RecordState::Launched(claim.epoch) {
                return Err(io::Error::other(format!(
                    "cannot call provider for {:?} at epoch {:?} from {:?}",
                    claim.reference, claim.epoch, record.state
                )));
            }
            if record.provider_call_recorded {
                return Err(io::Error::other(format!(
                    "provider call already recorded for {:?}",
                    claim.reference
                )));
            }
            record.provider_call_recorded = true;
        }
        *inner
            .stats
            .provider_calls
            .entry(claim.reference.clone())
            .or_default() += 1;
        drop(inner);
        self.notify();
        Ok(())
    }

    async fn complete(&self, claim: &StepClaim, result: TerminalResult) -> io::Result<()> {
        let mut inner = self.inner.lock().await;
        let terminal = {
            let record = inner.records.get_mut(&claim.reference).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("missing host record for {:?}", claim.reference),
                )
            })?;
            if record.state != RecordState::Launched(claim.epoch) {
                return Err(io::Error::other(format!(
                    "cannot complete {:?} at epoch {:?} from {:?}",
                    claim.reference, claim.epoch, record.state
                )));
            }
            if !record.provider_call_recorded {
                return Err(io::Error::other(format!(
                    "cannot complete {:?} before its provider call boundary",
                    claim.reference
                )));
            }
            record.state = RecordState::Terminal(claim.epoch);
            record.terminal.clone()
        };
        inner.stats.terminal_records += 1;
        drop(inner);

        terminal.send_replace(Some(result));
        self.notify();
        Ok(())
    }

    /// Fence work owned by a worker that is known to be gone.
    ///
    /// A claim still retains its call and is safe to make Pending again. A
    /// launched call may have crossed an external effect boundary, so it can
    /// only settle as Uncertain. The returned Pending references can be
    /// re-enqueued immediately or by a coordinator replay.
    #[allow(dead_code, reason = "exercised by the worker-loss proof tests")]
    async fn reconcile_worker_loss(&self) -> WorkerLossRecovery {
        let mut inner = self.inner.lock().await;
        let mut recovery = WorkerLossRecovery::default();
        let mut settlements = Vec::new();
        for (reference, record) in &mut inner.records {
            match record.state {
                RecordState::Claimed(_) => {
                    record.state = RecordState::Pending;
                    recovery.retryable.push(reference.clone());
                }
                RecordState::Launched(epoch) => {
                    record.state = RecordState::Uncertain(epoch);
                    let claim = StepClaim {
                        reference: reference.clone(),
                        epoch,
                    };
                    recovery.uncertain.push(claim);
                    settlements.push((reference.clone(), record.terminal.clone()));
                }
                RecordState::Pending | RecordState::Terminal(_) | RecordState::Uncertain(_) => {}
            }
        }
        inner.stats.uncertain_records += recovery.uncertain.len();
        drop(inner);

        for (reference, settlement) in settlements {
            settlement.send_replace(Some(Err(AgentError::new(
                ErrorKind::Internal,
                format!(
                    "worker was lost after `{}` crossed the launch boundary; its outcome is uncertain",
                    reference.step_id
                ),
                FailurePhase::Settlement,
                EffectState::Possible,
            ))));
        }
        if !recovery.retryable.is_empty() || !recovery.uncertain.is_empty() {
            self.notify();
        }
        recovery
    }

    async fn stats(&self) -> StoreStats {
        self.inner.lock().await.stats.clone()
    }

    async fn wait_for_stats(
        &self,
        predicate: impl Fn(&StoreStats) -> bool,
    ) -> io::Result<StoreStats> {
        let mut revision = self.revision.subscribe();
        loop {
            let stats = self.stats().await;
            if predicate(&stats) {
                return Ok(stats);
            }
            revision.changed().await.map_err(|_| {
                io::Error::new(io::ErrorKind::BrokenPipe, "host-store revision closed")
            })?;
        }
    }

    fn notify(&self) {
        self.revision
            .send_modify(|revision| *revision = revision.wrapping_add(1));
    }
}

/// Application-owned bridge from a ready workflow step to Apalis transport.
#[derive(Clone)]
struct DurableStepDispatcher {
    store: LocalRunStore,
    queue: MemorySink<StepJobRef>,
}

impl Service<LocalStepCall> for DurableStepDispatcher {
    type Response = TurnOutcome;
    type Error = AgentError;
    type Future = Pin<Box<dyn Future<Output = TerminalResult> + Send + 'static>>;

    fn poll_ready(&mut self, _context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, call: LocalStepCall) -> Self::Future {
        let store = self.store.clone();
        let mut queue = self.queue.clone();
        Box::pin(async move {
            let reference = StepJobRef::for_call(&call);
            let (should_enqueue, mut terminal) = store
                .register(reference.clone(), call)
                .await
                .map_err(internal)?;

            if should_enqueue {
                println!(
                    "enqueue {}",
                    serde_json::to_string(&reference).map_err(internal)?
                );

                // Deliberate at-least-once delivery: the host claim, rather
                // than an Apalis idempotency key, must suppress the duplicate.
                queue
                    .send(Task::new(reference.clone()))
                    .await
                    .map_err(internal)?;
                queue.send(Task::new(reference)).await.map_err(internal)?;
            }

            loop {
                if let Some(result) = terminal.borrow_and_update().clone() {
                    return result;
                }
                terminal.changed().await.map_err(|_| {
                    internal("host terminal-result channel closed before settlement")
                })?;
            }
        })
    }
}

/// Deterministic root rendezvous plus a non-lossy test-controlled release.
#[derive(Clone)]
struct RootControl {
    rendezvous: Arc<Barrier>,
    release: CancellationToken,
}

impl RootControl {
    fn held() -> Self {
        Self {
            rendezvous: Arc::new(Barrier::new(2)),
            release: CancellationToken::new(),
        }
    }

    fn release(&self) {
        self.release.cancel();
    }

    async fn wait(&self, shutdown: &CancellationToken, step: &CancellationToken) {
        tokio::select! {
            biased;
            () = shutdown.cancelled() => {
                step.cancel();
                return;
            }
            () = step.cancelled() => return,
            _ = self.rendezvous.wait() => {}
        }
        tokio::select! {
            biased;
            () = shutdown.cancelled() => step.cancel(),
            () = step.cancelled() => {}
            () = self.release.cancelled() => {}
        }
    }
}

/// An ordinary Tower service installed directly into the Apalis worker.
#[derive(Clone)]
struct LocalAgentTaskService {
    store: LocalRunStore,
    root_control: RootControl,
    shutdown: CancellationToken,
    architect: BoxTurnService<FakeOptions>,
    verifier: BoxTurnService<FakeOptions>,
}

impl LocalAgentTaskService {
    fn new(store: LocalRunStore, shutdown: CancellationToken, root_control: RootControl) -> Self {
        Self {
            store,
            root_control,
            shutdown,
            architect: fake_provider(ARCHITECT_PROVIDER),
            verifier: fake_provider(VERIFIER_PROVIDER),
        }
    }
}

impl Service<ApalisTask> for LocalAgentTaskService {
    type Response = ();
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, _context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, task: ApalisTask) -> Self::Future {
        let store = self.store.clone();
        let root_control = self.root_control.clone();
        let shutdown = self.shutdown.clone();
        let architect = self.architect.clone();
        let verifier = self.verifier.clone();

        Box::pin(async move {
            let reference = task.args;
            let Some(claim) = store.claim(&reference).await? else {
                return Ok(());
            };
            let call = store.launch(&claim).await?;
            let cancellation = call.cancellation.clone();
            if matches!(
                &call.job,
                ReviewJob::Architecture(_) | ReviewJob::Verification(_)
            ) {
                root_control.wait(&shutdown, &cancellation).await;
            }

            store.note_provider_call(&claim).await?;
            let execution = execute_agent_step(call, architect, verifier);
            tokio::pin!(execution);
            let result = tokio::select! {
                biased;
                result = &mut execution => result,
                () = shutdown.cancelled() => {
                    cancellation.cancel();
                    execution.await
                }
            };

            // Typed domain success or failure is committed before returning
            // queue-level success to Apalis.
            store.complete(&claim, result).await?;
            Ok(())
        })
    }
}

#[derive(Debug)]
struct ProofSummary {
    output: String,
    before_replay: StoreStats,
    stats: StoreStats,
}

struct LocalWorker {
    queue: MemorySink<StepJobRef>,
    shutdown: CancellationToken,
    task: tokio::task::JoinHandle<Result<(), WorkerError>>,
}

impl LocalWorker {
    fn start(store: LocalRunStore, root_control: RootControl) -> Self {
        let (backend, queue) = local_queue();
        let shutdown = CancellationToken::new();
        let worker_shutdown = shutdown.clone();
        let worker = WorkerBuilder::new("local-agent-steps")
            .backend(backend)
            .layer(ConcurrencyLimitLayer::new(4))
            .build(LocalAgentTaskService::new(
                store,
                shutdown.clone(),
                root_control,
            ));
        let task = tokio::spawn(worker.run_until(async move {
            worker_shutdown.cancelled().await;
            Ok::<(), io::Error>(())
        }));
        Self {
            queue,
            shutdown,
            task,
        }
    }

    fn queue(&self) -> MemorySink<StepJobRef> {
        self.queue.clone()
    }

    async fn stop(self) -> Result<()> {
        let Self { shutdown, task, .. } = self;
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .context("local Apalis worker did not stop")?
            .context("local Apalis worker task panicked")??;
        Ok(())
    }

    #[allow(dead_code, reason = "exercised by the worker-loss proof test")]
    async fn abort(self) -> Result<()> {
        let Self { task, .. } = self;
        task.abort();
        let joined = tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .context("aborted local Apalis worker did not join")?;
        match joined {
            Err(error) => {
                ensure!(error.is_cancelled(), "worker abort returned {error}");
                Ok(())
            }
            Ok(Ok(())) => anyhow::bail!("aborted local Apalis worker returned normally"),
            Ok(Err(error)) => anyhow::bail!("aborted local Apalis worker failed: {error}"),
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let proof = run_replay_proof().await?;
    println!("\n{}", proof.output);
    println!(
        "\nbefore replay: {} deliveries, {} terminal records\n\
         after replay: {} deliveries, {} claim wins, {} launches, {} duplicate skips, {} terminal records, {} uncertain records",
        proof.before_replay.deliveries,
        proof.before_replay.terminal_records,
        proof.stats.deliveries,
        proof.stats.claims,
        proof.stats.launches,
        proof.stats.duplicate_skips,
        proof.stats.terminal_records,
        proof.stats.uncertain_records,
    );
    Ok(())
}

async fn run_replay_proof() -> Result<ProofSummary> {
    let store = LocalRunStore::new();
    let root_control = RootControl::held();
    let worker = LocalWorker::start(store.clone(), root_control.clone());
    let queue = worker.queue();

    let proof: Result<ProofSummary> = async {
        let first_coordinator = tokio::spawn(execute_workflow(
            store.clone(),
            queue.clone(),
            "apalis-replay-run",
        ));
        let roots_ready = tokio::time::timeout(
            Duration::from_secs(2),
            store.wait_for_stats(|stats| stats.launches == 2 && stats.deliveries == 4),
        )
        .await;

        // Dropping a JoinHandle detaches its task, so simulate coordinator
        // loss with an explicit abort and wait for cancellation to settle.
        first_coordinator.abort();
        let coordinator_error = tokio::time::timeout(Duration::from_secs(2), first_coordinator)
            .await
            .context("lost coordinator did not join")?
            .expect_err("first coordinator unexpectedly completed");
        ensure!(
            coordinator_error.is_cancelled(),
            "first coordinator abort returned {coordinator_error}"
        );
        roots_ready.context("root steps did not reach their launch boundary")??;

        root_control.release();
        let before_replay = tokio::time::timeout(
            Duration::from_secs(2),
            store.wait_for_stats(|stats| stats.terminal_records == 2),
        )
        .await
        .context("root steps did not settle after coordinator loss")??;
        validate_root_snapshot(&before_replay)?;

        // The same logical identity re-subscribes to settled roots. Only the
        // newly-ready join is registered and delivered.
        let outcome = tokio::time::timeout(
            Duration::from_secs(2),
            execute_workflow(store.clone(), queue, "apalis-replay-run"),
        )
        .await
        .context("replayed coordinator did not settle")??;

        tokio::time::timeout(
            Duration::from_secs(2),
            store.wait_for_stats(|stats| stats.terminal_records == EXPECTED_LOGICAL_STEPS),
        )
        .await
        .context("replayed join did not settle")??;
        let stats = store.stats().await;
        let synthesis = validate_outcome(&outcome, "apalis-replay-run")?;
        validate_final_snapshot(&stats)?;

        Ok(ProofSummary {
            output: synthesis.output.clone(),
            before_replay,
            stats,
        })
    }
    .await;

    // Also releases a held gate on every error path before graceful shutdown.
    root_control.release();
    worker.stop().await?;
    proof
}

async fn execute_workflow(
    store: LocalRunStore,
    queue: MemorySink<StepJobRef>,
    run_id: &str,
) -> Result<WorkflowOutcome<TurnOutcome>, WorkflowFailure<AgentError, TurnOutcome>> {
    WorkflowService::new(DurableStepDispatcher { store, queue })
        .with_max_concurrency(NonZeroUsize::new(2).expect("two is nonzero"))
        .oneshot(WorkflowRequest::new(
            WorkflowContext::new(
                WorkflowRunId::new(run_id).expect("example uses a valid static run id"),
            )
            .with_deadline(Instant::now() + Duration::from_secs(4)),
            review_workflow().expect("example workflow definition is valid"),
            ReviewRequest {
                repository: "tower-agent".to_owned(),
                objective: "keep graph semantics above queue transport".to_owned(),
            },
        ))
        .await
}

fn validate_root_snapshot(stats: &StoreStats) -> Result<()> {
    ensure!(stats.registered_records == 2);
    ensure!(stats.deliveries == 4);
    ensure!(stats.claims == 2);
    ensure!(stats.launches == 2);
    ensure!(stats.duplicate_skips == 2);
    ensure!(stats.terminal_records == 2);
    ensure!(stats.uncertain_records == 0);
    ensure!(stats.provider_calls.len() == 2);
    ensure!(stats.provider_calls.values().all(|count| *count == 1));
    ensure!(
        stats
            .provider_calls
            .keys()
            .all(|reference| reference.step_id != "synthesize")
    );
    Ok(())
}

fn validate_final_snapshot(stats: &StoreStats) -> Result<()> {
    ensure!(stats.registered_records == EXPECTED_LOGICAL_STEPS);
    ensure!(stats.deliveries == EXPECTED_DELIVERIES);
    ensure!(stats.claims == EXPECTED_LOGICAL_STEPS);
    ensure!(stats.launches == EXPECTED_LOGICAL_STEPS);
    ensure!(stats.duplicate_skips == EXPECTED_LOGICAL_STEPS);
    ensure!(stats.terminal_records == EXPECTED_LOGICAL_STEPS);
    ensure!(stats.uncertain_records == 0);
    ensure!(stats.provider_calls.len() == EXPECTED_LOGICAL_STEPS);
    ensure!(stats.provider_calls.values().all(|count| *count == 1));
    Ok(())
}

fn validate_outcome<'a>(
    outcome: &'a WorkflowOutcome<TurnOutcome>,
    run_id: &str,
) -> Result<&'a TurnOutcome> {
    ensure!(outcome.run_id.as_str() == run_id);
    ensure!(outcome.workflow_id.as_str() == "repository-review");
    ensure!(outcome.workflow_version.as_str() == "v1");
    ensure!(outcome.outputs.len() == EXPECTED_LOGICAL_STEPS);
    ensure!(outcome.leaf_outputs.len() == 1);

    let synthesis = &outcome.outputs[&step_id("synthesize")];
    ensure!(
        synthesis.output
            == "joined: architecture kept transport thin | verification covered duplicate delivery"
    );
    ensure!(
        synthesis
            .session
            .as_ref()
            .is_some_and(|session| session.provider() == ARCHITECT_PROVIDER)
    );
    Ok(synthesis)
}

fn review_workflow() -> Result<WorkflowDefinition<ReviewJob>> {
    Ok(DagBuilder::new("repository-review", "v1")
        .step(StepSpec::new(
            "architecture",
            ReviewJob::Architecture(FakeOptions {
                delay: Some(Duration::from_millis(30)),
                output: Some("architecture kept transport thin".to_owned()),
                simulated_tokens: Some(120),
                simulated_cost_usd: Some(0.006),
                ..FakeOptions::default()
            }),
        ))
        .step(StepSpec::new(
            "verification",
            ReviewJob::Verification(FakeOptions {
                delay: Some(Duration::from_millis(20)),
                output: Some("verification covered duplicate delivery".to_owned()),
                simulated_tokens: Some(90),
                simulated_cost_usd: Some(0.004),
                ..FakeOptions::default()
            }),
        ))
        .step(
            StepSpec::new("synthesize", ReviewJob::Synthesize)
                .needs(["architecture", "verification"]),
        )
        .build()?)
}

async fn execute_agent_step(
    call: LocalStepCall,
    architect: BoxTurnService<FakeOptions>,
    verifier: BoxTurnService<FakeOptions>,
) -> TerminalResult {
    let context = call.agent_context();
    let (provider, turn) = match &call.job {
        ReviewJob::Architecture(options) => {
            let prompt = format!(
                "Review {} for: {}",
                call.input.repository, call.input.objective
            );
            (architect, Turn::new(prompt).with_options(options.clone()))
        }
        ReviewJob::Verification(options) => {
            let prompt = format!(
                "Verify {} for: {}",
                call.input.repository, call.input.objective
            );
            (verifier, Turn::new(prompt).with_options(options.clone()))
        }
        ReviewJob::Synthesize => {
            let architecture = dependency(&call, "architecture")?;
            let verification = dependency(&call, "verification")?;
            let session = architecture.session.clone().ok_or_else(|| {
                AgentError::invalid_request("architecture output did not include a session")
            })?;
            let output = format!("joined: {} | {}", architecture.output, verification.output);
            let options = FakeOptions {
                output: Some(output),
                simulated_tokens: Some(60),
                simulated_cost_usd: Some(0.003),
                ..FakeOptions::default()
            };
            let prompt = format!(
                "Synthesize:\n{}\n{}",
                architecture.output, verification.output
            );
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

fn dependency(call: &LocalStepCall, id: &str) -> Result<Arc<TurnOutcome>, AgentError> {
    call.dependencies
        .get(&step_id(id))
        .cloned()
        .ok_or_else(|| AgentError::invalid_request(format!("missing `{id}` dependency output")))
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

fn local_queue() -> (MemoryStorage<StepJobRef>, MemorySink<StepJobRef>) {
    let (raw_sender, raw_receiver) = futures_channel::mpsc::unbounded::<ApalisTask>();
    let sender = MemorySink::new(Arc::new(futures_util::lock::Mutex::new(
        Box::new(raw_sender) as _,
    )));
    let backend = MemoryStorage::new_with(sender.clone(), raw_receiver.boxed());
    (backend, sender)
}

fn step_id(value: &str) -> StepId {
    StepId::new(value).expect("example uses valid static step ids")
}

fn internal(error: impl std::fmt::Display) -> AgentError {
    AgentError::new(
        ErrorKind::Internal,
        error.to_string(),
        FailurePhase::Admission,
        EffectState::None,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_agent_workflow::{WorkflowId, WorkflowVersion};

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn coordinator_replay_reuses_settled_roots_and_only_schedules_the_join() {
        let proof = run_replay_proof()
            .await
            .expect("local Apalis replay proof should pass");
        assert_eq!(proof.before_replay.registered_records, 2);
        assert_eq!(proof.before_replay.deliveries, 4);
        assert_eq!(proof.before_replay.claims, 2);
        assert_eq!(proof.before_replay.launches, 2);
        assert_eq!(proof.before_replay.duplicate_skips, 2);
        assert_eq!(proof.before_replay.terminal_records, 2);
        assert_eq!(proof.stats.deliveries, EXPECTED_DELIVERIES);
        assert_eq!(proof.stats.claims, EXPECTED_LOGICAL_STEPS);
        assert_eq!(proof.stats.launches, EXPECTED_LOGICAL_STEPS);
        assert_eq!(proof.stats.duplicate_skips, EXPECTED_LOGICAL_STEPS);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lost_launched_work_fails_closed_and_replay_does_not_relaunch_it() {
        let store = LocalRunStore::new();
        let root_control = RootControl::held();
        let worker = LocalWorker::start(store.clone(), root_control);
        let queue = worker.queue();
        let replay_queue = queue.clone();
        let coordinator = tokio::spawn(execute_workflow(
            store.clone(),
            queue,
            "apalis-uncertain-run",
        ));

        let launched = tokio::time::timeout(
            Duration::from_secs(2),
            store.wait_for_stats(|stats| stats.launches == 2 && stats.deliveries == 4),
        )
        .await
        .expect("root work did not reach the launch boundary")
        .expect("host-store revision channel closed");
        assert_eq!(launched.provider_calls.len(), 0);
        assert!(!coordinator.is_finished());

        // This is deliberately not graceful shutdown: the worker disappears
        // with two owned calls beyond the host's launch boundary.
        worker
            .abort()
            .await
            .expect("worker abort should be observed");
        let recovery = store.reconcile_worker_loss().await;
        assert!(recovery.retryable.is_empty());
        let abandoned = recovery.uncertain;
        assert_eq!(abandoned.len(), 2);

        let first_failure = tokio::time::timeout(Duration::from_secs(1), coordinator)
            .await
            .expect("uncertain settlements left the coordinator hanging")
            .expect("coordinator task panicked")
            .expect_err("uncertain root work unexpectedly succeeded");
        assert_uncertain_failure(&first_failure, 2, 2);

        let before_replay = store.stats().await;
        assert_eq!(before_replay.registered_records, 2);
        assert_eq!(before_replay.deliveries, 4);
        assert_eq!(before_replay.claims, 2);
        assert_eq!(before_replay.launches, 2);
        assert_eq!(before_replay.duplicate_skips, 2);
        assert_eq!(before_replay.terminal_records, 0);
        assert_eq!(before_replay.uncertain_records, 2);
        assert!(before_replay.provider_calls.is_empty());

        for claim in &abandoned {
            let stale_result = store
                .complete(claim, Ok(TurnOutcome::new("stale completion")))
                .await;
            assert!(stale_result.is_err(), "a stale worker committed a result");
        }

        let replay_failure = tokio::time::timeout(
            Duration::from_secs(1),
            execute_workflow(store.clone(), replay_queue, "apalis-uncertain-run"),
        )
        .await
        .expect("replay of uncertain work hung")
        .expect_err("replay relaunched uncertain work");
        // Replay may observe one already-terminal root failure before calling
        // its ready sibling, or observe both if both dispatcher calls cross
        // first. The store remains the authority for both uncertain records.
        assert_uncertain_failure(&replay_failure, 1, 2);

        let after_replay = store.stats().await;
        assert_eq!(after_replay.registered_records, 2);
        assert_eq!(after_replay.deliveries, before_replay.deliveries);
        assert_eq!(after_replay.claims, before_replay.claims);
        assert_eq!(after_replay.launches, before_replay.launches);
        assert_eq!(after_replay.duplicate_skips, before_replay.duplicate_skips);
        assert_eq!(after_replay.uncertain_records, 2);
        assert!(after_replay.provider_calls.is_empty());

        for claim in abandoned {
            assert!(
                store
                    .claim(&claim.reference)
                    .await
                    .expect("duplicate claim lookup should succeed")
                    .is_none(),
                "uncertain work was reacquired"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn graceful_worker_stop_cancels_and_commits_called_steps_before_joining() {
        let store = LocalRunStore::new();
        let root_control = RootControl::held();
        let worker = LocalWorker::start(store.clone(), root_control);
        let queue = worker.queue();
        let replay_queue = queue.clone();
        let coordinator = tokio::spawn(execute_workflow(
            store.clone(),
            queue,
            "apalis-graceful-stop-run",
        ));

        let launched = tokio::time::timeout(
            Duration::from_secs(2),
            store.wait_for_stats(|stats| stats.launches == 2 && stats.deliveries == 4),
        )
        .await
        .expect("root work did not reach the launch boundary")
        .expect("host-store revision channel closed");
        assert!(launched.provider_calls.is_empty());

        // `run_until` signals the service and drains its in-flight futures.
        // Both records must settle before the worker task joins.
        worker
            .stop()
            .await
            .expect("graceful worker stop should drain called services");
        let failure = tokio::time::timeout(Duration::from_secs(1), coordinator)
            .await
            .expect("coordinator hung after graceful worker stop")
            .expect("coordinator task panicked")
            .expect_err("cancelled root work unexpectedly succeeded");
        assert_cancelled_step_failure(&failure, "apalis-graceful-stop-run", 2, 2);

        let before_replay = store.stats().await;
        assert_eq!(before_replay.registered_records, 2);
        assert_eq!(before_replay.deliveries, 4);
        assert_eq!(before_replay.launches, 2);
        assert_eq!(before_replay.terminal_records, 2);
        assert_eq!(before_replay.uncertain_records, 0);
        assert_eq!(before_replay.provider_calls.len(), 2);

        let replay_failure = tokio::time::timeout(
            Duration::from_secs(1),
            execute_workflow(store.clone(), replay_queue, "apalis-graceful-stop-run"),
        )
        .await
        .expect("replay of cancelled terminal work hung")
        .expect_err("replay relaunched cancelled terminal work");
        assert_cancelled_step_failure(&replay_failure, "apalis-graceful-stop-run", 1, 2);

        let after_replay = store.stats().await;
        assert_eq!(after_replay.registered_records, 2);
        assert_eq!(after_replay.deliveries, before_replay.deliveries);
        assert_eq!(after_replay.claims, before_replay.claims);
        assert_eq!(after_replay.launches, before_replay.launches);
        assert_eq!(
            after_replay.terminal_records,
            before_replay.terminal_records
        );
        assert_eq!(after_replay.uncertain_records, 0);
        assert_eq!(after_replay.provider_calls, before_replay.provider_calls);
    }

    #[tokio::test]
    async fn worker_loss_returns_claimed_work_to_pending_with_a_new_fence() {
        let store = LocalRunStore::new();
        let call = test_step_call("claimed-recovery-run", "architecture");
        let reference = StepJobRef::for_call(&call);
        let (should_enqueue, _terminal) = store
            .register(reference.clone(), call)
            .await
            .expect("first registration should succeed");
        assert!(should_enqueue);

        let stale_claim = store
            .claim(&reference)
            .await
            .expect("first claim lookup should succeed")
            .expect("pending work should be claimable");
        let recovery = store.reconcile_worker_loss().await;
        assert_eq!(recovery.retryable, vec![reference.clone()]);
        assert!(recovery.uncertain.is_empty());
        assert!(
            store.launch(&stale_claim).await.is_err(),
            "the lost worker retained a valid launch fence"
        );

        let replacement_claim = store
            .claim(&reference)
            .await
            .expect("replacement claim lookup should succeed")
            .expect("recovered work should be claimable");
        assert_ne!(replacement_claim.epoch, stale_claim.epoch);
        let replacement_call = store
            .launch(&replacement_claim)
            .await
            .expect("the replacement claim should own the call");
        assert_eq!(replacement_call.step_id, step_id("architecture"));
    }

    #[tokio::test]
    async fn replay_rejects_a_changed_invocation_for_the_same_identity() {
        let store = LocalRunStore::new();
        let call = test_step_call("identity-collision-run", "architecture");
        let reference = StepJobRef::for_call(&call);
        store
            .register(reference.clone(), call)
            .await
            .expect("first registration should succeed");

        let mut conflicting = test_step_call("identity-collision-run", "architecture");
        conflicting.input = Arc::new(ReviewRequest {
            repository: "tower-agent".to_owned(),
            objective: "silently changed objective".to_owned(),
        });
        let error = match store.register(reference.clone(), conflicting).await {
            Ok(_) => panic!("changed input reused an existing logical identity"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(store.stats().await.registered_records, 1);

        let claim = store
            .claim(&reference)
            .await
            .expect("claim lookup should succeed")
            .expect("original call should remain pending");
        let retained = store
            .launch(&claim)
            .await
            .expect("original call should still be launchable");
        assert_eq!(retained.input.objective, "test worker-loss reconciliation");
    }

    fn assert_uncertain_failure(
        failure: &WorkflowFailure<AgentError, TurnOutcome>,
        minimum_failures: usize,
        maximum_failures: usize,
    ) {
        assert_eq!(failure.run_id().as_str(), "apalis-uncertain-run");
        assert_eq!(failure.workflow_id().as_str(), "repository-review");
        assert_eq!(failure.workflow_version().as_str(), "v1");
        assert!(failure.completed().is_empty());

        let WorkflowFailure::StepsFailed {
            completed,
            failures,
            ..
        } = failure
        else {
            panic!("unexpected failure: {failure:?}");
        };
        assert!(completed.is_empty());
        assert!(
            (minimum_failures..=maximum_failures).contains(&failures.len()),
            "unexpected failure set: {failure:?}"
        );
        for settled in failures {
            assert!(
                settled.step_id == step_id("architecture")
                    || settled.step_id == step_id("verification")
            );
            assert_uncertain_error(&settled.error);
        }
    }

    fn assert_uncertain_error(error: &AgentError) {
        assert_eq!(error.kind, ErrorKind::Internal);
        assert_eq!(error.phase, FailurePhase::Settlement);
        assert_eq!(error.effects, EffectState::Possible);
        assert!(error.message.contains("uncertain"));
        assert!(error.evidence.is_none());
        assert!(error.cause.is_none());
    }

    fn assert_cancelled_step_failure(
        failure: &WorkflowFailure<AgentError, TurnOutcome>,
        run_id: &str,
        minimum_failures: usize,
        maximum_failures: usize,
    ) {
        assert_eq!(failure.run_id().as_str(), run_id);
        assert!(failure.completed().is_empty());
        let WorkflowFailure::StepsFailed { failures, .. } = failure else {
            panic!("unexpected failure: {failure:?}");
        };
        assert!((minimum_failures..=maximum_failures).contains(&failures.len()));
        for settled in failures {
            assert_eq!(settled.error.kind, ErrorKind::Cancelled);
            assert_eq!(settled.error.phase, FailurePhase::Running);
            assert_eq!(settled.error.effects, EffectState::None);
        }
    }

    fn test_step_call(run_id: &str, id: &str) -> LocalStepCall {
        StepCall {
            run_id: WorkflowRunId::new(run_id).expect("test run id is valid"),
            workflow_id: WorkflowId::new("repository-review").expect("test workflow id is valid"),
            workflow_version: WorkflowVersion::new("v1").expect("test workflow version is valid"),
            step_id: step_id(id),
            input: Arc::new(ReviewRequest {
                repository: "tower-agent".to_owned(),
                objective: "test worker-loss reconciliation".to_owned(),
            }),
            job: ReviewJob::Architecture(FakeOptions::default()),
            dependencies: BTreeMap::new(),
            cancellation: CancellationToken::new(),
            deadline: None,
        }
    }
}
