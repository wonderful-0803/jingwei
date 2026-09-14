"""Own exactly one CUDA model process; never stop externally managed services."""
import json
import pathlib
import socket
import subprocess
import threading
import time
import urllib.request


class ModelManager:
    def __init__(self, directory, binary, port, data):
        self.directory = directory.resolve()
        self.binary, self.port, self.data = binary, port, data
        self.lock = threading.RLock()
        self.process = None
        self.log = None
        self.thread = None
        self.closing = threading.Event()
        self.state = {'status': 'idle', 'current': None, 'target': None, 'error': None}

    def catalog(self):
        result, seen = [], set()
        if self.directory.is_dir():
            for file in sorted(self.directory.iterdir(), key=lambda p: p.name.lower()):
                try:
                    actual = file.resolve()
                    if file.suffix.lower() != '.gguf' or not actual.is_relative_to(self.directory) or not actual.is_file() or actual in seen:
                        continue
                    seen.add(actual)
                    result.append({'id': file.name, 'bytes': actual.stat().st_size})
                except OSError:
                    continue
        return result

    def snapshot(self):
        with self.lock:
            if self.state['status'] == 'ready' and self.process.poll() is not None:
                self.state.update(status='error', current=None, error='模型进程已退出，请重新加载。')
            return dict(self.state, directory=str(self.directory), models=self.catalog(), managed=True)

    def switch(self, ident):
        with self.lock:
            if self.state['status'] == 'loading' or self.closing.is_set():
                raise ValueError('模型正在切换，请稍候。')
            if ident not in {item['id'] for item in self.catalog()}:
                raise ValueError('请选择模型目录中存在的 GGUF 文件。')
            if self.snapshot()['status'] == 'ready' and self.state['current'] == ident:
                return
            self.state.update(status='loading', target=ident, error=None)
            self.thread = threading.Thread(target=self._load, args=(ident,), daemon=True)
            self.thread.start()

    def _stop(self):
        if self.process and self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait()
        if self.log:
            self.log.close()
        self.process = None
        self.log = None

    def _load(self, ident):
        try:
            devices = subprocess.check_output([self.binary, '--list-devices'], text=True, timeout=15)
            if 'CUDA0' not in devices:
                raise RuntimeError('CUDA0 不可用；未降级到 CPU。')
            self._stop()
            with self.lock:
                self.state['current'] = None
            if self.closing.is_set():
                return
            with socket.socket() as sock:
                sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
                sock.bind(('127.0.0.1', self.port))
            logfile = self.data / f'model-{time.time_ns()}.log'
            self.log = logfile.open('w')
            self.state['log'] = logfile.name
            self.process = subprocess.Popen([self.binary, '--model', str(self.directory / ident),
                '--alias', ident, '--device', 'CUDA0', '--gpu-layers', '999', '--ctx-size', '16384',
                '--parallel', '1', '--host', '127.0.0.1', '--port', str(self.port), '--jinja',
                '--reasoning', 'off', '--temp', '0', '--seed', '42'], stdout=self.log, stderr=subprocess.STDOUT)
            for _ in range(120):
                if self.closing.is_set():
                    return
                if self.process.poll() is not None:
                    raise RuntimeError('模型加载失败；可能格式不兼容、显存不足或不是对话模型。')
                try:
                    with urllib.request.urlopen(f'http://127.0.0.1:{self.port}/health', timeout=2) as response:
                        if response.status == 200:
                            with self.lock:
                                self.state.update(status='ready', current=ident, target=None)
                            (self.data / 'last-model.json').write_text(json.dumps({'model': ident}))
                            return
                except OSError:
                    pass
                self.closing.wait(1)
            raise RuntimeError('模型加载超时。')
        except Exception as error:
            self._stop()
            with self.lock:
                self.state.update(status='error', current=None, error=str(error))
        finally:
            if self.closing.is_set():
                self._stop()

    def close(self):
        self.closing.set()
        if self.thread:
            self.thread.join(timeout=30)
        if not self.thread or not self.thread.is_alive():
            self._stop()
