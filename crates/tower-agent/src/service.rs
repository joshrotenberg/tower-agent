use tower::util::BoxCloneSyncService;

use crate::{AgentError, AgentRequest, Turn, TurnOutcome};

pub type TurnRequest<O = ()> = AgentRequest<Turn<O>>;

/// A cloneable, sendable, shareable type-erased finite-turn service.
///
/// The `Sync` bound lets applications install this service directly as state
/// in protocol adapters such as `tower-mcp` without an artificial mutex.
pub type BoxTurnService<O = ()> = BoxCloneSyncService<TurnRequest<O>, TurnOutcome, AgentError>;
