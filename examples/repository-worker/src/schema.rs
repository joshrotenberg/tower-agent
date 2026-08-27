//! The application's own configuration schema.
//!
//! Deliberately application-owned. The workflow crate never sees this: it is
//! handed only compiled definitions. That boundary is what lets the schema
//! version and migrate on its own cadence.
//!
//! What configuration may name is constrained on purpose. A step refers to a
//! host-owned profile by name and supplies a prompt. It cannot express a
//! provider option, a credential, a session handle, a cancellation token, or
//! a deadline: those are process-local or secret, and serializing them into a
//! file is how they leak or go stale.

use std::collections::BTreeMap;

use serde::Deserialize;

/// The only schema version this build understands.
pub const SCHEMA_VERSION: u32 = 1;

/// One workflow file.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowFile {
    /// Refused rather than guessed when it does not match [`SCHEMA_VERSION`],
    /// so an old file fails loudly instead of being misread.
    pub schema_version: u32,
    pub workflow: WorkflowHeader,
    /// Steps in a linear pipeline, or a phase list. Exactly one of `step`,
    /// `phase` must be present.
    #[serde(default)]
    pub step: Vec<StepEntry>,
    /// Phase-oriented sugar. Each phase runs after the previous one
    /// completes, and steps within a phase are independent.
    #[serde(default)]
    pub phase: Vec<PhaseEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowHeader {
    pub id: String,
    pub version: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhaseEntry {
    /// Read for documentation and error messages rather than compiled into
    /// the definition: phases are sugar over dependencies, and the workflow
    /// crate has no phase concept to carry a name into.
    #[allow(dead_code)]
    pub name: String,
    pub step: Vec<StepEntry>,
}

/// One step. `needs` is explicit in a DAG and implied in a pipeline or phase.
///
/// No `deny_unknown_fields` here: serde cannot combine it with `flatten`,
/// and flattening is what lets a step read as one table rather than nesting
/// its job under a sub-table. The tagged enum below still rejects an unknown
/// `kind`, so the case that matters is covered.
#[derive(Debug, Deserialize)]
pub struct StepEntry {
    pub id: String,
    #[serde(default)]
    pub needs: Vec<String>,
    #[serde(flatten)]
    pub job: JobEntry,
}

/// What a step does. Tagged so an unknown kind is a schema error rather than
/// a silently ignored field.
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JobEntry {
    /// An agent turn, configured by a host-owned profile name.
    Agent { profile: String, prompt: String },
    /// A typed in-process operation the host implements.
    Mechanical {
        op: String,
        #[serde(default)]
        args: BTreeMap<String, String>,
    },
}

/// Where something was found, for error reporting a person can act on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Location {
    pub file: String,
    /// A dotted path into the document, such as `phase[1].step[0].needs`.
    pub path: String,
}

impl Location {
    pub fn new(file: &str, path: impl Into<String>) -> Self {
        Self {
            file: file.to_string(),
            path: path.into(),
        }
    }
}

impl std::fmt::Display for Location {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.file, self.path)
    }
}
