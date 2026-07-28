#!/usr/bin/env bash
set -euo pipefail

expected="8eb210175687c9f0c889a3b6795c16781d830e3a"
revs=$(grep -oE 'github\.com/paradigmxyz/reth\?[^"]*#[0-9a-f]{40}' Cargo.lock |
    sed 's/.*#//' |
    sort -u)
count=$(printf '%s' "$revs" | grep -c . || true)

if [ "$count" -ne 1 ] || [ "$revs" != "$expected" ]; then
    echo "Error: expected only Reth revision $expected; found $count revision(s):" >&2
    printf '%s\n' "$revs" >&2
    exit 1
fi

echo "single expected Reth revision in Cargo.lock: $revs"
