use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;

use tower_agent::SessionHandle;
use uuid::Uuid;

/// A future returned by a [`ContinuationStore`].
///
/// Boxed rather than an `async fn` in the trait so the trait stays usable
/// behind `dyn`, which is how a host supplies its own store.
pub type StoreFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Largest number of continuations [`InMemoryContinuationStore`] retains.
pub const DEFAULT_CAPACITY: usize = 4096;

/// The public name for a resumable conversation.
///
/// This is the only continuation identity that crosses a protocol boundary. It
/// is minted by the adapter, carries no provider information, and is not
/// derived from the [`SessionHandle`] it names, so holding one reveals nothing
/// about the provider or its private handle.
///
/// There is deliberately no `Display`. A bearer-scoped host treats this value
/// as a credential, and `{}` in a log line is how credentials escape. Callers
/// that need the string ask for it with [`as_str`](Self::as_str).
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct ContinuationId(String);

impl ContinuationId {
    /// Mint a fresh identifier from the system random source.
    ///
    /// Version 4 UUIDs carry 122 random bits, which is the usual size for a
    /// value that may be treated as a capability.
    #[must_use]
    pub fn generate() -> Self {
        Self(Uuid::new_v4().to_string())
    }

    /// Accept an identifier a client sent back.
    ///
    /// The value is not parsed or validated beyond being non-blank. It is a
    /// lookup key, and deciding whether it names anything is
    /// [`ContinuationStore::resolve`]'s job, under a scope.
    pub fn parse(value: impl Into<String>) -> Result<Self, InvalidContinuationId> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(InvalidContinuationId);
        }
        Ok(Self(value))
    }

    /// The identifier as it travels on the wire.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ContinuationId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("ContinuationId")
            .field(&"..")
            .finish()
    }
}

/// The error returned when a continuation identifier is blank.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("continuation id must not be blank")]
pub struct InvalidContinuationId;

/// What a continuation belongs to.
///
/// A continuation resumes a conversation, and a resumed turn can read that
/// conversation's prior context, so an identifier is a capability over history
/// rather than a key. The scope is what stops one holder from spending
/// another's.
///
/// The variants are distinct types rather than one string because a transport
/// session identifier and an authenticated subject are drawn from unrelated
/// namespaces. If they shared one, a session that happened to be named like a
/// principal would resolve that principal's continuations.
#[derive(Clone, PartialEq, Eq, Hash)]
pub enum Scope {
    /// The transport session that minted the continuation. Ends with the
    /// connection, so continuations do not outlive a reconnect.
    Session(String),
    /// The authenticated subject that minted the continuation. Survives
    /// reconnects, and requires the transport to have authenticated someone.
    Principal(String),
}

impl Scope {
    /// Scope a continuation to one transport session.
    pub fn session(id: impl Into<String>) -> Self {
        Self::Session(id.into())
    }

    /// Scope a continuation to one authenticated subject.
    pub fn principal(subject: impl Into<String>) -> Self {
        Self::Principal(subject.into())
    }
}

impl fmt::Debug for Scope {
    /// Shows which kind of scope this is and not whose.
    ///
    /// A principal subject names a person. The variant is what a reader
    /// debugging a scope mismatch actually needs.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Session(_) => "Session",
            Self::Principal(_) => "Principal",
        };
        formatter.debug_tuple(name).field(&"..").finish()
    }
}

/// Why a continuation store could not answer.
///
/// Absence is not an error: an identifier that names nothing in this scope
/// resolves to `Ok(None)`. This reports that the store itself failed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ContinuationError {
    /// The host's storage failed.
    #[error("continuation store backend failed: {0}")]
    Backend(String),
}

/// Maps public continuation identifiers to provider session handles.
///
/// The adapter mints identity. The host owns persistence, which is the same
/// split `tower-agent-workflow` makes with its opaque jobs: naming here,
/// truth in the host.
///
/// Implementations are asked for a scope on every operation rather than
/// trusting the identifier alone. An implementation that ignores the scope has
/// built a bearer-token store, where possession of an identifier is sufficient
/// to resume anyone's conversation. That is a legitimate choice for a
/// single-user host and a serious one for a shared host, and it should be made
/// deliberately rather than by omission.
pub trait ContinuationStore: Send + Sync + 'static {
    /// Record `session` and return the public name for it.
    fn mint(
        &self,
        session: SessionHandle,
        scope: Scope,
    ) -> StoreFuture<'_, Result<ContinuationId, ContinuationError>>;

    /// The session `id` names within `scope`, if any.
    ///
    /// Returns `Ok(None)` when the identifier is unknown, when it belongs to a
    /// different scope, and when it has been dropped. A caller cannot tell
    /// these apart, which is deliberate: distinguishing them would answer
    /// whether an identifier exists somewhere, for a caller that cannot use it.
    fn resolve(
        &self,
        id: ContinuationId,
        scope: Scope,
    ) -> StoreFuture<'_, Result<Option<SessionHandle>, ContinuationError>>;

    /// Drop every continuation belonging to `scope`.
    ///
    /// A transport calls this when a session ends. Without it a session-scoped
    /// store accumulates continuations that can never resolve again.
    fn forget_scope(&self, scope: Scope) -> StoreFuture<'_, Result<(), ContinuationError>>;
}

