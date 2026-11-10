#!/usr/bin/env python3
"""Replay sealed same-program marginal observations into an ordinary-tariff candidate."""
import argparse
import copy
from fractions import Fraction
import hashlib
from itertools import combinations
import json
from pathlib import Path

BASE = Path(__file__).resolve().parent
PARENT_SHA256 = 'eea6f1da87ee129d2d9f759ae6cef01cb8ac93988376a86efb290398e6db5da6'
PARENT_PATH = 'successors/parent-eea6f1da.json'
EVIDENCE_PATH = 'successors/ordinary-tariffs-v1.json'
EVIDENCE_SHA256 = '67cd3e3aa3c164c3e4462f5b21a3be1ba8ed25239a7f733e13bbea70470d206a'
ELF_SHA256 = 'b76baf2e0f3fa3fe6f8a20fe53dbf82f58601ed14b370dfc0f8d31db2069e034'
SCALE = 10**18
METADATA_FIELDS = ('artifact_sha256', 'purpose', 'generator', 'parent_generator',
                   'validation', 'parent_validation', 'successor')
FIXED_ROUTES = {'codesize': 'opcode:0x38', 'codecopy': 'opcode:0x39',
                'staticcall': 'opcode:0xfa:confirmed_spawn_wrapper'}
LOG_ROUTES = {'log3': 'opcode:0xa3', 'log4': 'opcode:0xa4'}


def canonical_json(value):
    """Encode portable canonical JSON for reproducible evidence and model identities."""
    return json.dumps(value, sort_keys=True, separators=(',', ':')).encode()


def digest(value):
    """Authenticate every field, including source identities and sealed policy."""
    return hashlib.sha256(canonical_json(value)).hexdigest()


def content_address(value):
    """Exclude only the self-referential model address."""
    return digest({key: item for key, item in value.items() if key != 'artifact_sha256'})


