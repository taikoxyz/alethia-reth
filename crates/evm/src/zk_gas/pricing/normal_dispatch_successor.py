#!/usr/bin/env python3
"""Derive an additive normal-dispatch reserve from sixty sealed composite observations."""
import argparse
import copy
from fractions import Fraction
import hashlib
import json
from pathlib import Path

BASE = Path(__file__).resolve().parent
PARENT_SHA256 = 'bbce9be0cd9792bfa9c46369203f6441c3fd288787ea357e311eae8ebd48aac4'
PARENT_PATH = 'successors/parent-bbce9be0.json'
EVIDENCE_PATH = 'successors/normal-dispatch-v1.json'
EVIDENCE_SHA256 = '840afbb5272c40a2d266fdf1fd41f9a2d923b8b8b297c0b445bbaaa5ac1ce2c6'
SCALE = 10**18


def canonical_json(value):
    """Encode portable canonical bytes for deterministic evidence replay."""
    return json.dumps(value, sort_keys=True, separators=(',', ':')).encode()


def digest(value):
    """Hash a complete canonical evidence or provenance object."""
    return hashlib.sha256(canonical_json(value)).hexdigest()


def content_address(value):
    """Exclude only the specification's self-referential artifact address."""
    return digest({k: v for k, v in value.items() if k != 'artifact_sha256'})