struct Entry {
    scope: Scope,
    session: SessionHandle,
}

#[derive(Default)]
struct State {
    entries: HashMap<ContinuationId, Entry>,
    order: VecDeque<ContinuationId>,
}

/// A bounded, in-process [`ContinuationStore`].
///
/// The default for a host that has not chosen otherwise. Continuations live
/// only as long as the process, so they do not survive a restart. A durable
/// host supplies its own store and in doing so chooses the lifetime and the
/// scope check together.
///
/// Retention is bounded because an unbounded map keyed by a client-triggered
/// mint is a memory-growth path. When the store is full the oldest
/// continuation is dropped, which costs a stale conversation its resumability
/// rather than failing the turn that is currently settling.
pub struct InMemoryContinuationStore {
    capacity: usize,
    state: Mutex<State>,
}

impl InMemoryContinuationStore {
    /// A store holding [`DEFAULT_CAPACITY`] continuations.
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    /// A store holding at most `capacity` continuations.
    ///
    /// # Panics
    ///
    /// Panics when `capacity` is zero, which would discard every continuation
    /// as soon as it was minted.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        assert!(capacity > 0, "continuation store capacity must be positive");
        Self {
            capacity,
            state: Mutex::new(State::default()),
        }
    }

    /// How many continuations are currently retained.
    #[must_use]
    pub fn len(&self) -> usize {
        self.locked().entries.len()
    }

    /// Whether the store holds no continuations.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn locked(&self) -> std::sync::MutexGuard<'_, State> {
        // A poisoned lock means another thread panicked while holding it. The
        // map is still structurally sound, and refusing every continuation
        // afterwards would be a worse outcome than continuing.
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Default for InMemoryContinuationStore {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for InMemoryContinuationStore {
    /// Reports size and never contents. Entries hold session handles.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InMemoryContinuationStore")
            .field("capacity", &self.capacity)
            .field("retained", &self.len())
            .finish()
    }
}

