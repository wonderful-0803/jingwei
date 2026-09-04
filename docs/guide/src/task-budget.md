# 任务预算与运行报告

`jingwei::budget` 提供共享内存账本，供受信任的宿主和 runtime 管理任务预算。canonical AgentRuntime 把同一预算作用域注入受控 ModelGateway 和 ToolGateway，累计模型、工具、步骤、修正、token、工具输出字节和活动时长；预算停止后关闭执行并保存运行报告。账本仍在内存中，尚无跨进程恢复。

宿主或受信任执行层持有 `TaskBudget`、`BudgetRun`、`BudgetScope` 和 reservation；模型输出、不可信 Agent 或工具数据不能决定预算身份、额度或 token 上界。进程内任意宿主代码仍属于信任边界，这些类型不是插件沙箱。

## 让 Agent 使用同一 Task

宿主通过 `AgentTurnRequest::with_budget` 移交共享账本和本次 run 上限。下面的函数接收已装配的 Harness 和已注册 Agent 名称；它不会在教程中创建 provider 或连接网络。应用调用该函数时，正常执行所选 Agent。

```rust
use std::sync::Arc;
use jingwei::{AgentTurnReport, Harness};
use jingwei::agent::AgentTurnRequest;
use jingwei::budget::*;
use jingwei::id::{SessionId, TaskId};

async fn run_budgeted_turn(
    harness: &Harness,
    session_id: SessionId,
    agent_key: &str,
) -> Result<(TaskBudget, AgentTurnReport), Box<dyn std::error::Error>> {
    let mut limits = BudgetLimits::default();
    limits.resources.steps = 8;
    limits.resources.model_requests = 8;
    limits.resources.tool_calls = 4;
    let task = TaskBudget::new(
        BudgetIdentity {
            task_id: TaskId::new(),
            session_id: session_id.clone(),
            agent_key: agent_key.to_owned(),
        },
        limits,
        limits,
        TokenBudgetMode::Soft,
        Arc::new(MonotonicBudgetClock::default()),
    )?;
    let request = AgentTurnRequest::new(session_id, agent_key, "执行下一步")
        .with_budget(task.clone(), limits);
    let report = harness.start_turn_request(request)?.wait().await?;
    assert!(report.task_run_report().is_some());
    Ok((task, report))
}
```

返回的 TaskBudget 仍是同一账本。下一次 Turn，包括 WaitingForInput 后收到用户答复时，宿主应复用该句柄并保持相同 Session/Agent 身份；TaskId 字符串相同却重新创建账本不会保留旧消耗，也不是恢复。显式身份冲突在 Agent 执行前拒绝。

未传入 Task 的 Echo 和自定义 Agent 入口使用本次运行的有限临时 Task。canonical AgentRuntime 的 `with_budget_limits` 可配置宿主上限；默认采用 `BudgetLimits::default()`：

| 资源 | 默认上限 |
| --- | --- |
| 步骤 / 模型请求 / 工具调用 | 各 128 次 |
| 修正 | 32 次 |
| 输入 / 输出 token | 4,000,000 / 1,000,000 |
| 工具输出字节 | 16 MiB |
| 活动时长 | 600 秒 |

默认临时 Task 使用 Soft token 模式，不自动跨 Turn 关联。宿主直接绑定 canonical 模型/工具 runtime 时也得到有限临时作用域；传入显式 `with_budget(BudgetScope)` 才与宿主持有账本共享余额。显式绑定核对规范 Session/Turn 和可选动作 TaskId；默认直接绑定允许旧调用保留独立的 opaque 关联，但这些关联不授予预算权限。

Agent 通过 `ctx.budget()` 获得窄 `AgentBudget` 接口：`report()`、`consume_step()` 和 `consume_correction()`。每个接受的模型调用自动计一步和一次模型请求；工具调用计一次工具调用。摘要走相同模型入口。无模型的确定性步骤可显式 `consume_step()`，修正应显式 `consume_correction()`；不要给已由模型入口计费的同一步再扣一次。ActionStep 使用这两个 gateway，无需另持账本或自行结算。

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

`BudgetIdentity` 固定 TaskId、SessionId 和 Agent key。一个 Task 同时仅能有一个 active run；重复 `begin_run` 返回 `RunActive`。run 内可以克隆 `BudgetScope` 进行并发预留，所有克隆共用一个原子账本。AgentRuntime 在 Session 等待前调用 `begin_admission`，取得真实 TurnId 后再 `bind_turn`；因此 `BudgetRunReport.turn_id` 为 Option，准入失败时可以是 None。

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

canonical 模型 runtime 默认采用 Soft 估算：输入为请求 JSON 字节数加 1024，输出为 `max_tokens` 或 4096，且至少为 1。这是启发式估计，不是 tokenizer 计数或后端强制上界。宿主可通过 `CanonicalLlmRuntimePlugin::with_budget_estimator` 安装可信的 `ModelBudgetEstimator`，为输入和输出分别提供数值及证据；它必须同步、非阻塞。使用 Hard Task 时，两维都需要 VerifiedUpperBound，否则在 provider 执行前拒绝。Agent 调用选项没有提交该证明的入口。

## 保留执行与结算的所有权

`BudgetReservation` 不可 Clone。受信任执行层在可能消耗变量资源之前调用 `mark_started`；只有能确定尚未开始时，才调用 `cancel_before_start` 返还变量预留。该取消不返还尝试次数。开始之后，无论成功、失败或取消，都应根据已知证据调用 `settle`，无法确认用量时使用 Unknown。

