//! The host's durable state, as an append-only log.
//!
//! Append-only rather than mutable rows, because the questions this spike has
//! to answer are historical: was this step claimed before the worker died,
//! did a result arrive after its claim was fenced out, who decided an
//! uncertain step was safe to resume. A log answers those; a row that was
//! overwritten does not.
//!
//! The log holds host-owned facts only. It never contains an `AgentRequest`,
//! provider options, credentials, a provider session value, a cancellation
//! token, or an `Instant`. Deadlines persist as UTC milliseconds and are
//! reconstructed locally, because an `Instant` is meaningless in the process
//! that reads it back.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Monotonic fencing token. A claim from an older epoch cannot commit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Epoch(pub u64);

/// Identity of one step within one run. Stable across restarts.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct StepKey {
    pub run_id: String,
    pub step_id: String,
}

/// What a step's terminal result was, in host-owned terms.
///
/// Bounded on purpose: a provider can produce an unbounded result, and a
/// durable store that writes all of it is a disk-exhaustion path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TerminalRecord {
    Succeeded { output: String },
    Failed { kind: String, message: String },
}

pub const MAX_RECORDED_OUTPUT: usize = 4 * 1024;

/// One durable fact. The log is a sequence of these.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "record", rename_all = "snake_case")]
pub enum Record {
    /// The step exists and what it is. The fingerprint freezes the work, so a
    /// redelivery that describes different work is rejected rather than run.
    Frozen {
        key: StepKey,
        workflow_id: String,
        workflow_version: String,
        fingerprint: String,
        /// Absolute deadline as UTC milliseconds, never an `Instant`.
        not_after_utc_ms: Option<u64>,
    },
    /// A worker took ownership under a fencing epoch.
    Claimed {
        key: StepKey,
        epoch: Epoch,
        worker: String,
    },
    /// The provider call started. Past this line effects are possible.
    Launched { key: StepKey, epoch: Epoch },
    /// The authoritative terminal result, committed before the queue is told
    /// anything.
    Settled {
        key: StepKey,
        epoch: Epoch,
        result: TerminalRecord,
    },
    /// Ownership was lost after launch. Whether the work happened is unknown.
    Uncertain {
        key: StepKey,
        epoch: Epoch,
        reason: String,
    },
    /// A human or policy resolved an uncertain step. Recorded so the decision
    /// is auditable rather than implied by a state change.
    Reconciled {
        key: StepKey,
        decision: Reconciliation,
        by: String,
    },
    /// Durable cancellation intent for a whole run.
    CancelRequested { run_id: String, reason: String },
}

/// How an uncertain step was resolved.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reconciliation {
    /// Evidence showed the work completed; adopt this result.
    AdoptCompleted { output: String },
    /// Evidence showed the work never ran, so replaying it is safe.
    ProvedNoEffects,
    /// Abandoned deliberately. Descendants stay blocked.
    Abandoned { note: String },
}

/// The state a step is in, folded from the log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StepState {
    Pending,
    Claimed(Epoch),
    Launched(Epoch),
    Settled(TerminalRecord),
    /// Lost after launch. Never automatically relaunched.
    Uncertain {
        epoch: Epoch,
        reason: String,
    },
}

#[derive(Clone, Debug)]
pub struct StepView {
    pub state: StepState,
    pub fingerprint: String,
    pub not_after_utc_ms: Option<u64>,
    pub highest_epoch: Epoch,
}

/// An append-only durable store.
pub struct DurableStore {
    path: PathBuf,
    steps: BTreeMap<StepKey, StepView>,
    cancelled_runs: BTreeMap<String, String>,
}

