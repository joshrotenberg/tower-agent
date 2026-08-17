#!/bin/sh

set -eu

tree="$(cargo tree --locked -p tower-agent --edges normal --prefix none)"

if printf '%s\n' "$tree" | grep -Eq '^(tower-mcp|tower-agent-server) v'; then
    printf '%s\n' "tower-agent must not depend on tower-mcp or tower-agent-server"
    exit 1
fi

printf '%s\n' "core dependency boundary is clean"
