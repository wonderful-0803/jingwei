#!/usr/bin/env python3
"""Local Jingwei workbench. Runs the Rust Harness; Python only serves UI and observes transport."""
import argparse
import copy
import datetime
import json
import pathlib
import secrets
import signal
import subprocess
import threading
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

HERE = pathlib.Path(__file__).resolve().parent
REPO = HERE.parents[1]
LOCK = threading.RLock()
RUNS = {}
TOKEN = secrets.token_urlsafe(32)
ARGS = None
SERVER = None


def read_json(path, default=None):
    try:
        return json.loads(path.read_text())
    except (OSError, ValueError):
        return default


def save(path, value):
    temp = path.with_suffix('.tmp')
    temp.write_text(json.dumps(value, ensure_ascii=False, indent=2))
    temp.replace(path)


def upstream(path, data=None, timeout=35):
    body = None if data is None else json.dumps(data).encode()
    request = urllib.request.Request(ARGS.upstream.rstrip('/') + path, data=body,
                                     headers={'Content-Type': 'application/json'})
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            return response.status, response.read(16 * 1024 * 1024)
    except urllib.error.HTTPError as error:
        return error.code, error.read(1024 * 1024)


def events_for(run):
    events = []
    for file in sorted((run['path'] / 'output').rglob('*.jsonl')):
        if file.name == 'results.jsonl':
            continue
        try:
            for line in file.read_text().splitlines():
                try:
                    event = json.loads(line)
                    if 'event_id' in event:
                        events.append(event)
                except ValueError:
                    pass  # Last line may still be being appended.
        except OSError:
            pass
    return sorted(events, key=lambda e: e['seq'])


def snapshot(run, full=True):
    data = {k: v for k, v in run.items() if k not in ('process', 'path')}
    data['result'] = None
    if full:
        data['model_service'] = read_json(run['path'] / 'model-service.json')
    try:
        lines = (run['path'] / 'output/results.jsonl').read_text().splitlines()
        if lines:
            data['result'] = json.loads(lines[0])
    except (OSError, ValueError):
        pass
    if full:
        data['events'] = events_for(run)
        data['transport'] = read_json(run['path'] / 'transport.json', [])
        data['log'] = (run['path'] / 'runner.log').read_text()[-12000:] if (run['path'] / 'runner.log').exists() else ''
    return data


def worker(run):
    try:
        if run['config']['backend'] == 'openai':
            try:
                status, body = upstream('/props', timeout=3)
                save(run['path'] / 'model-service.json', {'status': status, 'props': json.loads(body)})
            except Exception as error:
                save(run['path'] / 'model-service.json', {'unavailable': str(error)})
        with (run['path'] / 'runner.log').open('w') as log:
            with LOCK:
                if run['status'] == 'interrupted':
                    return
                process = subprocess.Popen([str(REPO / 'target/debug/jw-eval'), '--config',
                    str(run['path'] / 'config.json'), '--output', str(run['path'] / 'output')],
                    cwd=REPO, stdout=log, stderr=subprocess.STDOUT)
                run['process'] = process
            code = process.wait()
        with LOCK:
            if run['status'] != 'interrupted':
                run['status'] = 'finished' if code in (0, 1) else 'error'
            run['exit_code'] = code
    except Exception as error:
        run['status'] = 'error'
        run['error'] = str(error)
    finally:
        run['finished_at'] = time.time()
        save(run['path'] / 'run.json', {k: v for k, v in run.items() if k not in ('process', 'path')})


