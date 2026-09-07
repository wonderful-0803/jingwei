# ADR-0006：受控运行时共享预算与 Task 运行报告

日期：2026-09-04。状态：JW-04-c 实施契约，实际检查结果另行记录。关联 F4.1/F4.2/F4.4–F4.9。基于 ADR-0004 的内存账本和 ADR-0005 的模型调度；持久恢复仍在 JW-04-d 验收。

## 宿主移交与默认作用域

- `AgentTurnRequest::with_budget(TaskBudget, BudgetLimits)` 将宿主持有账本的共享句柄和本次 run 限额交给 AgentRuntime。共享句柄不复制余额，同一 Task 仍只能有一个 active run。
- 显式 Task 固定绑定 Session 和 Agent key。AgentRuntime 在接受执行前检查身份，错误绑定不得启动 Agent、模型或工具，也不能根据调用 options 的 TaskId 自动选择另一账本。
- 无显式 Task 的既有 Echo/自定义 Agent 入口继续可用，由 canonical AgentRuntime 创建本次运行的有限临时 Task。默认不是无限额度；临时 Task 只服务本次调用，不承诺下一次请求或 AskUser 答复自动关联旧账本。需跨 Turn 累计时，宿主显式复用同一 TaskBudget。
- 直接绑定模型或工具 runtime 的宿主入口也必须有有限作用域。默认直接绑定使用临时作用域，允许历史调用使用独立的 opaque Task/Step 关联；显式 `with_budget(BudgetScope)` 严格检查 Task 和规范 Session/Turn 身份。默认兼容路径不等于把关联字段升级为预算凭证。

## Session 准入与时间

当前 SessionRuntime 在发放租约时才生成真实 TurnId，因此预算需要独立的 admission 阶段。`TaskBudget::begin_admission(limits)` 开始计时并创建尚未绑定 TurnId 的 run；`BudgetRunReport.turn_id` 使用 Option 表示这一事实，不能虚构 TurnId。

拿到 Session 租约后，AgentRuntime 通过 `BudgetRun::bind_turn` 绑定该真实 TurnId；已有 `begin_run(turn_id, limits)` 继续表示直接建立带身份的 run。Session 等待、模型排队、执行必须消耗同一任务活动时长。未获得 Session 租约时失败，不伪造规范 Session 事件，但返回已消耗的准入时间及停止证据。

绑定之前必须核对已取得租约的 SessionId 与请求和预算身份一致。自定义 SessionRuntime 若交回其他 Session 的租约，AgentRuntime 不写 UserMessage、不绑定预算 TurnId、不进入 Agent，也不向错误 Session 写 TaskRunReport；仅尝试 Error 和 settle 关闭已取得租约。TaskRunReportAttempt::Rejected 保留报告和 IdentityMismatch，报告的预算 TurnId 仍为 None。

预算截止时间不仅在新调用时检查：AgentRuntime 需要在 Session admission、Agent body 和能力收尾边界响应期限与预算停止，模型/工具已有调用同样使用受控取消路径。预算错误被 Agent 捕获后，不得把已停止的 run 重新报告成 Completed 或 WaitingForInput。

## 能力绑定和计量

- AgentRuntime 向 ModelTurnBinding、ToolTurnBinding 注入同一个 BudgetScope；显式直接绑定的宿主也采用相同契约。账本累计限制与单调用、provider 和调度限制同时取严，不能为两种 runtime 各复制一套可独立透支的余额。
- AgentContext 只暴露窄 `AgentBudget` 入口：观察 report、consume_step、consume_correction。Agent 不取得 TaskBudget 所有权、任意 reservation、结算权限、增额权限或 token 上界证明权限。
- 每个被接受的受控模型调用计 steps=1、model_requests=1，工具调用计 tool_calls=1；摘要等模型工作走同一入口。无模型确定性工作可显式 consume_step，修正通过 consume_correction 计量。ActionStep 不在模型已经计费后重复收取同一个步骤。
- 调度容量与预算预留共同定义接受边界。预留失败不执行工作；已成功预留的尝试次数永久消费。若执行尚未开始就取消，只能返还变量预留，不能把计费次数回滚为未接受。
- token 计数或上界证据由宿主/受信任模型执行策略提供，不接受 Agent 在调用 options 中自述 VerifiedUpperBound。未知 usage 按预留保守计费；Actual/Estimated/Unknown 不互相伪装，实际超额仍保留原值。
- 工具继续先执行既有 schema/grant/guard/审批边界。正文开始之前才标记变量资源开始消耗；失败或取消保留实际/未知证据，不能用预算错误抹除已确认的工具结果。

