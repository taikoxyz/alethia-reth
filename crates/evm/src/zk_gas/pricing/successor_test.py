"""Regression coverage for the authenticated per-event EXP refinement."""

import copy
from fractions import Fraction
import importlib.util
import json
from pathlib import Path
import unittest

BASE = Path(__file__).resolve().parent
SPEC = importlib.util.spec_from_file_location("exp_successor", BASE / "successor.py")
assert SPEC is not None and SPEC.loader is not None
SUCCESSOR = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(SUCCESSOR)
PARENT = "80459b4dd85741f04703e2ab6b4760f2a6a5736c6c77fd69a5dd98b61e2d325d"
SCALE = 10**18


class SuccessorTests(unittest.TestCase):
    def setUp(self):
        self.parent = json.loads((BASE / "successors/parent-80459b4d.json").read_text())

    def synthetic_evidence(self, required=Fraction(96, 5)):
        # Choose a whole-event requirement exactly on the 24-PGU headroom boundary.
        original = 2_703_276_834_322_027_481_408
        actual, count, raw = 1_000_000, 2, 210
        total = actual * SCALE - count * (required * raw * SCALE - original)
        self.assertEqual(total.denominator, 1)
        return {
            "schema_version": 1, "parent_artifact_sha256": PARENT,
            "measurement_identity": copy.deepcopy(SUCCESSOR.MEASUREMENT_IDENTITY),
            "measurements": [{
                "sample_id": "synthetic-exp-boundary", "route_key": "opcode:0x0a",
                "block_count": 1, "executed_primary_count": count,
                "exponent_byte_length": 4, "parent_primary_component_scaled": original,
                "actual_pgu": actual, "parent_total_scaled": int(total),
                "canonical_input_sha256": "1" * 64, "public_values_sha256": "2" * 64,
                "source_sha256": {role: "3" * 64 for role in (
                    "construction_request", "guest_input", "native_report", "quote_report",
                    "sp1_report", "measurement_status")},
            }],
        }

    def test_authenticates_current_parent(self):
        self.assertEqual(SUCCESSOR.PARENT_SHA256, PARENT)
        SUCCESSOR.validate_parent(self.parent)
        self.parent["proposal_once"]["startup"] += 1
        self.parent["artifact_sha256"] = SUCCESSOR.content_address(self.parent)
        with self.assertRaisesRegex(ValueError, "parent"):
            SUCCESSOR.validate_parent(self.parent)

    def test_exact_rounding_preserves_sub_pgu_boundary(self):
        evidence = self.synthetic_evidence()
        self.assertEqual(SUCCESSOR.derive_coefficient(self.parent, evidence), 24)
        evidence["measurements"][0]["parent_total_scaled"] -= 1
        self.assertEqual(SUCCESSOR.derive_coefficient(self.parent, evidence), 25)

    def test_non_deficits_do_not_create_a_negative_floor(self):
        evidence = self.synthetic_evidence()
        row = evidence["measurements"][0]
        row["parent_total_scaled"] = (row["actual_pgu"] + 100_000) * SCALE
        self.assertEqual(SUCCESSOR.derive_coefficient(self.parent, evidence), 0)

    def test_parent_exp_curve_has_original_bucket_and_quadratic_parts(self):
        self.assertEqual(SUCCESSOR.parent_exp_component(self.parent, 0), 2_703_276_834_322_027_481_408)
        self.assertEqual(SUCCESSOR.parent_exp_component(self.parent, 4), 2_703_276_834_322_027_481_408)
        self.assertEqual(SUCCESSOR.parent_exp_component(self.parent, 5), 3_534_207_877_640_930_240_549)
        self.assertEqual(SUCCESSOR.parent_exp_component(self.parent, 32), 44_408_346_523_183_755_813_503)

    def test_rejects_scope_counts_shapes_contribution_and_provenance(self):
        mutations = [
            lambda e: e.update(parent_artifact_sha256="0" * 64),
            lambda e: e["measurement_identity"].update(sp1_sdk_version="6.1.0"),
            lambda e: e["measurement_identity"].update(executor_opts_sha256="0" * 64),
            lambda e: e["measurements"][0]["source_sha256"].pop("guest_input"),
            lambda e: e["measurements"][0].update(executed_primary_count=0),
            lambda e: e["measurements"][0].update(executed_primary_count=True),
            lambda e: e["measurements"][0].update(actual_pgu=1.25),
            lambda e: e["measurements"][0].update(parent_primary_component_scaled=1),
            lambda e: e["measurements"][0].update(sample_id="reserved-holdout"),
            lambda e: e["measurements"][0].update(route_key="opcode:0x09"),
            lambda e: e["measurements"][0].update(exponent_byte_length=33),
            lambda e: e["measurements"][0].update(exponent_byte_length=True),
            lambda e: e["measurements"][0].update(parent_total_scaled=1),
            lambda e: e["measurements"].append(copy.deepcopy(e["measurements"][0])),
            lambda e: e.update(unreviewed_selector="operand_width"),
        ]
        for mutate in mutations:
            with self.subTest(mutate=mutate):
                evidence = self.synthetic_evidence()
                mutate(evidence)
                with self.assertRaises(ValueError):
                    SUCCESSOR.derive_coefficient(self.parent, evidence)

    def test_sealed_real_evidence_is_replayed_and_scope_remains_exact(self):
        evidence = json.loads((BASE / SUCCESSOR.EVIDENCE_PATH).read_text())
        result = SUCCESSOR.transform(self.parent, evidence)
        coefficient = SUCCESSOR.derive_coefficient(self.parent, evidence)
        self.assertEqual(coefficient, 24)
        self.assertEqual(len(evidence["measurements"]), 29)
        self.assertEqual(sum(SUCCESSOR.has_deficit(row) for row in evidence["measurements"]), 21)
        self.assertEqual(SUCCESSOR.projected_underquotes(self.parent, evidence, coefficient), 0)
        self.assertEqual(result, SUCCESSOR.transform(self.parent, copy.deepcopy(evidence)))
        for old, new in zip(self.parent["block_local"]["operation_routes"], result["block_local"]["operation_routes"]):
            expected = copy.deepcopy(old)
            if old["route_key"] == "opcode:0x0a":
                expected["accepted_absolute_model"]["parameters"]["per_event_raw_gas_floor"] = coefficient * SCALE
            self.assertEqual(new, expected)
        expected = copy.deepcopy(self.parent)
        expected["block_local"]["operation_routes"] = result["block_local"]["operation_routes"]
        for key in ("artifact_sha256", "purpose", "generator", "parent_generator", "validation", "parent_validation", "successor"):
            expected[key] = result[key]
        self.assertEqual(result, expected, "all unrelated parent fields remain immutable")
        self.assertEqual(result["parent_validation"], self.parent["validation"])
        SUCCESSOR.validate_successor(result, self.parent, evidence)

    def test_seal_and_successor_replay_reject_drift(self):
        evidence = json.loads((BASE / SUCCESSOR.EVIDENCE_PATH).read_text())
        for mutate in [lambda e: e["measurements"][0].update(executed_primary_count=1),
                       lambda e: e["measurements"][0]["source_sha256"].update(sp1_report="f" * 64)]:
            changed = copy.deepcopy(evidence)
            mutate(changed)
            with self.assertRaisesRegex(ValueError, "sealed evidence"):
                SUCCESSOR.transform(self.parent, changed)
        for mutate in [lambda r: r["proposal_once"].update(startup=0),
                       lambda r: r["generator"].update(file_sha256="0" * 64),
                       lambda r: next(row for row in r["block_local"]["operation_routes"] if row["route_key"] == "opcode:0x0a")["accepted_absolute_model"]["parameters"].update(per_event_raw_gas_floor=0)]:
            result = SUCCESSOR.transform(self.parent, evidence)
            mutate(result)
            result["artifact_sha256"] = SUCCESSOR.content_address(result)
            with self.assertRaisesRegex(ValueError, "successor"):
                SUCCESSOR.validate_successor(result, self.parent, evidence)


if __name__ == "__main__":
    unittest.main()
