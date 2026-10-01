#!/usr/bin/env python3
"""Reproduce synthetic legacy vectors from the pinned v1.4.1 production sources.

Run from any directory. Requires Python 3.11+ and the pinned Rust dependencies cached locally.
The isolated archive, vectors, and run manifest remain in the platform temporary directory.
This compares synthetic execution, not public-chain historical blocks or another client.
"""

import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import tomllib

BASE_TAG = "v1.4.1"
BASE = "0fb47d966f290c032e0ce88bdc8877121768d253"
ROOT = Path(__file__).resolve().parents[4]
WORKDIR = Path(tempfile.mkdtemp(prefix="alethia-legacy-v1.4.1-"))
SNAPSHOT = WORKDIR / "checkout"
OUTPUT = WORKDIR / "historical-vectors.json"
MANIFEST = WORKDIR / "historical-run.json"
SNAPSHOT.mkdir()


def sha256(data):
    return hashlib.sha256(data).hexdigest()


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


def lock_identities(lock):
    return {
        (package["name"], package["version"], package.get("source", ""),
         package.get("checksum", ""))
        for package in tomllib.loads(lock.decode())["package"]
    }


resolved = subprocess.check_output(
    ["git", "rev-parse", f"{BASE_TAG}^{{commit}}"], cwd=ROOT, text=True).strip()
assert resolved == BASE, f"{BASE_TAG} no longer resolves to the pinned release"
archive = subprocess.Popen(["git", "archive", BASE], cwd=ROOT, stdout=subprocess.PIPE)
subprocess.run(["tar", "-x", "-C", str(SNAPSHOT)], stdin=archive.stdout, check=True)
archive.stdout.close()
if archive.wait():
    raise RuntimeError("git archive failed")
baseline_lock = (SNAPSHOT / "Cargo.lock").read_bytes()
production = {
    str(path.relative_to(SNAPSHOT)): sha256(path.read_bytes())
    for directory in ["crates", "bin"]
    for path in (SNAPSHOT / directory).rglob("*.rs")
    if "src" in path.relative_to(SNAPSHOT).parts
}

# Preserve the release workspace dependencies and production crate manifests. Add only the
# already-pinned Reth test utility and the node test's dev-dependency feature overlay.
workspace = SNAPSHOT / "Cargo.toml"
text = workspace.read_text()
reth = tomllib.loads(text)["workspace"]["dependencies"]["reth-node-builder"]
addition = ('reth-e2e-test-utils = { git = "' + reth["git"] + '", rev = "' +
            reth["rev"] + '" }\n')
text = text.replace("[workspace.dependencies]\n", "[workspace.dependencies]\n" + addition, 1)
workspace.write_text(text)
node_manifest = SNAPSHOT / "crates/node/Cargo.toml"
node_text = node_manifest.read_text()
# These test-only additions do not replace the release's production dependency declarations.
dev_additions = """alloy-genesis = { workspace = true }
alloy-hardforks = { workspace = true }
alloy-rlp = { workspace = true }
alloy-rpc-types-engine = { workspace = true }
alloy-signer = { workspace = true }
alloy-signer-local = { workspace = true }
jsonrpsee = { workspace = true, features = ["client"] }
reth-db = { workspace = true, features = ["test-utils"] }
reth-e2e-test-utils = { workspace = true }
reth-node-builder = { workspace = true, features = ["test-utils"] }
reth-node-core = { workspace = true }
reth-rpc-server-types = { workspace = true }
reth-tasks = { workspace = true }
serde_json = { workspace = true }
"""
node_manifest.write_text(node_text.replace("[dev-dependencies]\n", "[dev-dependencies]\n" +
                                           dev_additions, 1))
for relative in ["crates/node/tests/fixtures/etna-genesis.json",
                 "crates/node/tests/fixtures/historical-genesis.json",
                 "crates/node/tests/fixtures/etna-cases.json"]:
    destination = SNAPSHOT / relative
    destination.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(ROOT / relative, destination)