def derive(parent, evidence):
    """Recompute exact matched margins and round the largest nonnegative reserve to eight PGU."""
    if parent.get('artifact_sha256') != PARENT_SHA256 or content_address(parent) != PARENT_SHA256:
        raise ValueError('wrong immutable parent')
    if digest(evidence) != EVIDENCE_SHA256 or evidence['parent_artifact_sha256'] != PARENT_SHA256:
        raise ValueError('wrong sealed normal-dispatch evidence')
    policy = evidence['policy']
    if (policy['candidate_pgu'], policy['margin_numerator'], policy['margin_denominator'],
        policy['rounding_quantum_pgu'], policy['development_rows'], policy['matched_pairs'],
        policy['prospective_actuals_used_for_fit']) != (64, 5, 4, 8, 60, 60, 0):
        raise ValueError('wrong empirical development policy')
    rows = {r['sample_id']: r for r in evidence['measurements']}
    if len(rows) != 60 or len({r['input_sha256'] for r in rows.values()}) != 60:
        raise ValueError('wrong development population')
    if {c: sum(r['cohort'] == c for r in rows.values()) for c in ('arithmetic', 'dup-pop', 'stack')} != {'arithmetic': 24, 'dup-pop': 12, 'stack': 24}:
        raise ValueError('wrong cohort population')
    for row in rows.values():
        if row['completed_user_opcode_events'] != sum(row['completed_user_opcode_counts'].values()):
            raise ValueError('wrong completed opcode count')
    pairs = []; requirements = []
    for expected in evidence['matched_pairs']:
        low, high = rows[expected['low_sample_id']], rows[expected['high_sample_id']]
        if low['cohort'] != high['cohort'] or high['cohort'] != expected['cohort']:
            raise ValueError('cross-cohort matched pair')
        extra = high['completed_user_opcode_events'] - low['completed_user_opcode_events']
        if extra <= 0: raise ValueError('nonpositive matched dispatch difference')
        actual = high['actual_proposal_sp1_pgu'] - low['actual_proposal_sp1_pgu']
        old = int(high['node_scaled']) - int(low['node_scaled'])
        required = (Fraction(5, 4) * actual * SCALE - old) / (extra * SCALE)
        fraction = {'numerator': str(required.numerator), 'denominator': str(required.denominator)}
        # Original independent audit encodes this fraction with JSON integers.
        if (extra != expected['extra_completed_user_opcodes'] or actual != expected['actual_increment_pgu'] or
            old != int(expected['parent_node_increment_scaled']) or
            required != Fraction(int(expected['required_reserve_pgu_per_event']['numerator']), int(expected['required_reserve_pgu_per_event']['denominator']))):
            raise ValueError('matched policy constraint differs from original observations')
        pairs.append({'low_sample_id': low['sample_id'], 'high_sample_id': high['sample_id'],
                      'extra_completed_user_opcodes': extra, 'actual_increment_pgu': actual,
                      'parent_node_increment_scaled': str(old), 'required_reserve_pgu_per_event': fraction})
        requirements.append(required)
    if len(pairs) != 60 or len({(p['low_sample_id'], p['high_sample_id']) for p in pairs}) != 60:
        raise ValueError('wrong matched pair population')
    maximum = max(Fraction(0), *requirements)
    sealed = policy['maximum_required_pgu']
    if maximum != Fraction(int(sealed['numerator']), int(sealed['denominator'])):
        raise ValueError('wrong sealed maximum')
    coefficient = -(-maximum.numerator // (maximum.denominator * 8)) * 8
    if coefficient != 64: raise ValueError('evidence no longer derives 64 PGU')
    return {'coefficient_pgu': coefficient, 'maximum_required_pgu': maximum, 'pairs': pairs}


def transform(parent, evidence):
    """Preserve all inherited numeric terms and add only the explicit normalized reserve."""
    derived = derive(parent, evidence)
    result = copy.deepcopy(parent)
    result['block_local']['normal_dispatch_tariff'] = derived['coefficient_pgu'] * SCALE
    result['purpose'] = 'schema9_normal_dispatch_reserve'
    result['parent_generator'] = result['generator']
    result['generator'] = {'path': 'crates/evm/src/zk_gas/pricing/normal_dispatch_successor.py',
        'file_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        'table_generator_path': 'crates/evm/src/zk_gas/pricing/generate.py',
        'table_generator_sha256': hashlib.sha256((BASE / 'generate.py').read_bytes()).hexdigest()}
    result['parent_validation'] = result['validation']
    result['validation'] = {'inherited_authority_fields_reexecuted': False, 'new_sp1_measurement_rows': 60,
        'matched_increment_pairs': 60, 'prospective_actuals_used_for_fit': 0,
        'new_native_repricing_performed': False, 'historical_repricing_performed': False,
        'universal_upper_bound_claimed': False}
    maximum = derived['maximum_required_pgu']
    result['successor'] = {'schema_version': 1, 'parent_artifact_sha256': PARENT_SHA256,
        'parent_snapshot': PARENT_PATH, 'normal_dispatch_evidence_path': EVIDENCE_PATH,
        'normal_dispatch_evidence_sha256': digest(evidence), 'sources': copy.deepcopy(evidence['sources']),
        'normal_dispatch_pgu_per_event': derived['coefficient_pgu'],
        'exact_maximum_required_pgu': {'numerator': maximum.numerator, 'denominator': maximum.denominator},
        'eligible_events': 'normally_priced_Opcode_and_ConfirmedSpawn_only;exclude_Precompile_feature_error_selected_undispatched',
        'composition': 'add_once_after_route_max;operations_scaled_includes_normal_dispatch_scaled',
        'coefficient_policy': evidence['policy']['formula'],
        'empirical_limit': evidence['empirical_limit'], 'production_sdk_elf_parity_established': False,
        'scope': 'add_normal_dispatch_reserve_only;all_parent_coefficients_floors_discount_ownership_domains_and_Unzen_unchanged'}
    result['artifact_sha256'] = content_address(result)
    return result


def validate_successor(payload, parent, evidence):
    """Reject every numeric, identity, policy or provenance difference from canonical replay."""
    if payload != transform(parent, evidence): raise ValueError('successor differs from deterministic reserve replay')


def main():
    """Write or verify the authenticated successor and separate acceptance record."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--check', action='store_true'); args = parser.parse_args()
    payload = transform(json.loads((BASE / PARENT_PATH).read_text()), json.loads((BASE / EVIDENCE_PATH).read_text()))
    output, acceptance = BASE / 'runtime-spec.json', BASE / 'accepted-runtime-spec.sha256'
    if args.check:
        if json.loads(output.read_text()) != payload or acceptance.read_text().strip() != payload['artifact_sha256']:
            raise ValueError('successor output or acceptance record is stale')
    else:
        output.write_bytes(canonical_json(payload) + b'\n'); acceptance.write_text(payload['artifact_sha256'] + '\n')
    print(('verified ' if args.check else 'generated ') + payload['artifact_sha256'])


if __name__ == '__main__': main()
