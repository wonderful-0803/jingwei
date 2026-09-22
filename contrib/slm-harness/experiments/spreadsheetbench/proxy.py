"""Experiment-only loopback proxy: reproducible options and inspectable traces."""
import json,time,threading,urllib.request,urllib.error
from http.server import BaseHTTPRequestHandler,ThreadingHTTPServer
from contextlib import contextmanager

def merge_leading_system(messages):
    count=0
    while count<len(messages) and messages[count].get('role')=='system':count+=1
    if count<2:return list(messages)
    return [{'role':'system','content':'\n\n'.join(m['content'] for m in messages[:count])}]+messages[count:]

@contextmanager
def proxy(trace,merge_system=False):
    lock=threading.Lock()
    class Handler(BaseHTTPRequestHandler):
        def log_message(self,*args):pass
        def do_POST(self):
            if self.path!='/v1/chat/completions':self.send_error(404);return
            start=time.monotonic();request=json.loads(self.rfile.read(int(self.headers['Content-Length'])))
            request.update(temperature=0,seed=7,reasoning_effort='none')
            if merge_system:request['messages']=merge_leading_system(request['messages'])
            schema=request.get('response_format',{}).get('json_schema',{}).get('schema')
            if schema:
                request['messages'][0]['content']+='\nAvailable JSON actions and tool parameters:\n'+json.dumps(schema,ensure_ascii=False)
            code=200
            try:
                req=urllib.request.Request('http://127.0.0.1:11434/v1/chat/completions',json.dumps(request).encode(),{'Content-Type':'application/json'})
                with urllib.request.urlopen(req,timeout=580) as r:body=r.read()
            except urllib.error.HTTPError as e:code=e.code;body=e.read()
            except Exception as e:code=502;body=json.dumps({'error':{'message':str(e)}}).encode()
            try:response=json.loads(body)
            except ValueError:response={'raw':body.decode(errors='replace')}
            # Synthetic tasks only. No authorization headers or reasoning traces persisted.
            for choice in response.get('choices',[]):
                for key in ['reasoning','reasoning_content']:choice.get('message',{}).pop(key,None)
            with lock,open(trace,'a') as f:f.write(json.dumps({'request':request,'response':response,'status':code,'seconds':time.monotonic()-start},ensure_ascii=False)+'\n')
            self.send_response(code);self.send_header('Content-Type','application/json');self.end_headers()
            try:self.wfile.write(body)
            except BrokenPipeError:pass
    server=ThreadingHTTPServer(('127.0.0.1',0),Handler);thread=threading.Thread(target=server.serve_forever,daemon=True);thread.start()
    try:yield f'http://127.0.0.1:{server.server_port}/v1'
    finally:server.shutdown();server.server_close();thread.join()
