# 任务状态契约

启用 facade 的 `task-state` feature 后，通过 `jingwei::task` 使用 TaskSnapshot、日志评估和 TaskStateStore；也可单独依赖 jingwei-task。它不依赖参考 Agent，不自动安装到 Harness 或 StandardCoreBundle。

当前提供状态契约、有界内存存储和可选本地文件存储。JW-07-c 已接入[步骤暂停与受控续跑](task-recovery.md)；现有[持久预算](durable-budget.md)的独占执行 lease 不能被任务快照替代。

## 快照记录什么

| 字段 | 含义 |
| --- | --- |
| identity / revision | Task、Session、Agent key 及任务状态版本；与预算身份复用同一契约 |
| compatibility | 独立的 Agent/策略修订、工具契约修订、payload 命名空间及 schema 版本 |
| cursor | 与预算检查点一致的已确认日志尾地址 |
| budget | 预算检查点版本和累计 charged 的引用；不是另一份可运行账本 |
| phase | Created、Checkpointed、Completed、WaitingForInput 或 Stopped，来自已确认回合状态 |
| settled_steps | 已有结果的决策 ID，含被拒模型提案；不代表每一步业务成功 |
| payloads | 应用拥有的 JSON 数据，键必须与声明的命名空间完全一致 |

Completed 表示规范回合完成，不是业务成功证明。框架不会验证应用 payload 的业务语义，也不会从某个工具的结果推断业务 schema。工具修订号需由宿主在参数契约变化时更新；它不是工具授权或自动计算的 schema 摘要。

## 捕获和存储

TaskSnapshot::capture 目前只接受空日志或完整结束的回合。输入必须由宿主独立获取：最新已确认的完整 Session 日志、其对应的预算检查点，以及当前兼容性声明。冻结预算、活动执行占用、缺失结果、预算版本/日志尾不匹配均不能生成安全快照。

```rust
use std::collections::BTreeMap;
use jingwei::budget::BudgetCheckpoint;
use jingwei::event::SessionEvent;
use jingwei::task::*;
use serde_json::json;

async fn save_task_state(
    store: &dyn TaskStateStore,
    budget: &BudgetCheckpoint,
    confirmed_history: &[SessionEvent],
    expected_revision: u64,
) -> Result<TaskSnapshot, Box<dyn std::error::Error>> {
    let compatibility = TaskCompatibility {
        agent_revision: "my-agent-v1".into(),
        policy_revision: "my-policy-v1".into(),
        tool_revisions: BTreeMap::from([
            ("lookup".into(), "lookup-schema-v1".into()),
        ]),
        payload_schemas: BTreeMap::from([("my-app".into(), 1)]),
    };
    let snapshot = TaskSnapshot::capture(
        expected_revision,
        compatibility,
        BTreeMap::from([("my-app".into(), json!({"stage":"review"}))]),
        budget,
        confirmed_history,
    )?;
    store.compare_exchange(expected_revision, &snapshot).await?;
    Ok(snapshot)
}
// 开发时可使用 MemoryTaskStateStore::new(128)?；它不持久化数据。
```

状态存储使用 compare_exchange：expected_revision=0 表示尚无状态，候选版本必须为预期版本加一。并发不同更新只有一个成功；完全相同的候选精确重试返回 AlreadyPresent。身份/兼容性变化、游标倒退、累计预算减少或已结算决策丢失会被拒绝，不覆盖旧状态。业务数据可以在同一确认边界更新，但不能借此更改框架证据。

TaskSnapshot::from_json 限制输入并检查结构及版本；解码成功仍不证明内容可信。使用前调用 verify_evidence，传入从当前配置、存储和确认日志独立取得的身份、版本及兼容性。不要直接把快照自己的 compatibility 当作“当前期望值”。应用还需按自己的 schema 校验 payload；框架不会静默迁移未知版本。

## 缺失结果与待核验

assess_task_history 是纯读取函数，返回回合边界、已结算决策和 unresolved 操作的类型、调用 ID、原始意图游标及关联 StepId。只有调用记录、没有结果时，只能确定执行结果未知，不能认定工具没有运行。即使工具结果已记录，只要回合尚未关闭，本批仍拒绝捕获安全快照。

已记录失败结果表示该次调用在日志中结束，不保证外部副作用没有发生。工具副作用声明、幂等键及宿主核验/重试策略将在后续接入；当前模块不会重放任何模型或工具调用，也不会沿用旧审批。

## 日志、预算和状态的提交顺序

先确认规范日志，随后通过既有预算流程提交精确检查点，再写引用该边界的任务状态。三者不是原子事务。预算已经推进而任务状态写入失败时，不能为了“补状态”重放工具；宿主应在独占所有权下核对日志与预算，重建框架派生字段，并让应用提供对应业务 payload。

