#!/usr/bin/env python3
"""Derive an empirical raw-address pair reserve from immutable model c306."""
import argparse
import copy
import hashlib
import json
from pathlib import Path
import re

BASE = Path(__file__).resolve().parent
PARENT_SHA256 = 'c3065ba83157d1f375d3d34d49f1e9ddfcea5b2bdb7377aae7ad62a3fd28b69d'
PARENT_PATH = 'successors/parent-c3065ba8.json'
EVIDENCE_PATH = 'successors/access-list-address-pairs-v1.json'
EVIDENCE_SHA256 = '2c318a9763e701c8d04a211cd5e18d58d0dcb51c362ae07af2fe7aef0863abde'
PAIR_BOUND = 10**12
TARIFF = 128 * 10**18


def canonical_json(value):
    """Encode deterministic portable source bytes without implicit field deletion."""
    return json.dumps(value, sort_keys=True, separators=(',', ':')).encode()


def digest(value):
    """Hash a complete canonical source object."""
    return hashlib.sha256(canonical_json(value)).hexdigest()


def content_address(value):
    """Exclude only the self-referential artifact address."""
    return digest({k: v for k, v in value.items() if k != 'artifact_sha256'})


def integer(value, name):
    """Accept actual nonnegative U64 counters, including zero but excluding bool."""
    if type(value) is not int or not 0 <= value <= 2**64 - 1:
        raise ValueError('invalid ' + name)
    return value


