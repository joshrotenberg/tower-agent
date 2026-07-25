//! Scheduling: fire an agent's prompt on a cron cadence.
//!
//! An agent with a `schedule` gets one background task that computes the next
//! occurrence, sleeps, then runs the agent's `schedule_prompt`. A scheduled tick
//! is just another way to call the atom. Scheduled runs reuse a session, so an
//! agent accumulates memory across ticks. Times are UTC.

use std::time::Duration;

use croner::Cron;
use croner::parser::{CronParser, Seconds};
use tokio::task::JoinHandle;

use crate::mcp::Server;
use crate::params::Call;

/// A running scheduler: one task per scheduled agent. Dropping or aborting it
/// stops the ticks.
pub struct SchedulerHandle {
    tasks: Vec<JoinHandle<()>>,
}

impl SchedulerHandle {
    /// Stop all scheduled tasks.
    pub fn abort(&self) {
        for task in &self.tasks {
            task.abort();
        }
    }

    /// How many agents are scheduled.
    pub fn len(&self) -> usize {
        self.tasks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }
}

impl Drop for SchedulerHandle {
    fn drop(&mut self) {
        self.abort();
    }
}

/// A bad cron expression on a scheduled agent.
#[derive(Debug, thiserror::Error)]
#[error("agent {agent}: invalid cron {expr:?}: {source}")]
pub struct ScheduleError {
    pub agent: String,
    pub expr: String,
    #[source]
    pub source: croner::errors::CronError,
}

/// Parse a cron expression, allowing an optional leading seconds field.
pub(crate) fn parse_cron(expr: &str) -> Result<Cron, croner::errors::CronError> {
    CronParser::builder()
        .seconds(Seconds::Optional)
        .build()
        .parse(expr)
}

impl Server {
    /// Start the scheduler: one task per scheduled agent. Fails fast on a bad
    /// cron expression. The returned handle stops the ticks when dropped.
    pub fn spawn_scheduler(&self) -> Result<SchedulerHandle, ScheduleError> {
        let mut tasks = Vec::new();
        for spec in self.scheduled_agents() {
            let cron = parse_cron(&spec.schedule).map_err(|source| ScheduleError {
                agent: spec.name.clone(),
                expr: spec.schedule.clone(),
                source,
            })?;
            let server = self.clone();
            tasks.push(tokio::spawn(run_schedule(
                server,
                spec.name,
                cron,
                spec.prompt,
            )));
        }
        Ok(SchedulerHandle { tasks })
    }
}

/// The per-agent loop: wait for the next occurrence, fire, repeat, keeping the
/// session so ticks share memory.
async fn run_schedule(server: Server, agent: String, cron: Cron, prompt: String) {
    let mut session: Option<String> = None;
    loop {
        let now = chrono::Utc::now();
        let Ok(next) = cron.find_next_occurrence(&now, false) else {
            tracing::warn!(agent = %agent, "no next occurrence; stopping schedule");
            break;
        };
        let wait = (next - now).to_std().unwrap_or(Duration::ZERO);
        tokio::time::sleep(wait).await;

        let call = Call {
            prompt: prompt.clone(),
            agent: Some(agent.clone()),
            session: session.clone(),
            ..Default::default()
        };
        match server.run(call).await {
            Ok(outcome) => {
                session = outcome.session;
                tracing::info!(agent = %agent, "scheduled run");
            }
            Err(e) => tracing::warn!(agent = %agent, error = %e, "scheduled run failed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_five_and_six_field_crons() {
        assert!(parse_cron("0 */6 * * *").is_ok(), "5-field");
        assert!(parse_cron("*/30 * * * * *").is_ok(), "6-field with seconds");
        assert!(parse_cron("not a cron").is_err());
    }

    #[test]
    fn next_occurrence_is_deterministic() {
        use chrono::{TimeZone, Utc};
        let cron = parse_cron("0 0 * * *").unwrap(); // midnight daily
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 10, 0, 0).unwrap();
        let next = cron.find_next_occurrence(&now, false).unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2026, 1, 2, 0, 0, 0).unwrap());
    }

    fn server_with_ticker(schedule: &str) -> Server {
        use std::sync::Arc;
        let config = crate::Config::parse(&format!(
            "[agents.ticker]\nsystem = \"tick\"\nschedule = \"{schedule}\"\nschedule_prompt = \"do the thing\"\n"
        ))
        .unwrap();
        Server::new(config, Arc::new(crate::StubBackend))
    }

    #[tokio::test]
    async fn tick_runs_the_agents_schedule_prompt() {
        let server = server_with_ticker("0 0 * * *");
        let out = server.tick("ticker", None).await.unwrap();
        // The stub echoes the resolved params as JSON, so the prompt is visible.
        assert!(out.text.contains("do the thing"), "{}", out.text);
        assert_eq!(out.session.as_deref(), Some("s1"));
    }

    #[tokio::test]
    async fn spawn_scheduler_rejects_a_bad_cron() {
        let server = server_with_ticker("nonsense");
        assert!(server.spawn_scheduler().is_err());
    }

    #[tokio::test]
    async fn scheduler_fires_on_cadence() {
        let server = server_with_ticker("* * * * * *"); // every second
        let handle = server.spawn_scheduler().unwrap();
        assert_eq!(handle.len(), 1);
        tokio::time::sleep(Duration::from_millis(2500)).await;
        handle.abort();

        let sessions = server.sessions().list();
        assert_eq!(sessions.len(), 1, "the ticker created a session");
        assert!(sessions[0].turns >= 1, "at least one tick fired");
    }
}
