"""jingwei Word plugin worker: inspect or submit constrained DOCX operations."""
import json, sys, os
from pathlib import Path
try:
    import docx  # noqa: F401
except ModuleNotFoundError:
    os.execv('/home/hugo/.cache/codex-runtimes/codex-primary-runtime/dependencies/python/bin/python3', ['/home/hugo/.cache/codex-runtimes/codex-primary-runtime/dependencies/python/bin/python3', __file__, *sys.argv[1:]])
from word_plugin import apply_operations, snapshot

task_dir = Path(sys.argv[1]).resolve(); tool = sys.argv[2]
args = json.load(sys.stdin)
input_path = task_dir / 'input.docx'; output_path = task_dir / 'output.docx'
if tool == 'inspect_docx':
    print(json.dumps({'ok': True, 'snapshot': snapshot(input_path), 'note': 'read-only; use submit_docx_operations to save'}, ensure_ascii=False))
elif tool == 'submit_docx_operations':
    # Qwen occasionally places the read-only inspect request inside the submit
    # envelope. Return the same untrusted observation and let the Agent retry
    # with a real submission; do not create or mark an output artifact.
    if isinstance(args, dict) and args.get('operations') and args['operations'][0].get('op') == 'inspect_docx':
        print(json.dumps({'ok': True, 'validated': False, 'snapshot': snapshot(input_path), 'note': 'read-only observation; submit actual document operations next'}, ensure_ascii=False))
        raise SystemExit(0)
    result = apply_operations(input_path, output_path, args)
    print(json.dumps({'ok': True, 'validated': True, 'artifact': 'output.docx', **result}, ensure_ascii=False))
else:
    raise SystemExit('unknown tool: ' + tool)
