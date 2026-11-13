"""Protect the one-coefficient ECMUL refinement and its empirical admission gates."""
import copy
import importlib.util
import json
from pathlib import Path
import unittest

BASE = Path(__file__).resolve().parent


class EcmulIncrementalSuccessorTests(unittest.TestCase):
    def load(self):
        path = BASE / 'ecmul_incremental_successor.py'
        self.assertTrue(path.exists(), 'missing ECMUL incremental successor generator')
        spec = importlib.util.spec_from_file_location('ecmul_incremental_successor', path)
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        return (module, json.loads((BASE / module.PARENT_PATH).read_bytes()),
                json.loads((BASE / module.EVIDENCE_PATH).read_bytes()))

    def test_only_pc07_incremental_coefficient_and_provenance_change(self):
        m, parent, evidence = self.load()
        result = m.transform(parent, evidence)
        expected = copy.deepcopy(parent)
        for field in m.METADATA_FIELDS:
            expected[field] = result[field]
        route = next(r for r in expected['block_local']['operation_routes']
                     if r['route_key'] == 'precompile:0x07')
        route['incremental_coefficient'] = 0
        self.assertEqual(result, expected)
        self.assertEqual(result['artifact_sha256'], m.content_address(result))
        self.assertFalse(result['validation']['universal_upper_bound_claimed'])
        self.assertFalse(result['validation']['production_sdk_parity_established'])
        m.validate_successor(result, parent, evidence)

    def test_exact_route_aggregation_and_fractional_accepted_slope_survive(self):
        m, parent, evidence = self.load()
        derived = m.derive(parent, evidence)
        self.assertEqual(len(derived['rows']), 33)
        self.assertEqual(derived['full_deficits'], 0)
        self.assertEqual(derived['increment_deficits'], 0)
        mix = next(r for r in derived['rows'] if r['cohort'] == 'mix' and r['repetitions'] == 1)
        self.assertEqual(int(mix['candidate_route_scaled']), 300_000_000 * m.SCALE)
        fail = next(r for r in derived['rows'] if r['cohort'] == 'failure'
                    and r['native_gas_per_call'] == 200_000 and r['repetitions'] == 1)
        self.assertEqual(int(fail['candidate_route_scaled']), 131390224983920708487 * 200_000)
        self.assertNotEqual(int(fail['candidate_route_scaled']) % m.SCALE, 0)

    def test_identity_type_and_readdressed_numeric_drift_are_rejected(self):
        m, parent, evidence = self.load()
        for field, value in [('candidate_incremental_coefficient', 1),
                             ('production_sdk_parity_established', True)]:
            wrong = copy.deepcopy(evidence)
            wrong[field] = value
            with self.assertRaises(ValueError):
                m.transform(parent, wrong)
        wrong = copy.deepcopy(parent)
        wrong['block_local']['normal_dispatch_tariff'] = 0
        wrong['artifact_sha256'] = m.content_address(wrong)
        with self.assertRaises(ValueError):
            m.transform(wrong, evidence)
        for field in ('incremental_coefficient', 'accepted_absolute_model', 'structural_floor'):
            wrong = m.transform(parent, evidence)
            route = next(r for r in wrong['block_local']['operation_routes']
                         if r['route_key'] == 'precompile:0x07')
            if field == 'incremental_coefficient':
                route[field] = 1
            else:
                route[field]['coefficient'] += 1
            wrong['artifact_sha256'] = m.content_address(wrong)
            with self.subTest(field=field), self.assertRaises(ValueError):
                m.validate_successor(wrong, parent, evidence)

    def test_endpoint_replay_rejects_missing_groups_bad_types_and_mixed_programs(self):
        m, parent, evidence = self.load()
        rows = evidence['measurements']
        for field, value in [('raw_gas', True), ('actual_pgu', -1), ('events', 2**64),
                             ('elf_sha256', '0' * 64), ('sdk', '6.8.1'),
                             ('input_sha256', rows[1]['input_sha256']),
                             ('native_gas_per_call', 0), ('work_units', True),
                             ('old_node_scaled', '01'), ('cohort', 'reserved')]:
            wrong = copy.deepcopy(rows)
            wrong[0][field] = value
            with self.subTest(field=field), self.assertRaises(ValueError):
                m.replay_measurements(parent, wrong)
        with self.assertRaises(ValueError):
            m.replay_measurements(parent, rows[:-1])
        wrong = copy.deepcopy(rows)
        wrong[0] = copy.deepcopy(wrong[1])
        with self.assertRaises(ValueError):
            m.replay_measurements(parent, wrong)

    def test_replay_uses_raw_actuals_and_ceils_after_replacing_scaled_route(self):
        m, parent, evidence = self.load()
        rows = copy.deepcopy(evidence['measurements'])
        fail = next(r for r in rows if r['cohort'] == 'failure'
                    and r['native_gas_per_call'] == 200_000 and r['repetitions'] == 1)
        result = next(r for r in m.replay_measurements(parent, rows)['rows']
                      if r['sample_id'] == fail['sample_id'])
        scaled = (int(fail['old_node_scaled']) - int(result['old_route_scaled'])
                  + int(result['candidate_route_scaled']))
        self.assertEqual(result['candidate_node_pgu'], -(-scaled // m.SCALE))
        fail['raw_gas'] += 191 * 1_000_000_000
        fail['actual_pgu'] = fail['raw_gas'] * 10 // 191
        changed = m.replay_measurements(parent, rows)
        self.assertGreater(changed['full_deficits'], 0)
        self.assertGreater(changed['increment_deficits'], 0)


if __name__ == '__main__':
    unittest.main()
