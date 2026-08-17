# tower-agent tasks

# Format, lint, and test. Run before every push.
check:
    cargo fmt --all -- --check
    ./scripts/check-core-deps.sh
    cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
    cargo test --workspace --all-targets --all-features --locked
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --locked --no-deps

fmt:
    cargo fmt --all

# Run a single prompt through the stub backend.
run prompt agent="scout":
    cargo run -p agent -- run "{{prompt}}" --agent "{{agent}}"

# List configured agents.
list:
    cargo run -p agent -- list

# Serve the agent server over stdio (MCP).
serve:
    cargo run -p agent -- serve
