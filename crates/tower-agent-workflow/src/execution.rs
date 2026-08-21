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
pub type BoxStepService<I, J, O, E> = BoxCloneSyncService<StepCall<I, J, O>, O, E>;

/// Host-local context for one in-memory workflow execution.
#[derive(Clone, Debug)]
pub struct WorkflowContext {
    run_id: WorkflowRunId,
    cancellation: CancellationToken,
    deadline: Option<Instant>,
}

impl WorkflowContext {
    pub fn new(run_id: WorkflowRunId) -> Self {
        Self {
            run_id,
            cancellation: CancellationToken::new(),
            deadline: None,
        }
    }

    pub fn run_id(&self) -> &WorkflowRunId {
        &self.run_id
    }

    pub fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }

    pub fn with_cancellation(mut self, cancellation: CancellationToken) -> Self {
        self.cancellation = cancellation;
        self
    }

    pub const fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    pub fn with_deadline(mut self, deadline: Instant) -> Self {
        self.deadline = Some(deadline);
        self
    }
}

/// One invocation of a validated workflow definition.
#[derive(Debug)]
pub struct WorkflowRequest<I, J> {
    pub context: WorkflowContext,
    pub definition: WorkflowDefinition<J>,
    pub input: I,
}

impl<I, J> WorkflowRequest<I, J> {
    pub fn new(context: WorkflowContext, definition: WorkflowDefinition<J>, input: I) -> Self {
        Self {
            context,
            definition,
            input,
        }
    }
}

/// The owned request presented to the host-supplied step dispatcher.
#[derive(Debug)]
pub struct StepCall<I, J, O> {
    pub run_id: WorkflowRunId,
    pub workflow_id: WorkflowId,
    pub workflow_version: WorkflowVersion,
    pub step_id: StepId,
    pub input: Arc<I>,
    pub job: J,
    /// Successful outputs from direct dependencies only, ordered by step id.
    pub dependencies: BTreeMap<StepId, Arc<O>>,
    pub cancellation: CancellationToken,
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
    pub run_id: WorkflowRunId,
    pub workflow_id: WorkflowId,
    pub workflow_version: WorkflowVersion,
    pub outputs: BTreeMap<StepId, Arc<O>>,
    pub leaf_outputs: BTreeMap<StepId, Arc<O>>,
}

/// One settled step failure. The dispatcher's concrete error is preserved.
#[derive(Debug)]
pub struct StepFailure<E> {
    pub step_id: StepId,
    pub error: E,
}

/// Terminal failure from the non-durable reference runner.
#[derive(Debug)]
pub enum WorkflowFailure<E, O> {
    Cancelled {
        run_id: WorkflowRunId,
        completed: BTreeMap<StepId, Arc<O>>,
        settled_failures: Vec<StepFailure<E>>,
    },
    DeadlineExceeded {
        run_id: WorkflowRunId,
        completed: BTreeMap<StepId, Arc<O>>,
        settled_failures: Vec<StepFailure<E>>,
    },
    StepsFailed {
        run_id: WorkflowRunId,
        completed: BTreeMap<StepId, Arc<O>>,
        failures: Vec<StepFailure<E>>,
    },
    Incomplete {
        run_id: WorkflowRunId,
        completed: BTreeMap<StepId, Arc<O>>,
        pending: Vec<StepId>,
    },
}

impl<E, O> WorkflowFailure<E, O> {
    pub fn run_id(&self) -> &WorkflowRunId {
        match self {
            Self::Cancelled { run_id, .. }
            | Self::DeadlineExceeded { run_id, .. }
            | Self::StepsFailed { run_id, .. }
            | Self::Incomplete { run_id, .. } => run_id,
        }
    }

    pub fn completed(&self) -> &BTreeMap<StepId, Arc<O>> {
        match self {
            Self::Cancelled { completed, .. }
            | Self::DeadlineExceeded { completed, .. }
            | Self::StepsFailed { completed, .. }
            | Self::Incomplete { completed, .. } => completed,
        }
    }
}

impl<E, O> fmt::Display for WorkflowFailure<E, O> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled { run_id, .. } => {
                write!(formatter, "workflow run `{run_id}` was cancelled")
            }
            Self::DeadlineExceeded { run_id, .. } => {
                write!(formatter, "workflow run `{run_id}` exceeded its deadline")
            }
            Self::StepsFailed {
                run_id, failures, ..
            } => write!(
                formatter,
                "workflow run `{run_id}` failed in {} step(s)",
                failures.len()
            ),
            Self::Incomplete {
                run_id, pending, ..
            } => write!(
                formatter,
                "workflow run `{run_id}` stopped with {} pending step(s)",
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
/// Ready steps are admitted in stable, bounded waves. The next wave is not
/// released until every step in the current wave has settled, so completion
/// races cannot change which later steps are called. A step failure likewise
/// signals run-local cancellation and drains its already-called siblings.
/// Dropping the workflow future can still drop dispatcher futures; provider
/// services should already carry Tower Agent's supervision layer when
/// settlement must outlive the workflow caller.
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
    pub fn with_max_concurrency(mut self, max_concurrency: NonZeroUsize) -> Self {
        self.max_concurrency = max_concurrency;
        self
    }

    pub const fn max_concurrency(&self) -> NonZeroUsize {
        self.max_concurrency
    }

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
        // Stable waves make admission independent of sibling completion order:
        // do not refill capacity until the entire current wave has settled.
        if stop_reason.is_none() && in_flight.is_empty() {
            for _ in 0..max_concurrency {
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
                    workflow_id: definition.id().clone(),
                    workflow_version: definition.version().clone(),
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
                completed: outputs,
                settled_failures: failures,
            });
        }
        Some(StopReason::Deadline) => {
            return Err(WorkflowFailure::DeadlineExceeded {
                run_id,
                completed: outputs,
                settled_failures: failures,
            });
        }
        Some(StopReason::StepFailure) => {
            return Err(WorkflowFailure::StepsFailed {
                run_id,
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
        workflow_id: definition.id().clone(),
        workflow_version: definition.version().clone(),
        outputs,
        leaf_outputs,
    })
}
