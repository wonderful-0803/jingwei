"""Fixed-selection paired run using unchanged v2 agent, worker and grader."""
import argparse,fcntl,hashlib,importlib.util,json,os,shutil,signal,subprocess,sys,time
from pathlib import Path
from datetime import datetime,timezone
from sync_state import record
from report import collect,make_report
HERE=Path(__file__).resolve().parent;ROOT=HERE.parents[1]
BENCH=ROOT/'experiments/spreadsheetbench';sys.path.insert(0,str(BENCH))
spec=importlib.util.spec_from_file_location('benchmark_driver',BENCH/'run.py');base=importlib.util.module_from_spec(spec);spec.loader.exec_module(base)
OUT=ROOT/'artifacts/spreadsheetbench/stratified50-v2'
REPORT=ROOT/'docs/reports/spreadsheetbench-stratified50-v2-2026-09-21.md'
STOP=False
OWNED=False

def stopped(sig,frame):
 global STOP;STOP=True

def finish_metadata(status,summary):
 # Short shared-state transaction; never hold the lock across model/engine work.
 lock=ROOT/'.coordination/write.lock'
 for _ in range(100):
  try:lock.mkdir();break
  except FileExistsError:time.sleep(.05)
 else:raise RuntimeError('metadata lock occupied')
 owner={'actor':'codex','run_id':'20260921-stratified50-v2','pid':os.getpid()};(lock/'owner.json').write_text(json.dumps(owner))
 try:
  p=ROOT/'collaboration/TASKS.md';lines=p.read_text().splitlines()
  lines=[line.replace('| in_progress |',f'| {status} |').replace('codex / 20260921-stratified50-v2','已释放（codex / 20260921-stratified50-v2）') if line.startswith('| E006 |') else line for line in lines];p.write_text('\n'.join(lines)+'\n')
  import re
  p=ROOT/'collaboration/STATE.md';s=p.read_text();s=re.sub(r'当前阶段：[^\n]+','当前阶段：'+summary,s,count=1);s=re.sub(r'当前 owner：[^\n]+','当前 owner：E006已释放；H001仍由claude持有。',s,count=1);s=re.sub(r'活动 execution：[^\n]+','活动 execution：E006 runner已退出，见complete/failed/stopped.json。',s,count=1);p.write_text(s)
 finally:
  if json.loads((lock/'owner.json').read_text())==owner:(lock/'owner.json').unlink();lock.rmdir()

