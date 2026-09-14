# 核验并恢复最终预算候选

本章面向宿主开发者，接续[持久预算运行](durable-budget.md)。已经完成正文、报告、终态和 settle，却没有确认最终检查点的回合，可以在保留原始候选的条件下核验恢复。恢复不执行模型或工具，也不根据旧占用清零本轮消耗。

## Session 所有权

JSONL provider 首次读取或写入 Session 时取得非阻塞的跨进程排他文件锁。同一 provider 的克隆共享所有权，不同 Session 可以独立工作。锁贯穿缓存历史、正文、报告、终态、settle 和预算收尾；直到所有 provider 克隆及已接收 IO 都释放。`registry.shutdown()` 负责排空，之后还须释放 runtime、registry 和 provider 句柄，才能交接所有权。

恢复使用独立的 `JsonlRecoveryOwnership::acquire`。仍有写者时返回 `SessionPersistenceError::Io`，operation 为 `session_writer_lock`；进程退出由操作系统释放锁，不使用 PID 猜测或 TTL。恢复期间其他合作写者无法读取或追加同一 Session。读取完整日志和直接精确重试已有事件时，都会同步确认已有内容；损坏或同步失败不能成为恢复依据。

使用可信本地目录和支持文件锁的文件系统。`*.writer.lock` 是永久侧文件，运行期间不得删除、替换或绕过它写日志。该锁隔离遵守同一协议的 provider，不能终止绕过框架的进程或撤销外部工具副作用。自定义 provider 通过 `SessionRecoveryOwnership` 实现相同的宿主契约；actor/source 文本并不提供认证。

## 保留证据后恢复

宿主应可靠保存 `BudgetExecutionError::Commit` 的原始最终 `checkpoint`、原 claim 的 execution_id/revision，以及稳定操作 ID 和审批信息。不得从 TaskRunReport 拼造候选：报告不含完整账本 ID 水位。无法取得原候选时继续冻结。

```rust,no_run
use std::{path::PathBuf, sync::Arc};
use jingwei::budget::{
    recover_budget_candidate, BudgetCheckpoint, BudgetCheckpointStore,
    BudgetRecoveryOutcome, BudgetRecoveryRequest,
};
use jingwei_journal_jsonl::JsonlRecoveryOwnership;

async fn recover_retained_candidate(
    session_root: PathBuf,
    store: Arc<dyn BudgetCheckpointStore>,
    original_candidate: BudgetCheckpoint,
    saved_request: BudgetRecoveryRequest,
) -> Result<BudgetRecoveryOutcome, Box<dyn std::error::Error>> {
    // 先停止并排空旧 runtime，释放全部旧 provider 句柄。
    // 参数来自宿主保存的原失败回执和审批，不能来自模型输出。
    let owner = Arc::new(JsonlRecoveryOwnership::acquire(
        session_root, original_candidate.identity().session_id.clone(),
    ).await?);
    let outcome = recover_budget_candidate(
        store, owner, saved_request, &original_candidate,
    ).await?;
    Ok(outcome)
}
```

恢复核对最新 claim、完整物理日志及候选的准确终态边界。成功时把候选和恢复审计作为一个 V4 检查点 CAS 提交。审计含操作 ID、原 execution_id/revision、操作者、来源、理由、所有权 ID、确认游标、原候选报告和 ID 水位。消耗、未知用量、时间、限制与已有增额审计均保留。

- `Applied`：原 claim 已转为带审计的最终检查点。
- `OriginalCommitted`：原始候选已经落盘，核验后返回原镜像，不重复写入或伪造恢复记录。
- `AlreadyApplied`：相同请求及候选证据已记录。这是历史回执，不能据此判断当前任务可执行。
- `Commit` 错误：保留确切审计候选和底层提交确定性。重试恢复 API 时继续传原始最终候选和原请求；也可在保持排他所有权的前提下核验错误中的确切审计镜像。

任何回执都不是执行句柄。后续仍须重新 acquire 执行占用，由 runtime 核验新鲜 Session 历史后运行。

## 取消、存储与保留上限

恢复提交调用 `BudgetCheckpointStore::compare_exchange_guarded`。文件适配器把所有权 Arc 移入已接收的后台 IO；即使取消等待，锁也保持到该工作结束。自定义存储必须实现同一生命周期保证，默认实现拒绝恢复提交。关闭存储时仍须排空已接收工作，未知结果按原操作核验。

每 Task 最多保存 16 条恢复审计，文本字段上限为 1024 UTF-8 字节。满额拒绝新恢复，不自动删除历史。V1/V2/V3 仍可读；没有恢复审计的普通导出保持 V3，有恢复审计的占用、增额和后续快照保持 V4。Session 和检查点文件外层版本不变。旧版本程序必须拒绝 V4，不得降级解释。

## 仍然冻结的情况

`inspect_budget_recovery` 只读取并分类，不授予执行权或写入审计：

| 结果 | 含义 |
| --- | --- |
| Ready | 安全检查点与当前日志边界一致 |
| FrozenNoNewEvents | 没有新事件，但不能证明没有工作或耗时 |
| FrozenUnconfirmedWork | 日志尚无报告和终态闭合证据 |
| FrozenClosedTailNeedsCandidate | 日志尾部有报告和终态，仍需原始候选及完整核验 |

分类中的闭合尾部只是线索，实际恢复还要验证内容。缺失结果、缺少 ID 水位、错误历史、无法隔离旧执行者时均不自动恢复。已验证零字节/部分 write 的内核失败、同步前后错误注入和选定进程中断窗口。实际介质故障及掉电、任务业务状态、待答状态和工具幂等恢复仍待后续验收。
