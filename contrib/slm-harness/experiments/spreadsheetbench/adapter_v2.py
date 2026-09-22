"""Read-only observation and explicit submission. No hidden answers in this worker."""
import json, os, sys
from pathlib import Path
from adapter import HERE, execute_python
from workbook_validation import validate_output

if __name__=='__main__':
    expected=Path(os.environ.get('SLMHARNESS_PYTHON', str(HERE/'.venv/bin/python')))
    # Prefer the experiment venv when it is present; source-only checkouts use
    # the interpreter that launched the worker instead of failing on a missing venv.
    if expected.exists() and Path(sys.prefix).resolve()!=expected.parent.parent.resolve():
        os.execv(str(expected),[str(expected),__file__,*sys.argv[1:]])
    task_dir=Path(sys.argv[1]).resolve();tool=sys.argv[2];args=json.load(sys.stdin)
    code=args.get('code')
    if not isinstance(code,str) or len(code)>60000:raise ValueError('code must be <=60000 characters')
    if tool not in ('inspect_python','submit_solution'):raise ValueError('unknown tool')
    submit=tool=='submit_solution'
    result=execute_python(task_dir/'input.xlsx',task_dir/('sandbox' if submit else 'inspection'),code,fresh=submit,read_only=not submit)
    result['ok']=result['exit_code']==0
    if submit:
        (task_dir/'solution.py').write_text(code)
        result['ok'] &= result['output_valid']
        if result['ok']:
            result['validation']=validate_output(task_dir/'input.xlsx',task_dir/'sandbox')
            result['ok']=result['validation']['ok']
        result['validated']=result['ok']
        result['note']=('Submission registered with solution.py and output.xlsx. This checks file structure, not answer correctness. After success, finish.'
                        if result['ok'] else 'Submission failed and was not registered. Read the execution/validation errors, then submit corrected standalone code.')
    else:
        result['note']='Inspection cannot save results. Use submit_solution with concise standalone code to create and register the output.'
    print(json.dumps(result,ensure_ascii=False))
