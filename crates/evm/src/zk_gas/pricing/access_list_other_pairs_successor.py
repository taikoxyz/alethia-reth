#!/usr/bin/env python3
"""Derive one explicit different-low32 raw-pair tariff from immutable model389."""
import argparse
import copy
import hashlib
import json
from pathlib import Path
import re

BASE = Path(__file__).resolve().parent
PARENT_SHA256 = '389ca65f21fb17b04121e1b0915662f85e4b617b4c12b3b2c949faf591eb5493'
PARENT_PATH = 'successors/parent-389ca65f.json'
EVIDENCE_PATH = 'successors/access-list-other-pairs-v1.json'
EVIDENCE_SHA256 = 'b8fcbf6453819c20fb860b5f2036ac951fb258f9d56b0d2ce064bcd03eb98906'
PAIR_BOUND = 10**12
TARIFF = 96 * 10**18


def canonical_json(value):
    """Return deterministic portable bytes, excluding no data implicitly."""
    return json.dumps(value, sort_keys=True, separators=(',', ':')).encode()


def digest(value):
    """Return the canonical payload digest used by sealed source evidence."""
    return hashlib.sha256(canonical_json(value)).hexdigest()


def content_address(value):
    """Exclude only the content-address field that would refer to itself."""
    return digest({k: v for k, v in value.items() if k != 'artifact_sha256'})


def integer(value, name):
    """Accept only actual bounded nonnegative integer counters, never bool."""
    if type(value) is not int or not 0 <= value <= 2**64 - 1:
        raise ValueError('invalid ' + name)
    return value


def derive(parent, evidence):
    """Authenticate finite whole-input observations and recompute actual pair counts."""
    if parent.get('artifact_sha256') != PARENT_SHA256 or content_address(parent) != PARENT_SHA256:
        raise ValueError('wrong immutable different-prefix parent')
    if digest(evidence) != EVIDENCE_SHA256 or evidence['parent_model_id'] != PARENT_SHA256:
        raise ValueError('wrong sealed different-prefix evidence')
    if (evidence['tariff_pgu_per_other_pair'] != 96 or evidence['low32_tariff_pgu_unchanged'] != 384
            or evidence['other_pair_bound'] != PAIR_BOUND
            or evidence['inherited_coefficients_refitted'] is not False
            or evidence['universal_upper_bound_claimed'] is not False
            or evidence['production_sdk_parity_established'] is not False
            or evidence['projection_is_fresh_quote'] is not False
            or evidence['new_actuals_requested'] != 0):
        raise ValueError('unsupported pricing or accuracy claim')
    if (evidence['measurement_identity']['elf_sha256'] != 'b76baf2e0f3fa3fe6f8a20fe53dbf82f58601ed14b370dfc0f8d31db2069e034'
            or evidence['measurement_identity']['sp1_sdk_version'] != '6.3.0'
            or evidence['measurement_identity']['production_sdk_parity_verified'] is not False):
        raise ValueError('wrong finite measurement program or SDK claim')
    if (evidence['fixture_tag'] != 'fixture-unzen-access-list-short-pairs-v1'
            or evidence['publication_status_at_model_generation'] != 'planned'):
        raise ValueError('incorrect publication event identity')
    rows = {row['label']: row for row in evidence['samples']}
    if len(evidence['samples']) != 4 or set(rows) != {'short-k64', 'short-k7680', 'packed-reference', 'packed-aligned'}:
        raise ValueError('wrong finite population')
    for row in rows.values():
        for key in ('user_count', 'raw_storage_keys_each', 'actual_sp1_pgu', 'raw_gas', 'charged_block_gas',
                    'old_model_node_pgu', 'old_model_proposal_pgu', 'expected_old_prefix_pairs', 'expected_new_other_pairs'):
            integer(row[key], key)
        if (row['old_model_id'] != PARENT_SHA256 or row['canonical_attempts'] != 1
                or not 0 < row['charged_block_gas'] <= 30_000_000
                or row['raw_gas'] * 10 // 191 != row['actual_sp1_pgu']
                or len(row['scopes']) != row['user_count']):
            raise ValueError('invalid canonical whole-input identity or normalization')
        owner_pairs = low_pairs = 0
        for scope in row['scopes']:
            n = integer(scope['raw_keys'], 'scope raw keys')
            total = 0
            for size, groups in scope['low32_bin_size_histogram'].items():
                if not re.fullmatch('[1-9][0-9]*', size):
                    raise ValueError('noncanonical prefix histogram')
                size = int(size)
                integer(size, 'bin size'); integer(groups, 'bin multiplicity')
                if groups == 0:
                    raise ValueError('zero histogram multiplicity')
                total += size * groups
                low_pairs += groups * size * (size - 1) // 2
            if n != total or n != row['raw_storage_keys_each']:
                raise ValueError('raw histogram count mismatch')
            owner_pairs += n * (n - 1) // 2
        other_pairs = owner_pairs - low_pairs
        if (not 0 <= low_pairs <= PAIR_BOUND or not 0 <= other_pairs <= PAIR_BOUND
                or low_pairs != row['expected_old_prefix_pairs']
                or other_pairs != row['expected_new_other_pairs']):
            raise ValueError('inconsistent raw pair projection')
        for name in ('input_sha256', 'quote_sha256', 'native_sha256', 'raw_sha256'):
            if not re.fullmatch('[0-9a-f]{64}', row[name]):
                raise ValueError('missing retained raw identity')
    last = rows['packed-aligned']
    if (last['actual_sp1_pgu'] != 2_484_257_836
            or last['actual_sp1_pgu'] - last['old_model_node_pgu'] != 2_276_457_001
            or last['expected_new_other_pairs'] != 58_974_720
            or last['whole_input_pressure_qualified'] is not None
            or last['first_observed_local_pressure'] is not True):
        raise ValueError('lost high-priority observation or finite profile limits')
    return rows


