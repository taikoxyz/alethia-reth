#!/usr/bin/env python3
"""Replay finite ECMUL gates and remove only the native-gas incremental tariff."""
import argparse
import copy
import hashlib
from itertools import combinations
import json
from pathlib import Path
import re

BASE = Path(__file__).resolve().parent
PARENT_SHA256 = 'e8e574db50607857daadfd79f24e2cd6773177f6b46e4ee950f168379b4cab57'
PARENT_PATH = 'successors/parent-e8e574db.json'
PARENT_FILE_SHA256 = '9549a6a92291f46946447232cfdf86462567916a16a39003671d460a2888094f'
EVIDENCE_PATH = 'successors/ecmul-incremental-v1.json'
EVIDENCE_SHA256 = '5d8ee31fe1b9bcce0e855236faacb2de862dcc5b3324a98280bd2f8be7f1abd6'
EVIDENCE_FILE_SHA256 = '641ed9888b15c9ec497488f93198d55d07dbc5efc7b1ea6e739d3360830413b6'
ELF_SHA256 = 'b76baf2e0f3fa3fe6f8a20fe53dbf82f58601ed14b370dfc0f8d31db2069e034'
OBSERVER_SHA256 = 'dfaee7ac41099df02e8a66d9b15b6c4b53d539d1a7e051a67b7180c64d6a0992'
SCALE = 10**18
METADATA_FIELDS = ('artifact_sha256', 'purpose', 'generator', 'parent_generator',
                   'validation', 'parent_validation', 'successor')


def canonical_json(value):
    """Encode typed JSON deterministically, including all provenance fields."""
    return json.dumps(value, sort_keys=True, separators=(',', ':')).encode()


def digest(value):
    """Authenticate canonical JSON separately from its file encoding."""
    return hashlib.sha256(canonical_json(value)).hexdigest()


def content_address(value):
    """Exclude only the self-referential artifact address."""
    return digest({key: item for key, item in value.items() if key != 'artifact_sha256'})


