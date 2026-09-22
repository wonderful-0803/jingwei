import unittest
from collections import Counter
from sample import select
class SampleTests(unittest.TestCase):
 def fixture(self):
  return [{'id':f'{level}-{i}', 'instruction_type':level, 'instruction':text} for level in ['Cell-Level Manipulation','Sheet-Level Manipulation'] for i,text in enumerate(['sum values','extract characters','sort rows','calculate percentage']*20)]
 def test_balanced_unique_seeded_selection_excludes_seen(self):
  tasks=self.fixture();excluded={'Cell-Level Manipulation-0','Sheet-Level Manipulation-0'}
  picked=select(tasks,excluded)
  self.assertEqual(len(picked),50)
  self.assertEqual(Counter(t['instruction_type'] for t in picked),{'Cell-Level Manipulation':25,'Sheet-Level Manipulation':25})
  self.assertEqual(len({str(t['id']) for t in picked}),50)
  self.assertFalse({str(t['id']) for t in picked}&excluded)
  self.assertEqual(picked,select(tasks,excluded))
  self.assertNotEqual(picked,select(tasks,excluded,seed=42))
  self.assertEqual({t['primary_category'] for t in picked},{'aggregation_statistics','text_processing','filter_sort_deduplicate','numeric_calculation'})
 def test_short_stratum_fails_instead_of_silently_shrinking(self):
  with self.assertRaises(ValueError):select(self.fixture()[:10],set())
 def test_metadata_flagged_pick_replaced_with_same_stratum(self):
  tasks=self.fixture();original=select(tasks,set());flagged=original[0]['id']
  for t in tasks:
   if t['id']==flagged:t['exclude']='known invalid annotation'
  updated=select(tasks,set())
  self.assertEqual(len(updated),50);self.assertNotIn(flagged,[t['id'] for t in updated])
  self.assertEqual(updated[0]['instruction_type'],original[0]['instruction_type'])
  self.assertEqual(updated[0]['primary_category'],original[0]['primary_category'])
  self.assertEqual([t['id'] for t in updated[1:]],[t['id'] for t in original[1:]])
 def test_duplicate_ids_rejected(self):
  tasks=self.fixture()
  with self.assertRaises(ValueError):select(tasks+[tasks[0]],set())
if __name__=='__main__':unittest.main()
