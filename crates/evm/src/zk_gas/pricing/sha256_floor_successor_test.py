"""Recompute the finite SHA256 envelope while protecting every inherited price."""
import copy
from fractions import Fraction
import importlib.util
import json
from pathlib import Path
import unittest

BASE = Path(__file__).resolve().parent


class Sha256FloorSuccessorTests(unittest.TestCase):
    def load(self):
        path = BASE / 'sha256_floor_successor.py'
        self.assertTrue(path.exists(), 'missing SHA256 floor successor generator')
        spec = importlib.util.spec_from_file_location('sha256_floor_successor', path)
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        return (module, json.loads((BASE / module.PARENT_PATH).read_bytes()),
                json.loads((BASE / module.EVIDENCE_PATH).read_bytes()))

    def test_raw_count_pairs_determine_the_minimal_rounded_floor(self):
        m, parent, evidence = self.load()
        derived = m.derive(parent, evidence)
        self.assertEqual(derived['constant_pgu'], 81024)
        self.assertEqual(derived['pairs'], evidence['matched_pairs'])
        self.assertEqual(len(derived['pairs']), 36)
        for pair in derived['pairs']:
            margin = Fraction(**pair['margin_pgu_per_call'])
            self.assertGreaterEqual(max(81024, pair['structural_pgu_per_call']), margin)
        binding = next(p for p in derived['pairs'] if p['low_sample_id'] == 'sha256-bulk-zero-l4096-n0'
                       and p['high_sample_id'] == 'sha256-bulk-zero-l4096-n1')
        self.assertEqual(binding['actual_increment_pgu'], 129577)
        self.assertEqual(binding['extra_events'], 2)
        self.assertEqual(Fraction(**binding['margin_pgu_per_call']), Fraction(647885, 8))
        self.assertLess(max(81024 - 64, binding['structural_pgu_per_call']), Fraction(647885, 8))

    def test_only_pc02_constant_and_explicit_provenance_change(self):
        m, parent, evidence = self.load()
        result = m.transform(parent, evidence)
        expected = copy.deepcopy(parent)
        for field in m.METADATA_FIELDS:
            expected[field] = result[field]
        route = next(r for r in expected['block_local']['operation_routes'] if r['route_key'] == 'precompile:0x02')
        route['accepted_absolute_model']['constant'] = 81024 * 10**18
        self.assertEqual(result, expected)
        self.assertEqual(result['artifact_sha256'], m.content_address(result))
        self.assertFalse(result['validation']['universal_upper_bound_claimed'])
        self.assertFalse(result['validation']['production_sdk_parity_established'])
        m.validate_successor(result, parent, evidence)

    def test_authenticated_inputs_and_readdressed_output_drift_are_rejected(self):
        m, parent, evidence = self.load()
        for field, value in [('candidate_constant_pgu', 1), ('production_sdk_parity_established', True)]:
            wrong = copy.deepcopy(evidence)
            wrong[field] = value
            with self.assertRaises(ValueError):
                m.transform(parent, wrong)
        wrong = copy.deepcopy(parent)
        wrong['block_local']['operation_routes'][0]['composition'] = 'changed'
        wrong['artifact_sha256'] = m.content_address(wrong)
        with self.assertRaises(ValueError):
            m.transform(wrong, evidence)
        result = m.transform(parent, evidence)
        for field in ('constant', 'coefficient'):
            wrong = copy.deepcopy(result)
            route = next(r for r in wrong['block_local']['operation_routes'] if r['route_key'] == 'precompile:0x02')
            route['accepted_absolute_model'][field] += 1
            wrong['artifact_sha256'] = m.content_address(wrong)
            with self.assertRaises(ValueError):
                m.validate_successor(wrong, parent, evidence)
        wrong = copy.deepcopy(result)
        route = next(r for r in wrong['block_local']['operation_routes'] if r['route_key'] == 'precompile:0x02')
        route['structural_floor']['coefficient'] += 1
        wrong['artifact_sha256'] = m.content_address(wrong)
        with self.assertRaises(ValueError):
            m.validate_successor(wrong, parent, evidence)

    def test_raw_replay_rejects_invalid_values_ownership_and_identity(self):
        m, _, evidence = self.load()
        rows = evidence['measurements']
        for field in ('raw_gas', 'actual_pgu', 'length', 'repetitions', 'target_events',
                      'native_gas_per_call', 'native_staticcall_count', 'users', 'input_bytes'):
            for value in (True, -1, 2**64):
                wrong = copy.deepcopy(rows)
                wrong[0][field] = value
                with self.subTest(field=field, value=value), self.assertRaises(ValueError):
                    m.replay_measurements(wrong)
        for field, value in [('users', 1), ('target_events', 1), ('native_staticcall_count', 1),
                             ('native_gas_per_call', 1), ('sample_id', rows[1]['sample_id']),
                             ('input_sha256', rows[1]['input_sha256']), ('pattern', 'other'),
                             ('cohort', 'other'), ('sdk', '6.1.0'), ('elf_sha256', '0' * 64),
                             ('observer_sha256', '0' * 64)]:
            wrong = copy.deepcopy(rows)
            wrong[0][field] = value
            with self.subTest(field=field), self.assertRaises(ValueError):
                m.replay_measurements(wrong)
        wrong = copy.deepcopy(rows)
        wrong[0] = copy.deepcopy(wrong[1])
        with self.assertRaises(ValueError):
            m.replay_measurements(wrong)
        with self.assertRaises(ValueError):
            m.replay_measurements(rows[:-1])

    def test_replay_uses_endpoints_instead_of_cached_pair_or_candidate(self):
        m, _, evidence = self.load()
        rows = copy.deepcopy(evidence['measurements'])
        row = next(r for r in rows if r['sample_id'] == 'sha256-bulk-zero-l4096-n1')
        row['raw_gas'] += 191 * 100000
        row['actual_pgu'] = row['raw_gas'] * 10 // 191
        self.assertGreater(m.replay_measurements(rows)['constant_pgu'], 81024)
        # Negative increments remain signed facts; their required margin clips at zero.
        rows = copy.deepcopy(evidence['measurements'])
        for row in rows:
            row['raw_gas'] = (100000 - row['repetitions']) * 191
            row['actual_pgu'] = row['raw_gas'] * 10 // 191
        replay = m.replay_measurements(rows)
        self.assertEqual(replay['constant_pgu'], 0)
        self.assertTrue(all(p['margin_pgu_per_call']['numerator'] == 0 for p in replay['pairs']))


if __name__ == '__main__':
    unittest.main()
