use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt,
    future::Future,
    marker::PhantomData,
    num::NonZeroUsize,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Instant,
};

use futures_util::{StreamExt, stream::FuturesUnordered};
use tower::{Service, ServiceExt, util::BoxCloneSyncService};
use tower_agent::{CallContext, CancellationToken};

use crate::{StepId, WorkflowDefinition, WorkflowId, WorkflowRunId, WorkflowVersion};

/// A cloneable, type-erased dispatcher for one application's concrete step types.
///
/// [`WorkflowService`] clones its dispatcher for independently ready steps. A
/// boxed dispatcher must therefore keep shared admission, rate, budget, or
/// provider state behind its clones when those policies are intended to apply
/// across a workflow or across concurrent workflow runs.
pub type BoxStepService<I, J, O, E> = BoxCloneSyncService<StepCall<I, J, O>, O, E>;

/// Host-local context for one in-memory workflow execution.
///
/// Its cancellation token and [`Instant`] deadline are process-local controls,
/// not portable workflow data. Durable hosts should persist their own absolute
/// deadline and cancellation state and reconstruct this context at dispatch.
#[derive(Clone, Debug)]
pub struct WorkflowContext {
    run_id: WorkflowRunId,
    cancellation: CancellationToken,
    deadline: Option<Instant>,
}

impl WorkflowContext {
    /// Construct context for a run with a fresh, uncancelled token and no deadline.
    pub fn new(run_id: WorkflowRunId) -> Self {
        Self {
            run_id,
            cancellation: CancellationToken::new(),
            deadline: None,
        }
    }

    /// Return the host-defined identity of this workflow run.
    pub fn run_id(&self) -> &WorkflowRunId {
        &self.run_id
    }

    /// Borrow the caller-owned cancellation token for this run.
    ///
    /// The runner creates a child token for its steps. Internal failure or
    /// deadline cancellation reaches those children without cancelling this
    /// caller-owned token.
    pub fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }

    /// Replace the run's caller-owned cancellation token.
    pub fn with_cancellation(mut self, cancellation: CancellationToken) -> Self {
        self.cancellation = cancellation;
        self
    }

    /// Return the optional host-local absolute deadline.
    pub const fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    /// Set a host-local absolute deadline for the whole workflow run.
    ///
    /// The same deadline is propagated to every called step. An [`Instant`]
    /// must never be persisted or transferred between processes.
    pub fn with_deadline(mut self, deadline: Instant) -> Self {
        self.deadline = Some(deadline);
        self
    }
}

/// One invocation of a validated workflow definition.
#[derive(Debug)]
pub struct WorkflowRequest<I, J> {
    /// Host-local controls and the host-defined identity for this run.
    pub context: WorkflowContext,
    /// The validated workflow shape and opaque jobs to execute.
    pub definition: WorkflowDefinition<J>,
    /// Application input shared by every called step in this run.
    pub input: I,
}

impl<I, J> WorkflowRequest<I, J> {
    /// Combine run context, a validated definition, and application input.
    pub fn new(context: WorkflowContext, definition: WorkflowDefinition<J>, input: I) -> Self {
        Self {
            context,
            definition,
            input,
        }
    }
}

/// The owned request presented to the host-supplied step dispatcher.
///
/// The workflow input and successful dependency outputs are [`Arc`]-shared so
/// concurrently executing branches can read them without requiring `I` or `O`
/// to implement [`Clone`]. Only direct dependency outputs are included.
#[derive(Debug)]
pub struct StepCall<I, J, O> {
    /// The host-defined identity of this workflow run.
    pub run_id: WorkflowRunId,
    /// The stable identity of the workflow definition.
    pub workflow_id: WorkflowId,
    /// The host-defined version of the workflow definition.
    pub workflow_version: WorkflowVersion,
    /// The identity of the step being dispatched.
    pub step_id: StepId,
    /// Shared immutable input for the whole workflow run.
    pub input: Arc<I>,
    /// The opaque host-owned job associated with this step.
    pub job: J,
    /// Successful outputs from direct dependencies only, ordered by step id.
    pub dependencies: BTreeMap<StepId, Arc<O>>,
    /// A run-child token the dispatcher and operation must observe cooperatively.
    pub cancellation: CancellationToken,
    /// The workflow's optional host-local absolute deadline.
    ///
    /// This [`Instant`] is valid only in the current process and must not cross
    /// a durable transport boundary.
    pub deadline: Option<Instant>,
}