## 规范报告与终态屏障

新增专用 `SessionEventKind::TaskRunReport { report: Box<TaskRunReport> }`，负载有独立显式数字 `version: 1`。新报告由 AgentRuntime 生成和记录，AgentEventKind 不提供该变体；不能使用 Agent 可提交的 Custom 或业务 artifact 充当规范预算证据。

ModelRequest/ModelResult 的 version 1 和 options 语义不因新增报告而改变。TaskRunReport 版本单独校验，无版本或未知版本拒绝；其负载是结算观察，不是可反序列化为执行权限的持久恢复状态。

成功收尾的顺序为：

1. 禁止继续接受该 Agent body 的新工作，排空模型和工具 scope，保留所有记录失败或未决证据。
2. 完成预算结算并通过 `BudgetRun::prepare_report` 关闭本 run 的消费，捕获本次报告但保留 Task 租约。此时 open 为 false，新 run 仍必须返回 RunActive。
3. 将版本化 TaskRunReport 提交到当前 Session/Turn，并确认记录结果。
4. 依据 runtime 的最终处置追加唯一 Done/Error，然后 settle Session。
5. Session 收尾返回后再 `finish` 释放 Task 租约，返回与已确认事件一致的 AgentTurnReport；失败路径返回类型化报告尝试、终态和 Session 结算证据。

TaskRunReport 的预算时间采样点在其自身 append、终态 append 和 Session settle 之前。当前 prepare_report 在无 pending 时关闭 run，后续 finish 不补计这段延迟。不能把该聚合报告当作完整清理计时或跨进程检查点。后续完整运行报告若增加这些字段，须明确各自采样点与持久/内存来源。

TaskRunReport 写入失败时，不允许返回 Completed/WaitingForInput，`AgentTurnReport::task_run_report()` 只暴露确认提交的报告。失败对象通过 TaskRunReportAttempt::Failed 保留尝试的版本化负载与 Session 错误，并以 DriveFailure::BudgetReport 的 prior 保留此前 drive failure；即使 Error terminal 和 Session settle 成功，也只能表示以失败处置完成了本 Turn 的规范关闭。报告失败不能丢弃此前的模型/工具证据。

append 返回 Ok 仍需核对返回事件的 SessionId、TurnId、种类和完整报告负载。任何不匹配都通过 TaskRunReportAttempt::Invalid 保留尝试报告及异常回执，按 Failed 收尾，getter 不把该回执作为已确认报告。异常回执不能证明原事件未提交，也不许可自动重放。

Task 级 stop 和 run 级 stop 都参与最终处置。预算耗尽、时钟异常、计量溢出、放弃未决 reservation 等原因保留类型化语义，不统一改名为用户取消。外部副作用不会因预算停止撤销，也不会自动重试。

## 验证边界

本批需覆盖默认有限域、显式身份冲突、同 Task 跨 Turn 累计、无模型步骤/修正、模型与工具共同计费、Agent 吞错后的真实终态、Session 排队耗时、能力排空、TaskRunReport 记录失败、未知版本和唯一终态顺序。

同进程多个 Turn 复用共享账本只验证内存累计。Task 状态存储、事件/快照 CAS、一致性协议、跨进程恢复和审计增额仍在 JW-04-d/JW-07；不得用本批聚合报告宣布 AT-F4-04 或 F6 完成。