impl DurableStore {
    /// Open a store, replaying whatever a previous process left behind.
    ///
    /// This is the whole restart story: state comes from the file, never from
    /// memory carried across the boundary.
    pub fn open(path: impl Into<PathBuf>) -> std::io::Result<Self> {
        let path = path.into();
        let mut store = Self {
            path: path.clone(),
            steps: BTreeMap::new(),
            cancelled_runs: BTreeMap::new(),
        };
        if path.exists() {
            let reader = BufReader::new(File::open(&path)?);
            for line in reader.lines() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                // A corrupt tail is skipped rather than fatal: a process
                // killed mid-write leaves a partial final line, and refusing
                // to start would make a crash unrecoverable.
                if let Ok(record) = serde_json::from_str::<Record>(&line) {
                    store.apply(record);
                }
            }
        }
        Ok(store)
    }

    /// Append a fact and fold it in. Durable before it is visible.
    pub fn append(&mut self, record: Record) -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        writeln!(file, "{}", serde_json::to_string(&record)?)?;
        // Flushed before the caller continues, so a commit that returned is
        // a commit that survives.
        file.sync_all()?;
        self.apply(record);
        Ok(())
    }

    fn apply(&mut self, record: Record) {
        match record {
            Record::Frozen {
                key,
                fingerprint,
                not_after_utc_ms,
                ..
            } => {
                self.steps.entry(key).or_insert(StepView {
                    state: StepState::Pending,
                    fingerprint,
                    not_after_utc_ms,
                    highest_epoch: Epoch(0),
                });
            }
            Record::Claimed { key, epoch, .. } => {
                if let Some(view) = self.steps.get_mut(&key) {
                    // A stale claim cannot displace a newer one.
                    if epoch >= view.highest_epoch && !view.is_terminal() {
                        view.highest_epoch = epoch;
                        view.state = StepState::Claimed(epoch);
                    }
                }
            }
            Record::Launched { key, epoch } => {
                if let Some(view) = self.steps.get_mut(&key)
                    && epoch >= view.highest_epoch
                    && !view.is_terminal()
                {
                    view.state = StepState::Launched(epoch);
                }
            }
            Record::Settled { key, epoch, result } => {
                if let Some(view) = self.steps.get_mut(&key) {
                    // The fence: a result from a claim that was superseded is
                    // recorded in the log but does not become the answer.
                    if epoch >= view.highest_epoch && !view.is_terminal() {
                        view.state = StepState::Settled(result);
                    }
                }
            }
            Record::Uncertain { key, epoch, reason } => {
                if let Some(view) = self.steps.get_mut(&key)
                    && !view.is_terminal()
                {
                    view.state = StepState::Uncertain { epoch, reason };
                }
            }
            Record::Reconciled { key, decision, .. } => {
                if let Some(view) = self.steps.get_mut(&key) {
                    view.state = match decision {
                        Reconciliation::AdoptCompleted { output } => {
                            StepState::Settled(TerminalRecord::Succeeded { output })
                        }
                        // Proving no effects is the only route back to
                        // runnable, and it takes a recorded decision.
                        Reconciliation::ProvedNoEffects => StepState::Pending,
                        Reconciliation::Abandoned { note } => {
                            StepState::Settled(TerminalRecord::Failed {
                                kind: "abandoned".to_string(),
                                message: note,
                            })
                        }
                    };
                }
            }
            Record::CancelRequested { run_id, reason } => {
                self.cancelled_runs.insert(run_id, reason);
            }
        }
    }

    pub fn view(&self, key: &StepKey) -> Option<&StepView> {
        self.steps.get(key)
    }

    pub fn settled_output(&self, key: &StepKey) -> Option<&str> {
        match &self.steps.get(key)?.state {
            StepState::Settled(TerminalRecord::Succeeded { output }) => Some(output),
            _ => None,
        }
    }

    pub fn is_cancelled(&self, run_id: &str) -> Option<&str> {
        self.cancelled_runs.get(run_id).map(String::as_str)
    }

    /// Steps whose claim was lost, grouped by what recovery is permitted.
    ///
    /// Losing a claim before launch is safe to retry: nothing ran. Losing it
    /// after launch is not, and never becomes so without a recorded decision.
    pub fn recover_lost(&mut self, worker: &str) -> std::io::Result<Recovery> {
        let mut recovery = Recovery::default();
        let lost: Vec<(StepKey, StepState)> = self
            .steps
            .iter()
            .filter(|(_, view)| {
                matches!(view.state, StepState::Claimed(_) | StepState::Launched(_))
            })
            .map(|(key, view)| (key.clone(), view.state.clone()))
            .collect();

        for (key, state) in lost {
            match state {
                StepState::Claimed(epoch) => {
                    // Nothing was launched, so a new epoch may take it.
                    let next = Epoch(epoch.0 + 1);
                    self.append(Record::Claimed {
                        key: key.clone(),
                        epoch: next,
                        worker: worker.to_string(),
                    })?;
                    recovery.refenced.push((key, next));
                }
                StepState::Launched(epoch) => {
                    self.append(Record::Uncertain {
                        key: key.clone(),
                        epoch,
                        reason: "worker lost ownership after launch".to_string(),
                    })?;
                    recovery.uncertain.push(key);
                }
                _ => {}
            }
        }
        Ok(recovery)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[derive(Debug, Default)]
pub struct Recovery {
    /// Reclaimed under a new fencing epoch, safe because nothing launched.
    pub refenced: Vec<(StepKey, Epoch)>,
    /// Left for reconciliation. Never relaunched automatically.
    pub uncertain: Vec<StepKey>,
}

impl StepView {
    fn is_terminal(&self) -> bool {
        matches!(self.state, StepState::Settled(_))
    }
}

/// Truncate a provider result to what the store is willing to keep.
pub fn bounded_output(output: &str) -> String {
    if output.len() <= MAX_RECORDED_OUTPUT {
        return output.to_string();
    }
    let mut cut = MAX_RECORDED_OUTPUT;
    while !output.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &output[..cut])
}