impl<I, J, O> StepCall<I, J, O> {
    /// Construct the local Tower Agent context for this attempt.
    ///
    /// A host dispatcher may add an event observer or preassigned provider
    /// session to the returned context before constructing an `AgentRequest`.
    pub fn agent_context(&self) -> CallContext {
        let context = CallContext::new().with_cancellation(self.cancellation.clone());
        match self.deadline {
            Some(deadline) => context.with_deadline(deadline),
            None => context,
        }
    }
}

/// Successful terminal state for a non-durable workflow execution.
#[derive(Clone, Debug)]
pub struct WorkflowOutcome<O> {
    /// The host-defined identity of the completed run.
    pub run_id: WorkflowRunId,
    /// The stable identity of the completed workflow.
    pub workflow_id: WorkflowId,
    /// The host-defined version of the completed workflow.
    pub workflow_version: WorkflowVersion,
    /// Every successful step output, ordered by step id.
    ///
    /// Values are [`Arc`]-shared with dependency calls and `leaf_outputs`.
    pub outputs: BTreeMap<StepId, Arc<O>>,
    /// Successful outputs for steps without successors, ordered by step id.
    ///
    /// These entries share the same [`Arc`] allocations as `outputs`.
    pub leaf_outputs: BTreeMap<StepId, Arc<O>>,
}

/// One settled step failure. The dispatcher's concrete error is preserved.
#[derive(Debug)]
pub struct StepFailure<E> {
    /// The identity of the step that failed.
    pub step_id: StepId,
    /// The concrete error returned by the host dispatcher.
    pub error: E,
}

/// Terminal failure from the non-durable reference runner.
#[derive(Debug)]
pub enum WorkflowFailure<E, O> {
    /// The caller's run cancellation was observed.
    ///
    /// Already-called steps are drained before this failure is returned.
    Cancelled {
        /// The host-defined identity of the cancelled run.
        run_id: WorkflowRunId,
        /// The stable identity of the workflow.
        workflow_id: WorkflowId,
        /// The host-defined workflow version.
        workflow_version: WorkflowVersion,
        /// Successful outputs from already-called steps, ordered by step id.
        completed: BTreeMap<StepId, Arc<O>>,
        /// Errors produced while already-called steps were being drained.
        settled_failures: Vec<StepFailure<E>>,
    },
    /// The workflow's host-local absolute deadline was observed.
    ///
    /// Already-called steps receive cancellation and are drained before this
    /// failure is returned.
    DeadlineExceeded {
        /// The host-defined identity of the expired run.
        run_id: WorkflowRunId,
        /// The stable identity of the workflow.
        workflow_id: WorkflowId,
        /// The host-defined workflow version.
        workflow_version: WorkflowVersion,
        /// Successful outputs from already-called steps, ordered by step id.
        completed: BTreeMap<StepId, Arc<O>>,
        /// Errors produced while already-called steps were being drained.
        settled_failures: Vec<StepFailure<E>>,
    },
    /// At least one called step returned an error.
    ///
    /// No descendant is called after failure is latched. Already-called
    /// siblings receive cancellation and are drained before return.
    StepsFailed {
        /// The host-defined identity of the failed run.
        run_id: WorkflowRunId,
        /// The stable identity of the workflow.
        workflow_id: WorkflowId,
        /// The host-defined workflow version.
        workflow_version: WorkflowVersion,
        /// Successful outputs from already-called steps, ordered by step id.
        completed: BTreeMap<StepId, Arc<O>>,
        /// All observed called-step errors, ordered by step id.
        failures: Vec<StepFailure<E>>,
    },
    /// The scheduler stopped without a more specific reason before every step completed.
    Incomplete {
        /// The host-defined identity of the incomplete run.
        run_id: WorkflowRunId,
        /// The stable identity of the workflow.
        workflow_id: WorkflowId,
        /// The host-defined workflow version.
        workflow_version: WorkflowVersion,
        /// Successful outputs settled before the scheduler stopped, ordered by step id.
        completed: BTreeMap<StepId, Arc<O>>,
        /// Steps without a successful output, ordered by step id.
        pending: Vec<StepId>,
    },
}

