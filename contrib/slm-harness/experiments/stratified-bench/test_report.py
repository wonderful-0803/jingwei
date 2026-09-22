import json,tempfile,unittest
from pathlib import Path
from report import collect
class ReportTests(unittest.TestCase):
 def test_grade_and_delivery_are_not_conflated(self):
  with tempfile.TemporaryDirectory() as tmp:
   root=Path(tmp);(root/'selection.json').write_text(json.dumps({'seed':7,'ids':['a','b'],'official_type_counts':{'Cell':2},'primary_category_counts':{'text':2}}))
   for tid,passed,delivered in [('a',True,False),('b',False,True)]:
    p=root/'9b'/tid;p.mkdir(parents=True);(p/'metrics.json').write_text(json.dumps({'id':tid,'passed':passed,'delivery_completed':delivered,'elapsed_seconds':1,'model_calls':1,'category':'test','instruction_type':'Cell','primary_category':'text'}))
   r=collect(root)['models']['9b'];self.assertEqual((r['passed'],r['delivered'],r['end_to_end_passed']),(1,1,0))
if __name__=='__main__':unittest.main()
