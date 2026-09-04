# 开发环境与兼容性

## 当前源码快照

workspace 目前由 17 个 crate 组成，包版本暂为 0.1.0；这是开发中的 v0.1 功能里程碑，不表示这些新增功能已经发布到 crates.io。jingwei-action 是独立可选组件，facade 的默认 feature 不启用它；共享内存账本 jingwei-budget 通过 facade 的 budget 模块访问。

Rust 开发版本固定为 1.96.0，最低支持声明同步为 1.96。此前的 1.85 声明与源码使用的语法不符；本轮基于真实构建结果收敛声明，未承诺更旧工具链。

`rust-toolchain.toml` 会指定开发版本及 Rustfmt/Clippy。已有 Windows x64/MSVC 历史验证记录；当前开发快照按固定工具链在 Linux 完成基线验证，不能据此宣称所有端侧平台已验证。Windows 构建需要 MSVC 构建工具和 Windows SDK。

## 框架源码检查

准备依赖后，在 workspace 根目录运行：

```text
cargo check --workspace --all-targets --all-features
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo doc --workspace --all-features --no-deps
```

生成的 API 文档入口在 target/doc/jingwei/index.html。指南 Markdown 位于独立的 docs/guide 区域，不是任何 crate 的编译输入。

dev 分支包含独立契约测试和其他开发资料，所有层级的 target 目录均被排除。
这些开发资产不进入 Cargo 发布包；框架源码和指南构建不依赖验证工程。
指南教学片段由独立工程编译验证，而不是让文档构建依赖测试套件。

在 dev 分支准备主工作区和测试工作区的依赖后，Windows 可执行
`& .\scripts\check-baseline.ps1 -Jobs 4`；Linux 执行 `bash scripts/check-baseline.sh --jobs 4`（需 Python 3），重跑相同的 9 项离线基线检查。
脚本依赖本机 Rust 工具链和两个工作区已缓存的锁定依赖，协议测试需要监听本机回环端口，日志写入 results/baseline。

## 扩展接口的兼容原则

当前处于未上线开发期，模型接口已直接替换为 generate/generate_stream，不提供旧文本接口兼容层。已有 provider 需要实现新的结构化方法并声明实际支持的能力；默认 Unknown 会在受控推理前被拒绝。

开发阶段每次接口变更同时维护契约说明、文本功能回归和指南片段。兼容 HTTP 适配器已有文本/工具/JSON Schema 映射与类型化 SSE，使用本机回环 HTTP 验证；真实模型、跨平台、独立 Cargo 包消费及完整指南站的验收仍在后续阶段进行。规范模型记录使用显式版本；旧开发日志不自动迁移或删除。

canonical 模型 runtime 已增加有限调度和默认 600 秒的准入期限。穷尽匹配 `ModelRuntimeError` 的扩展需要处理新增的过载、请求过大、排队超时和无效期限变体。自定义 runtime 可沿用默认返回 None 的 `scheduler_snapshot`，无需提供虚构计数；配置和记录兼容语义见[模型调度](model-scheduling.md)。

共享预算接入新增 `AgentTurnRequest::with_budget`、窄 `AgentContext::budget()`、模型/工具绑定的 `with_budget`，以及 runtime 的类型化 Budget 错误。canonical Agent 默认获得有限临时 Task，显式 Task 可跨 Turn 复用；ActionStep 的 TaskId 应来自当前预算身份。自定义 runtime/context 的可选观察默认 None 不表示预算已经实现，扩展需自行遵循受控预算契约。自定义 runtime 应使用 `into_budget_parts()` 接收并保留传入的作用域；旧 `into_parts()` 不携带预算，不适用于需要受控预算的新实现。

SessionEventKind 新增由 runtime 写入的 TaskRunReport，使用独立数字版本 1；旧模型事件仍为 V1。事件消费者的穷尽匹配需要更新，预算 run 的 TurnId 现为 Option，以表示尚未取得 Session 租约的准入阶段。报告失败会保留类型化尝试证据；这些观察数据不提供恢复授权。详见[任务预算](task-budget.md)，本批检查结果以对应实施记录为准。

JW-04-d1 新增 BudgetCheckpoint、BudgetRestoreContext 和 BudgetCheckpointStore 契约；新增 CheckpointSealed 错误与 RecoveryRequired 停止原因。快照使用独立版本，不改变旧模型事件含义。验证工程增加本机子进程测试，需要允许启动自身测试可执行文件和写入临时测试目录；不启动模型服务。当前接入边界见[预算快照](budget-checkpoints.md)。

JW-04-d2-a 新增显式选择的 jingwei-budget-file，使用 Rust 标准文件锁与 Tokio blocking IO；不进入默认 facade 依赖图。新增 Busy/Closed/Corrupt/LimitExceeded 存储错误，未上线阶段直接扩展接口。指南见[文件检查点存储](file-checkpoint-store.md)。测试会启动并终止自身创建的持锁子进程，以验证进程退出后的锁释放，不涉及外部服务。