def transform(parent, evidence):
    """Add only a separate normalized tariff/domain and conservative arithmetic allowance."""
    rows = derive(parent, evidence)
    result = copy.deepcopy(parent)
    result['purpose'] = 'block_local_access_list_construction_pair_supplements'
    result['block_local']['access_list_construction_other_pair_tariff'] = TARIFF
    result['arithmetic']['access_list_construction_other_pair_bound'] = PAIR_BOUND
    result['arithmetic']['declared_worst_case_scaled_pgu'] += TARIFF * PAIR_BOUND
    if result['arithmetic']['declared_worst_case_scaled_pgu'] > 2**127 - 1:
        raise ValueError('mixed-domain i128 declaration exceeded')
    result['ownership']['block_local'].append('block.access_list_construction_other_pairs')
    result['parent_generator'] = parent['generator']
    result['generator'] = {'path': 'crates/evm/src/zk_gas/pricing/access_list_other_pairs_successor.py',
        'file_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        'table_generator_path': 'crates/evm/src/zk_gas/pricing/generate.py',
        'table_generator_sha256': hashlib.sha256((BASE / 'generate.py').read_bytes()).hexdigest()}
    result['parent_validation'] = parent['validation']
    result['validation'] = {'opened_whole_input_sp1_actual_rows': 4,
        'new_actual_measurements_requested': 0, 'inherited_coefficients_refitted': False,
        'new_tariff_empirical_conservative': True, 'fresh_repricing_performed': False,
        'historical_repricing_performed': False, 'universal_upper_bound_claimed': False,
        'production_sdk_parity_established': False,
        'projected_high_priority_node_deficits': sum(
            row['actual_sp1_pgu'] - row['old_model_node_pgu'] - row['expected_new_other_pairs'] * 96 > 2_000_000_000
            for row in rows.values())}
    result['successor'] = {'schema_version': 1, 'parent_artifact_sha256': PARENT_SHA256,
        'parent_snapshot': PARENT_PATH, 'construction_evidence_path': EVIDENCE_PATH,
        'construction_evidence_sha256': digest(evidence), 'pgu_per_raw_other_pair': 96,
        'raw_other_pair_bound': PAIR_BOUND, 'selector_policy': evidence['selector_policy'],
        'composition': 'add once to node/prove block-local pricing; proposal_once zero; no second discount',
        'scope': 'different-low32 raw occurrence supplement; all inherited prices including384 low32 unchanged',
        'empirical_limit': 'Four finite whole-input actuals; first-map local attribution is not whole-two-transaction pressure. Fresh quotes/historical sensitivity remain separate validation, not universal bounds or production certification.'}
    result['artifact_sha256'] = content_address(result)
    return result


def validate_successor(payload, parent, evidence):
    """Require exact deterministic replay, including unchanged inherited numeric objects."""
    if payload != transform(parent, evidence):
        raise ValueError('successor differs from deterministic different-prefix replay')


def main():
    """Generate or check the new accepted model through its authoritative source inputs."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--check', action='store_true')
    args = parser.parse_args()
    payload = transform(json.loads((BASE / PARENT_PATH).read_text()), json.loads((BASE / EVIDENCE_PATH).read_text()))
    output, acceptance = BASE / 'runtime-spec.json', BASE / 'accepted-runtime-spec.sha256'
    if args.check:
        if json.loads(output.read_text()) != payload or acceptance.read_text().strip() != payload['artifact_sha256']:
            raise ValueError('different-prefix successor or acceptance record is stale')
    else:
        output.write_bytes(canonical_json(payload) + b'\n')
        acceptance.write_text(payload['artifact_sha256'] + '\n')
    print(('verified ' if args.check else 'generated ') + payload['artifact_sha256'])


if __name__ == '__main__':
    main()
