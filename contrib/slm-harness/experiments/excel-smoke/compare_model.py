"""Run the unchanged E001 suite with a selected model; warm up and unload only it."""
import argparse,hashlib,json,subprocess,sys,time,urllib.request
from pathlib import Path
import evaluate
from functools import partial
from proxy import proxy

def api(path,payload=None):
    data=None if payload is None else json.dumps(payload).encode()
    req=urllib.request.Request('http://127.0.0.1:11434/api/'+path,data,{'Content-Type':'application/json'})
    with urllib.request.urlopen(req,timeout=180) as r:return json.load(r)

def gpu():
    return subprocess.check_output(['nvidia-smi','--query-gpu=name,memory.used,memory.total,driver_version','--format=csv,noheader'],text=True).strip()

def main():
    parser=argparse.ArgumentParser();parser.add_argument('--model',required=True);parser.add_argument('--out',required=True);args=parser.parse_args()
    out=Path(args.out).resolve();out.mkdir(parents=True,exist_ok=False)
    existing=api('ps')['models']
    if existing:raise RuntimeError('Expected idle empty Ollama; leave other loaded models untouched')
    if args.model not in [m['name'] for m in api('tags')['models']]:raise ValueError('Model not installed')
    evaluate.MODEL=args.model
    evaluate.proxy=partial(proxy,merge_system=True)
    hashes={str(p.relative_to(evaluate.HERE)):hashlib.sha256(p.read_bytes()).hexdigest() for p in evaluate.HERE.rglob('*') if p.is_file() and (p.suffix in ('.py','.json','.rs','.md','.toml','.lock'))}
    metadata={'model':args.model,'source_sha256':hashes,'binary_sha256':hashlib.sha256((evaluate.ROOT/'jingwei/target/debug/jingwei-smoke-runner').read_bytes()).hexdigest(),'ollama_version':api('version'),'gpu_before':gpu(),'sampling':{'temperature':0,'seed':7,'reasoning_effort':'none'},'merge_leading_system':True,'max_tokens':1024,'max_steps':12,'cases':list(evaluate.CASES)+['text'],'time':time.strftime('%Y-%m-%dT%H:%M:%SZ',time.gmtime())}
    evaluate.dump(out/'environment.json',metadata)
    try:
        start=time.monotonic()
        warm=api('generate',{'model':args.model,'prompt':'Reply OK.','stream':False,'think':False,'options':{'num_predict':1,'temperature':0,'seed':7,'num_ctx':32768}})
        metadata['warmup_seconds']=time.monotonic()-start
        metadata['warmup']={k:v for k,v in warm.items() if k not in ('thinking','response','context')}
        metadata['ollama_ps_loaded']=api('ps');metadata['gpu_loaded']=gpu();evaluate.dump(out/'environment.json',metadata)
        print(json.dumps({'event':'warmed','model':args.model,'seconds':metadata['warmup_seconds'],'gpu':metadata['gpu_loaded'],'loaded':metadata['ollama_ps_loaded']},ensure_ascii=False),flush=True)
        # Existing evaluator and exact prompts/tools/gold answers remain unchanged.
        sys.argv=[str(evaluate.HERE/'evaluate.py'),'--out',str(out),'--mode','model','--cases',','.join(metadata['cases'])]
        return evaluate.main()
    finally:
        api('generate',{'model':args.model,'stream':False,'keep_alive':0})
        evaluate.dump(out/'cleanup.json',{'ollama_ps':api('ps'),'gpu_after':gpu()})
if __name__=='__main__':raise SystemExit(main())
