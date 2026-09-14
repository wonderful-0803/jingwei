# JW-05 / F2 验收映射

日期：2026-09-14。a–d 的本地契约实现完成；本表描述 Linux GNU、私有模型/工具和真实 Session 记录范围，不代表真实模型 tokenizer、全部平台或完整 v0.1 已验收。

| 验收项 | 实现与证据 | 限定 |
| --- | --- | --- |
| AT-F2-01 确定性上下文/报告 | projection::identical_snapshots_produce_identical_messages_and_source_reports_without_mutation；context_builder::repeated_builds_and_serialized_reports_are_deterministic；context_views::grouped_selection_is_sorted_deterministic_and_preserves_exact_authorized_definitions | 相同输入、配置、宿主计数方法、引用 scope/time 和存储内容；运行产生的新 StepId 不是上下文确定性输入 |
| AT-F2-02 完整/流式/工具/版本化历史 | agent_budget/durable/messages 的 final-only、流式、空正文、Waiting、新进程 JSONL；projection 的配对、旧日志和未确认工作拒绝；context_actions 的 Native/Json 两步工具往返 | 旧缺失正文明确 Reject 或 Omit，不回填日志；内存内容引用不跨重启 |
| AT-F2-03 长历史/结果/schema 与预算 | context_builder 的 80 回合中文长结果、exact_counts、native_schema_growth、nonmonotonic、必要项超限、软预算标签；context_actions::budget_failure_including_protocol_schema_happens_before_inference | 精确计数针对私有字节模型；真实 provider 的硬计数/输出上界由宿主验证，内置仅估算 |
| AT-F2-04 策略独立与日志不变 | ContextBuilder/ContextTokenCounter、ToolSelector/ToolResultPolicy/ContentStore 接口；context_actions::both_protocols_send_exact_budgeted_request_and_keep_result_data_scoped 验证完整 ToolResult 和既有事件前缀未变 | 本批未修改 AgentRuntime、SessionRuntime、provider；官方组合只接 Native/Json，自定义协议自行装配底层策略 |
| AT-F2-05 授权与提示注入 | context_views::malicious_selection_unknown_groups_duplicates_and_limits_fail_closed；context_actions::unauthorized_custom_selection_is_rejected_before_inference_or_execution、empty_visible_set_and_injected_followup_cannot_execute_ungranted_tools、tool_prompt_injection_in_followup_cannot_expand_execution_authority、denied_tool_result_remains_failed_after_view_projection | 验证不可通过输出扩大工具名称权限，非模型语义免疫保证；读取引用须另行授权并注入可信作用域 |

F2.1–F2.3 见 [JW-05-a](JW-05-a.md)、[JW-05-b](JW-05-b.md)；F2.4/F2.5/F2.8 见 [JW-05-c](JW-05-c.md)；F2.6/F2.7/F2.9 见 [JW-05-d](JW-05-d.md)。最新完整基线为 [341 项 / 九项通过](../../../results/baseline/20260914-140246-187036-dcw2ft6o/summary.json)。

构建报告、视图和引用都是派生数据，由宿主按自己的持久策略保存，不覆盖规范事实。生产 tokenizer、跨平台和端到端真实模型评测继续在 JW-08/JW-10 跟踪；下一功能阶段进入 JW-06 有限参考 Agent。
