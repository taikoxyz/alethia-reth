"""Check the distinct transient selector extension without changing any inherited price."""
import copy
import importlib.util
import json
from pathlib import Path
import unittest

BASE = Path(__file__).resolve().parent

class TransientJournalSuccessorTests(unittest.TestCase):
    def load(self):
        spec=importlib.util.spec_from_file_location('transient_journal_successor',BASE/'transient_journal_successor.py')
        module=importlib.util.module_from_spec(spec);spec.loader.exec_module(module)
        return module,json.loads((BASE/module.PARENT_PATH).read_text()),json.loads((BASE/module.EVIDENCE_PATH).read_text())

    def test_all_numeric_price_and_arithmetic_objects_are_unchanged(self):
        m,parent,evidence=self.load();result=m.transform(parent,evidence)
        expected=copy.deepcopy(parent)
        for key in m.CHANGED_METADATA_FIELDS:expected[key]=result[key]
        self.assertEqual(result,expected)
        self.assertEqual(result['arithmetic'],parent['arithmetic'])
        self.assertEqual(result['block_local'],parent['block_local'])
        self.assertNotEqual(result['artifact_sha256'],parent['artifact_sha256'])
        m.validate_successor(result,parent,evidence)

    def test_opened_finite_actuals_and_conservative_false_positive_are_explicit(self):
        m,parent,evidence=self.load();rows=m.derive(parent,evidence)
        self.assertEqual(len(rows),8)
        high=rows['high32/r512'];low=rows['low32/zero'];normal=rows['default/r512']
        self.assertEqual(high['transient_work_units'],17564672)
        self.assertEqual(high['additional_pgu'],6744834048)
        self.assertEqual(low['actual_pgu']-low['projected_node_pgu'],96543801)
        self.assertGreater(normal['projected_node_pgu'],36*normal['actual_pgu'])
        result=m.transform(parent,evidence)
        self.assertFalse(result['validation']['coefficient_refitted'])
        self.assertFalse(result['validation']['universal_upper_bound_claimed'])
        self.assertEqual(result['successor']['combined_work_unit_bound'],10**12)
        self.assertIn('distinct',result['successor']['transient_materialization_policy'])

    def test_mutations_of_parent_evidence_semantics_and_prices_are_rejected(self):
        m,parent,evidence=self.load()
        for key,value in [('actual_pgu',1),('N',511),('M',1),('transient_work_units',0)]:
            wrong=copy.deepcopy(evidence);wrong['samples'][0][key]=value
            with self.assertRaises(ValueError):m.transform(parent,wrong)
        wrong=copy.deepcopy(evidence);wrong['coefficient_refitted']=True
        with self.assertRaises(ValueError):m.transform(parent,wrong)
        wrong=copy.deepcopy(parent);wrong['block_local']['storage_journal_work_tariff']=0;wrong['artifact_sha256']=m.content_address(wrong)
        with self.assertRaises(ValueError):m.transform(wrong,evidence)
        for mutate in [lambda x:x['block_local'].update(storage_journal_work_tariff=0),
                       lambda x:x['arithmetic'].update(storage_journal_work_unit_bound=2*10**12),
                       lambda x:x['successor'].update(transient_materialization_policy='reinsertion events')]:
            wrong=m.transform(parent,evidence);mutate(wrong);wrong['artifact_sha256']=m.content_address(wrong)
            with self.assertRaises(ValueError):m.validate_successor(wrong,parent,evidence)

if __name__=='__main__':unittest.main()
