#!/usr/bin/env python3
"""Replay retained SHA256 count contrasts into a constant-only empirical successor."""
import argparse
import copy
from fractions import Fraction
import hashlib
from itertools import combinations
import json
from pathlib import Path
import re

BASE = Path(__file__).resolve().parent
PARENT_SHA256 = 'c0877d65a0b5fac178342645b5689001e0801718fe1dae0150c59c39ee1c15c1'
PARENT_PATH = 'successors/parent-c0877d65.json'
PARENT_FILE_SHA256 = 'db78423715502f60c93c6fa1fb2b6a074808bec69ae5d703059da4fc27f5e48d'
EVIDENCE_PATH = 'successors/sha256-floor-v1.json'
EVIDENCE_SHA256 = '8a3c0bae858a67762c4232d9a3dc795093f0e9bb494537901e13ec430ab1e01c'
EVIDENCE_FILE_SHA256 = 'ddba5f9344a091bb9f2c8a00e71343f7d7e5ddb2788bb5693b6bbe3308110949'
ELF_SHA256 = 'b76baf2e0f3fa3fe6f8a20fe53dbf82f58601ed14b370dfc0f8d31db2069e034'
OBSERVER_SHA256 = '1998fb85e62fbbd190593ed1a71af044d197224174d45295f968da0d1a60330b'
SCALE = 10**18
METADATA_FIELDS = ('artifact_sha256', 'purpose', 'generator', 'parent_generator',
                   'validation', 'parent_validation', 'successor')
# These are the retained finite population, not claims about other lengths or schedules.
POPULATION = {64: ('bulk', 42557), 4096: ('bulk', 7999), 1048576: ('bulk', 32),
              1269664: ('linear', 24), 1269665: ('linear', 24), 2097152: ('linear', 7)}


def canonical_json(value):
    """Encode the complete typed JSON deterministically, including provenance."""
    return json.dumps(value, sort_keys=True, separators=(',', ':')).encode()


def digest(value):
    """Authenticate canonical evidence bytes independently of pretty printing."""
    return hashlib.sha256(canonical_json(value)).hexdigest()


def content_address(value):
    """Exclude only the self-referential artifact address."""
    return digest({key: item for key, item in value.items() if key != 'artifact_sha256'})


