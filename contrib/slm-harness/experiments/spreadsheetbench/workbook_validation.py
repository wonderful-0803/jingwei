"""Validate untrusted generated workbooks under the executor's resource limits."""
from adapter import execute_python

_VALIDATION = '''
import zipfile
import openpyxl
with zipfile.ZipFile('/work/output.xlsx') as archive:
    entries=archive.infolist()
    if len(entries)>2048 or sum(e.file_size for e in entries)>64*1024*1024 or any(e.file_size>16*1024*1024 for e in entries):
        raise ValueError('archive expansion limit')
book=openpyxl.load_workbook('/work/output.xlsx',read_only=True)
book.close()
'''

def validate_output(input_path,work):
    result=execute_python(input_path,work,_VALIDATION,read_only=True,timeout=10)
    result['ok']=result['exit_code']==0 and result['output_valid']
    return result
