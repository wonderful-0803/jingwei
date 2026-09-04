# 让预算跨进程运行保留

本章在[文件检查点存储](file-checkpoint-store.md)之上，介绍可选的持久预算运行路径。它可以让正常结束或 AskUser 后的新 Turn 接着消费剩余额度；遇到未完成占用则拒绝自动恢复，不会重放工具。

## 先确认部署边界

宿主负责可信存储、Task/Session/Agent 映射和 Session 的单一写者。默认 JSONL Session adapter 尚无跨进程 Session 锁：同一 Task 的预算占用不能保护不同 Task 共用的日志。当前示例用于受控、串行的同一 Task/Session；不要据此启动多个无协调 Session 写者。

新任务需要宿主独立确认没有历史，显式保存初始 BudgetCheckpoint。已存在的任务缺少文件、遇到 Corrupt 或 RecoveryRequired 时，不得新建预算覆盖它。先阅读[快照原语](budget-checkpoints.md)理解这些边界。

## 取得占用并交给运行时

`BudgetExecutionLease::acquire` 从存储读取已初始化的检查点、核对宿主期望，并通过 CAS 持久保存唯一占用。成功得到的是不能 Clone 的句柄；模型和工具不会接触这个句柄。使用已有 canonical AgentRuntime，不需要另写 Agent 循环：

```rust,no_run
use std::sync::Arc;
use jingwei::agent::{AgentRuntime, AgentTurnReport, AgentTurnRequest};
use jingwei::budget::{
    BudgetCheckpointStore, BudgetExecutionLease, BudgetLimits,
    BudgetRestoreContext, MonotonicBudgetClock,
};

async fn continue_task(
    runtime: &dyn AgentRuntime,
    store: Arc<dyn BudgetCheckpointStore>,
    expected: BudgetRestoreContext,
    run_limits: BudgetLimits,
    user_message: String,
) -> Result<AgentTurnReport, Box<dyn std::error::Error>> {
    // expected 来自宿主的可信状态，不来自模型或工具输出。
    // canonical runtime 还会用 Session admission 的真实历史再次核验尾部。
    let identity = expected.identity.clone();
    let lease = BudgetExecutionLease::acquire(
        store, expected, Arc::new(MonotonicBudgetClock::default()),
    ).await?;
    let request = AgentTurnRequest::new(
        identity.session_id, identity.agent_key, user_message,
    ).with_durable_budget(lease, run_limits);
    let report = runtime.start_turn(request)?.wait().await?;
    let committed = report.budget_checkpoint()
        .expect("canonical durable completion includes a checkpoint receipt");
    assert!(committed.execution_id().is_none());
    Ok(report)
}
```

仍使用 `with_budget` 的调用只提供内存累计，不会隐式持久化。自定义 Agent 在 canonical runtime 内使用同样的受控模型、工具和步骤接口；自定义 AgentRuntime 则必须实现同样的占用、日志核验和收尾契约，不能只取出 TaskBudget 后丢弃租约。

占用确认前没有执行句柄。拒绝 runtime admission 或丢弃租约后，内存 clone 被封存，持久占用保留；因此先确认 Agent 已注册、runtime 可用，再申请占用。没有“抢占过期租约”的便利入口。

## 正常完成时发生什么

初始 ready revision 1 → 运行占用 revision 2 → 报告、终态、Session 结算 → ready revision 3。下一 Turn 再申请 revision 4 的占用；不能重复使用上一次的 TaskBudget clone。

Runtime 在新用户消息及能力调用之前核对日志。没有匹配的末尾 TaskRunReport 与终态，或者游标之后还有未计入快照的事件时，正文不会执行。完成后先封存内存账本，再提交最终检查点；completion 与 shutdown 都等待这个存储屏障。`report.budget_checkpoint()` 是确认镜像，可用于宿主的下一步版本核对，但不是永远最新的执行授权。

已有 TurnFinally hook 只反映 Session 关闭状态，不能用它判断最终预算检查点已提交。调用者丢弃 completion 不会取消 runtime 持有的收尾；未送达的持久化失败也会在 shutdown 报错。

## 发生故障时如何处理

- `RecoveryRequired`：存在占用、冻结账本或未决运行。停止自动恢复，保留检查点和规范日志，等待后续审计处理。
- `RecoveryBoundary`：历史与预算不符，runtime 未写入新 envelope、未执行正文；错误还包含 Session lease 结算结果。
- `Durability`：保留 `outcome`，它可能表明正文和 Session 已完成。不能把这个错误当成“什么都没发生”而重复调用。
- CAS 失败的 `BudgetExecutionError::Commit` 保留 `expected_revision`、确切的 `checkpoint` 候选和 `source`。按照存储确定性核验原操作；需要重试时只能重试原镜像，不能修改版本或额度。确认一份占用记录不会重新生成已丢失的执行句柄。

进程在本轮中断时，磁盘中的占用镜像保存的是上个安全边界，而不是本轮实时用量；本轮可能已经消耗 token 或产生副作用。框架拒绝重新消费旧额度，不会把缺失的最终快照解释为本轮零成本。新进程使用新单调时钟起点，正常完成的累计活动时间继续保留；停机及等待用户不计为活动时间。

新导出快照为数字版本 3，保留 V2 的持久占用语义并增加宿主审计增额。V1 普通镜像、V2 占用仍可读；旧版本不能携带 V3 审计。Session TaskRunReport 与文件记录外层仍各自使用版本 1。旧实现应明确拒绝未知版本，不回退解释。

## 尚未交付

宿主安全边界上的增额已提供，见[审计增额](budget-grants.md)。冻结占用的审计解除和故障核验仍在 d2-c 后续；本批不提供未完成工具自动重试或完整 AskUser 业务状态恢复。完整 Task 状态与工具幂等性在 JW-07，仍是 v0.1 必须验收的内容。
