//! Restart recovery for a workflow host backed by persistent Apalis storage.
//!
//! #105 proved the workflow/Apalis boundary with in-memory storage and stopped
//! at the process boundary. This answers what #108 asked next: do frozen
//! invocations, claim fencing, typed results, deadlines, and cancellation
//! survive a restart?
//!
//! The boundary from #105 is unchanged. The workflow runner owns graph
//! readiness. Apalis transports only opaque, versioned job references. The
//! host store owns identity, claims, launch state, terminal results, and
//! reconciliation. Provider and cancellation objects are rebuilt locally.
//!
//! Run it to see the phases, or read the tests, which are the actual proof.

mod store;

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use store::{DurableStore, Epoch, Record, StepKey, StepState, TerminalRecord, bounded_output};

/// The only thing that crosses the queue: which step, and which shape of
/// reference this is. No request, no options, no credentials, no session.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct StepJobRef {
    schema_version: u16,
    run_id: String,
    step_id: String,
}

const JOB_REF_SCHEMA_VERSION: u16 = 1;

fn now_utc_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after the epoch")
        .as_millis() as u64
}

/// Rebuild a local deadline from persisted wall-clock time.
///
/// Conservative on purpose. An `Instant` cannot cross a restart, so the store
/// keeps UTC milliseconds and the deadline is reconstructed against the
/// current clock. Work whose deadline has passed gets `None` and must not
/// launch: the alternative, treating an expired deadline as "plenty of time",
/// is how a restart silently doubles a budget.
fn reconstruct_deadline(not_after_utc_ms: Option<u64>) -> DeadlineDecision {
    let Some(not_after) = not_after_utc_ms else {
        return DeadlineDecision::NoDeadline;
    };
    let now = now_utc_ms();
    if not_after <= now {
        return DeadlineDecision::Expired;
    }
    DeadlineDecision::Remaining(Duration::from_millis(not_after - now))
}

#[derive(Debug, PartialEq, Eq)]
enum DeadlineDecision {
    NoDeadline,
    Remaining(Duration),
    Expired,
}

/// A step the host is willing to execute right now.
#[derive(Debug, PartialEq, Eq)]
enum Admission {
    /// Run it under this fencing epoch.
    Run(Epoch),
    /// Already settled; reuse the recorded result rather than calling again.
    AlreadySettled,
    /// Refused, with the reason a person would want.
    Refused(String),
}

/// Decide whether a delivered job reference may run.
///
/// Every refusal here is a call that does not happen, which for an operation
/// that spends money is the point.
fn admit(
    store: &mut DurableStore,
    job: &StepJobRef,
    fingerprint: &str,
    worker: &str,
) -> Result<Admission> {
    if job.schema_version != JOB_REF_SCHEMA_VERSION {
        return Ok(Admission::Refused(format!(
            "job reference schema {} is not {JOB_REF_SCHEMA_VERSION}",
            job.schema_version
        )));
    }
    let key = StepKey {
        run_id: job.run_id.clone(),
        step_id: job.step_id.clone(),
    };
    if let Some(reason) = store.is_cancelled(&job.run_id) {
        return Ok(Admission::Refused(format!("run cancelled: {reason}")));
    }
    let Some(view) = store.view(&key) else {
        return Ok(Admission::Refused("step was never frozen".to_string()));
    };
    if view.fingerprint != fingerprint {
        // Redelivery describing different work is a bug or an attack, never
        // something to execute.
        return Ok(Admission::Refused(
            "delivered work does not match the frozen invocation".to_string(),
        ));
    }
    match &view.state {
        // The whole point of committing before acknowledging: a redelivery
        // finds the answer already there and calls no provider.
        StepState::Settled(_) => Ok(Admission::AlreadySettled),
        StepState::Uncertain { reason, .. } => Ok(Admission::Refused(format!(
            "uncertain and awaiting reconciliation: {reason}"
        ))),
        StepState::Launched(epoch) => Ok(Admission::Refused(format!(
            "already launched under epoch {}",
            epoch.0
        ))),
        StepState::Claimed(_) | StepState::Pending => {
            match reconstruct_deadline(view.not_after_utc_ms) {
                DeadlineDecision::Expired => Ok(Admission::Refused(
                    "deadline passed before this delivery".to_string(),
                )),
                _ => {
                    let epoch = Epoch(view.highest_epoch.0 + 1);
                    store.append(Record::Claimed {
                        key,
                        epoch,
                        worker: worker.to_string(),
                    })?;
                    Ok(Admission::Run(epoch))
                }
            }
        }
    }
}