impl<E, O> WorkflowFailure<E, O> {
    /// Return the host-defined identity of the failed run.
    pub fn run_id(&self) -> &WorkflowRunId {
        match self {
            Self::Cancelled { run_id, .. }
            | Self::DeadlineExceeded { run_id, .. }
            | Self::StepsFailed { run_id, .. }
            | Self::Incomplete { run_id, .. } => run_id,
        }
    }

    /// Return successful outputs settled before failure, ordered by step id.
    ///
    /// Output values are [`Arc`]-shared with any dependency calls made during
    /// this run.
    pub fn completed(&self) -> &BTreeMap<StepId, Arc<O>> {
        match self {
            Self::Cancelled { completed, .. }
            | Self::DeadlineExceeded { completed, .. }
            | Self::StepsFailed { completed, .. }
            | Self::Incomplete { completed, .. } => completed,
        }
    }

    /// Return the stable identity of the failed workflow.
    pub fn workflow_id(&self) -> &WorkflowId {
        match self {
            Self::Cancelled { workflow_id, .. }
            | Self::DeadlineExceeded { workflow_id, .. }
            | Self::StepsFailed { workflow_id, .. }
            | Self::Incomplete { workflow_id, .. } => workflow_id,
        }
    }

    /// Return the host-defined version of the failed workflow.
    pub fn workflow_version(&self) -> &WorkflowVersion {
        match self {
            Self::Cancelled {
                workflow_version, ..
            }
            | Self::DeadlineExceeded {
                workflow_version, ..
            }
            | Self::StepsFailed {
                workflow_version, ..
            }
            | Self::Incomplete {
                workflow_version, ..
            } => workflow_version,
        }
    }
}

impl<E, O> fmt::Display for WorkflowFailure<E, O> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled {
                run_id,
                workflow_id,
                workflow_version,
                ..
            } => {
                write!(
                    formatter,
                    "workflow `{workflow_id}` version `{workflow_version}` run `{run_id}` was cancelled"
                )
            }
            Self::DeadlineExceeded {
                run_id,
                workflow_id,
                workflow_version,
                ..
            } => {
                write!(
                    formatter,
                    "workflow `{workflow_id}` version `{workflow_version}` run `{run_id}` exceeded its deadline"
                )
            }
            Self::StepsFailed {
                run_id,
                workflow_id,
                workflow_version,
                failures,
                ..
            } => write!(
                formatter,
                "workflow `{workflow_id}` version `{workflow_version}` run `{run_id}` failed in {} step(s)",
                failures.len()
            ),
            Self::Incomplete {
                run_id,
                workflow_id,
                workflow_version,
                pending,
                ..
            } => write!(
                formatter,
                "workflow `{workflow_id}` version `{workflow_version}` run `{run_id}` stopped with {} pending step(s)",
                pending.len()
            ),
        }
    }
}

impl<E, O> Error for WorkflowFailure<E, O>
where
    E: fmt::Debug,
    O: fmt::Debug,
{
}

