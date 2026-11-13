"""Focused invariants for the independently named different-prefix successor."""
import copy
import importlib.util
import json
from pathlib import Path
import unittest

BASE = Path(__file__).resolve().parent
SPEC = importlib.util.spec_from_file_location('other_pairs_successor', BASE / 'access_list_other_pairs_successor.py')
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class OtherPairsSuccessorTests(unittest.TestCase):
    def setUp(self):
        self.parent = json.loads((BASE / MODULE.PARENT_PATH).read_text())
        self.evidence = json.loads((BASE / MODULE.EVIDENCE_PATH).read_text())

    def test_exactly_one_new_numeric_tariff_and_domain(self):
        result = MODULE.transform(self.parent, self.evidence)
        block = dict(result['block_local'])
        self.assertEqual(block.pop('access_list_construction_other_pair_tariff'), 96 * 10**18)
        self.assertEqual(block, self.parent['block_local'])
        arithmetic = dict(result['arithmetic'])
        self.assertEqual(arithmetic.pop('access_list_construction_other_pair_bound'), 10**12)
        arithmetic['declared_worst_case_scaled_pgu'] -= 96 * 10**18 * 10**12
        self.assertEqual(arithmetic, self.parent['arithmetic'])
        self.assertLess(result['arithmetic']['declared_worst_case_scaled_pgu'], 2**127)
        for key in ('proposal_once', 'execution_identity', 'coverage', 'protocol_compatibility'):
            if key in self.parent: self.assertEqual(result[key], self.parent[key])
        self.assertNotEqual(result['artifact_sha256'], self.parent['artifact_sha256'])

    def test_true_disjoint_pairs_and_literal_projection(self):
        rows = MODULE.derive(self.parent, self.evidence)
        row = rows['packed-aligned']
        self.assertEqual(row['expected_new_other_pairs'], 2 * 7680 * 7679 // 2)
        self.assertEqual(row['expected_new_other_pairs'] * 96, 5_661_573_120)
        self.assertEqual(row['old_model_node_pgu'] + row['expected_new_other_pairs'] * 96, 5_869_373_955)
        self.assertEqual(row['old_model_proposal_pgu'] + row['expected_new_other_pairs'] * 96, 6_024_315_785)
        self.assertIsNone(row['whole_input_pressure_qualified'])
        self.assertFalse(rows['packed-reference']['first_observed_local_pressure'])
        self.assertEqual(rows['short-k64']['expected_new_other_pairs'], 2016)

    def test_parent_evidence_and_generated_semantics_fail_closed(self):
        for target in ('parent','evidence'):
            parent,evidence=copy.deepcopy(self.parent),copy.deepcopy(self.evidence)
            if target=='parent':parent['block_local']['access_list_construction_pair_tariff']+=1
            else:evidence['samples'][-1]['actual_sp1_pgu']-=1
            with self.assertRaises(ValueError):MODULE.derive(parent,evidence)
        payload=MODULE.transform(self.parent,self.evidence)
        MODULE.validate_successor(payload,self.parent,self.evidence)
        payload['block_local']['access_list_construction_other_pair_tariff']+=1
        with self.assertRaises(ValueError):MODULE.validate_successor(payload,self.parent,self.evidence)


if __name__ == '__main__': unittest.main()