def derive(parent, evidence):
    """Authenticate opened whole-input measurements and finalized transaction pairs."""
    if parent.get('artifact_sha256') != PARENT_SHA256 or content_address(parent) != PARENT_SHA256:
        raise ValueError('wrong immutable address-pair parent')
    if digest(evidence) != EVIDENCE_SHA256 or evidence['parent_model_id'] != PARENT_SHA256:
        raise ValueError('wrong sealed address-pair evidence')
    if (evidence['tariff_pgu_per_raw_address_pair'] != 128 or evidence['address_pair_bound'] != PAIR_BOUND
            or evidence['inherited_coefficients_refitted'] is not False
            or evidence['universal_upper_bound_claimed'] is not False
            or evidence['production_sdk_parity_established'] is not False
            or evidence['projection_is_fresh_quote'] is not False or evidence['new_actuals_requested'] != 0
            or evidence['fixture_tag'] != 'fixture-unzen-access-list-address-pairs-v1'
            or evidence['publication_status_at_model_generation'] != 'planned'):
        raise ValueError('unsupported pricing or accuracy claim')
    identity = evidence['measurement_identity']
    if (identity['elf_sha256'] != 'b76baf2e0f3fa3fe6f8a20fe53dbf82f58601ed14b370dfc0f8d31db2069e034'
            or identity['sp1_sdk_version'] != '6.3.0' or identity['production_sdk_parity_verified'] is not False):
        raise ValueError('wrong finite program or SDK identity')
    rows = {row['label']: row for row in evidence['samples']}
    if len(evidence['samples']) != 3 or set(rows) != {'normal', 'reference', 'aligned'}:
        raise ValueError('wrong finite opened population')
    for row in rows.values():
        for key in ('user_count', 'expected_address_pairs', 'actual_sp1_pgu', 'raw_gas', 'charged_block_gas',
                    'old_model_node_pgu', 'old_model_proposal_pgu', 'input_bincode_length'):
            integer(row[key], key)
        counts = [integer(n, 'raw address count') for n in row['raw_address_counts']]
        pairs = sum(n * (n - 1) // 2 for n in counts)
        if (row['old_model_id'] != PARENT_SHA256 or row['canonical_attempts'] != 1
                or row['user_count'] != len(counts) or row['user_count'] != 2
                or pairs != row['expected_address_pairs'] or not 0 <= pairs <= PAIR_BOUND
                or not 0 < row['charged_block_gas'] <= 30_000_000
                or row['raw_gas'] * 10 // 191 != row['actual_sp1_pgu']
                or row['whole_input_pressure_qualified'] is not None
                or row['data_role'] != 'opened_development_calibration'):
            raise ValueError('invalid canonical measurement or per-transaction pair sum')
        for key in ('input_sha256', 'raw_sha256', 'quote_sha256', 'native_sha256', 'independent_actual_report_sha256'):
            if not re.fullmatch('[0-9a-f]{64}', row[key]):
                raise ValueError('missing immutable evidence identity')
    last = rows['aligned']
    if (last['raw_address_counts'] != [6144, 6144] or last['actual_sp1_pgu'] != 3_009_978_952
            or last['actual_sp1_pgu'] - last['old_model_node_pgu'] != 2_754_519_621
            or last['expected_address_pairs'] != 37_742_592
            or last['first_observed_local_equal_hashes'] is not True
            or rows['reference']['first_observed_local_equal_hashes'] is not False
            or rows['normal']['expected_address_pairs'] != 12):
        raise ValueError('lost high-priority observation or local-profile boundary')
    return rows


def transform(parent, evidence):
    """Add one independently bounded block-local tariff, preserving inherited numeric tables."""
    rows = derive(parent, evidence)
    result = copy.deepcopy(parent)
    result['purpose'] = 'block_local_access_list_construction_pair_supplements'
    result['block_local']['access_list_address_pair_tariff'] = TARIFF
    result['arithmetic']['access_list_address_pair_bound'] = PAIR_BOUND
    result['arithmetic']['declared_worst_case_scaled_pgu'] += TARIFF * PAIR_BOUND
    if result['arithmetic']['declared_worst_case_scaled_pgu'] > 2**127 - 1:
        raise ValueError('mixed-domain i128 declaration exceeded')
    result['ownership']['block_local'].append('block.access_list_address_pairs')
    result['parent_generator'] = parent['generator']
    result['generator'] = dict(path='crates/evm/src/zk_gas/pricing/access_list_address_pairs_successor.py',
        file_sha256=hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        table_generator_path='crates/evm/src/zk_gas/pricing/generate.py',
        table_generator_sha256=hashlib.sha256((BASE / 'generate.py').read_bytes()).hexdigest())
    result['parent_validation'] = parent['validation']
    result['validation'] = dict(opened_whole_input_sp1_actual_rows=3, new_actual_measurements_requested=0,
        inherited_coefficients_refitted=False, new_tariff_empirical_conservative=True,
        fresh_repricing_performed=False, historical_repricing_performed=False,
        universal_upper_bound_claimed=False, production_sdk_parity_established=False,
        projected_high_priority_node_deficits=sum(row['actual_sp1_pgu'] - row['old_model_node_pgu']
            - row['expected_address_pairs'] * 128 > 2_000_000_000 for row in rows.values()))
    result['successor'] = dict(schema_version=1, parent_artifact_sha256=PARENT_SHA256, parent_snapshot=PARENT_PATH,
        construction_evidence_path=EVIDENCE_PATH, construction_evidence_sha256=digest(evidence),
        pgu_per_raw_address_pair=128, raw_address_pair_bound=PAIR_BOUND, selector_policy=evidence['selector_policy'],
        composition='add once to node/prove block-local pricing; proposal_once zero; no second discount',
        scope='raw outer-owner items only; existing per-owner storage-key384/96 prices unchanged',
        empirical_limit='Three opened finite whole-input actuals; local equal-H evidence does not certify whole-two-user pressure. Fresh/historical quotes are separate validation, not universal bounds or production-SDK certification.')
    result['artifact_sha256'] = content_address(result)
    return result


def validate_successor(payload, parent, evidence):
    """Require exact deterministic replay, including inherited numeric objects."""
    if payload != transform(parent, evidence):
        raise ValueError('successor differs from deterministic address-pair replay')


def main():
    """Generate or verify the one accepted model using authenticated immutable inputs."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--check', action='store_true')
    args = parser.parse_args()
    payload = transform(json.loads((BASE / PARENT_PATH).read_text()), json.loads((BASE / EVIDENCE_PATH).read_text()))
    output, acceptance = BASE / 'runtime-spec.json', BASE / 'accepted-runtime-spec.sha256'
    if args.check:
        if json.loads(output.read_text()) != payload or acceptance.read_text().strip() != payload['artifact_sha256']:
            raise ValueError('address-pair successor or acceptance record is stale')
    else:
        output.write_bytes(canonical_json(payload) + b'\n')
        acceptance.write_text(payload['artifact_sha256'] + '\n')
    print(('verified ' if args.check else 'generated ') + payload['artifact_sha256'])


if __name__ == '__main__':
    main()
