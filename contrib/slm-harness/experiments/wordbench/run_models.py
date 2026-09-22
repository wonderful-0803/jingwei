"""Run the Word plugin adapter against Ollama models and grade DOCX outputs."""
import argparse
import hashlib
import json
import shutil
import time
import urllib.request
from datetime import datetime, timezone
from pathlib import Path

from grader import grade
from render import render_docx
from tasks import build_task_specs
from word_plugin import OPERATION_SCHEMA, apply_operations, snapshot

ROOT = Path(__file__).resolve().parents[2]
FIXTURES = ROOT / "artifacts/wordbench/tasks"
DEFAULT_OUT = ROOT / "artifacts/wordbench-models"
MODELS = {
    "9b": "hf.co/unsloth/Qwen3.5-9B-GGUF:Q8_0",
    "27b": "hf.co/unsloth/Qwen3.8-27B-GGUF:UD-IQ3_S",
}


def api(path, payload=None, timeout=600):
    request = urllib.request.Request(
        "http://127.0.0.1:11434/api/" + path,
        None if payload is None else json.dumps(payload).encode(),
        {"Content-Type": "application/json"},
    )
    with urllib.request.urlopen(request, timeout=timeout) as response:
        return json.load(response)


def sha(path): return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def extract_json(text):
    text = text.strip()
    if text.startswith("```"):
        text = text.split("\n", 1)[1].rsplit("```", 1)[0].strip()
    value = json.loads(text)
    if not isinstance(value, dict) or not isinstance(value.get("operations"), list):
        raise ValueError("model response is not an operations object")
    return value


def prompt(spec, source):
    return f"""Task: {spec['prompt']}\n\nCurrent DOCX snapshot (the input is authoritative):
{json.dumps(snapshot(source), ensure_ascii=False, indent=2)}

Return only JSON matching the provided schema. Use the smallest set of operations needed. Do not return Python, Markdown, explanations, hidden answers, or invented facts. Preserve text and embedded media unless the task explicitly asks for a change. Available operation names include replace_text, append_paragraph, add_heading, set_style, set_title, create_table, set_cell, sort_table, table_to_text, caption_table, set_page, page_break_before, header_footer, remove_text, numbered_list."""


def call_model(model, spec, source):
    payload = {
        "model": model,
        "stream": False,
        "think": False,
        "format": OPERATION_SCHEMA,
        "messages": [
            {"role": "system", "content": "You are a careful DOCX editing agent. Output only the JSON operation object."},
            {"role": "user", "content": prompt(spec, source)},
        ],
        "options": {"temperature": 0, "seed": 7, "num_ctx": 32768, "num_predict": 1200},
    }
    started = time.monotonic(); response = api("chat", payload)
    content = response.get("message", {}).get("content", "")
    return response, content, time.monotonic() - started


def run_task(spec, model, folder, fixtures=FIXTURES):
    source = fixtures / spec["id"]
    folder.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(source / "input.docx", folder / "input.docx")
    result = {"id": spec["id"], "model": model, "passed": False, "category": "model_protocol_error"}
    try:
        raw, content, elapsed = call_model(model, spec, folder / "input.docx")
        trace = {"model": model, "task": spec["id"], "response": {k: v for k, v in raw.items() if k != "thinking"}, "seconds": elapsed}
        (folder / "llm-trace.jsonl").write_text(json.dumps(trace, ensure_ascii=False) + "\n")
        (folder / "model-output.txt").write_text(content, encoding="utf-8")
        payload = extract_json(content)
        (folder / "operations.json").write_text(json.dumps(payload, ensure_ascii=False, indent=2) + "\n")
        execution = apply_operations(folder / "input.docx", folder / "output.docx", payload)
        (folder / "execution.json").write_text(json.dumps(execution, ensure_ascii=False, indent=2) + "\n")
        rendered = render_docx(folder / "output.docx", folder / "render")
        result.update(grade(spec, folder / "input.docx", source / "gold.docx", folder / "output.docx", rendered))
        result["passed"] = bool(result["artifact_valid"] and result["structure_passed"] and result["content_passed"] and result["render_passed"])
        result.update({"category": "passed" if result["passed"] else "wrong_answer", "elapsed_seconds": elapsed, "render": rendered})
    except Exception as exc:
        result.update({"error": f"{type(exc).__name__}: {exc}"})
    (folder / "result.json").write_text(json.dumps(result, ensure_ascii=False, indent=2) + "\n")
    return result


def main():
    parser = argparse.ArgumentParser(); parser.add_argument("--out", type=Path, default=DEFAULT_OUT); parser.add_argument("--fixtures", type=Path, default=FIXTURES); parser.add_argument("--limit", type=int, default=6); parser.add_argument("--models", default="9b,27b")
    args = parser.parse_args(); args.out.mkdir(parents=True, exist_ok=True); fixtures = args.fixtures.resolve()
    specs = build_task_specs()[:args.limit]; selected = args.models.split(",")
    tags = {m["name"]: m for m in api("tags")["models"]}
    manifest = {"created_utc": datetime.now(timezone.utc).isoformat(), "task_ids": [s["id"] for s in specs], "models": {key: {"name": MODELS[key], "digest": tags[MODELS[key]]["digest"]} for key in selected}, "adapter": "experiments/wordbench/word_plugin.py", "manifest_sha256": sha(ROOT / "artifacts/wordbench/manifest.json"), "sampling": {"temperature": 0, "seed": 7, "num_ctx": 32768, "num_predict": 1200}}
    (args.out / "manifest.json").write_text(json.dumps(manifest, ensure_ascii=False, indent=2) + "\n")
    summaries = {}
    for key in selected:
        model = MODELS[key]; api("generate", {"model": model, "prompt": "OK", "stream": False, "think": False, "keep_alive": "30m", "options": {"temperature": 0, "seed": 7, "num_predict": 1}})
        results = []
        for spec in specs:
            results.append(run_task(spec, model, args.out / key / spec["id"], fixtures))
            print(json.dumps({"model": key, "task": spec["id"], "passed": results[-1].get("passed", False), "category": results[-1].get("category")}, ensure_ascii=False), flush=True)
        api("generate", {"model": model, "prompt": "", "stream": False, "keep_alive": 0})
        summaries[key] = {"model": model, "completed": len(results), "passed": sum(r.get("passed", False) for r in results), "categories": {c: sum(r.get("category") == c for r in results) for c in sorted({r.get("category") for r in results})}}
    summary = {"created_utc": datetime.now(timezone.utc).isoformat(), "expected_per_model": len(specs), "models": summaries}
    (args.out / "summary.json").write_text(json.dumps(summary, ensure_ascii=False, indent=2) + "\n")
    print(json.dumps(summary, ensure_ascii=False, indent=2))


if __name__ == "__main__": main()
