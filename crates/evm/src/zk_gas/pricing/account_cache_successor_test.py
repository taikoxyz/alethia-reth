"""Progressive account-cache candidate identity, independence and fail-closed generation."""
import copy
import importlib.util
import json
import unittest
from pathlib import Path

BASE = Path(__file__).resolve().parent
SPEC = importlib.util.spec_from_file_location("account_cache_successor", BASE / "account_cache_successor.py")
MODEL = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODEL)


class AccountCacheSuccessorTests(unittest.TestCase):
    def setUp(self):
        self.parent = json.loads((BASE / MODEL.PARENT_PATH).read_bytes())
        self.evidence = json.loads((BASE / MODEL.EVIDENCE_PATH).read_bytes())

    def test_new_component_preserves_all_inherited_numeric_objects(self):
        result = MODEL.transform(self.parent, self.evidence)
        block = dict(result["block_local"])
        self.assertEqual(block.pop("account_cache_work_tariff"), 128 * 10**18)
        self.assertEqual(block, self.parent["block_local"])
        arithmetic = dict(result["arithmetic"])
        self.assertEqual(arithmetic.pop("account_cache_work_unit_bound"), 10**12)
        arithmetic["declared_worst_case_scaled_pgu"] -= 128 * 10**18 * 10**12
        self.assertEqual(arithmetic, self.parent["arithmetic"])
        self.assertEqual(result["proposal_once"], self.parent["proposal_once"])
        self.assertEqual(result["account_journal_policy"], self.parent["account_journal_policy"])
        self.assertFalse(result["validation"]["opened_samples_are_holdout"])
        self.assertFalse(result["validation"]["fresh_repricing_performed"])

    def test_parent_evidence_and_algorithm_tamper_fail_closed(self):
        for target in ["parent", "evidence"]:
            parent, evidence = copy.deepcopy(self.parent), copy.deepcopy(self.evidence)
            if target == "parent":
                parent["block_local"]["account_journal_work_tariff"] += 1
            else:
                evidence["samples"][1]["actual_sp1_pgu"] -= 1
            with self.assertRaises(ValueError):
                MODEL.transform(parent, evidence)
        result = MODEL.transform(self.parent, self.evidence)
        result["account_cache_policy"]["transaction_budget"] = "all opcode budget"
        with self.assertRaises(ValueError):
            MODEL.validate_successor(result, self.parent, self.evidence)

    def test_independent_domain_i128_boundary(self):
        parent = copy.deepcopy(self.parent)
        parent["arithmetic"]["declared_worst_case_scaled_pgu"] = 2**127 - 1 - 128 * 10**30
        self.assertEqual(MODEL.checked_declaration(parent), 2**127 - 1)
        parent["arithmetic"]["declared_worst_case_scaled_pgu"] += 1
        with self.assertRaises(ValueError):
            MODEL.checked_declaration(parent)


if __name__ == "__main__":
    unittest.main()
