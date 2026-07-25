//! Sessions: resumable threads with an agent.
//!
//! A session has our own minted id (`s1`, `s2`, ...), decoupled from the
//! backend's resume token, which the store keeps as an internal detail. The
//! [`Server`](crate::Server) mints an id on a fresh call, resumes the backend
//! with the stored token on a returning call, and bumps the turn count. A
//! [`SessionStore`] holds the mapping; [`MemorySessionStore`] keeps it in memory,
//! [`FileSessionStore`] persists it as JSON so a one-shot CLI resumes across
//! invocations.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// A session as seen over the wire: our id plus metadata. The backend resume
/// token is not exposed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub agent: Option<String>,
    pub turns: u64,
    /// A short preview of the latest outcome text.
    pub last: Option<String>,
}

/// One stored session: its public info plus the backend resume token.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Record {
    info: SessionInfo,
    backend_token: Option<String>,
}

/// The store's whole state: the next id to mint and the records.
#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    next_id: u64,
    sessions: BTreeMap<String, Record>,
}

impl State {
    fn mint(&mut self) -> String {
        self.next_id += 1;
        format!("s{}", self.next_id)
    }

    fn exists(&self, id: &str) -> bool {
        self.sessions.contains_key(id)
    }

    fn backend_token(&self, id: &str) -> Option<String> {
        self.sessions.get(id).and_then(|r| r.backend_token.clone())
    }

    /// Record a completed turn: create the record if new, bump the turn count,
    /// keep the agent (a returning turn may not restate it), and update the token
    /// and preview.
    fn update(
        &mut self,
        id: &str,
        agent: Option<String>,
        backend_token: Option<String>,
        last: Option<String>,
    ) {
        let rec = self
            .sessions
            .entry(id.to_string())
            .or_insert_with(|| Record {
                info: SessionInfo {
                    id: id.to_string(),
                    agent: None,
                    turns: 0,
                    last: None,
                },
                backend_token: None,
            });
        rec.info.turns += 1;
        if agent.is_some() {
            rec.info.agent = agent;
        }
        rec.info.last = last;
        rec.backend_token = backend_token;
    }

    fn get(&self, id: &str) -> Option<SessionInfo> {
        self.sessions.get(id).map(|r| r.info.clone())
    }

    fn list(&self) -> Vec<SessionInfo> {
        self.sessions.values().map(|r| r.info.clone()).collect()
    }
}

/// The mapping from a session id to its backend resume token and metadata.
pub trait SessionStore: Send + Sync {
    /// Allocate a fresh session id. No record exists until the first turn is
    /// recorded, so a failed first turn leaves no orphan.
    fn mint(&self) -> String;
    /// Whether a session with this id has at least one recorded turn.
    fn exists(&self, id: &str) -> bool;
    /// The backend resume token stored for this session, if any.
    fn backend_token(&self, id: &str) -> Option<String>;
    /// Record a completed turn.
    fn record_turn(
        &self,
        id: &str,
        agent: Option<String>,
        backend_token: Option<String>,
        last: Option<String>,
    );
    /// One session by id.
    fn get(&self, id: &str) -> Option<SessionInfo>;
    /// All sessions.
    fn list(&self) -> Vec<SessionInfo>;
}

/// An in-memory session store. The default; lost when the process exits.
#[derive(Default)]
pub struct MemorySessionStore {
    state: Mutex<State>,
}

impl MemorySessionStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl SessionStore for MemorySessionStore {
    fn mint(&self) -> String {
        self.state.lock().unwrap().mint()
    }
    fn exists(&self, id: &str) -> bool {
        self.state.lock().unwrap().exists(id)
    }
    fn backend_token(&self, id: &str) -> Option<String> {
        self.state.lock().unwrap().backend_token(id)
    }
    fn record_turn(
        &self,
        id: &str,
        agent: Option<String>,
        backend_token: Option<String>,
        last: Option<String>,
    ) {
        self.state
            .lock()
            .unwrap()
            .update(id, agent, backend_token, last);
    }
    fn get(&self, id: &str) -> Option<SessionInfo> {
        self.state.lock().unwrap().get(id)
    }
    fn list(&self) -> Vec<SessionInfo> {
        self.state.lock().unwrap().list()
    }
}

