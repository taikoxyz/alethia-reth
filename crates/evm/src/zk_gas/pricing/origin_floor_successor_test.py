"""Replay the finite ORIGIN development points without accepting a new live table."""
import copy
from fractions import Fraction
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

BASE = Path(__file__).resolve().parent


class OriginFloorSuccessorTests(unittest.TestCase):
    def load(self):
        path = BASE / 'origin_floor_successor.py'
        self.assertTrue(path.exists(), 'missing ORIGIN candidate replay tool')
        spec = importlib.util.spec_from_file_location('origin_floor_successor', path)
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        parent, evidence = module.load_inputs()
        return module, parent, evidence

    def test_all_pairs_use_whole_target_increments_and_exact_margin(self):
        m, parent, evidence = self.load()
        replay = m.derive(parent, evidence)
        self.assertEqual(replay['constant_pgu'], 2176)
        self.assertEqual(len(replay['pairs']), 3)
        self.assertEqual(Fraction(**replay['max_positive_slope']), Fraction(12048, 7))
        self.assertEqual(Fraction(**replay['unrounded_required_pgu']), Fraction(15060, 7))
        self.assertLess(2176 - 64, Fraction(15060, 7))
        self.assertGreaterEqual(2176, Fraction(15060, 7))
        self.assertEqual(replay['pairs'], evidence['matched_pairs'])

    def test_only_origin_normal_coefficient_and_provenance_change(self):
        m, parent, evidence = self.load()
        candidate = m.transform(parent, evidence)
        expected = copy.deepcopy(parent)
        for field in m.METADATA_FIELDS:
            expected[field] = candidate[field]
        route = next(r for r in expected['block_local']['operation_routes']
                     if r['route_key'] == 'opcode:0x32')
        route['accepted_absolute_model']['coefficient'] = 2176 * 10**18
        self.assertEqual(candidate, expected)
        self.assertEqual(candidate['artifact_sha256'], m.content_address(candidate))
        self.assertEqual(candidate['successor']['publication_status_at_model_generation'],
                         'offline_candidate_pending_reserved_and_requotes')
        self.assertEqual(candidate['validation']['unopened_holdouts_used'], 0)
        self.assertFalse(candidate['validation']['universal_upper_bound_claimed'])

    def test_readdressed_output_drift_including_error_price_is_rejected(self):
        m, parent, evidence = self.load()
        original = m.transform(parent, evidence)
        for field in ('accepted_absolute_model', 'feature_error_absolute_model'):
            wrong = copy.deepcopy(original)
            route = next(r for r in wrong['block_local']['operation_routes']
                         if r['route_key'] == 'opcode:0x32')
            route[field]['coefficient'] += 1
            wrong['artifact_sha256'] = m.content_address(wrong)
            with self.subTest(field=field), self.assertRaises(ValueError):
                m.validate_successor(wrong, parent, evidence)
        wrong = copy.deepcopy(original)
        wrong['block_local']['operation_routes'][0]['accepted_absolute_model']['parameters']['constant'] += 1
        wrong['artifact_sha256'] = m.content_address(wrong)
        with self.assertRaises(ValueError):
            m.validate_successor(wrong, parent, evidence)

    def test_authenticated_inputs_and_cached_derivation_are_not_authority(self):
        m, parent, evidence = self.load()
        for field in ('candidate_constant_pgu', 'matched_pairs', 'policy'):
            wrong = copy.deepcopy(evidence)
            wrong[field] = None
            with self.subTest(field=field), self.assertRaises(ValueError):
                m.transform(parent, wrong)
        wrong = copy.deepcopy(parent)
        wrong['purpose'] = 'changed'
        wrong['artifact_sha256'] = m.content_address(wrong)
        with self.assertRaises(ValueError):
            m.transform(wrong, evidence)
        rows = copy.deepcopy(evidence['measurements'])
        rows[2]['raw_gas'] += 191 * 100000
        rows[2]['actual_pgu'] = rows[2]['raw_gas'] * 10 // 191
        self.assertGreater(m.replay_measurements(rows)['constant_pgu'], 2176)

    def test_wrong_count_lane_program_units_or_duplicate_is_rejected(self):
        m, _, evidence = self.load()
        for field, value in [('native_origin_count', 3), ('repetitions', 720),
                             ('lane', 'scaffold'), ('actual_pgu', True),
                             ('raw_gas', -1), ('input_bytes', 2**64),
                             ('sdk', '6.2.0'), ('elf_sha256', '0' * 64),
                             ('observer_sha256', '0' * 64), ('native_user_success', False),
                             ('native_exceptional_instructions', 1)]:
            rows = copy.deepcopy(evidence['measurements'])
            rows[0][field] = value
            with self.subTest(field=field), self.assertRaises(ValueError):
                m.replay_measurements(rows)
        rows = copy.deepcopy(evidence['measurements'])
        rows[1]['input_sha256'] = rows[0]['input_sha256']
        with self.assertRaises(ValueError):
            m.replay_measurements(rows)
        with self.assertRaises(ValueError):
            m.replay_measurements(evidence['measurements'][:-1])

    def test_source_reader_rejects_hash_drift_escape_and_oversized_artifacts(self):
        import hashlib
        m, _, _ = self.load()
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / 'sources'
            root.mkdir()
            sample = root / 'sample.json'
            sample.write_bytes(b'{}')
            expected = hashlib.sha256(b'{}').hexdigest()
            self.assertEqual(m.read_bound(root, 'sample.json', expected), b'{}')
            sample.write_bytes(b'[]')
            with self.assertRaises(ValueError):
                m.read_bound(root, 'sample.json', expected)
            (Path(tmp) / 'outside.json').write_bytes(b'{}')
            with self.assertRaises(ValueError):
                m.read_bound(root, '../outside.json', expected)
            sample.write_bytes(b'x' * (m.MAX_SOURCE_BYTES + 1))
            with self.assertRaises(ValueError):
                m.read_bound(root, 'sample.json', expected)

    def test_candidate_export_reuses_generator_without_accepting_or_mutating_801(self):
        m, parent, evidence = self.load()
        before = {p: (BASE / p).read_bytes() for p in
                  ('runtime-spec.json', 'accepted-runtime-spec.sha256', 'generated.rs', 'generate.py')}
        generator = m.load_table_generator()
        generator.validate_artifact(parent)
        candidate = m.transform(parent, evidence)
        with self.assertRaises(ValueError):
            generator.render(candidate)
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / 'candidate'
            m.export_candidate(candidate, parent, evidence, out)
            self.assertEqual(json.loads((out / 'runtime-spec.json').read_bytes()), candidate)
            self.assertIn(candidate['artifact_sha256'], (out / 'generated.rs').read_text())
            m.check_candidate(candidate, parent, evidence, out)
            with self.assertRaises(ValueError):
                m.export_candidate(candidate, parent, evidence, out)
            (out / 'generated.rs').write_text('tampered')
            with self.assertRaises(ValueError):
                m.check_candidate(candidate, parent, evidence, out)
        self.assertEqual(before, {p: (BASE / p).read_bytes() for p in before})
        generator.validate_artifact(parent)
        with self.assertRaises(ValueError):
            m.export_candidate(candidate, parent, evidence, BASE)


if __name__ == '__main__':
    unittest.main()
