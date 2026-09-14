# ADR-0016：有限参考 Agent 的基础驱动与停止

状态：已采纳，2026-09-14。范围 JW-06-a，接续 ADR-0015。

## 决策

新增独立可选 jingwei-reference-agent，facade reference-agent feature 显式启用并联动 actions/context。实现现有 Agent trait，宿主通过普通插件注册；不修改 AgentRuntime、SessionRuntime、模型适配器或 StandardCoreBundle，不自动选择 provider、路径或工具权限。

ReferenceAgentConfig 显式提供模型/模板和上下文预算；默认每 Turn 最多 8 步，合法范围 1–1024。持有的策略只接收数据、schema 或内容存储契约，不接收网关执行权/增额权。驱动确认 ctx.budget 的 Task/Session 身份、投影历史，并使用受保护 system/pinned_turns 和当前对话调用 ContextualActionStep；每步只接受一个动作，成功工具反馈进入下一步。

模型 runtime 已为每次推理扣一步，驱动不再次 consume_step。累计 Task 限额和取消仍由 canonical runtime 控制，不能因为 max_steps 在新 Turn 重新计数就重置 Task。没有工具网关时使用空目录，仅支持协议的 Final/AskUser；需要工具的宿主插件必须声明 TOOL_RUNTIME 能力并被授予工具。

Final 返回 Completed，但 artifact/报告的 business_verified 为 false，停止原因为 ModelClaimedComplete。AskUser 返回 WaitingForInput，artifact 保留 TaskId/TurnId/pending_question；宿主显式提交答复并复用原 TaskBudget，不在 Agent 内等待。a 批不执行完成检查、语义无进展检测或纠错，任何动作/工具错误均停止，不切换工具或重放。

## 报告与收尾

通过受限 Custom 事件记录 jingwei.reference 的 step_v1/run_v1。步骤报告包含 DecisionContext、计数/工具视图和已确认事件引用；confirmed_steps 表示成功提交步骤报告的数量，不代替 TaskRunReport 中的实际消耗。每个报告负载默认 256 KiB，写入或大小失败不得放行后续步骤或成功回复。

Custom 报告是 body 观察，写入在 canonical 完整回复/TaskRunReport/终态之前。runtime 的预算中止和取消可提前中断 body，run_v1 不保证存在；原模型、工具、Session 和预算类型化错误直接保留，不为追加自定义报告覆盖它们。原始 ToolExecution 的规范日志仍可查询，工具已执行后的视图/报告失败不能重放。最终以能力排空、TaskRunReport、终态及检查点/结算为准。

引用使用宿主时钟和本回合有限作用域，时钟回拨、过期或溢出停止新步骤；存储容量由宿主配置。时钟与步数不提供对任意阻塞代码的强制终止能力，完整恢复仍属 JW-07。

## 验证与接续

真实 canonical runtime/内存持久化中运行参考 Agent，覆盖无工具回答、两步工具依赖、步数耗尽、等待答复后同 Task 续跑、预算抢先停止、取消排空、报告失败及同 runtime 下替换自定义 Agent。详细结果见 [JW-06-a](../implementation/JW-06-a.md)。下一批增加完成检查与无进展策略。