/// A non-durable reference runner over one host-supplied Tower dispatcher.
///
/// The runner performs no retries and installs no per-step timeout. It does
/// enforce the workflow's absolute deadline cooperatively: expiry signals the
/// run-local cancellation token, prevents further calls once observed, and then
/// waits for every called step to settle. A dispatcher still has to observe
/// cancellation; an effectful call that ignores it can keep the workflow
/// pending indefinitely.
///
/// Ready steps are admitted in stable step-id order whenever bounded capacity
/// is available. The scheduler is work-conserving: an unrelated slow branch
/// does not hold back a newly ready branch. Concurrent completion timing can
/// therefore affect launch timing, but not graph ordering or ordered output
/// maps. Once a step failure is observed, the runner signals run-local
/// cancellation, starts no further calls, and drains its already-called
/// siblings. Dropping the workflow future can still drop dispatcher futures;
/// provider services should already carry Tower Agent's supervision layer when
/// settlement must outlive the workflow caller.
///
/// The concurrency bound applies independently to each workflow invocation. It
/// does not limit concurrent workflow runs or aggregate provider usage. The
/// service clones its dispatcher for every admitted step; dispatcher clones
/// must therefore share any admission, rate, budget, session, or provider state
/// whose policy is intended to span steps or runs.
pub struct WorkflowService<S, O, E> {
    dispatcher: S,
    max_concurrency: NonZeroUsize,
    output: PhantomData<fn() -> (O, E)>,
}

impl<S, O, E> Clone for WorkflowService<S, O, E>
where
    S: Clone,
{
    fn clone(&self) -> Self {
        Self {
            dispatcher: self.dispatcher.clone(),
            max_concurrency: self.max_concurrency,
            output: PhantomData,
        }
    }
}

impl<S, O, E> fmt::Debug for WorkflowService<S, O, E>
where
    S: fmt::Debug,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkflowService")
            .field("dispatcher", &self.dispatcher)
            .field("max_concurrency", &self.max_concurrency)
            .finish()
    }
}

impl<S, O, E> WorkflowService<S, O, E> {
    /// Construct a conservative sequential runner.
    pub fn new(dispatcher: S) -> Self {
        Self {
            dispatcher,
            max_concurrency: NonZeroUsize::MIN,
            output: PhantomData,
        }
    }

    /// Bound concurrently executing ready steps within each workflow run.
    ///
    /// This is a per-invocation scheduler bound. Cross-run or provider-wide
    /// admission belongs in a shared dispatcher layer or around this service.
    pub fn with_max_concurrency(mut self, max_concurrency: NonZeroUsize) -> Self {
        self.max_concurrency = max_concurrency;
        self
    }

    /// Return the maximum number of called or readiness-pending steps per run.
    pub const fn max_concurrency(&self) -> NonZeroUsize {
        self.max_concurrency
    }

    /// Borrow the dispatcher cloned for ready steps.
    ///
    /// Clones must share any state backing policies that apply across steps or
    /// concurrent runs; cloning must not accidentally create independent
    /// provider capacity boundaries.
    pub fn dispatcher(&self) -> &S {
        &self.dispatcher
    }
}

impl<S, I, J, O, E> Service<WorkflowRequest<I, J>> for WorkflowService<S, O, E>
where
    S: Service<StepCall<I, J, O>, Response = O, Error = E> + Clone + Send + 'static,
    S::Future: Send + 'static,
    I: Send + Sync + 'static,
    J: Clone + Send + 'static,
    O: Send + Sync + 'static,
    E: Send + 'static,
{
    type Response = WorkflowOutcome<O>;
    type Error = WorkflowFailure<E, O>;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // Per-provider readiness is request-dependent and belongs to the host
        // dispatcher. Run-level admission can be layered around this service.
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: WorkflowRequest<I, J>) -> Self::Future {
        let dispatcher = self.dispatcher.clone();
        let max_concurrency = self.max_concurrency.get();
        Box::pin(async move { execute(dispatcher, max_concurrency, request).await })
    }
}

enum StepSettlement<O, E> {
    /// Cancellation won while the cloned dispatcher was still becoming ready,
    /// so its `call` method was never invoked.
    NotCalled,
    Settled(Result<O, E>),
}

type InFlight<O, E> =
    Pin<Box<dyn Future<Output = (StepId, StepSettlement<O, E>)> + Send + 'static>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StopReason {
    ExternalCancellation,
    Deadline,
    StepFailure,
}

