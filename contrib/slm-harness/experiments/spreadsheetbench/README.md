# SpreadsheetBench experiment

This experiment mounts a task-selected Python-tools profile and skill into jingwei ReferenceAgent. It leaves jingwei core and the formal H001 design unchanged. See PLAN.md and GRADING.md for evaluation policy and limitations.

## Local dependencies

- jingwei checkout at dev d43258361a7aa71717989e436bcc4500485bb60a
- Ollama with both requested local model tags; only one model loaded at a time
- `/opt/libreoffice26.8/program/{soffice,python}` (do not use the older system UNO)
- `/usr/bin/bwrap`
- Codex bundled Python environment, plus experiment-local `.venv` containing tqdm4.67.1; openpyxl/numpy/pandas versions recorded in each manifest

## Commands from workspace root

```bash
# Actual native XLOOKUP test
/opt/libreoffice26.8/program/python experiments/spreadsheetbench/check_xlookup.py --output artifacts/spreadsheetbench/xlookup-new-run

# Host-side isolation and scoring regression tests
experiments/spreadsheetbench/.venv/bin/python -m unittest discover -s experiments/spreadsheetbench -p test_adapter.py

# Build only the independent experiment host
OPENSSL_DIR=/home/hugo/miniconda3 CARGO_TARGET_DIR=/home/hugo/projects/SLMHarness/jingwei/target cargo build --manifest-path experiments/spreadsheetbench/Cargo.toml --locked

# Full pair, or resume only if prior process is confirmed stopped and Ollama is idle
experiments/spreadsheetbench/.venv/bin/python experiments/spreadsheetbench/run.py --out artifacts/spreadsheetbench/full400
```

Do not start a second run while process.json's PID is alive. Completed task result.json files are checkpoints; partial tasks are archived before retry. A manifest mismatch refuses resume instead of mixing changed configurations. No global model unload command is used.

`progress.json` shows partial counts, `complete.json` exists only after both models finish, and `failed.json` records an orchestration exception. Per-task folders contain model requests/responses, jingwei journal, submitted solution, replay execution, recalculated XLSX and grade. Hidden standard answers remain only in the downloaded dataset, outside the model tool sandbox. Do not feed result.json grading details back into a model attempt.

Each full-run task is a coordinated execution batch; model/tool subcalls persist immediately in its journal/trace. Shared collaboration metadata is updated before/after each task with the batch result, and the final Markdown report is generated when all tasks finish. No PR or commit is created by these scripts.

## 2026-09-21 用户缩减范围

当前运行改为 `--limit 50 --out artifacts/spreadsheetbench/verified50`，两个模型选同一官方顺序前50题。旧full400中的19个完成结果经运行逻辑等价核验后复制保留；原始目录不覆盖。详见新运行reuse.json及control-change/verification.json。不要重启旧400题命令。
