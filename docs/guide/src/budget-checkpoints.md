# 预算快照与恢复原语

本章适用于编写宿主持久化接入的开发者。先阅读[任务预算](task-budget.md)。这里介绍底层原语；实际文件适配器和 runtime 持久占用路径分别见[文件存储](file-checkpoint-store.md)与[持久预算运行](durable-budget.md)。安全边界上的[审计增额](budget-grants.md)已提供，冻结后的处理仍未完成。

## 快照不是运行报告

`TaskRunReport` 用于说明发生了什么。`BudgetCheckpoint` 是带独立数字版本、revision 和 ID 水位的账本镜像，用于宿主控制的状态转移。新导出使用 V3，V1 普通镜像和 V2 占用仍可读取；V3 增加有界审计记录。它保留累计扣费、变量预留、实际/估计/未知用量、活动及清理时长、停止原因和未决项。

`TaskBudget::seal_checkpoint` 在锁内采样并封存整个旧账本。所有 clone、scope、run 和 reservation 随后不能再消费或结算；需要把镜像安全保存后才能移交执行。封存不是取消操作，不会停止工具正文或撤销副作用。可继续的检查点必须在受控工作已经排空、run.finish 完成后生成。

含有 run 或 pending 的镜像只能恢复为冻结账本，保留观察证据并拒绝开始新 run。即使没有 pending，只要 run 处于准入或 prepare_report 后尚未 finish 的阶段，也不能认为可以安全重启。不重建旧 Future、不退款、不自动重放工具。

## 纯账本用法

下面只演示没有 Session/模型/工具的确定性预算操作，不包含磁盘持久化，也不是实际 Agent 恢复示例。宿主已知这是自己的首个 revision，且没有关联的规范事件，因此期望 revision 为 1、游标为 None。

```rust
use std::sync::Arc;
use jingwei::budget::{
    BudgetAmounts, BudgetCheckpoint, BudgetIdentity, BudgetLimits, BudgetRequest,
    BudgetRestoreContext, MonotonicBudgetClock, TaskBudget, TokenBudgetMode,
};
use jingwei::id::{SessionId, TaskId, TurnId};

let identity = BudgetIdentity {
    task_id: TaskId::new(), session_id: SessionId::new(), agent_key: "worker".into(),
};
let limits = BudgetLimits::default();
let budget = TaskBudget::new(
    identity.clone(), limits, limits, TokenBudgetMode::Soft,
    Arc::new(MonotonicBudgetClock::default()),
)?;
let mut run = budget.begin_run(TurnId::new(), limits)?;
let reservation = run.scope().reserve(BudgetRequest::new(BudgetAmounts {
    steps: 1, ..Default::default()
}))?;
// 没有启动变量资源工作；取消不返还已经接受的步骤计数。
reservation.cancel_before_start()?;
run.finish()?;
let image = budget.seal_checkpoint(None)?;
let bytes = serde_json::to_vec(&image)?;
let image: BudgetCheckpoint = serde_json::from_slice(&bytes)?;
let restored = TaskBudget::restore_checkpoint(
    image,
    BudgetRestoreContext {
        identity, revision: 1, confirmed_anchor: None, host_limits: limits,
    },
    Arc::new(MonotonicBudgetClock::default()),
)?;
assert_eq!(restored.report()?.charged.steps, 1);
assert!(budget.report().is_err()); // 旧账本已封存。
# Ok::<(), Box<dyn std::error::Error>>(())
```

新进程使用新的单调时钟起点，累计活动时间保留，停机和等待用户的时间不计入活动时长。宿主新上限与快照上限取更严格值；已消费量即使超过新上限也不被截断，后续执行被拒绝。恢复接口不能把 Hard 改成 Soft，不能增加原额度或清除已有任务级停止。

## 接入存储前必须了解的边界

`BudgetCheckpointStore` 是可替换的异步 trait，本 crate 没有默认实现，也不在账本锁内进行 IO：

宿主现在可以显式选择独立的 [jingwei-budget-file 文件适配器](file-checkpoint-store.md)，而不是把 IO 依赖引入预算核心。

- `load` 返回指定身份的最新确认快照；不存在快照不代表已有任务可以免费重新创建。
- `compare_exchange` 原子比较 revision 并持久保存下一版，0 表示不存在。`validate_successor` 仅检查数据和版本关系，不能代替存储端原子 CAS。
- 存储还须在真实 CAS 与历史链读取时调用 `validate_transition(previous)`，保护既有审计记录并拒绝未审计增额；结构校验不替代宿主认证或规范日志核验。
- 精确重试同一个版本和相同内容可以得到 `ReplayedExact`；相同 revision、不同内容必须冲突。
- 存储错误区分确定未提交和 `Indeterminate`。结果不确定时应核验原操作，不能换 revision 重试或直接恢复旧快照。

恢复上下文的身份、最新 revision、`confirmed_anchor` 必须由宿主独立确定。`BudgetEventCursor` 包含 Session/Turn/Event ID 和 seq；复制事件地址不等于证明该事件已经持久化。恢复原语只核对它与快照地址相等，不会读取日志、检查游标之后的调用，或自动验证该位置是安全终态。

快照字节必须来自可信存储，输入大小限制由存储/传输接入负责，不接受模型或工具输出的快照。结构校验不是签名或真实性认证。

## 尚未完成的持久恢复闭环

仅在每轮结束后保存一次快照，会遗漏“上次快照之后已经开始工作，但新快照尚未提交”的崩溃窗口。仅 load 后恢复同一份字节，也不能阻止两个进程各恢复一份账本。因此当前 API 不能单独作为生产中的自动恢复方案。

d2-b 已增加显式持久占用与 canonical AgentRuntime 接入，使用时应转交 BudgetExecutionLease，而不是单独 restore 后调用内存入口。d2-c1 已提供安全边界上的审计增额；冻结后的核验及完整故障矩阵仍待 d2-c 后续，不表示 AT-F4-04、完整 JW-04 或 F6 已通过验收。
