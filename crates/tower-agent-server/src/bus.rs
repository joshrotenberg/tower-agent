//! The bus: agents react to messages on channels they subscribe to.
//!
//! This is the third way to fire an agent, alongside a prompt call and a
//! schedule. An operator (or, later, another agent) broadcasts a message to a
//! channel; every agent subscribed to that channel runs with the message as its
//! prompt, and its reply is recorded back on the channel, visible in the feed.
//!
//! A fired agent runs structured, so it can emit posts of its own. Each post is
//! recorded and, within a depth bound, fires its recipients (the cascade), so
//! agents converse; the bound turns a runaway loop into a log line. A post
//! reaches the subscribers of its channel plus a directed `to`, and threads via
//! `reply_to`. A fired turn is given the recent channel history as context.
//! Execution is serial: one background worker drains a queue.

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

/// How many hops of agent-to-agent cascade to allow before dropping further
/// posts, so a runaway loop becomes a log line, not a hang.
const MAX_DEPTH: usize = 6;

enum Job {
    Fire {
        agent: String,
        message: Message,
        depth: usize,
    },
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
    /// Post a message to a channel and fire its recipients. `to` addresses one
    /// agent directly, reaching it even if it does not subscribe. The message is
    /// returned immediately; the reactions run on the worker.
    pub fn broadcast(&self, channel: &str, from: &str, to: Option<&str>, body: &str) -> Message {
        let msg = self
            .bus
            .feed
            .post(channel, from, to.map(String::from), body, None);
        self.fan_out(&msg, 0);
        msg
    }

