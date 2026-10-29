"""Scope and provenance coverage for the CALL parent-memory ownership repair."""
import copy
import importlib.util
import json
from pathlib import Path
import unittest

BASE = Path(__file__).resolve().parent
SPEC = importlib.util.spec_from_file_location("call_memory_successor", BASE / "call_memory_successor.py")
assert SPEC is not None and SPEC.loader is not None
SUCCESSOR = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(SUCCESSOR)


class CallMemorySuccessorTests(unittest.TestCase):
    def setUp(self):
        self.parent = json.loads((BASE / SUCCESSOR.PARENT_PATH).read_text())
        self.policy = json.loads((BASE / SUCCESSOR.EVIDENCE_PATH).read_text())

    def test_reuses_shared_memory_and_preserves_fixed_costs_and_other_routes(self):
        result = SUCCESSOR.transform(self.parent, self.policy)
        memory = next(row for row in self.parent["block_local"]["operation_routes"]
                      if row["route_key"] == "opcode:0x51")["accepted_absolute_model"]["shared_memory"]
        changed = 0
        for old, new in zip(self.parent["block_local"]["operation_routes"], result["block_local"]["operation_routes"]):
            expected = copy.deepcopy(old)
            if old["route_key"] in SUCCESSOR.CALL_ROUTES:
                expected["accepted_absolute_model"].update(
                    kind="fixed_with_optional_shared_memory", model_input_kind="spawn_memory",
                    shared_memory=memory)
                changed += 1
            self.assertEqual(new, expected)
        self.assertEqual(changed, 4)
        expected = copy.deepcopy(self.parent)
        expected["block_local"]["operation_routes"] = result["block_local"]["operation_routes"]
        for key in ("artifact_sha256", "purpose", "generator", "parent_generator", "validation", "parent_validation", "successor"):
            expected[key] = result[key]
        self.assertEqual(result, expected)
        self.assertFalse(result["validation"]["new_sp1_coverage_claimed"])
        SUCCESSOR.validate_successor(result, self.parent, self.policy)

    def test_rejects_parent_policy_and_successor_drift(self):
        parent = copy.deepcopy(self.parent)
        parent["proposal_once"]["startup"] += 1
        parent["artifact_sha256"] = SUCCESSOR.content_address(parent)
        with self.assertRaisesRegex(ValueError, "parent"):
            SUCCESSOR.transform(parent, self.policy)
        policy = copy.deepcopy(self.policy)
        policy["route_keys"].append("opcode:0xf0:confirmed_spawn_wrapper")
        with self.assertRaisesRegex(ValueError, "sealed ownership"):
            SUCCESSOR.transform(self.parent, policy)
        for mutate in (
            lambda result: result["proposal_once"].update(startup=0),
            lambda result: result["generator"].update(file_sha256="0" * 64),
            lambda result: result["block_local"]["operation_routes"][-1]["accepted_absolute_model"].update(coefficient=0),
        ):
            result = SUCCESSOR.transform(self.parent, self.policy)
            mutate(result)
            result["artifact_sha256"] = SUCCESSOR.content_address(result)
            with self.assertRaisesRegex(ValueError, "successor"):
                SUCCESSOR.validate_successor(result, self.parent, self.policy)


if __name__ == "__main__":
    unittest.main()
