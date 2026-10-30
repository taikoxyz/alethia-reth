#!/usr/bin/env python3
"""Reproduce a finite-evidence raw access-list address-item tariff in normalized PGU.

The original 23 cases and exact matched increments are sealed inputs. This changes
one additive tariff; keys, unique resources, payload, authorization and discounts
retain their parent prices. Shape/order diagnostics are not universal coverage.
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
PARENT_SHA256 = "4c0306015efc1572db444d8b78cafc9431988bf5ca3d678e750d0292e9ca8a55"
PARENT_PATH = "successors/parent-4c030601.json"
EVIDENCE_PATH = "successors/access-list-items-v1.json"
EVIDENCE_SHA256 = "9c0c5e96a579693bd9cbcb555f05ca0b9bac4e25b8405cb7821bdce55b007608"
FIT_COLLECTIONS = ("matched_baseline_increments", "same_pattern_adjacent_increments",
                   "grouping_comparisons", "three_block_baseline_comparisons")
DIAGNOSTIC_COLLECTIONS = ("unique_vs_repeated_comparisons", "ordering_comparisons")
COUNT_FIELDS = ("raw_address_items", "raw_storage_keys", "unique_addresses", "unique_storage_pairs")
SCALE = 10**18


def canonical_json(payload):
    """Encode canonical JSON independently of file formatting."""
    return json.dumps(payload, sort_keys=True, separators=(",", ":")).encode()


def digest(payload):
    """Hash one canonical JSON value."""
    return hashlib.sha256(canonical_json(payload)).hexdigest()


def content_address(payload):
    """Address the runtime specification without its self-referential digest."""
    return digest({key: value for key, value in payload.items() if key != "artifact_sha256"})


def fields(value, expected, label):
    """Reject unknown or missing schema fields."""
    if not isinstance(value, dict) or set(value) != set(expected):
        raise ValueError(f"invalid closed schema for {label}")


def integer(value, minimum, maximum, label):
    """Require exact bounded integers, rejecting booleans and floats."""
    if type(value) is not int or not minimum <= value <= maximum:
        raise ValueError(f"invalid integer {label}")


def sha256(value):
    """Validate a portable lowercase SHA-256 identity."""
    if not isinstance(value, str) or re.fullmatch(r"[0-9a-f]{64}", value) is None:
        raise ValueError("invalid source SHA-256")


def validate_counts(value):
    """Check raw and unique native counts without changing vector ownership."""
    fields(value, COUNT_FIELDS, "counts")
    for name in COUNT_FIELDS:
        integer(value[name], 0, 10**9, name)
    if value["unique_addresses"] > value["raw_address_items"] or value["unique_storage_pairs"] > value["raw_storage_keys"]:
        raise ValueError("unique counts exceed raw counts")
    if value["raw_address_items"] == 0 and value["raw_storage_keys"] != 0:
        raise ValueError("keys require an address item")


def validate_evidence(policy):
    """Authenticate all original observations and recompute every exported pair."""
    if digest(policy) != EVIDENCE_SHA256:
        raise ValueError("wrong sealed access-list evidence policy")
    if policy["parent_artifact_sha256"] != PARENT_SHA256 or policy["independent_audit_status"] != "passed":
        raise ValueError("wrong evidence parent or independent audit status")
    for name in ("summary_sha256", "independent_audit_sha256"):
        sha256(policy[name])
    rows = policy["measurements"]
    if not isinstance(rows, list) or len(rows) != 23:
        raise ValueError("exactly the original 23 observations are required")
    by_id = {}
    for row in rows:
        fields(row, {"sample_id", "parameters", "actual_pgu", "parent_node_scaled", "parent_complete_scaled",
                     "counts_total", "counts_by_block", "input_sha256", "public_output", "source_sha256"}, "measurement")
        name = row["sample_id"]
        if not isinstance(name, str) or re.fullmatch(r"access-list-values-[a-z0-9_-]{1,180}", name) is None or name in by_id:
            raise ValueError("invalid or duplicate original sample identity")
        by_id[name] = row
        fields(row["parameters"], {"pattern", "count", "block_count"}, "parameters")
        blocks = row["parameters"]["block_count"]
        integer(blocks, 1, 32, "block_count")
        integer(row["actual_pgu"], 1, 2**64-1, "actual_pgu")
        for name in ("parent_node_scaled", "parent_complete_scaled"):
            integer(row[name], 0, 2**127-1, name)
        if row["parent_complete_scaled"] < row["parent_node_scaled"]:
            raise ValueError("complete quote does not cover its known subtotal")
        validate_counts(row["counts_total"])
        if not isinstance(row["counts_by_block"], list) or len(row["counts_by_block"]) != blocks:
            raise ValueError("per-block native counts are incomplete")
        for value in row["counts_by_block"]:
            validate_counts(value)
        for name in COUNT_FIELDS:
            if sum(value[name] for value in row["counts_by_block"]) != row["counts_total"][name]:
                raise ValueError("per-block native counts do not sum to total")
        sha256(row["input_sha256"])
        fields(row["source_sha256"], {"native_report_sha256", "quote_report_sha256", "sp1_report_sha256",
                                      "sp1_resource_receipt_sha256"}, "source hashes")
        for value in row["source_sha256"].values():
            sha256(value)
        if not isinstance(row["public_output"], str) or re.fullmatch(r"0x[0-9a-f]{64}", row["public_output"]) is None:
            raise ValueError("invalid public output")
    for role, collections in (("fit_pairs", FIT_COLLECTIONS), ("diagnostic_pairs", DIAGNOSTIC_COLLECTIONS)):
        fields(policy[role], collections, role)
        for collection in collections:
            for pair in policy[role][collection]:
                fields(pair, {"kind", "low_sample_id", "high_sample_id", "same_block_count", "actual_increment_pgu",
                              "node_increment_scaled", "full_increment_scaled", "node_increment_covered",
                              "full_increment_covered", "raw_unique_count_deltas", "charged_user_gas_increment"}, "pair")
                low, high = by_id[pair["low_sample_id"]], by_id[pair["high_sample_id"]]
                if pair["same_block_count"] != low["parameters"]["block_count"] or pair["same_block_count"] != high["parameters"]["block_count"]:
                    raise ValueError("pair block counts differ")
                actual = high["actual_pgu"] - low["actual_pgu"]
                node = high["parent_node_scaled"] - low["parent_node_scaled"]
                full = high["parent_complete_scaled"] - low["parent_complete_scaled"]
                if pair["actual_increment_pgu"] != actual or pair["node_increment_scaled"] != str(node) or pair["full_increment_scaled"] != str(full):
                    raise ValueError("exported matched increment differs from original observations")
                expected = {name: high["counts_total"][name] - low["counts_total"][name] for name in COUNT_FIELDS}
                if pair["raw_unique_count_deltas"] != expected:
                    raise ValueError("exported raw/unique deltas differ from observations")
                if pair["node_increment_covered"] != (node >= actual*SCALE) or pair["full_increment_covered"] != (full >= actual*SCALE):
                    raise ValueError("exported increment coverage differs")
    return by_id


def derive(policy):
    """Apply exact 5/4 constraints, integer ceiling and upward power-of-two rounding."""
    validate_evidence(policy)
    required = Fraction(0)
    for row in policy["measurements"]:
        items = row["counts_total"]["raw_address_items"]
        deficit = max(Fraction(row["actual_pgu"]) - Fraction(row["parent_complete_scaled"], SCALE), 0)
        if not items:
            if deficit:
                raise ValueError("empty-list baseline cannot be repaired by an item tariff")
            continue
        required = max(required, Fraction(5, 4)*deficit/items)
    for collection in FIT_COLLECTIONS:
        for pair in policy["fit_pairs"][collection]:
            items = pair["raw_unique_count_deltas"]["raw_address_items"]
            actual = pair["actual_increment_pgu"]
            if items > 0 and actual > 0:
                required = max(required, (Fraction(5, 4)*actual - Fraction(int(pair["node_increment_scaled"]), SCALE))/items)
    ceiling = -(-required.numerator // required.denominator)
    rounded = 1 << (ceiling - 1).bit_length() if ceiling else 0
    return required, ceiling, rounded


def projected_underquotes(policy, coefficient):
    """Project full totals only; this does not run a native quote or a guest."""
    return sum((row["parent_complete_scaled"] + row["counts_total"]["raw_address_items"]*coefficient*SCALE + SCALE-1)//SCALE < row["actual_pgu"]
               for row in policy["measurements"])


def transform(parent, policy):
    """Add one tariff while sealing exact evidence, arithmetic and generator provenance."""
    if parent.get("artifact_sha256") != PARENT_SHA256 or content_address(parent) != PARENT_SHA256:
        raise ValueError("wrong immutable parent runtime specification")
    if parent["arithmetic"]["scale"] != SCALE:
        raise ValueError("unexpected parent normalization scale")
    requirement, ceiling, coefficient = derive(policy)
    expected = policy["expected_derivation"]
    if expected != {"maximum_requirement_numerator": requirement.numerator,
                    "maximum_requirement_denominator": requirement.denominator,
                    "ceiling_pgu": ceiling, "normalized_pgu_per_raw_item": coefficient}:
        raise ValueError("derived coefficient differs from sealed exact policy")
    result = copy.deepcopy(parent)
    result["block_local"]["access_list_item_tariff"] = coefficient*SCALE
    result["purpose"] = "schema9_raw_access_list_address_item_tariff"
    result["parent_generator"] = result["generator"]
    result["generator"] = {
        "path": "crates/evm/src/zk_gas/pricing/access_list_successor.py",
        "file_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        "table_generator_path": "crates/evm/src/zk_gas/pricing/generate.py",
        "table_generator_sha256": hashlib.sha256((BASE / "generate.py").read_bytes()).hexdigest(),
    }
    result["parent_validation"] = result["validation"]
    result["validation"] = {
        "parent_validation_is_inherited_not_reexecuted": True,
        "new_sp1_measurement_rows": 23,
        "coefficient_fit_performed": True,
        "projected_full_underquotes_before": projected_underquotes(policy, 0),
        "projected_full_underquotes_after": projected_underquotes(policy, coefficient),
        "new_native_repricing_performed": False,
        "zero_item_order_diagnostics_retained": True,
        "raw_key_tariff_added": False,
        "universal_upper_bound_claimed": False,
    }
    result["successor"] = {
        "schema_version": 1,
        "parent_artifact_sha256": PARENT_SHA256,
        "parent_snapshot": PARENT_PATH,
        "access_list_policy_path": EVIDENCE_PATH,
        "access_list_policy_sha256": digest(policy),
        "maximum_requirement": {"numerator": requirement.numerator, "denominator": requirement.denominator},
        "ceiling_pgu": ceiling,
        "normalized_pgu_per_raw_item": coefficient,
        "scope": "add_raw_nonlegacy_access_list_address_items_only;duplicates_and_anchor_items_count;all_old_prices_and_discount_unchanged",
        "measurement_policy": policy["scope"],
    }
    result["artifact_sha256"] = content_address(result)
    return result


def validate_successor(payload, parent, policy):
    """Reject changed schema, scope, provenance or coefficients by exact replay."""
    if payload != transform(parent, policy):
        raise ValueError("successor differs from deterministic parent/access-list evidence replay")


def main():
    """Write or verify the model and its separate acceptance record."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--parent", type=Path, default=BASE/PARENT_PATH)
    parser.add_argument("--evidence", type=Path, default=BASE/EVIDENCE_PATH)
    parser.add_argument("--output", type=Path, default=BASE/"runtime-spec.json")
    parser.add_argument("--acceptance-output", type=Path, default=BASE/"accepted-runtime-spec.sha256")
    parser.add_argument("--check", action="store_true")
    options = parser.parse_args()
    payload = transform(json.loads(options.parent.read_text()), json.loads(options.evidence.read_text()))
    if options.check:
        if json.loads(options.output.read_text()) != payload or options.acceptance_output.read_text().strip() != payload["artifact_sha256"]:
            raise ValueError("successor output or acceptance record is stale")
    else:
        options.output.write_bytes(canonical_json(payload)+b"\n")
        options.acceptance_output.write_text(payload["artifact_sha256"]+"\n")
    print(f"{'verified' if options.check else 'generated'} {payload['artifact_sha256']}")


if __name__ == "__main__":
    main()