impl ContinuationStore for InMemoryContinuationStore {
    fn mint(
        &self,
        session: SessionHandle,
        scope: Scope,
    ) -> StoreFuture<'_, Result<ContinuationId, ContinuationError>> {
        let id = ContinuationId::generate();
        {
            let mut state = self.locked();
            state.entries.insert(id.clone(), Entry { scope, session });
            state.order.push_back(id.clone());
            while state.entries.len() > self.capacity {
                match state.order.pop_front() {
                    Some(oldest) => {
                        state.entries.remove(&oldest);
                    }
                    None => break,
                }
            }
        }
        Box::pin(async move { Ok(id) })
    }

    fn resolve(
        &self,
        id: ContinuationId,
        scope: Scope,
    ) -> StoreFuture<'_, Result<Option<SessionHandle>, ContinuationError>> {
        let found = self.locked().entries.get(&id).and_then(|entry| {
            // The scope check is the security boundary. Everything else in
            // this type is bookkeeping around it.
            (entry.scope == scope).then(|| entry.session.clone())
        });
        Box::pin(async move { Ok(found) })
    }

    fn forget_scope(&self, scope: Scope) -> StoreFuture<'_, Result<(), ContinuationError>> {
        {
            let mut state = self.locked();
            state.entries.retain(|_, entry| entry.scope != scope);
            let retained = state.entries.keys().cloned().collect::<Vec<_>>();
            state.order.retain(|id| retained.contains(id));
        }
        Box::pin(async move { Ok(()) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handle(value: &str) -> SessionHandle {
        SessionHandle::new("fake", value)
    }

    #[tokio::test]
    async fn a_continuation_resolves_only_within_the_scope_that_minted_it() {
        let store = InMemoryContinuationStore::new();
        let id = store
            .mint(handle("private-session"), Scope::session("connection-a"))
            .await
            .expect("mint");

        let mine = store
            .resolve(id.clone(), Scope::session("connection-a"))
            .await
            .expect("resolve");
        assert_eq!(
            mine.as_ref().map(SessionHandle::value),
            Some("private-session")
        );

        // The identifier is real and the holder is not its owner.
        let theirs = store
            .resolve(id, Scope::session("connection-b"))
            .await
            .expect("resolve");
        assert!(theirs.is_none());
    }

    #[tokio::test]
    async fn a_session_scope_never_matches_a_principal_with_the_same_name() {
        let store = InMemoryContinuationStore::new();
        let id = store
            .mint(handle("private-session"), Scope::principal("alice"))
            .await
            .expect("mint");

        // Transport session ids and authenticated subjects are unrelated
        // namespaces. Sharing one string type would make this resolve.
        let crossed = store
            .resolve(id.clone(), Scope::session("alice"))
            .await
            .expect("resolve");
        assert!(crossed.is_none());

        let owner = store
            .resolve(id, Scope::principal("alice"))
            .await
            .expect("resolve");
        assert!(owner.is_some());
    }

    #[tokio::test]
    async fn an_unknown_id_and_a_dropped_id_are_indistinguishable() {
        let store = InMemoryContinuationStore::with_capacity(1);
        let evicted = store
            .mint(handle("first"), Scope::session("c"))
            .await
            .expect("mint");
        store
            .mint(handle("second"), Scope::session("c"))
            .await
            .expect("mint");

        let never_existed = ContinuationId::parse("never-minted").expect("parse");
        let dropped = store
            .resolve(evicted, Scope::session("c"))
            .await
            .expect("resolve");
        let unknown = store
            .resolve(never_existed, Scope::session("c"))
            .await
            .expect("resolve");

        // Reporting these differently would tell a caller that an identifier
        // exists somewhere it cannot reach, which is an oracle rather than an
        // error message.
        assert_eq!(dropped, unknown);
        assert!(dropped.is_none());
    }

    #[tokio::test]
    async fn an_identifier_is_not_derived_from_the_session_it_names() {
        let store = InMemoryContinuationStore::new();
        let first = store
            .mint(handle("private-session"), Scope::session("c"))
            .await
            .expect("mint");
        let second = store
            .mint(handle("private-session"), Scope::session("c"))
            .await
            .expect("mint");

        // Same handle, same scope, different identifiers: the identifier
        // cannot be a function of the session, so it correlates nothing.
        assert_ne!(first, second);
        assert!(!first.as_str().contains("private-session"));
        assert!(!second.as_str().contains("private-session"));
    }

    #[tokio::test]
    async fn debug_reveals_neither_the_session_nor_the_identifier() {
        let store = InMemoryContinuationStore::new();
        let id = store
            .mint(handle("private-session"), Scope::principal("alice"))
            .await
            .expect("mint");

        let rendered = format!("{id:?} {store:?} {:?}", Scope::principal("alice"));
        assert!(!rendered.contains("private-session"), "{rendered}");
        assert!(!rendered.contains(id.as_str()), "{rendered}");
        assert!(!rendered.contains("alice"), "{rendered}");
        // The kind of scope still shows, because that is what a mismatch
        // investigation needs.
        assert!(rendered.contains("Principal"), "{rendered}");
    }

    #[tokio::test]
    async fn forgetting_a_scope_drops_only_that_scope() {
        let store = InMemoryContinuationStore::new();
        let ending = store
            .mint(handle("a"), Scope::session("closing"))
            .await
            .expect("mint");
        let surviving = store
            .mint(handle("b"), Scope::session("open"))
            .await
            .expect("mint");

        store
            .forget_scope(Scope::session("closing"))
            .await
            .expect("forget");

        assert!(
            store
                .resolve(ending, Scope::session("closing"))
                .await
                .expect("resolve")
                .is_none()
        );
        assert!(
            store
                .resolve(surviving, Scope::session("open"))
                .await
                .expect("resolve")
                .is_some()
        );
        assert_eq!(store.len(), 1);
    }

    #[tokio::test]
    async fn retention_is_bounded_and_drops_the_oldest_first() {
        let store = InMemoryContinuationStore::with_capacity(2);
        let first = store
            .mint(handle("1"), Scope::session("c"))
            .await
            .expect("mint");
        let second = store
            .mint(handle("2"), Scope::session("c"))
            .await
            .expect("mint");
        let third = store
            .mint(handle("3"), Scope::session("c"))
            .await
            .expect("mint");

        assert_eq!(store.len(), 2);
        let scope = Scope::session("c");
        assert!(
            store
                .resolve(first, scope.clone())
                .await
                .expect("r")
                .is_none()
        );
        assert!(
            store
                .resolve(second, scope.clone())
                .await
                .expect("r")
                .is_some()
        );
        assert!(store.resolve(third, scope).await.expect("r").is_some());
    }

    #[tokio::test]
    async fn the_provider_tag_survives_the_round_trip() {
        let store = InMemoryContinuationStore::new();
        let id = store
            .mint(SessionHandle::new("codex", "s"), Scope::session("c"))
            .await
            .expect("mint");

        let resolved = store
            .resolve(id, Scope::session("c"))
            .await
            .expect("resolve")
            .expect("present");
        // The adapters refuse a foreign-provider handle at validation, which
        // only works if the tag survives storage.
        assert_eq!(resolved.provider(), "codex");
    }

    #[test]
    fn a_blank_identifier_is_refused() {
        assert_eq!(ContinuationId::parse("   "), Err(InvalidContinuationId));
        assert_eq!(ContinuationId::parse(""), Err(InvalidContinuationId));
        assert!(ContinuationId::parse("abc").is_ok());
    }

    #[test]
    fn the_store_is_usable_behind_dyn() {
        // The host supplies its own, so the trait has to stay object safe.
        let store: Box<dyn ContinuationStore> = Box::new(InMemoryContinuationStore::new());
        assert!(!std::ptr::addr_of!(*store).is_null());
    }
}
