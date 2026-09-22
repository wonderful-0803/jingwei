"""Build the frozen 24-task Word fixture set."""
import hashlib
import json
import platform
import sys
from datetime import datetime, timezone
from pathlib import Path

from docx import __version__ as docx_version

from tasks import build_document, build_task_specs

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / 'artifacts/wordbench'


def sha(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def build(out=OUT):
    task_root = out / 'tasks'
    task_root.mkdir(parents=True, exist_ok=True)
    specs = build_task_specs()
    manifest = {
        'schema': 1,
        'created_utc': datetime.now(timezone.utc).isoformat(),
        'python': sys.version,
        'platform': platform.platform(),
        'python_docx': docx_version,
        'task_count': len(specs),
        'pilot_ids': [spec['id'] for spec in specs[:6]],
        'categories': {category: sum(s['category'] == category for s in specs)
                       for category in sorted({s['category'] for s in specs})},
        'tasks': [],
    }
    for spec in specs:
        folder = task_root / spec['id']
        folder.mkdir(parents=True, exist_ok=True)
        input_path, gold_path = folder / 'input.docx', folder / 'gold.docx'
        build_document(spec, input_path, gold=False)
        build_document(spec, gold_path, gold=True)
        task = dict(spec)
        task['input_file'] = 'input.docx'
        task['gold_file'] = 'gold.docx'
        task['input_sha256'] = sha(input_path)
        task['gold_sha256'] = sha(gold_path)
        (folder / 'task.json').write_text(json.dumps(task, ensure_ascii=False, indent=2) + '\n')
        manifest['tasks'].append({
            'id': spec['id'], 'category': spec['category'],
            'input_sha256': task['input_sha256'], 'gold_sha256': task['gold_sha256'],
        })
    (out / 'manifest.json').write_text(json.dumps(manifest, ensure_ascii=False, indent=2) + '\n')
    return manifest


if __name__ == '__main__':
    import argparse
    parser=argparse.ArgumentParser();parser.add_argument('--out',type=Path,default=OUT);args=parser.parse_args()
    print(json.dumps(build(args.out), ensure_ascii=False, indent=2))
