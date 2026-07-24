# tower-agent tasks

# Format, lint, and test. Run before every push.
check:
    cargo fmt --all -- --check
    cargo clippy --all-targets -- -D warnings
    cargo test

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
