"""Regression checks against real exported workbooks: extra writes must not pass."""
import json,shutil
from pathlib import Path
from evaluate import ROOT,score
from office import start_office,stop_office,load,save
base=ROOT/'artifacts/excel-smoke/run-03/control-empty'
root=ROOT/'artifacts/excel-smoke/grading-regression';root.mkdir(exist_ok=False)
for name in ['input.xlsx','working.xlsx','output.xlsx','state.json','protected.json']:shutil.copy2(base/name,root/name)
proc,log,pipe,desktop=start_office(root/'office')
try:
    assert all(score(desktop,root,'empty',True).values())
    doc=load(desktop,root/'output.xlsx')
    doc.Sheets.getByName('Data').getCellRangeByName('F2').setFormula('=SUM(D2:D9)');save(doc,root/'output.xlsx');doc.close(True)
    extra_cell=score(desktop,root,'empty',True);assert not extra_cell['whole_data_sheet']
    shutil.copy2(base/'output.xlsx',root/'output.xlsx')
    doc=load(desktop,root/'output.xlsx');doc.Sheets.insertNewByName('Summary',2);save(doc,root/'output.xlsx');doc.close(True)
    extra_sheet=score(desktop,root,'empty',True);assert not extra_sheet['sheet_set']
    result={'baseline_pass':True,'extra_cell_rejected':True,'extra_sheet_rejected':True}
    (root/'result.json').write_text(json.dumps(result,indent=2));print(json.dumps(result))
finally:stop_office(proc,log)