def replay_measurements(parent, measurements):
    """Recompute max-after-sum prices and matched whole-input contrasts.

    This is an empirical removal gate, not a regression fit or universal upper bound.
    Keep A's native-gas slope for failures, and ceil only after replacing the scaled
    route contribution in each complete one-block quote.
    """
    if not isinstance(measurements, list) or len(measurements) != 33:
        raise ValueError('wrong retained ECMUL measurement population')
    route = next(r for r in parent['block_local']['operation_routes']
                 if r['route_key'] == 'precompile:0x07')
    accepted = route['accepted_absolute_model']
    if (accepted['constant'] != 20_000_000 * SCALE or
        accepted['coefficient'] != 131390224983920708487 or
        route['incremental_coefficient'] != 29901 * SCALE or
        route['structural_floor']['coefficient'] != 572936 * SCALE):
        raise ValueError('wrong inherited ECMUL tariff')
    counts, groups, seen_ids, seen_inputs, rows = {}, {}, set(), set(), []
    for row in measurements:
        for field in ('raw_gas', 'actual_pgu', 'repetitions', 'events', 'work_units',
                      'native_gas_per_call', 'block_gas'):
            if type(row[field]) is not int or not 0 <= row[field] < 2**64:
                raise ValueError('unsigned u64 measurement required')
        cohort, repetitions = row['cohort'], row['repetitions']
        if (cohort not in ('fit', 'mid', 'mix', 'failure') or repetitions not in (0, 1, 8)
            or not 0 < row['block_gas'] <= 30_000_000 or row['native_gas_per_call'] == 0):
            raise ValueError('unsupported retained execution domain')
        if row['sample_id'] in seen_ids or row['input_sha256'] in seen_inputs:
            raise ValueError('duplicate sample or canonical input')
        if row['actual_pgu'] != row['raw_gas'] * 10 // 191:
            raise ValueError('actual differs from raw-gas normalization')
        if (row['elf_sha256'] != ELF_SHA256 or row['observer_sha256'] != OBSERVER_SHA256
            or row['sdk'] != '6.3.0'):
            raise ValueError('mixed program, SDK or observer identity')
        for field in ('input_sha256', 'actual_report_sha256', 'native_report_sha256',
                      'quote_sha256', 'executor_opts_sha256'):
            if not isinstance(row[field], str) or re.fullmatch('[0-9a-f]{64}', row[field]) is None:
                raise ValueError('invalid retained artifact identity')
        if not isinstance(row['public_output'], str) or re.fullmatch('0x[0-9a-f]{64}', row['public_output']) is None:
            raise ValueError('invalid public output identity')
        for field in ('old_node_scaled', 'old_full_scaled'):
            if (not isinstance(row[field], str) or re.fullmatch('0|[1-9][0-9]*', row[field]) is None
                or not 0 <= int(row[field]) < 2**127):
                raise ValueError('invalid scaled quote')
        n, g, w = row['events'], row['native_gas_per_call'], row['work_units']
        if n != repetitions * (15 if cohort == 'mix' else 1):
            raise ValueError('wrong retained event ownership')
        if cohort == 'failure':
            valid = g in (6000, 200000) and w == 0
        else:
            valid = g == 6000 and w in {repetitions * v for v in
                {'fit': (0, 3, 507), 'mid': (254, 255), 'mix': (507,)}[cohort]}
        if not valid:
            raise ValueError('wrong retained work or native gas')
        a = n * max(accepted['constant'], accepted['coefficient'] * g)
        i, s = n * g * route['incremental_coefficient'], w * route['structural_floor']['coefficient']
        old_route, new_route = max(a, i, s), max(a, s)
        node = int(row['old_node_scaled']) - old_route + new_route
        full = int(row['old_full_scaled']) - old_route + new_route
        if node < new_route or full < node:
            raise ValueError('inconsistent whole-input quote')
        result = dict(sample_id=row['sample_id'], cohort=cohort, repetitions=repetitions,
                      native_gas_per_call=g, actual_pgu=row['actual_pgu'],
                      old_route_scaled=str(old_route), candidate_route_scaled=str(new_route),
                      candidate_node_pgu=-(-node // SCALE), candidate_full_pgu=-(-full // SCALE))
        rows.append(result)
        groups.setdefault(row['baseline_sample_id'], []).append((row, result))
        counts[cohort] = counts.get(cohort, 0) + 1
        seen_ids.add(row['sample_id'])
        seen_inputs.add(row['input_sha256'])
    if counts != {'fit': 18, 'mid': 6, 'mix': 3, 'failure': 6} or len(groups) != 11:
        raise ValueError('missing retained gate population')
    pairs = []
    for baseline_id, group in sorted(groups.items()):
        group.sort(key=lambda pair: pair[0]['repetitions'])
        if [r['repetitions'] for r, _ in group] != [0, 1, 8] or group[0][0]['sample_id'] != baseline_id:
            raise ValueError('missing or inconsistent own-zero control')
        first = group[0][0]
        if any((r['cohort'], r['native_gas_per_call']) !=
               (first['cohort'], first['native_gas_per_call']) for r, _ in group):
            raise ValueError('mixed contrast domain')
        if group[2][0]['work_units'] != 8 * group[1][0]['work_units']:
            raise ValueError('inconsistent contrast work')
        for (low, low_result), (high, high_result) in combinations(group, 2):
            actual = high['actual_pgu'] - low['actual_pgu']
            node = high_result['candidate_node_pgu'] - low_result['candidate_node_pgu']
            full = high_result['candidate_full_pgu'] - low_result['candidate_full_pgu']
            pairs.append(dict(low_sample_id=low['sample_id'], high_sample_id=high['sample_id'],
                              actual_increment_pgu=actual, node_increment_pgu=node, full_increment_pgu=full))
    return dict(rows=rows, matched_pairs=pairs,
                node_deficits=sum(r['candidate_node_pgu'] < r['actual_pgu'] for r in rows),
                full_deficits=sum(r['candidate_full_pgu'] < r['actual_pgu'] for r in rows),
                increment_deficits=sum(p['node_increment_pgu'] < p['actual_increment_pgu'] or
                                      p['full_increment_pgu'] < p['actual_increment_pgu'] for p in pairs))


def derive(parent, evidence):
    """Authenticate inputs and reject a policy unsupported by the finite gates."""
    if parent.get('artifact_sha256') != PARENT_SHA256 or content_address(parent) != PARENT_SHA256:
        raise ValueError('wrong immutable ECMUL parent')
    if digest(evidence) != EVIDENCE_SHA256 or evidence['parent_artifact_sha256'] != PARENT_SHA256:
        raise ValueError('wrong retained ECMUL evidence')
    result = replay_measurements(parent, evidence['measurements'])
    if (evidence['candidate_incremental_coefficient'] != 0 or
        evidence['reserved_actuals_opened_at_generation'] != 0 or
        evidence['production_sdk_parity_established'] is not False or
        result['full_deficits'] or result['increment_deficits']):
        raise ValueError('ECMUL removal gates did not pass')
    return result


def transform(parent, evidence):
    """Change only PC07 incremental coefficient and explicit successor metadata."""
    derived = derive(parent, evidence)
    result = copy.deepcopy(parent)
    route = next(r for r in result['block_local']['operation_routes'] if r['route_key'] == 'precompile:0x07')
    route['incremental_coefficient'] = 0
    result['purpose'] = 'block_local_empirical_ecmul_incremental_refinement'
    result['parent_generator'], result['parent_validation'] = parent['generator'], parent['validation']
    result['generator'] = {'path': 'crates/evm/src/zk_gas/pricing/ecmul_incremental_successor.py',
        'file_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        'table_generator_path': 'crates/evm/src/zk_gas/pricing/generate.py',
        'table_generator_sha256': hashlib.sha256((BASE / 'generate.py').read_bytes()).hexdigest()}
    result['validation'] = {'retained_measurement_rows': len(derived['rows']),
        'matched_increment_pairs': len(derived['matched_pairs']), 'full_deficits': 0,
        'matched_increment_deficits': 0, 'unopened_holdouts_used': 0,
        'universal_upper_bound_claimed': False, 'production_sdk_parity_established': False}
    result['successor'] = {'parent_artifact_sha256': PARENT_SHA256, 'parent_snapshot': PARENT_PATH,
        'parent_file_sha256': PARENT_FILE_SHA256, 'evidence_path': EVIDENCE_PATH,
        'evidence_sha256': EVIDENCE_SHA256, 'evidence_file_sha256': EVIDENCE_FILE_SHA256,
        'policy': evidence['policy'], 'source_bindings': copy.deepcopy(evidence['source_bindings']),
        'independent_evidence_verification': copy.deepcopy(evidence['independent_verification']),
        'scope': 'PC07 incremental coefficient only; complete accepted model, scalar structural slope, selectors, ownership, resources and Unzen unchanged',
        'empirical_limits': copy.deepcopy(evidence['limitations']),
        'publication_status_at_model_generation': 'candidate_pending_reserved_actuals_and_requotes'}
    result['artifact_sha256'] = content_address(result)
    return result


def validate_successor(payload, parent, evidence):
    """Reject readdressed price, boolean type and provenance drift."""
    if canonical_json(payload) != canonical_json(transform(parent, evidence)):
        raise ValueError('ECMUL successor differs from deterministic endpoint replay')


def main():
    """Write or check the current artifact and its separate acceptance record."""
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
            raise ValueError('ECMUL output or acceptance record is stale')
    else:
        output.write_bytes(canonical_json(result) + b'\n')
        acceptance.write_text(result['artifact_sha256'] + '\n')
    print(result['artifact_sha256'])


if __name__ == '__main__':
    main()
