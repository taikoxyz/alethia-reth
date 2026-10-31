"""Exact bounded DELEGATECALL refinement, immutable evidence and single-term scope."""
import copy
from fractions import Fraction
import importlib.util
import json
from pathlib import Path
import unittest

BASE=Path(__file__).resolve().parent


class DelegatecallSuccessorTests(unittest.TestCase):
    def load(self):
        path=BASE/'delegatecall_successor.py'
        self.assertTrue(path.is_file(),'DELEGATECALL successor exporter is not implemented')
        spec=importlib.util.spec_from_file_location('delegatecall_successor',path)
        m=importlib.util.module_from_spec(spec);spec.loader.exec_module(m);return m

    def test_exact_only_f4_coefficient_changes_and_finite_margin_policy(self):
        m=self.load();parent=json.loads((BASE/m.PARENT_PATH).read_text());evidence=json.loads((BASE/m.EVIDENCE_PATH).read_text())
        derived=m.derive(parent,evidence);self.assertEqual(derived['coefficient_pgu'],6000)
        self.assertGreater(derived['maximum_required_pgu'],Fraction(554189,100));self.assertLess(derived['maximum_required_pgu'],Fraction(554190,100))
        self.assertEqual(len(derived['pairs']),10)
        result=m.transform(parent,evidence);expected=copy.deepcopy(parent)
        route=next(r for r in expected['block_local']['operation_routes'] if r['route_key']==m.ROUTE)
        self.assertEqual(route['accepted_absolute_model']['coefficient'],3841906721059316908619)
        route['accepted_absolute_model']['coefficient']=6000*10**18
        for key in ('artifact_sha256','purpose','generator','parent_generator','validation','parent_validation','successor'):expected[key]=result[key]
        self.assertEqual(result,expected)
        self.assertEqual(m.projected_underquotes(parent,evidence,False),1);self.assertEqual(m.projected_underquotes(parent,evidence,True),0)
        for pair in derived['pairs']:
            uplift=(6000*10**18-3841906721059316908619)*pair['extra_calls']
            required=Fraction(5,4)*pair['actual_increment_pgu']*10**18
            self.assertGreaterEqual(int(pair['parent_node_increment_scaled'])+uplift,required)
            self.assertGreaterEqual(int(pair['parent_complete_increment_scaled'])+uplift,required)
        self.assertEqual(result['validation']['prospective_actuals_used_for_fit'],0)
        self.assertFalse(result['validation']['new_native_repricing_performed']);self.assertFalse(result['validation']['universal_upper_bound_claimed'])
        m.validate_successor(result,parent,evidence)

    def test_exact_evidence_identity_and_replay_reject_drift(self):
        m=self.load();parent=json.loads((BASE/m.PARENT_PATH).read_text());evidence=json.loads((BASE/m.EVIDENCE_PATH).read_text())
        self.assertEqual(len(evidence['measurements']),8);self.assertEqual(len({r['input_sha256'] for r in evidence['measurements']}),8)
        self.assertEqual(evidence['policy']['prospective_actuals_opened'],0)
        self.assertEqual(evidence['policy_file_sha256'],'cbaa730e43c7243e7a03343204aa05e38d01ff8638b494ad00df4eaaa7e84a4a')
        self.assertEqual(evidence['sources']['independent_actual_audit_sha256'],'40ba8b660dc325f2d2a8a6f66c60dcfb359328ce5c2f5bf29aa242eb6b456499')
        for change in [lambda p:p['measurements'][0].update(actual_pgu=1),lambda p:p['measurements'][0].update(confirmed_delegatecalls=1),
                       lambda p:p['measurements'][0].update(input_sha256='0'*64),lambda p:p['policy'].update(frozen_confirmed_delegatecall_pgu=7000),
                       lambda p:p['policy'].update(prospective_actuals_opened=1),lambda p:p['measurements'].append(copy.deepcopy(p['measurements'][0])),
                       lambda p:p['sources'].update(independent_actual_audit_sha256='0'*64)]:
            wrong=copy.deepcopy(evidence);change(wrong)
            with self.assertRaises(ValueError):m.transform(parent,wrong)
        wrong=copy.deepcopy(parent);wrong['block_local']['authorization_validation_tariff']=0;wrong['artifact_sha256']=m.content_address(wrong)
        with self.assertRaises(ValueError):m.transform(wrong,evidence)
        for change in [lambda p:p['block_local'].update(access_list_item_tariff=0),lambda p:p['authorities'].update(runtime_candidate_sha256='0'*64),
                       lambda p:p['execution_identity'].update(extra=1),lambda p:p['generator'].update(file_sha256='0'*64),
                       lambda p:next(r for r in p['block_local']['operation_routes'] if r['route_key']==m.ROUTE)['accepted_absolute_model']['shared_memory'].update(memory_growth_event=0)]:
            wrong=m.transform(parent,evidence);change(wrong);wrong['artifact_sha256']=m.content_address(wrong)
            with self.assertRaises(ValueError):m.validate_successor(wrong,parent,evidence)


if __name__=='__main__':unittest.main()
