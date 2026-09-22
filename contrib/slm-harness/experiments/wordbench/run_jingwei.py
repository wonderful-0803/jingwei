"""Run the frozen Word plugin through jingwei's real Agent/Tool host."""
import argparse, fcntl, hashlib, json, os, shutil, signal, subprocess, sys, time
from pathlib import Path
from datetime import datetime, timezone
from grader import grade
from render import render_docx
from tasks import build_task_specs
from proxy import proxy
from sync_state import record

HERE=Path(__file__).resolve().parent; ROOT=HERE.parents[1]
BINARY=ROOT/'jingwei/target/debug/jingwei-bench-runner'
PROFILE=HERE/'word_profile_v2.json'; FIXTURES=ROOT/'artifacts/wordbench-v2/tasks'
MODELS={'9b':'hf.co/unsloth/Qwen3.5-9B-GGUF:Q8_0','27b':'hf.co/unsloth/Qwen3.8-27B-GGUF:UD-IQ3_S'}

def dump(path,value):
    path=Path(path);path.parent.mkdir(parents=True,exist_ok=True);tmp=path.with_suffix('.tmp');tmp.write_text(json.dumps(value,ensure_ascii=False,indent=2,default=str));tmp.replace(path)
def api(path,payload=None):
    import urllib.request
    req=urllib.request.Request('http://127.0.0.1:11434/api/'+path,None if payload is None else json.dumps(payload).encode(),{'Content-Type':'application/json'})
    with urllib.request.urlopen(req,timeout=600) as r:return json.load(r)
def sha(path):return hashlib.sha256(Path(path).read_bytes()).hexdigest()

def task_prompt(spec):
    return ('Edit the DOCX task according to this instruction. Input file is /input/input.docx. '
            'Use inspect_docx once, then submit_docx_operations with the smallest valid JSON operations. '
            'Finish after a successful submission. Never claim completion without using the submit tool.\n\n'
            'Instruction: '+spec['prompt'])

def one(spec,key,model,folder):
    folder.mkdir(parents=True,exist_ok=True); shutil.copyfile(FIXTURES/spec['id']/'input.docx',folder/'input.docx')
    with proxy(folder/'llm-trace.jsonl',merge_system=True) as endpoint:
        config={'profile':str(PROFILE),'task_dir':str(folder),'worker_args':[str(folder)],'endpoint':endpoint,'model':model,'prompt':task_prompt(spec)};dump(folder/'config.json',config)
        with (folder/'host.log').open('wb') as log:
            proc=subprocess.run([str(BINARY),str(folder/'config.json')],stdout=log,stderr=subprocess.STDOUT,timeout=900)
    result={'id':spec['id'],'model':model,'host_exit':proc.returncode,'passed':False}
    report=folder/'agent-report.json'
    if report.exists():result['agent']=json.loads(report.read_text())
    output=folder/'output.docx'
    if output.exists():
        rendered=render_docx(output,folder/'render');result.update(grade(spec,folder/'input.docx',FIXTURES/spec['id']/'gold.docx',output,rendered));result['render']=rendered
        result['passed']=bool(result['artifact_valid'] and result['structure_passed'] and result['content_passed'] and result['render_passed']);result['category']='passed' if result['passed'] else 'wrong_answer'
    else:result['category']='no_submission'
    result['model_calls']=sum(1 for line in (folder/'llm-trace.jsonl').read_text().splitlines() if line) if (folder/'llm-trace.jsonl').exists() else 0
    dump(folder/'result.json',result);return result

def main():
    global FIXTURES
    ap=argparse.ArgumentParser();ap.add_argument('--out',type=Path,required=True);ap.add_argument('--fixtures',type=Path,default=FIXTURES);ap.add_argument('--limit',type=int,default=6);ap.add_argument('--models',default='9b,27b');args=ap.parse_args()
    FIXTURES=args.fixtures.resolve();args.out=args.out.resolve();args.out.mkdir(parents=True,exist_ok=True)
    specs=build_task_specs()[:args.limit];tags={m['name']:m for m in api('tags')['models']}
    dump(args.out/'manifest.json',{'created_utc':datetime.now(timezone.utc).isoformat(),'mode':'jingwei','task_ids':[s['id'] for s in specs],'models':{k:{'name':MODELS[k],'digest':tags[MODELS[k]]['digest']} for k in args.models.split(',')},'profile_sha256':sha(PROFILE),'fixture_manifest_sha256':sha(FIXTURES.parent/'manifest.json'),'binary_sha256':sha(BINARY)})
    allsum={}
    for key in args.models.split(','):
        model=MODELS[key];api('generate',{'model':model,'prompt':'OK','stream':False,'think':False,'keep_alive':'30m','options':{'temperature':0,'seed':7,'num_predict':1}});rows=[]
        for spec in specs:
            record('execution_started','jingwei-'+key+'-'+spec['id'],'运行jingwei Word任务 '+key+' '+spec['id'],str((args.out/key/spec['id']).relative_to(ROOT)))
            try:r=one(spec,key,model,args.out/key/spec['id'])
            except Exception as e:r={'id':spec['id'],'model':model,'passed':False,'category':'runner_error','error':repr(e)};dump(args.out/key/spec['id']/'result.json',r)
            rows.append(r);record('execution_finished','jingwei-'+key+'-'+spec['id'],'完成 '+r.get('category','unknown'),str((args.out/key/spec['id']/'result.json').relative_to(ROOT)),0 if r.get('category')!='runner_error' else 1);print(json.dumps({'model':key,'task':spec['id'],'passed':r.get('passed',False),'category':r.get('category')},ensure_ascii=False),flush=True)
        api('generate',{'model':model,'prompt':'','stream':False,'keep_alive':0});allsum[key]={'model':model,'completed':len(rows),'passed':sum(r.get('passed',False) for r in rows),'categories':{c:sum(r.get('category')==c for r in rows) for c in sorted({r.get('category') for r in rows})}}
    summary={'created_utc':datetime.now(timezone.utc).isoformat(),'expected_per_model':len(specs),'models':allsum};dump(args.out/'summary.json',summary);print(json.dumps(summary,ensure_ascii=False,indent=2))
if __name__=='__main__':main()
