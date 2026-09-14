# 从已确认任务检查点继续

JW-07-c 提供独立 `jingwei-task-runtime`，把[任务状态](task-state.md)、持久预算执行 lease 和 canonical AgentRuntime 连接起来。它面向可信宿主，不随默认 façade 引入。当前是仓库开发版本，可使用指向 `crates/task/runtime` 的 path 依赖。

## 步骤如何成为安全边界

有限参考 Agent 默认仍在单个 Turn 内执行有限循环。宿主启用 ReferenceAgentConfig::checkpoint_each_step 后，Agent 每完成一个成功工具步骤就返回 Checkpointed。canonical runtime 随后排空模型和工具、写入完整消息及 TaskRunReport、提交 checkpointed 终态、settle，再通过原 durable lease 封存并保存预算。协调器最后保存引用该边界的 TaskSnapshot。

Checkpointed 表示步骤已确认，任务等待宿主继续；Completed 表示回合完成；WaitingForInput 表示等待用户答复。只有工具成功路径产生步骤暂停，失败、取消和无进展检测仍保留原有停止语义。该实现把步骤作为完整关闭的 Turn 保存，不在仍有工作运行的回合中间伪造安全检查点。

后续显式开启新 Turn，规范历史投影包含先前已确认的工具调用与结果；框架不会重新派发旧调用。模型仍可能提出新的相似动作，这些动作继续经过当前权限和审批；跨尝试幂等键与外部副作用去重将在 JW-07-d 提供。

## 宿主装配

先明确配置实际 Agent、工具权限、当前策略与业务 payload schema。TaskResumePolicy 每次重新验证操作者权限及这些实际契约，也应验证应用 payload 的业务语义；没有默认允许策略。TaskPayloadReducer 仅生成应用命名空间内容，不生成预算或执行权限。两者都是可信宿主代码；reducer 须有界、非阻塞，不执行工具副作用。

使用同一个已选定 SessionPersistence 供协调器和 AgentRuntime 读取。官方 JSONL provider 的独占写者锁贯穿整个生命周期；通过 PluginRegistry::session_persistence 取得相同实例，避免重新创建竞争 provider。自定义 provider 必须提供同等的独占所有权和确认读取语义。

```rust,no_run
use std::sync::Arc;
use jingwei::budget::{BudgetCheckpointStore, BudgetLimits, MonotonicBudgetClock};
use jingwei::plugin::PluginRegistry;
use jingwei::task::{TaskCompatibility, TaskIdentity, TaskStateStore};
use jingwei_task_runtime::*;

async fn continue_checkpointed_task(
    registry: &PluginRegistry,
    identity: TaskIdentity,
    budgets: Arc<dyn BudgetCheckpointStore>,
    states: Arc<dyn TaskStateStore>,
    policy: Arc<dyn TaskResumePolicy>,
    reducer: Arc<dyn TaskPayloadReducer>,
    current_compatibility: TaskCompatibility,
    expected_revision: u64,
) -> Result<TaskRunResult, Box<dyn std::error::Error>> {
    let coordinator = TaskCoordinator::new(TaskCoordinatorConfig {
        identity,
        runtime: registry.agent_runtime().ok_or("missing AgentRuntime")?,
        session: registry.session_persistence().ok_or("missing SessionPersistence")?,
        budgets,
        states,
        clock: Arc::new(MonotonicBudgetClock::default()),
        policy,
        reducer,
    });
    let result = coordinator.start(TaskResumeRequest {
        expected_revision,
        compatibility: current_compatibility,
        // 示例使用默认值；生产宿主显式配置当前预算上限。
        host_limits: BudgetLimits::default(),
        run_limits: BudgetLimits::default(),
        intent: TaskIntent::Continue {
            instruction: "根据已确认结果继续完成任务，不重复旧调用".into(),
        },
    })?.wait().await;
    coordinator.close().await;
    Ok(result?)
}
```

初始预算与 Created 快照由宿主显式创建并按预算 → 任务状态顺序保存；协调器不会把文件缺失解释为新任务。运行前加载任务、预算、完整日志，验证预期版本、当前兼容性和精确确认边界，执行当前宿主策略检查，再 CAS 取得唯一 durable lease。取得 lease 后复查日志及任务状态，runtime 还会核对自身准入历史。任何不一致都不能降级为普通内存预算继续运行。

新进程继续前，应先关闭并排空旧协调器、旧 runtime 和存储，释放全部旧 Session provider/registry 句柄，再创建新实例。不要仅凭旧句柄已经 close 就认为所有 provider 引用都释放了。

## 三种显式输入

| 任务阶段 | 接受的输入 |
| --- | --- |
| Created | Start，首次用户消息 |
| Checkpointed | Continue，宿主新的继续指令 |
| WaitingForInput | Reply，准确的提问 TurnId 与本次用户答复 |
| Completed / Stopped | 拒绝续跑；完成不隐式重新打开，停止不隐式重试 |

文本必须非空且不超过 64 KiB。Reply 必须匹配已确认待答快照的回合，编码为带 reply_to 和 task_id 的用户消息；用户答复本身不是工具审批。所有新回合继续使用原 Task 累计预算与当前更严格限制。每 Turn 的参考 Agent 计数会重新开始，但 Task 消耗不会清零，宿主也不会被自动循环调用。

## 执行结果与状态提交分别观察

TaskRunResult 包含 turn 与 state 两个结果。turn 保存 canonical 回合报告或原始 runtime 错误；state 表示任务快照是否已成功发布。状态写入失败不会撤销已完成工具或已持久化预算。

TaskPublishError::Commit 保留准确候选与存储错误。保留该候选，按 candidate.revision()-1 精确重试状态存储，不能重新执行回合。若预算已前进但旧任务状态仍落后，下一次 start 会拒绝；宿主须在独占所有权下核验最新日志/预算和应用 payload，显式修复状态。丢失原候选时，业务 payload 不能凭旧值猜测。

预算最终提交未确认、活动 claim、未结束日志或缺失结果时，不发布可恢复任务状态。保存 turn 错误中的原始预算候选，先按[预算候选恢复](budget-recovery.md)处理；协调器不会自动清除 claim、退款或重放工具。

## 生命周期及验收范围

每个协调器绑定一个 Task，实例及其 clone 同时只接受一个操作；跨实例通过预算 CAS 防止重复执行。start 需要 Tokio runtime，同步准入后由后台任务拥有完整检查、执行和状态发布流程。丢弃控制器或 wait 只分离等待；cancel/canceller 显式取消，已执行工作仍需收尾。close 停止准入并等待整个流程，包括状态提交；宿主应先 close，再关闭 runtime 和文件存储。

取消发生在成功取得 lease 之后但 runtime 尚未接收时，claim 可能保持冻结，必须核验；取消不能成为撤销已确认占用的捷径。宿主策略若永久阻塞，close 也会等待，框架不强行终止任意用户代码。

已验证三个独立进程依次完成“工具步骤暂停 → 继续并提问 → 回复并完成”，工具正文累计只执行一次；还覆盖并发恢复、待答关联、策略拒绝、当前工具授权、累计预算、状态提交失败、预算收尾失败和取消/分离等待。这不是任意指令处崩溃恢复，也不是 exactly-once 外部副作用承诺；完整 AT-F6 故障矩阵继续在 JW-07-e 验收。
