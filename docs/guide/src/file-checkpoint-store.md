# 将预算检查点保存到本地文件

本章介绍可选 `jingwei-budget-file`。它解决检查点存储的版本竞争、精确重试和有界 IO，不会自动让 AgentRuntime 获得跨进程恢复能力。先阅读[预算快照与恢复原语](budget-checkpoints.md)。

## 何时使用

宿主需要把已经封存的预算镜像保存在本机、且部署文件系统支持排他文件锁和同步持久化时，可显式依赖这个 crate。默认 SDK 不会引入它；也可以实现 `BudgetCheckpointStore` 接入其他存储。

当前仍是仓库开发版本，尚未发布到 crates.io；在宿主工程中使用指向 `crates/budget/file` 的 path 依赖进行验证。不要把下面的示例当作完整生产恢复流程。

## 保存一份封存镜像

宿主先为每个 Task/Session/Agent 绑定准备独立的普通文件，并保证文件及目录项已按部署要求持久化。适配器不会自动创建文件：空的既有文件表示尚无检查点；路径缺失表示错误，而不是新任务。运行期间不要替换、删除、截断这个文件，或绕过适配器直接写入。

```rust,no_run
use std::path::Path;
use jingwei::budget::{
    BudgetCheckpoint, BudgetCheckpointCommit, BudgetCheckpointStore,
    BudgetCheckpointStoreError,
};
use jingwei_budget_file::{FileBudgetCheckpointConfig, FileBudgetCheckpointStore};

// 在 Tokio runtime 内调用。image 来自已停止并排空工作的 TaskBudget 封存；
// expected_revision 来自宿主核验过的版本，不是通过重试随意自增的数字。
async fn save_sealed_image(
    path: &Path,
    expected_revision: u64,
    image: &BudgetCheckpoint,
) -> Result<BudgetCheckpointCommit, BudgetCheckpointStoreError> {
    let store = FileBudgetCheckpointStore::open(
        path,
        FileBudgetCheckpointConfig::default(),
    )?;
    let result = store.compare_exchange(expected_revision, image).await;
    // 成功或失败都排空受拥有的 IO；close 不会把失败变为成功。
    store.close().await;
    result
}
```

新文件的首次提交使用 expected_revision = 0，镜像 revision = 1。后续必须连续提交。`Committed` 表示追加并同步成功；对当前最新的同一份镜像精确重试返回 `ReplayedExact`，不会追加第二条记录。其他镜像或旧版本不能覆盖当前值。

`load(&identity)` 返回经过验证并同步确认的最新镜像。它需要写权限（用于确认此前完整但未获回执的记录），但不会授予执行权。先 load 再 restore 并不是排他的恢复操作：两个宿主可能同时得到相同的预算镜像。应使用[持久预算运行](durable-budget.md)中的占用句柄，而不是仅凭本章 API 并发恢复任务。

## 错误与资源边界

- `Busy`：IO 作业容量或文件锁冲突，本次未写入；按宿主策略有界等待，不使用无限重试循环。
- `Conflict` / `Invalid`：检查版本和 Task 绑定，不新建额度绕过。
- `Corrupt`：包括未知版本、半行、空行或不连续版本；停止恢复，保留原文件核验。不自动截尾或跳过记录。
- `LimitExceeded`：默认单记录 8 MiB、日志 64 MiB；本批没有自动压缩。应在日志满之前制定受控迁移方案。
- `Storage`：检查 certainty。本次写入前失败是 `DefinitelyNotCommitted`；write/sync 后失败为 `Indeterminate`，保留原操作身份与镜像进行核验。一次后续读失败不能推翻之前的不确定结果。
- `Closed`：同实例的所有 clone 都停止准入，需要显式另开实例才能读写。

默认每实例最多 4 个已接受 IO 作业，使用 Tokio blocking pool。丢弃调用 future 不会撤销已接受的写入：容量仍被 worker 持有，`status()` 可观察积压，`close().await` 排空所有 clone 的作业。即使 close 等待者被取消，实例也不会重新开放。排空并不等于所有写入成功；离开的调用者留下的未知结果仍需另开 store 核对。

日志大小、作业数上限不等于整个进程内存上限。构造时的路径检查是同步操作；IO 方法须在 Tokio runtime 内首次 poll。结束 runtime 之前先排空存储。本地锁依赖操作系统和文件系统支持，不保证网络盘、外部篡改或任意硬件故障下的行为。

## 下一步

文件存储是 d2 的第一批。d2-b 已接入运行前持久占用和日志边界核验，详见[持久预算运行](durable-budget.md)。d2-c1 已接入[审计增额](budget-grants.md)，CAS 与文件链读取保护审计前缀、拒绝未审计增额。冻结后的处理和完整故障验收仍在后续；不把本章完成视为 AT-F4-04 或完整任务恢复已验收。