    /// Fire the recipients of a message: the channel's subscribers, plus a
    /// directed `to`. The sender is never fired, so an agent does not react to
    /// its own message.
    fn fan_out(&self, msg: &Message, depth: usize) {
        let mut targets = self.config.subscribers(&msg.channel);
        if let Some(to) = &msg.to
            && !targets.contains(to)
        {
            targets.push(to.clone());
        }
        for agent in targets {
            if agent == msg.from {
                continue;
            }
            self.bus.idle.inc();
            if self
                .bus
                .tx
                .send(Job::Fire {
                    agent,
                    message: msg.clone(),
                    depth,
                })
                .is_err()
            {
                self.bus.idle.dec();
            }
        }
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

/// Render recent channel messages as context for a fired turn, so the agent
/// sees the thread it is part of, newest last.
fn context(history: &[Message], channel: &str) -> String {
    if history.is_empty() {
        return String::new();
    }
    let mut s = format!("Recent messages on channel '{channel}' (newest last):\n");
    for m in history {
        let to =
            m.to.as_deref()
                .map(|t| format!(" -> {t}"))
                .unwrap_or_default();
        let re = m
            .reply_to
            .map(|r| format!(" (re #{r})"))
            .unwrap_or_default();
        s.push_str(&format!("[#{}] <{}>{to}{re}: {}\n", m.id, m.from, m.body));
    }
    s.push('\n');
    s
}

async fn bus_worker(server: Server, mut rx: UnboundedReceiver<Job>) {
    while let Some(Job::Fire {
        agent,
        message,
        depth,
    }) = rx.recv().await
    {
        let session = server.bus.sessions.lock().unwrap().get(&agent).cloned();
        // Give the turn the recent thread on this channel, not just its trigger.
        let history = server.feed(Some(&message.channel), 20);
        let prompt = format!(
            "{}You were addressed by message #{} from {}. Respond per your instructions.",
            context(&history, &message.channel),
            message.id,
            message.from
        );
        let call = Call {
            prompt,
            agent: Some(agent.clone()),
            session,
            ..Default::default()
        };
        // A fired turn is structured, so the agent can emit posts.
        match server.run_structured(call).await {
            Ok(outcome) => {
                if let Some(sid) = &outcome.session {
                    server
                        .bus
                        .sessions
                        .lock()
                        .unwrap()
                        .insert(agent.clone(), sid.clone());
                }
                if outcome.posts.is_empty() {
                    // The agent only replied: record it, threaded. A reply is an
                    // observation, not a new trigger.
                    server.bus.feed.post(
                        &message.channel,
                        &agent,
                        None,
                        &outcome.reply,
                        Some(message.id),
                    );
                } else {
                    // Each post is recorded and, within the depth bound, fires its
                    // recipients (the cascade).
                    for post in outcome.posts {
                        let reply_to = post.reply_to.or(Some(message.id));
                        let posted = server.bus.feed.post(
                            &post.channel,
                            &agent,
                            post.to.clone(),
                            &post.body,
                            reply_to,
                        );
                        if depth < MAX_DEPTH {
                            server.fan_out(&posted, depth + 1);
                        } else {
                            tracing::warn!(agent = %agent, depth, "cascade depth bound reached; dropping post");
                        }
                    }
                }
            }
            Err(e) => tracing::warn!(agent = %agent, error = %e, "bus run failed"),
        }
        server.bus.idle.dec();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{Backend, BackendError, Outcome, Post};
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

    /// A backend that posts to the channel named in its agent's system prompt
    /// (`post-to:CHANNEL`), and otherwise just reacts. Deterministic, for
    /// exercising routing and the cascade without a live model.
    struct Poster;
    #[async_trait::async_trait]
    impl Backend for Poster {
        async fn run(&self, params: &Params) -> Result<Outcome, BackendError> {
            match params
                .system
                .as_deref()
                .and_then(|s| s.strip_prefix("post-to:"))
            {
                Some(channel) => Ok(Outcome {
                    summary: "posting".into(),
                    reply: String::new(),
                    posts: vec![Post {
                        channel: channel.to_string(),
                        body: "ping".into(),
                        to: None,
                        reply_to: None,
                    }],
                    session: params.session.clone(),
                    cost_usd: None,
                }),
                None => Ok(Outcome::from_reply("reacted", params.session.clone())),
            }
        }
    }

    fn watcher_server() -> Server {
        let config = Config::parse(
            r#"
            [agents.watcher]
            system = "watch the board"
            subscriptions = ["board"]

            [agents.helper]
            system = "reachable when addressed"
            "#,
        )
        .unwrap();
        Server::new(config, Arc::new(Reactor))
    }

    #[test]
    fn config_resolves_subscribers() {
        let c = watcher_server();
        assert_eq!(c.config.subscribers("board"), vec!["watcher".to_string()]);
        assert!(c.config.subscribers("other").is_empty());
    }

    #[tokio::test]
    async fn a_broadcast_fires_the_subscriber_and_threads_the_reply() {
        let server = watcher_server();
        let bus = server.spawn_bus();
        server.broadcast("board", "operator", None, "issue 42 needs triage");
        server.wait_idle().await;
        bus.abort();

        let feed = server.feed(None, 50);
        assert_eq!(feed.len(), 2, "{feed:?}");
        assert_eq!(feed[0].from, "operator");
        assert_eq!(feed[1].from, "watcher");
        assert!(feed[1].body.contains("reacted to"));
        assert_eq!(feed[1].reply_to, Some(feed[0].id));

        // A bus fire is recorded as a subscribe run.
        let runs = server.runs().list(10);
        assert!(
            runs.iter()
                .any(|r| matches!(r.kind, crate::run::RunKind::Subscribe)),
            "{runs:?}"
        );
    }

    #[tokio::test]
    async fn a_directed_broadcast_reaches_an_unsubscribed_agent() {
        let server = watcher_server();
        let bus = server.spawn_bus();
        // `helper` subscribes to nothing, but a directed `to` reaches it.
        server.broadcast("void", "operator", Some("helper"), "you around?");
        server.wait_idle().await;
        bus.abort();

        let feed = server.feed(None, 50);
        assert_eq!(feed.len(), 2, "{feed:?}");
        assert_eq!(feed[1].from, "helper");
    }

    #[tokio::test]
    async fn a_cascade_is_bounded() {
        // A and B post to each other's channels forever; the depth bound stops it.
        let config = Config::parse(
            r#"
            [agents.a]
            system = "post-to:chb"
            subscriptions = ["cha"]

            [agents.b]
            system = "post-to:cha"
            subscriptions = ["chb"]
            "#,
        )
        .unwrap();
        let server = Server::new(config, Arc::new(Poster));
        let bus = server.spawn_bus();
        server.broadcast("cha", "operator", None, "go");
        // If the bound did not work this would never return.
        server.wait_idle().await;
        bus.abort();

        let feed = server.feed(None, 50);
        assert!(feed.len() > 3, "the cascade ran: {}", feed.len());
        assert!(
            feed.len() <= 2 + MAX_DEPTH + 1,
            "the cascade is bounded: {}",
            feed.len()
        );
    }

    #[tokio::test]
    async fn a_fired_turn_sees_recent_channel_history() {
        // A backend that records the prompt it was given.
        struct Capture {
            seen: Arc<Mutex<Vec<String>>>,
        }
        #[async_trait::async_trait]
        impl Backend for Capture {
            async fn run(&self, params: &Params) -> Result<Outcome, BackendError> {
                self.seen.lock().unwrap().push(params.prompt.clone());
                Ok(Outcome::from_reply("ok", params.session.clone()))
            }
        }

        let seen = Arc::new(Mutex::new(Vec::new()));
        let config = Config::parse(
            r#"
            [agents.watcher]
            system = "watch"
            subscriptions = ["board"]
            "#,
        )
        .unwrap();
        let server = Server::new(config, Arc::new(Capture { seen: seen.clone() }));
        let bus = server.spawn_bus();

        server.broadcast("board", "operator", None, "first message");
        server.wait_idle().await;
        server.broadcast("board", "operator", None, "second message");
        server.wait_idle().await;
        bus.abort();

        let prompts = seen.lock().unwrap().clone();
        assert_eq!(prompts.len(), 2);
        // The second turn's prompt carries the earlier exchange as context.
        assert!(prompts[1].contains("first message"), "{}", prompts[1]);
        assert!(
            prompts[1].contains("Recent messages on channel 'board'"),
            "{}",
            prompts[1]
        );
    }
}
