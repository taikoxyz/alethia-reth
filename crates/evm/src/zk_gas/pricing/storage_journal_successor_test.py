"""Replay the additive journal tariff and reject unsealed evidence or numeric drift."""
import copy
from fractions import Fraction
import importlib.util
import json
from pathlib import Path
import unittest

BASE = Path(__file__).resolve().parent

class StorageJournalSuccessorTests(unittest.TestCase):
    def load(self):
        spec = importlib.util.spec_from_file_location('storage_journal_successor', BASE/'storage_journal_successor.py')
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        return module, json.loads((BASE/module.PARENT_PATH).read_text()), json.loads((BASE/module.EVIDENCE_PATH).read_text())

    def test_only_journal_tariff_and_its_sealed_arithmetic_domain_are_added(self):
        m,parent,evidence=self.load()
        result=m.transform(parent,evidence)
        expected=copy.deepcopy(parent)
        expected['block_local']['storage_journal_work_tariff']=384*10**18
        expected['arithmetic']['storage_journal_work_unit_bound']=10**12
        expected['arithmetic']['declared_worst_case_scaled_pgu']+=384*10**18*10**12
        for key in ['artifact_sha256','purpose','generator','parent_generator','validation','parent_validation','successor']:
            expected[key]=result[key]
        self.assertEqual(result,expected)
        self.assertLessEqual(result['arithmetic']['declared_worst_case_scaled_pgu'],2**127-1)
        d=m.derive(parent,evidence)
        self.assertEqual(d['coefficient_pgu'],384)
        self.assertEqual(d['maximum_required_pgu'],Fraction(672101993,2229825))
        self.assertEqual(d['prior_256_shortfall_pgu'],101266793)
        self.assertEqual(result['arithmetic']['selector_bound'],10**9)
        m.validate_successor(result,parent,evidence)

    def test_unchanged_256_negative_observation_and_each_calibration_row_are_preserved(self):
        m,parent,evidence=self.load()
        d=m.derive(parent,evidence)
        for row in evidence['samples']:
            self.assertGreaterEqual(row['old_node_pgu']+d['coefficient_pgu']*row['comparison_work_units'],row['actual_pgu'])
        short=next(r for r in evidence['samples'] if r['sample_id']=='concentrated-short-prefix')
        self.assertLess(short['old_node_pgu']+256*short['comparison_work_units'],short['actual_pgu'])
        self.assertFalse(m.transform(parent,evidence)['validation']['universal_upper_bound_claimed'])
        self.assertFalse(m.transform(parent,evidence)['validation']['new_native_repricing_performed'])

    def test_parent_evidence_policy_units_and_unrelated_price_mutations_are_rejected(self):
        m,parent,evidence=self.load()
        mutations=[lambda x:x['samples'][0].update(actual_pgu=1),
                   lambda x:x['samples'][1].update(C=1),
                   lambda x:x['samples'][2].update(comparison_work_units=1),
                   lambda x:x.update(coefficient_pgu_per_work_unit=256),
                   lambda x:x.update(global_upper_bound_claimed=True)]
        for mutate in mutations:
            wrong=copy.deepcopy(evidence);mutate(wrong)
            with self.assertRaises(ValueError):m.transform(parent,wrong)
        wrong=copy.deepcopy(parent);wrong['block_local']['access_list_item_tariff']=0;wrong['artifact_sha256']=m.content_address(wrong)
        with self.assertRaises(ValueError):m.transform(wrong,evidence)
        wrong=m.transform(parent,evidence);wrong['block_local']['normal_dispatch_tariff']+=1;wrong['artifact_sha256']=m.content_address(wrong)
        with self.assertRaises(ValueError):m.validate_successor(wrong,parent,evidence)
        wrong=m.transform(parent,evidence);wrong['arithmetic']['storage_journal_work_unit_bound']-=1;wrong['artifact_sha256']=m.content_address(wrong)
        with self.assertRaises(ValueError):m.validate_successor(wrong,parent,evidence)

if __name__=='__main__':unittest.main()
