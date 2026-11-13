#!/usr/bin/env python3
"""Reproduce the single ConfirmedSpawn(F4) price refinement from frozen density evidence."""
import argparse
import copy
from fractions import Fraction
import hashlib
import json
from pathlib import Path

BASE=Path(__file__).resolve().parent
PARENT_SHA256='a2988920125e304121acb629b9b96fd1ad85ee707c89978240408c81e2a42f3e'
PARENT_PATH='successors/parent-a2988920.json'
EVIDENCE_PATH='successors/delegatecall-wrapper-v1.json'
EVIDENCE_SHA256='6b053ed3ba587a7a6cef4a720d32a224304f2ac841c691e2b488f8fab7de6fc2'
ROUTE='opcode:0xf4:confirmed_spawn_wrapper'
SCALE=10**18


def canonical_json(value):
    """Encode portable sorted compact JSON for content-addressed replay."""
    return json.dumps(value,sort_keys=True,separators=(',',':')).encode()


def digest(value):
    """Hash the complete canonical evidence or provenance object."""
    return hashlib.sha256(canonical_json(value)).hexdigest()


def content_address(value):
    """Exclude only the runtime specification's self-referential address."""
    return digest({k:v for k,v in value.items() if k!='artifact_sha256'})


def validate_parent(parent):
    """Authenticate the complete immutable parent, including inherited authority fields."""
    if parent.get('artifact_sha256')!=PARENT_SHA256 or content_address(parent)!=PARENT_SHA256:
        raise ValueError('wrong immutable parent runtime specification')


def validate_evidence(evidence):
    """Bind exactly eight development observations and the policy frozen before holdouts."""
    if digest(evidence)!=EVIDENCE_SHA256 or evidence['parent_artifact_sha256']!=PARENT_SHA256 or evidence['route_key']!=ROUTE:
        raise ValueError('wrong sealed DELEGATECALL density evidence')
    policy=evidence['policy'];rows=evidence['measurements']
    if (policy['parent_model_id']!=PARENT_SHA256 or policy['development_rows']!=8 or policy['prospective_actuals_opened']!=0 or
        evidence['prospective_holdouts_used_for_fit']!=0 or policy['development_summary_sha256']!=evidence['sources']['development_summary_sha256'] or
        policy['independent_actual_audit_sha256']!=evidence['sources']['independent_actual_audit_sha256'] or
        policy['target_increment_margin']!={'numerator':5,'denominator':4} or policy['rounding_quantum_pgu']!=1000 or
        policy['frozen_confirmed_delegatecall_pgu']!=6000):
        raise ValueError('wrong frozen empirical policy or prospective scope')
    matrix={(r['child_terminal'],r['confirmed_delegatecalls']) for r in rows}
    if (len(rows)!=8 or len({r['sample_id'] for r in rows})!=8 or len({r['input_sha256'] for r in rows})!=8 or
        matrix!={(t,n) for t in ('stop','return0') for n in (0,64,1024,32768)}):
        raise ValueError('wrong density measurement population')
    return {r['sample_id']:r for r in rows}


