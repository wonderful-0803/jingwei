"""Targeted diagnostic rerun, not a held-out benchmark or accuracy estimate."""
import json,os,sys,time,fcntl,subprocess,hashlib
from pathlib import Path
from sync_state import record
ROOT=Path(__file__).resolve().parents[2]
sys.path.insert(0,str(ROOT/'experiments/spreadsheetbench'))
from run import api,Calc,task_run,DATA,MODELS,dump,sha,BINARY,HERE
OUT=ROOT/'artifacts/harness-reliability/targeted-v2'
SELECT={'9b':['13-1','267-21'],'27b':['97-36','17-35']}

def main():
 OUT.mkdir(parents=True,exist_ok=True)
 lock=(OUT/'.runner.lock').open('a+');fcntl.flock(lock,fcntl.LOCK_EX|fcntl.LOCK_NB)
 if (OUT/'manifest.json').exists():raise RuntimeError('Use a new output directory; no silent resume')
 if api('ps')['models']:raise RuntimeError('Loaded model belongs to another activity; refusing to disturb')
 data={str(t['id']):t for t in json.loads((DATA/'dataset.json').read_text())}
 tags={t['name']:t for t in api('tags')['models']}
 files=list((ROOT/'jingwei/crates/agent/reference/src').glob('*.rs'))+list((HERE/'src').glob('*.rs'))+[HERE/p for p in ['profile_v2.json','skill_v2.md','adapter.py','adapter_v2.py','proxy.py','run.py','Cargo.toml','Cargo.lock']]
 baseline=ROOT/'artifacts/spreadsheetbench/verified50'
 manifest={'selection':SELECT,'selection_reason':'known failure diagnostics; not held-out','baseline_sha256':{str(p.relative_to(ROOT)):sha(p) for p in baseline.rglob('result.json')},'source_sha256':{str(p.relative_to(ROOT)):sha(p) for p in files},'binary_sha256':sha(BINARY),'models':{k:{'tag':MODELS[k],'digest':tags[MODELS[k]]['digest']} for k in SELECT},'parameters':{'max_steps':8,'max_corrections':2,'output_reserve':4096,'num_ctx':32768,'temperature':0,'seed':7,'thinking':'none','max_inspections':2,'reserve_steps':2},'dataset_sha256':sha(DATA/'dataset.json')}
 dump(OUT/'manifest.json',manifest);dump(OUT/'process.json',{'pid':os.getpid(),'started':time.time()})
 calc=Calc(OUT/'calc');results=[]
 try:
  for key,ids in SELECT.items():
   tag=MODELS[key]
   try:
    api('generate',{'model':tag,'prompt':'OK','stream':False,'think':False,'keep_alive':'30m','options':{'temperature':0,'seed':7,'num_ctx':32768,'num_predict':1}})
    dump(OUT/key/'environment.json',{'loaded':api('ps'),'version':api('version')})
    for tid in ids:
     eid='E005-targeted-'+key+'-'+tid
     record('execution_started',eid,'小失败集真实模型验证 '+key+' / '+tid,str((OUT/key/tid).relative_to(ROOT)))
     print(json.dumps({'started':key,'id':tid}),flush=True)
     r=task_run(data[tid],OUT/key/tid,tag,calc,profile=HERE/'profile_v2.json')
     r['delivery_completed']=bool(r.get('agent',{}).get('artifact',{}).get('business_verified'))
     dump(OUT/key/tid/'result.json',r);results.append(r)
     dump(OUT/'progress.json',[{k:r.get(k) for k in ['id','model','category','passed','delivery_completed','elapsed_seconds','model_calls']} for r in results])
     print(json.dumps({'finished':key,'id':tid,'passed':r['passed'],'delivery':r['delivery_completed'],'category':r['category']}),flush=True)
     record('execution_finished',eid,'真实题结束；grade='+str(r['passed'])+'；delivery='+str(r['delivery_completed']),str((OUT/key/tid/'result.json').relative_to(ROOT)),r['host_exit'])
   finally:api('generate',{'model':tag,'stream':False,'keep_alive':0});dump(OUT/key/'cleanup.json',api('ps'))
  unchanged=all(sha(ROOT/p)==value for p,value in manifest['baseline_sha256'].items())
  dump(OUT/'complete.json',{'results':json.loads((OUT/'progress.json').read_text()),'baseline_unchanged':unchanged,'models_unloaded':not api('ps')['models']})
 finally:calc.close()
if __name__=='__main__':
 try:main()
 except BaseException as e:
  dump(OUT/'failed.json',{'error':repr(e),'pid':os.getpid()});raise
