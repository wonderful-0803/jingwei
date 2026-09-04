# 任务预算账本

`jingwei::budget` 提供共享内存账本，供受信任的宿主和 runtime 管理任务预算。当前需要显式创建账本、预留、标记执行开始并结算。它尚未自动接入 ModelGateway、ToolGateway 或 AgentRuntime，也没有持久恢复和自动中止任务的能力。

宿主或受信任执行层持有 `TaskBudget`、`BudgetRun`、`BudgetScope` 和 reservation；模型输出、不可信 Agent 或工具数据不能决定预算身份、额度或 token 上界。进程内任意宿主代码仍属于信任边界，这些类型不是插件沙箱。

## 运行一个有限步骤

本例不需要模型、工具、网络或 API key。它创建仅允许一个步骤的 Task，在一次 run 中预留、执行并结算；第二次预留得到明确的预算停止错误。`TaskBudget::new` 需要显式提供 `Arc<dyn BudgetClock>`，示例使用基于单调时间的 `MonotonicBudgetClock`。

```rust
use std::sync::Arc;
use std::time::Duration;

use jingwei::budget::*;
use jingwei::id::{SessionId, TaskId, TurnId};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let limits = BudgetLimits {
        resources: BudgetAmounts {
            steps: 1,
            ..BudgetAmounts::default()
        },
        active_time: Duration::from_secs(60),
    };
    let task = TaskBudget::new(
        BudgetIdentity {
            task_id: TaskId::from("task-budget-guide"),
            session_id: SessionId::from("session-budget-guide"),
            agent_key: "host-step".to_owned(),
        },
        limits, // 宿主上限
        limits, // Task 累计上限
        TokenBudgetMode::Hard,
        Arc::new(MonotonicBudgetClock::default()),
    )?;
    let mut run = task.begin_run(TurnId::from("turn-budget-guide"), limits)?;
    let scope = run.scope();
    let request = BudgetRequest::new(BudgetAmounts {
        steps: 1,
        ..BudgetAmounts::default()
    });

    let mut reservation = scope.reserve(request)?;
    reservation.mark_started()?;
    // 在此执行宿主确定的本地步骤；本例没有 token 或工具输出消耗。
    reservation.settle(BudgetUsage::actual(0, 0, 0))?;

    assert!(matches!(
        scope.reserve(request),
        Err(BudgetError::Stopped(BudgetStopReason::ResourceLimit(
            BudgetResource::Steps
        )))
    ));
    let report = run.finish()?;
    assert_eq!(report.charged.steps, 1);
    assert!(report.pending.is_empty());
    let closed_run = report.run.as_ref().expect("finish 保留本次 run 的证据");
    assert!(!closed_run.open);
    assert_eq!(closed_run.charged.steps, 1);
    assert!(task.report()?.run.is_none());
    Ok(())
}
```

预期结果是程序成功结束，累计步骤数为 1，第二次预留被拒绝且不收费。`BudgetAmounts::default()` 的各资源为零；零限额表示禁止该资源，不表示无限制。这里没有请求模型，所以没有让不可信代码声称 token 上界为零。

## 身份、限额和累计范围

`BudgetIdentity` 固定 TaskId、SessionId 和 Agent key。一个 Task 同时仅能有一个 active run；重复 `begin_run` 返回 `RunActive`。run 内可以克隆 `BudgetScope` 进行并发预留，所有克隆共用一个原子账本。

宿主和 Task 各维度的有效限额取最小值，run 限额再进一步收紧。每次预留同时检查 Task 的剩余额度和 run 的剩余额度。例如，宿主允许 8 步、Task 允许 5 步、run 允许 3 步，则本 run 最多 3 步；若前次 run 已消耗 4 步，本次最多再接受 1 步。新 run 只重置本次运行计数，不重置 Task 累计值。

`BudgetLimits` 包括步骤、模型请求、工具调用、修正、输入/输出 token、工具输出字节和活动时长。一次 `BudgetRequest` 至少消费一种尝试次数；预留成功时 steps/model_requests/tool_calls/corrections 就计入 `charged`，即使随后执行前取消也不返还。变量资源先进入 `reserved`，然后按证据结算。任何维度不满足时整个预留被拒绝，不会部分扣费。

`BudgetReport.stop` 表示 Task 停止原因，`report.run` 中的 `stop` 表示当前或刚结束 run 的停止原因。只有 run 限额耗尽时，成功结束该 run 后，Task 可在剩余额度内开始后续 run；Task 已停止时不能通过新 run 清零或解除停止。

## 用量的四种含义

报告中的计费值与证据分别保存，不能把 `charged` 全部当作上游实测用量。

