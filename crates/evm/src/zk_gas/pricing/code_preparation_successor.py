#!/usr/bin/env python3
"""Derive code-byte and accepted-creation preparation prices from frozen observations."""
import argparse
import copy
from fractions import Fraction
import hashlib
import json
from pathlib import Path

BASE = Path(__file__).resolve().parent
PARENT_SHA256 = '3794f020f947fbaff4b26e8128996fcf9dcffb350b4360cc1d3d4114a809605e'
PARENT_PATH = 'successors/parent-3794f020.json'
EVIDENCE_PATH = 'successors/code-preparation-v1.json'
EVIDENCE_SHA256 = 'cf4a0278b663a11e0f5ce13b45494f1f4047ba8f2145f026e7400dd10f389aea'
SCALE = 10**18


def canonical_json(value):
    """Encode portable canonical JSON without formatting-dependent identities."""
    return json.dumps(value, sort_keys=True, separators=(',', ':')).encode()


def digest(value):
    """Hash one complete evidence object."""
    return hashlib.sha256(canonical_json(value)).hexdigest()


def content_address(value):
    """Exclude only the runtime specification's self-referential address."""
    return digest({k:v for k,v in value.items() if k != 'artifact_sha256'})


def validate_evidence(policy):
    """Authenticate the exact92 development observations, never prospective holdouts."""
    if digest(policy) != EVIDENCE_SHA256 or policy['parent_artifact_sha256'] != PARENT_SHA256:
        raise ValueError('wrong sealed code preparation evidence')
    rows = policy['measurements']
    if len(rows) != 92 or len({r['sample_id'] for r in rows}) != 92 or len({r['input_sha256'] for r in rows}) != 88 or policy['prospective_holdouts_used_for_fit'] != 0:
        raise ValueError('wrong evidence population or prospective scope')
    return {r['sample_id']:r for r in rows}


