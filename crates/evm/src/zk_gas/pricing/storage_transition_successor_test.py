"""Independent storage-transition identity, immutable inherited prices and checked domain."""
import copy
import importlib.util
import json
from pathlib import Path
import unittest
BASE=Path(__file__).resolve().parent
SPEC=importlib.util.spec_from_file_location('storage_transition_successor',BASE/'storage_transition_successor.py')
MODEL=importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODEL)
class StorageTransitionSuccessorTests(unittest.TestCase):
    def setUp(self):
        self.parent=json.loads((BASE/MODEL.PARENT_PATH).read_bytes())
        self.evidence=json.loads((BASE/MODEL.EVIDENCE_PATH).read_bytes())
    def test_independent_transition_tariff_and_inherited_prices(self):
        result=MODEL.transform(self.parent,self.evidence)
        block=dict(result['block_local'])
        self.assertEqual(block.pop('storage_transition_work_tariff',None),384*10**18)
        self.assertEqual(block,self.parent['block_local'])
        self.assertEqual(result['proposal_once'],self.parent['proposal_once'])
        self.assertEqual(result['account_cache_policy'],self.parent['account_cache_policy'])
        arithmetic=dict(result['arithmetic'])
        self.assertEqual(arithmetic.pop('storage_transition_work_unit_bound'),10**12)
        arithmetic['declared_worst_case_scaled_pgu']-=384*10**30
        self.assertEqual(arithmetic,self.parent['arithmetic'])
    def test_parent_evidence_and_policy_tamper_rejected(self):
        for target in ['parent','evidence']:
            parent,evidence=copy.deepcopy(self.parent),copy.deepcopy(self.evidence)
            if target=='parent':parent['block_local']['storage_journal_work_tariff']+=1
            else:evidence['samples'][1]['actual_sp1_pgu']-=1
            with self.assertRaises(ValueError):MODEL.transform(parent,evidence)
        result=MODEL.transform(self.parent,self.evidence)
        result['storage_transition_policy']['transaction_budget']='all SLOADs'
        with self.assertRaises(ValueError):MODEL.validate_successor(result,self.parent,self.evidence)
    def test_i128_declaration_boundary(self):
        parent=copy.deepcopy(self.parent)
        parent['arithmetic']['declared_worst_case_scaled_pgu']=2**127-1-384*10**30
        self.assertEqual(MODEL.checked_declaration(parent),2**127-1)
        parent['arithmetic']['declared_worst_case_scaled_pgu']+=1
        with self.assertRaises(ValueError):MODEL.checked_declaration(parent)
if __name__=='__main__':unittest.main()
