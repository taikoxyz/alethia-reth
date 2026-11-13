"""Authenticate the finite BLOCKHASH evidence, single coefficient and 25% margin."""
import copy
from fractions import Fraction
import importlib.util
import json
from pathlib import Path
import unittest

BASE=Path(__file__).resolve().parent


class BlockhashSuccessorTests(unittest.TestCase):
    def load(self):
        path=BASE/'blockhash_successor.py'
        self.assertTrue(path.is_file(),'BLOCKHASH successor exporter is not implemented')
        spec=importlib.util.spec_from_file_location('blockhash_successor',path)
        m=importlib.util.module_from_spec(spec);spec.loader.exec_module(m);return m

    def test_current_runtime_uses_seven_hundred_fixed_price(self):
        payload=json.loads((BASE/'runtime-spec.json').read_text())
        route=next(r for r in payload['block_local']['operation_routes'] if r['route_key']=='opcode:0x40')
        self.assertEqual(route['accepted_absolute_model']['coefficient'],700*10**18)

    def test_exact_only_blockhash_coefficient_and_empirical_margin(self):
        m=self.load();parent=json.loads((BASE/m.PARENT_PATH).read_text());evidence=json.loads((BASE/m.EVIDENCE_PATH).read_text())
        d=m.derive(parent,evidence);self.assertEqual(d['coefficient_pgu'],700);self.assertEqual(len(d['pairs']),15)
        self.assertGreater(d['maximum_required_pgu'],Fraction(612549,1000));self.assertLess(d['maximum_required_pgu'],Fraction(612550,1000))
        result=m.transform(parent,evidence);expected=copy.deepcopy(parent)
        next(r for r in expected['block_local']['operation_routes'] if r['route_key']==m.ROUTE)['accepted_absolute_model']['coefficient']=700*10**18
        for key in ('artifact_sha256','purpose','generator','parent_generator','validation','parent_validation','successor'):expected[key]=result[key]
        self.assertEqual(expected,result)
        self.assertEqual(m.projected_underquotes(parent,evidence,False),6);self.assertEqual(m.projected_underquotes(parent,evidence,True),0)
        for pair in d['pairs']:
            uplift=(700*10**18-32117684584888432025)*pair['extra_blockhash_queries'];required=Fraction(5,4)*pair['actual_increment_pgu']*10**18
            self.assertGreaterEqual(int(pair['parent_node_increment_scaled'])+uplift,required)
            self.assertGreaterEqual(int(pair['parent_complete_increment_scaled'])+uplift,required)
        self.assertEqual(result['validation']['prospective_actuals_used_for_fit'],0)
        self.assertFalse(result['validation']['new_native_repricing_performed']);self.assertFalse(result['validation']['universal_upper_bound_claimed'])
        m.validate_successor(result,parent,evidence)

    def test_evidence_parent_and_replay_reject_drift(self):
        m=self.load();parent=json.loads((BASE/m.PARENT_PATH).read_text());evidence=json.loads((BASE/m.EVIDENCE_PATH).read_text())
        self.assertEqual(len(evidence['measurements']),12);self.assertEqual(len({r['input_sha256'] for r in evidence['measurements']}),12)
        self.assertEqual(evidence['policy_file_sha256'],'0b3878b9fa538baa871af449510a126bdf9b52b265f09e92e038a28e7ee1ec4e')
        self.assertEqual(evidence['sources']['independent_actual_audit_sha256'],'bfb76bb32df324a3c5851197fce216cada0cf1d5de7ff2a88de4e213c07ee543')
        for change in [lambda p:p['measurements'][0].update(actual_pgu=1),lambda p:p['measurements'][0].update(total_blockhash_queries=0),
                       lambda p:p['measurements'][0].update(input_sha256='0'*64),lambda p:p['policy'].update(candidate_price_pgu=800),
                       lambda p:p.update(prospective_holdouts_used_for_fit=1),lambda p:p['matched_pairs'].pop(),
                       lambda p:p['sources'].update(independent_actual_audit_sha256='0'*64)]:
            wrong=copy.deepcopy(evidence);change(wrong)
            with self.assertRaises(ValueError):m.transform(parent,wrong)
        wrong=copy.deepcopy(parent);wrong['block_local']['authorization_validation_tariff']=0;wrong['artifact_sha256']=m.content_address(wrong)
        with self.assertRaises(ValueError):m.transform(wrong,evidence)
        for change in [lambda p:p['block_local'].update(access_list_item_tariff=0),lambda p:p['authorities'].update(runtime_candidate_sha256='0'*64),
                       lambda p:p['execution_identity'].update(extra=1),lambda p:p['generator'].update(file_sha256='0'*64),
                       lambda p:next(r for r in p['block_local']['operation_routes'] if r['route_key']==m.ROUTE)['feature_error_absolute_model'].update(minimum=0)]:
            wrong=m.transform(parent,evidence);change(wrong);wrong['artifact_sha256']=m.content_address(wrong)
            with self.assertRaises(ValueError):m.validate_successor(wrong,parent,evidence)


if __name__=='__main__':unittest.main()
