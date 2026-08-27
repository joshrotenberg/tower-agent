//! Restart proved at the process boundary.
//!
//! The unit tests drop and rebuild a store inside one process, which shows
//! state comes from disk but leaves an allocator and a page cache in common.
//! This runs the binary twice, so the second process begins with nothing but
//! a file path.

use std::process::Command;

fn run(binary: &str, phase: &str, log: &std::path::Path) -> String {
    let output = Command::new(binary)
        .args(["--phase", phase, "--log", log.to_str().expect("utf-8 path")])
        .output()
        .unwrap_or_else(|error| panic!("phase {phase} runs: {error}"));
    assert!(
        output.status.success(),
        "phase {phase} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn a_second_process_reuses_what_the_first_committed() {
    let temp = std::env::temp_dir().join(format!("apalis-durable-proc-{}", std::process::id()));
    std::fs::create_dir_all(&temp).expect("temp dir");
    let log = temp.join("host.log");
    let binary = env!("CARGO_BIN_EXE_apalis-durable-example");

    let first = run(binary, "one", &log);
    assert!(first.contains("architecture reviewed"), "{first}");

    let second = run(binary, "two", &log);
    // The new process found both roots settled, called no provider for them,
    // and admitted only the step that became ready.
    assert!(second.contains("architecture: AlreadySettled"), "{second}");
    assert!(second.contains("verification: AlreadySettled"), "{second}");
    assert!(second.contains("join: Run("), "{second}");

    std::fs::remove_dir_all(&temp).ok();
}

/// Repeated restarts never re-run settled work, and re-claiming is fenced.
///
/// A step that was claimed but never launched is deliberately re-claimable:
/// nothing ran, and the previous claimer may be dead. What must hold is that
/// each attempt takes a strictly higher epoch, so a resurrected worker cannot
/// commit, and that settled work is never called again.
#[test]
fn repeated_restarts_refence_unlaunched_work_and_never_rerun_settled_work() {
    let temp = std::env::temp_dir().join(format!("apalis-durable-idem-{}", std::process::id()));
    std::fs::create_dir_all(&temp).expect("temp dir");
    let log = temp.join("host.log");
    let binary = env!("CARGO_BIN_EXE_apalis-durable-example");

    run(binary, "one", &log);
    let first = run(binary, "two", &log);
    let second = run(binary, "two", &log);

    // Settled roots are reused every time, so no provider is called again.
    for output in [&first, &second] {
        assert!(output.contains("architecture: AlreadySettled"), "{output}");
        assert!(output.contains("verification: AlreadySettled"), "{output}");
    }

    // The join was claimed but never launched, so each process may retake it,
    // and each must do so under a strictly higher fencing epoch.
    let epoch = |output: &str| -> u64 {
        let marker = "join: Run(Epoch(";
        let start = output
            .find(marker)
            .unwrap_or_else(|| panic!("no join claim in {output}"))
            + marker.len();
        let rest = &output[start..];
        let end = rest.find(')').expect("closing paren");
        rest[..end].parse().expect("epoch is a number")
    };
    assert!(
        epoch(&second) > epoch(&first),
        "epoch must advance: first {}, second {}",
        epoch(&first),
        epoch(&second)
    );

    std::fs::remove_dir_all(&temp).ok();
}
