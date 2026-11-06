#!/usr/bin/env python3
"""Derive the independent empirical Account-density reserve from immutable model69."""
import argparse, copy, hashlib, json, re
from pathlib import Path
BASE=Path(__file__).resolve().parent
PARENT_SHA256='69d2b812552a4c5380da478ebcf35aeaaa851881371de738633584aa0d695b4f'
PARENT_PATH='successors/parent-69d2b812.json'
EVIDENCE_PATH='successors/account-journal-v1.json'
EVIDENCE_SHA256='171d9f665d735e7170e6bf6333851cc6f784d48e7a0aba4808af9dfca1099644'
BOUND=10**12
OPCODE_BUDGETS={**{f'opcode:0x{x:02x}':2 for x in [0x31,0x3b,0x3c,0x3f,0x47]},**{f'opcode:0x{x:02x}':4 for x in [0x54,0x55]},**{f'opcode:0x{x:02x}':32 for x in [0xf0,0xf1,0xf2,0xf4,0xf5,0xfa,0xff]}}
def canonical_json(v):
    """Encode portable canonical source values."""
    return json.dumps(v,sort_keys=True,separators=(',',':')).encode()
def digest(v):
    """Bind every field of an immutable evidence object."""
    return hashlib.sha256(canonical_json(v)).hexdigest()
def content_address(v):
    """Exclude only the self-referential artifact address."""
    return digest({k:x for k,x in v.items() if k!='artifact_sha256'})
def tariff(evidence):
    """Derive fourfold positive residual divided by lower basis, upward to64PGU."""
    row=evidence['samples'][1];residual=row['actual_sp1_pgu']-row['node_pgu'];basis=evidence['lower_basis']['work_units']
    if residual<=0 or basis<=0:raise ValueError('nonpositive empirical basis')
    return ((4*residual+64*basis-1)//(64*basis))*64

def checked_declaration(parent):
    """Extend the inherited mixed-domain i128 declaration without changing its terms."""
    value=parent['arithmetic']['declared_worst_case_scaled_pgu']+128*10**18*BOUND
    if not 0<=value<=2**127-1:raise ValueError('mixed-domain i128 overflow')
    return value

def transform(parent,evidence):
    """Authenticate evidence and add one policy-sealed independently normalized term."""
    if parent.get('artifact_sha256')!=PARENT_SHA256 or content_address(parent)!=PARENT_SHA256:raise ValueError('wrong immutable Account parent')
    if digest(evidence)!=EVIDENCE_SHA256 or evidence['parent_model_id']!=PARENT_SHA256:raise ValueError('wrong sealed Account evidence')
    b=evidence['lower_basis']
    if b!=dict(user_balances=66561,query_budget=133122,physical_population=1024,max_prefix_group=1024,comparison_equivalents=1024,work_units=137365504):raise ValueError('wrong lower basis')
    if b['query_budget']!=2*b['user_balances'] or b['work_units']!=b['comparison_equivalents']*(b['query_budget']+b['physical_population']):raise ValueError('invalid lower arithmetic')
    if tariff(evidence)!=128:raise ValueError('changed derived tariff')
    for row in evidence['samples']:
        if row['model_id']!=PARENT_SHA256 or row['canonical_attempts']!=1 or not 0<row['charged_block_gas']<=30_000_000:raise ValueError('invalid canonical scope')
        if any(not re.fullmatch('[0-9a-f]{64}',row[k]) for k in ['input_sha256','native_sha256','quote_sha256','raw_sha256','independent_actual_sha256','independent_profile_sha256']):raise ValueError('missing evidence identity')
    result=copy.deepcopy(parent)
    result['purpose']='block_local_empirical_account_journal_density_reserve'
    result['block_local']['account_journal_work_tariff']=tariff(evidence)*10**18
    result['arithmetic']['account_journal_work_unit_bound']=BOUND
    result['arithmetic']['declared_worst_case_scaled_pgu']=checked_declaration(parent)
    result['account_journal_policy']=dict(opcode_budgets=dict(OPCODE_BUDGETS),other_opcode_budget=0,early_failures_included=True,confirmed_spawn_recounted=False,prefix='Address20 first4 network-order bytes',population='actual finalized EvmState keys per transaction; includes absent/untouched/reverted; excludes declared-unread AL',comparison_proxy='ceil((N+4*M)/5)',work_units='sum_tx(C*(Q+N))',threshold=0,external_residuals=evidence['external_validation_authority_fee_residuals'],budget_is_exact_query_count=False,universal_upper_bound_claimed=False)
    result['ownership']['block_local'].append('block.account_journal_work_units')
    result['parent_generator']=parent['generator'];result['parent_validation']=parent['validation']
    result['generator']=dict(path='crates/evm/src/zk_gas/pricing/account_journal_successor.py',file_sha256=hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),table_generator_path='crates/evm/src/zk_gas/pricing/generate.py',table_generator_sha256=hashlib.sha256((BASE/'generate.py').read_bytes()).hexdigest())
    result['validation']=dict(opened_whole_input_sp1_actual_rows=2,new_actual_measurements_requested=0,inherited_coefficients_refitted=False,new_tariff_empirical_conservative=True,fresh_repricing_performed=False,historical_repricing_performed=False,universal_upper_bound_claimed=False,production_sdk_parity_established=False)
    result['successor']=dict(parent_artifact_sha256=PARENT_SHA256,parent_snapshot=PARENT_PATH,evidence_path=EVIDENCE_PATH,evidence_sha256=EVIDENCE_SHA256,empirical_policy=evidence['coefficient_policy'],pgu_per_unit=128,work_unit_bound=BOUND,composition='independent block-local node committed/prove attempted; proposal_once zero; no second discount',fixture_tag=evidence['fixture_tag'],publication_status_at_model_generation='planned')
    result['artifact_sha256']=content_address(result)
    return result

def validate_successor(payload,parent,evidence):
    """Require exact deterministic replay including policy and inherited objects."""
    if payload!=transform(parent,evidence):raise ValueError('Account successor differs from deterministic replay')
def main():
    """Generate or verify the accepted runtime specification; no measurements."""
    parser=argparse.ArgumentParser(description=__doc__);parser.add_argument('--check',action='store_true');args=parser.parse_args()
    result=transform(json.loads((BASE/PARENT_PATH).read_bytes()),json.loads((BASE/EVIDENCE_PATH).read_bytes()))
    spec,acceptance=BASE/'runtime-spec.json',BASE/'accepted-runtime-spec.sha256'
    if args.check:
        if json.loads(spec.read_bytes())!=result or acceptance.read_text().strip()!=result['artifact_sha256']:raise ValueError('stale Account model')
    else:spec.write_bytes(canonical_json(result)+b'\n');acceptance.write_text(result['artifact_sha256']+'\n')
    print(result['artifact_sha256'])
if __name__=='__main__':main()