def rounded_power_two(value):
    """Ceil an exact positive rational requirement, then round upward to a power of two."""
    ceiling = max(0, -(-value.numerator // value.denominator))
    return 1 << (ceiling-1).bit_length() if ceiling else 0


def derive(policy):
    """Recompute byte loading, one-byte event and within-block repeated-code constraints."""
    rows = validate_evidence(policy)
    loading = [r for r in rows.values() if r['cohort'] == 'loading10']
    unique_requirement = Fraction(0)
    for low in loading:
        for high in loading:
            byte_delta = high['node_unique_code_bytes']-low['node_unique_code_bytes']
            actual = high['actual_pgu']-low['actual_pgu']
            if byte_delta > 0 and actual > 0:
                old = Fraction(int(high['parent_node_scaled'])-int(low['parent_node_scaled']), SCALE)
                unique_requirement = max(unique_requirement, (actual-old+2*byte_delta)/byte_delta)
    def create(size,count=1):
        return rows[f'top-create-followup-create-return_runtime-r{size}-n{count}-b1-g8000000']
    zero, one = create(0), create(1)
    event_requirement = Fraction(5,4)*(one['actual_pgu']-zero['actual_pgu'])-Fraction(int(one['parent_node_scaled'])-int(zero['parent_node_scaled']),SCALE)
    event_unit = rounded_power_two(event_requirement)
    repeated = []
    for count in (2,4):
        large, empty, large_one, empty_one = create(24576,count),create(0,count),create(24576),create(0)
        actual = large['actual_pgu']-empty['actual_pgu']-large_one['actual_pgu']+empty_one['actual_pgu']
        old = int(large['parent_node_scaled'])-int(empty['parent_node_scaled'])-int(large_one['parent_node_scaled'])+int(empty_one['parent_node_scaled'])
        unique_delta = large['node_unique_code_bytes']-empty['node_unique_code_bytes']-large_one['node_unique_code_bytes']+empty_one['node_unique_code_bytes']
        if unique_delta != 0: raise ValueError('repetition fit must cancel the unique byte term')
        repeated.append((Fraction(5,4)*actual-Fraction(old,SCALE)-event_unit*(count-1))/((count-1)*24576))
    return {'unique_requirement':unique_requirement,'event_requirement':event_requirement,
            'repeated_byte_requirements':repeated,'unique_unit':rounded_power_two(unique_requirement),
            'event_unit':event_unit,'byte_unit':rounded_power_two(max(repeated))}


def projected_underquotes(policy, successor):
    """Project complete quotes only; actual successor collection/replay remains separate."""
    validate_evidence(policy)
    count = 0
    for row in policy['measurements']:
        price = int(row['parent_complete_scaled'])
        if successor:
            selectors = row['derived_top_level_code_preparation']
            price += (62*row['full_unique_code_bytes']+8192*selectors['nonempty_events']+32*selectors['total_bytes'])*SCALE
        count += (price+SCALE-1)//SCALE < row['actual_pgu']
    return count


def transform(parent, policy):
    """Change only three declared normalized units while preserving the exact parent."""
    if parent.get('artifact_sha256') != PARENT_SHA256 or content_address(parent) != PARENT_SHA256:
        raise ValueError('wrong immutable parent runtime specification')
    derived = derive(policy)
    if [derived[k] for k in ('unique_unit','event_unit','byte_unit')] != [64,8192,32]:
        raise ValueError('sealed empirical policy no longer derives64/8192/32')
    result = copy.deepcopy(parent)
    result['block_local']['resource_tariffs']['unique_code_bytes'] = derived['unique_unit']*SCALE
    result['block_local']['code_preparation_tariffs'] = {'nonempty_events':derived['event_unit']*SCALE,'total_bytes':derived['byte_unit']*SCALE}
    result['ownership']['block_local'].append('block.code_preparation')
    result['purpose'] = 'schema9_code_loading_and_accepted_creation_preparation'
    result['parent_generator'] = result['generator']
    result['generator'] = {'path':'crates/evm/src/zk_gas/pricing/code_preparation_successor.py',
        'file_sha256':hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        'table_generator_path':'crates/evm/src/zk_gas/pricing/generate.py',
        'table_generator_sha256':hashlib.sha256((BASE/'generate.py').read_bytes()).hexdigest()}
    result['parent_validation'] = result['validation']
    result['validation'] = {'parent_validation_is_inherited_not_reexecuted':True,'new_sp1_measurement_rows':92,
        'distinct_canonical_inputs':88,'projected_full_underquotes_before':projected_underquotes(policy,False),
        'projected_full_underquotes_after':projected_underquotes(policy,True),'new_native_repricing_performed':False,
        'prospective_holdouts_used_for_fit':0,'multiblock_carried_code_used_for_fit':False,'universal_upper_bound_claimed':False}
    def rational(value): return {'numerator':value.numerator,'denominator':value.denominator}
    result['successor'] = {'schema_version':1,'parent_artifact_sha256':PARENT_SHA256,'parent_snapshot':PARENT_PATH,
        'code_preparation_policy_path':EVIDENCE_PATH,'code_preparation_policy_sha256':digest(policy),
        'normalized_pgu_per_unique_code_byte':64,'normalized_pgu_per_nonempty_preparation':8192,'normalized_pgu_per_prepared_byte':32,
        'unique_byte_requirement':rational(derived['unique_requirement']),'event_requirement':rational(derived['event_requirement']),
        'repeated_byte_requirements':[rational(v) for v in derived['repeated_byte_requirements']],
        'scope':'unique_code_bytes64;accepted_nonempty_create_preparation8192+32perbyte;all_other_terms_and_discount_unchanged',
        'quantitative_inspector_contract':'semantics-preserving inspector callbacks; arbitrary outcome-mutating inspectors excluded',
        'current_maximum_prepared_bytes_per_nonempty_event':24576,
        'measurement_policy':policy['scope']}
    result['artifact_sha256'] = content_address(result)
    return result


def validate_successor(payload,parent,policy):
    """Reject any coefficient, ownership, identity or evidence drift by exact replay."""
    if payload != transform(parent,policy): raise ValueError('successor differs from deterministic code preparation evidence replay')


def main():
    """Write or verify generated specification and its separate acceptance record."""
    parser=argparse.ArgumentParser(description=__doc__);parser.add_argument('--check',action='store_true');args=parser.parse_args()
    payload=transform(json.loads((BASE/PARENT_PATH).read_text()),json.loads((BASE/EVIDENCE_PATH).read_text()))
    output,acceptance=BASE/'runtime-spec.json',BASE/'accepted-runtime-spec.sha256'
    if args.check:
        if json.loads(output.read_text()) != payload or acceptance.read_text().strip() != payload['artifact_sha256']:
            raise ValueError('successor output or acceptance record is stale')
    else:
        output.write_bytes(canonical_json(payload)+b'\n');acceptance.write_text(payload['artifact_sha256']+'\n')
    print(('verified ' if args.check else 'generated ')+payload['artifact_sha256'])


if __name__=='__main__':main()
