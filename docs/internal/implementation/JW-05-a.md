# JW-05-a：完整 assistant 消息持久化

日期：2026-09-14。开始基线 dev / `b982870`，账户 wonderful-0803。范围为 PRD F2.3，后续计划见 [JW-05-plan](JW-05-plan.md)，契约见 [ADR-0012](../adr/0012-canonical-assistant-message.md)。

## 已交付

- AssistantMessage V1 记录完整 final_text，具有独立 MessageId。canonical AgentRuntime 是规范写者，AgentEventKind 不开放重复提交入口。
- Completed/WaitingForInput 的完整文本（包括空文本）在能力排空和 cleanup 开始后、TaskRunReport/终态之前持久化。delta 原样保留，完整消息是最终正文，不与 delta 再拼接。
- 写入错误和不可信回执以 DriveFailure::AssistantMessage 保留原始 draft 与类型化错误。校验 event_id、message_id、Session/Turn、generation_id 与负载；不匹配时持久预算 claim 不闭合。
- settle 必须保留唯一、准确的完整消息，否则返回 InvalidMessageSettlement，保留期望事件与原始结算窗口，不返回成功报告。
- 进入提交前的失败/取消不虚构完整回复；进入提交阶段后，等待者离开或 shutdown 继续排空已接收工作。后续报告、终态和预算提交仍有独立失败可能，完整消息不是回合成功证据。
- 下一回合历史和真正的新进程都能读取此前完整回复；旧日志不改写、不回填，预算版本及 Cargo 依赖不变。

## 验证

[最终 Linux GNU 基线](../../../results/baseline/20260914-113324-917585-q06fvn9p/summary.json)：**9/9 通过，302 项 Rust 测试**，包括 14 root、274 private、14 guide doctest。Rust 1.96.0，两套 workspace 锁定离线依赖。

新增 10 项私有测试入口（含一个新进程 worker）和一个指南 doctest：最终文本无 delta、流式正文不同于最终文本、空正文、等待输入、后续历史、写入失败、不可信回执、结算遗漏、失败/取消、等待者离开后的提交排空，以及新进程 Session admission。现有预算、故障注入与报告/终态顺序回归全部通过。开发中修正了测试对 crate 私有方法的误用，改以公开 execution_id 状态断言，没有开放内部 API 来迁就测试。

独立 VitePress 根路径和 /jingwei/ 构建、Markdown 测试和产物检查通过：每个产物 16 页、598 个链接、3 个搜索查询，未包含内部源码。公开说明见[完整 assistant 回复](../../guide/src/assistant-messages.md)。

## 尚未交付与接续

本批不等于 F2 整体验收。不提供标准对话投影、工具调用/结果分组、上下文裁剪、硬 token 计数或结果精简；也不根据正文判断业务完成。

下一批 JW-05-b：明确 Completed/WaitingForInput、Error/Cancelled、未关闭回合和旧 delta-only 日志的投影策略，确定性关联用户、完整 assistant 与工具调用/结果，记录来源事件，不改变审计日志。之后再进入上下文预算和受控工具/结果视图。
