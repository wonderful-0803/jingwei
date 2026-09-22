# Qwen + jingwei + LibreOffice 冒烟实验

这是隔离的可复现实验，不是正式的通用装配层，也不修改 jingwei 核心。宿主从受信任 profile 装配工具与 skill，同一 ReferenceAgent 可运行 spreadsheet 或 text profile。没有运行中插件热加载。

## 执行

本机要求 Rust、Ollama、指定 GGUF 模型、LibreOffice 和系统 Python UNO。当前 OpenSSL 开发文件来自已有 Miniconda，未改系统依赖。

```bash
cd /home/hugo/projects/SLMHarness
OPENSSL_DIR=/home/hugo/miniconda3 CARGO_TARGET_DIR="$PWD/jingwei/target" \
  cargo build --manifest-path experiments/excel-smoke/Cargo.toml
python3 -m unittest discover -s experiments/excel-smoke -p 'test_*.py'
/usr/bin/python3 experiments/excel-smoke/evaluate.py \
  --out artifacts/excel-smoke/new-run --mode control
/usr/bin/python3 experiments/excel-smoke/evaluate.py \
  --out artifacts/excel-smoke/new-run --mode model \
  --cases empty,dedup,sort,group,formula,chain,overwrite,scope,text
```

每次指定全新输出目录；已有 case 目录不覆盖。依照顶层 AGENTS.md，在执行前后同步任务与事件。评测失败进程退出 1，逐例查看 result.json，不能仅看 Rust runner 退出码。单例可用 `--cases formula`。

## 流程和证据

- evaluate.py 在独立 LibreOffice profile 生成合成 input.xlsx；worker 只操作 working.xlsx，原文件校验 SHA256。
- Rust 宿主实际调用 jingwei ReferenceAgent、工具运行时、OpenAI provider、JSON action 及 JSONL journal。
- proxy.py 仅转发 loopback Ollama 请求，固定 temperature=0 / seed=7 / reasoning_effort=none。response_format 负责约束解码；同一 schema 明文加入提示，使模型知道可选工具与参数。只有 grammar、没有可读说明的初始结果保留在 run-01。
- 每次操作核对范围、版本、参数与有限函数；公式range是写目标，source_range是读源，两者分别授权，禁止任意代码、路径、原始公式。成功才替换工作副本；失败锁定该事务。
- validate_workbook 实际重算、检查公式错误/保护表/原文件，导出 candidate.xlsx 再打开验证。发布必须同时满足 Completed、当前验证回执、未拒绝且发生有效写入，才复制成 output.xlsx。
- 隐藏业务答案只在 evaluate.py，工具进程不导入它。在线校验不判断业务答案；离线检查实际输出、数据表完整内容、工作表集合、原文件与保护表。失败的业务输出仍保留作证据，不能将技术校验等同于语义正确。
- llm-trace.jsonl 保存合成数据请求、模型动作、usage、耗时；不记录请求头/密钥，并移除单独 reasoning 字段。journal/ 是 jingwei 原生记录。

## 范围和局限

6 个正向例子（删空行、去重、排序、分组、SUM、三步组合）、2 个拒绝例子、1 个非 Excel 文本统计。数据仅 8 条原始记录；不是总体能力或稳定成功率证明。空行操作是范围内数据压缩，不删除整张表的物理行。公式由函数和范围参数构造，不支持自由公式编程。图表、日期、拆并列、复杂筛选、多个数据规模、重复采样都未覆盖。

宿主 context 设置 8192 Soft，使用估算器；proxy 注入 schema 未纳入该估算。Ollama 实际加载 context_length=32768，不能把本轮说成严格 8K 上下文测评。每次请求 max_tokens=1024，ReferenceAgent 最多 12 步，整例进程超时 210 秒。

原文件隔离与临时保存不是生产级崩溃恢复/事务协议。skill 的“先检查”当前为模型策略，不是宿主状态机强制门禁。权限范围比单个业务意图宽：额外但授权范围内的写入由离线完整评分识别，线上技术校验不代替业务授权确认。只读/拒绝最终可 Completed，但没有可发布文件；自然语言拒绝自动评分只是初筛，应结合最终动作人工检查。

工具服务超时、极端文件、恶意工作簿、外链、复杂格式保真尚未验证。仅面向本实验生成的合成文件。XLOOKUP 未测，本机 LibreOffice 24.2 不在该函数支持版本范围。

本轮结果见 ../../docs/reports/excel-smoke-2026-09-21.md。最终产物评分9/9，先检查流程遵从6/8；两例跳过检查仍由工具网关执行权限/覆盖校验。
