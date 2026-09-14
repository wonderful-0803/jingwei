# JW-05：完整消息、对话投影与上下文

日期：2026-09-14。接续 JW-04-d2-c3，起点 dev / b982870。遵循 PRD F2，不把完整消息持久化等同于整个上下文系统完成。

| 批次 | 范围与完成条件 |
| --- | --- |
| JW-05-a（已实施） | F2.3：规范 AssistantMessage、final_text 持久化、失败/回执/结算屏障、跨进程历史；契约见 [ADR-0012](../adr/0012-canonical-assistant-message.md)，[实施记录](JW-05-a.md)为 Linux 302 项、9/9 通过 |
| JW-05-b（已实施） | F2.1/F2.2：独立确定性对话投影，正确关联用户、最终 assistant、模型工具调用与工具结果；不重复 delta，不把失败或未确认工作当成功；明确旧日志兼容及来源事件；见 [ADR-0013](../adr/0013-conversation-projection.md) 和[实施记录](JW-05-b.md) |
| JW-05-c（已实施） | F2.4/F2.5/F2.8：可替换计数器和有界上下文选择，覆盖 system、消息、schema、模板、输出预留、安全余量；硬模式需要可信计数/上界；保护必要项及完整调用组，提供构建报告；见 [ADR-0014](../adr/0014-context-budget.md) 和[实施记录](JW-05-c.md) |
| JW-05-d（已实施） | F2.6/F2.7/F2.9：授权集合内的工具视图、受控结果精简与内容引用，保留不受信任身份，接入 ActionStep 并映射 AT-F2；见 [ADR-0015](../adr/0015-tool-and-result-views.md)、[实施记录](JW-05-d.md)和[验收映射](JW-05-acceptance.md) |

策略和 IO 分离，先复用既有消息/事件类型，按实际依赖选择可选 crate，不创建空接口或把业务循环放进 AgentRuntime。压缩和投影不得改写审计日志或提升工具输出到 system 权限。参考 Agent 的循环与修正留在 JW-06，完整任务恢复在 JW-07。

各批均执行两套 workspace 的九项检查、指南片段和 VitePress 根/子路径验证。开发分支 dev，提交/推送账户 wonderful-0803；根目录 pelican-ride.html 是用户测试文件，继续忽略并保留。

JW-05 a–d 本地契约实现已完成。JW-06-a 显式装配、有限步骤驱动与停止路径已实施，下一批见 [JW-06 计划](JW-06-plan.md)；真实模型计数和跨平台验收继续按总体计划推进。
