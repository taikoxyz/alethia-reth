"""Replay sealed ordinary tariffs while preserving unrelated policy and feature boundaries."""
import copy
from fractions import Fraction
import importlib.util
import json
from pathlib import Path
import unittest

BASE = Path(__file__).resolve().parent
ROUTES = {'opcode:0x38', 'opcode:0x39', 'opcode:0xa3', 'opcode:0xa4',
          'opcode:0xfa:confirmed_spawn_wrapper'}


class OrdinaryTariffSuccessorTests(unittest.TestCase):
    def load(self):
        path = BASE / 'ordinary_tariff_successor.py'
        self.assertTrue(path.exists(), 'missing independently replayable ordinary tariff generator')
        spec = importlib.util.spec_from_file_location('ordinary_tariff_successor', path)
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        return (module, json.loads((BASE / module.PARENT_PATH).read_bytes()),
                json.loads((BASE / module.EVIDENCE_PATH).read_bytes()))

    def test_replay_reconstructs_every_pair_and_margin_from_raw_facts(self):
        m, parent, evidence = self.load()
        derived = m.derive(parent, evidence)
        self.assertEqual(derived['candidate'], {'fixed_pgu': {'codesize': 2048, 'codecopy': 1600,
            'staticcall': 9280}, 'log_base_pgu': {'log3': 45888, 'log4': 64064}, 'log_word_pgu': 2688})
        self.assertEqual(len(derived['pairs']), 34)
        self.assertEqual(derived['pairs'], evidence['candidate']['matched_pairs'])
        for pair in derived['pairs']:
            op = pair['operation']
            quoted = (derived['candidate']['fixed_pgu'][op] if op in derived['candidate']['fixed_pgu']
                      else derived['candidate']['log_base_pgu'][op] +
                      derived['candidate']['log_word_pgu'] * ((pair['length'] + 31) // 32))
            self.assertGreaterEqual(quoted * pair['extra_events'], Fraction(5, 4) * pair['actual_increment_pgu'])
        self.assertEqual(m.round_margin(Fraction(256, 5)), 64)
        self.assertEqual(m.round_margin(Fraction(256, 5) + Fraction(1, 1000)), 128)

    def test_scoped_route_changes_keep_ownership_floors_and_envelopes(self):
        m, parent, evidence = self.load()
        result = m.transform(parent, evidence)
        old = {r['route_key']: r for r in parent['block_local']['operation_routes']}
        new = {r['route_key']: r for r in result['block_local']['operation_routes']}
        self.assertEqual({key for key in old if old[key] != new[key]}, ROUTES)
        for key in ROUTES:
            expected = copy.deepcopy(old[key]); expected['accepted_absolute_model'] = new[key]['accepted_absolute_model']
            self.assertEqual(new[key], expected)
        for key, rate in [('opcode:0x38', 2048), ('opcode:0x39', 1600),
                          ('opcode:0xfa:confirmed_spawn_wrapper', 9280)]:
            expected = copy.deepcopy(old[key]['accepted_absolute_model']); expected['coefficient'] = rate * 10**18
            self.assertEqual(new[key]['accepted_absolute_model'], expected)
        self.assertEqual(new['opcode:0x38']['accepted_absolute_model']['raw_gas'], 2)
        self.assertEqual(new['opcode:0x39']['accepted_absolute_model']['maximum_raw_gas'], 12)
        for key, base in [('opcode:0xa3', 45888), ('opcode:0xa4', 64064)]:
            model = new[key]['accepted_absolute_model']
            self.assertEqual(model['model_kind'], 'log_piecewise')
            self.assertEqual(model['input_envelope'], {'kind': 'log'})
            self.assertEqual(model['shared_memory'], old[key]['accepted_absolute_model']['shared_memory'])
            self.assertEqual(model['parameters'], {'constant': base * 10**18, 'topic': 0,
                'nonzero_data': 2688 * 10**18, 'additional_word': 2688 * 10**18})
            self.assertEqual(model['common_dispatch'], 0)
        expected = copy.deepcopy(parent)
        for field in m.METADATA_FIELDS: expected[field] = result[field]
        expected['block_local']['operation_routes'] = result['block_local']['operation_routes']
        self.assertEqual(result, expected)

    def test_authenticated_inputs_and_readdressed_output_drift_fail_closed(self):
        m, parent, evidence = self.load()
        for mutate in (lambda e: e['candidate']['measurements'][0].update(actual_pgu=1),
                       lambda e: e['candidate']['measurements'][0].update(target_events=1),
                       lambda e: e['candidate']['matched_pairs'].pop(),
                       lambda e: e['candidate']['candidate']['fixed_pgu'].update(codecopy=1),
                       lambda e: e['seal'].update(reserved_actuals_present_at_seal=True)):
            wrong = copy.deepcopy(evidence); mutate(wrong)
            with self.assertRaises(ValueError): m.transform(parent, wrong)
        wrong = copy.deepcopy(parent); wrong['ownership']['changed'] = True
        wrong['artifact_sha256'] = m.content_address(wrong)
        with self.assertRaises(ValueError): m.transform(wrong, evidence)
        result = m.transform(parent, evidence)
        self.assertEqual(result, m.transform(parent, evidence))
        self.assertEqual(result['artifact_sha256'], m.content_address(result))
        self.assertNotEqual(parent['artifact_sha256'], result['artifact_sha256'])
        for mutate in (lambda p: p['block_local'].update(normal_dispatch_tariff=0),
                       lambda p: p['block_local']['operation_routes'][0].update(composition='changed'),
                       lambda p: p['validation'].update(reserved_actuals_used_for_fit=1)):
            wrong = copy.deepcopy(result); mutate(wrong); wrong['artifact_sha256'] = m.content_address(wrong)
            with self.assertRaises(ValueError): m.validate_successor(wrong, parent, evidence)

    def test_replay_does_not_trust_cached_pairs_prices_or_scaffold_truth(self):
        m, _, evidence = self.load()
        raw = evidence['candidate']['measurements']
        baseline = m.replay_measurements(raw)
        modified = copy.deepcopy(raw)
        for row in modified:
            if row['lane'] == 'scaffold':
                row['raw_gas'] += 191 * 10**12
                row['actual_pgu'] = row['raw_gas'] * 10 // 191
        self.assertEqual(baseline, m.replay_measurements(modified))
        target = copy.deepcopy(raw)
        target[2]['raw_gas'] += 191 * 10**12
        target[2]['actual_pgu'] = target[2]['raw_gas'] * 10 // 191
        self.assertNotEqual(baseline['candidate'], m.replay_measurements(target)['candidate'])
        # A STATICCALL warmup is present in both inputs and cancels: count differences are N differences.
        static = [p for p in baseline['pairs'] if p['operation'] == 'staticcall']
        self.assertEqual([p['extra_events'] for p in static], [256, 4096, 3840])
        log4 = [p for p in baseline['pairs'] if p['operation'] == 'log4']
        self.assertEqual(log4[0]['extra_events'], 2)
        # Wrong declared user/event ownership must fail even before evidence authentication.
        target = copy.deepcopy(raw); target[-1]['target_events'] //= 2
        with self.assertRaises(ValueError): m.replay_measurements(target)
        with self.assertRaises(ValueError): m.replay_measurements(raw + [raw[0]])

    def test_each_endpoint_recomputes_unsigned_raw_gas_pgu_normalization(self):
        m, _, evidence = self.load()
        raw = evidence['candidate']['measurements']
        for invalid in (-1, 1.0, True, 2**64):
            wrong = copy.deepcopy(raw); wrong[0]['raw_gas'] = invalid
            with self.assertRaises(ValueError): m.replay_measurements(wrong)
        for index in (0, 1, len(raw) - 1):
            wrong = copy.deepcopy(raw); wrong[index]['actual_pgu'] += 1
            with self.assertRaises(ValueError): m.replay_measurements(wrong)


if __name__ == '__main__': unittest.main()
