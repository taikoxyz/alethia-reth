"""Regression tests for the narrowly scoped MULMOD successor transform."""

import copy
import importlib.util
import json
from pathlib import Path
import unittest


BASE = Path(__file__).resolve().parent
SPEC = importlib.util.spec_from_file_location("mulmod_successor", BASE / "successor.py")
assert SPEC is not None and SPEC.loader is not None
SUCCESSOR = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(SUCCESSOR)


class SuccessorTests(unittest.TestCase):
    def test_payload_ownership_is_block_local_with_proposal_residual_only(self):
        parent = json.loads((BASE / "successors/parent-93d04edd.json").read_text())
        evidence = json.loads((BASE / "successors/mulmod-evidence-v1.json").read_text())
        result = SUCCESSOR.transform(parent, evidence)
        self.assertIn("block.transaction_payload", result["ownership"]["block_local"])
        self.assertNotIn("proposal.transaction_payload", result["ownership"]["proposal_once"])
        self.assertIn("proposal.additional_transaction_payload", result["ownership"]["proposal_once"])
        self.assertEqual(result["proposal_once"]["transaction_payload_tariffs"],
                         parent["proposal_once"]["transaction_payload_tariffs"])

    def setUp(self):
        self.parent = json.loads((BASE / "successors/parent-93d04edd.json").read_text())
        self.evidence = {
            "schema_version": 1,
            "parent_artifact_sha256": SUCCESSOR.PARENT_SHA256,
            "measurement_identity": {
                "elf_sha256": SUCCESSOR.LOCAL_ELF_SHA256,
                "sp1_sdk_version": "6.3.0",
                "sp1_core_executor_version": "6.3.0",
                "estimator_sha256": "a" * 64,
                "executor_opts_sha256": "b" * 64,
            },
            "measurements": [{
                "sample_id": "synthetic-exact-rational",
                "block_count": 1,
                "executed_mulmod_count": 8192,
                "actual_pgu": 170000000,
                "parent_total_scaled": 162000000 * 10**18,
                "canonical_input_sha256": "c" * 64,
                "public_values_sha256": "d" * 64,
                "operands": {key: "0x" + "f" * 64 for key in ("a", "b", "modulus")},
                "source_sha256": {key: "e" * 64 for key in SUCCESSOR.SOURCE_ROLES},
            }],
        }

    def test_exact_rational_floor_and_deterministic_scoped_transform(self):
        self.assertEqual(SUCCESSOR.derive_floor(self.parent, self.evidence), 1600)
        first = SUCCESSOR.transform(self.parent, self.evidence)
        self.assertEqual(first, SUCCESSOR.transform(self.parent, copy.deepcopy(self.evidence)))
        SUCCESSOR.validate_successor(first, self.parent, self.evidence)
        old_routes = self.parent["block_local"]["operation_routes"]
        new_routes = first["block_local"]["operation_routes"]
        for old, new in zip(old_routes, new_routes):
            if old["route_key"] != "opcode:0x09":
                self.assertEqual(old, new)
            else:
                self.assertEqual(old["accepted_absolute_model"], new["accepted_absolute_model"])
                self.assertEqual(old["feature_error_absolute_model"], new["feature_error_absolute_model"])
                self.assertEqual(new["structural_floor"]["coefficient"], 1600 * 10**18)
        self.assertEqual(first["validation"]["historical_replay_results_embedded_in_successor"], 0)
        self.assertEqual(first["parent_validation"], self.parent["validation"])

    def test_rejects_wrong_parent_even_with_recomputed_address(self):
        self.parent["proposal_once"]["startup"] += 1
        self.parent["artifact_sha256"] = SUCCESSOR.content_address(self.parent)
        with self.assertRaisesRegex(ValueError, "parent"):
            SUCCESSOR.transform(self.parent, self.evidence)

    def test_rejects_scope_changes_and_nonmonotone_floor(self):
        for mutate in (lambda p: p["proposal_once"].update(startup=0),
                       lambda p: p["block_local"]["operation_routes"][9]["structural_floor"].update(coefficient=1599 * 10**18)):
            payload = SUCCESSOR.transform(self.parent, self.evidence)
            mutate(payload)
            payload["artifact_sha256"] = SUCCESSOR.content_address(payload)
            with self.assertRaisesRegex(ValueError, "successor"):
                SUCCESSOR.validate_successor(payload, self.parent, self.evidence)

    def test_floor_rounding_preserves_a_sub_pgu_rational_boundary(self):
        row = self.evidence["measurements"][0]
        old = SUCCESSOR.mulmod(self.parent)["accepted_absolute_model"]
        old_scaled = old["common_dispatch"] + 8 * old["parameters"]["body_per_raw_gas"]
        row["parent_total_scaled"] = row["actual_pgu"] * 10**18 - row["executed_mulmod_count"] * (1280 * 10**18 - old_scaled)
        self.assertEqual(SUCCESSOR.derive_floor(self.parent, self.evidence), 1600)
        row["parent_total_scaled"] -= 1
        self.assertEqual(SUCCESSOR.derive_floor(self.parent, self.evidence), 1700)

    def test_rejects_incomplete_or_invalid_measurement_provenance(self):
        mutations = [
            lambda e: e.update(parent_artifact_sha256="0" * 64),
            lambda e: e["measurement_identity"].update(sp1_sdk_version="6.1.0"),
            lambda e: e["measurements"][0]["source_sha256"].pop("sp1_report"),
            lambda e: e["measurements"][0].update(executed_mulmod_count=0),
            lambda e: e["measurements"][0].update(actual_pgu=1.25),
            lambda e: e["measurements"].append(copy.deepcopy(e["measurements"][0])),
            lambda e: e.update(unreviewed_selector="operand_width"),
        ]
        for mutation in mutations:
            with self.subTest(mutation=mutation):
                evidence = copy.deepcopy(self.evidence)
                mutation(evidence)
                with self.assertRaises(ValueError):
                    SUCCESSOR.transform(self.parent, evidence)

    def test_rejects_changed_generator_or_evidence_binding(self):
        payload = SUCCESSOR.transform(self.parent, self.evidence)
        payload["generator"]["file_sha256"] = "0" * 64
        payload["artifact_sha256"] = SUCCESSOR.content_address(payload)
        with self.assertRaisesRegex(ValueError, "successor"):
            SUCCESSOR.validate_successor(payload, self.parent, self.evidence)


if __name__ == "__main__":
    unittest.main()
