#!/usr/bin/env python3
"""Start the workbench and optionally a dedicated CUDA llama.cpp server."""
import argparse
import pathlib
import socket
import signal
import subprocess
import sys
import time
import urllib.request

HERE = pathlib.Path(__file__).resolve().parent
ROOT = HERE.parents[1]
p = argparse.ArgumentParser(description=__doc__)
p.add_argument('--cuda-model', type=pathlib.Path)
p.add_argument('--llama-server', default=str(pathlib.Path.home() / '.local/share/luxiclaw-runtime/llama/bin/llama-server'))
p.add_argument('--port', type=int, default=3080)
p.add_argument('--model-port', type=int, default=18088)
a = p.parse_args()
def stop_launcher(_signum, _frame):
    raise KeyboardInterrupt
signal.signal(signal.SIGTERM, stop_launcher)
children = []
log = None
try:
    ports = [a.port] + ([a.model_port] if a.cuda_model else [])
    for port in ports:
        with socket.socket() as sock:
            sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            sock.bind(('127.0.0.1', port))
    if a.cuda_model:
        if not a.cuda_model.is_file():
            p.error('Model file does not exist')
        devices = subprocess.check_output([a.llama_server, '--list-devices'], text=True)
        if 'CUDA0' not in devices:
            p.error('CUDA0 unavailable; no CPU fallback')
        data = ROOT / 'results/evaluations/workbench'
        data.mkdir(parents=True, exist_ok=True)
        log = (data / f'model-{int(time.time())}.log').open('w')
        model = subprocess.Popen([a.llama_server, '--model', str(a.cuda_model.resolve()),
            '--alias', 'qwen38-4b-local', '--device', 'CUDA0', '--gpu-layers', '999',
            '--ctx-size', '16384', '--parallel', '1', '--host', '127.0.0.1',
            '--port', str(a.model_port), '--jinja', '--reasoning', 'off', '--temp', '0', '--seed', '42'],
            stdout=log, stderr=subprocess.STDOUT)
        children.append(model)
        for _ in range(120):
            if model.poll() is not None:
                raise RuntimeError('Model exited; inspect workbench/model-*.log')
            try:
                with urllib.request.urlopen(f'http://127.0.0.1:{a.model_port}/health', timeout=2) as response:
                    if response.status == 200:
                        break
            except Exception:
                time.sleep(1)
        else:
            raise RuntimeError('Model startup timed out')
        print('CUDA model ready', flush=True)
    ui = subprocess.Popen([sys.executable, str(HERE / 'server.py'), '--port', str(a.port),
                           '--upstream', f'http://127.0.0.1:{a.model_port}'])
    children.append(ui)
    ui.wait()
except KeyboardInterrupt:
    pass
finally:
    for child in reversed(children):
        if child.poll() is None:
            child.terminate()
            try:
                child.wait(timeout=10)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait()
    if log:
        log.close()
