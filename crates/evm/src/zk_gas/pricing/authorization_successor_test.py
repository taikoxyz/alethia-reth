"""Closed successor scope, evidence sealing and exact normalized authorization tariff."""
import copy
import importlib.util
import json
from pathlib import Path
import unittest

BASE = Path(__file__).resolve().parent
SPEC = importlib.util.spec_from_file_location("authorization_successor", BASE / "authorization_successor.py")
assert SPEC is not None and SPEC.loader is not None
SUCCESSOR = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(SUCCESSOR)


class AuthorizationSuccessorTests(unittest.TestCase):
    def setUp(self):
        self.parent = json.loads((BASE / SUCCESSOR.PARENT_PATH).read_text())
        self.policy = json.loads((BASE / SUCCESSOR.EVIDENCE_PATH).read_text())

    def test_adds_only_normalized_entry_tariff_and_provenance(self):
        result = SUCCESSOR.transform(self.parent, self.policy)
        expected = copy.deepcopy(self.parent)
        expected["block_local"]["authorization_validation_tariff"] = 200_000 * 10**18
        for key in ("artifact_sha256", "purpose", "generator", "parent_generator", "validation", "parent_validation", "successor"):
            expected[key] = result[key]
        self.assertEqual(result, expected)
        self.assertEqual(len(self.policy["measurements"]), 12)
        self.assertFalse(result["validation"]["new_sp1_universal_coverage_claimed"])
        SUCCESSOR.validate_successor(result, self.parent, self.policy)

    def test_finite_evidence_projection_covers_all_twelve_observations(self):
        scale = self.parent["arithmetic"]["scale"]
        before = after = 0
        for row in self.policy["measurements"]:
            count = row["parameters"]["count"] or 0
            self.assertEqual(count, row["verified_signed_entries"])
            parent_quote = int(row["complete_scaled"])
            actual = row["actual_sp1_pgu"]
            candidate_quote = parent_quote + count * 200_000 * scale
            before += (parent_quote + scale - 1) // scale < actual
            after += (candidate_quote + scale - 1) // scale < actual
        self.assertEqual(before, 9)
        self.assertEqual(after, 0)

    def test_rejects_policy_parent_schema_and_successor_drift(self):
        for key, value in (("effective_pgu_per_entry", 100_000), ("new_field", 1), ("normalization_policy", "halve_again")):
            policy = copy.deepcopy(self.policy)
            policy[key] = value
            with self.assertRaisesRegex(ValueError, "sealed authorization"):
                SUCCESSOR.transform(self.parent, policy)
        parent = copy.deepcopy(self.parent)
        parent["block_local"]["fixed_costs"]["block_base"] += 1
        parent["artifact_sha256"] = SUCCESSOR.content_address(parent)
        with self.assertRaisesRegex(ValueError, "parent"):
            SUCCESSOR.transform(parent, self.policy)
        for mutation in (
            lambda result: result["block_local"].update(authorization_validation_tariff=100_000 * 10**18),
            lambda result: result["block_local"].update(extra_tariff=1),
            lambda result: result["generator"].update(file_sha256="0" * 64),
            lambda result: result["proposal_once"].update(startup=0),
        ):
            result = SUCCESSOR.transform(self.parent, self.policy)
            mutation(result)
            result["artifact_sha256"] = SUCCESSOR.content_address(result)
            with self.assertRaisesRegex(ValueError, "successor"):
                SUCCESSOR.validate_successor(result, self.parent, self.policy)


if __name__ == "__main__":
    unittest.main()
