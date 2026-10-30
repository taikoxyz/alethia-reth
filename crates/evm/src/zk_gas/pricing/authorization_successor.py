#!/usr/bin/env python3
"""Reproduce the initial additive EIP-7702 authorization-validation tariff.

The sealed finite experiment motivates a conservative operator-selected initial
tariff, not a universal upper bound. Other coefficients and discounts are inherited.
"""
from __future__ import annotations

import argparse
import copy
import hashlib
import json
from pathlib import Path

BASE = Path(__file__).resolve().parent
PARENT_SHA256 = "537a965e51dd0416e3472a31cca8b067a85723fe4bbd08c52027ef3ca73f0e3b"
PARENT_PATH = "successors/parent-537a965e.json"
EVIDENCE_PATH = "successors/authorization-validation-v1.json"
EVIDENCE_SHA256 = "634b08864ba2e7c28922241b186e958613e6525444a61c3da049b0a36094eb59"


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
    """Add only the raw-entry tariff and bind the exact parent, policy and tooling."""
    if parent.get("artifact_sha256") != PARENT_SHA256 or content_address(parent) != PARENT_SHA256:
        raise ValueError("wrong immutable parent runtime specification")
    if digest(policy) != EVIDENCE_SHA256:
        raise ValueError("wrong sealed authorization policy")
    result = copy.deepcopy(parent)
    result["block_local"]["authorization_validation_tariff"] = policy["effective_pgu_per_entry"] * parent["arithmetic"]["scale"]
    result["purpose"] = "schema9_authorization_validation_initial_tariff"
    result["parent_generator"] = result["generator"]
    result["generator"] = {
        "path": "crates/evm/src/zk_gas/pricing/authorization_successor.py",
        "file_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        "table_generator_path": "crates/evm/src/zk_gas/pricing/generate.py",
        "table_generator_sha256": hashlib.sha256((BASE / "generate.py").read_bytes()).hexdigest(),
    }
    result["parent_validation"] = result["validation"]
    result["validation"] = {
        "parent_validation_is_inherited_not_reexecuted": True,
        "new_sp1_measurement_rows": len(policy["measurements"]),
        "new_sp1_universal_coverage_claimed": False,
        "coefficient_fit_performed": False,
    }
    result["successor"] = {
        "schema_version": 1,
        "parent_artifact_sha256": PARENT_SHA256,
        "parent_snapshot": PARENT_PATH,
        "authorization_policy_path": EVIDENCE_PATH,
        "authorization_policy_sha256": digest(policy),
        "scope": "add_raw_authorization_validation_tariff_only;existing_coefficients_discount_routes_payload_resources_mpt_and_unzen_unchanged",
        "measurement_policy": policy["measurement_scope"],
    }
    result["artifact_sha256"] = content_address(result)
    return result


def validate_successor(payload, parent, policy):
    """Reject scope expansion and stale provenance by exact deterministic replay."""
    if payload != transform(parent, policy):
        raise ValueError("successor differs from deterministic parent/authorization-policy replay")


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
