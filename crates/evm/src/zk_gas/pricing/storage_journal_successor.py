#!/usr/bin/env python3
"""Derive a conservative additive journal reserve from sealed long/short-prefix observations."""
import argparse
import copy
from fractions import Fraction
import hashlib
import json
from pathlib import Path
import re

BASE = Path(__file__).resolve().parent
PARENT_SHA256 = 'c8936124c1b074b57abf36873e1c37e01f9bc49fa6c9bb161a92542f1e6551ef'
PARENT_PATH = 'successors/parent-c8936124.json'
EVIDENCE_PATH = 'successors/storage-journal-comparison-v1.json'
EVIDENCE_SHA256 = '7c7a33af8e9de9b2dc3a46da1eef5569548b195297a3152270c424e13471e520'
SCALE = 10**18
WORK_UNIT_BOUND = 10**12
POLICY = 'ceil_to_64(5/4 * max positive((whole_input_actual_pgu-old_literal_node_pgu)/(C*(A+N+J))))'


def canonical_json(value):
    """Encode deterministic portable JSON bytes."""
    return json.dumps(value,sort_keys=True,separators=(',',':')).encode()


def digest(value):
    """Hash the complete evidence payload or provenance object."""
    return hashlib.sha256(canonical_json(value)).hexdigest()


def content_address(value):
    """Exclude only the runtime artifact's self-referential address."""
    return digest({k:v for k,v in value.items() if k!='artifact_sha256'})


def integer(value, minimum, maximum, name):
    """Reject booleans and out-of-domain coefficient/count values."""
    if type(value) is not int or not minimum<=value<=maximum:
        raise ValueError('invalid '+name)


