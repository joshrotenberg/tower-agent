//! The backend seam: the one place a model backend lives.
//!
//! A [`Backend`] takes a resolved [`Params`] and runs it. The core depends on no
//! concrete backend; reference impls (claude, later codex) live in their own
//! crates and drop in here. [`StubBackend`] runs no model: it echoes the
//! resolved parameters, which is enough to exercise the whole server without a
//! live model.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedSender;

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

/// A message from an agent to a channel. Directed (`to`) and threaded
/// (`reply_to`) fields support agent-to-agent conversation; the routing that
/// consumes them arrives with channels.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Post {
    /// The channel to post on.
    pub channel: String,
    /// The message body.
    pub body: String,
    /// Address one agent directly, so it is reached regardless of subscription.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    /// The id of the message this one answers, so a thread forms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<u64>,
}

/// What a backend returns from a run: a structured result plus the session.
///
/// - `reply` is the answer to whoever called (the work product). For a plain
///   prompt it is the model's text.
/// - `summary` is one line for the operator's log.
/// - `posts` are messages to other agents; empty until an agent participates in
///   the bus.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Outcome {
    pub summary: String,
    pub reply: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub posts: Vec<Post>,
    /// The backend's session id for this run, if it keeps one (for resume).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
}

impl Outcome {
    /// A plain outcome: the reply is the answer, the summary a preview of it, no
    /// posts. Backends use this for a normal, non-structured run.
    pub fn from_reply(reply: impl Into<String>, session: Option<String>) -> Self {
        let reply = reply.into();
        let summary = summarize(&reply);
        Outcome {
            summary,
            reply,
            posts: Vec::new(),
            session,
        }
    }
}

/// A one-line, length-capped summary of a reply.
fn summarize(reply: &str) -> String {
    let line = reply.trim().lines().next().unwrap_or("").trim();
    line.chars().take(120).collect()
}

/// An incremental event emitted while a prompt runs, for callers that opt into
/// streaming. Non-exhaustive: richer events (tool use, turn boundaries) are
/// planned.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Event {
    /// A chunk of assistant text as it is produced.
    TextDelta(String),
    /// A human-readable status line.
    Status(String),
}

/// The one seam where a model backend lives.
#[async_trait]
pub trait Backend: Send + Sync {
    /// Run a prompt to completion and return the outcome.
    async fn run(&self, params: &Params) -> Result<Outcome, BackendError>;

    /// Run a prompt, emitting incremental [`Event`]s to `events` as they occur,
    /// then return the final outcome. The default does not stream: it runs and
    /// returns the outcome, sending no events. Backends that can stream override
    /// this.
    async fn run_streaming(
        &self,
        params: &Params,
        events: UnboundedSender<Event>,
    ) -> Result<Outcome, BackendError> {
        let _ = events;
        self.run(params).await
    }
}

/// A backend that runs no model: it echoes the resolved parameters as JSON.
/// Useful as a dry run (see exactly what a call resolves to) and for testing the
/// server without a live model.
pub struct StubBackend;

#[async_trait]
impl Backend for StubBackend {
    async fn run(&self, params: &Params) -> Result<Outcome, BackendError> {
        let reply = serde_json::to_string_pretty(params)
            .map_err(|e| BackendError::new(format!("serialize params: {e}")))?;
        Ok(Outcome::from_reply(reply, params.session.clone()))
    }
}
