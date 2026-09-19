#!/usr/bin/env bash
#
# Fails when the lockfile resolves reth from more than one git source (repository or revision).
# reth-optimism-trie resolves its reth dependencies inside the OP monorepo workspace, so the graph
# only stays coherent while Alethia's reth pin references the exact repository and commit OP pins
# (see the reth-optimism-trie note in Cargo.toml). OP may pin a fork rather than paradigmxyz/reth,
# and cargo treats different URLs as different sources, so a drifted URL or rev splits the
# workspace into two incompatible reth copies.
set -euo pipefail

sources=$(grep -oE 'github\.com/[A-Za-z0-9_.-]+/reth\?[^"]*#[0-9a-f]{40}' Cargo.lock |
    sed -E 's|github\.com/([^/]+)/reth\?.*#([0-9a-f]{40})$|\1/reth@\2|' | sort -u)
count=$(printf '%s' "$sources" | grep -c . || true)

if [ "$count" -ne 1 ]; then
    echo "Error: expected exactly one reth git source in Cargo.lock, found $count:" >&2
    printf '%s\n' "$sources" >&2
    exit 1
fi

echo "single reth git source in Cargo.lock: $sources"
