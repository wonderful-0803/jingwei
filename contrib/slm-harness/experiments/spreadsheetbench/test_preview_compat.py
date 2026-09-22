import tempfile,unittest
from pathlib import Path
import openpyxl
from run import preview
class PreviewCompatTests(unittest.TestCase):
 def test_empty_sheet_dimensions_do_not_crash(self):
  with tempfile.TemporaryDirectory() as d:
   p=Path(d)/'input.xlsx';w=openpyxl.Workbook();w.create_sheet('Empty');w.save(p)
   out=preview(p);self.assertIn('Empty',out)
if __name__=='__main__':unittest.main()
