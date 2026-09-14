# ADR-0012：完整 assistant 消息的规范写入

状态：采用。日期：2026-09-14。范围：JW-05-a / F2.3。

## 问题

AgentTurnReport 保存 final_text，但 Session 只有 UserMessage 和 AssistantDelta。只返回 final_text 的 Agent 重启后丢失完整回复；直接拼接 delta 又不能表达 Agent 最终修订后的正文。对话投影不能依赖一次内存回执。

## 决策

- 新增 AssistantMessage，数字版本 1 必填，含完整 text；信封有独立 MessageId。canonical AgentRuntime 唯一写入，AgentEventKind 不开放该事件。
- 对 Completed/WaitingForInput 写入，包括空文本；final_text 是正文来源，delta 保留为过程记录，未来投影不能双重拼入。失败/取消在进入提交阶段前不生成完整消息。
- 排空模型/工具、进入 cleanup 后写入完整消息，再准备并提交 TaskRunReport、终态、settle、最终预算快照。已接收的消息 IO 受 runtime 所有，等待者离开或 shutdown 不能丢弃它。
- 校验准确 draft event_id、MessageId、Session/Turn、generation_id 和负载。失败保留 AssistantMessageFailure，正常错误终态继续尝试，但不返回成功；不可信回执导致持久预算不闭合。
- settle 必须包含且仅包含一份期望完整消息。若不满足，以 InvalidMessageSettlement 保留原始窗口和期望事件，作为结算失败返回，不能伪造成功回执。
- 完整消息仅确认正文，后续报告和关闭仍可能失败；未来投影应结合终态处理，不能以一个消息事件替代成功状态。

## 兼容与后续

旧事件和旧日志不修改、不回填。新增事件让旧消费者的穷尽匹配和反序列化显式暴露兼容问题；预算版本、存储外层版本不变，无新增依赖。

本批不交付完整 F2。JW-05-b 再定义确定性对话投影（用户、最终消息、工具调用/结果、失败及旧日志），JW-05-c 实现上下文预算、工具/结果视图和构建报告。历史审计和模型上下文始终分离。
