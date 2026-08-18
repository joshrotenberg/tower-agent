# tower-agent tasks

# Format, lint, and test. Run before every push.
check:
    cargo fmt --all -- --check
    ./scripts/check-core-deps.sh
    cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
    cargo test --workspace --all-targets --all-features --locked
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --locked --no-deps
    cargo package -p tower-agent -p tower-agent-claude -p tower-agent-codex --allow-dirty --locked --no-verify

fmt:
    cargo fmt --all

# Run one prompt through a native provider service.
run prompt provider="claude":
    cargo run -p agent-example -- --provider "{{provider}}" "{{prompt}}"
