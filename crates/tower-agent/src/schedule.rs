//! Scheduling: fire an agent's prompt on a cron cadence.
//!
//! An agent with a `schedule` gets one background task that computes the next
//! occurrence, sleeps, then runs the agent's `schedule_prompt`. A scheduled tick
//! is just another way to call the atom. Scheduled runs reuse a session, so an
//! agent accumulates memory across ticks. A schedule runs in the agent's
//! timezone (UTC by default) and can fire once on start (`run_at_start`).

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

/// A bad schedule on an agent (a cron expression or a timezone).
#[derive(Debug, thiserror::Error)]
pub enum ScheduleError {
    #[error("agent {agent}: invalid cron {expr:?}: {source}")]
    Cron {
        agent: String,
        expr: String,
        #[source]
        source: croner::errors::CronError,
    },
    #[error("agent {agent}: unknown timezone {tz:?}")]
    Timezone { agent: String, tz: String },
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
            let cron = parse_cron(&spec.schedule).map_err(|source| ScheduleError::Cron {
                agent: spec.name.clone(),
                expr: spec.schedule.clone(),
                source,
            })?;
            let tz = match &spec.timezone {
                Some(tz) => tz
                    .parse::<chrono_tz::Tz>()
                    .map_err(|_| ScheduleError::Timezone {
                        agent: spec.name.clone(),
                        tz: tz.clone(),
                    })?,
                None => chrono_tz::UTC,
            };
            let server = self.clone();
            tasks.push(tokio::spawn(run_schedule(
                server,
                spec.name,
                cron,
                spec.prompt,
                tz,
                spec.run_at_start,
            )));
        }
        Ok(SchedulerHandle { tasks })
    }
}

/// The per-agent loop: optionally fire once on start, then wait for each next
/// occurrence (in the agent's timezone) and fire, keeping the session so ticks
/// share memory.
async fn run_schedule(
    server: Server,
    agent: String,
    cron: Cron,
    prompt: String,
    tz: chrono_tz::Tz,
    run_at_start: bool,
) {
    let mut session: Option<String> = None;
    if run_at_start {
        session = fire(&server, &agent, &prompt, session).await;
    }
    loop {
        let now = chrono::Utc::now().with_timezone(&tz);
        let Ok(next) = cron.find_next_occurrence(&now, false) else {
            tracing::warn!(agent = %agent, "no next occurrence; stopping schedule");
            break;
        };
        let wait = (next - now).to_std().unwrap_or(Duration::ZERO);
        tokio::time::sleep(wait).await;
        session = fire(&server, &agent, &prompt, session).await;
    }
}

/// Fire one scheduled run; return the session to carry to the next tick (the new
/// one on success, the same one on failure).
async fn fire(
    server: &Server,
    agent: &str,
    prompt: &str,
    session: Option<String>,
) -> Option<String> {
    let call = Call {
        prompt: prompt.to_string(),
        agent: Some(agent.to_string()),
        session: session.clone(),
        ..Default::default()
    };
    match server.run(call).await {
        Ok(outcome) => {
            tracing::info!(agent = %agent, "scheduled run");
            outcome.session
        }
        Err(e) => {
            tracing::warn!(agent = %agent, error = %e, "scheduled run failed");
            session
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

    #[test]
    fn next_occurrence_respects_timezone() {
        use chrono::TimeZone;
        let cron = parse_cron("0 9 * * *").unwrap(); // 9am daily
        let tz: chrono_tz::Tz = "America/New_York".parse().unwrap();
        let now = tz.with_ymd_and_hms(2026, 1, 1, 10, 0, 0).unwrap(); // 10am ET, past 9am
        let next = cron.find_next_occurrence(&now, false).unwrap();
        assert_eq!(next, tz.with_ymd_and_hms(2026, 1, 2, 9, 0, 0).unwrap());
    }

    #[tokio::test]
    async fn spawn_scheduler_rejects_a_bad_timezone() {
        use std::sync::Arc;
        let config = crate::Config::parse(
            "[agents.t]\nsystem = \"x\"\nschedule = \"0 0 * * *\"\ntimezone = \"Nowhere/Nope\"\n",
        )
        .unwrap();
        let server = Server::new(config, Arc::new(crate::StubBackend));
        assert!(matches!(
            server.spawn_scheduler(),
            Err(ScheduleError::Timezone { .. })
        ));
    }

    #[tokio::test]
    async fn run_at_start_fires_immediately() {
        use std::sync::Arc;
        // A far-future cron; run_at_start fires once now anyway.
        let config = crate::Config::parse(
            "[agents.t]\nsystem = \"x\"\nschedule = \"0 0 1 1 *\"\nschedule_prompt = \"go\"\nrun_at_start = true\n",
        )
        .unwrap();
        let server = Server::new(config, Arc::new(crate::StubBackend));
        let handle = server.spawn_scheduler().unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        handle.abort();
        assert!(
            !server.sessions().list().is_empty(),
            "run_at_start should fire once"
        );
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
        assert!(out.reply.contains("do the thing"), "{}", out.reply);
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
