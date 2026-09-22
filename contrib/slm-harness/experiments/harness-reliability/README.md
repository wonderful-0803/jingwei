# E005 通用 harness 可靠性

此实验接通 jingwei 的可选工作流策略与 profile 产物契约，不替代 H001 中的完整插件发现/动态装配设计。运行宿主仍位于 `experiments/spreadsheetbench/src/`，同一个二进制按 profile 加载 worker、工具 schema 和 skill；本目录的 `text/profile.json` 不依赖 Excel。

## 核心策略

`ReferenceAgentConfig.workflow` 默认关闭，配置为 `WorkflowPolicy`：

- `inspection_tools`：宿主声明的检查工具名；只能从现有授权集合缩小可见工具集。
- `max_inspections`：本 turn 已确认的检查调用总数上限。和 Task-wide 预算不同，跨 turn 会重新计数。
- `reserve_steps`：最后若干推理步骤隐藏检查工具，留给执行/提交/完成；不保证模型一定用好这些步骤。
- `recover_no_progress`：相同动作/反馈达到原有重复阈值后最多一次策略切换机会。
- `recover_completion`：完成检查拒绝后允许修复。

所有修复共用 `max_corrections` 与累计 Task correction/step/token/time 预算。取消、权限拒绝、工具失败、结果投影失败不转成自动重试。已经执行的工具不被框架直接重放。模型在获得反馈后发起的新动作仍走授权网关。

`IncompleteResponse` 现在进入有限的执行前纠错，请求更短的完整动作；该错误包含长度截断和其他不完整终止，现有 action 错误类型不保留具体 finish reason。无法借此断言每次都是 token 长度截断。

## 产物契约

可信 profile 的 `delivery.submission_tools` 指定可登记产物的工具，`delivery.artifacts` 指定任务目录内相对路径。必须收到指定工具的 `ok=true, validated=true`，且文件存在、非软链、大小受限，才能形成 SHA256 回执。变更工具执行前使旧回执失效；Final 时重新核对任务身份和文件指纹。

`BusinessVerified` 在本宿主只表示 **交付文件完整性验证通过**，不表示业务答案正确。独立 benchmark grader 不对模型可见；结果报告必须分开列 `delivery_completed` 和 `passed`。

Excel v2 profile 在 `../spreadsheetbench/profile_v2.json`。检查目录真正只读；只有提交工具可在新工作目录产生 output.xlsx 并登记 solution.py。原 v1 profile/skill/worker 仍保留，核心截断纠错行为已经更新，因此复现旧版本须同时按旧 manifest 恢复核心和二进制。

## 验证入口

从 workspace 根目录执行：

```bash
OPENSSL_DIR=/home/hugo/miniconda3 CARGO_TARGET_DIR="$PWD/jingwei/target" cargo test --manifest-path jingwei/tests/model-protocol/Cargo.toml
OPENSSL_DIR=/home/hugo/miniconda3 CARGO_TARGET_DIR="$PWD/jingwei/target" cargo test --manifest-path experiments/spreadsheetbench/Cargo.toml
OPENSSL_DIR=/home/hugo/miniconda3 CARGO_TARGET_DIR="$PWD/jingwei/target" cargo build --manifest-path experiments/spreadsheetbench/Cargo.toml
experiments/spreadsheetbench/.venv/bin/python -m unittest discover -s experiments/spreadsheetbench -p 'test*.py'
python3 -m unittest discover -s experiments/harness-reliability -p 'test_host.py'
```

`verify_models.py` 是固定四个已知失败题的诊断脚本，会调用真实 Ollama、重新执行提交程序、LibreOffice 重算、独立评分。仅允许新结果目录，拒绝覆盖已有 manifest；不提供静默重跑。当前证据：`artifacts/harness-reliability/targeted-v2/`。它不是随机保留集，不能计算或宣称 benchmark 整体提升。

宿主 worker 是受信任插件；加载 skill 不增加工具权限。这里没有实现第三方插件安装、热加载或跨机器自动同步。

完整 benchmark 驱动支持显式 `--profile experiments/spreadsheetbench/profile_v2.json`，并将选中的profile路径和hash写入manifest；默认仍是v1以避免静默切换。不要向已存在的旧输出目录混写新版结果。真实模型诊断之后才加入的验证加固通过离线测试及四份产物重新校验；版本差异见 `final-verification.json`。
