#!/usr/bin/env python3
"""Derive one independently priced progressive observed-account-cache candidate from af30."""
import argparse, copy, hashlib, json
from pathlib import Path
BASE=Path(__file__).resolve().parent
PARENT_SHA256='af30c61656fd456ab290a837b44c181e4ff54e575a949d3579f07df20659a949'
PARENT_PATH="successors/parent-af30c616.json"
EVIDENCE_PATH="successors/account-cache-v1.json"
EVIDENCE_SHA256='6c0aeafef62c2f2d5db001cb939564b78d74313c3e4f1659bbcea9d756e4c26d'
BOUND=10**12
RATE=128

def canonical_json(value):
    """Encode portable canonical identities."""
    return json.dumps(value,sort_keys=True,separators=(',',':')).encode()
def digest(value):
    """Bind every field of evidence."""
    return hashlib.sha256(canonical_json(value)).hexdigest()
def content_address(value):
    """Exclude only the self-referential artifact address."""
    return digest({k:v for k,v in value.items() if k!='artifact_sha256'})
def checked_declaration(parent):
    """Add the independent domain without widening any inherited bounds."""
    result=parent['arithmetic']['declared_worst_case_scaled_pgu']+RATE*10**18*BOUND
    if not 0<=result<2**127:raise ValueError('mixed-domain i128 overflow')
    return result

def transform(parent,evidence):
    """Authenticate opened stress evidence and preserve all existing numeric tariffs."""
    if parent.get('artifact_sha256')!=PARENT_SHA256 or content_address(parent)!=PARENT_SHA256:raise ValueError('wrong immutable cache parent')
    if evidence.get('parent_model_id')!=PARENT_SHA256 or digest(evidence)!=EVIDENCE_SHA256:raise ValueError('wrong sealed cache evidence')
    result=copy.deepcopy(parent)
    result['purpose']='block_local_empirical_progressive_account_cache_candidate'
    result['block_local']['account_cache_work_tariff']=RATE*10**18
    result['arithmetic']['account_cache_work_unit_bound']=BOUND
    result['arithmetic']['declared_worst_case_scaled_pgu']=checked_declaration(parent)
    result['account_cache_policy']=copy.deepcopy(evidence['policy'])
    result['ownership']['block_local'].append('block.account_cache_work_units')
    result['parent_generator']=parent['generator'];result['parent_validation']=parent['validation']
    result['generator']=dict(path='crates/evm/src/zk_gas/pricing/account_cache_successor.py',file_sha256=hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),table_generator_path='crates/evm/src/zk_gas/pricing/generate.py',table_generator_sha256=hashlib.sha256((BASE/'generate.py').read_bytes()).hexdigest())
    result['validation']=dict(opened_whole_input_sp1_actual_rows=2,new_actual_measurements_requested=0,inherited_coefficients_refitted=False,new_tariff_empirical_conservative=True,fresh_repricing_performed=False,historical_repricing_performed=False,universal_upper_bound_claimed=False,production_sdk_parity_established=False,opened_samples_are_holdout=False)
    result['successor']=dict(parent_artifact_sha256=PARENT_SHA256,parent_snapshot=PARENT_PATH,evidence_path=EVIDENCE_PATH,evidence_sha256=EVIDENCE_SHA256,empirical_policy=evidence['coefficient_policy'],pgu_per_unit=RATE,work_unit_bound=BOUND,composition='independent block-local node committed/prove attempted; proposal_once zero; no second discount',fixture_tag=evidence['fixture_tag'],publication_status_at_model_generation='candidate_pending_requotes')
    result['artifact_sha256']=content_address(result)
    return result

def validate_successor(payload,parent,evidence):
    """Require deterministic replay including all policy/provenance and inherited prices."""
    if payload!=transform(parent,evidence):raise ValueError('cache successor differs from deterministic replay')
def main():
    """Generate or check runtime specification without executing any workload."""
    p=argparse.ArgumentParser(description=__doc__);p.add_argument('--check',action='store_true');args=p.parse_args()
    result=transform(json.loads((BASE/PARENT_PATH).read_bytes()),json.loads((BASE/EVIDENCE_PATH).read_bytes()))
    spec,accept=BASE/'runtime-spec.json',BASE/'accepted-runtime-spec.sha256'
    if args.check:
        if json.loads(spec.read_bytes())!=result or accept.read_text().strip()!=result['artifact_sha256']:raise ValueError('stale cache candidate')
    else:spec.write_bytes(canonical_json(result)+b'\n');accept.write_text(result['artifact_sha256']+'\n')
    print(result['artifact_sha256'])
if __name__=='__main__':main()
