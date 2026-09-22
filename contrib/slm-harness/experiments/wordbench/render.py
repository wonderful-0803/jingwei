"""Headless DOCX rendering through the bundled documents skill."""
import subprocess
from pathlib import Path

PYTHON = Path('/home/hugo/.cache/codex-runtimes/codex-primary-runtime/dependencies/python/bin/python3')
RENDERER = Path('/home/hugo/.codex/plugins/cache/openai-primary-runtime/documents/26.909.12148/skills/documents/render_docx.py')


def render_docx(input_path, output_dir):
    output_dir = Path(output_dir)
    output_dir.mkdir(parents=True, exist_ok=True)
    command = [str(PYTHON), str(RENDERER), str(input_path), '--output_dir', str(output_dir), '--emit_pdf']
    completed = subprocess.run(command, capture_output=True, text=True)
    pages = sorted(output_dir.glob('page-*.png'))
    pdfs = sorted(output_dir.glob('*.pdf'))
    return {
        'ok': completed.returncode == 0 and bool(pages),
        'returncode': completed.returncode,
        'pages': [str(path) for path in pages],
        'pdfs': [str(path) for path in pdfs],
        'stdout': completed.stdout[-2000:],
        'stderr': completed.stderr[-2000:],
    }
