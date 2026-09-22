"""Short metadata transactions for the already claimed E004 experiment."""
from datetime import datetime,timezone
import json
import os
from pathlib import Path
import re
import time
import uuid
ROOT=Path(__file__).resolve().parents[2]
RUN='20260921-spreadsheetbench'

def record(kind,execution,summary,evidence=None,exit_code=None):
    lock=ROOT/'.coordination/write.lock'
    for _ in range(100):
        try:lock.mkdir();break
        except FileExistsError:time.sleep(.05)
    else:raise RuntimeError('Coordination lock occupied; no metadata overwritten')
    ts=datetime.now(timezone.utc).isoformat();eid=str(uuid.uuid4())
    (lock/'owner.json').write_text(json.dumps({'actor':'codex','run_id':RUN,'pid':os.getpid(),'acquired_at':ts}))
    def atomic(path,text):
        tmp=path.with_name(path.name+'.'+eid+'.tmp');tmp.write_text(text);tmp.replace(path)
    try:
        status={'execution_started':'running','execution_progress':'running','execution_finished':'succeeded','execution_failed':'failed'}.get(kind,'recorded')
        event={'schema_version':1,'event_id':eid,'timestamp':ts,'actor':'codex','run_id':RUN,'task_id':'E004','execution_id':execution,'type':kind,'status':status,'summary':summary,'operations':['jingwei model/tool execution batch; individual calls recorded in task journal and llm-trace.jsonl'],'files_changed':[evidence] if evidence else [],'evidence':[evidence] if evidence else [],'verification':{'exit_code':exit_code,'result':summary},'next_action':'读取artifacts/spreadsheetbench/verified50/progress.json；运行中不要重复启动GPU评测'}
        with (ROOT/'collaboration/events/2026-09-21.jsonl').open('a') as f:f.write(json.dumps(event,ensure_ascii=False)+'\n');f.flush();os.fsync(f.fileno())
        p=ROOT/'collaboration/STATE.md';s=p.read_text()
        s=re.sub(r'Revision：(\d+)',lambda m:'Revision：'+str(int(m[1])+1),s,count=1)
        s=re.sub(r'Last event：[^\n]+','Last event：'+eid,s,count=1)
        s=re.sub(r'更新时间：[^\n]+','更新时间：'+ts,s,count=1)
        s=re.sub(r'活动 execution：[^\n]+','活动 execution：E004 / '+execution+' / '+status+'。',s,count=1)
        section='\n## E004：SpreadsheetBench\n\n'+ts+'：'+summary+'\nexecution：'+execution+' / '+status+'。\n证据：'+str(evidence or 'artifacts/spreadsheetbench')+'\n'
        if '\n## E004：SpreadsheetBench\n' in s:s=re.sub(r'\n## E004：SpreadsheetBench\n.*?(?=\n## |\Z)',lambda _:section,s,flags=re.S)
        else:s+=section
        atomic(p,s)
    finally:
        owner=json.loads((lock/'owner.json').read_text())
        if owner['run_id']==RUN and owner['pid']==os.getpid():(lock/'owner.json').unlink();lock.rmdir()
