//! The run registry: every invocation of the atom is recorded as a run.
//!
//! A run is a first-class, observable record of one turn: which agent, in which
//! session, triggered how (a prompt call, a scheduled tick, a bus fire), its
//! status, timestamps, and a summary. Runs are listable over the MCP `runs`
//! tool, so activity is observable across clients within a server. In-memory and
//! capped; a durable store is a later concern.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;

/// How a run was triggered.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RunKind {
    /// A direct prompt call.
    Invoke,
    /// A scheduled tick.
    Schedule,
    /// A message on a subscribed channel (a bus fire).
    Subscribe,
}

/// Where a run is in its lifecycle.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RunStatus {
    Running,
    Done,
    Failed,
}

/// One recorded run.
#[derive(Debug, Clone, Serialize)]
pub struct Run {
    pub id: String,
    pub agent: Option<String>,
    pub session: Option<String>,
    pub kind: RunKind,
    pub status: RunStatus,
    pub started_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
    /// The outcome summary, or the error message on failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// The run's cost in USD, if the backend reported it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
}

/// An in-memory, capped registry of runs.
pub struct Runs {
    inner: Mutex<VecDeque<Run>>,
    next_id: AtomicU64,
    cap: usize,
}

impl Runs {
    pub fn new() -> Self {
        Runs {
            inner: Mutex::new(VecDeque::new()),
            next_id: AtomicU64::new(0),
            cap: 1000,
        }
    }

    /// Record the start of a run and return its id.
    pub fn start(&self, agent: Option<String>, session: Option<String>, kind: RunKind) -> String {
        let id = format!("r{}", self.next_id.fetch_add(1, Ordering::SeqCst) + 1);
        let run = Run {
            id: id.clone(),
            agent,
            session,
            kind,
            status: RunStatus::Running,
            started_at: now(),
            ended_at: None,
            summary: None,
            cost_usd: None,
        };
        let mut q = self.inner.lock().unwrap();
        q.push_back(run);
        while q.len() > self.cap {
            q.pop_front();
        }
        id
    }

    /// Record the end of a run.
    pub fn finish(
        &self,
        id: &str,
        status: RunStatus,
        summary: Option<String>,
        cost_usd: Option<f64>,
    ) {
        let mut q = self.inner.lock().unwrap();
        if let Some(run) = q.iter_mut().find(|r| r.id == id) {
            run.status = status;
            run.ended_at = Some(now());
            run.summary = summary;
            run.cost_usd = cost_usd;
        }
    }

    /// The most recent runs, newest last, up to `limit`.
    pub fn list(&self, limit: usize) -> Vec<Run> {
        let q = self.inner.lock().unwrap();
        let start = q.len().saturating_sub(limit);
        q.iter().skip(start).cloned().collect()
    }

    /// One run by id.
    pub fn get(&self, id: &str) -> Option<Run> {
        let q = self.inner.lock().unwrap();
        q.iter().find(|r| r.id == id).cloned()
    }
}

impl Default for Runs {
    fn default() -> Self {
        Self::new()
    }
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_finishes_a_run() {
        let runs = Runs::new();
        let id = runs.start(Some("scout".into()), Some("s1".into()), RunKind::Invoke);
        assert_eq!(id, "r1");
        let run = runs.get(&id).unwrap();
        assert!(matches!(run.status, RunStatus::Running));
        assert!(run.ended_at.is_none());

        runs.finish(
            &id,
            RunStatus::Done,
            Some("did the thing".into()),
            Some(0.02),
        );
        let run = runs.get(&id).unwrap();
        assert!(matches!(run.status, RunStatus::Done));
        assert!(run.ended_at.is_some());
        assert_eq!(run.summary.as_deref(), Some("did the thing"));
        assert_eq!(run.cost_usd, Some(0.02));
    }

    #[test]
    fn lists_newest_last() {
        let runs = Runs::new();
        runs.start(None, None, RunKind::Invoke);
        runs.start(None, None, RunKind::Schedule);
        let list = runs.list(10);
        assert_eq!(list.len(), 2);
        assert_eq!(list[1].id, "r2");
    }
}
