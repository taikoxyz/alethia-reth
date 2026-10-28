#!/usr/bin/env python3
"""Reproduce the block-cost refinement directly from the immutable model0a3 parent.

Compact, sealed records bind previously validated native/SP1 executions and complete
parent quotes. Generation projects their prices; it does not repeat those executions.
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
PARENT_SHA256 = "0a3a9663ca0b2c399bce514ae06ee009b43781f957898dfec5cf1ea27f80a885"
PARENT_PATH = "successors/parent-0a3a9663.json"
EVIDENCE_PATH = "successors/block-cost-evidence-v1.json"
# Seal all source digests, exact counts, scope and observations, independently of
# the generated artifact's address. A new evidence set requires explicit review.
EVIDENCE_SHA256 = "3fd115b9f4c828b48d2eaaf8a812bbb785c33abe3d97f21f3396fa5fd4edbfaf"
MEASUREMENT_IDENTITY = {
    "elf_sha256": "416f5a475b17aa15914fb9387159d9eae9ecd474e7104da65ac22512ab56da2e",
    "sp1_sdk_version": "6.3.0",
    "sp1_core_executor_version": "6.3.0",
    "estimator_sha256": "475d00bdc5e88909abc1b6f1f782a62ec12782155c7d22298e844570c28208f7",
    "executor_opts_sha256": "1fabe34edc34ec2a536390c7afb7cd023c7fb9fd29867dc9b272f5c0539fbb8f",
}
SOURCE_ROLES = {"construction_request", "guest_input", "native_report", "quote_report", "sp1_report", "measurement_status"}
ARITHMETIC_RAW_GAS = {"opcode:0x04": 5, "opcode:0x05": 5, "opcode:0x06": 5,
                      "opcode:0x07": 5, "opcode:0x08": 8}
COPY_ROUTE = "opcode:0x5e"
ROUTES = set(ARITHMETIC_RAW_GAS) | {COPY_ROUTE}
SCALE = 10**18


def canonical_json(payload):
    """Encode sorted compact JSON, independent of file formatting."""
    return json.dumps(payload, sort_keys=True, separators=(",", ":")).encode()


def digest(payload):
    """Hash a canonical JSON value."""
    return hashlib.sha256(canonical_json(payload)).hexdigest()


def content_address(payload):
    """Hash a runtime specification excluding only its own address field."""
    return digest({key: value for key, value in payload.items() if key != "artifact_sha256"})


def fields(payload, expected, label):
    """Reject missing and unknown fields instead of silently widening scope."""
    if not isinstance(payload, dict) or set(payload) != set(expected):
        raise ValueError(f"{label}: expected exactly {sorted(expected)}")


def sha256(value):
    """Validate a lowercase SHA-256 provenance digest."""
    if not isinstance(value, str) or re.fullmatch(r"[0-9a-f]{64}", value) is None:
        raise ValueError("invalid SHA-256 provenance")


def integer(value, minimum, maximum, label):
    """Reject booleans, floats and values outside the declared bounds."""
    if type(value) is not int or not minimum <= value <= maximum:
        raise ValueError(f"invalid integer {label}")


def validate_parent(parent):
    """Authenticate the full immutable snapshot, including prior MULMOD/payload work."""
    if parent.get("artifact_sha256") != PARENT_SHA256 or content_address(parent) != PARENT_SHA256:
        raise ValueError("wrong immutable parent runtime specification")


def parent_components(parent):
    """Read arithmetic whole-event costs and MCOPY's word-only cost from the parent."""
    routes = {route["route_key"]: route for route in parent["block_local"]["operation_routes"]}
    result = {}
    for key, raw_gas in ARITHMETIC_RAW_GAS.items():
        model = routes[key]["accepted_absolute_model"]
        result[key] = model["common_dispatch"] + raw_gas * model["parameters"]["body_per_raw_gas"]
    result[COPY_ROUTE] = routes[COPY_ROUTE]["accepted_absolute_model"]["parameters"]["copy_words"]
    return result


