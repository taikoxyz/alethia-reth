"""Authenticate the composite dispatch reserve without changing inherited pricing."""
import copy
from fractions import Fraction
import importlib.util
import json
from pathlib import Path
import unittest

BASE = Path(__file__).resolve().parent


class NormalDispatchSuccessorTests(unittest.TestCase):
    def load(self):
        spec = importlib.util.spec_from_file_location('normal_dispatch_successor', BASE / 'normal_dispatch_successor.py')
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        parent = json.loads((BASE / module.PARENT_PATH).read_text())
        evidence = json.loads((BASE / module.EVIDENCE_PATH).read_text())
        return module, parent, evidence

    def test_only_additive_tariff_changes_and_all_sixty_pair_margins_hold(self):
        m, parent, evidence = self.load()
        result = m.transform(parent, evidence)
        expected = copy.deepcopy(parent)
        expected['block_local']['normal_dispatch_tariff'] = 64 * 10**18
        for key in ('artifact_sha256', 'purpose', 'generator', 'parent_generator', 'validation', 'parent_validation', 'successor'):
            expected[key] = result[key]
        self.assertEqual(result, expected)
        derived = m.derive(parent, evidence)
        self.assertEqual(derived['coefficient_pgu'], 64)
        self.assertEqual(len(derived['pairs']), 60)
        self.assertGreater(derived['maximum_required_pgu'], Fraction(58384, 1000))
        self.assertLess(derived['maximum_required_pgu'], Fraction(58385, 1000))
        self.assertEqual(len(evidence['measurements']), 60)
        for p in derived['pairs']:
            self.assertGreaterEqual(int(p['parent_node_increment_scaled']) + p['extra_completed_user_opcodes'] * 64 * 10**18,
                                    Fraction(5, 4) * p['actual_increment_pgu'] * 10**18)
        self.assertFalse(result['validation']['new_native_repricing_performed'])
        self.assertEqual(result['validation']['prospective_actuals_used_for_fit'], 0)
        m.validate_successor(result, parent, evidence)

    def test_parent_evidence_and_numeric_or_policy_drift_fail_closed(self):
        m, parent, evidence = self.load()
        for mutate in (lambda x: x['measurements'][0].update(actual_proposal_sp1_pgu=1),
                       lambda x: x['measurements'][0].update(completed_user_opcode_events=0),
                       lambda x: x['policy'].update(candidate_pgu=65),
                       lambda x: x['matched_pairs'].pop(),
                       lambda x: x['sources'].update(independent_panel_sha256='0' * 64)):
            wrong = copy.deepcopy(evidence); mutate(wrong)
            with self.assertRaises(ValueError): m.transform(parent, wrong)
        wrong = copy.deepcopy(parent); wrong['block_local']['authorization_validation_tariff'] = 0
        wrong['artifact_sha256'] = m.content_address(wrong)
        with self.assertRaises(ValueError): m.transform(wrong, evidence)
        wrong = m.transform(parent, evidence); wrong['block_local']['operation_routes'][0]['accepted_absolute_model']['common_dispatch'] += 1
        wrong['artifact_sha256'] = m.content_address(wrong)
        with self.assertRaises(ValueError): m.validate_successor(wrong, parent, evidence)


if __name__ == '__main__': unittest.main()
