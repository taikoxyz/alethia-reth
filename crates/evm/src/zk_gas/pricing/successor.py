#!/usr/bin/env python3
"""Reproduce the per-event EXP floor from an authenticated model80459 parent.

Sealed records bind validated native/SP1 observations and complete parent quotes.
Generation projects their prices; it does not repeat those executions. Unchanged
parent validation and coefficients are inherited, not newly certified.
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
PARENT_SHA256 = "80459b4dd85741f04703e2ab6b4760f2a6a5736c6c77fd69a5dd98b61e2d325d"
PARENT_PATH = "successors/parent-80459b4d.json"
EVIDENCE_PATH = "successors/exp-evidence-v1.json"
# Seal counts, source digests and observations independently of the generated
# artifact's own address. Changing the evidence requires explicit review.
EVIDENCE_SHA256 = "b57db97ce31f37ea569296fd55e6daeb4738fd4feabd96b1d03eb66894e73154"
MEASUREMENT_IDENTITY = {
    "elf_sha256": "416f5a475b17aa15914fb9387159d9eae9ecd474e7104da65ac22512ab56da2e",
    "sp1_sdk_version": "6.3.0",
    "sp1_core_executor_version": "6.3.0",
    "estimator_sha256": "475d00bdc5e88909abc1b6f1f782a62ec12782155c7d22298e844570c28208f7",
    "executor_opts_sha256": "1fabe34edc34ec2a536390c7afb7cd023c7fb9fd29867dc9b272f5c0539fbb8f",
}
SOURCE_ROLES = {"construction_request", "guest_input", "native_report", "quote_report", "sp1_report", "measurement_status"}
EXP_ROUTE = "opcode:0x0a"
SCALE = 10**18


def canonical_json(payload):
    """Encode sorted compact JSON independently of file formatting."""
    return json.dumps(payload, sort_keys=True, separators=(",", ":")).encode()


def digest(payload):
    """Hash a canonical JSON value."""
    return hashlib.sha256(canonical_json(payload)).hexdigest()


def content_address(payload):
    """Hash a runtime specification excluding only its self-address field."""
    return digest({key: value for key, value in payload.items() if key != "artifact_sha256"})


def fields(payload, expected, label):
    """Reject missing and unknown fields rather than silently widening scope."""
    if not isinstance(payload, dict) or set(payload) != set(expected):
        raise ValueError(f"{label}: expected exactly {sorted(expected)}")


def sha256(value):
    """Validate one lowercase SHA-256 provenance digest."""
    if not isinstance(value, str) or re.fullmatch(r"[0-9a-f]{64}", value) is None:
        raise ValueError("invalid SHA-256 provenance")


def integer(value, minimum, maximum, label):
    """Reject booleans, floats and values outside the declared integer bounds."""
    if type(value) is not int or not minimum <= value <= maximum:
        raise ValueError(f"invalid integer {label}")


def validate_parent(parent):
    """Authenticate the exact snapshot, preserving prior arithmetic/payload work."""
    if parent.get("artifact_sha256") != PARENT_SHA256 or content_address(parent) != PARENT_SHA256:
        raise ValueError("wrong immutable parent runtime specification")


def parent_exp_component(parent, exponent_bytes):
    """Evaluate the unchanged parent accepted whole-event EXP price, scaled PGU."""
    integer(exponent_bytes, 0, 32, "exponent_byte_length")
    route = next(route for route in parent["block_local"]["operation_routes"] if route["route_key"] == EXP_ROUTE)
    model = route["accepted_absolute_model"]
    parameters = model["parameters"]
    body = parameters["small_bucket_body"]
    if exponent_bytes > 4:
        body = max(body, parameters["constant"] + parameters["exponent_byte"] * exponent_bytes
                   + parameters["exponent_byte_sq"] * exponent_bytes**2)
    return model["common_dispatch"] + body


def validate_evidence(parent, evidence):
    """Validate closed scope and exact contributions within the local identity.

    Source files are external retained material. The transform's independent seal
    binds prior raw validation; schema checks alone cannot prove execution counts.
    """
    fields(evidence, {"schema_version", "parent_artifact_sha256", "measurement_identity", "measurements"}, "evidence")
    if type(evidence["schema_version"]) is not int or evidence["schema_version"] != 1:
        raise ValueError("unsupported evidence schema")
    if evidence["parent_artifact_sha256"] != PARENT_SHA256:
        raise ValueError("evidence refers to a different parent")
    if evidence["measurement_identity"] != MEASUREMENT_IDENTITY:
        raise ValueError("wrong local measurement identity; production SDK/ELF parity is not established")
    rows = evidence["measurements"]
    if not isinstance(rows, list) or not 1 <= len(rows) <= 1024:
        raise ValueError("evidence requires 1..1024 accepted measurement rows")
    ids, canonical_inputs = set(), set()
    for row in rows:
        fields(row, {"sample_id", "route_key", "block_count", "executed_primary_count", "exponent_byte_length",
                     "parent_primary_component_scaled", "actual_pgu", "parent_total_scaled", "canonical_input_sha256",
                     "public_values_sha256", "source_sha256"}, "measurement")
        name = row["sample_id"]
        if not isinstance(name, str) or re.fullmatch(r"[a-z0-9][a-z0-9_/-]{0,199}", name) is None or ".." in name or "holdout" in name or name in ids:
            raise ValueError("invalid, reserved or duplicate sample identity")
        ids.add(name)
        if row["route_key"] != EXP_ROUTE:
            raise ValueError("route outside EXP refinement")
        integer(row["block_count"], 1, 32, "block_count")
        integer(row["executed_primary_count"], 1, row["block_count"] * 1_000_000, "executed_primary_count")
        integer(row["exponent_byte_length"], 0, 32, "exponent_byte_length")
        integer(row["actual_pgu"], 1, 2**64 - 1, "actual_pgu")
        integer(row["parent_total_scaled"], 1, 2**127 - 1, "parent_total_scaled")
        integer(row["parent_primary_component_scaled"], 1, 2**127 - 1, "parent_primary_component_scaled")
        if row["parent_primary_component_scaled"] != parent_exp_component(parent, row["exponent_byte_length"]):
            raise ValueError("primary contribution differs from authenticated parent")
        if row["parent_total_scaled"] < row["parent_primary_component_scaled"] * row["executed_primary_count"]:
            raise ValueError("complete quote does not cover primary contribution")
        sha256(row["canonical_input_sha256"])
        sha256(row["public_values_sha256"])
        if row["canonical_input_sha256"] in canonical_inputs:
            raise ValueError("duplicate canonical input")
        canonical_inputs.add(row["canonical_input_sha256"])
        fields(row["source_sha256"], SOURCE_ROLES, "source_sha256")
        for value in row["source_sha256"].values():
            sha256(value)


def has_deficit(row):
    """Select observations needing a higher event price; others only validate."""
    return row["actual_pgu"] * SCALE > row["parent_total_scaled"]


def derive_coefficient(parent, evidence):
    """Fit exact conservative PGU per canonical EXP gas unit, rounding upward.

    Covered rows do not constrain a new floor: the original event curve already
    covers their observations and remains a candidate for every event. For deficit
    rows, solve for the required complete event price, take the maximum normalized
    by 10+50*bytes, apply 5/4 headroom, and ceil to an integer PGU per unit.
    """
    validate_parent(parent)
    validate_evidence(parent, evidence)
    required = Fraction(0)
    for row in evidence["measurements"]:
        if has_deficit(row):
            original = Fraction(row["parent_primary_component_scaled"], SCALE)
            deficit_per_event = Fraction(row["actual_pgu"] * SCALE - row["parent_total_scaled"],
                                         row["executed_primary_count"] * SCALE)
            required = max(required, (original + deficit_per_event) / (10 + 50 * row["exponent_byte_length"]))
    conservative = required * Fraction(5, 4)
    return -(-conservative.numerator // conservative.denominator)


def projected_underquotes(parent, evidence, coefficient):
    """Project complete coverage by replacing only each retained EXP contribution."""
    underquotes = 0
    for row in evidence["measurements"]:
        old = parent_exp_component(parent, row["exponent_byte_length"])
        new = max(old, coefficient * (10 + 50 * row["exponent_byte_length"]) * SCALE)
        total = row["parent_total_scaled"] + row["executed_primary_count"] * (new - old)
        underquotes += (total + SCALE - 1) // SCALE < row["actual_pgu"]
    return underquotes


def transform(parent, evidence):
    """Produce the sole current successor without altering unrelated parent fields."""
    validate_parent(parent)
    if digest(evidence) != EVIDENCE_SHA256:
        raise ValueError("wrong sealed evidence manifest")
    coefficient = derive_coefficient(parent, evidence)
    result = copy.deepcopy(parent)
    route = next(route for route in result["block_local"]["operation_routes"] if route["route_key"] == EXP_ROUTE)
    route["accepted_absolute_model"]["parameters"]["per_event_raw_gas_floor"] = coefficient * SCALE
    result["purpose"] = "schema9_exp_per_event_floor_v1"
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
        "deficit_fit_rows": sum(has_deficit(row) for row in evidence["measurements"]),
        "already_covered_validation_rows": sum(not has_deficit(row) for row in evidence["measurements"]),
        "projected_local_measurement_underquotes_after_refinement": projected_underquotes(parent, evidence, coefficient),
    }
    result["successor"] = {
        "schema_version": 1,
        "parent_artifact_sha256": PARENT_SHA256,
        "parent_snapshot": PARENT_PATH,
        "evidence_path": EVIDENCE_PATH,
        "evidence_sha256": digest(evidence),
        "exp_pgu_per_canonical_raw_gas_unit": coefficient,
        "exp_policy": "sum_per_event(max(parent_accepted_event,coefficient*(10+50*exponent_bytes)))",
        "coefficient_policy": "ceil(max_deficit_rows(required_complete_event_price/(10+50*exponent_bytes))*5/4)",
        "already_covered_row_policy": "validation_only_no_new_floor_constraint",
        "measurement_identity": copy.deepcopy(evidence["measurement_identity"]),
        "production_sdk_elf_parity_established": False,
        "scope": "normal_exp_accepted_per_event_floor;all_other_coefficients_routes_ownership_and_unzen_unchanged",
        "historical_parent_source_revision": "1ef47962868b73cf45a81bd16f50f671493cda83",
    }
    result["artifact_sha256"] = content_address(result)
    return result


def validate_successor(payload, parent, evidence):
    """Reject scope expansion, weakened pricing and stale provenance by exact replay."""
    if payload != transform(parent, evidence):
        raise ValueError("successor differs from deterministic parent/evidence replay")


def main():
    """Write or check the runtime successor using portable input paths."""
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
