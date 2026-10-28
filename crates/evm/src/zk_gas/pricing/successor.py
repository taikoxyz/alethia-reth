#!/usr/bin/env python3
"""Reproduce the MULMOD floor and block-payload ownership successor of the 93d parent.

Measurement records are compact, hash-bound summaries of previously accepted local
executions, not newly performed executions or historical calibration replays.
"""

from __future__ import annotations

import argparse
import copy
from fractions import Fraction
import hashlib
import json
from pathlib import Path
import re


BASE = Path(__file__).resolve().parent
PARENT_SHA256 = "93d04eddda3779393807808859eb36561d35aad0cdd8a8a3960ec3e279a93046"
LOCAL_ELF_SHA256 = "416f5a475b17aa15914fb9387159d9eae9ecd474e7104da65ac22512ab56da2e"
SOURCE_ROLES = {"construction_request", "native_report", "quote_report", "sp1_report", "measurement_status"}
ROUTE = "opcode:0x09"
SCALE = 10**18


def canonical_json(payload):
    """Encode the same sorted compact JSON used by the original runtime exporter."""
    return json.dumps(payload, sort_keys=True, separators=(",", ":")).encode()


def digest(payload):
    """Hash a canonical JSON value without depending on its file formatting."""
    return hashlib.sha256(canonical_json(payload)).hexdigest()


def content_address(payload):
    """Hash a runtime specification excluding only its own address field."""
    return digest({key: value for key, value in payload.items() if key != "artifact_sha256"})


def fields(payload, expected, label):
    """Reject missing and unknown fields rather than silently changing the evidence scope."""
    if not isinstance(payload, dict) or set(payload) != set(expected):
        raise ValueError(f"{label}: expected exactly {sorted(expected)}")


def sha256(value):
    """Validate a lowercase SHA-256 digest copied from retained measurement records."""
    if not isinstance(value, str) or re.fullmatch(r"[0-9a-f]{64}", value) is None:
        raise ValueError("invalid SHA-256 provenance")


def integer(value, minimum, maximum, label):
    """Reject booleans, floats and values outside the declared measurement bounds."""
    if type(value) is not int or not minimum <= value <= maximum:
        raise ValueError(f"invalid integer {label}")


def validate_parent(parent):
    """Authenticate the full parent, retaining every historical coefficient unchanged."""
    if parent.get("artifact_sha256") != PARENT_SHA256 or content_address(parent) != PARENT_SHA256:
        raise ValueError("wrong immutable parent runtime specification")


def mulmod(parent):
    """Find the unique ordinary MULMOD route in the authenticated parent."""
    return next(route for route in parent["block_local"]["operation_routes"] if route["route_key"] == ROUTE)


def validate_evidence(evidence):
    """Validate the closed local-execution evidence schema and its identity envelope."""
    fields(evidence, {"schema_version", "parent_artifact_sha256", "measurement_identity", "measurements"}, "evidence")
    if type(evidence["schema_version"]) is not int or evidence["schema_version"] != 1:
        raise ValueError("unsupported evidence schema")
    if evidence["parent_artifact_sha256"] != PARENT_SHA256:
        raise ValueError("evidence refers to a different parent")
    identity = evidence["measurement_identity"]
    fields(identity, {"elf_sha256", "sp1_sdk_version", "sp1_core_executor_version", "estimator_sha256", "executor_opts_sha256"}, "measurement_identity")
    for key in ("elf_sha256", "estimator_sha256", "executor_opts_sha256"):
        sha256(identity[key])
    if identity["elf_sha256"] != LOCAL_ELF_SHA256 or identity["sp1_sdk_version"] != "6.3.0" or identity["sp1_core_executor_version"] != "6.3.0":
        raise ValueError("wrong local execution identity; production SDK/ELF parity is not established")
    rows = evidence["measurements"]
    if not isinstance(rows, list) or not 1 <= len(rows) <= 1024:
        raise ValueError("evidence requires 1..1024 accepted measurement rows")
    ids = set()
    for row in rows:
        fields(row, {"sample_id", "block_count", "executed_mulmod_count", "actual_pgu", "parent_total_scaled", "canonical_input_sha256", "public_values_sha256", "operands", "source_sha256"}, "measurement")
        name = row["sample_id"]
        if not isinstance(name, str) or re.fullmatch(r"[a-z0-9][a-z0-9_/-]{0,159}", name) is None or ".." in name or name in ids:
            raise ValueError("invalid or duplicate sample identity")
        ids.add(name)
        integer(row["block_count"], 1, 32, "block_count")
        integer(row["executed_mulmod_count"], 1, row["block_count"] * 1_000_000, "executed_mulmod_count")
        integer(row["actual_pgu"], 1, 2**64 - 1, "actual_pgu")
        integer(row["parent_total_scaled"], 1, 2**127 - 1, "parent_total_scaled")
        sha256(row["canonical_input_sha256"])
        sha256(row["public_values_sha256"])
        fields(row["operands"], {"a", "b", "modulus"}, "operands")
        for value in row["operands"].values():
            if not isinstance(value, str) or re.fullmatch(r"0x[0-9a-f]{64}", value) is None:
                raise ValueError("operands must be explicit 32-byte hexadecimal values")
        fields(row["source_sha256"], SOURCE_ROLES, "source_sha256")
        for value in row["source_sha256"].values():
            sha256(value)