# Adapt test support only to release APIs; the dedicated historical test itself is unchanged.
support = (ROOT / "crates/node/tests/support/mod.rs").read_text()
support = re.sub(r"forks\.insert\(TaikoHardfork::Etna,[^;]+;", "", support)
support = support.replace("activation: u64", "_activation: u64")
support = re.sub(r"\s*osaka::TaikoExecutionPayloadV3,?", "", support)
start = support.index("let (payload, data, response) = if etna {")
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
shutil.copyfile(ROOT / "crates/node/tests/etna_history.rs",
                SNAPSHOT / "crates/node/tests/legacy_reference.rs")

# Begin with the release lock, resolve only harness additions offline, then fail closed if a
# pinned package identity/version disappeared or a second version/source replaced its pin.
subprocess.run(["cargo", "metadata", "--offline", "--format-version", "1"],
               cwd=SNAPSHOT, stdout=subprocess.DEVNULL, check=True)
effective_lock = (SNAPSHOT / "Cargo.lock").read_bytes()
baseline_ids = lock_identities(baseline_lock)
effective_ids = lock_identities(effective_lock)
assert baseline_ids <= effective_ids, "test overlay removed or changed a release dependency pin"
added_ids = effective_ids - baseline_ids
assert {entry[0] for entry in added_ids} <= {"reth-e2e-test-utils", "reth-testing-utils"}, (
    "unexpected dependency additions", sorted(added_ids))
for relative, expected_hash in production.items():
    assert sha256((SNAPSHOT / relative).read_bytes()) == expected_hash, relative

# Package versions remain pinned; test features may add edges to the dependency graph.
before = {p["name"] + "@" + p["version"]: p for p in tomllib.loads(baseline_lock.decode())["package"]}
after = {p["name"] + "@" + p["version"]: p for p in tomllib.loads(effective_lock.decode())["package"]}
changed_edges = {key: {"release": before[key].get("dependencies", []),
                       "harness": after[key].get("dependencies", [])}
                 for key in before if before[key].get("dependencies") != after[key].get("dependencies")}
manifest = {
    "evidenceKind": "synthetic legacy differential, not real-chain replay",
    "baselineTag": BASE_TAG,
    "baselineCommit": BASE,
    "productionRustSha256": sha256(json.dumps(production, sort_keys=True).encode()),
    "releaseLockSha256": sha256(baseline_lock),
    "harnessLockSha256": sha256(effective_lock),
    "releaseDependencyPinsPreserved": True,
    "addedPackages": sorted(added_ids),
    "dependencyEdgeChanges": changed_edges,
    "harnessInputsSha256": {
        relative: sha256((ROOT / relative).read_bytes())
        for relative in ["crates/node/tests/etna_history.rs", "crates/node/tests/support/mod.rs",
                         "crates/node/tests/fixtures/historical-genesis.json",
                         "crates/node/tests/fixtures/generate-legacy-reference.py"]
    },
    "adaptedSupportSha256": sha256(destination.read_bytes()),
    "rustc": subprocess.check_output(["rustc", "--version", "--verbose"], cwd=SNAPSHOT, text=True),
    "cargo": subprocess.check_output(["cargo", "--version"], cwd=SNAPSHOT, text=True).strip(),
    "command": "cargo test --offline --locked -p alethia-reth-node --test legacy_reference --all-features historical_v2",
}
MANIFEST.write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n")
target = Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "target")).absolute()
env = dict(os.environ, CARGO_TARGET_DIR=str(target), ETNA_VECTOR_OUTPUT=str(OUTPUT))
subprocess.run(["cargo", "test", "--offline", "--locked", "--manifest-path", str(SNAPSHOT / "Cargo.toml"),
                "-p", "alethia-reth-node", "--test", "legacy_reference", "--all-features", "historical_v2"],
               cwd=ROOT, env=env, check=True)
actual = json.loads(OUTPUT.read_text())
expected = json.loads((ROOT / "crates/node/tests/fixtures/etna-cases.json").read_text())
assert len(actual) == 5
# This digest identifies output content; it is not independent execution provenance.
manifest["capturedVectorsSha256"] = sha256(OUTPUT.read_bytes())
manifest["captureCompleted"] = True
MANIFEST.write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n")
for name, value in actual.items():
    assert value == expected[name], f"synthetic legacy mismatch: {name}; capture: {OUTPUT}"
manifest["matchesCheckedInVectors"] = True
MANIFEST.write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n")
print(f"Five synthetic legacy vectors match. Capture: {OUTPUT}; run manifest: {MANIFEST}")
