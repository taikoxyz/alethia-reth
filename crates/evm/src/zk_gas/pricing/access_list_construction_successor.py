#!/usr/bin/env python3
"""Add an empirical raw-prefix access-list construction supplement to the sealed parent."""
import argparse
import copy
import hashlib
import json
from pathlib import Path
import re

BASE = Path(__file__).resolve().parent
PARENT_SHA256 = '923c8f916f6e8603677b61adb60d92d2cd4355e02030c8fafa6ec549d10525f8'
PARENT_PATH = 'successors/parent-923c8f91.json'
EVIDENCE_PATH = 'successors/access-list-construction-v1.json'
EVIDENCE_SHA256 = '05877d72c4a3afd634ed4afcb2a124657ae844ba21e97e6a3bc3777c186f9dda'
PAIR_BOUND = 10**12
TARIFF = 384 * 10**18


def canonical_json(value):
    """Return portable deterministic bytes for evidence and model identities."""
    return json.dumps(value, sort_keys=True, separators=(',', ':')).encode()


def digest(value):
    """Hash the canonical payload rather than an environment-dependent filename."""
    return hashlib.sha256(canonical_json(value)).hexdigest()


def content_address(value):
    """Exclude only the model's self-referential content-address field."""
    return digest({k: v for k, v in value.items() if k != 'artifact_sha256'})


def integer(value, name):
    """Require an exact nonnegative selector value, rejecting bool and oversized counts."""
    if type(value) is not int or not 0 <= value <= 2**64 - 1:
        raise ValueError('invalid ' + name)