def required_floors(parent, evidence):
    """Return exact per-event floors that cover each complete proposal measurement.

Only the parent's accepted MULMOD contribution is replaced; every other operation,
block, state and proposal term remains charged. The original scaled complete quote
is used before its final integer ceiling, avoiding loss of sub-PGU information.
"""
    validate_parent(parent)
    validate_evidence(evidence)
    accepted = mulmod(parent)["accepted_absolute_model"]
    old_per_event = accepted["common_dispatch"] + 8 * accepted["parameters"]["body_per_raw_gas"]
    return [max(Fraction(old_per_event, SCALE),
                Fraction(old_per_event, SCALE) +
                Fraction(row["actual_pgu"] * SCALE - row["parent_total_scaled"], row["executed_mulmod_count"] * SCALE))
            for row in evidence["measurements"]]


def derive_floor(parent, evidence):
    """Derive ceil-to-100(max(required per-event floor) * 5/4) exactly."""
    units = max(required_floors(parent, evidence)) * Fraction(5, 4) / 100
    return -(-units.numerator // units.denominator) * 100


def transform(parent, evidence):
    """Construct a scoped monotone successor; never modify fitted parent coefficients."""
    floor = derive_floor(parent, evidence)
    result = copy.deepcopy(parent)
    route = mulmod(result)
    route["composition"] = "max_incremental_accepted_absolute_and_structural"
    route["structural_floor"] = {"selector": "constant_one_per_event", "coefficient": floor * SCALE}
    result["purpose"] = "schema9_mulmod_operand_cost_and_block_payload_successor_v1"
    result["ownership"]["block_local"].append("block.transaction_payload")
    result["ownership"]["proposal_once"] = [
        "proposal.additional_transaction_payload" if item == "proposal.transaction_payload" else item
        for item in result["ownership"]["proposal_once"]
    ]
    result["ownership"]["transaction_payload"] = {
        "block_local": "committed_canonical_signed_eip2718_bytes_and_nonempty_calldata_count",
        "proposal_once": "additional_input_payload_not_already_charged_by_committed_blocks_and_true_unattempted_count",
        "coefficient_owner": "proposal_once.transaction_payload_tariffs",
        "coefficient_scope": "shared_authoritative_tariffs_for_disjoint_block_and_proposal_payload",
    }
    result["status"] = "conservative_runtime_successor"
    result["parent_generator"] = result["generator"]
    result["generator"] = {
        "path": "crates/evm/src/zk_gas/pricing/successor.py",
        "file_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        "table_generator_path": "crates/evm/src/zk_gas/pricing/generate.py",
        "table_generator_sha256": hashlib.sha256((BASE / "generate.py").read_bytes()).hexdigest(),
    }
    result["parent_validation"] = result["validation"]
    result["validation"] = {
        "historical_replay_results_embedded_in_successor": 0,
        "parent_validation_is_inherited_not_reexecuted": True,
        "local_measurement_rows": len(evidence["measurements"]),
        "projected_local_measurement_underquotes_after_floor": sum(
            required > floor for required in required_floors(parent, evidence)),
    }
    result["successor"] = {
        "schema_version": 1,
        "parent_artifact_sha256": PARENT_SHA256,
        "parent_snapshot": "successors/parent-93d04edd.json",
        "evidence_path": "successors/mulmod-evidence-v1.json",
        "evidence_sha256": digest(evidence),
        "route_key": ROUTE,
        "floor_pgu_per_normal_executed_event": floor,
        "floor_policy": "ceil_to_100(max(parent_accepted_per_event,required_per_event)*5/4)",
        "measurement_identity": copy.deepcopy(evidence["measurement_identity"]),
        "production_sdk_elf_parity_established": False,
        "scope": "normal_executed_mulmod_floor_and_block_payload_ownership;payload_tariffs_feature_error_and_unzen_unchanged",
    }
    result["artifact_sha256"] = content_address(result)
    return result


def validate_successor(payload, parent, evidence):
    """Reject any scope expansion, weakened floor, or stale provenance by exact replay."""
    if payload != transform(parent, evidence):
        raise ValueError("successor differs from deterministic parent/evidence replay")


def main():
    """Write or check a runtime successor from explicit portable input paths."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--parent", type=Path, default=BASE / "successors/parent-93d04edd.json")
    parser.add_argument("--evidence", type=Path, default=BASE / "successors/mulmod-evidence-v1.json")
    parser.add_argument("--output", type=Path, default=BASE / "runtime-spec.json")
    parser.add_argument("--check", action="store_true")
    options = parser.parse_args()
    payload = transform(json.loads(options.parent.read_text()), json.loads(options.evidence.read_text()))
    if options.check:
        if json.loads(options.output.read_text()) != payload:
            raise ValueError("successor output is stale")
    else:
        options.output.write_bytes(canonical_json(payload) + b"\n")
    print(f"{'verified' if options.check else 'generated'} {payload['artifact_sha256']}; MULMOD floor {payload['successor']['floor_pgu_per_normal_executed_event']} PGU/event")


if __name__ == "__main__":
    main()
