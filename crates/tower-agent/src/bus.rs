//! The bus: agents react to messages on channels they subscribe to.
//!
//! This is the third way to fire an agent, alongside a prompt call and a
//! schedule. An operator (or, later, another agent) broadcasts a message to a
//! channel; every agent subscribed to that channel runs with the message as its
//! prompt, and its reply is recorded back on the channel, visible in the feed.
//!
//! This is the one-hop bus: a broadcast fires subscribers, which react. An
//! agent's own posts re-triggering subscribers (the cascade) and its depth bound
//! are a later slice; kept one-hop here so it is safe without a bound. Execution
//! is serial: one background worker drains a queue.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use serde::Serialize;
use tokio::sync::Notify;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::task::JoinHandle;

use crate::mcp::Server;
use crate::params::Call;

/// A message on a channel. `to` and `reply_to` support directed, threaded
/// conversation; the routing that consumes them arrives with later slices.
#[derive(Debug, Clone, Serialize)]
pub struct Message {
    pub id: u64,
    pub channel: String,
    pub from: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    pub body: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<u64>,
}

/// A capped, in-memory log of bus messages.
struct Feed {
    inner: Mutex<VecDeque<Message>>,
    next_id: AtomicU64,
    cap: usize,
}

impl Feed {
    fn new(cap: usize) -> Self {
        Feed {
            inner: Mutex::new(VecDeque::new()),
            next_id: AtomicU64::new(0),
            cap,
        }
    }

    fn post(
        &self,
        channel: &str,
        from: &str,
        to: Option<String>,
        body: &str,
        reply_to: Option<u64>,
    ) -> Message {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        let msg = Message {
            id,
            channel: channel.to_string(),
            from: from.to_string(),
            to,
            body: body.to_string(),
            reply_to,
        };
        let mut q = self.inner.lock().unwrap();
        q.push_back(msg.clone());
        while q.len() > self.cap {
            q.pop_front();
        }
        msg
    }

    fn recent(&self, channel: Option<&str>, limit: usize) -> Vec<Message> {
        let q = self.inner.lock().unwrap();
        let mut v: Vec<Message> = q
            .iter()
            .filter(|m| channel.is_none_or(|c| m.channel == c))
            .cloned()
            .collect();
        let start = v.len().saturating_sub(limit);
        v.split_off(start)
    }
}

/// Tracks in-flight bus work so a caller can wait for the bus to settle.
#[derive(Clone)]
struct Idle(Arc<(AtomicUsize, Notify)>);

impl Idle {
    fn new() -> Self {
        Idle(Arc::new((AtomicUsize::new(0), Notify::new())))
    }
    fn inc(&self) {
        self.0.0.fetch_add(1, Ordering::SeqCst);
    }
    fn dec(&self) {
        if self.0.0.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.0.1.notify_waiters();
        }
    }
    async fn wait(&self) {
        loop {
            if self.0.0.load(Ordering::SeqCst) == 0 {
                return;
            }
            let notified = self.0.1.notified();
            if self.0.0.load(Ordering::SeqCst) == 0 {
                return;
            }
            notified.await;
        }
    }
}

enum Job {
    Fire { agent: String, message: Message },
}

/// The shared bus state a [`Server`] holds: the feed, the job queue, idle
/// tracking, and the per-agent session used so an agent remembers the
/// conversation across messages.
#[derive(Clone)]
pub struct Bus {
    feed: Arc<Feed>,
    tx: UnboundedSender<Job>,
    idle: Idle,
    rx: Arc<Mutex<Option<UnboundedReceiver<Job>>>>,
    sessions: Arc<Mutex<HashMap<String, String>>>,
}

