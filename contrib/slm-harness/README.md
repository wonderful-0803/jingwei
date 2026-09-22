# 多 LLM 协作工作区

本目录提供 jingwei 项目的共同接手入口、任务认领、执行状态记录和交接约定。协作者先读 [AGENTS.md](AGENTS.md)，再读 [当前状态](collaboration/STATE.md) 和最新交接。

这是可移植的协作协议与初始化快照；不是自动同步服务。具体功能设计、领域实现、实验数据和模型评测材料独立维护。

## 克隆后的入口

在 jingwei 仓库内运行：

```bash
cd "$(git rev-parse --show-toplevel)/contrib/slm-harness"
cat AGENTS.md collaboration/STATE.md collaboration/TASKS.md
```

路径以本目录为基准，仓库根为 `../..`。CLAUDE.md 和 GEMINI.md 指向同一份 AGENTS.md，避免各模型维护互相矛盾的进度。外层原始工作区的活动任务、运行进程和个人路径不随此快照迁移。

## 导航

| 文件 | 用途 |
|---|---|
| [AGENTS.md](AGENTS.md) | 接手顺序、执行同步和并发规则 |
| [STATE.md](collaboration/STATE.md) | 当前阶段、活动执行和下一步 |
| [TASKS.md](collaboration/TASKS.md) | 任务 owner、写范围、依赖与验收 |
| [DECISIONS.md](collaboration/DECISIONS.md) | 授权、决定与适用范围 |
| [PROTOCOL.md](collaboration/PROTOCOL.md) | 日志格式、目录锁、恢复和交接 |
| [执行事件](collaboration/events/) | 追加式 JSONL 历史 |
| [交接](collaboration/handoffs/) | 最新接手检查点 |
| [交接模板](collaboration/templates/HANDOFF.md) | 可复用交接格式 |
| [workspace.json](workspace.json) | 可移植路径和协作入口 |

## 使用方式

1. 读取快照和事件，核对实际 Git HEAD、dirty 状态与运行进程。
2. 在短期元数据锁内认领任务、run ID 和精确写范围，登记 execution。
3. 释放锁后执行；结束后重新持锁记录结果、证据、失败或待办。
4. 交棒前关闭 execution、释放认领或注明未完成状态，并写新 handoff。

`.coordination/`、`artifacts/`、`outputs/` 只用于本地锁、证据和产物，不自动提交。共享交接应记录可复现命令及证据的可访问位置；没有提交的证据不能以失效链接代替。

目录锁只协调遵守协议且使用同一目录的本地协作者。跨机器或多个克隆需要指定唯一状态汇总者或集中服务，不能假定 Git 合并提供互斥。此目录不会强制其他 LLM 自动读入或写入状态。

## 验证协作材料

从仓库根运行 `git diff --check`；从本目录运行：

```bash
python3 - <<'PYCHECK'
from pathlib import Path
import json, re
root = Path.cwd()
manifest = json.loads((root / 'workspace.json').read_text())
assert (root / manifest['repository_root'] / 'Cargo.toml').is_file()
for p in root.rglob('*.md'):
    for href in re.findall(r'\]\(([^)]+)\)', p.read_text()):
        if '://' in href or href.startswith('#'):
            continue
        assert (p.parent / href.split('#')[0]).exists(), (p, href)
events = [json.loads(line) for p in (root / 'collaboration/events').glob('*.jsonl')
          for line in p.read_text().splitlines() if line.strip()]
assert events and len({e['event_id'] for e in events}) == len(events)
print('Manifest, links and event JSONL validated.')
PYCHECK
```

这些检查仅验证协作文件，不代表 runtime 或业务功能已通过测试。

## 领域组件（源码快照）

`experiments/` 收纳按任务装配的 Excel 和 Word 插件、skills、工具 worker、独立 grader、运行器及回归测试。它们复用 jingwei 的 ReferenceAgent/Tool bridge；领域策略和副作用约束留在 profile、skill 与插件中，核心 harness 不硬编码 Excel 或 Word。

- `experiments/spreadsheetbench/`：SpreadsheetBench 适配、LibreOffice 重算、profile v1/v2、提交产物契约和独立评分。
- `experiments/excel-smoke/`：小型 Excel 冒烟插件与策略回归。
- `experiments/wordbench/`：python-docx Word 插件、24 题任务规格、fixture/grader/render 流程和 profile/skill。
- `experiments/harness-reliability/`、`experiments/stratified-bench/`：通用可靠性验证和分层抽样运行器。

这些文件是可审阅的源码组件；数据集、模型输出、DOCX/XLSX 夹具和逐题 trace 仍由外部 SLMHarness 工作区挂载，按 `.gitignore` 排除。真实评测前请从外部工作区根目录执行各实验 README 中的命令，并设置 `SLMHARNESS_ROOT` 或按本地目录调整输入/产物路径。仓库 PR 不宣称已将本地评测产物发布。
