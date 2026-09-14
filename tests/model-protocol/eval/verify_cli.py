"""Targeted JW-08-a CLI/HTTP integration check; standard library, no real model."""
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, HTTPServer

binary = Path(sys.argv[1]).resolve()
fixtures = Path(__file__).resolve().parent
config = json.loads((fixtures / 'fake.json').read_text())
tasks = json.loads((fixtures / 'pilot.json').read_text())

with tempfile.TemporaryDirectory(prefix='jw08-cli-') as temp:
    root = Path(temp)
    (root / 'pilot.json').write_text(json.dumps(tasks))

    def run(name, expected_exit, *extra):
        (root / 'config.json').write_text(json.dumps(config))
        result = subprocess.run(
            [str(binary), '--config', str(root / 'config.json'),
             '--output', str(root / name), *extra],
            capture_output=True, text=True, timeout=60)
        assert result.returncode == expected_exit, (result.stdout, result.stderr)
        return root / name

    output = run('all', 0)
    assert json.loads((output / 'summary.json').read_text())['passed'] == 4
    before = (output / 'results.jsonl').read_bytes()
    run('all', 2)
    assert (output / 'results.jsonl').read_bytes() == before
    run('unknown', 2, '--case', 'not-a-case')
    assert not (root / 'unknown').exists()
    selected = run('selected', 0, '--case', 'doc-extract')
    assert json.loads((selected / 'summary.json').read_text())['completed'] == 1

    # A false completion must not abort or disappear from the aggregate report.
    tasks[0]['fake_actions'] = [{'action': 'final', 'text': 'done'}]
    (root / 'pilot.json').write_text(json.dumps(tasks))
    failed = run('failed', 1)
    summary = json.loads((failed / 'summary.json').read_text())
    assert summary['completed'] == 4 and summary['failed'] == 1
    tasks = json.loads((fixtures / 'pilot.json').read_text())
    (root / 'pilot.json').write_text(json.dumps(tasks))

    requests = []

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_POST(self):
            request = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
            requests.append((self.path, request))
            action = tasks[0]['fake_actions'][len(requests) - 1]
            data = json.dumps({
                'choices': [{'index': 0, 'message': {'role': 'assistant',
                    'content': json.dumps(action)}, 'finish_reason': 'stop'}],
                'usage': {'prompt_tokens': 10, 'completion_tokens': 5, 'total_tokens': 15},
            }).encode()
            self.send_response(200)
            self.send_header('Content-Type', 'application/json')
            self.send_header('Content-Length', str(len(data)))
            self.end_headers()
            self.wfile.write(data)

    server = HTTPServer(('127.0.0.1', 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        config.update(backend='openai', endpoint=f'http://127.0.0.1:{server.server_port}/v1', model='mock-http')
        live = run('http', 0, '--case', 'doc-copy')
        assert len(requests) == 3
        for path, request in requests:
            assert path == '/v1/chat/completions'
            assert request['model'] == 'mock-http'
            assert request['max_tokens'] == 512
            assert request['response_format']['type'] == 'json_schema'
        assert json.loads((live / 'summary.json').read_text())['passed'] == 1
        assert 'fake_actions' not in json.dumps(requests)
    finally:
        server.shutdown()
        server.server_close()
        thread.join()
print('JW-08-a CLI checks passed: smoke, selection, no overwrite, failure retention, HTTP adapter')
