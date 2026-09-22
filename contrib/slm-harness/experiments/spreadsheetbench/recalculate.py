"""Native LibreOffice worker; trusted, isolated profile, macros and link updates disabled."""
import json
from pathlib import Path
import subprocess
import sys
import time
import uuid
sys.path.insert(0,str(Path(__file__).resolve().parents[1]/'excel-smoke'))
from office import connect,load,save,stop_office

def main():
    root=Path(sys.argv[1]).resolve();root.mkdir(parents=True,exist_ok=True)
    pipe='sb_recalc_'+uuid.uuid4().hex
    log=(root/'office.log').open('ab')
    proc=subprocess.Popen(['/opt/libreoffice26.8/program/soffice',f'-env:UserInstallation={(root/"profile").as_uri()}','--headless','--norestore','--nodefault','--nofirststartwizard',f'--accept=pipe,name={pipe};urp;StarOffice.ComponentContext'],stdout=log,stderr=subprocess.STDOUT)
    try:
        for _ in range(150):
            try:desktop=connect(pipe);break
            except Exception:
                if proc.poll() is not None:raise RuntimeError('Office exited')
                time.sleep(.1)
        else:raise TimeoutError('UNO startup')
        print(json.dumps({'ready':True,'office_pid':proc.pid}),flush=True)
        for line in sys.stdin:
            job=json.loads(line);doc=None
            try:
                doc=load(desktop,job['input'])
                save(doc,job['output'])
                print(json.dumps({'ok':True,'output':job['output']}),flush=True)
            except Exception as exc:print(json.dumps({'ok':False,'error':repr(exc)}),flush=True)
            finally:
                if doc is not None:doc.close(True)
    finally:stop_office(proc,log)
if __name__=='__main__':main()
