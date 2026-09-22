"""Ollama loopback proxy used only by the experiment host."""
import json, threading, time, urllib.error, urllib.request
from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

@contextmanager
def proxy(trace, merge_system=False):
    lock=threading.Lock()
    class Handler(BaseHTTPRequestHandler):
        def log_message(self,*args): pass
        def do_POST(self):
            if self.path!='/v1/chat/completions': self.send_error(404); return
            started=time.monotonic();request=json.loads(self.rfile.read(int(self.headers['Content-Length'])));request.update(temperature=0,seed=7,reasoning_effort='none')
            if merge_system:
                messages=request.get('messages',[]);request['messages']=[{'role':'system','content':'\n\n'.join(m['content'] for m in messages if m.get('role')=='system')}]+[m for m in messages if m.get('role')!='system']
            try:
                req=urllib.request.Request('http://127.0.0.1:11434/v1/chat/completions',json.dumps(request).encode(),{'Content-Type':'application/json'})
                with urllib.request.urlopen(req,timeout=580) as response: body=response.read();code=response.status
            except urllib.error.HTTPError as e: code=e.code;body=e.read()
            except Exception as e: code=502;body=json.dumps({'error':{'message':str(e)}}).encode()
            try: parsed=json.loads(body)
            except ValueError: parsed={'raw':body.decode(errors='replace')}
            for choice in parsed.get('choices',[]): choice.get('message',{}).pop('reasoning',None);choice.get('message',{}).pop('reasoning_content',None)
            with lock: trace.open('a').write(json.dumps({'request':request,'response':parsed,'status':code,'seconds':time.monotonic()-started},ensure_ascii=False)+'\n')
            self.send_response(code);self.send_header('Content-Type','application/json');self.end_headers();self.wfile.write(body)
    server=ThreadingHTTPServer(('127.0.0.1',0),Handler);thread=threading.Thread(target=server.serve_forever,daemon=True);thread.start()
    try:yield f'http://127.0.0.1:{server.server_port}/v1'
    finally:server.shutdown();server.server_close();thread.join()
