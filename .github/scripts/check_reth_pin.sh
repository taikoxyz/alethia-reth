#!/usr/bin/env bash
#
# Fails when the lockfile resolves more than one paradigmxyz/reth revision. reth-optimism-trie
# resolves its reth dependencies inside the OP monorepo workspace, so the graph only stays
# coherent while Alethia's reth pin references the exact commit OP pins (see the
# reth-optimism-trie note in Cargo.toml); a drifted pin splits the workspace into two
# incompatible reth copies.
#
# Also fails on multiple paradigmxyz/revmc revisions: Alethia pins revmc by rev while reth's
# own (unused) `jit` feature declares it by branch, so re-enabling that reth feature would
# otherwise split the graph into two revmc copies silently.
#
# Finally, verifies Alethia's revmc revision equals the one reth's own Cargo.lock pins at our
# reth revision. reth tracks revmc by branch in its manifest, so nothing enforces this at
# build time — a reth bump that forgets to move Alethia's revmc rev would build fine while
# compiled-code semantics silently drift from what upstream tested against.
set -euo pipefail

single_rev() {
    local repo=$1
    local revs count
    revs=$(grep -oE "github\.com/paradigmxyz/${repo}(\.git)?\?[^\"]*#[0-9a-f]{40}" Cargo.lock | sed 's/.*#//' | sort -u)
    count=$(printf '%s' "$revs" | grep -c . || true)

    if [ "$count" -ne 1 ]; then
        echo "Error: expected exactly one paradigmxyz/${repo} revision in Cargo.lock, found $count:" >&2
        printf '%s\n' "$revs" >&2
        exit 1
    fi

    printf '%s' "$revs"
}

reth_rev=$(single_rev reth)
echo "single reth revision in Cargo.lock: $reth_rev"
revmc_rev=$(single_rev revmc)
echo "single revmc revision in Cargo.lock: $revmc_rev"

reth_lock_url="https://raw.githubusercontent.com/paradigmxyz/reth/${reth_rev}/Cargo.lock"
if ! reth_lock=$(curl -fsSL --retry 5 "$reth_lock_url"); then
    echo "Error: failed to fetch reth's lockfile from $reth_lock_url" >&2
    exit 1
fi

reth_locked_revmc=$(grep -oE 'github\.com/paradigmxyz/revmc(\.git)?\?[^"]*#[0-9a-f]{40}' <<< "$reth_lock" | sed 's/.*#//' | sort -u)
if [ -z "$reth_locked_revmc" ]; then
    echo "Error: found no locked revmc revision in $reth_lock_url" >&2
    exit 1
fi

if [ "$revmc_rev" != "$reth_locked_revmc" ]; then
    echo "Error: Cargo.lock pins revmc $revmc_rev, but reth $reth_rev locks:" >&2
    printf '%s\n' "$reth_locked_revmc" >&2
    echo "Keep the revmc rev in Cargo.toml matched to the reth pin's locked revmc." >&2
    exit 1
fi

echo "revmc revision matches reth's locked revmc"