/// A session store persisted to a JSON file, so a one-shot CLI resumes across
/// invocations. A minimal id-to-token map, not the durable event store.
pub struct FileSessionStore {
    path: PathBuf,
    state: Mutex<State>,
}

impl FileSessionStore {
    /// Open (or start) a store at `path`. A missing or unreadable file starts
    /// empty.
    pub fn open(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let state = load(&path).unwrap_or_default();
        FileSessionStore {
            path,
            state: Mutex::new(state),
        }
    }

    fn save(&self, state: &State) {
        if let Some(parent) = self.path.parent().filter(|p| !p.as_os_str().is_empty()) {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(state) {
            let _ = std::fs::write(&self.path, json);
        }
    }
}

fn load(path: &Path) -> Option<State> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

impl SessionStore for FileSessionStore {
    fn mint(&self) -> String {
        let mut state = self.state.lock().unwrap();
        let id = state.mint();
        self.save(&state);
        id
    }
    fn exists(&self, id: &str) -> bool {
        self.state.lock().unwrap().exists(id)
    }
    fn backend_token(&self, id: &str) -> Option<String> {
        self.state.lock().unwrap().backend_token(id)
    }
    fn record_turn(
        &self,
        id: &str,
        agent: Option<String>,
        backend_token: Option<String>,
        last: Option<String>,
    ) {
        let mut state = self.state.lock().unwrap();
        state.update(id, agent, backend_token, last);
        self.save(&state);
    }
    fn get(&self, id: &str) -> Option<SessionInfo> {
        self.state.lock().unwrap().get(id)
    }
    fn list(&self) -> Vec<SessionInfo> {
        self.state.lock().unwrap().list()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mint_is_unique_and_sequential() {
        let s = MemorySessionStore::new();
        assert_eq!(s.mint(), "s1");
        assert_eq!(s.mint(), "s2");
    }

    #[test]
    fn record_turn_creates_then_bumps() {
        let s = MemorySessionStore::new();
        let id = s.mint();
        assert!(!s.exists(&id), "no record until a turn is recorded");

        s.record_turn(
            &id,
            Some("tester".into()),
            Some("bk-1".into()),
            Some("hi".into()),
        );
        assert!(s.exists(&id));
        let info = s.get(&id).unwrap();
        assert_eq!(info.turns, 1);
        assert_eq!(info.agent.as_deref(), Some("tester"));
        assert_eq!(s.backend_token(&id).as_deref(), Some("bk-1"));

        // A returning turn without an agent keeps the original and bumps.
        s.record_turn(&id, None, Some("bk-2".into()), Some("bye".into()));
        let info = s.get(&id).unwrap();
        assert_eq!(info.turns, 2);
        assert_eq!(info.agent.as_deref(), Some("tester"));
        assert_eq!(s.backend_token(&id).as_deref(), Some("bk-2"));
    }

    #[test]
    fn file_store_round_trips_across_reload() {
        let dir = std::env::temp_dir().join(format!("tower-agent-sess-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("sessions.json");

        let id = {
            let s = FileSessionStore::open(&path);
            let id = s.mint();
            s.record_turn(
                &id,
                Some("scout".into()),
                Some("bk-9".into()),
                Some("done".into()),
            );
            id
        };

        // Reopen: the record and the id counter survive.
        let s = FileSessionStore::open(&path);
        assert!(s.exists(&id));
        assert_eq!(s.get(&id).unwrap().turns, 1);
        assert_eq!(s.backend_token(&id).as_deref(), Some("bk-9"));
        assert_eq!(s.mint(), "s2", "the counter continues past persisted ids");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
