#!/usr/bin/env python3
"""Reproduce the single BLOCKHASH fixed-price correction from sealed finite evidence."""
import argparse
import copy
from fractions import Fraction
import hashlib
import json
from pathlib import Path

BASE=Path(__file__).resolve().parent
PARENT_SHA256='2fb4bb71092f11cca2217e69b25e94217342ffb46a540a961e7a77a483cb5601'
PARENT_PATH='successors/parent-2fb4bb71.json'
EVIDENCE_PATH='successors/blockhash-density-v1.json'
EVIDENCE_SHA256='5c7240d06411d4e50fe2892ccdfd9736628479d836070463538a59726ebca1be'
ROUTE='opcode:0x40'
OLD_COEFFICIENT=32117684584888432025
SCALE=10**18


def canonical_json(value):
    """Encode portable sorted compact JSON for content-addressed replay."""
    return json.dumps(value,sort_keys=True,separators=(',',':')).encode()


def digest(value):
    """Hash a complete canonical evidence or provenance object."""
    return hashlib.sha256(canonical_json(value)).hexdigest()


def content_address(value):
    """Exclude only the runtime specification's self-referential address."""
    return digest({k:v for k,v in value.items() if k!='artifact_sha256'})


def validate_parent(parent):
    """Authenticate the immutable parent including every inherited authority field."""
    if parent.get('artifact_sha256')!=PARENT_SHA256 or content_address(parent)!=PARENT_SHA256:
        raise ValueError('wrong immutable parent runtime specification')


def validate_evidence(evidence):
    """Bind exactly twelve development observations and the pre-holdout frozen policy."""
    if digest(evidence)!=EVIDENCE_SHA256 or evidence['parent_artifact_sha256']!=PARENT_SHA256 or evidence['route_key']!=ROUTE:
        raise ValueError('wrong sealed BLOCKHASH density evidence')
    policy=evidence['policy'];rows=evidence['measurements']
    if (policy['parent_model_id']!=PARENT_SHA256 or policy['development_rows']!=12 or policy['matched_increments']!=15 or
        policy['fresh_validation_design']['new_actuals_opened']!=0 or evidence['prospective_holdouts_used_for_fit']!=0 or
        policy['development_summary_sha256']!=evidence['sources']['development_summary_sha256'] or
        policy['increment_margin_numerator']!=5 or policy['increment_margin_denominator']!=4 or policy['rounding_quantum_pgu']!=100 or
        policy['candidate_price_pgu']!=700 or int(policy['candidate_price_scaled'])!=700*SCALE or int(policy['old_price_scaled'])!=OLD_COEFFICIENT):
        raise ValueError('wrong frozen empirical policy or prospective scope')
    matrix={(r['mode'],r['bulk_queries']) for r in rows}
    if (len(rows)!=12 or len({r['sample_id'] for r in rows})!=12 or len({r['input_sha256'] for r in rows})!=12 or
        matrix!={(m,n) for m in ('nearest','farthest','expired') for n in (0,64,4096,262144)} or
        any(r['total_blockhash_queries']!=r['bulk_queries']+1 for r in rows)):
        raise ValueError('wrong density measurement population or warmup count')
    return {r['sample_id']:r for r in rows}