/// Execute an admitted step and commit its result before anything else.
async fn execute(
    store: &mut DurableStore,
    key: StepKey,
    epoch: Epoch,
    provider: impl std::future::Future<Output = Result<String, String>>,
) -> Result<TerminalRecord> {
    store.append(Record::Launched {
        key: key.clone(),
        epoch,
    })?;
    let result = match provider.await {
        Ok(output) => TerminalRecord::Succeeded {
            output: bounded_output(&output),
        },
        Err(message) => TerminalRecord::Failed {
            kind: "provider".to_string(),
            message,
        },
    };
    // Commit before the queue hears anything. If the process dies between
    // this line and the acknowledgement, redelivery finds the result.
    store.append(Record::Settled {
        key,
        epoch,
        result: result.clone(),
    })?;
    Ok(result)
}

/// Enqueue the steps of a run into persistent Apalis storage.
///
/// Exercised by the queue tests rather than the demo phases, which is the
/// point: the queue carries readiness, and the host store carries truth.
#[cfg_attr(not(test), allow(dead_code))]
///
/// Only the opaque reference crosses. The queue is a transport for "this step
/// is ready", never a place the workflow's meaning lives.
async fn enqueue(path: &std::path::Path, refs: &[StepJobRef]) -> Result<usize> {
    use apalis::prelude::*;
    use futures_util::SinkExt;

    let mut storage: apalis_file_storage::JsonStorage<StepJobRef> =
        apalis_file_storage::JsonStorage::new(path)?;
    for job in refs {
        storage.push(job.clone()).await?;
    }
    // Pushing alone buffers in memory: this backend's `start_send` fills a
    // buffer and only `poll_flush` writes it. A host that enqueues and then
    // dies without flushing has enqueued nothing, which is exactly the class
    // of durability gap this spike exists to find.
    storage.flush().await?;
    Ok(refs.len())
}

/// Count what persistent Apalis storage still holds after a restart.
#[cfg_attr(not(test), allow(dead_code))]
async fn queued_after_reopen(path: &std::path::Path) -> Result<usize> {
    use futures_util::StreamExt;
    let storage: apalis_file_storage::JsonStorage<StepJobRef> =
        apalis_file_storage::JsonStorage::new(path)?;
    let mut stream = Box::pin(storage);
    let mut seen = 0usize;
    // The stream stays open waiting for future work, so this takes what is
    // already there and stops. Counting what a worker would be handed, not
    // running one.
    while let Ok(Some(_task)) =
        tokio::time::timeout(Duration::from_millis(250), stream.next()).await
    {
        seen += 1;
    }
    Ok(seen)
}

/// Phase one: freeze and settle the two roots, then stop.
async fn phase_one(log: &std::path::Path) -> Result<()> {
    let mut store = DurableStore::open(log)?;
    for step in ["architecture", "verification"] {
        freeze(&mut store, "run-1", step, "fp-1", None)?;
        if let Admission::Run(epoch) =
            admit(&mut store, &job_ref("run-1", step), "fp-1", "worker-a")?
        {
            let outcome = execute(&mut store, key("run-1", step), epoch, async {
                Ok(format!("{step} reviewed"))
            })
            .await?;
            println!("phase-one {step}: {outcome:?}");
        }
    }
    Ok(())
}

