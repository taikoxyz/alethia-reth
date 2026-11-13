"""Exact empirical code-preparation policy, sealed evidence and unchanged parent terms."""
import copy
from fractions import Fraction
import importlib.util
import json
from pathlib import Path
import unittest

BASE = Path(__file__).resolve().parent


class CodePreparationSuccessorTests(unittest.TestCase):
    def load(self):
        path = BASE/'code_preparation_successor.py'
        self.assertTrue(path.is_file(), 'code preparation successor generator is not implemented')
        spec = importlib.util.spec_from_file_location('code_preparation_successor',path)
        module = importlib.util.module_from_spec(spec); spec.loader.exec_module(module)
        return module

    def test_exact_policy_and_scope_preserve_all_other_prices(self):
        s = self.load(); parent=json.loads((BASE/s.PARENT_PATH).read_text()); policy=json.loads((BASE/s.EVIDENCE_PATH).read_text())
        derived=s.derive(policy)
        self.assertEqual(derived['unique_requirement'],Fraction(242969,4096))
        self.assertEqual(derived['event_requirement'],Fraction(5525449319692974457273,10**18))
        self.assertEqual(derived['repeated_byte_requirements'],[Fraction(12804639688156788955551,512000000000000000000),Fraction(115840038443411100599959,4608000000000000000000)])
        self.assertEqual([derived[k] for k in ('unique_unit','event_unit','byte_unit')],[64,8192,32])
        result=s.transform(parent,policy);expected=copy.deepcopy(parent)
        expected['block_local']['resource_tariffs']['unique_code_bytes']=64*10**18
        expected['block_local']['code_preparation_tariffs']={'nonempty_events':8192*10**18,'total_bytes':32*10**18}
        expected['ownership']['block_local'].append('block.code_preparation')
        for name in ('artifact_sha256','purpose','generator','parent_generator','successor','validation','parent_validation'):expected[name]=result[name]
        self.assertEqual(result,expected)
        self.assertEqual(s.projected_underquotes(policy,False),18)
        self.assertEqual(s.projected_underquotes(policy,True),0)
        self.assertFalse(result['validation']['new_native_repricing_performed'])
        self.assertFalse(result['validation']['universal_upper_bound_claimed'])
        self.assertEqual(result['validation']['prospective_holdouts_used_for_fit'],0)

    def test_rejects_evidence_parent_and_successor_drift(self):
        s=self.load();parent=json.loads((BASE/s.PARENT_PATH).read_text());policy=json.loads((BASE/s.EVIDENCE_PATH).read_text())
        for change in [lambda p:p.update(extra=1),lambda p:p['measurements'][0].update(actual_pgu=1),lambda p:p['measurements'].append(copy.deepcopy(p['measurements'][0]))]:
            wrong=copy.deepcopy(policy);change(wrong)
            with self.assertRaises(ValueError):s.transform(parent,wrong)
        wrong=copy.deepcopy(parent);wrong['block_local']['authorization_validation_tariff']=0;wrong['artifact_sha256']=s.content_address(wrong)
        with self.assertRaises(ValueError):s.transform(wrong,policy)
        for change in [lambda p:p['block_local']['code_preparation_tariffs'].update(nonempty_events=1),lambda p:p['block_local']['resource_tariffs'].update(unique_code_bytes=32*10**18),lambda p:p['block_local'].update(access_list_item_tariff=0),lambda p:p['generator'].update(file_sha256='0'*64)]:
            wrong=s.transform(parent,policy);change(wrong);wrong['artifact_sha256']=s.content_address(wrong)
            with self.assertRaises(ValueError):s.validate_successor(wrong,parent,policy)


if __name__=='__main__':unittest.main()
