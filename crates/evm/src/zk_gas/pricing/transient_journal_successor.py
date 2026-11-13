#!/usr/bin/env python3
"""Extend the existing journal selector to distinct transient materializations; reuse its tariff."""
import argparse
import copy
import hashlib
import json
from pathlib import Path
import re

BASE = Path(__file__).resolve().parent
PARENT_SHA256 = '5f93909ea1372eb6e10fa2b599c6087aafd4ec493ba28eb1cea4c4abb3708a11'
PARENT_PATH = 'successors/parent-5f93909e.json'
EVIDENCE_PATH = 'successors/transient-journal-v1.json'
EVIDENCE_SHA256 = '2e8a86c949a87c7f1f977da6de72e193f87561c7c6557b967a92ee2ad2475c53'
CHANGED_METADATA_FIELDS = ('artifact_sha256','purpose','generator','parent_generator','validation','parent_validation','successor')
TRANSIENT_POLICY = ('per(tx,owner) distinct full-U256 keys first potentially materialized by vacant nonzero TSTORE; '
                    'initial physical keys seed both seen and N/M; TLOAD miss and absent-zero TSTORE do not mark seen; '
                    'clear/revert retain seen; transaction finish charges then resets')


def canonical_json(value):
    """Encode deterministic portable JSON bytes."""
    return json.dumps(value,sort_keys=True,separators=(',',':')).encode()


def digest(value):
    """Hash a portable evidence or provenance object."""
    return hashlib.sha256(canonical_json(value)).hexdigest()


def content_address(value):
    """Exclude only the artifact's self-referential address."""
    return digest({k:v for k,v in value.items() if k!='artifact_sha256'})


def integer(value,name):
    """Require an exact nonnegative integer within the checked runtime count domain."""
    if type(value) is not int or not 0<=value<=2**64-1:
        raise ValueError('invalid '+name)