impl Bus {
    pub fn new() -> Self {
        let (tx, rx) = unbounded_channel();
        Bus {
            feed: Arc::new(Feed::new(500)),
            tx,
            idle: Idle::new(),
            rx: Arc::new(Mutex::new(Some(rx))),
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl Default for Bus {
    fn default() -> Self {
        Self::new()
    }
}

/// A running bus worker. Dropping or aborting it stops processing.
pub struct BusHandle {
    worker: JoinHandle<()>,
}

impl BusHandle {
    pub fn abort(&self) {
        self.worker.abort();
    }
}

impl Drop for BusHandle {
    fn drop(&mut self) {
        self.worker.abort();
    }
}

impl Server {
    /// Post a message to a channel and fire every subscribed agent. The message
    /// is returned immediately; the reactions run on the worker.
    pub fn broadcast(&self, channel: &str, from: &str, body: &str) -> Message {
        let msg = self.bus.feed.post(channel, from, None, body, None);
        for agent in self.config.subscribers(channel) {
            self.bus.idle.inc();
            if self
                .bus
                .tx
                .send(Job::Fire {
                    agent,
                    message: msg.clone(),
                })
                .is_err()
            {
                self.bus.idle.dec();
            }
        }
        msg
    }

    /// Recent messages on the bus (newest last), optionally one channel.
    pub fn feed(&self, channel: Option<&str>, limit: usize) -> Vec<Message> {
        self.bus.feed.recent(channel, limit)
    }

    /// Wait until the bus has no work in flight.
    pub async fn wait_idle(&self) {
        self.bus.idle.wait().await;
    }

    /// Start the bus worker (serial). Call once; the returned handle stops it when
    /// dropped.
    pub fn spawn_bus(&self) -> BusHandle {
        let rx = self
            .bus
            .rx
            .lock()
            .unwrap()
            .take()
            .expect("bus worker already spawned");
        let worker = tokio::spawn(bus_worker(self.clone(), rx));
        BusHandle { worker }
    }
}

async fn bus_worker(server: Server, mut rx: UnboundedReceiver<Job>) {
    while let Some(Job::Fire { agent, message }) = rx.recv().await {
        let session = server.bus.sessions.lock().unwrap().get(&agent).cloned();
        let prompt = format!(
            "Message on channel '{}' from {}:\n\n{}",
            message.channel, message.from, message.body
        );
        let call = Call {
            prompt,
            agent: Some(agent.clone()),
            session,
            ..Default::default()
        };
        match server.run(call).await {
            Ok(outcome) => {
                if let Some(sid) = &outcome.session {
                    server
                        .bus
                        .sessions
                        .lock()
                        .unwrap()
                        .insert(agent.clone(), sid.clone());
                }
                // Record the reply on the channel. In the one-hop bus this is an
                // observation, not a new trigger.
                server.bus.feed.post(
                    &message.channel,
                    &agent,
                    None,
                    &outcome.reply,
                    Some(message.id),
                );
            }
            Err(e) => tracing::warn!(agent = %agent, error = %e, "bus run failed"),
        }
        server.bus.idle.dec();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{Backend, BackendError, Outcome};
    use crate::config::Config;
    use crate::params::Params;
    use std::sync::Arc;

    /// A backend whose reply echoes the prompt, so we can see the bus fired it.
    struct Reactor;
    #[async_trait::async_trait]
    impl Backend for Reactor {
        async fn run(&self, params: &Params) -> Result<Outcome, BackendError> {
            Ok(Outcome::from_reply(
                format!("reacted to: {}", params.prompt),
                params.session.clone(),
            ))
        }
    }

    fn server() -> Server {
        let config = Config::parse(
            r#"
            [agents.watcher]
            system = "watch the board"
            subscriptions = ["board"]

            [agents.bystander]
            system = "not subscribed"
            "#,
        )
        .unwrap();
        Server::new(config, Arc::new(Reactor))
    }

    #[test]
    fn config_resolves_subscribers() {
        let c = server();
        assert_eq!(c.config.subscribers("board"), vec!["watcher".to_string()]);
        assert!(c.config.subscribers("other").is_empty());
    }

    #[tokio::test]
    async fn a_broadcast_fires_the_subscriber_and_lands_in_the_feed() {
        let server = server();
        let bus = server.spawn_bus();
        server.broadcast("board", "operator", "issue 42 needs triage");
        server.wait_idle().await;
        bus.abort();

        let feed = server.feed(None, 50);
        // The operator broadcast, then the watcher's reaction.
        assert_eq!(feed.len(), 2, "{feed:?}");
        assert_eq!(feed[0].from, "operator");
        assert_eq!(feed[0].body, "issue 42 needs triage");
        assert_eq!(feed[1].from, "watcher");
        assert!(feed[1].body.contains("reacted to"));
        assert_eq!(feed[1].reply_to, Some(feed[0].id));
    }

    #[tokio::test]
    async fn a_broadcast_to_an_unsubscribed_channel_fires_no_one() {
        let server = server();
        let bus = server.spawn_bus();
        server.broadcast("quiet", "operator", "anyone?");
        server.wait_idle().await;
        bus.abort();

        let feed = server.feed(None, 50);
        assert_eq!(feed.len(), 1, "only the broadcast, no reaction: {feed:?}");
    }
}