def round_margin(value):
    """Apply the exact 5/4 empirical margin, then round upwards to 64 whole PGU."""
    value = max(Fraction(0), value) * Fraction(5, 4)
    return -(-value.numerator // (64 * value.denominator)) * 64


def fraction_record(value):
    """Record an exact rational without floating-point conversion."""
    return {'numerator': value.numerator, 'denominator': value.denominator}


def replay_measurements(measurements):
    """Rebuild every within-operation/length/pattern pair; never subtract scaffold costs.

    STATICCALL's one warmup exists at every N and cancels in count differences. LOG4 has
    two users, hence 2N measured events. Signed data increments remain in word-envelope
    evaluation; fixed and zero-length base maxima are nonnegative.
    """
    groups = {}
    sample_ids = set()
    for row in measurements:
        if (type(row['raw_gas']) is not int or not 0 <= row['raw_gas'] < 2**64 or
            row['actual_pgu'] != row['raw_gas'] * 10 // 191):
            raise ValueError('endpoint PGU differs from unsigned raw-gas normalization')
        sample_id, operation = row['sample_id'], row['operation']
        if sample_id in sample_ids or operation not in FIXED_ROUTES.keys() | LOG_ROUTES.keys():
            raise ValueError('duplicate sample or unsupported operation')
        sample_ids.add(sample_id)
        if row['lane'] not in ('target', 'scaffold'):
            raise ValueError('unsupported observation lane')
        users = 2 if operation == 'log4' else 1
        if row['users'] != users or row['target_events'] != (users * row['repetitions'] if row['lane'] == 'target' else 0):
            raise ValueError('wrong event ownership or warmup count')
        if any(type(row[field]) is not int or row[field] < 0 for field in
               ('actual_pgu', 'repetitions', 'length', 'target_events')):
            raise ValueError('nonnegative integer measurement required')
        if row['lane'] == 'target':
            groups.setdefault((operation, row['length'], row.get('pattern', 'zero')), []).append(row)
    pairs = []
    for (operation, length, _), rows in groups.items():
        for low, high in combinations(sorted(rows, key=lambda row: row['target_events']), 2):
            events = high['target_events'] - low['target_events']
            if events <= 0:
                raise ValueError('duplicate within-group event count')
            actual = high['actual_pgu'] - low['actual_pgu']
            pairs.append({'operation': operation, 'length': length,
                'low_sample_id': low['sample_id'], 'high_sample_id': high['sample_id'],
                'extra_events': events, 'actual_increment_pgu': actual,
                'marginal_pgu': fraction_record(Fraction(actual, events))})
    marginal = lambda p: Fraction(p['actual_increment_pgu'], p['extra_events'])
    fixed = {operation: max(Fraction(0), *(marginal(p) for p in pairs if p['operation'] == operation))
             for operation in FIXED_ROUTES}
    bases = {operation: max(Fraction(0), *(marginal(p) for p in pairs
             if p['operation'] == operation and p['length'] == 0)) for operation in LOG_ROUTES}
    words = max(Fraction(0), *((marginal(p) - bases[p['operation']]) / ((p['length'] + 31) // 32)
                for p in pairs if p['operation'] in LOG_ROUTES and p['length'] > 0))
    candidate = {'fixed_pgu': {operation: round_margin(value) for operation, value in fixed.items()},
                 'log_base_pgu': {operation: round_margin(value) for operation, value in bases.items()},
                 'log_word_pgu': round_margin(words)}
    for pair in pairs:
        op = pair['operation']
        price = (candidate['fixed_pgu'][op] if op in FIXED_ROUTES else
                 candidate['log_base_pgu'][op] + candidate['log_word_pgu'] * ((pair['length'] + 31) // 32))
        if price * pair['extra_events'] < Fraction(5, 4) * pair['actual_increment_pgu']:
            raise ValueError('derived price misses a measured 25 percent margin')
    return {'candidate': candidate, 'pairs': pairs, 'unrounded_fixed': {k: fraction_record(v) for k, v in fixed.items()},
            'unrounded_log_bases': {k: fraction_record(v) for k, v in bases.items()},
            'unrounded_log_word': fraction_record(words)}


def derive(parent, evidence):
    """Authenticate the immutable parent and pre-reserved seal before independent replay."""
    if parent.get('artifact_sha256') != PARENT_SHA256 or content_address(parent) != PARENT_SHA256:
        raise ValueError('wrong immutable ordinary-tariff parent')
    if digest(evidence) != EVIDENCE_SHA256:
        raise ValueError('wrong sealed ordinary-tariff evidence')
    candidate, seal = evidence['candidate'], evidence['seal']
    qualification = evidence['qualification_summary']
    binding = qualification['program_binding']
    if (digest(qualification) != evidence['qualification_summary_sha256'] or
        binding['artifact_sha256']['elf'] != ELF_SHA256 or binding['sdk'] != '6.3.0' or
        binding['execution_profile'] != 'evm-gas-only' or
        qualification['observer_parity']['status'] != 'PASS' or
        qualification['source_hashes']['FIT-QUALIFICATION.json'] != candidate['fit_qualification_sha256'] or
        qualification['source_hashes']['PARITY.json'] != candidate['observer_parity_sha256']):
        raise ValueError('wrong same-program native or observer qualification')
    original_bytes = (json.dumps(candidate, sort_keys=True, indent=2) + '\n').encode()
    if (hashlib.sha256(original_bytes).hexdigest() != seal['candidate_sha256'] or
        candidate['parent_artifact_sha256'] != PARENT_SHA256 or
        candidate['reserved_actuals_used_for_fit'] != 0 or seal['reserved_actuals_present_at_seal'] is not False or
        seal['status'] != 'SEALED_BEFORE_RESERVED_ACTUALS'):
        raise ValueError('candidate is not the authenticated pre-reserved selection')
    rows = candidate['measurements']
    if len(rows) != 46 or len({r['input_sha256'] for r in rows}) != 46 or any(
        row['elf_sha256'] != ELF_SHA256 or row['sdk'] != '6.3.0' for row in rows):
        raise ValueError('wrong same-program measurement population')
    if sum(row['origin'] == 'new_fit' for row in rows) != 31 or sum(row['origin'] == 'retained_LOG4' for row in rows) != 15:
        raise ValueError('wrong fit/retained measurement split')
    derived = replay_measurements(rows)
    if (len(derived['pairs']) != 34 or derived['pairs'] != candidate['matched_pairs'] or
        derived['candidate'] != candidate['candidate'] or derived['candidate'] != seal['candidate']):
        raise ValueError('cached pairs or candidate differ from raw marginal replay')
    return derived


def transform(parent, evidence):
    """Replace only five accepted models, preserving inherited maxima, protections and domains."""
    derived = derive(parent, evidence)
    result = copy.deepcopy(parent)
    routes = {route['route_key']: route for route in result['block_local']['operation_routes']}
    for operation, key in FIXED_ROUTES.items():
        routes[key]['accepted_absolute_model']['coefficient'] = derived['candidate']['fixed_pgu'][operation] * SCALE
    for operation, key in LOG_ROUTES.items():
        old = routes[key]['accepted_absolute_model']
        word = derived['candidate']['log_word_pgu'] * SCALE
        routes[key]['accepted_absolute_model'] = {
            'authority_sha256': old['authority_sha256'], 'kind': 'opcode_model', 'model_kind': 'log_piecewise',
            'common_dispatch': 0, 'input_envelope': {'kind': 'log'},
            'parameters': {'constant': derived['candidate']['log_base_pgu'][operation] * SCALE,
                           'topic': 0, 'nonzero_data': word, 'additional_word': word},
            'shared_memory': old['shared_memory']}
    result['purpose'] = 'block_local_empirical_ordinary_tariff_candidate'
    result['parent_generator'], result['parent_validation'] = parent['generator'], parent['validation']
    result['generator'] = {'path': 'crates/evm/src/zk_gas/pricing/ordinary_tariff_successor.py',
        'file_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        'table_generator_path': 'crates/evm/src/zk_gas/pricing/generate.py',
        'table_generator_sha256': hashlib.sha256((BASE / 'generate.py').read_bytes()).hexdigest()}
    result['validation'] = {'new_fit_sp1_measurement_rows': 31, 'retained_log4_measurement_rows': 15,
        'matched_increment_pairs': len(derived['pairs']), 'reserved_actuals_used_for_fit': 0,
        'fresh_repricing_performed': False, 'historical_repricing_performed': False,
        'universal_upper_bound_claimed': False, 'production_sdk_parity_established': False}
    result['successor'] = {'parent_artifact_sha256': PARENT_SHA256, 'parent_snapshot': PARENT_PATH,
        'evidence_path': EVIDENCE_PATH, 'evidence_sha256': EVIDENCE_SHA256,
        'original_candidate_sha256': evidence['seal']['candidate_sha256'],
        'sources': copy.deepcopy(evidence['candidate']['sources']),
        'coefficient_policy': 'all within-operation/length/pattern target pairs; no scaffold subtraction; exact 5/4 margin; ceil to64 PGU',
        'candidate': derived['candidate'], 'unrounded_fixed': derived['unrounded_fixed'],
        'unrounded_log_bases': derived['unrounded_log_bases'], 'unrounded_log_word': derived['unrounded_log_word'],
        'scope': 'five accepted models only; inherited route maxima/floors/protections/ownership/domains/Unzen unchanged',
        'empirical_limits': copy.deepcopy(evidence['candidate']['diagnostic_limits']),
        'fixture_raw_inputs_and_executions_embedded': False,
        'publication_status_at_model_generation': 'candidate_pending_requotes'}
    result['artifact_sha256'] = content_address(result)
    return result


def validate_successor(payload, parent, evidence):
    """Reject every readdressed numeric, policy, provenance or inherited-field difference."""
    if payload != transform(parent, evidence):
        raise ValueError('ordinary-tariff successor differs from deterministic replay')


def main():
    """Write or check the generated runtime specification and separate acceptance record."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--check', action='store_true')
    args = parser.parse_args()
    result = transform(json.loads((BASE / PARENT_PATH).read_bytes()), json.loads((BASE / EVIDENCE_PATH).read_bytes()))
    output, acceptance = BASE / 'runtime-spec.json', BASE / 'accepted-runtime-spec.sha256'
    if args.check:
        if json.loads(output.read_bytes()) != result or acceptance.read_text().strip() != result['artifact_sha256']:
            raise ValueError('ordinary-tariff output or acceptance record is stale')
    else:
        output.write_bytes(canonical_json(result) + b'\n')
        acceptance.write_text(result['artifact_sha256'] + '\n')
    print(result['artifact_sha256'])


if __name__ == '__main__': main()