def validate_evidence(parent, evidence):
    """Validate exact primary contribution/counts and the closed identity envelope.

Raw source files are retained externally. Their prior validation is authenticated
by the sealed manifest checked by transform; schema checks alone cannot prove an
execution count or digest corresponds to a real observation.
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
    components = parent_components(parent)
    ids, canonical_inputs, fit_routes = set(), set(), set()
    for row in rows:
        fields(row, {"sample_id", "route_key", "block_count", "executed_primary_count", "copy_length_bytes",
                     "copy_words_per_event", "parent_primary_component_scaled", "actual_pgu", "parent_total_scaled",
                     "canonical_input_sha256", "public_values_sha256", "source_sha256"}, "measurement")
        name = row["sample_id"]
        if not isinstance(name, str) or re.fullmatch(r"[a-z0-9][a-z0-9_/-]{0,199}", name) is None or ".." in name or "holdout" in name or name in ids:
            raise ValueError("invalid, reserved or duplicate sample identity")
        ids.add(name)
        key = row["route_key"]
        if key not in ROUTES:
            raise ValueError("route outside six-route refinement")
        integer(row["block_count"], 1, 32, "block_count")
        integer(row["executed_primary_count"], 1, row["block_count"] * 1_000_000, "executed_primary_count")
        integer(row["actual_pgu"], 1, 2**64 - 1, "actual_pgu")
        integer(row["parent_total_scaled"], 1, 2**127 - 1, "parent_total_scaled")
        integer(row["parent_primary_component_scaled"], 1, 2**127 - 1, "parent_primary_component_scaled")
        if row["parent_primary_component_scaled"] != components[key]:
            raise ValueError("primary contribution differs from authenticated parent")
        sha256(row["canonical_input_sha256"])
        sha256(row["public_values_sha256"])
        identity = (key, row["canonical_input_sha256"])
        if identity in canonical_inputs:
            raise ValueError("duplicate canonical input within route")
        canonical_inputs.add(identity)
        if key == COPY_ROUTE:
            integer(row["copy_length_bytes"], 0, 2**32 - 1, "copy_length_bytes")
            integer(row["copy_words_per_event"], 0, 2**32 - 1, "copy_words_per_event")
            if row["copy_words_per_event"] != (row["copy_length_bytes"] + 31) // 32:
                raise ValueError("copy words do not match copied length")
        elif row["copy_length_bytes"] is not None or row["copy_words_per_event"] is not None:
            raise ValueError("arithmetic row carries copy selectors")
        units = primary_units(row)
        if row["parent_total_scaled"] < components[key] * units:
            raise ValueError("complete quote does not cover primary contribution")
        if units:
            fit_routes.add(key)
        fields(row["source_sha256"], SOURCE_ROLES, "source_sha256")
        for value in row["source_sha256"].values():
            sha256(value)
    if fit_routes != ROUTES:
        raise ValueError("every refined route requires positive-unit fit evidence")


def primary_units(row):
    """Count events for arithmetic or copied words for MCOPY (including zero controls)."""
    return row["executed_primary_count"] * (row["copy_words_per_event"] if row["route_key"] == COPY_ROUTE else 1)


def derive_prices(parent, evidence):
    """Fit exact-rational conservative prices; zero-word rows never divide or fit."""
    validate_parent(parent)
    validate_evidence(parent, evidence)
    components = parent_components(parent)
    required = {key: Fraction(value, SCALE) for key, value in components.items()}
    for row in evidence["measurements"]:
        units = primary_units(row)
        if units:
            key = row["route_key"]
            required[key] = max(required[key], Fraction(components[key], SCALE) + Fraction(
                row["actual_pgu"] * SCALE - row["parent_total_scaled"], units * SCALE))
    result = {}
    for key, value in required.items():
        grid = 8 if key == COPY_ROUTE else 100
        rounded_units = value * Fraction(5, 4) / grid
        result[key] = -(-rounded_units.numerator // rounded_units.denominator) * grid
    return result


def projected_underquotes(parent, evidence, prices):
    """Check complete-quote coverage after replacing only the priced contribution."""
    components = parent_components(parent)
    return sum((row["parent_total_scaled"] + primary_units(row) *
                (prices[row["route_key"]] * SCALE - components[row["route_key"]]) + SCALE - 1) // SCALE
               < row["actual_pgu"] for row in evidence["measurements"])


def transform(parent, evidence):
    """Produce the sole current successor; preserve all unrelated parent fields."""
    validate_parent(parent)
    if digest(evidence) != EVIDENCE_SHA256:
        raise ValueError("wrong sealed evidence manifest")
    prices = derive_prices(parent, evidence)
    result = copy.deepcopy(parent)
    for route in result["block_local"]["operation_routes"]:
        key = route["route_key"]
        if key == COPY_ROUTE:
            route["accepted_absolute_model"]["parameters"]["copy_words"] = prices[key] * SCALE
        elif key in ARITHMETIC_RAW_GAS:
            route["composition"] = "max_incremental_accepted_absolute_and_structural"
            route["structural_floor"] = {"selector": "constant_one_per_event", "coefficient": prices[key] * SCALE}
    result["purpose"] = "schema9_arithmetic_and_copy_block_cost_refinement_v1"
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
        "zero_word_validation_rows": sum(primary_units(row) == 0 for row in evidence["measurements"]),
        "projected_local_measurement_underquotes_after_refinement": projected_underquotes(parent, evidence, prices),
    }
    result["successor"] = {
        "schema_version": 1,
        "parent_artifact_sha256": PARENT_SHA256,
        "parent_snapshot": PARENT_PATH,
        "evidence_path": EVIDENCE_PATH,
        "evidence_sha256": digest(evidence),
        "arithmetic_floor_pgu_per_normal_executed_event": {key: prices[key] for key in ARITHMETIC_RAW_GAS},
        "mcopy_accepted_pgu_per_copy_word": prices[COPY_ROUTE],
        "arithmetic_floor_policy": "ceil_to_100(max(parent_accepted_per_event,required_per_event)*5/4)",
        "mcopy_word_policy": "ceil_to_8(max(parent_copy_word,required_per_copy_word)*5/4)",
        "zero_word_policy": "validation_only_no_division_no_fit",
        "measurement_identity": copy.deepcopy(evidence["measurement_identity"]),
        "production_sdk_elf_parity_established": False,
        "scope": "five_normal_arithmetic_event_floors_and_mcopy_accepted_copy_words;all_other_coefficients_routes_ownership_and_unzen_unchanged",
        "historical_parent_source_revision": "db54157ebd4de9fc4c1a2edc499b0db297d1bc15",
    }
    result["artifact_sha256"] = content_address(result)
    return result


def validate_successor(payload, parent, evidence):
    """Reject scope expansion, weakened prices or stale provenance by exact replay."""
    if payload != transform(parent, evidence):
        raise ValueError("successor differs from deterministic parent/evidence replay")


def main():
    """Write or check the runtime successor from portable input paths."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--parent", type=Path, default=BASE / PARENT_PATH)
    parser.add_argument("--evidence", type=Path, default=BASE / EVIDENCE_PATH)
    parser.add_argument("--output", type=Path, default=BASE / "runtime-spec.json")
    parser.add_argument("--check", action="store_true")
    options = parser.parse_args()
    payload = transform(json.loads(options.parent.read_text()), json.loads(options.evidence.read_text()))
    if options.check:
        if json.loads(options.output.read_text()) != payload:
            raise ValueError("successor output is stale")
    else:
        options.output.write_bytes(canonical_json(payload) + b"\n")
    print(f"{'verified' if options.check else 'generated'} {payload['artifact_sha256']}")


if __name__ == "__main__":
    main()