def derive(parent,evidence):
    """Solve exact 5/4 matched node-increment constraints, then ceil to100PGU/event."""
    validate_parent(parent);rows=validate_evidence(evidence)
    route=next(r for r in parent['block_local']['operation_routes'] if r['route_key']==ROUTE)
    if route['accepted_absolute_model']!={'authority_sha256':'5fbdf5d26918e57214d70997795b2b41fad3219b8821730a08b0430f890c1d0c',
        'coefficient':OLD_COEFFICIENT,'kind':'fixed','raw_gas':20}:
        raise ValueError('wrong parent fixed BLOCKHASH model')
    matrix={(r['mode'],r['bulk_queries']):r for r in rows.values()};pairs=[];candidates=[];requirements=[]
    for mode in ('expired','farthest','nearest'):
        for low_count,high_count in ((0,64),(0,4096),(0,262144),(64,4096),(64,262144)):
            low,high=matrix[mode,low_count],matrix[mode,high_count];extra=high_count-low_count
            pair={'low_sample_id':low['sample_id'],'high_sample_id':high['sample_id'],'extra_blockhash_queries':extra,
                'actual_increment_pgu':high['actual_pgu']-low['actual_pgu'],
                'parent_node_increment_scaled':str(int(high['parent_node_scaled'])-int(low['parent_node_scaled'])),
                'parent_complete_increment_scaled':str(int(high['parent_complete_scaled'])-int(low['parent_complete_scaled']))}
            required=OLD_COEFFICIENT+(Fraction(5,4)*pair['actual_increment_pgu']*SCALE-int(pair['parent_node_increment_scaled']))/extra
            requirements.append(required/SCALE);pairs.append(pair)
            candidates.append({'low_sample_id':low['sample_id'],'high_sample_id':high['sample_id'],'extra_blockhash_queries':extra,
                'actual_increment_pgu':pair['actual_increment_pgu'],'old_node_increment_scaled':pair['parent_node_increment_scaled'],
                'required_blockhash_price_scaled_numerator':str(required.numerator),'required_blockhash_price_scaled_denominator':str(required.denominator)})
    key=lambda p:(p['low_sample_id'],p['high_sample_id'])
    if sorted(pairs,key=key)!=sorted(evidence['matched_pairs'],key=key) or sorted(candidates,key=key)!=sorted(evidence['policy']['development_candidates'],key=key):
        raise ValueError('matched policy constraints differ from canonical observation differences')
    required=max(requirements);sealed=evidence['policy']['maximum_required_price_scaled']
    if required*SCALE!=Fraction(int(sealed['numerator']),int(sealed['denominator'])):
        raise ValueError('sealed maximum threshold differs from exact policy replay')
    quantum=evidence['policy']['rounding_quantum_pgu'];coefficient=-(-required.numerator//(required.denominator*quantum))*quantum
    if coefficient!=evidence['policy']['candidate_price_pgu']:raise ValueError('sealed observations no longer derive700PGU')
    return {'coefficient_pgu':coefficient,'maximum_required_pgu':required,'pairs':sorted(pairs,key=key)}


def projected_underquotes(parent,evidence,successor):
    """Project complete quotes by exact per-event uplift, without claiming native replay."""
    delta=derive(parent,evidence)['coefficient_pgu']*SCALE-OLD_COEFFICIENT
    return sum((int(r['parent_complete_scaled'])+(r['total_blockhash_queries']*delta if successor else 0)+SCALE-1)//SCALE<r['actual_pgu'] for r in evidence['measurements'])


def transform(parent,evidence):
    """Change only BLOCKHASH's fixed coefficient and explicit successor provenance."""
    derived=derive(parent,evidence);result=copy.deepcopy(parent)
    next(r for r in result['block_local']['operation_routes'] if r['route_key']==ROUTE)['accepted_absolute_model']['coefficient']=derived['coefficient_pgu']*SCALE
    result['purpose']='schema9_blockhash_density_refinement';result['parent_generator']=result['generator']
    result['generator']={'path':'crates/evm/src/zk_gas/pricing/blockhash_successor.py','file_sha256':hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        'table_generator_path':'crates/evm/src/zk_gas/pricing/generate.py','table_generator_sha256':hashlib.sha256((BASE/'generate.py').read_bytes()).hexdigest()}
    result['parent_validation']=result['validation']
    result['validation']={'parent_validation_is_inherited_not_reexecuted':True,'new_sp1_measurement_rows':12,'distinct_canonical_inputs':12,'matched_increment_pairs':15,
        'projected_full_underquotes_before':projected_underquotes(parent,evidence,False),'projected_full_underquotes_after':projected_underquotes(parent,evidence,True),
        'new_native_repricing_performed':False,'prospective_actuals_used_for_fit':0,'historical_repricing_performed':False,'universal_upper_bound_claimed':False}
    required=derived['maximum_required_pgu']
    result['successor']={'schema_version':1,'parent_artifact_sha256':PARENT_SHA256,'parent_snapshot':PARENT_PATH,'blockhash_evidence_path':EVIDENCE_PATH,
        'blockhash_evidence_sha256':digest(evidence),'policy_file_sha256':evidence['policy_file_sha256'],'blockhash_pgu_per_event':derived['coefficient_pgu'],
        'exact_maximum_required_pgu':{'numerator':required.numerator,'denominator':required.denominator},
        'coefficient_policy':'ceil_to_100(max(old_BLOCKHASH+(5/4*actual_delta-parent_node_delta)/extra_BLOCKHASH))',
        'measurement_identity':copy.deepcopy(evidence['measurement_identity']),'inherited_authority_fields_reexecuted':False,
        'new_coefficient_authority':'sealed twelve-row density observations and pre-holdout policy in blockhash_evidence_path','production_sdk_elf_parity_established':False,
        'scope':'normal_BLOCKHASH_fixed_coefficient_only;other_coefficients_memory_floors_global_discount_ownership_limits_and_Unzen_unchanged',
        'empirical_limit':'Finite same-mode composite loop observations, not isolated lookup cost or a universal bound. Native successor replay and fresh validation are separate.'}
    result['artifact_sha256']=content_address(result);return result


def validate_successor(payload,parent,evidence):
    """Reject unrelated coefficient, evidence, identity, authority or generator drift."""
    if payload!=transform(parent,evidence):raise ValueError('successor differs from deterministic BLOCKHASH evidence replay')


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
