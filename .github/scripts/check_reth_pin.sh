#!/usr/bin/env bash
#
# Fails when the lockfile resolves reth from more than one source.
# reth-optimism-trie resolves its reth dependencies inside the OP monorepo workspace, so the graph
# only stays coherent while Alethia's reth pin references the exact repository and commit OP pins
# (see the reth-optimism-trie note in Cargo.toml). OP may pin a fork rather than paradigmxyz/reth,
# and cargo treats every distinct source string (URL, `.git` suffix, `rev`/`branch` query) as a
# different source, so any drift splits the workspace into two incompatible reth copies.
set -euo pipefail

# Distinct full source strings of every reth git dependency.
sources=$(grep -oE '^source = "git\+https://github\.com/[A-Za-z0-9_.-]+/reth(\.git)?[?#][^"]*"' Cargo.lock |
    sed -E 's/^source = "(.*)"$/\1/' | sort -u)
count=$(printf '%s' "$sources" | grep -c . || true)

if [ "$count" -ne 1 ]; then
    echo "Error: expected exactly one reth git source in Cargo.lock, found $count:" >&2
    printf '%s\n' "$sources" >&2
    exit 1
fi

# A crate resolved from that reth source must not also resolve from another source (e.g. crates.io).
duplicates=$(awk -v reth="$sources" '
    function flush() {
        if (name != "") { if (source == reth) in_reth[name] = 1; else other[name] = 1 }
        name = ""; source = ""
    }
    /^\[\[package\]\]/ { flush() }
    /^name = /         { name = $3; gsub(/"/, "", name) }
    /^source = /       { source = $3; gsub(/"/, "", source) }
    END { flush(); for (n in in_reth) if (n in other) print n }
' Cargo.lock | sort)

if [ -n "$duplicates" ]; then
    echo "Error: crates resolved from the reth git source and from another source:" >&2
    printf '%s\n' "$duplicates" >&2
    exit 1
fi

echo "single reth git source in Cargo.lock: $sources"
