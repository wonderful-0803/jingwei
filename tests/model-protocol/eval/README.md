# JW-08-a 内部任务运行器

从仓库根目录运行（无需模型、网络或下载）：

```sh
cargo run --locked --offline --manifest-path tests/model-protocol/Cargo.toml --target-dir target --bin jw-eval -- --config tests/model-protocol/eval/fake.json --output /tmp/jw08-pilot
```

输出目录必须不存在，父目录必须存在。单独重跑时使用新目录并追加 `--case doc-extract`；可重复 `--case`，未知 ID 会拒绝运行。`runs` 控制每个选中任务的重复次数。每次执行重新创建 Harness、模型脚本、业务状态、Session 和内容存储，顺序执行、并发为 1。退出码：0 全部通过，1 存在任务失败，2 配置/输出基础设施错误。单条任务失败仍记录并继续下一条。

输出：

- `manifest.json`：配置、完整任务快照和选择范围；fake 脚本与 expected 仅供宿主使用，不进入真实模型输入。
- `results.jsonl`：每次执行一条，含实际状态、失败分类、端到端耗时、最终回复或错误证据、canonical TaskRunReport（预算及模型/工具计数）。每条及时 flush。
- `summary.json`：计划数、已完成、通过、失败和未完成数。异常中断可能留下最后一次完整汇总；逐条 JSONL 和 Session 是核对依据。
- 每个任务/轮次的独立目录：原始 canonical JSONL Session，保留模型与工具交互。此处不是生产恢复或可执行重放入口。

成功必须同时满足正常 Completed、正常关闭和完整业务状态等于 expected。模型仅声称完成不会通过。写入仅限任务的 writable 列表；所有工具只操作内存模拟记录，不访问业务数据库或真实文件。模型产生的错误、权限拒绝和失败日志均保留。

## 已运行的本地模型

复制 `local.example.json`，填写已部署服务的 endpoint、model 和权重/量化/设备/模板记录；tasks 路径相对于配置文件，复制到别处时应同步调整。然后使用同一命令传入新配置。服务必须支持所选 `json`（JSON Schema）或 `native`（原生工具）协议。不支持时保留失败，不自动降级。超时和输出 Token 上限显式传入，max_steps 限制回合。

可选 `api_key_env` 填环境变量名，运行器从进程环境读取；不要把密钥放入配置或 endpoint。运行器不启动服务、不下载模型、不主动运行付费评测。endpoint 禁止内嵌凭据、query 和 fragment。

现有适配器未提供 temperature/seed 等采样参数，本批使用服务端默认值并在 manifest 明示；模板标识也是宿主记录，不会替换服务端模板。真实基线前必须核实并固定服务端参数。上下文使用 16384 的软估算窗口，保留 max_tokens 输出空间；不是目标模型真实窗口或 tokenizer 的证明。其余 Agent/预算使用当前框架默认值，应配合提交版本复现。

本批未测量宿主/模型服务峰值内存，manifest 分别记 null；Token 缺失按 canonical 原始报告保留未知，不补造 0。四个脚本任务只验证运行器，不代表真实模型成绩；40 任务、两模型三轮、消融、阈值冻结与恢复故障验收仍未完成。

开发资产由 dev 跟踪，运行结果建议保存在 `/tmp` 或已忽略的 `results/evaluations/` 中；不得直接纳入公开 Cargo/源码交付。原始记录可能包含真实模型输入输出，内部留存与备份由宿主管理。

针对性检查：

```sh
cargo test --locked --offline --manifest-path tests/model-protocol/Cargo.toml --target-dir target --bin jw-eval
cargo clippy --locked --offline --manifest-path tests/model-protocol/Cargo.toml --target-dir target --bin jw-eval -- -D warnings
```

编译二进制后，可用 `python3 tests/model-protocol/eval/verify_cli.py target/debug/jw-eval` 验证 CLI 与临时本地 HTTP 模型服务；需要允许监听回环地址，不访问真实模型。
