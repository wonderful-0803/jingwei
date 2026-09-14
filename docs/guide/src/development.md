# 开发环境与兼容性

## 当前源码快照

workspace 目前由 20 个 crate 组成，包版本暂为 0.1.0；这是开发中的 v0.1 功能里程碑，不表示这些新增功能已经发布到 crates.io。jingwei-action 是独立可选组件，facade 的默认 feature 不启用它；共享内存账本 jingwei-budget 通过 facade 的 budget 模块访问。

Rust 开发版本固定为 1.96.0，最低支持声明同步为 1.96。此前的 1.85 声明与源码使用的语法不符；本轮基于真实构建结果收敛声明，未承诺更旧工具链。

`rust-toolchain.toml` 会指定开发版本及 Rustfmt/Clippy。早期阶段有 Windows 与 Linux 验证记录；d2-c2/c3 已在 Linux GNU 完成两套 workspace 的九项基线；Windows x64/MSVC 有此前阶段记录，不能据此宣称所有端侧平台已验证。Windows 构建需要 MSVC 构建工具和 Windows SDK。

## 框架源码检查

准备依赖后，在 workspace 根目录运行：

```text
cargo check --workspace --all-targets --all-features
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo doc --workspace --all-features --no-deps
```

生成的 API 文档入口在 target/doc/jingwei/index.html。指南 Markdown 位于独立的 docs/guide 区域，不是任何 crate 的编译输入。

指南站使用 VitePress，Node.js/npm 工程和锁文件只位于 docs/guide。阅读已发布的静态指南不需要 Node.js；本地编写与构建流程见[编写与发布指南](writing-guide.md)，Rust API 文档入口见 [API 参考](api-reference.md)。

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

JW-04-d2-b 增加 BudgetExecutionLease、with_durable_budget 与确认回执；into_budget_parts 现在同时移交可选持久租约，自定义 runtime 需要处理它，不能忽略。新快照导出为 V2，V1 普通镜像可读，但不能承载占用；TaskRunReport 和文件外层版本仍为 1。独立工程增加真实 Session JSONL/预算文件的跨进程运行与中断测试，只终止自身创建的测试子进程。详见[持久预算运行](durable-budget.md)。

JW-04-d2-c1 将新导出快照升级为 V3，增加宿主[审计增额](budget-grants.md)和稳定操作 ID；V1/V2 读取规则保留。自定义存储须在真实 CAS 与链读取中增加 validate_transition 校验，不再仅检查 revision。审计限制有限，核心不新增 IO 或第三方依赖；冻结占用仍不能通过增额解除。

JW-04-d2-c2 增加 JSONL 跨进程写者锁和[预算候选恢复](budget-recovery.md)。带恢复审计的检查点升级为 V4；自定义存储须支持 compare_exchange_guarded，将所有权保留到所有已接收 IO 结束，默认实现拒绝恢复。预算核心新增对既有 jingwei-session 契约 crate 的依赖，没有新增第三方依赖或 IO runtime。

## Linux 存储故障验证

GNU/Linux 的完整私有测试还需要 `cc`、glibc 动态加载和可用的 `/proc/self/fd`。测试在自己的子进程中使用文件大小限制制造真实 EFBIG，并以私有 LD_PRELOAD 包装器注入同步前/后 EIO；生成的共享库只位于 target/storage-faults，不进入框架依赖或 Cargo 包。缺少编译器或注入未命中时测试明确失败。其他平台仍运行通用契约与重试回归，不声称已验证相同系统调用故障。

JW-04-d2-c3 修复 JSONL 直接精确重试：即使事件字节已可见，也必须重新同步确认才能返回 ReplayedExact；失败继续锁存不确定状态。测试覆盖零字节/部分写入、同步回执失败、写后退出及额外 runtime 中断窗口。进程退出和同步错误注入不等于硬件掉电，工具副作用不确定性仍按[恢复边界](budget-recovery.md)处理。

JW-05-a 增加 [AssistantMessage](assistant-messages.md) V1：canonical AgentRuntime 将完整 final_text 写入 Session，保留 delta 为过程记录。自定义事件消费者需更新穷尽匹配；DriveFailure 新增 AssistantMessage，SessionRuntimeError 新增 InvalidMessageSettlement，错误中保留原始尝试或结算窗口。旧日志不回填，无新依赖。

JW-05-b 增加可选 jingwei-context（facade 的 context feature 默认关闭），提供[确定性对话投影](conversation-projection.md)。仅依赖现有 core/session 契约和 serde/serde_json/thiserror，不新增第三方依赖或 IO runtime；输入必须是完整物理日志，旧回复缺失需显式策略。

JW-05-c 在既有可选 context 组件内增加 ContextBuilder / ContextTokenCounter、硬/软上下文预算与可序列化构建报告。复用 core 的 TokenBudgetMode/TokenBoundEvidence，不增加依赖。硬模式需要宿主提供目标模型的可信输入计数与可执行输出上界，内置估算器仅支持软预算。见[上下文预算](context-budget.md)。

JW-05-d 在 context 中增加工具选择、受控结果视图和有界 MemoryContentStore。jingwei-action 的可选 context feature 接入 ContextualActionStep；facade 同时开启 actions/context 时自动连接。仅增加对既有本地 context crate 的可选依赖，不新增第三方包，单独 actions 仍独立。见[工具视图](tool-views.md)。

JW-06-a 增加默认关闭的 jingwei-reference-agent，facade 通过 reference-agent feature 导出。组合现有 Agent、上下文和动作接口，不修改 core runtime 或添加第三方依赖。模型调用自动计步，报告是 body 观察，取消/预算停止仍以 canonical 终态为准。见[有限参考 Agent](reference-agent.md)。

JW-06-b 增加宿主完成证据检查、连续相同动作/反馈/状态检测与有限轮询配置；拒绝完成立即停止，仍复用原 Task 预算。
