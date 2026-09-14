#!/usr/bin/env python3
"""Start a workbench owning its switchable CUDA model service."""
import argparse
import os
import pathlib
import sys

HERE = pathlib.Path(__file__).resolve().parent
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--cuda-model', type=pathlib.Path, help='Optional initial model (otherwise restore last selection)')
parser.add_argument('--models-dir', type=pathlib.Path, default=pathlib.Path.home() / '.local/share/luxiclaw-runtime/models')
parser.add_argument('--llama-server', default=str(pathlib.Path.home() / '.local/share/luxiclaw-runtime/llama/bin/llama-server'))
parser.add_argument('--port', type=int, default=3080)
parser.add_argument('--model-port', type=int, default=18088)
a = parser.parse_args()
command = [sys.executable, str(HERE / 'server.py'), '--managed-models', '--models-dir', str(a.models_dir),
           '--llama-server', a.llama_server, '--port', str(a.port), '--model-port', str(a.model_port)]
if a.cuda_model:
    if not a.cuda_model.is_file() or a.cuda_model.resolve().parent != a.models_dir.resolve():
        parser.error('Initial model must be a GGUF in --models-dir')
    command += ['--initial-model', a.cuda_model.name]
os.execv(sys.executable, command)
