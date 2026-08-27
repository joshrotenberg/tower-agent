#![warn(missing_docs)]

//! Experimental, backend-neutral workflows over finite Tower services.
//!
//! A [`WorkflowDefinition`] owns stable identities, dependency topology, and
//! opaque host jobs. It does not own configuration parsing, provider options,
//! persistence, retry, sessions, or queue policy. [`WorkflowService`] is a
//! deliberately non-durable reference runner: it calls one host-supplied Tower
//! service for each ready step and never retries a call.
//!
//! Mechanical work such as Git or GitHub operations is an ordinary host
//! service behind the same dispatcher; this crate defines no mechanical trait
//! and no subprocess runner. `docs/mechanical-steps.md` records that boundary
//! and why running arbitrary commands is deferred rather than added.
//!
//! The job, workflow input, and step output are generic. An application can use
//! a typed enum for heterogeneous agent and mechanical work, or an opaque job
//! reference that a durable host resolves immediately before execution. This
//! crate never serializes [`tower_agent::AgentRequest`], provider options,
//! cancellation tokens, deadlines, or provider session handles.

mod agent;
mod definition;
mod execution;
mod id;

pub use agent::AgentStepService;
pub use definition::{
    DagBuilder, PipelineBuilder, StepDefinition, StepSpec, WorkflowDefinition,
    WorkflowDefinitionError,
};
pub use execution::{
    BoxStepService, StepCall, StepFailure, WorkflowContext, WorkflowFailure, WorkflowOutcome,
    WorkflowRequest, WorkflowService,
};
pub use id::{InvalidIdentifier, StepId, WorkflowId, WorkflowRunId, WorkflowVersion};
