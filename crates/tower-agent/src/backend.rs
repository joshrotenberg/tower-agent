//! The backend seam: the one place a model backend lives.
//!
//! A [`Backend`] takes a resolved [`Params`] and runs it. The core depends on no
//! concrete backend; reference impls (claude, later codex) live in their own
//! crates and drop in here. [`StubBackend`] runs no model: it echoes the
//! resolved parameters, which is enough to exercise the whole server without a
//! live model.

use async_trait::async_trait;
use serde::Serialize;

use crate::params::Params;

/// A backend failure, surfaced to the caller as a tool error.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct BackendError(pub String);

impl BackendError {
    pub fn new(msg: impl Into<String>) -> Self {
        BackendError(msg.into())
    }
}

/// What a backend returns from a run.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Outcome {
    /// The model's output.
    pub text: String,
    /// The backend's session id for this run, if it keeps one (for resume).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
}

/// The one seam where a model backend lives.
#[async_trait]
pub trait Backend: Send + Sync {
    async fn run(&self, params: &Params) -> Result<Outcome, BackendError>;
}

/// A backend that runs no model: it echoes the resolved parameters as JSON.
/// Useful as a dry run (see exactly what a call resolves to) and for testing the
/// server without a live model.
pub struct StubBackend;

#[async_trait]
impl Backend for StubBackend {
    async fn run(&self, params: &Params) -> Result<Outcome, BackendError> {
        let text = serde_json::to_string_pretty(params)
            .map_err(|e| BackendError::new(format!("serialize params: {e}")))?;
        Ok(Outcome {
            text,
            session: params.session.clone(),
        })
    }
}
