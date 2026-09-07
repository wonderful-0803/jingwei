# 为任务显式追加预算

本章面向宿主开发者。先阅读[持久预算运行](durable-budget.md)。`jingwei::budget::grant_budget` 可以在已经核验的安全边界提高任务上限，让正常关闭或预算耗尽的任务继续；它不会清零用量、恢复未决操作或重新执行工具。

## 什么时候可以增额

宿主已认证并批准这次操作，持有独立的 Session 单写者边界，拥有最新可信检查点与完整、持久的物理日志。检查点不能有执行占用、冻结、未结束 run 或 pending；历史须恰好截止检查点的安全终态。模型要求“给我更多额度”不构成宿主授权。

新限制是绝对值：至少一个维度增大，其余不减少，且全部不超过宿主上限。仅预算 ResourceLimit/ActiveTime 停止可以在相应维度增大后清除；记账溢出、身份错误、未知未决状态等不会被增额修复。已消费资源和时间仍保留，活动时间必须有余量。Hard/Soft 模式不改变。

## 发起宿主操作

在第一次调用前可靠保存操作 ID、完整请求和原期望上下文。下面的参数应来自宿主审批和单写者保护下的日志读取，不要照抄模型输出，也不要把简单复制快照字段当作独立核验：

```rust,no_run
use jingwei::budget::{
    grant_budget, BudgetCheckpointStore, BudgetGrantOutcome, BudgetGrantRequest,
    BudgetLimits, BudgetRestoreContext,
};
use jingwei::event::SessionEvent;
use jingwei::id::BudgetOperationId;

async fn apply_approved_grant(
    store: &dyn BudgetCheckpointStore,
    original_expected: &BudgetRestoreContext,
    confirmed_history: &[SessionEvent],
    saved_operation_id: BudgetOperationId,
    approved_new_limits: BudgetLimits,
) -> Result<BudgetGrantOutcome, Box<dyn std::error::Error>> {
    // 宿主在调用全程持有 Session 单写者所有权，并已完成认证/授权。
    let request = BudgetGrantRequest {
        operation_id: saved_operation_id,
        actor: "authenticated-host-operator".into(),
        source: "host-admin-workflow".into(),
        reason: "approved additional work".into(),
        new_limits: approved_new_limits,
    };
    let outcome = grant_budget(
        store, original_expected, confirmed_history, &request,
    ).await?;
    match &outcome {
        BudgetGrantOutcome::Applied { checkpoint, .. } => {
            assert!(!checkpoint.grants().is_empty());
            // 这是提交回执，不是运行句柄；后续仍需重新 acquire。
        }
        BudgetGrantOutcome::AlreadyApplied { record } => {
            assert_eq!(record.request, request);
            // 仅证明原操作已记录，不保证当前任务仍可运行。
        }
    }
    Ok(outcome)
}
```

提交时，审计记录和新预算上限写入同一份检查点，一次 CAS 确认。记录包含操作者、来源、理由、旧/新限制、原 revision、日志游标和原停止原因。已有 Session 运行报告不会被修改；后续核验通过这份审计解释上限变化。累计用量、未知 token 证据、活动/清理时长保持不变。

继续执行仍走 `BudgetExecutionLease::acquire` → `with_durable_budget`。不要从提交回执直接恢复内存 TaskBudget 开始工作，也不要复用已封存的旧句柄。

## 重试与失败

- `AlreadyApplied`：同一 ID、请求和原 revision/游标已经记录。即便中间运行过新 Turn 或现在有冻结占用，也只返回历史回执，不重复增额。
- `OperationConflict`：同一 ID 的内容或原期望不同，停止自动重试。不要换 ID 掩盖冲突。
- `Commit`：保留确切候选镜像、expected_revision 和底层存储错误。结果不确定时先核验原操作；同一镜像可精确重试，不能重算增量或改版本。
- `RecoveryRequired`：未决运行仍然冻结。本接口不提供 TTL 抢占、清除 claim、忽略 pending 或自动退款。
- `Missing`、边界不符、存储损坏：保留证据，不初始化已有任务。
- `AuditFull`：当前每个任务最多保留 64 条增额审计；拒绝继续追加，不丢弃旧幂等键。长期归档协议尚未提供。

每个操作 ID、actor、source、reason 限制为 1024 UTF-8 字节，不能为空。操作记录不是身份认证，也不是防篡改签名；宿主须保护存储和审批入口。取消等待或网络返回错误并不能证明没有写入，遵循 provider 的核验与排空协议。

## 存储与版本

新导出快照为 V3；V1 普通镜像、V2 占用仍可读，不能携带 V3 审计。Session TaskRunReport 与文件外层仍为版本 1。自定义存储必须在实际 CAS 和链读取中调用 `validate_transition(previous)`，保护旧审计前缀并拒绝无审计增额。默认[文件存储](file-checkpoint-store.md)已接入该校验。

目前 Session JSONL 没有跨进程单写者锁，宿主必须自行保证独占。冻结后的审计解除、完整故障矩阵和 Task 业务状态恢复仍在后续开发；本章不是完整故障恢复指南。
