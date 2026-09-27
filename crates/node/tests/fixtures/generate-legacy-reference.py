#!/usr/bin/env python3
"""Reproduce the five historical V2 vectors without changing any checkout's production code.

Run from any directory: python3 crates/node/tests/fixtures/generate-legacy-reference.py
The isolated archive and captured JSON stay in the platform temporary directory for inspection.
"""

import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile

BASE = "ca88961eb1f7de266ad16eda015d3cacb886ff62"
ROOT = Path(__file__).resolve().parents[4]
WORKDIR = Path(tempfile.mkdtemp(prefix="alethia-legacy-ca88961-"))
SNAPSHOT = WORKDIR / "checkout"
SNAPSHOT.mkdir()
OUTPUT = WORKDIR / "historical-vectors.json"


def closing_brace(text, opening):
    depth = 0
    for index in range(opening, len(text)):
        if text[index] == "{":
            depth += 1
        elif text[index] == "}":
            depth -= 1
            if depth == 0:
                return index
    raise ValueError("unbalanced harness function")


archive = subprocess.Popen(["git", "archive", BASE], cwd=ROOT, stdout=subprocess.PIPE)
subprocess.run(["tar", "-x", "-C", str(SNAPSHOT)], stdin=archive.stdout, check=True)
archive.stdout.close()
if archive.wait():
    raise RuntimeError("git archive failed")
for relative in ["Cargo.toml", "Cargo.lock", "crates/node/Cargo.toml",
                 "crates/node/tests/fixtures/tbd-genesis.json",
                 "crates/node/tests/fixtures/tbd-cases.json"]:
    destination = SNAPSHOT / relative
    destination.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(ROOT / relative, destination)

# Adapt only the test harness to APIs present before TBD. Production Rust remains untouched.
support = (ROOT / "crates/node/tests/support/mod.rs").read_text()
support = re.sub(r"forks\.insert\(TaikoHardfork::TBD,[^;]+;", "", support)
support = support.replace("activation: u64", "_activation: u64")
support = re.sub(r"\s*osaka::TaikoExecutionPayloadV3,?", "", support)
start = support.index("let (payload, data, response) = if tbd {")
opening = support.index("{", start)
closing = closing_brace(support, opening)
assert support[closing:closing + 8] == "} else {"
support = support[:start] + "let (payload, data, response) = {" + support[closing + 8:]
start = support.index("pub async fn assert_execution_parity")
opening = support.index("{", start)
closing = closing_brace(support, opening)
support = support[:start] + support[closing + 1:]
support = support.replace("for version in [2, 3]", "for version in [2]")
destination = SNAPSHOT / "crates/node/tests/support/mod.rs"
destination.parent.mkdir(parents=True, exist_ok=True)
destination.write_text("#![allow(dead_code, unused_imports, unused_mut)]\n" + support)

tests = (ROOT / "crates/node/tests/tbd_engine.rs").read_text()
start = tests.index("async fn historical_snapshot")
opening = tests.index("{", start)
closing = closing_brace(tests, opening)
historical = tests[start:closing + 1]
output = "mod support;\nuse support::*;\nuse reth_chainspec::EthChainSpec;\nuse reth_tasks::Runtime;\n"
output += historical
for stage, name in enumerate(["genesis", "ontake", "pacaya", "shasta", "unzen"]):
    output += (f'\n#[test]\nfn historical_{name}() -> eyre::Result<()> {{\n'
               f'    run_live_test(historical_snapshot({stage}, "{name}"))\n}}\n')
(SNAPSHOT / "crates/node/tests/legacy_reference.rs").write_text(output)

# Resolve an explicit relative override against the caller before changing Cargo's cwd.
target = Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "target")).absolute()
env = dict(os.environ, CARGO_TARGET_DIR=str(target), TBD_VECTOR_OUTPUT=str(OUTPUT))
subprocess.run(["cargo", "test", "--offline", "--manifest-path", str(SNAPSHOT / "Cargo.toml"),
                "-p", "alethia-reth-node", "--test", "legacy_reference", "--all-features"],
               cwd=ROOT, env=env, check=True)
actual = json.loads(OUTPUT.read_text())
expected = json.loads((ROOT / "crates/node/tests/fixtures/tbd-cases.json").read_text())
assert len(actual) == 5
for name, value in actual.items():
    assert value == expected[name], f"independent legacy mismatch: {name}"
print(f"Five independent legacy vectors match. Archive: {SNAPSHOT}; capture: {OUTPUT}")
