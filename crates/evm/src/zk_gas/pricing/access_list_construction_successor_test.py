"""Checks the explicit raw-prefix successor without rewriting inherited pricing."""
import copy
import importlib.util
import json
from pathlib import Path
import unittest

BASE = Path(__file__).resolve().parent
SPEC = importlib.util.spec_from_file_location(
    'access_list_construction_successor', BASE / 'access_list_construction_successor.py'
)
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class ConstructionSuccessorTests(unittest.TestCase):
    """Keep finite projections, untouched parent prices and immutable identities inspectable."""

    def setUp(self):
        self.parent = json.loads((BASE / MODULE.PARENT_PATH).read_text())
        self.evidence = json.loads((BASE / MODULE.EVIDENCE_PATH).read_text())

    def test_only_new_reserve_and_its_domain_extend_numeric_parent(self):
        result = MODULE.transform(self.parent, self.evidence)
        block = dict(result['block_local'])
        self.assertEqual(block.pop('access_list_construction_pair_tariff'), 384 * 10**18)
        self.assertEqual(block, self.parent['block_local'])
        arithmetic = dict(result['arithmetic'])
        self.assertEqual(arithmetic.pop('access_list_construction_pair_bound'), 10**12)
        self.assertEqual(arithmetic, self.parent['arithmetic'])
        self.assertEqual(result['proposal_once'], self.parent['proposal_once'])
        self.assertEqual(result['execution_identity'], self.parent['execution_identity'])
        self.assertNotEqual(result['artifact_sha256'], self.parent['artifact_sha256'])

    def test_exact_large_projection_covers_the_recorded_high_priority_deficit(self):
        rows = MODULE.derive(self.parent, self.evidence)
        row = rows['aligned-k7680']
        self.assertEqual(row['comparison_pairs'], 29_487_360)
        self.assertEqual(row['comparison_pairs'] * 384, 11_323_146_240)
        self.assertEqual(row['parent_node_pgu'] + row['comparison_pairs'] * 384, 11_465_844_853)
        self.assertGreater(row['actual_pgu'] - row['parent_node_pgu'], 2_000_000_000)
        self.assertGreater(row['parent_node_pgu'] + row['comparison_pairs'] * 384, row['actual_pgu'])

    def test_unmeasured_and_unrealized_rows_cannot_be_promoted(self):
        rows = MODULE.derive(self.parent, self.evidence)
        self.assertEqual({k for k, v in rows.items() if v['actual_pgu'] is None}, {'empty', 'micro', 'dispersed'})
        self.assertFalse(rows['large-k7680']['pressure_realized'])
        self.assertTrue(rows['aligned-k7680']['pressure_realized'])
        self.assertEqual(rows['dispersed']['comparison_pairs'], 0)
        self.assertEqual(rows['micro']['comparison_pairs'], 1)

    def test_changed_evidence_and_parent_are_rejected(self):
        bad = copy.deepcopy(self.evidence)
        bad['samples'][-1]['actual_pgu'] -= 1
        with self.assertRaises(ValueError):
            MODULE.derive(self.parent, bad)
        bad_parent = copy.deepcopy(self.parent)
        bad_parent['proposal_once']['startup'] += 1
        with self.assertRaises(ValueError):
            MODULE.derive(bad_parent, self.evidence)

    def test_generated_semantics_and_price_are_not_mutable_compatibility_aliases(self):
        payload = MODULE.transform(self.parent, self.evidence)
        MODULE.validate_successor(payload, self.parent, self.evidence)
        payload['block_local']['access_list_construction_pair_tariff'] += 1
        with self.assertRaises(ValueError):
            MODULE.validate_successor(payload, self.parent, self.evidence)


if __name__ == '__main__':
    unittest.main()
