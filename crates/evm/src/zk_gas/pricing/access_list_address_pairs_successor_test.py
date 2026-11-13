"""Address-table reserve preserves inherited prices and transaction boundaries."""
import copy
import importlib.util
import json
from pathlib import Path
import unittest

BASE = Path(__file__).resolve().parent
SPEC = importlib.util.spec_from_file_location('address_pairs_successor', BASE / 'access_list_address_pairs_successor.py')
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class AddressPairsSuccessorTests(unittest.TestCase):
    def setUp(self):
        self.parent = json.loads((BASE / MODULE.PARENT_PATH).read_text())
        self.evidence = json.loads((BASE / MODULE.EVIDENCE_PATH).read_text())

    def test_one_independent_tariff_and_domain_leave_inherited_numbers_unchanged(self):
        result = MODULE.transform(self.parent, self.evidence)
        block = dict(result['block_local'])
        self.assertEqual(block.pop('access_list_address_pair_tariff'), 128 * 10**18)
        self.assertEqual(block, self.parent['block_local'])
        arithmetic = dict(result['arithmetic'])
        self.assertEqual(arithmetic.pop('access_list_address_pair_bound'), 10**12)
        arithmetic['declared_worst_case_scaled_pgu'] -= 128 * 10**18 * 10**12
        self.assertEqual(arithmetic, self.parent['arithmetic'])
        self.assertLess(result['arithmetic']['declared_worst_case_scaled_pgu'], 2**127)
        for key in ('proposal_once', 'execution_identity', 'coverage', 'protocol_compatibility'):
            if key in self.parent:
                self.assertEqual(result[key], self.parent[key])
        self.assertNotEqual(result['artifact_sha256'], self.parent['artifact_sha256'])

    def test_finalized_transaction_pairs_and_measured_deficit(self):
        rows = MODULE.derive(self.parent, self.evidence)
        row = rows['aligned']
        self.assertEqual(row['expected_address_pairs'], 2 * 6144 * 6143 // 2)
        self.assertNotEqual(row['expected_address_pairs'], 12288 * 12287 // 2)
        self.assertEqual(row['actual_sp1_pgu'] - row['old_model_node_pgu'], 2_754_519_621)
        self.assertEqual(row['old_model_node_pgu'] + row['expected_address_pairs'] * 128, 5_086_511_107)
        self.assertEqual(row['old_model_proposal_pgu'] + row['expected_address_pairs'] * 128, 5_241_452_937)
        self.assertIsNone(row['whole_input_pressure_qualified'])
        self.assertTrue(row['first_observed_local_equal_hashes'])
        self.assertEqual(rows['normal']['expected_address_pairs'], 12)
        self.assertFalse(rows['reference']['first_observed_local_equal_hashes'])

    def test_immutable_inputs_and_generated_semantics_fail_closed(self):
        for target in ('parent', 'evidence'):
            parent, evidence = copy.deepcopy(self.parent), copy.deepcopy(self.evidence)
            if target == 'parent':
                parent['block_local']['access_list_construction_other_pair_tariff'] += 1
            else:
                evidence['samples'][-1]['actual_sp1_pgu'] -= 1
            with self.assertRaises(ValueError):
                MODULE.derive(parent, evidence)
        payload = MODULE.transform(self.parent, self.evidence)
        MODULE.validate_successor(payload, self.parent, self.evidence)
        payload['block_local']['access_list_address_pair_tariff'] += 1
        with self.assertRaises(ValueError):
            MODULE.validate_successor(payload, self.parent, self.evidence)


if __name__ == '__main__':
    unittest.main()
