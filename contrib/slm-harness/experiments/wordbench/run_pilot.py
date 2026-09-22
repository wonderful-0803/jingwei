"""Offline fixture validation runner; model execution plugs into the same contract."""
import argparse
import hashlib
import json
import shutil
from datetime import datetime, timezone
from pathlib import Path

from grader import grade
from render import render_docx
from tasks import build_task_specs

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / 'artifacts/wordbench'


def classify_render(render):
    return 'rendered' if render.get('ok') else 'render_evidence_gap'


def _sha(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def run(limit=6):
    specs = build_task_specs()[:limit]
    results = []
    for spec in specs:
        source = OUT / 'tasks' / spec['id']
        folder = OUT / 'runs' / ('pilot' if limit == 6 else 'all24') / spec['id']
        folder.mkdir(parents=True, exist_ok=True)
        output = folder / 'output.docx'
        shutil.copyfile(source / 'gold.docx', output)
        render_result = render_docx(output, folder / 'render')
        result = grade(spec, source / 'input.docx', source / 'gold.docx', output, render_result)
        result.update({'render_class': classify_render(render_result),
                       'input_sha256': _sha(source / 'input.docx'),
                       'output_sha256': _sha(output),
                       'render': render_result})
        (folder / 'result.json').write_text(json.dumps(result, ensure_ascii=False, indent=2) + '\n')
        results.append(result)
    summary = {
        'created_utc': datetime.now(timezone.utc).isoformat(),
        'mode': 'pilot6' if limit == 6 else 'all24',
        'expected': limit,
        'completed': len(results),
        'artifact_valid': sum(r['artifact_valid'] for r in results),
        'structure_passed': sum(r['structure_passed'] for r in results),
        'content_passed': sum(r['content_passed'] for r in results),
        'render_passed': sum(r['render_passed'] is True for r in results),
        'render_evidence_gaps': sum(r['render_class'] == 'render_evidence_gap' for r in results),
        'results': results,
    }
    target = OUT / ('pilot-summary.json' if limit == 6 else 'all24-summary.json')
    target.write_text(json.dumps(summary, ensure_ascii=False, indent=2) + '\n')
    return summary


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('--all', action='store_true', help='run all 24 fixture tasks')
    args = parser.parse_args()
    print(json.dumps(run(24 if args.all else 6), ensure_ascii=False, indent=2))