def derive(parent,evidence):
    """Recompute exact residual/unit requirements and round their 5/4 maximum to 64."""
    if parent.get('artifact_sha256')!=PARENT_SHA256 or content_address(parent)!=PARENT_SHA256:
        raise ValueError('wrong immutable journal-tariff parent')
    if digest(evidence)!=EVIDENCE_SHA256 or evidence['parent_model_id']!=PARENT_SHA256:
        raise ValueError('wrong sealed journal comparison evidence')
    if evidence['coefficient_policy']!=POLICY or evidence['fixed_point_scale']!=SCALE:
        raise ValueError('wrong journal comparison derivation policy')
    if evidence['global_upper_bound_claimed'] is not False or evidence['production_sdk_parity_established'] is not False:
        raise ValueError('unsupported journal comparison evidence claim')
    rows={r['sample_id']:r for r in evidence['samples']}
    if len(evidence['samples'])!=3 or set(rows)!={'concentrated-zero','concentrated-long-prefix','concentrated-short-prefix'}:
        raise ValueError('wrong journal calibration population')
    requirements=[]
    for row in rows.values():
        for name in ['A','N','M','J','C','actual_pgu','old_node_pgu','comparison_work_units']:
            integer(row[name],0,2**64-1,name)
        if not 1<=row['M']<=row['N'] or row['C']!=(row['N']+7*row['M']+7)//8:
            raise ValueError('wrong low32 comparison envelope')
        units=row['C']*(row['A']+row['N']+row['J'])
        if row['comparison_work_units']!=units or not 1<=units<=WORK_UNIT_BOUND:
            raise ValueError('wrong or out-of-domain journal work units')
        for name in ['actual_input_sha256','verified_report_sha256']:
            if re.fullmatch('[0-9a-f]{64}',row[name]) is None:
                raise ValueError('invalid sealed source identity')
        residual=max(row['actual_pgu']-row['old_node_pgu'],0)
        required=Fraction(residual,units)
        sealed=row['required_coefficient']
        if required!=Fraction(sealed['numerator'],sealed['denominator']):
            raise ValueError('sealed journal requirement differs')
        requirements.append(required)
    maximum=max(Fraction(0),*requirements)
    margin=Fraction(5,4)*maximum
    coefficient=-(-margin.numerator//(margin.denominator*64))*64
    if coefficient!=evidence['coefficient_pgu_per_work_unit'] or coefficient!=384 or evidence['coefficient_scaled']!=coefficient*SCALE:
        raise ValueError('journal evidence no longer derives 384 PGU')
    short=rows['concentrated-short-prefix']
    prior_shortfall=short['actual_pgu']-short['old_node_pgu']-256*short['comparison_work_units']
    if prior_shortfall!=101266793:
        raise ValueError('prior 256-PGU negative observation must remain visible')
    control=evidence['dispersed_long_prefix_control']
    if control['hypothetical_candidate_pgu']!=control['node_pgu']+coefficient*control['work_units']:
        raise ValueError('wrong dispersed-control projection')
    if control['synthetic_overquote_accepted'] is not True:
        raise ValueError('dispersed-control conservatism is not acknowledged')
    return {'coefficient_pgu':coefficient,'maximum_required_pgu':maximum,'prior_256_shortfall_pgu':prior_shortfall}


def transform(parent,evidence):
    """Add one normalized tariff/domain and preserve every inherited numerical price."""
    d=derive(parent,evidence)
    a=parent['arithmetic']
    if a['scale']!=SCALE or a['signed_integer_bits']!=128 or a['selector_bound']!=10**9:
        raise ValueError('unexpected parent arithmetic domain')
    if a['declared_worst_case_scaled_pgu']!=a['maximum_coefficient_numerator']*a['selector_bound']*a['maximum_terms_per_scope']:
        raise ValueError('unrecognized parent worst-case declaration')
    tariff=d['coefficient_pgu']*SCALE
    worst=a['declared_worst_case_scaled_pgu']+tariff*WORK_UNIT_BOUND
    if worst>2**127-1:
        raise ValueError('journal term exceeds signed i128 worst-case envelope')
    result=copy.deepcopy(parent)
    result['block_local']['storage_journal_work_tariff']=tariff
    result['arithmetic']['storage_journal_work_unit_bound']=WORK_UNIT_BOUND
    result['arithmetic']['declared_worst_case_scaled_pgu']=worst
    result['purpose']='block_local_storage_journal_comparison_reserve'
    result['parent_generator']=result['generator']
    result['generator']={'path':'crates/evm/src/zk_gas/pricing/storage_journal_successor.py',
        'file_sha256':hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        'table_generator_path':'crates/evm/src/zk_gas/pricing/generate.py',
        'table_generator_sha256':hashlib.sha256((BASE/'generate.py').read_bytes()).hexdigest()}
    result['parent_validation']=result['validation']
    result['validation']={'inherited_authority_fields_reexecuted':False,'new_sp1_measurement_rows':1,
        'reused_sp1_measurement_rows':2,'prospective_actuals_used_for_fit':0,'new_native_repricing_performed':False,
        'historical_repricing_performed':False,'universal_upper_bound_claimed':False}
    maximum=d['maximum_required_pgu']
    result['successor']={'schema_version':1,'parent_artifact_sha256':PARENT_SHA256,'parent_snapshot':PARENT_PATH,
        'journal_evidence_path':EVIDENCE_PATH,'journal_evidence_sha256':digest(evidence),
        'storage_journal_pgu_per_work_unit':d['coefficient_pgu'],'storage_journal_work_unit_bound':WORK_UNIT_BOUND,
        'exact_maximum_required_pgu':{'numerator':maximum.numerator,'denominator':maximum.denominator},
        'coefficient_policy':POLICY,'prior_256_shortfall_pgu':d['prior_256_shortfall_pgu'],
        'comparison_envelope':'C=ceil((N+7*M)/8); per(tx,owner) potential materializations and low32 groups',
        'composition':'independent normalized block-local addition, outside opcode route max; no second discount',
        'empirical_limit':'Three controlled b76/SDK6.3 whole inputs; fixed calls/probes/growth not causally decomposed; no global PGU bound or production SDK certification',
        'scope':'add_journal_work_tariff_only;all_parent_numeric_prices_and_Unzen_unchanged'}
    result['artifact_sha256']=content_address(result)
    return result


def validate_successor(payload,parent,evidence):
    """Reject any price, evidence, domain, policy or provenance drift."""
    if payload!=transform(parent,evidence):
        raise ValueError('successor differs from deterministic journal tariff replay')


def main():
    """Write or check the sealed runtime artifact and separate acceptance record."""
    parser=argparse.ArgumentParser(description=__doc__);parser.add_argument('--check',action='store_true');args=parser.parse_args()
    payload=transform(json.loads((BASE/PARENT_PATH).read_text()),json.loads((BASE/EVIDENCE_PATH).read_text()))
    output,acceptance=BASE/'runtime-spec.json',BASE/'accepted-runtime-spec.sha256'
    if args.check:
        if json.loads(output.read_text())!=payload or acceptance.read_text().strip()!=payload['artifact_sha256']:
            raise ValueError('journal successor output or acceptance record is stale')
    else:
        output.write_bytes(canonical_json(payload)+b'\n');acceptance.write_text(payload['artifact_sha256']+'\n')
    print(('verified ' if args.check else 'generated ')+payload['artifact_sha256'])

if __name__=='__main__':main()
