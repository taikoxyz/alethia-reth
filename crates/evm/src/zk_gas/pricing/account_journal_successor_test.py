"""Account density policy, empirical tariff and immutable-parent checks."""
import copy, importlib.util, json, unittest
from pathlib import Path
BASE=Path(__file__).resolve().parent
S=importlib.util.spec_from_file_location('account_successor',BASE/'account_journal_successor.py')
M=importlib.util.module_from_spec(S);S.loader.exec_module(M)
class AccountSuccessorTests(unittest.TestCase):
    def setUp(self):
        self.parent=json.loads((BASE/M.PARENT_PATH).read_bytes())
        self.evidence=json.loads((BASE/M.EVIDENCE_PATH).read_bytes())
    def test_derived_empirical_tariff_and_inherited_numeric_objects(self):
        result=M.transform(self.parent,self.evidence)
        block=dict(result['block_local']);self.assertEqual(block.pop('account_journal_work_tariff'),128*10**18)
        self.assertEqual(block,self.parent['block_local'])
        arithmetic=dict(result['arithmetic']);self.assertEqual(arithmetic.pop('account_journal_work_unit_bound'),10**12)
        arithmetic['declared_worst_case_scaled_pgu']-=128*10**18*10**12
        self.assertEqual(arithmetic,self.parent['arithmetic'])
        self.assertEqual(result['proposal_once'],self.parent['proposal_once'])
        self.assertEqual(result['account_journal_policy']['opcode_budgets'],M.OPCODE_BUDGETS)
        self.assertLess(result['arithmetic']['declared_worst_case_scaled_pgu'],2**127)
    def test_evidence_parent_and_policy_tamper_fail_closed(self):
        for target in ['parent','evidence']:
            p,e=copy.deepcopy(self.parent),copy.deepcopy(self.evidence)
            if target=='parent':p['block_local']['access_list_address_pair_tariff']+=1
            else:e['samples'][1]['actual_sp1_pgu']-=1
            with self.assertRaises(ValueError):M.transform(p,e)
        result=M.transform(self.parent,self.evidence);result['account_journal_policy']['opcode_budgets']['opcode:0x31']=1
        with self.assertRaises(ValueError):M.validate_successor(result,self.parent,self.evidence)
        self.assertEqual(M.OPCODE_BUDGETS['opcode:0x31'],2)
    def test_mixed_domain_i128_max_and_overflow(self):
        p,e=copy.deepcopy(self.parent),copy.deepcopy(self.evidence)
        p['arithmetic']['declared_worst_case_scaled_pgu']=2**127-1-128*10**18*10**12
        # Arithmetic validation is separately exercised without bypassing immutable parent checks.
        self.assertEqual(M.checked_declaration(p),2**127-1)
        p['arithmetic']['declared_worst_case_scaled_pgu']+=1
        with self.assertRaises(ValueError):M.checked_declaration(p)
if __name__=='__main__':unittest.main()
