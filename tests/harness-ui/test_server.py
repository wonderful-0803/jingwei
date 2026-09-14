"""Focused local integration tests; temporary mock model, no GPU or real inference."""
import json
import pathlib
import socket
import subprocess
import sys
import tempfile
import threading
import time
import unittest
import urllib.request
import urllib.error
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

HERE = pathlib.Path(__file__).resolve().parent
class Workbench(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.temp = tempfile.TemporaryDirectory(prefix='jw-ui-test-')
        class Model(BaseHTTPRequestHandler):
            def log_message(self, *_): pass
            def do_GET(self):
                self.send_response(200); self.end_headers(); self.wfile.write(b'{}')
            def do_POST(self):
                request = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
                if self.path == '/apply-template':
                    body = {'prompt': json.dumps(request['messages'])}
                else:
                    body = {'choices':[{'index':0,'message':{'role':'assistant','content':'{"action":"final","text":"done"}'},'finish_reason':'stop'}],
                            'usage':{'prompt_tokens':10,'completion_tokens':5,'total_tokens':15}}
                self.send_response(200); self.end_headers(); self.wfile.write(json.dumps(body).encode())
        cls.model = ThreadingHTTPServer(('127.0.0.1',0), Model)
        threading.Thread(target=cls.model.serve_forever,daemon=True).start()
        with socket.socket() as sock:
            sock.bind(('127.0.0.1',0)); cls.port=sock.getsockname()[1]
        cls.base=f'http://127.0.0.1:{cls.port}'
        cls.proc=subprocess.Popen([sys.executable,str(HERE/'server.py'),'--port',str(cls.port),'--upstream',f'http://127.0.0.1:{cls.model.server_port}','--data',cls.temp.name],stdout=subprocess.DEVNULL)
        for _ in range(50):
            try:
                cls.boot=cls.request('/api/bootstrap');break
            except OSError:time.sleep(.1)
        else:raise RuntimeError('UI did not start')
    @classmethod
    def tearDownClass(cls):
        cls.proc.terminate();cls.proc.wait(timeout=5);cls.model.shutdown();cls.model.server_close();cls.temp.cleanup()
    @classmethod
    def request(cls,path,data=None,headers=None):
        h={'Content-Type':'application/json','X-Jingwei-Token':getattr(cls,'boot',{}).get('token','')}
        h.update(headers or {})
        r=urllib.request.Request(cls.base+path,data=None if data is None else json.dumps(data).encode(),headers=h)
        with urllib.request.urlopen(r,timeout=5) as response:return json.loads(response.read())
    def run_task(self,**options):
        data={'case_id':'doc-copy','backend':'fake','protocol':'json'};data.update(options)
        ident=self.request('/api/runs',data)['id']
        for _ in range(100):
            run=self.request('/api/runs/'+ident)
            if run['status']!='running':return run
            time.sleep(.05)
        self.fail('run timeout')
    def test_fake_uses_canonical_events_and_verifies_state(self):
        run=self.run_task()
        self.assertTrue(run['result']['passed'])
        self.assertIn('task_run_report',[e['kind']['type'] for e in run['events']])
        self.assertFalse(run['transport'])
    def test_transport_preserves_original_and_marks_experiment(self):
        run=self.run_task(backend='openai',inject_schema=True,merge_system=True)
        self.assertFalse(run['result']['passed'])
        t=run['transport'][0]
        self.assertEqual(t['http_status'],200)
        self.assertNotIn('Available actions and tools',json.dumps(t['before']['messages']))
        self.assertIn('Available actions and tools',t['sent']['messages'][0]['content'])
        self.assertIn('write_record',t['rendered']['prompt'])
        self.assertEqual(sum(m['role']=='system' for m in t['sent']['messages']),1)
    def test_rejects_foreign_origin_missing_token_and_bad_input(self):
        for data,headers,status in [({}, {'Origin':'http://evil.test'},403),({}, {'X-Jingwei-Token':''},403),({'case_id':'missing'}, {},400)]:
            with self.assertRaises(urllib.error.HTTPError) as raised:self.request('/api/runs',data,headers)
            self.assertEqual(raised.exception.code,status)
            raised.exception.close()

if __name__=='__main__':unittest.main()
