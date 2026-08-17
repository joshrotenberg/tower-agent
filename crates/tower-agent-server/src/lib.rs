//! The preserved MCP-first tower-agent server.
//!
//! The core is one primitive, the **`prompt` tool**: it requires a `prompt` and
//! optionally takes any parameter the backend takes, so the surface is a
//! faithful, minimal projection of what the backend can do. Config supplies the
//! defaults, so in practice a call carries little.
//!
//! An **agent** is a named bundle of default parameters plus a base prompt: it
//! is config, not code. A call selects one with `agent`, and may override any of
//! its parameters. The [`Backend`] trait is the one seam where a model backend
//! lives; the core names none.
//!
//! Everything else (sessions, scheduling, inter-agent communication,
//! observability) is built up from this atom. See
//! `docs/design/spec.md`.

pub mod backend;
pub mod budget;
pub mod bus;
pub mod compat;
pub mod config;
pub mod error;
pub mod mcp;
pub mod params;
pub mod run;
pub mod schedule;
pub mod session;

pub use backend::{Backend, BackendError, Event, Outcome, Post, StubBackend};
pub use budget::Budget;
pub use bus::{Bus, BusHandle, Message};
pub use compat::BackendService;
pub use config::{AgentDef, BudgetConfig, Config, Defaults, ScheduledAgent, SecurityConfig};
pub use error::RunError;
pub use mcp::{Server, router};
pub use params::{Call, Params};
pub use run::{Run, RunKind, RunStatus, Runs};
pub use schedule::{ScheduleError, SchedulerHandle};
pub use session::{FileSessionStore, MemorySessionStore, SessionInfo, SessionStore};