async fn dispatch_step<S, I, J, O, E>(
    mut service: S,
    call: StepCall<I, J, O>,
    cancellation: CancellationToken,
) -> StepSettlement<O, E>
where
    S: Service<StepCall<I, J, O>, Response = O, Error = E>,
    S::Future: Send + 'static,
{
    let ready = tokio::select! {
        biased;
        () = cancellation.cancelled() => return StepSettlement::NotCalled,
        ready = service.ready() => ready,
    };
    let ready = match ready {
        Ok(ready) => ready,
        Err(error) => return StepSettlement::Settled(Err(error)),
    };

    // A cancellation wakeup and readiness can race. Do not cross the call
    // boundary once cancellation has been observed.
    if cancellation.is_cancelled() {
        return StepSettlement::NotCalled;
    }

    // Once `call` has happened, deliberately await settlement without racing
    // cancellation. The cancellation token is part of `call` itself.
    StepSettlement::Settled(ready.call(call).await)
}

async fn execute<S, I, J, O, E>(
    dispatcher: S,
    max_concurrency: usize,
    request: WorkflowRequest<I, J>,
) -> Result<WorkflowOutcome<O>, WorkflowFailure<E, O>>
where
    S: Service<StepCall<I, J, O>, Response = O, Error = E> + Clone + Send + 'static,
    S::Future: Send + 'static,
    I: Send + Sync + 'static,
    J: Clone + Send + 'static,
    O: Send + Sync + 'static,
    E: Send + 'static,
{
    let WorkflowRequest {
        context,
        definition,
        input,
    } = request;
    let run_id = context.run_id.clone();
    let workflow_id = definition.id().clone();
    let workflow_version = definition.version().clone();
    let input = Arc::new(input);
    let mut remaining_dependencies = definition
        .steps()
        .map(|step| (step.id().clone(), step.needs().len()))
        .collect::<BTreeMap<_, _>>();
    let mut outgoing = definition
        .steps()
        .map(|step| (step.id().clone(), Vec::new()))
        .collect::<BTreeMap<_, _>>();
    for step in definition.steps() {
        for dependency in step.needs() {
            outgoing
                .get_mut(dependency)
                .expect("validated dependency must exist")
                .push(step.id().clone());
        }
    }
    for successors in outgoing.values_mut() {
        successors.sort();
    }

    let mut ready = remaining_dependencies
        .iter()
        .filter_map(|(step_id, count)| (*count == 0).then_some(step_id.clone()))
        .collect::<BTreeSet<_>>();
    let mut in_flight = FuturesUnordered::<InFlight<O, E>>::new();
    let mut outputs = BTreeMap::<StepId, Arc<O>>::new();
    let mut failures = Vec::<StepFailure<E>>::new();
    // Internal cancellation is a child of the caller's token. Stopping this
    // run therefore reaches every step without mutating caller-owned state.
    let run_cancellation = context.cancellation.child_token();
    let mut stop_reason = if context.cancellation.is_cancelled() {
        Some(StopReason::ExternalCancellation)
    } else if context
        .deadline
        .is_some_and(|deadline| Instant::now() >= deadline)
    {
        Some(StopReason::Deadline)
    } else {
        None
    };
    if stop_reason.is_some() {
        run_cancellation.cancel();
    }

    loop {
        while stop_reason.is_none() && in_flight.len() < max_concurrency {
            if context.cancellation.is_cancelled() {
                stop_reason = Some(StopReason::ExternalCancellation);
                run_cancellation.cancel();
                break;
            }
            if context
                .deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
            {
                stop_reason = Some(StopReason::Deadline);
                run_cancellation.cancel();
                break;
            }
            let Some(step_id) = ready.pop_first() else {
                break;
            };
            let step = definition
                .step(&step_id)
                .expect("ready step must exist in a validated definition");
            let dependencies = step
                .needs()
                .iter()
                .map(|dependency| {
                    let output = outputs
                        .get(dependency)
                        .expect("ready step dependencies must have completed")
                        .clone();
                    (dependency.clone(), output)
                })
                .collect();
            let step_cancellation = run_cancellation.child_token();
            let call = StepCall {
                run_id: run_id.clone(),
                workflow_id: workflow_id.clone(),
                workflow_version: workflow_version.clone(),
                step_id: step_id.clone(),
                input: Arc::clone(&input),
                job: step.job().clone(),
                dependencies,
                cancellation: step_cancellation.clone(),
                deadline: context.deadline,
            };
            let service = dispatcher.clone();
            in_flight.push(Box::pin(async move {
                let settlement = dispatch_step(service, call, step_cancellation).await;
                (step_id, settlement)
            }));
        }

        if in_flight.is_empty() {
            break;
        }

        let completed = if stop_reason.is_some() {
            in_flight.next().await
        } else {
            match context.deadline {
                Some(deadline) => {
                    tokio::select! {
                        biased;
                        _ = context.cancellation.cancelled() => {
                            stop_reason = Some(StopReason::ExternalCancellation);
                            run_cancellation.cancel();
                            None
                        }
                        _ = tokio::time::sleep_until(deadline.into()) => {
                            stop_reason = Some(StopReason::Deadline);
                            run_cancellation.cancel();
                            None
                        }
                        completed = in_flight.next() => completed,
                    }
                }
                None => {
                    tokio::select! {
                        biased;
                        _ = context.cancellation.cancelled() => {
                            stop_reason = Some(StopReason::ExternalCancellation);
                            run_cancellation.cancel();
                            None
                        }
                        completed = in_flight.next() => completed,
                    }
                }
            }
        };
        let Some((step_id, settlement)) = completed else {
            continue;
        };

        match settlement {
            StepSettlement::NotCalled => {}
            StepSettlement::Settled(Ok(output)) => {
                outputs.insert(step_id.clone(), Arc::new(output));
                if stop_reason.is_none() {
                    for successor in &outgoing[&step_id] {
                        let remaining = remaining_dependencies
                            .get_mut(successor)
                            .expect("validated successor must exist");
                        *remaining -= 1;
                        if *remaining == 0 {
                            ready.insert(successor.clone());
                        }
                    }
                }
            }
            StepSettlement::Settled(Err(error)) => {
                failures.push(StepFailure { step_id, error });
                if stop_reason.is_none() {
                    stop_reason = Some(StopReason::StepFailure);
                    run_cancellation.cancel();
                }
            }
        }
    }

    failures.sort_by(|left, right| left.step_id.cmp(&right.step_id));
    match stop_reason {
        Some(StopReason::ExternalCancellation) => {
            return Err(WorkflowFailure::Cancelled {
                run_id,
                workflow_id,
                workflow_version,
                completed: outputs,
                settled_failures: failures,
            });
        }
        Some(StopReason::Deadline) => {
            return Err(WorkflowFailure::DeadlineExceeded {
                run_id,
                workflow_id,
                workflow_version,
                completed: outputs,
                settled_failures: failures,
            });
        }
        Some(StopReason::StepFailure) => {
            return Err(WorkflowFailure::StepsFailed {
                run_id,
                workflow_id,
                workflow_version,
                completed: outputs,
                failures,
            });
        }
        None => {}
    }
    if outputs.len() != definition.steps().len() {
        let pending = definition
            .steps()
            .filter(|step| !outputs.contains_key(step.id()))
            .map(|step| step.id().clone())
            .collect();
        return Err(WorkflowFailure::Incomplete {
            run_id,
            workflow_id,
            workflow_version,
            completed: outputs,
            pending,
        });
    }

    let leaf_outputs = definition
        .leaves()
        .into_iter()
        .map(|step| {
            let output = outputs
                .get(step.id())
                .expect("completed workflow must contain every leaf")
                .clone();
            (step.id().clone(), output)
        })
        .collect();
    Ok(WorkflowOutcome {
        run_id,
        workflow_id,
        workflow_version,
        outputs,
        leaf_outputs,
    })
}
