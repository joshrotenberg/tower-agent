# tower-agent tasks

# Format, lint, and test. Run before every push.
check:
    cargo fmt --all -- --check
    ./scripts/check-core-deps.sh
    cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
    just check-feature-matrix
    cargo test --workspace --all-targets --all-features --locked
    cargo test --workspace --doc --all-features --locked
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --locked --no-deps
    cargo package -p tower-agent -p tower-agent-claude -p tower-agent-codex --allow-dirty --locked --no-verify

# Lint the provider-feature combinations the all-features build cannot reach.
check-feature-matrix:
    cargo clippy -p tower-agent-plan --all-targets --locked -- -D warnings
    cargo clippy -p tower-agent-plan --all-targets --locked --features claude -- -D warnings
    cargo clippy -p tower-agent-plan --all-targets --locked --features codex -- -D warnings

fmt:
    cargo fmt --all

# Run one prompt through a native provider service.
run prompt provider="claude":
    cargo run -p agent-example -- --provider "{{provider}}" "{{prompt}}"
