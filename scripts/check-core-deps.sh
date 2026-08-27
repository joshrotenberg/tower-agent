#!/bin/sh

set -eu

tree="$(cargo tree --locked -p tower-agent --edges normal --prefix none)"

if printf '%s\n' "$tree" | grep -Eq '^(tower-mcp|tower-agent-workflow|tower-agent-apalis|apalis[^[:space:]]*) v'; then
    printf '%s\n' "tower-agent must not depend on an interface or orchestration implementation"
    exit 1
fi

if printf '%s\n' "$tree" | grep -Eq '^tower-agent-plan v'; then
    printf '%s\n' "tower-agent must not depend on the planning crate"
    exit 1
fi

printf '%s\n' "core dependency boundary is clean"
