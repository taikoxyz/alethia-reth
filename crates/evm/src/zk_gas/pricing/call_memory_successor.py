#!/usr/bin/env python3
"""Reproduce CALL-family parent-memory ownership using unchanged parent tariffs.

This successor repairs selector ownership. It fits no coefficient and asserts no
new SP1 coverage; the parent's validation remains inherited, not reexecuted.
"""
from __future__ import annotations

import argparse
import copy
import hashlib
import json
from pathlib import Path

BASE = Path(__file__).resolve().parent
PARENT_SHA256 = "67d0b01fee7f5ba208dfd2311e5be469ec96421156ecf5f0c14962c27e7b4df9"
PARENT_PATH = "successors/parent-67d0b01f.json"
EVIDENCE_PATH = "successors/call-memory-ownership-v1.json"
EVIDENCE_SHA256 = "b7277ce58c955210447d20cd78e9a0992d94e7bd1d927a37e38db456ec18710c"
CALL_ROUTES = tuple(f"opcode:0x{opcode}:confirmed_spawn_wrapper" for opcode in ("f1", "f2", "f4", "fa"))


def canonical_json(payload):
    """Encode sorted compact JSON independently of file formatting."""
    return json.dumps(payload, sort_keys=True, separators=(",", ":")).encode()


def digest(payload):
    """Hash one canonical JSON value."""
    return hashlib.sha256(canonical_json(payload)).hexdigest()


def content_address(payload):
    """Hash the runtime specification excluding its self-address."""
    return digest({key: value for key, value in payload.items() if key != "artifact_sha256"})


def transform(parent, policy):
    """Change only four fixed CALL models and explicit successor provenance."""
    if parent.get("artifact_sha256") != PARENT_SHA256 or content_address(parent) != PARENT_SHA256:
        raise ValueError("wrong immutable parent runtime specification")
    if digest(policy) != EVIDENCE_SHA256:
        raise ValueError("wrong sealed ownership policy")
    result = copy.deepcopy(parent)
    source = next(row for row in parent["block_local"]["operation_routes"]
                  if row["route_key"] == policy["shared_memory_source_route"])
    memory = source["accepted_absolute_model"]["shared_memory"]
    for route in result["block_local"]["operation_routes"]:
        if route["route_key"] in CALL_ROUTES:
            model = route["accepted_absolute_model"]
            if model["kind"] != "fixed":
                raise ValueError("parent CALL model must be fixed")
            model.update(kind="fixed_with_optional_shared_memory", model_input_kind="spawn_memory",
                         shared_memory=copy.deepcopy(memory))
    result["purpose"] = "schema9_confirmed_call_parent_memory_v1"
    result["parent_generator"] = result["generator"]
    result["generator"] = {
        "path": "crates/evm/src/zk_gas/pricing/call_memory_successor.py",
        "file_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        "table_generator_path": "crates/evm/src/zk_gas/pricing/generate.py",
        "table_generator_sha256": hashlib.sha256((BASE / "generate.py").read_bytes()).hexdigest(),
    }
    result["parent_validation"] = result["validation"]
    result["validation"] = {
        "parent_validation_is_inherited_not_reexecuted": True,
        "new_sp1_measurement_rows": 0,
        "new_sp1_coverage_claimed": False,
        "coefficient_fit_performed": False,
    }
    result["successor"] = {
        "schema_version": 1,
        "parent_artifact_sha256": PARENT_SHA256,
        "parent_snapshot": PARENT_PATH,
        "ownership_policy_path": EVIDENCE_PATH,
        "ownership_policy_sha256": digest(policy),
        "shared_memory_source_route": policy["shared_memory_source_route"],
        "shared_memory_coefficients_sha256": digest(memory),
        "scope": "confirmed_call_callcode_delegatecall_staticcall_parent_memory_only;other_routes_coefficients_composition_and_unzen_unchanged",
        "measurement_policy": policy["measurement_policy"],
    }
    result["artifact_sha256"] = content_address(result)
    return result


def validate_successor(payload, parent, policy):
    """Reject scope expansion and stale provenance by exact deterministic replay."""
    if payload != transform(parent, policy):
        raise ValueError("successor differs from deterministic parent/ownership-policy replay")


def main():
    """Write or verify the current successor and its separate acceptance record."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--parent", type=Path, default=BASE / PARENT_PATH)
    parser.add_argument("--evidence", type=Path, default=BASE / EVIDENCE_PATH)
    parser.add_argument("--output", type=Path, default=BASE / "runtime-spec.json")
    parser.add_argument("--acceptance-output", type=Path, default=BASE / "accepted-runtime-spec.sha256")
    parser.add_argument("--check", action="store_true")
    options = parser.parse_args()
    payload = transform(json.loads(options.parent.read_text()), json.loads(options.evidence.read_text()))
    if options.check:
        if json.loads(options.output.read_text()) != payload:
            raise ValueError("successor output is stale")
        if options.acceptance_output.read_text().strip() != payload["artifact_sha256"]:
            raise ValueError("successor acceptance record is stale")
    else:
        options.output.write_bytes(canonical_json(payload) + b"\n")
        options.acceptance_output.write_text(payload["artifact_sha256"] + "\n")
    print(f"{'verified' if options.check else 'generated'} {payload['artifact_sha256']}")


if __name__ == "__main__":
    main()
