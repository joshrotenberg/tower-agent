//! The host's typed job vocabulary and its profile catalog.
//!
//! Configuration names a profile; the host owns what that name means. This is
//! the indirection that keeps provider options, credentials, and model
//! choices out of a file that a person edits and a repository stores.

use std::collections::BTreeMap;

use tower_agent_plan::{PartialTurn, Profile, ProviderId};

/// What one compiled step does.
///
/// Opaque to the workflow crate, which only ever moves it around. Both
/// variants travel through one dispatcher, which is the composition #107
/// exists to test.
#[derive(Clone, Debug)]
pub enum Job {
    /// An agent turn whose configuration has already been resolved against a
    /// profile. The prompt is carried separately because it is the one part
    /// of a turn the configuration genuinely owns.
    Agent {
        profile: String,
        prompt: String,
        provider: ProviderId,
    },
    /// A typed in-process operation. No shell, no subprocess: #106 records
    /// why a subprocess boundary needs a decision before one exists.
    Mechanical {
        op: MechanicalOp,
        args: BTreeMap<String, String>,
    },
}

/// The mechanical operations this host implements, as a closed set.
///
/// Closed on purpose. An open string would let configuration name work the
/// host has not implemented, which turns a compile-time error into a
/// run-time one halfway through a workflow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MechanicalOp {
    /// Report the checked-out branch.
    ReadBranch,
    /// Count files matching a pattern.
    CountFiles,
    /// Collect prior step results into one summary.
    Collect,
}

impl MechanicalOp {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "read_branch" => Some(Self::ReadBranch),
            "count_files" => Some(Self::CountFiles),
            "collect" => Some(Self::Collect),
            _ => None,
        }
    }
}

/// Host-owned agent profiles, keyed by the name configuration uses.
///
/// A profile is a saved partial turn, exactly as the planning crate defines
/// it, so the host gets layered resolution and requirements-as-data without
/// inventing a second configuration model.
#[derive(Debug, Default)]
pub struct ProfileCatalog {
    profiles: BTreeMap<String, Profile>,
}

impl ProfileCatalog {
    /// The catalog this example ships with.
    pub fn example() -> Self {
        let mut profiles = BTreeMap::new();
        profiles.insert(
            "reviewer".to_string(),
            Profile {
                name: "reviewer".to_string(),
                turn: PartialTurn {
                    provider: Some(ProviderId::Claude),
                    ..Default::default()
                },
            },
        );
        profiles.insert(
            "implementer".to_string(),
            Profile {
                name: "implementer".to_string(),
                turn: PartialTurn {
                    provider: Some(ProviderId::Codex),
                    ..Default::default()
                },
            },
        );
        Self { profiles }
    }

    pub fn get(&self, name: &str) -> Option<&Profile> {
        self.profiles.get(name)
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.profiles.keys().map(String::as_str)
    }
}
