"""Regression coverage for the authenticated block-cost refinement."""

import copy
from fractions import Fraction
import importlib.util
import json
from pathlib import Path
import unittest

BASE = Path(__file__).resolve().parent
SPEC = importlib.util.spec_from_file_location("block_cost_successor", BASE / "successor.py")
assert SPEC is not None and SPEC.loader is not None
SUCCESSOR = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(SUCCESSOR)
PARENT = "0a3a9663ca0b2c399bce514ae06ee009b43781f957898dfec5cf1ea27f80a885"
PRICES = {"opcode:0x04": 800, "opcode:0x05": 900, "opcode:0x06": 800,
          "opcode:0x07": 800, "opcode:0x08": 1100, "opcode:0x5e": 48}


class SuccessorTests(unittest.TestCase):
    def setUp(self):
        self.parent = json.loads((BASE / "successors/parent-0a3a9663.json").read_text())
        self.evidence = json.loads((BASE / "successors/block-cost-evidence-v1.json").read_text())

    def test_authenticates_immutable_current_parent(self):
        self.assertEqual(SUCCESSOR.PARENT_SHA256, PARENT)
        SUCCESSOR.validate_parent(self.parent)
        self.parent["proposal_once"]["startup"] += 1
        self.parent["artifact_sha256"] = SUCCESSOR.content_address(self.parent)
        with self.assertRaisesRegex(ValueError, "parent"):
            SUCCESSOR.transform(self.parent, self.evidence)

    def test_exact_prices_and_projected_complete_coverage(self):
        self.assertEqual(SUCCESSOR.derive_prices(self.parent, self.evidence), PRICES)
        self.assertEqual(len(self.evidence["measurements"]), 127)
        self.assertEqual(SUCCESSOR.projected_underquotes(self.parent, self.evidence, PRICES), 0)

    def test_exact_rounding_preserves_sub_pgu_boundaries(self):
        for key, grid, boundary, expected in [("opcode:0x04", 100, 640, 800),
                                              ("opcode:0x5e", 8, Fraction(192, 5), 48)]:
            evidence = copy.deepcopy(self.evidence)
            row = next(r for r in evidence["measurements"] if r["route_key"] == key
                       and r["copy_words_per_event"] != 0)
            evidence["measurements"] = [r for r in evidence["measurements"] if r["route_key"] != key] + [row]
            units = row["executed_primary_count"] * (row["copy_words_per_event"] or 1)
            deficit = units * (boundary * 10**18 - row["parent_primary_component_scaled"])
            self.assertEqual(deficit.denominator if isinstance(deficit, Fraction) else 1, 1)
            row["parent_total_scaled"] = row["actual_pgu"] * 10**18 - int(deficit)
            self.assertEqual(SUCCESSOR.derive_prices(self.parent, evidence)[key], expected)
            row["parent_total_scaled"] -= 1
            self.assertEqual(SUCCESSOR.derive_prices(self.parent, evidence)[key], expected + grid)

    def test_transform_changes_only_five_floors_and_mcopy_word_coefficient(self):
        result = SUCCESSOR.transform(self.parent, self.evidence)
        self.assertEqual(result, SUCCESSOR.transform(self.parent, copy.deepcopy(self.evidence)))
        for old, new in zip(self.parent["block_local"]["operation_routes"], result["block_local"]["operation_routes"]):
            expected = copy.deepcopy(old)
            key = old["route_key"]
            if key in PRICES:
                if key == "opcode:0x5e":
                    expected["accepted_absolute_model"]["parameters"]["copy_words"] = 48 * 10**18
                else:
                    expected["composition"] = "max_incremental_accepted_absolute_and_structural"
                    expected["structural_floor"] = {"selector": "constant_one_per_event", "coefficient": PRICES[key] * 10**18}
            self.assertEqual(new, expected)
        for key in ("ownership", "proposal_once", "execution_identity", "authorities", "sources"):
            self.assertEqual(self.parent[key], result[key])
        expected = copy.deepcopy(self.parent)
        expected["block_local"]["operation_routes"] = result["block_local"]["operation_routes"]
        for key in ("artifact_sha256", "purpose", "generator", "parent_generator", "validation", "parent_validation", "successor"):
            expected[key] = result[key]
        self.assertEqual(result, expected, "all unrelated parent fields are immutable")
        self.assertEqual(result["parent_validation"], self.parent["validation"])
        self.assertEqual(result["validation"]["projected_local_measurement_underquotes_after_refinement"], 0)
        SUCCESSOR.validate_successor(result, self.parent, self.evidence)

    def test_zero_word_controls_are_validated_without_division(self):
        evidence = copy.deepcopy(self.evidence)
        rows = [r for r in evidence["measurements"] if r["route_key"] == "opcode:0x5e"]
        self.assertEqual(sum(r["copy_words_per_event"] == 0 for r in rows), 4)
        for row in rows:
            if row["copy_words_per_event"] == 0:
                row["actual_pgu"] = row["parent_total_scaled"] // 10**18 + 1_000_000
        self.assertEqual(SUCCESSOR.derive_prices(self.parent, evidence), PRICES)
        self.assertEqual(SUCCESSOR.projected_underquotes(self.parent, evidence, PRICES), 4)

    def test_rejects_invalid_scope_counts_words_contribution_and_provenance(self):
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
            lambda e: e["measurements"][0].update(copy_words_per_event=1),
            lambda e: next(r for r in e["measurements"] if r["route_key"] == "opcode:0x5e").update(copy_words_per_event=999),
            lambda e: e["measurements"].append(copy.deepcopy(e["measurements"][0])),
            lambda e: e["measurements"][1].update(canonical_input_sha256=e["measurements"][0]["canonical_input_sha256"]),
            lambda e: e.update(unreviewed_selector="operand_width"),
        ]
        for mutate in mutations:
            with self.subTest(mutate=mutate):
                evidence = copy.deepcopy(self.evidence)
                mutate(evidence)
                with self.assertRaises(ValueError):
                    SUCCESSOR.derive_prices(self.parent, evidence)

    def test_sealed_evidence_binds_all_counts_and_source_digests(self):
        for mutate in [lambda e: e["measurements"][0].update(executed_primary_count=1),
                       lambda e: e["measurements"][0]["source_sha256"].update(sp1_report="f" * 64)]:
            evidence = copy.deepcopy(self.evidence)
            mutate(evidence)
            with self.assertRaisesRegex(ValueError, "sealed evidence"):
                SUCCESSOR.transform(self.parent, evidence)

    def test_replay_rejects_scope_expansion_weakened_price_and_stale_generator(self):
        for mutate in [lambda r: r["proposal_once"].update(startup=0),
                       lambda r: r["block_local"]["operation_routes"][4]["structural_floor"].update(coefficient=799 * 10**18),
                       lambda r: r["generator"].update(file_sha256="0" * 64)]:
            result = SUCCESSOR.transform(self.parent, self.evidence)
            mutate(result)
            result["artifact_sha256"] = SUCCESSOR.content_address(result)
            with self.assertRaisesRegex(ValueError, "successor"):
                SUCCESSOR.validate_successor(result, self.parent, self.evidence)


if __name__ == "__main__":
    unittest.main()
