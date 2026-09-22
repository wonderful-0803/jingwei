"""Real host, tool worker, checker and protocol; scripted model is the only double."""
import json,subprocess,tempfile,threading,unittest
from pathlib import Path
from http.server import ThreadingHTTPServer,BaseHTTPRequestHandler
ROOT=Path(__file__).resolve().parents[2]
BINARY=ROOT/'jingwei/target/debug/jingwei-bench-runner'
FINAL={'action':'final','text':'done'}
SUBMIT={'action':'call_tool','name':'submit_text','arguments':{'content':'ALPHA\nBETA\n'}}
class HostTests(unittest.TestCase):
 def run_host(self,actions):
  requests=[]
  class Handler(BaseHTTPRequestHandler):
   def log_message(self,*args):pass
   def do_POST(self):
    request=json.loads(self.rfile.read(int(self.headers['Content-Length'])));requests.append(request)
    index=len(requests)-1
    action=actions[index] if index<len(actions) else FINAL
    body=json.dumps({'id':'fake-'+str(index),'object':'chat.completion','created':0,'model':'test','choices':[{'index':0,'message':{'role':'assistant','content':json.dumps(action)},'finish_reason':'stop'}],'usage':{'prompt_tokens':100,'completion_tokens':30,'total_tokens':130}}).encode()
    self.send_response(200);self.send_header('Content-Type','application/json');self.end_headers();self.wfile.write(body)
  server=ThreadingHTTPServer(('127.0.0.1',0),Handler);thread=threading.Thread(target=server.serve_forever);thread.start()
  try:
   with tempfile.TemporaryDirectory() as temp:
    root=Path(temp);(root/'input.txt').write_text('alpha\nbeta\n')
    config={'profile':str(ROOT/'experiments/harness-reliability/text/profile.json'),'task_dir':temp,'worker_args':[temp],'endpoint':f'http://127.0.0.1:{server.server_port}/v1','model':'test','prompt':'Uppercase the input and save it.'}
    (root/'config.json').write_text(json.dumps(config))
    run=subprocess.run([str(BINARY),str(root/'config.json')],capture_output=True,text=True,timeout=30)
    self.assertEqual(run.returncode,0,run.stderr)
    report=json.loads((root/'agent-report.json').read_text())
    output=(root/'answer.txt').read_text() if (root/'answer.txt').exists() else None
    events=[json.loads(line) for p in (root/'journal').glob('*.jsonl') for line in p.read_text().splitlines()]
    return report,output,requests,events
  finally:server.shutdown();server.server_close();thread.join()
 def test_premature_final_is_repaired_and_delivered_in_same_host(self):
  report,output,requests,events=self.run_host([FINAL,SUBMIT,FINAL])
  self.assertTrue(report['runtime_ok']);self.assertTrue(report['artifact']['business_verified'])
  self.assertEqual(output,'ALPHA\nBETA\n');self.assertEqual(len(requests),3)
  self.assertIn('CompletionRejected',json.dumps(events))
  self.assertEqual(report['visible_tools'],['inspect_text','submit_text'])
 def test_claim_alone_exhausts_correction_budget_without_delivery(self):
  report,output,requests,events=self.run_host([FINAL]*3)
  self.assertFalse(report['runtime_ok']);self.assertFalse(report['receipt']);self.assertIsNone(output)
  self.assertEqual(len(requests),3);self.assertIn('CorrectionLimit',json.dumps(events))
 def test_inspection_then_submission_uses_same_generic_flow(self):
  report,output,requests,_=self.run_host([{'action':'call_tool','name':'inspect_text','arguments':{}},SUBMIT,FINAL])
  self.assertTrue(report['artifact']['business_verified']);self.assertEqual(output,'ALPHA\nBETA\n')
  self.assertNotIn('inspect_text',json.dumps(requests[1]['response_format']))
if __name__=='__main__':unittest.main()
