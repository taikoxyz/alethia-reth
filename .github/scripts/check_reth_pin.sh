#!/usr/bin/env bash
#
# Fails when the lockfile resolves more than one paradigmxyz/reth revision. reth-optimism-trie
# resolves its reth dependencies inside the OP monorepo workspace, so the graph only stays
# coherent while Alethia's reth pin references the exact commit OP pins (see the
# reth-optimism-trie note in Cargo.toml); a drifted pin splits the workspace into two
# incompatible reth copies.
set -euo pipefail

revs=$(grep -oE 'github\.com/paradigmxyz/reth\?[^"]*#[0-9a-f]{40}' Cargo.lock | sed 's/.*#//' | sort -u)
count=$(printf '%s' "$revs" | grep -c . || true)

if [ "$count" -ne 1 ]; then
    echo "Error: expected exactly one paradigmxyz/reth revision in Cargo.lock, found $count:" >&2
    printf '%s\n' "$revs" >&2
    exit 1
fi

echo "single reth revision in Cargo.lock: $revs"
