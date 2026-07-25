//! Errors from running a prompt.
//!
//! A run can fail because the request was malformed (an unknown agent, an empty
//! prompt) or because the backend failed. [`RunError`] keeps the two apart; both
//! surface to an MCP client as a tool error.

use crate::backend::BackendError;

/// Why a call to [`crate::Server::run`] failed.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    /// The call named an agent that is not configured.
    #[error("unknown agent: {0}")]
    UnknownAgent(String),
    /// The call named a session that does not exist.
    #[error("unknown session: {0}")]
    UnknownSession(String),
    /// The prompt was empty or only whitespace.
    #[error("prompt must not be empty")]
    EmptyPrompt,
    /// The server's budget cap has been reached.
    #[error("budget exceeded")]
    BudgetExceeded,
    /// The backend failed to run the prompt.
    #[error(transparent)]
    Backend(#[from] BackendError),
}