def derive(parent, evidence):
    """Authenticate all retained rows and derive raw-prefix pairs without fitting a coefficient."""
    if parent.get('artifact_sha256') != PARENT_SHA256 or content_address(parent) != PARENT_SHA256:
        raise ValueError('wrong immutable access-list construction parent')
    if digest(evidence) != EVIDENCE_SHA256 or evidence['parent_model_id'] != PARENT_SHA256:
        raise ValueError('wrong sealed access-list construction evidence')
    if (evidence['tariff_pgu_per_pair'] != 384 or evidence['comparison_pair_bound'] != PAIR_BOUND
            or evidence['coefficient_refitted'] is not False
            or evidence['universal_upper_bound_claimed'] is not False
            or evidence['projection_is_fresh_quote'] is not False
            or evidence['production_sdk_parity_established'] is not False):
        raise ValueError('unsupported pricing or validation claim')
    rows = {row['case_id']: row for row in evidence['samples']}
    expected = {'empty', 'micro', 'reference', 'dispersed', 'own-k1024', 'aligned-k1024',
                'large-k7680', 'aligned-k7680'}
    if set(rows) != expected or len(evidence['samples']) != 8:
        raise ValueError('wrong finite construction population')
    measured = 0
    for row in rows.values():
        for name in ('raw_address_items', 'raw_storage_keys', 'comparison_pairs', 'block_gas',
                     'parent_node_pgu', 'parent_proposal_pgu'):
            integer(row[name], name)
        counts = row['prefix_bin_counts']
        if type(counts) is not list:
            raise ValueError('missing raw prefix counts')
        for count in counts:
            integer(count, 'raw_prefix_count')
            if count == 0:
                raise ValueError('empty bin must be omitted')
        pairs = sum(n * (n - 1) // 2 for n in counts)
        if (sum(counts) != row['raw_storage_keys'] or pairs != row['comparison_pairs']
                or pairs > PAIR_BOUND or not 0 < row['block_gas'] <= 30_000_000):
            raise ValueError('inconsistent finite raw-prefix projection')
        for name in ('input_sha256', 'parent_quote_sha256'):
            if re.fullmatch('[0-9a-f]{64}', row[name]) is None:
                raise ValueError('invalid saved source identity')
        if row['actual_pgu'] is None:
            if row['actual_status'] != 'unmeasured' or row['raw_actual_sha256'] is not None or row['pressure_realized'] is not None:
                raise ValueError('unmeasured row cannot claim actual qualification')
        else:
            integer(row['actual_pgu'], 'actual_pgu')
            measured += 1
            if (row['actual_status'] != ('measured_realized' if row['pressure_realized'] else 'measured_unrealized')
                    or type(row['pressure_realized']) is not bool
                    or re.fullmatch('[0-9a-f]{64}', row['raw_actual_sha256']) is None):
                raise ValueError('missing actual measurement identity')
    if measured != 5 or evidence['new_actual_count'] != 5:
        raise ValueError('wrong actual count')
    if evidence['unmeasured_actual_count'] != 3:
        raise ValueError('wrong unmeasured count')
    large = rows['aligned-k7680']
    if (large['actual_pgu'] != 5_773_513_175 or large['comparison_pairs'] != 29_487_360
            or large['actual_pgu'] - large['parent_node_pgu'] != 5_630_814_562
            or large['pressure_realized'] is not True
            or rows['large-k7680']['pressure_realized'] is not False):
        raise ValueError('lost motivating high-priority observation or unrealized control')
    return rows


def transform(parent, evidence):
    """Extend the model with exactly one named component and preserve every old price."""
    rows = derive(parent, evidence)
    result = copy.deepcopy(parent)
    result['purpose'] = 'block_local_access_list_construction_prefix_supplement'
    result['block_local']['access_list_construction_pair_tariff'] = TARIFF
    result['arithmetic']['access_list_construction_pair_bound'] = PAIR_BOUND
    result['ownership']['block_local'].append('block.access_list_construction')
    result['parent_generator'] = parent['generator']
    result['generator'] = {'path': 'crates/evm/src/zk_gas/pricing/access_list_construction_successor.py',
        'file_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        'table_generator_path': 'crates/evm/src/zk_gas/pricing/generate.py',
        'table_generator_sha256': hashlib.sha256((BASE / 'generate.py').read_bytes()).hexdigest()}
    result['parent_validation'] = parent['validation']
    result['validation'] = {'opened_sp1_actual_rows': 5, 'unmeasured_actual_rows': 3,
        'coefficient_refitted': False, 'actual_panel_used_to_select_semantic_extension': True,
        'new_native_repricing_performed': False, 'historical_repricing_performed': False,
        'projected_high_priority_node_deficits': sum(
            row['actual_pgu'] - row['parent_node_pgu'] - row['comparison_pairs'] * 384 > 2_000_000_000
            for row in rows.values() if row['actual_pgu'] is not None),
        'universal_upper_bound_claimed': False, 'production_sdk_parity_established': False}
    result['successor'] = {'schema_version': 1, 'parent_artifact_sha256': PARENT_SHA256,
        'parent_snapshot': PARENT_PATH, 'construction_evidence_path': EVIDENCE_PATH,
        'construction_evidence_sha256': digest(evidence), 'pgu_per_raw_prefix_pair': 384,
        'raw_prefix_pair_bound': PAIR_BOUND, 'selector_policy': evidence['selector_policy'],
        'composition': 'add once to block-local known/full pricing; proposal_once zero; no second discount',
        'scope': 'one new access-list construction supplement; every inherited numeric price and Unzen unchanged',
        'empirical_limit': evidence['limitation']}
    result['artifact_sha256'] = content_address(result)
    return result


def validate_successor(payload, parent, evidence):
    """Reject drift in evidence, semantics, prior prices or generated provenance."""
    if payload != transform(parent, evidence):
        raise ValueError('successor differs from deterministic construction replay')


def main():
    """Generate or check the accepted construction successor and its separate address record."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--check', action='store_true')
    args = parser.parse_args()
    payload = transform(json.loads((BASE / PARENT_PATH).read_text()), json.loads((BASE / EVIDENCE_PATH).read_text()))
    output, acceptance = BASE / 'runtime-spec.json', BASE / 'accepted-runtime-spec.sha256'
    if args.check:
        if json.loads(output.read_text()) != payload or acceptance.read_text().strip() != payload['artifact_sha256']:
            raise ValueError('construction successor or acceptance record is stale')
    else:
        output.write_bytes(canonical_json(payload) + b'\n')
        acceptance.write_text(payload['artifact_sha256'] + '\n')
    print(('verified ' if args.check else 'generated ') + payload['artifact_sha256'])


if __name__ == '__main__':
    main()