账本停止后仍允许结算已接受的工作。`BudgetRun::finish(&mut self)` 遇到 pending 返回 `PendingReservations`；保留 run，完成结算后可再次调用。成功时返回的报告保留 `run: Some(...)`，其中 `open` 为 false；随后 `TaskBudget::report()` 的 `run` 为 None，Task 累计值仍保留。

runtime 需要先持久记录再释放 Task 时，可调用 `prepare_report()` 关闭消费并取得本 run 的报告，待记录屏障结束再 `finish()`。两者之间报告的 run 已关闭消费，但 Task 租约仍占用，新 run 仍返回 `RunActive`；不能仅凭 `open: false` 判断 Task 已可重入。

丢弃未结算 reservation 会保留 pending、标记 abandoned，并冻结 Task；它不会自动退款或重试。丢弃尚未成功 finish 的 run 同样冻结 Task。当前没有从这些冻结状态恢复的公开入口，因此宿主必须保留句柄并显式收尾。报告可以观察错误，不授予解冻或重新创建账本的权限。

## 活动时间和清理时间

账本在操作或 `report()` 时采样可信单调时钟；读取报告也可能发现截止时间已到并记录停止。`active_time` 累计各 run 的活动墙钟时间，不叠加并行调用耗时。到达 Task 或 run 截止时间后，超过截止点的采样时间计入 `cleanup_time`；其他预算停止之后的时间也计入 cleanup。无 pending 时，prepare_report 关闭 run 后停止采样；直接 finish 也结束采样，后续用户等待不再增加两项时间。

清理时间是账本的时间分类，不证明 provider、工具或持久化已经停止。账本自身不创建后台计时任务；canonical runtimes 负责响应 deadline、停止执行和管理收尾。任意阻塞插件仍需要宿主控制，预算不会强制杀死线程。时钟反向会记录 `ClockMovedBackwards` 并停止新准入，已有 reservation 仍能结算；不会用反向时间返还消耗。

Session 等待、模型排队和执行均计入同一 Task 活动时间。单次模型/工具 timeout 与任务截止时间同时约束工作，不能在出队或重新绑定时重置 Task 剩余时间。预算停止后的耗时归清理；正常完成路径在能力排空后进入清理。因此并非所有 scope finish 等待都会自动从活动时间中扣除。

## 运行报告和终态

canonical AgentRuntime 先排空模型/工具能力、结算预算，通过 `prepare_report()` 关闭消费并持有 Task 租约，然后记录专用 `TaskRunReport` 事件，追加唯一 Done/Error，settle Session，最后 `finish()` 释放 Task。Agent 自己不能提交该专用事件。即使 Agent 捕获预算错误并返回 Completed，runtime 仍依据 Task 与 run 的停止状态处理，不能把预算耗尽报告为成功。

TaskRunReport 使用独立的数字 `version: 1`；缺失、未知版本及字符串 `"1"` 拒绝。ModelRequest/ModelResult V1 保持原义。报告包含身份、Task 与本次 run 的预算、停止原因、未决项及能力是否排空；run.metrics 给出 Session 等待、模型排队/执行、工具执行时长和请求/结果事件的 confirmed/unconfirmed 计数。并行执行耗时指标可累加，不能当成活动墙钟时间。

`AgentTurnReport::task_run_report()` 仅返回已确认写入的报告。报告写入失败使 Turn 失败，`TurnFailure::task_run_report_attempt()` 保留 `Failed { report, source }`；已确认报告则为 `Committed { report, event }`。append 返回的事件仍须匹配 Session、Turn、种类和完整负载；异常回执保留为 `Invalid { report, event }`，不能当作成功或自动重放的依据。先前模型/工具失败证据也会保留。

未获 Session 租约的预算停止通过 `NotAdmittedFailure::Budget` 返回观察，不伪造 Session 事件。已取得租约若属于其他 Session，runtime 在 UserMessage 和 Agent 执行之前拒绝，不绑定预算 TurnId，也不向错误 Session 提交预算报告；只尝试 Error 和 settle 关闭该租约，以 `Rejected { report, error }` 保留 IdentityMismatch。等待期间可用 `AgentTurnController::budget_report()` 观察；自定义 controller 默认 None，表示没有提供该观察。

持久报告在自身 append、终态 append 和 Session settle 之前采样，不包含这些之后发生的等待；当前账本也不会在 finish 时补计这段延迟。TaskBudget 的当前观察不能替代已持久化的事件。报告是运行证据，不是跨进程恢复检查点。

## 当前边界与下一步

预算报告虽可序列化，但不是持久快照，反序列化不能创建可执行账本。当前没有跨进程恢复、版本化预算日志、CAS 更新或审计增额；跨 run 累计只限于同一内存账本。v0.1 后续恢复必须保留累计消耗，不能通过重启重置预算。

[有限模型调度](model-scheduling.md)提供执行槽、等待队列、总在途容量与单次超时控制，共享 Task 预算补充累计限制。后续实现预算持久化、未决状态重建和审计增额；当前运行报告及同进程累计不代表 F4 整体验收或恢复完成。

需要推进一次模型决策时，可阅读[单步动作执行](action-step.md)；官方完整循环、自动修正策略、上下文裁剪和任务恢复仍在后续阶段。