def main():
 global STOP,OWNED
 for sig in (signal.SIGTERM,signal.SIGINT):signal.signal(sig,stopped)
 OUT.mkdir(parents=True,exist_ok=True)
 lock=(OUT/'.runner.lock').open('a+');fcntl.flock(lock,fcntl.LOCK_EX|fcntl.LOCK_NB)
 gpu_lock=(ROOT/'.coordination/ollama-eval.lock').open('a+');fcntl.flock(gpu_lock,fcntl.LOCK_EX|fcntl.LOCK_NB)
 OWNED=True
 selection=json.loads((OUT/'selection.json').read_text());ids=selection['ids']
 if len(ids)!=50 or len(set(ids))!=50:raise ValueError('selection must contain 50 unique IDs')
 tasks={str(t['id']):t for t in json.loads((base.DATA/'dataset.json').read_text())}
 for selected in selection['tasks']:
  task=tasks[str(selected['id'])]
  if any(task.get(k)!=selected.get(k) for k in task):raise RuntimeError('selection differs from dataset')
 tags={t['name']:t for t in base.api('tags')['models']}
 sources=[p for parent in [HERE,BENCH] for p in parent.rglob('*') if p.is_file() and '.venv' not in p.parts and '__pycache__' not in p.parts and p.suffix in ('.py','.rs','.md','.json','.toml','.lock')]
 sources+=list((ROOT/'jingwei/crates/agent/reference/src').glob('*.rs'))
 sources+=list((ROOT/'artifacts/spreadsheetbench/upstream/evaluation').rglob('*.py'))
 manifest={'selection_sha256':base.sha(OUT/'selection.json'),'ids':ids,'models':{k:{'name':v,'digest':tags[v]['digest']} for k,v in base.MODELS.items()},'source_sha256':{str(p.relative_to(ROOT)):base.sha(p) for p in sources},'binary_sha256':base.sha(base.BINARY),'dataset_sha256':base.sha(base.DATA/'dataset.json'),'data_files_sha256':{str(p.relative_to(base.DATA)):base.sha(p) for t in selection['tasks'] for p in (base.DATA/t['spreadsheet_path']).glob('*.xlsx')},'profile':str(BENCH/'profile_v2.json'),'sampling':{'temperature':0,'seed':7,'thinking':'none','context':32768,'max_output':4096,'steps':8,'corrections':2,'inspections':2,'reserved_steps':2},'engine':subprocess.check_output(['/opt/libreoffice26.8/program/soffice','--version'],text=True).strip(),'dependencies':{'python':sys.version,'openpyxl':base.openpyxl.__version__,'numpy':base.numpy.__version__,'pandas':base.pandas.__version__},'core_head':subprocess.check_output(['git','-C',str(ROOT/'jingwei'),'rev-parse','HEAD'],text=True).strip()}
 old=OUT/'manifest.json'
 if old.exists() and json.loads(old.read_text())!=manifest:raise RuntimeError('Manifest changed; refusing mixed-version resume')
 base.dump(old,manifest)
 frozen=OUT/'jingwei-bench-runner'
 if not frozen.exists():shutil.copy2(base.BINARY,frozen)
 if base.sha(frozen)!=manifest['binary_sha256']:raise RuntimeError('frozen binary mismatch')
 base.BINARY=frozen
 def verify_sources():
  if base.sha(OUT/'selection.json')!=manifest['selection_sha256'] or base.sha(base.DATA/'dataset.json')!=manifest['dataset_sha256']:raise RuntimeError('Selection or dataset metadata changed during run')
  if any(base.sha(ROOT/p)!=v for p,v in manifest['source_sha256'].items()):raise RuntimeError('Source changed during run; stopped to preserve provenance')
 if base.api('ps')['models']:raise RuntimeError('Ollama busy with another loaded model; not disturbing it')
 base.dump(OUT/'process.json',{'pid':os.getpid(),'started':datetime.now(timezone.utc).isoformat(),'command':sys.argv,'stop':'SIGTERM requests stop after current task; no forced kill','resume':'same script, unchanged manifest; completed results are retained'})
 record('execution_started','E006-run','双模型50题固定清单运行；PID '+str(os.getpid())+'；每题同步，SIGTERM当前题结束后停止',str((OUT/'process.json').relative_to(ROOT)))
 calc=base.Calc(OUT/'calc');current=None
 try:
  for key,tag in base.MODELS.items():
   pending=[t for t in selection['tasks'] if not (OUT/key/str(t['id'])/'metrics.json').exists()]
   if not pending:continue
   if STOP:break
   try:
    verify_sources()
    warm=base.api('generate',{'model':tag,'prompt':'OK','stream':False,'think':False,'keep_alive':'30m','options':{'temperature':0,'seed':7,'num_ctx':32768,'num_predict':1}})
    base.dump(OUT/key/'environment.json',{'loaded':base.api('ps'),'version':base.api('version'),'gpu':subprocess.check_output(['nvidia-smi','--query-gpu=name,memory.used,memory.total','--format=csv,noheader'],text=True)})
    print(json.dumps({'model_started':key,'pending':len(pending)}),flush=True);errors=0
    for selected in pending:
     if STOP:break
     verify_sources();tid=str(selected['id']);current='E006-'+key+'-'+tid
     record('execution_started',current,'运行 '+key+' / '+tid+' / '+selected['primary_category'],str((OUT/key/tid).relative_to(ROOT)))
     print(json.dumps({'task_started':key,'id':tid}),flush=True)
     r=base.task_run(tasks[tid],OUT/key/tid,tag,calc,profile=BENCH/'profile_v2.json')
     r['delivery_completed']=bool((r.get('agent',{}).get('artifact') or {}).get('business_verified'))
     r['primary_category']=selected['primary_category'];r['end_to_end_passed']=r['passed'] and r['delivery_completed']
     base.dump(OUT/key/tid/'result.json',r)
     metrics={k:r.get(k) for k in ['id','model','instruction_type','primary_category','passed','delivery_completed','end_to_end_passed','category','elapsed_seconds','model_calls','usage','http_errors','host_exit']}
     # Infrastructure outages are not completed benchmark attempts. Keep evidence,
     # but omit metrics so a validated resume archives and reruns this task.
     if r['host_exit']!=0 or r['category'] in ('harness_error','grader_error','recalculation_error') or r.get('http_errors'):
      base.dump(OUT/key/tid/'infrastructure-failure.json',metrics)
      raise RuntimeError('Infrastructure failure; inspect last task before resuming')
     base.dump(OUT/key/tid/'metrics.json',metrics);summary=collect(OUT);base.dump(OUT/'progress.json',summary)
     record('execution_finished',current,'题目完成；grade='+str(r['passed'])+'；delivery='+str(r['delivery_completed'])+'；'+r['category'],str((OUT/key/tid/'result.json').relative_to(ROOT)),r['host_exit']);current=None
     print(json.dumps({'task_finished':key,**metrics,'progress':{m:{k:v for k,v in z.items() if k in ('completed','passed','delivered','end_to_end_passed')} for m,z in summary['models'].items()}}),flush=True)

   finally:
    base.api('generate',{'model':tag,'stream':False,'keep_alive':0});base.dump(OUT/key/'cleanup.json',base.api('ps'))
  verify_sources()
  summary=collect(OUT);base.dump(OUT/'progress.json',summary)
  if STOP:
   base.dump(OUT/'stopped.json',{'pid':os.getpid(),'summary':summary});record('execution_finished','E006-run','按停止请求结束，当前题已落盘；可按同一manifest恢复',str((OUT/'stopped.json').relative_to(ROOT)),0);return
  if any(v['completed']!=50 for v in summary['models'].values()):raise RuntimeError('incomplete paired run')
  unchanged=all(base.sha(base.DATA/p)==v for p,v in manifest['data_files_sha256'].items())
  if not unchanged:raise RuntimeError('dataset changed during evaluation')
  calc.close()
  summary=make_report(OUT,REPORT);base.dump(OUT/'complete.json',dict(summary,dataset_unchanged=unchanged,models_unloaded=not base.api('ps')['models']))
  record('execution_finished','E006-run','两模型各50题完成，报告已生成；模型卸载；数据hash不变',str(REPORT.relative_to(ROOT)),0)
  record('task_released','E006-release','E006评测完成并释放，等待结果解读；原先实验保持不变',str((OUT/'complete.json').relative_to(ROOT)))
  finish_metadata('done','E006新版harness分层50题双模型评测完成，报告已生成。')
 except BaseException as e:
  if current:record('execution_failed',current,str(e),str((OUT/'failed.json').relative_to(ROOT)),1)
  raise
 finally:calc.close()
if __name__=='__main__':
 try:main()
 except BaseException as e:
  if not OWNED:raise
  base.dump(OUT/'failed.json',{'error':repr(e),'pid':os.getpid(),'time':datetime.now(timezone.utc).isoformat()})
  record('execution_failed','E006-run','评测停止：'+repr(e),str((OUT/'failed.json').relative_to(ROOT)),1)
  raise