def create_run(data):
    if any(r['status'] == 'running' for r in RUNS.values()):
        raise ValueError('已有任务运行中，请等待或中断后再开始。')
    cases = read_json(REPO / 'tests/model-protocol/eval/pilot.json')
    chosen = next((c for c in cases if c['id'] == data.get('case_id')), None)
    if chosen is None:
        raise ValueError('未知的任务场景。')
    case = copy.deepcopy(chosen)
    for key in ('prompt', 'initial', 'expected', 'writable'):
        if key in data:
            case[key] = data[key]
    if not isinstance(case['prompt'], str) or not case['prompt'].strip() or len(case['prompt']) > 16000:
        raise ValueError('请输入不超过 16000 字符的任务。')
    for key in ('initial', 'expected'):
        if not isinstance(case[key], dict) or not all(isinstance(k, str) and isinstance(v, str) for k, v in case[key].items()):
            raise ValueError(key + ' 必须是字符串键值 JSON 对象。')
    if not case['expected'] or not isinstance(case['writable'], list) or not all(isinstance(k, str) for k in case['writable']):
        raise ValueError('请填写预期状态与允许写入的字段列表。')
    config = read_json(REPO / 'tests/model-protocol/eval/fake.json')
    backend = data.get('backend', 'openai')
    if backend not in ('fake', 'openai') or data.get('protocol') not in ('json', 'native'):
        raise ValueError('无效的模型后端或协议。')
    steps = int(data.get('max_steps', 6))
    tokens = int(data.get('max_tokens', 512))
    if not 1 <= steps <= 64 or not 1 <= tokens <= 4096:
        raise ValueError('步数范围 1–64；输出 Token 范围 1–4096。')
    model = data.get('model', ARGS.model_alias)
    if not isinstance(model, str) or not model.strip() or len(model) > 200:
        raise ValueError('无效的模型名称。')
    ident = datetime.datetime.now().strftime('%Y%m%d-%H%M%S-') + secrets.token_hex(3)
    path = ARGS.data / ident
    path.mkdir(parents=True)
    config.update(backend=backend, model=model, protocol=data['protocol'], max_steps=steps,
                  max_tokens=tokens, tasks='task.json', endpoint=None if backend == 'fake' else
                  f'http://127.0.0.1:{ARGS.port}/bridge/{ident}/v1')
    if backend == 'openai':
        config.update(weights='host-managed; see model-service.json', quantization='host-managed',
                      device='host-managed', template_revision='host-managed; see model-service.json')
    save(path / 'task.json', [case])
    save(path / 'config.json', config)
    run = dict(id=ident, title=case['prompt'][:60], status='running', started_at=time.time(),
               config=config, case=case, merge_system=bool(data.get('merge_system', True)),
               inject_schema=bool(data.get('inject_schema', False)), path=path)
    RUNS[ident] = run
    save(path / 'run.json', {k: v for k, v in run.items() if k != 'path'})
    threading.Thread(target=worker, args=(run,), daemon=True).start()
    return {'id': ident}


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def reply(self, status, value, content_type='application/json'):
        body = value if isinstance(value, bytes) else json.dumps(value, ensure_ascii=False).encode()
        self.send_response(status)
        self.send_header('Content-Type', content_type)
        self.send_header('Content-Length', str(len(body)))
        self.send_header('Cache-Control', 'no-store')
        self.send_header('X-Content-Type-Options', 'nosniff')
        self.send_header('Content-Security-Policy', "default-src 'self'; script-src 'self'; style-src 'self'; connect-src 'self'; frame-ancestors 'none'")
        self.end_headers()
        try:
            self.wfile.write(body)
        except (BrokenPipeError, ConnectionResetError):
            pass

    def allowed(self):
        hosts = {f'127.0.0.1:{ARGS.port}', f'localhost:{ARGS.port}'}
        return self.headers.get('Host') in hosts and self.headers.get('Origin') in (None, *(f'http://{h}' for h in hosts))

    def do_GET(self):
        if not self.allowed():
            return self.reply(403, {'error': 'Local origin required'})
        route = self.path.split('?')[0]
        if route in ('/', '/app.js', '/style.css'):
            name = 'index.html' if route == '/' else route[1:]
            mime = {'index.html': 'text/html; charset=utf-8', 'app.js': 'text/javascript; charset=utf-8', 'style.css': 'text/css; charset=utf-8'}[name]
            return self.reply(200, (HERE / name).read_bytes(), mime)
        if route == '/api/bootstrap':
            return self.reply(200, {'token': TOKEN, 'cases': read_json(REPO / 'tests/model-protocol/eval/pilot.json'),
                                    'model': ARGS.model_alias, 'upstream': ARGS.upstream})
        if route == '/api/health':
            try:
                status, body = upstream('/health', timeout=2)
                return self.reply(200, {'ready': status == 200, 'detail': body.decode()})
            except Exception as error:
                return self.reply(200, {'ready': False, 'detail': str(error)})
        with LOCK:
            if route == '/api/runs':
                return self.reply(200, [snapshot(r, False) for r in reversed(list(RUNS.values()))])
            ident = route.removeprefix('/api/runs/')
            if route.startswith('/api/runs/') and ident in RUNS:
                return self.reply(200, snapshot(RUNS[ident]))
        self.reply(404, {'error': 'Not found'})

    def do_POST(self):
        if not self.allowed():
            return self.reply(403, {'error': 'Local origin required'})
        try:
            length = int(self.headers.get('Content-Length', '0'))
            if not 0 < length <= 1024 * 1024:
                return self.reply(413, {'error': 'Request limit: 1 MiB'})
            data = json.loads(self.rfile.read(length))
            if not isinstance(data, dict):
                raise ValueError('Expected JSON object')
            if self.path.startswith('/bridge/'):
                # Bridge is used by the local Rust process, never by browser scripts.
                if self.headers.get('Origin') is not None:
                    return self.reply(403, {'error': 'Bridge is not a browser API'})
                return self.bridge(data)
            if self.headers.get('X-Jingwei-Token') != TOKEN:
                return self.reply(403, {'error': 'Refresh this local page'})
            with LOCK:
                if self.path == '/api/runs':
                    return self.reply(201, create_run(data))
                if self.path.endswith('/stop'):
                    ident = self.path.split('/')[-2]
                    run = RUNS.get(ident)
                    if run and run['status'] == 'running':
                        run['status'] = 'interrupted'
                        if run.get('process'):
                            run['process'].terminate()
                        return self.reply(200, {'status': 'interrupted'})
            self.reply(404, {'error': 'Not found'})
        except (ValueError, TypeError) as error:
            self.reply(400, {'error': str(error)})
        except Exception as error:
            self.reply(500, {'error': str(error)})

    def bridge(self, data):
        parts = self.path.split('/')
        if len(parts) != 6 or parts[3:] != ['v1', 'chat', 'completions']:
            return self.reply(404, {'error': 'Unknown model route'})
        with LOCK:
            run = RUNS.get(parts[2])
            if not run or run['status'] != 'running':
                return self.reply(409, {'error': 'Run is not active'})
        outgoing = copy.deepcopy(data)
        messages = outgoing.get('messages', [])
        schema = outgoing.get('response_format', {}).get('json_schema', {}).get('schema')
        if run['inject_schema'] and schema:
            messages.insert(0, {'role': 'system', 'content': 'Available actions and tools (JSON Schema):\n' + json.dumps(schema, ensure_ascii=False)})
        if run['merge_system']:
            leading = []
            while messages and messages[0].get('role') == 'system':
                leading.append(messages.pop(0)['content'])
            if leading:
                messages.insert(0, {'role': 'system', 'content': '\n\n'.join(leading)})
        record = {'index': len(read_json(run['path'] / 'transport.json', [])) + 1,
                  'started_at': time.time(), 'before': data, 'sent': outgoing,
                  'rendered': None, 'render_note': 'Not yet requested'}
        records = read_json(run['path'] / 'transport.json', [])
        records.append(record)
        save(run['path'] / 'transport.json', records)
        try:
            template_body = {'messages': messages, 'add_generation_prompt': True}
            if outgoing.get('tools'):
                template_body['tools'] = outgoing['tools']
            status, body = upstream('/apply-template', template_body, timeout=5)
            record['rendered'] = json.loads(body) if status == 200 else None
            record['render_note'] = 'llama.cpp /apply-template response; diagnostic rendering, not token capture' if status == 200 else f'Template inspection unavailable: HTTP {status}'
        except Exception as error:
            record['render_note'] = str(error)
        try:
            status, body = upstream('/v1/chat/completions', outgoing)
            record['http_status'] = status
            try:
                record['response'] = json.loads(body)
            except ValueError:
                record['response'] = {'raw': body.decode(errors='replace')}
        except Exception as error:
            status, body = 502, json.dumps({'error': str(error)}).encode()
            record['http_status'], record['response'] = status, {'error': str(error)}
        record['elapsed_ms'] = round((time.time() - record['started_at']) * 1000)
        save(run['path'] / 'transport.json', records)
        self.reply(status, body)