| 含义 | API 与计费规则 |
| --- | --- |
| 实际值 | `UsageValue::Actual(n)` 是可信执行层确认的实际消耗。结算按 n 计费，未使用的预留返还；超过预留时保留原始 n 并停止后续准入 |
| 预留值 | `BudgetRequest.amounts` 指定变量预留，未结算时在 `reserved` 中占用额度；它不是实际用量证据 |
| 估计值 | `TokenBoundEvidence::Estimate` 标记准入估算；`UsageValue::Estimated(n)` 标记结算估计。后者按 `max(n, 预留值)` 保守计费，较低估计不会释放额度 |
| 未知值 | `UsageValue::Unknown` 表示没有实际或估计证据。结算将相应全额预留转入 `charged`，清除已结算的 `reserved`，并在 `usage` 中记录 unknown 次数 |

`UsageTotals.actual` 和 `estimated` 是各自证据之和，`unknown` 是未知结算的次数，不是 token 数。没有 usage 时不能传入 `Actual(0)` 来获取余额。若实际或估计结算超过预留，`settle` 返回 `UsageExceededReservation`，同时保留已经计入的原始证据；不能把该错误当作“没有执行”。算术溢出时任务停止，未决报告保留 `failed_usage`，不静默丢弃证据。

`TokenBudgetMode::Hard` 要求模型请求的输入、输出都带有 `TokenBoundEvidence::VerifiedUpperBound`。该枚举只传递信任声明，不会运行 tokenizer 或验证后端；证据必须来自受信任宿主或执行层，不能根据模型或不可信 Agent 的自述设置。单独传入 `max_tokens` 也不能证明所有实际用量的上界。

`TokenBudgetMode::Soft` 允许估计值，但不能宣称硬 token 保证。模型请求使用估计时，输入和输出预留都必须大于零，否则返回 `ZeroTokenEstimate`。有限请求次数和活动时长仍约束准入；任务执行者还必须响应停止状态。

## 保留执行与结算的所有权

`BudgetReservation` 不可 Clone。受信任执行层在可能消耗变量资源之前调用 `mark_started`；只有能确定尚未开始时，才调用 `cancel_before_start` 返还变量预留。该取消不返还尝试次数。开始之后，无论成功、失败或取消，都应根据已知证据调用 `settle`，无法确认用量时使用 Unknown。

账本停止后仍允许结算已接受的工作。`BudgetRun::finish(&mut self)` 遇到 pending 返回 `PendingReservations`；保留 run，完成结算后可再次调用。成功时返回的报告保留 `run: Some(...)`，其中 `open` 为 false；随后 `TaskBudget::report()` 的 `run` 为 None，Task 累计值仍保留。

丢弃未结算 reservation 会保留 pending、标记 abandoned，并冻结 Task；它不会自动退款或重试。丢弃尚未成功 finish 的 run 同样冻结 Task。当前没有从这些冻结状态恢复的公开入口，因此宿主必须保留句柄并显式收尾。报告可以观察错误，不授予解冻或重新创建账本的权限。

## 活动时间和清理时间

账本在操作或 `report()` 时采样可信单调时钟；读取报告也可能发现截止时间已到并记录停止。`active_time` 累计各 run 的活动墙钟时间，不叠加并行调用耗时。到达 Task 或 run 截止时间后，超过截止点的采样时间计入 `cleanup_time`；其他预算停止之后的时间也计入 cleanup。成功 finish 后，用户等待不再增加两项时间。

清理时间是账本的时间分类，不证明 provider、工具或持久化已经停止。当前没有后台计时器，也不会自动取消 future；宿主必须检查停止并管理实际执行与清理。时钟反向会记录 `ClockMovedBackwards` 并停止新准入，已有 reservation 仍能结算；不会用反向时间返还消耗。

后续 runtime 接入需要在 Session/模型排队之前开始计时，排队和执行共用剩余额度。本批仅实现账本计时，没有自动覆盖现有 gateway 的等待路径。

## 当前边界与下一步

预算报告虽可序列化，但不是持久快照，反序列化不能创建可执行账本。当前没有跨进程恢复、版本化预算日志、CAS 更新或审计增额；跨 run 累计只限于同一内存账本。v0.1 后续恢复必须保留累计消耗，不能通过重启重置预算。

后续将分别接入有限模型调度、Agent/Model/Tool 的共享预算与停止报告，再实现预算持久化和恢复。无显式 Task 的现有 Echo/自定义 Agent 如何绑定默认有限作用域，以及单调用限额如何由调用参数进一步收紧，都属于 runtime 集成阶段的决策。现有调用入口仍按当前 runtime 行为运行，不能因为引入此模块就声称已经受到 Task 预算保护。

需要推进一次模型决策时，可阅读[单步动作执行](action-step.md)；当前 `ActionStep` 也尚未自动消费本账本。
