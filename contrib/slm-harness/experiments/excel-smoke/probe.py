from pathlib import Path
import json,urllib.request,time
root=Path(__file__).resolve().parents[2]/'artifacts/excel-smoke'
original=json.loads((root/'run-01/model-empty/llm-trace.jsonl').read_text().splitlines()[0])['request']
results=[]
for name in ['merged','catalog','merged_catalog']:
    r=json.loads(json.dumps(original))
    if 'merged' in name:
        system='\n\n'.join(m['content'] for m in r['messages'] if m['role']=='system')
        r['messages']=[{'role':'system','content':system}]+[m for m in r['messages'] if m['role']!='system']
    if 'catalog' in name:
        r['messages'][0]['content']+='\nAvailable JSON actions and tool parameters:\n'+json.dumps(r['response_format']['json_schema']['schema'],ensure_ascii=False)
    t=time.monotonic();req=urllib.request.Request('http://127.0.0.1:11434/v1/chat/completions',json.dumps(r).encode(),{'Content-Type':'application/json'})
    with urllib.request.urlopen(req,timeout=60) as response:res=json.load(response)
    results.append({'variant':name,'request':r,'response':res,'seconds':time.monotonic()-t})
    (root/'prompt-probes.json').write_text(json.dumps(results,ensure_ascii=False,indent=2))
    print(name,res.get('usage'),res['choices'][0]['message']['content'],flush=True)
