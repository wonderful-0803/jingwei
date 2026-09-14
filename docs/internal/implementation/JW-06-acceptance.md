# JW-06 / F3 验收映射

日期：2026-09-14。JW-06 a–d 在 Linux GNU、私有确定性模型与工具、真实 canonical runtimes 的范围内完成。以下不代表真实模型效果、跨平台或完整 v0.1 已验收。

## 验收项

| 验收项 | 实现与证据 | 限定 |
| --- | --- | --- |
| AT-F3-01 五条公共路径 | reference_agent::public_reference_agent_answers_without_a_tool_gateway_and_marks_claim_unverified；acceptance::public_single_tool_path_has_one_execution_and_queryable_closed_evidence；acceptance::complete_example_uses_dynamic_tool_dependency_and_swaps_agent_on_same_runtime；correction::invalid_parameters_receive_structured_feedback_before_any_tool_execution；reference_agent::ask_user_closes_turn_and_host_reuses_same_task_budget_on_reply | Native/Json 两条协议路径；第二个工具的参数来自第一个工具运行时生成的票据，不由预置脚本提供。等待答复走 ReferenceWaiting，继续原 TaskBudget |
| AT-F3-02 有限停止 | correction::local_and_shared_correction_limits_never_start_an_extra_inference、no_correction_is_charged_without_another_step_or_when_disabled；reference_agent::repeated_calls_stop_after_confirmed_third_result、completion_requires_host_evidence_and_rejection_never_retries | 连续非法参数受本地/共享修正上限约束，连续重复工具默认第三次后停止，完成检查首次拒绝即停止；都断言没有额外模型调用 |
| AT-F3-03 完整装配及替换 | acceptance::example 与 ExamplePlugin 显式装配 Agent、模型、两个工具、审批、持久化、授权和四个 canonical runtimes；complete_example_uses_dynamic_tool_dependency_and_swaps_agent_on_same_runtime 在同一 Harness 实例运行 reference 和 custom | 应用只调用 run_turn，没有通用 while/tool loop；DirectAgent 显式完成两次确定性依赖工具调用，不调用模型，仍有相同工具预算、配对记录和终态约束 |
| AT-F3-04 拒绝及收尾证据 | acceptance::user_refusal_never_falls_back_to_another_granted_tool；assert_closed 应用于参考 Agent 和纠错已有集成路径；shutdown_drains_accepted_reference_tool_without_another_inference；uncertain_tool_result_commit_never_replays_and_returns_failure_evidence | 真正经过 ToolAuthorizer 返回 ApprovalDenied，另一个工具已授权且可见，但不再推理、不执行任何工具 body、不扣修正预算。持久化及核对均失败时，检查 TurnFailure 的工具证据、报告/终态/settlement 失败尝试，不伪造完整日志 |

完整可运行内部示例及收尾断言位于 [reference_acceptance.rs](../../../tests/model-protocol/src/reference_acceptance.rs)，其余路径位于 [reference_agent.rs](../../../tests/model-protocol/src/reference_agent.rs) 和 [reference_correction.rs](../../../tests/model-protocol/src/reference_correction.rs)。这些文件留在私有测试 workspace，不进入公共包或指南产物。

## 停止与证据

| 路径 | 验证 |
| --- | --- |
| 模型声明完成、宿主验证完成、拒绝完成、等待用户 | 既有完成/待答测试验证 business_verified、证据及次数；统一断言对应 Done 或 Error 与 TaskRunReport.stop 一致 |
| 步数、纠错及 Task 预算上限 | 本地停止报告及 canonical TaskRunReport；模型/工具次数无越界，所有已接受请求均有结果 |
| 连续无进展与合法轮询 | 重复计数、参数/状态变化、显式轮询阈值与全局步数限制；已确认步骤引用只能指向此前的真实工具事件 |
| 上下文/策略/历史/无效动作、时钟和内容过期 | 既有上下文、策略、大小边界；新增 expiry_clock_and_result_view_stops_preserve_canonical_closure 和 legacy_history_without_complete_assistant_message_stops_before_inference |
| 工具失败、审批拒绝、结果视图失败 | 已接受调用/结果配对，失败保持类别和停止原因，不重放或换工具；视图失败保留已确认原始工具事件 |
| 调用方取消与 runtime 关闭 | pending 工具已进入 body 后取消或关闭；保留取消结果、唯一取消终态和结算，不额外推理 |
| body 报告大小/写入失败 | 既有普通/修正报告失败测试继续执行统一收尾断言；修正预算已扣不返还 |
| 工具结果写入及物理核对失败 | 不对残缺日志使用“完整闭合”断言；类型化 TurnFailure 包含原始 ToolCall/ToolResult 尝试，未确认工具结果计数为 1，预算无悬挂预留，终态与结算失败可分别查询 |

assert_closed 对可成功写完的回合检查：事件序号连续且 ID 唯一；模型/工具请求和结果逐一配对；所有工作先于唯一 TaskRunReport 和唯一末尾终态；报告确认计数与日志一致、预留/未确认/待结算为空；Agent body 的步骤引用指向此前真实事件。它不把 body 的 run_v1 当成收尾权威，也不把无法持久化的事件伪装为已确认。

F3.1–F3.3 对应 [a 批](JW-06-a.md)，F3.6/F3.7 对应 [b 批](JW-06-b.md)，F3.4/F3.5 对应 [c 批](JW-06-c.md)。F3.8/F3.9 由既有受控 runtime、数据受限接口及本次验收补齐证据。完整测试结果见 [d 批记录](JW-06-d.md)。

## 保留边界

无进展检测只识别同一 Turn 的连续重复；宿主仍负责业务证据真实性、用户身份与待答唯一所有权。审批拒绝验证的是受控工具授权链，不宣称模型能可靠理解任意自然语言中的拒绝或提示注入。持久化失败的诊断需宿主保留，不能仅依赖残缺 Session 日志。完整 Task 状态与恢复推进至 JW-07，真实模型评测在 JW-08、平台验证在 JW-10。
