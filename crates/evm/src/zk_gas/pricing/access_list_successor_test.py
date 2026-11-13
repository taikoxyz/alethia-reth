"""Exact raw-item derivation, diagnostic boundaries and closed successor scope."""
import copy
from fractions import Fraction
import importlib.util
import json
from pathlib import Path
import unittest

BASE = Path(__file__).resolve().parent
SPEC = importlib.util.spec_from_file_location("access_list_successor", BASE/"access_list_successor.py")
assert SPEC is not None and SPEC.loader is not None
SUCCESSOR = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(SUCCESSOR)


class AccessListSuccessorTests(unittest.TestCase):
    def setUp(self):
        self.parent = json.loads((BASE/SUCCESSOR.PARENT_PATH).read_text())
        self.policy = json.loads((BASE/SUCCESSOR.EVIDENCE_PATH).read_text())

    def test_exact_constraints_then_integer_ceiling_and_power_of_two(self):
        requirement, ceiling, coefficient = SUCCESSOR.derive(self.policy)
        self.assertEqual(requirement, Fraction(19375485, 4094))
        self.assertEqual(ceiling, 4733)
        self.assertEqual(coefficient, 8192)
        for collection in SUCCESSOR.FIT_COLLECTIONS:
            for pair in self.policy['fit_pairs'][collection]:
                items = pair['raw_unique_count_deltas']['raw_address_items']
                actual = pair['actual_increment_pgu']
                if items > 0 and actual > 0:
                    new_delta = Fraction(int(pair['node_increment_scaled']), SUCCESSOR.SCALE) + items*coefficient
                    self.assertGreaterEqual(new_delta, Fraction(5,4)*actual)

    def test_adds_only_normalized_item_tariff_and_provenance(self):
        result = SUCCESSOR.transform(self.parent, self.policy)
        expected = copy.deepcopy(self.parent)
        expected['block_local']['access_list_item_tariff'] = 8192*10**18
        for key in ('artifact_sha256','purpose','generator','parent_generator','validation','parent_validation','successor'):
            expected[key] = result[key]
        self.assertEqual(result, expected)
        self.assertEqual(result['block_local']['authorization_validation_tariff'], 200_000*10**18)
        self.assertFalse(result['validation']['new_native_repricing_performed'])
        self.assertFalse(result['validation']['raw_key_tariff_added'])
        self.assertFalse(result['validation']['universal_upper_bound_claimed'])
        SUCCESSOR.validate_successor(result, self.parent, self.policy)

    def test_projects_full23_coverage_but_preserves_unpriced_zero_item_order_increment(self):
        self.assertEqual(SUCCESSOR.projected_underquotes(self.policy, 0), 3)
        self.assertEqual(SUCCESSOR.projected_underquotes(self.policy, 8192), 0)
        order = self.policy['diagnostic_pairs']['ordering_comparisons'][0]
        self.assertEqual(order['raw_unique_count_deltas']['raw_address_items'], 0)
        self.assertEqual(order['actual_increment_pgu'], 1502)
        self.assertEqual(order['node_increment_scaled'], '0')
        self.assertFalse(order['node_increment_covered'])
        self.assertEqual(int(order['node_increment_scaled']) +
                         order['raw_unique_count_deltas']['raw_address_items']*8192*10**18, 0)
        for row in self.policy['measurements']:
            if row['counts_total']['raw_address_items'] == 0:
                self.assertGreaterEqual(row['parent_complete_scaled'], row['actual_pgu']*10**18)

    def test_rejects_policy_schema_scope_parent_and_successor_drift(self):
        for mutate in (
            lambda policy: policy.update(extra_field=1),
            lambda policy: policy.update(raw_key_tariff=8192),
            lambda policy: policy['measurements'][0].update(actual_pgu=1),
            lambda policy: policy['measurements'].append(copy.deepcopy(policy['measurements'][0])),
            lambda policy: policy['fit_pairs']['grouping_comparisons'][0].update(actual_increment_pgu=1),
        ):
            policy = copy.deepcopy(self.policy); mutate(policy)
            with self.assertRaisesRegex(ValueError, 'sealed access-list'):
                SUCCESSOR.transform(self.parent, policy)
        parent = copy.deepcopy(self.parent)
        parent['block_local']['authorization_validation_tariff'] = 0
        parent['artifact_sha256'] = SUCCESSOR.content_address(parent)
        with self.assertRaisesRegex(ValueError, 'parent'):
            SUCCESSOR.transform(parent, self.policy)
        for mutate in (
            lambda result: result['block_local'].update(access_list_item_tariff=4096*10**18),
            lambda result: result['block_local'].update(raw_key_tariff=1),
            lambda result: result['block_local'].update(authorization_validation_tariff=0),
            lambda result: result['generator'].update(file_sha256='0'*64),
        ):
            result = SUCCESSOR.transform(self.parent, self.policy); mutate(result)
            result['artifact_sha256'] = SUCCESSOR.content_address(result)
            with self.assertRaisesRegex(ValueError, 'successor'):
                SUCCESSOR.validate_successor(result, self.parent, self.policy)


if __name__ == '__main__':
    unittest.main()