/// Phase two: a different process, holding nothing but the log path.
async fn phase_two(log: &std::path::Path) -> Result<()> {
    let mut store = DurableStore::open(log)?;
    for step in ["architecture", "verification"] {
        let decision = admit(&mut store, &job_ref("run-1", step), "fp-1", "worker-b")?;
        println!("phase-two {step}: {decision:?}");
    }
    freeze(&mut store, "run-1", "join", "fp-1", None)?;
    println!(
        "phase-two join: {:?}",
        admit(&mut store, &job_ref("run-1", "join"), "fp-1", "worker-b")?
    );
    Ok(())
}

/// Phase three: show what a worker's death leaves behind.
///
/// Two steps are abandoned deliberately, one before launch and one after, so
/// the difference between "safe to retake" and "nobody knows" is visible.
async fn phase_three(log: &std::path::Path) -> Result<()> {
    let mut store = DurableStore::open(log)?;

    freeze(&mut store, "run-2", "claimed-only", "fp-2", None)?;
    admit(
        &mut store,
        &job_ref("run-2", "claimed-only"),
        "fp-2",
        "worker-c",
    )?;

    freeze(&mut store, "run-2", "launched", "fp-2", None)?;
    if let Admission::Run(epoch) = admit(
        &mut store,
        &job_ref("run-2", "launched"),
        "fp-2",
        "worker-c",
    )? {
        store.append(Record::Launched {
            key: key("run-2", "launched"),
            epoch,
        })?;
    }

    // worker-c dies here. A survivor sweeps up.
    let recovery = store.recover_lost("worker-d")?;
    println!(
        "phase-three refenced: {:?}",
        recovery
            .refenced
            .iter()
            .map(|(k, e)| format!("{}@{}", k.step_id, e.0))
            .collect::<Vec<_>>()
    );
    println!(
        "phase-three uncertain: {:?}",
        recovery
            .uncertain
            .iter()
            .map(|k| k.step_id.clone())
            .collect::<Vec<_>>()
    );
    println!("phase-three log: {}", store.path().display());
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    // Driven as a subprocess by the restart test, so each phase is a real
    // process with nothing carried over but a file path.
    let args: Vec<String> = std::env::args().collect();
    let phase = args
        .iter()
        .position(|a| a == "--phase")
        .map(|i| args[i + 1].clone());
    let log = args
        .iter()
        .position(|a| a == "--log")
        .map(|i| std::path::PathBuf::from(&args[i + 1]));

    match (phase.as_deref(), log) {
        (Some("one"), Some(log)) => return phase_one(&log).await,
        (Some("two"), Some(log)) => return phase_two(&log).await,
        (Some("three"), Some(log)) => return phase_three(&log).await,
        _ => {}
    }

    let dir = std::env::temp_dir().join(format!("apalis-durable-demo-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let log = dir.join("host.log");
    println!("== phase one ==");
    phase_one(&log).await?;
    println!("== phase two, same log, fresh state ==");
    phase_two(&log).await?;
    println!("== phase three, recovering a dead worker's claims ==");
    phase_three(&log).await?;
    for step in ["architecture", "verification"] {
        let store = DurableStore::open(&log)?;
        println!(
            "  {step} settled output: {:?}",
            store.settled_output(&key("run-1", step))
        );
    }
    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}

fn key(run: &str, step: &str) -> StepKey {
    StepKey {
        run_id: run.to_string(),
        step_id: step.to_string(),
    }
}

fn job_ref(run: &str, step: &str) -> StepJobRef {
    StepJobRef {
        schema_version: JOB_REF_SCHEMA_VERSION,
        run_id: run.to_string(),
        step_id: step.to_string(),
    }
}

fn freeze(
    store: &mut DurableStore,
    run: &str,
    step: &str,
    fingerprint: &str,
    not_after_utc_ms: Option<u64>,
) -> Result<()> {
    store.append(Record::Frozen {
        key: key(run, step),
        workflow_id: "repository-review".to_string(),
        workflow_version: "v1".to_string(),
        fingerprint: fingerprint.to_string(),
        not_after_utc_ms,
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use store::Reconciliation;

    struct Temp(std::path::PathBuf);

    impl Temp {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "apalis-durable-{name}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            std::fs::create_dir_all(&dir).expect("temp dir");
            Self(dir)
        }
        fn log(&self) -> std::path::PathBuf {
            self.0.join("host.log")
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }

    /// Settled roots survive, and only the newly ready step is admitted.
    #[tokio::test]
    async fn a_restart_reuses_settled_roots_and_admits_only_the_join() {
        let temp = Temp::new("replay");
        {
            let mut store = DurableStore::open(temp.log()).expect("open");
            for step in ["architecture", "verification"] {
                freeze(&mut store, "r", step, "fp", None).expect("freeze");
                let Admission::Run(epoch) =
                    admit(&mut store, &job_ref("r", step), "fp", "w1").expect("admit")
                else {
                    panic!("first delivery should run");
                };
                execute(&mut store, key("r", step), epoch, async {
                    Ok("done".into())
                })
                .await
                .expect("execute");
            }
        }

        // A new store, reading only the file.
        let mut store = DurableStore::open(temp.log()).expect("reopen");
        for step in ["architecture", "verification"] {
            assert_eq!(
                admit(&mut store, &job_ref("r", step), "fp", "w2").expect("admit"),
                Admission::AlreadySettled,
                "{step} was settled before the restart"
            );
        }
        freeze(&mut store, "r", "join", "fp", None).expect("freeze");
        assert!(matches!(
            admit(&mut store, &job_ref("r", "join"), "fp", "w2").expect("admit"),
            Admission::Run(_)
        ));
    }

    /// Work lost while claimed never launched, so a new epoch may take it.
    #[tokio::test]
    async fn work_lost_before_launch_returns_under_a_new_epoch() {
        let temp = Temp::new("claimed");
        {
            let mut store = DurableStore::open(temp.log()).expect("open");
            freeze(&mut store, "r", "s", "fp", None).expect("freeze");
            admit(&mut store, &job_ref("r", "s"), "fp", "w1").expect("admit");
            // Process dies holding the claim.
        }

        let mut store = DurableStore::open(temp.log()).expect("reopen");
        let recovery = store.recover_lost("w2").expect("recover");
        assert_eq!(recovery.refenced.len(), 1);
        assert!(recovery.uncertain.is_empty());
        let (recovered, epoch) = &recovery.refenced[0];
        assert_eq!(recovered.step_id, "s");
        // Strictly newer, so a resurrected old worker cannot commit.
        assert!(epoch.0 > 1);
    }

    /// Work lost after launch may already have acted, so it is never
    /// relaunched on its own.
    #[tokio::test]
    async fn work_lost_after_launch_becomes_uncertain_and_stays_there() {
        let temp = Temp::new("launched");
        {
            let mut store = DurableStore::open(temp.log()).expect("open");
            freeze(&mut store, "r", "s", "fp", None).expect("freeze");
            let Admission::Run(epoch) =
                admit(&mut store, &job_ref("r", "s"), "fp", "w1").expect("admit")
            else {
                panic!("should run");
            };
            store
                .append(Record::Launched {
                    key: key("r", "s"),
                    epoch,
                })
                .expect("launch");
            // Killed between launch and settlement.
        }

        let mut store = DurableStore::open(temp.log()).expect("reopen");
        let recovery = store.recover_lost("w2").expect("recover");
        assert!(recovery.refenced.is_empty(), "must not be re-fenced");
        assert_eq!(recovery.uncertain.len(), 1);

        // And redelivery refuses rather than calling the provider again.
        let decision = admit(&mut store, &job_ref("r", "s"), "fp", "w2").expect("admit");
        let Admission::Refused(reason) = decision else {
            panic!("uncertain work must not run, got {decision:?}");
        };
        assert!(reason.contains("uncertain"), "{reason}");
    }

    /// A result from a fenced-out claim is logged but never becomes the answer.
    #[tokio::test]
    async fn a_stale_completion_is_rejected() {
        let temp = Temp::new("stale");
        let mut store = DurableStore::open(temp.log()).expect("open");
        freeze(&mut store, "r", "s", "fp", None).expect("freeze");
        admit(&mut store, &job_ref("r", "s"), "fp", "w1").expect("admit"); // epoch 1
        let recovery = store.recover_lost("w2").expect("recover"); // epoch 2
        let (_, current) = recovery.refenced[0];

        // The old worker comes back and tries to commit under epoch 1.
        store
            .append(Record::Settled {
                key: key("r", "s"),
                epoch: Epoch(1),
                result: TerminalRecord::Succeeded {
                    output: "stale".into(),
                },
            })
            .expect("append");

        assert_eq!(store.settled_output(&key("r", "s")), None);
        assert!(matches!(
            store.view(&key("r", "s")).expect("view").state,
            StepState::Claimed(epoch) if epoch == current
        ));
    }

    /// Committing before acknowledging is what makes redelivery free.
    #[tokio::test]
    async fn terminal_commit_before_ack_survives_redelivery() {
        let temp = Temp::new("redeliver");
        let calls = std::sync::Arc::new(std::sync::Mutex::new(0usize));
        let mut store = DurableStore::open(temp.log()).expect("open");
        freeze(&mut store, "r", "s", "fp", None).expect("freeze");

        for _ in 0..3 {
            match admit(&mut store, &job_ref("r", "s"), "fp", "w1").expect("admit") {
                Admission::Run(epoch) => {
                    let seen = calls.clone();
                    execute(&mut store, key("r", "s"), epoch, async move {
                        *seen.lock().unwrap() += 1;
                        Ok("answered".into())
                    })
                    .await
                    .expect("execute");
                }
                Admission::AlreadySettled => {}
                other => panic!("unexpected {other:?}"),
            }
        }

        // Three deliveries, one provider call.
        assert_eq!(*calls.lock().unwrap(), 1);
        assert_eq!(store.settled_output(&key("r", "s")), Some("answered"));
    }

    /// A redelivery describing different work is refused, not executed.
    #[tokio::test]
    async fn a_fingerprint_mismatch_is_refused() {
        let temp = Temp::new("fingerprint");
        let mut store = DurableStore::open(temp.log()).expect("open");
        freeze(&mut store, "r", "s", "fp-original", None).expect("freeze");
        let decision = admit(&mut store, &job_ref("r", "s"), "fp-different", "w1").expect("admit");
        let Admission::Refused(reason) = decision else {
            panic!("mismatched work must not run");
        };
        assert!(reason.contains("frozen invocation"), "{reason}");
    }

    /// An `Instant` cannot survive a restart, so the deadline is rebuilt from
    /// wall-clock time, and an expired one launches nothing.
    #[tokio::test]
    async fn an_expired_deadline_admits_nothing_after_restart() {
        let temp = Temp::new("deadline");
        let already_passed = now_utc_ms() - 1_000;
        {
            let mut store = DurableStore::open(temp.log()).expect("open");
            freeze(&mut store, "r", "s", "fp", Some(already_passed)).expect("freeze");
        }

        let mut store = DurableStore::open(temp.log()).expect("reopen");
        let decision = admit(&mut store, &job_ref("r", "s"), "fp", "w1").expect("admit");
        let Admission::Refused(reason) = decision else {
            panic!("expired work must not launch, got {decision:?}");
        };
        assert!(reason.contains("deadline"), "{reason}");

        assert_eq!(
            reconstruct_deadline(Some(already_passed)),
            DeadlineDecision::Expired
        );
        assert!(matches!(
            reconstruct_deadline(Some(now_utc_ms() + 60_000)),
            DeadlineDecision::Remaining(_)
        ));
    }

    /// Cancellation intent is durable, so a restart does not resume a run
    /// somebody stopped.
    #[tokio::test]
    async fn durable_cancellation_survives_restart() {
        let temp = Temp::new("cancel");
        {
            let mut store = DurableStore::open(temp.log()).expect("open");
            freeze(&mut store, "r", "s", "fp", None).expect("freeze");
            store
                .append(Record::CancelRequested {
                    run_id: "r".into(),
                    reason: "operator stopped the run".into(),
                })
                .expect("cancel");
        }

        let mut store = DurableStore::open(temp.log()).expect("reopen");
        assert_eq!(store.is_cancelled("r"), Some("operator stopped the run"));
        let decision = admit(&mut store, &job_ref("r", "s"), "fp", "w1").expect("admit");
        assert!(matches!(decision, Admission::Refused(reason) if reason.contains("cancelled")));
    }

    /// Uncertain work resolves only by a recorded decision, and each decision
    /// leads somewhere different.
    #[tokio::test]
    async fn reconciliation_is_explicit_and_auditable() {
        let temp = Temp::new("reconcile");
        let mut store = DurableStore::open(temp.log()).expect("open");
        for step in ["adopt", "replay", "abandon"] {
            freeze(&mut store, "r", step, "fp", None).expect("freeze");
            let Admission::Run(epoch) =
                admit(&mut store, &job_ref("r", step), "fp", "w1").expect("admit")
            else {
                panic!("should run");
            };
            store
                .append(Record::Launched {
                    key: key("r", step),
                    epoch,
                })
                .expect("launch");
            store
                .append(Record::Uncertain {
                    key: key("r", step),
                    epoch,
                    reason: "lost after launch".into(),
                })
                .expect("uncertain");
        }

        // Evidence showed it finished: adopt the result, call nothing.
        store
            .append(Record::Reconciled {
                key: key("r", "adopt"),
                decision: Reconciliation::AdoptCompleted {
                    output: "recovered from provider evidence".into(),
                },
                by: "operator:josh".into(),
            })
            .expect("adopt");
        assert_eq!(
            store.settled_output(&key("r", "adopt")),
            Some("recovered from provider evidence")
        );

        // Evidence showed it never ran: replaying is safe.
        store
            .append(Record::Reconciled {
                key: key("r", "replay"),
                decision: Reconciliation::ProvedNoEffects,
                by: "operator:josh".into(),
            })
            .expect("replay");
        assert!(matches!(
            admit(&mut store, &job_ref("r", "replay"), "fp", "w2").expect("admit"),
            Admission::Run(_)
        ));

        // Abandoned: terminal, and descendants stay blocked behind a failure.
        store
            .append(Record::Reconciled {
                key: key("r", "abandon"),
                decision: Reconciliation::Abandoned {
                    note: "cannot determine whether the PR was opened".into(),
                },
                by: "operator:josh".into(),
            })
            .expect("abandon");
        assert!(matches!(
            store.view(&key("r", "abandon")).expect("view").state,
            StepState::Settled(TerminalRecord::Failed { .. })
        ));

        // Every decision is in the log with its author.
        let log = std::fs::read_to_string(store.path()).expect("read log");
        assert_eq!(log.matches("\"operator:josh\"").count(), 3);
    }

    /// A process killed mid-write leaves a partial line. Refusing to start
    /// would make a crash unrecoverable.
    #[tokio::test]
    async fn a_torn_final_line_does_not_prevent_recovery() {
        let temp = Temp::new("torn");
        {
            let mut store = DurableStore::open(temp.log()).expect("open");
            freeze(&mut store, "r", "s", "fp", None).expect("freeze");
        }
        {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(temp.log())
                .expect("open for append");
            write!(file, "{{\"record\":\"claimed\",\"key\":{{\"run_id\":\"r\"")
                .expect("torn write");
        }

        let store = DurableStore::open(temp.log()).expect("reopen despite a torn tail");
        assert!(store.view(&key("r", "s")).is_some());
    }

    /// The store keeps a bounded result, because a provider can produce an
    /// unbounded one and a durable store that writes all of it fills a disk.
    #[test]
    fn recorded_output_is_bounded() {
        let long = "x".repeat(store::MAX_RECORDED_OUTPUT * 2);
        let recorded = bounded_output(&long);
        assert!(recorded.len() <= store::MAX_RECORDED_OUTPUT + 4);
        assert!(recorded.ends_with('…'));
    }
}

#[cfg(test)]
mod queue_tests {
    //! What the queue does and does not guarantee. Process-boundary restart
    //! lives in `tests/restart.rs`, where cargo exposes the built binary.

    use super::*;

    /// A finding, asserted so it cannot regress silently.
    ///
    /// `apalis-file-storage` 0.1.0-rc.9 does not write a pushed task to disk.
    /// Its `insert` only touches the in-memory map, and `persist_to_disk`
    /// runs from `remove`, from the acknowledge path, and when a task is
    /// polled out. Work that is enqueued and never polled is lost with the
    /// process.
    ///
    /// This is asserted rather than worked around because it is the honest
    /// state of the only storage backend published against apalis
    /// 1.0.0-rc.9, and because the design does not depend on it: the queue
    /// carries readiness, not truth. See `docs/durable-host-report.md`.
    #[tokio::test]
    async fn queued_references_are_not_durable_in_this_backend() {
        let temp = std::env::temp_dir().join(format!("apalis-durable-q-{}", std::process::id()));
        std::fs::create_dir_all(&temp).expect("temp dir");
        let queue = temp.join("queue.jsonl");

        let refs = vec![
            job_ref("run-1", "architecture"),
            job_ref("run-1", "verification"),
        ];
        assert_eq!(enqueue(&queue, &refs).await.expect("enqueue"), 2);

        let recovered = queued_after_reopen(&queue).await.expect("reopen");
        assert_eq!(
            recovered, 0,
            "if this backend gained enqueue durability, the report needs updating"
        );

        std::fs::remove_dir_all(&temp).ok();
    }

    /// The mitigation, and the reason the finding above is survivable.
    ///
    /// The host store is authoritative, so readiness is re-derived from it
    /// rather than trusted to the queue. A run whose queue was lost entirely
    /// still knows which steps settled and which are now ready, so a
    /// coordinator can enqueue again without calling a provider twice.
    #[tokio::test]
    async fn readiness_is_rederived_from_the_store_when_the_queue_is_lost() {
        let temp = std::env::temp_dir().join(format!("apalis-durable-rd-{}", std::process::id()));
        std::fs::create_dir_all(&temp).expect("temp dir");
        let log = temp.join("host.log");

        {
            let mut store = DurableStore::open(&log).expect("open");
            for step in ["architecture", "verification"] {
                freeze(&mut store, "r", step, "fp", None).expect("freeze");
                let Admission::Run(epoch) =
                    admit(&mut store, &job_ref("r", step), "fp", "w1").expect("admit")
                else {
                    panic!("should run");
                };
                execute(&mut store, key("r", step), epoch, async {
                    Ok("done".into())
                })
                .await
                .expect("execute");
            }
            freeze(&mut store, "r", "join", "fp", None).expect("freeze");
        }

        // The queue is simply gone. Nothing was acknowledged, nothing replayed.
        let mut store = DurableStore::open(&log).expect("reopen");
        let settled: Vec<&str> = ["architecture", "verification"]
            .into_iter()
            .filter(|step| store.settled_output(&key("r", step)).is_some())
            .collect();
        assert_eq!(settled.len(), 2, "settled work is known without the queue");

        // And the one step that became ready can be enqueued again, once.
        assert!(matches!(
            admit(&mut store, &job_ref("r", "join"), "fp", "w2").expect("admit"),
            Admission::Run(_)
        ));

        std::fs::remove_dir_all(&temp).ok();
    }
}