def main():
    global ARGS, SERVER
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--port', type=int, default=3080)
    parser.add_argument('--upstream', default='http://127.0.0.1:18088')
    parser.add_argument('--model-alias', default='qwen38-4b-local')
    parser.add_argument('--data', type=pathlib.Path, default=REPO / 'results/evaluations/workbench')
    ARGS = parser.parse_args()
    ARGS.data = ARGS.data.resolve()
    ARGS.data.mkdir(parents=True, exist_ok=True)
    for file in sorted(ARGS.data.glob('*/run.json')):
        run = read_json(file)
        if run:
            run['path'] = file.parent
            if run['status'] == 'running':
                run['status'] = 'interrupted'
            RUNS[run['id']] = run
    if not (REPO / 'target/debug/jw-eval').exists():
        parser.error('Build jw-eval first; see README.md')
    SERVER = ThreadingHTTPServer(('127.0.0.1', ARGS.port), Handler)
    def stop_server(_signum, _frame):
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM, stop_server)
    print(f'Jingwei Harness: http://127.0.0.1:{ARGS.port}', flush=True)
    try:
        SERVER.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        for run in RUNS.values():
            if run.get('process') and run['process'].poll() is None:
                run['process'].terminate()
        SERVER.server_close()


if __name__ == '__main__':
    main()