def derive(parent,evidence):
    """Authenticate the opened actual panel and recompute finite projections, never fit a tariff."""
    if parent.get('artifact_sha256')!=PARENT_SHA256 or content_address(parent)!=PARENT_SHA256:
        raise ValueError('wrong immutable transient-selector parent')
    if digest(evidence)!=EVIDENCE_SHA256 or evidence['parent_model_id']!=PARENT_SHA256:
        raise ValueError('wrong sealed transient evidence')
    if parent['block_local']['storage_journal_work_tariff']!=384*10**18 or parent['arithmetic']['storage_journal_work_unit_bound']!=10**12:
        raise ValueError('unexpected inherited journal tariff or domain')
    if (evidence['coefficient_refitted'] is not False or evidence['tariff_pgu_per_work_unit']!=384
            or evidence['global_pgu_upper_bound_claimed'] is not False
            or evidence['production_sdk_parity_established'] is not False
            or evidence['projection_is_fresh_quote'] is not False):
        raise ValueError('unsupported transient evidence claim')
    rows={r['sample_id']:r for r in evidence['samples']}
    population={'default/zero','default/r64','default/r512','high32/zero','high32/r512','low32/zero','low32/r512','low32/full'}
    if len(evidence['samples'])!=8 or set(rows)!=population or evidence['new_actual_count']!=8:
        raise ValueError('wrong finite transient population')
    if [r['case_id'] for r in evidence['skipped_actuals']]!=['high32/full']:
        raise ValueError('unmeasured actual must remain explicitly skipped')
    for sample_id,row in rows.items():
        for name in ['repetitions','block_gas','actual_pgu','old_node_pgu','old_proposal_pgu','A','N','M','J','C',
                     'transient_work_units','additional_pgu','projected_node_pgu','projected_proposal_pgu']:
            integer(row[name],name)
        expected_r={'zero':0,'r64':64,'r512':512,'full':2180}[sample_id.split('/')[1]]
        expected_m=1 if sample_id.startswith('low32/') else 512
        if (row['repetitions']!=expected_r or row['N']!=512 or row['M']!=expected_m
                or row['A']!=514+64*expected_r or row['J']!=512 or row['C']!=(row['N']+7*row['M']+7)//8):
            raise ValueError('wrong distinct transient comparison envelope')
        units=row['C']*(row['A']+row['N']+row['J'])
        if (row['transient_work_units']!=units or row['additional_pgu']!=384*units
                or row['projected_node_pgu']!=row['old_node_pgu']+384*units
                or row['projected_proposal_pgu']!=row['old_proposal_pgu']+384*units
                or not 0<units<=10**12 or not 0<row['block_gas']<=30_000_000):
            raise ValueError('wrong finite transient projection')
        for name in ['input_sha256','raw_actual_sha256','baseline_quote_sha256']:
            if re.fullmatch('[0-9a-f]{64}',row[name]) is None:
                raise ValueError('invalid transient source identity')
    # Keep both the motivating deficit and the residual/false-positive limitations visible.
    if rows['high32/r512']['actual_pgu']-rows['high32/r512']['old_node_pgu']!=2923362025:
        raise ValueError('lost high-priority baseline deficit')
    if rows['low32/zero']['actual_pgu']-rows['low32/zero']['projected_node_pgu']!=96543801:
        raise ValueError('lost residual node-only deficit')
    if rows['default/r512']['projected_node_pgu']<=36*rows['default/r512']['actual_pgu']:
        raise ValueError('lost constructed conservative false positive')
    return rows


def transform(parent,evidence):
    """Change only sealed selector semantics/provenance; preserve every numeric pricing object."""
    derive(parent,evidence)
    result=copy.deepcopy(parent)
    result['purpose']='block_local_ordinary_and_transient_journal_comparison_reserve'
    result['parent_generator']=parent['generator']
    result['generator']={'path':'crates/evm/src/zk_gas/pricing/transient_journal_successor.py',
        'file_sha256':hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        'table_generator_path':'crates/evm/src/zk_gas/pricing/generate.py',
        'table_generator_sha256':hashlib.sha256((BASE/'generate.py').read_bytes()).hexdigest()}
    result['parent_validation']=parent['validation']
    result['validation']={'inherited_authority_fields_reexecuted':False,'opened_sp1_actual_rows':8,
        'coefficient_refitted':False,'actual_panel_used_to_select_semantic_extension':True,
        'new_native_repricing_performed':False,'historical_zero_transient_repricing_performed':False,
        'universal_upper_bound_claimed':False,'production_sdk_parity_established':False}
    result['successor']={'schema_version':1,'parent_artifact_sha256':PARENT_SHA256,'parent_snapshot':PARENT_PATH,
        'journal_evidence_path':EVIDENCE_PATH,'journal_evidence_sha256':digest(evidence),
        'storage_journal_pgu_per_work_unit':384,'combined_work_unit_bound':10**12,
        'persistent_materialization_policy':'unchanged conservative initial-slot-plus-vacant-attempt events; no full-key dedup',
        'transient_materialization_policy':TRANSIENT_POLICY,
        'comparison_envelope':'C=ceil((N+7*M)/8); U=sum(tx,owner) C*(A+N+J); low32 raw U256 prefix groups',
        'transient_attempt_policy':'A counts every valid-operand TLOAD/TSTORE attempt; J counts every TSTORE attempt; no cold entries',
        'composition':'checked ordinary U + transient U in one existing normalized journal term; no second discount; proposal_once adds zero',
        'scope':'selector_semantics_only;all_parent_numeric_prices_arithmetic_domains_and_Unzen_unchanged',
        'empirical_limit':'Eight opened controlled b76/SDK6.3 actuals; 384 reused without refit; projections are not fresh quotes or holdouts; low32 zero node deficit and constructed overquote retained; no global PGU bound or production SDK certification'}
    result['artifact_sha256']=content_address(result)
    return result


def validate_successor(payload,parent,evidence):
    """Reject changed parent prices, domains, semantic policy, evidence or provenance."""
    if payload!=transform(parent,evidence):
        raise ValueError('successor differs from deterministic transient selector replay')


def main():
    """Write or check the active runtime artifact and its separate acceptance record."""
    parser=argparse.ArgumentParser(description=__doc__);parser.add_argument('--check',action='store_true');args=parser.parse_args()
    payload=transform(json.loads((BASE/PARENT_PATH).read_text()),json.loads((BASE/EVIDENCE_PATH).read_text()))
    output,acceptance=BASE/'runtime-spec.json',BASE/'accepted-runtime-spec.sha256'
    if args.check:
        if json.loads(output.read_text())!=payload or acceptance.read_text().strip()!=payload['artifact_sha256']:
            raise ValueError('transient successor output or acceptance record is stale')
    else:
        output.write_bytes(canonical_json(payload)+b'\n');acceptance.write_text(payload['artifact_sha256']+'\n')
    print(('verified ' if args.check else 'generated ')+payload['artifact_sha256'])

if __name__=='__main__':main()