def derive(parent,evidence):
    """Solve exact 5/4 matched node-increment constraints, then ceil to 1000 PGU/event."""
    validate_parent(parent);rows=validate_evidence(evidence)
    route=next(r for r in parent['block_local']['operation_routes'] if r['route_key']==ROUTE)
    model=route['accepted_absolute_model']
    if model['kind']!='fixed_with_optional_shared_memory' or model['coefficient']!=3841906721059316908619:
        raise ValueError('wrong parent confirmed wrapper model')
    old=Fraction(model['coefficient'],SCALE)
    matrix={(r['child_terminal'],r['confirmed_delegatecalls']):r for r in rows.values()};pairs=[]
    for terminal in ('stop','return0'):
        for low_count,high_count in ((0,64),(0,1024),(0,32768),(64,1024),(64,32768)):
            low,high=matrix[terminal,low_count],matrix[terminal,high_count]
            pairs.append({'low_sample_id':low['sample_id'],'high_sample_id':high['sample_id'],'extra_calls':high_count-low_count,
                'actual_increment_pgu':high['actual_pgu']-low['actual_pgu'],
                'parent_node_increment_scaled':str(int(high['parent_node_scaled'])-int(low['parent_node_scaled'])),
                'parent_complete_increment_scaled':str(int(high['parent_complete_scaled'])-int(low['parent_complete_scaled']))})
    key=lambda p:(p['low_sample_id'],p['high_sample_id'])
    if sorted(pairs,key=key)!=sorted(evidence['matched_pairs'],key=key):
        raise ValueError('matched pairs differ from canonical observation differences')
    requirements=[old+(Fraction(5,4)*p['actual_increment_pgu']-Fraction(int(p['parent_node_increment_scaled']),SCALE))/p['extra_calls'] for p in pairs]
    required=max(requirements);quantum=evidence['policy']['rounding_quantum_pgu']
    coefficient=-(-required.numerator//(required.denominator*quantum))*quantum
    if coefficient!=evidence['policy']['frozen_confirmed_delegatecall_pgu']:
        raise ValueError('sealed density observations no longer derive6000PGU')
    return {'coefficient_pgu':coefficient,'maximum_required_pgu':required,'pairs':sorted(pairs,key=key)}


def projected_underquotes(parent,evidence,successor):
    """Project full quotes by the exact per-confirmed-event delta; do not claim native replay."""
    derived=derive(parent,evidence);delta=derived['coefficient_pgu']*SCALE-3841906721059316908619
    return sum((int(r['parent_complete_scaled'])+(r['confirmed_delegatecalls']*delta if successor else 0)+SCALE-1)//SCALE<r['actual_pgu'] for r in evidence['measurements'])


def transform(parent,evidence):
    """Change only the F4 fixed coefficient and explicit successor/validation provenance."""
    derived=derive(parent,evidence);result=copy.deepcopy(parent)
    route=next(r for r in result['block_local']['operation_routes'] if r['route_key']==ROUTE)
    route['accepted_absolute_model']['coefficient']=derived['coefficient_pgu']*SCALE
    result['purpose']='schema9_confirmed_delegatecall_density_refinement'
    result['parent_generator']=result['generator']
    result['generator']={'path':'crates/evm/src/zk_gas/pricing/delegatecall_successor.py','file_sha256':hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        'table_generator_path':'crates/evm/src/zk_gas/pricing/generate.py','table_generator_sha256':hashlib.sha256((BASE/'generate.py').read_bytes()).hexdigest()}
    result['parent_validation']=result['validation']
    result['validation']={'parent_validation_is_inherited_not_reexecuted':True,'new_sp1_measurement_rows':8,'distinct_canonical_inputs':8,
        'matched_increment_pairs':10,'projected_full_underquotes_before':projected_underquotes(parent,evidence,False),
        'projected_full_underquotes_after':projected_underquotes(parent,evidence,True),'new_native_repricing_performed':False,
        'prospective_actuals_used_for_fit':0,'historical_repricing_performed':False,'universal_upper_bound_claimed':False}
    required=derived['maximum_required_pgu']
    result['successor']={'schema_version':1,'parent_artifact_sha256':PARENT_SHA256,'parent_snapshot':PARENT_PATH,
        'delegatecall_evidence_path':EVIDENCE_PATH,'delegatecall_evidence_sha256':digest(evidence),'policy_file_sha256':evidence['policy_file_sha256'],
        'confirmed_delegatecall_pgu_per_event':derived['coefficient_pgu'],'exact_maximum_required_pgu':{'numerator':required.numerator,'denominator':required.denominator},
        'coefficient_policy':'ceil_to_1000(max(old_wrapper+(5/4*actual_delta-parent_node_delta)/extra_confirmed_calls))',
        'measurement_identity':copy.deepcopy(evidence['measurement_identity']),
        'inherited_authority_fields_reexecuted':False,'new_coefficient_authority':'sealed density observations and pre-holdout policy in delegatecall_evidence_path',
        'production_sdk_elf_parity_established':False,
        'scope':'ConfirmedSpawn(F4)fixed_coefficient_only;memory_other_coefficients_floors_global_discount_ownership_and_Unzen_unchanged',
        'empirical_limit':'Finite composite loop/child observations, not isolated-frame cost or a universal bound. Successor native replay, PUSH0 holdouts and historical impact are separate validation.'}
    result['artifact_sha256']=content_address(result)
    return result


def validate_successor(payload,parent,evidence):
    """Reject unrelated coefficient, authority, identity, evidence or generator drift."""
    if payload!=transform(parent,evidence):raise ValueError('successor differs from deterministic DELEGATECALL evidence replay')


def main():
    """Write or check the canonical export and its independent acceptance record."""
    parser=argparse.ArgumentParser(description=__doc__);parser.add_argument('--check',action='store_true');args=parser.parse_args()
    payload=transform(json.loads((BASE/PARENT_PATH).read_text()),json.loads((BASE/EVIDENCE_PATH).read_text()))
    output,acceptance=BASE/'runtime-spec.json',BASE/'accepted-runtime-spec.sha256'
    if args.check:
        if json.loads(output.read_text())!=payload or acceptance.read_text().strip()!=payload['artifact_sha256']:
            raise ValueError('successor output or acceptance record is stale')
    else:output.write_bytes(canonical_json(payload)+b'\n');acceptance.write_text(payload['artifact_sha256']+'\n')
    print(('verified ' if args.check else 'generated ')+payload['artifact_sha256'])


if __name__=='__main__':main()