MemoryTaskStateStore 的锁只保护本进程内状态更新，不保护 Session 根目录、预算文件或另一个进程。它不具备跨重启恢复能力，序列化 TaskSnapshot 也不会恢复 TaskBudget 的权限或累计状态。现有预算恢复规则见[预算候选恢复](budget-recovery.md)。

## 资源边界

单份快照最多 512 KiB，最多 4096 个决策 ID、128 个 payload 命名空间、1024 个工具修订声明。payload 深度最多 64、节点数最多 65,536；完整日志评估最多 100,000 个事件。身份及修订文本最多 4096 字节，待答问题最多 64 KiB。内存存储显式指定 1–65,536 个任务槽；总容量还受槽数乘以单份快照上限约束。超限明确失败，不截断证据。

## 本地持久化任务状态

JW-07-b 新增独立的 `jingwei-task-file`，实现同一个 TaskStateStore 接口。它不会随默认 façade 或 task-state feature 自动引入；当前是未发布的仓库版本，宿主可显式使用指向 `crates/task/file` 的 path 依赖。

宿主先为每个 Task/Session/Agent 绑定准备一个普通文件，并确认文件与目录项已按部署要求持久化。空的既有文件表示尚未提交任务状态；文件缺失是错误，适配器不会创建文件。文件路径由宿主管理，运行期间不能替换、截断、删除文件或绕过文件锁写入。它不支持不可信路径、外部篡改或不遵守文件锁的写者。

```rust,no_run
use std::path::Path;
use jingwei::task::{TaskSnapshot, TaskStateStore, TaskStoreError, TaskWriteOutcome};
use jingwei_task_file::{FileTaskStateConfig, FileTaskStateStore};

// 在 Tokio runtime 内调用；snapshot 已由宿主核验日志、预算及当前兼容性。
async fn save_to_file(
    path: &Path,
    expected_revision: u64,
    snapshot: &TaskSnapshot,
) -> Result<TaskWriteOutcome, TaskStoreError> {
    let store = FileTaskStateStore::open(path, FileTaskStateConfig::default())?;
    let result = store.compare_exchange(expected_revision, snapshot).await;
    // 即使调用失败也排空本实例作业；排空不代表失败写入已成功。
    store.close().await;
    result
}
```

每次读取或更新都在独占 OS 文件锁内完成，其他进程或实例竞争时返回 Busy。JSONL 记录包含存储格式版本、预期版本和完整快照；读取校验整条连续版本链及每次状态迁移。CAS 的原子性指协作写者间的版本比较与更新串行化，并不承诺掉电时文件追加不可撕裂。

Applied 只在完整追加并同步文件后返回；对最新候选的完全相同重试，重新同步后返回 AlreadyPresent，不重复写入。旧候选或不同内容不会被当作精确重试。load 同样重新同步完整记录，以确认之前没有成功回执的写入；它只返回存储状态，使用前仍须独立 verify_evidence，不会自动恢复运行。

| 结果 | 宿主应如何处理 |
| --- | --- |
| Conflict | 重新核对当前状态与业务更新，不盲目递增预期版本 |
| Busy | 文件锁或本实例 IO 容量被占用，可稍后重试存储操作 |
| Closed | 同实例所有 clone 已关闭，需要显式重新打开 |
| Corrupt | 未结束的行、未知格式、非法快照或不连续/倒退状态链；停止并保留证据，不自动截断或重建 |
| LimitExceeded | 单行或累计文件超限；不截断，当前无自动压缩/轮转 |
| Storage / DefinitelyNotCommitted | 本次尝试在写入前失败，不能据此否定此前不确定写入 |
| Storage / Indeterminate | 写入或同步未确认；保留原候选及预期版本，重新读取/精确重试核验，不重放工具 |

默认单行（含换行）上限 1 MiB、日志上限 64 MiB、每实例最多 4 个已接受 IO 作业；快照本身仍受 512 KiB 限制。clone 共享准入计数，独立实例仅共享文件锁。构造时路径检查为同步操作，读写在 Tokio blocking pool 执行。首次 poll 才准入，丢弃已准入 future 不取消后台写入；status 可观察积压，close 停止所有 clone 准入并等待 worker 排空。应在销毁 Tokio runtime 前调用 close。

本地文件系统必须支持排他锁与 sync_all；文件供应和硬件持久性由部署负责。本批在 Linux GNU 上验证跨进程竞争、零字节/部分写入、同步前后错误、完整写入后进程退出并由新进程确认；这些测试不等同于掉电模拟或跨平台验收。

任务文件锁只保护任务状态文件，不覆盖规范日志、预算文件或 Agent 执行。JW-07-c 已提供[受控协调](task-recovery.md)：任务状态写入成功不授予执行权限，也不沿用旧审批。
