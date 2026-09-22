"""Short, locked E009 metadata transactions; no model work under the lock."""
import json, os, re, time, uuid
from pathlib import Path
from datetime import datetime, timezone
ROOT=Path(__file__).resolve().parents[2]
RUN='20260922-wordbench-models'
def atomic(path,text):
 p=Path(path); tmp=p.with_name(p.name+'.'+uuid.uuid4().hex+'.tmp');tmp.write_text(text);tmp.replace(p)
def record(kind, execution, summary, evidence='', code=None):
 lock=ROOT/'.coordination/write.lock'
 for _ in range(100):
  try:lock.mkdir();break
  except FileExistsError:time.sleep(.05)
 else:raise RuntimeError('metadata lock busy')
 owner={'actor':'codex','run_id':RUN,'pid':os.getpid(),'acquired_at':datetime.now(timezone.utc).isoformat()}
 (lock/'owner.json').write_text(json.dumps(owner))
 try:
  now=datetime.now(timezone.utc); eid=str(uuid.uuid4())
  event={'schema_version':1,'event_id':eid,'timestamp':now.isoformat(),'actor':'codex','run_id':RUN,'task_id':'E009','execution_id':execution,'type':kind,'status':'running' if kind in ('execution_started','execution_progress') else ('failed' if kind=='execution_failed' else 'succeeded'),'summary':summary,'operations':[summary],'files_changed':['experiments/wordbench/**','artifacts/wordbench-models/**'],'evidence':[evidence] if evidence else [],'verification':{'exit_code':code},'next_action':'修复预检或继续固定清单双模型评测；以process/progress文件为准'}
  with (ROOT/'collaboration/events'/f'{now:%Y-%m-%d}.jsonl').open('a') as f:f.write(json.dumps(event,ensure_ascii=False)+'\n');f.flush();os.fsync(f.fileno())
  path=ROOT/'collaboration/STATE.md';s=path.read_text();rev=int(re.search(r'Revision：(\d+)',s)[1])+1
  for key,value in [('Revision',str(rev)),('Last event',eid),('更新时间',now.isoformat()),('活动 execution',f'E009 / {execution} / {event["status"]}；{summary}')]:s=re.sub(r'- '+key+'：.*','- '+key+'：'+value,s,count=1)
  atomic(path,s)
 finally:
  if json.loads((lock/'owner.json').read_text())==owner:(lock/'owner.json').unlink();lock.rmdir()
if __name__=='__main__':
 import sys
 record(*sys.argv[1:])