def replay_measurements(measurements):
    """Derive all same-length/pattern count contrasts directly from whole-input actuals.

    Both users contribute SHA calls. No loop, wrapper, memory, startup or scaffold credit
    is subtracted. The inherited 42 PGU/native-gas structural slope covers a contrast
    whenever it already exceeds the exact 5/4 nonnegative marginal observation.
    """
    if not isinstance(measurements, list) or len(measurements) != 36:
        raise ValueError('wrong retained SHA256 measurement population')
    groups, sample_ids, input_hashes = {}, set(), set()
    for row in measurements:
        for field in ('raw_gas', 'actual_pgu', 'length', 'repetitions', 'target_events',
                      'native_gas_per_call', 'native_staticcall_count', 'users', 'input_bytes'):
            if type(row[field]) is not int or not 0 <= row[field] < 2**64:
                raise ValueError('unsigned u64 measurement required')
        length, pattern = row['length'], row['pattern']
        if length not in POPULATION or pattern not in ('zero', 'a5'):
            raise ValueError('unsupported retained length or content schedule')
        cohort, full_count = POPULATION[length]
        repetitions = row['repetitions']
        sample_id = f'sha256-bulk-{pattern}-l{length}-n{repetitions}'
        if (row['sample_id'] != sample_id or sample_id in sample_ids or
            row['input_sha256'] in input_hashes or repetitions not in (0, 1, full_count)):
            raise ValueError('duplicate or inconsistent retained sample identity')
        if (row['cohort'] != cohort or row['users'] != 2 or
            row['target_events'] != 2 * repetitions or
            row['native_staticcall_count'] != row['target_events'] or
            row['native_gas_per_call'] != 60 + 12 * ((length + 31) // 32)):
            raise ValueError('wrong SHA256 event ownership or native gas')
        if row['actual_pgu'] != row['raw_gas'] * 10 // 191:
            raise ValueError('endpoint PGU differs from raw-gas normalization')
        if (row['elf_sha256'] != ELF_SHA256 or row['sdk'] != '6.3.0' or
            row['observer_sha256'] != OBSERVER_SHA256):
            raise ValueError('mixed program, SDK or observer identity')
        for field in ('input_sha256', 'actual_report_sha256', 'native_report_sha256'):
            if not isinstance(row[field], str) or re.fullmatch('[0-9a-f]{64}', row[field]) is None:
                raise ValueError('invalid retained artifact identity')
        if not isinstance(row['public_output'], str) or re.fullmatch('0x[0-9a-f]{64}', row['public_output']) is None:
            raise ValueError('invalid retained public output identity')
        sample_ids.add(sample_id)
        input_hashes.add(row['input_sha256'])
        groups.setdefault((length, pattern), []).append(row)
    if set(groups) != {(length, pattern) for length in POPULATION for pattern in ('a5', 'zero')}:
        raise ValueError('missing retained length or content schedule')
    pairs, required = [], Fraction(0)
    for (length, pattern), rows in sorted(groups.items()):
        if sorted(row['repetitions'] for row in rows) != [0, 1, POPULATION[length][1]]:
            raise ValueError('missing or duplicate within-group event count')
        for low, high in combinations(sorted(rows, key=lambda row: row['target_events']), 2):
            events = high['target_events'] - low['target_events']
            actual = high['actual_pgu'] - low['actual_pgu']
            margin = max(Fraction(0), Fraction(actual, events)) * Fraction(5, 4)
            native_gas = low['native_gas_per_call']
            structural = 42 * native_gas
            needs_floor = margin > structural
            if needs_floor:
                required = max(required, margin)
            pairs.append({'low_sample_id': low['sample_id'], 'high_sample_id': high['sample_id'],
                'extra_events': events, 'actual_increment_pgu': actual,
                'margin_pgu_per_call': {'numerator': margin.numerator, 'denominator': margin.denominator},
                'native_gas_per_call': native_gas, 'structural_pgu_per_call': structural,
                'fixed_floor_required': needs_floor})
    constant = -(-required.numerator // (64 * required.denominator)) * 64
    if any(max(constant, pair['structural_pgu_per_call']) < Fraction(**pair['margin_pgu_per_call']) for pair in pairs):
        raise ValueError('derived floor misses an observed margin')
    return {'constant_pgu': constant, 'pairs': pairs,
            'unrounded_required_pgu': {'numerator': required.numerator, 'denominator': required.denominator}}


def derive(parent, evidence):
    """Authenticate immutable inputs and reject stale cached derivations."""
    if parent.get('artifact_sha256') != PARENT_SHA256 or content_address(parent) != PARENT_SHA256:
        raise ValueError('wrong immutable SHA256-floor parent')
    if digest(evidence) != EVIDENCE_SHA256 or evidence['parent_artifact_sha256'] != PARENT_SHA256:
        raise ValueError('wrong retained SHA256 evidence')
    derived = replay_measurements(evidence['measurements'])
    if (canonical_json(derived['pairs']) != canonical_json(evidence['matched_pairs']) or
        derived['constant_pgu'] != evidence['candidate_constant_pgu']):
        raise ValueError('cached SHA256 pairs or candidate differ from endpoint replay')
    return derived


def transform(parent, evidence):
    """Change only PC02's accepted constant and explicit successor provenance."""
    derived = derive(parent, evidence)
    result = copy.deepcopy(parent)
    route = next(r for r in result['block_local']['operation_routes'] if r['route_key'] == 'precompile:0x02')
    route['accepted_absolute_model']['constant'] = derived['constant_pgu'] * SCALE
    result['purpose'] = 'block_local_empirical_sha256_floor_candidate'
    result['parent_generator'], result['parent_validation'] = parent['generator'], parent['validation']
    result['generator'] = {'path': 'crates/evm/src/zk_gas/pricing/sha256_floor_successor.py',
        'file_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        'table_generator_path': 'crates/evm/src/zk_gas/pricing/generate.py',
        'table_generator_sha256': hashlib.sha256((BASE / 'generate.py').read_bytes()).hexdigest()}
    result['validation'] = {'retained_measurement_rows': 36, 'matched_increment_pairs': len(derived['pairs']),
        'new_sp1_executions': 0, 'unopened_holdouts_used': 0,
        'fresh_repricing_performed': False, 'historical_repricing_performed': False,
        'universal_upper_bound_claimed': False, 'production_sdk_parity_established': False}
    result['successor'] = {'parent_artifact_sha256': PARENT_SHA256, 'parent_snapshot': PARENT_PATH,
        'parent_file_sha256': PARENT_FILE_SHA256, 'evidence_path': EVIDENCE_PATH,
        'evidence_sha256': EVIDENCE_SHA256, 'evidence_file_sha256': EVIDENCE_FILE_SHA256,
        'policy': copy.deepcopy(evidence['policy']), 'constant_pgu': derived['constant_pgu'],
        'unrounded_required_pgu': derived['unrounded_required_pgu'],
        'source_bindings': copy.deepcopy(evidence['source_bindings']),
        'independent_evidence_verification': copy.deepcopy(evidence['independent_verification']),
        'scope': 'PC02 accepted constant only; all inherited numeric pricing, selectors, ownership, resources and Unzen unchanged',
        'empirical_limits': copy.deepcopy(evidence['limitations']),
        'publication_status_at_model_generation': 'candidate_pending_requotes'}
    result['artifact_sha256'] = content_address(result)
    return result


def validate_successor(payload, parent, evidence):
    """Reject readdressed inherited, numeric, boolean-type and provenance drift."""
    if canonical_json(payload) != canonical_json(transform(parent, evidence)):
        raise ValueError('SHA256-floor successor differs from deterministic endpoint replay')


def main():
    """Write or check the current runtime artifact and separate acceptance record."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--check', action='store_true')
    args = parser.parse_args()
    inputs = []
    for path, expected in ((PARENT_PATH, PARENT_FILE_SHA256), (EVIDENCE_PATH, EVIDENCE_FILE_SHA256)):
        raw = (BASE / path).read_bytes()
        if hashlib.sha256(raw).hexdigest() != expected:
            raise ValueError('immutable input file bytes changed: ' + path)
        inputs.append(json.loads(raw))
    result = transform(*inputs)
    output, acceptance = BASE / 'runtime-spec.json', BASE / 'accepted-runtime-spec.sha256'
    if args.check:
        if canonical_json(json.loads(output.read_bytes())) != canonical_json(result) or acceptance.read_text().strip() != result['artifact_sha256']:
            raise ValueError('SHA256-floor output or acceptance record is stale')
    else:
        output.write_bytes(canonical_json(result) + b'\n')
        acceptance.write_text(result['artifact_sha256'] + '\n')
    print(result['artifact_sha256'])


if __name__ == '__main__':
    main()
