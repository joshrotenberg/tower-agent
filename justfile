# tower-agent tasks

# Format, lint, and test. Run before every push.
check:
    cargo fmt --all -- --check
    ./scripts/check-core-deps.sh
    ./scripts/check-license-files.sh
    cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
    just check-feature-matrix
    cargo test --workspace --all-targets --all-features --locked
    cargo test --workspace --doc --all-features --locked
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --locked --no-deps
    cargo package -p tower-agent -p tower-agent-claude -p tower-agent-codex -p tower-agent-workflow --allow-dirty --locked --no-verify
    just check-examples

# Execute the library examples. `--all-targets` only builds them, so an
# example can carry a wrong assertion indefinitely without anyone noticing.
# All of these use fake providers and need no credentials, so a failure here
# is a real defect rather than a missing environment.
check-examples:
    cargo run -q -p tower-agent --all-features --locked --example bulkhead
    cargo run -q -p tower-agent --all-features --locked --example circuit_breaker
    cargo run -q -p tower-agent --all-features --locked --example health_gated_circuit
    cargo run -q -p tower-agent --all-features --locked --example rate_limiter
    cargo run -q -p tower-agent-plan --features claude --locked --example elicitation_loop
    cargo run -q -p tower-agent-mcp --locked --example stdio_server

# Lint the provider-feature combinations the all-features build cannot reach.
check-feature-matrix:
    cargo clippy -p tower-agent-plan --all-targets --locked -- -D warnings
    cargo clippy -p tower-agent-plan --all-targets --locked --features claude -- -D warnings
    cargo clippy -p tower-agent-plan --all-targets --locked --features codex -- -D warnings
    cargo clippy -p tower-agent-mcp --all-targets --locked -- -D warnings
    cargo clippy -p tower-agent-mcp --all-targets --locked --features plan -- -D warnings
    cargo clippy -p tower-agent-mcp --all-targets --locked --features plan-claude -- -D warnings
    cargo clippy -p tower-agent-mcp --all-targets --locked --features plan-codex -- -D warnings

fmt:
    cargo fmt --all

# Run one prompt through a native provider service.
run prompt provider="claude":
    cargo run -p agent-example -- --provider "{{provider}}" "{{prompt}}"
