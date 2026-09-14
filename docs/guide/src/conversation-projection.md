# 确定性对话投影

对话投影将 Session 的规范事件转换为模型消息。同一份完整历史、配置和算法版本会得到同一份结果；它不调用模型、不执行工具，也不修改日志。

## 显式启用

在同一源码快照的 jingwei 依赖上启用 `context` feature，使用 `jingwei::context`；也可以直接依赖独立的 jingwei-context。默认 facade 不启用此组件。

```rust
use jingwei::context::{
    CanonicalConversationProjector, ConversationProjector,
    ConversationProjection, ProjectionConfig, ProjectionError,
};
use jingwei::event::SessionEvent;
use jingwei::id::SessionId;

fn project_history(
    session: &SessionId,
    history: &[SessionEvent],
) -> Result<ConversationProjection, ProjectionError> {
    CanonicalConversationProjector.project(session, history, ProjectionConfig::default())
}

let projection = project_history(&SessionId::from("example"), &[])?;
let messages: Vec<_> = projection.messages().cloned().collect();
assert!(messages.is_empty());
assert_eq!(projection.algorithm_version, 1);
# Ok::<(), ProjectionError>(())
```

输入必须是从 seq 0 开始的完整物理 Session 快照，不能传入仅包含后续某个 Turn 的报告片段。AgentTurnInput.history 提供既有历史，当前 user_message 单独提供；宿主组装请求时应避免重复加入当前用户消息。

## 消息与调用规则

- 用户事件产生 user 消息；成功完成或等待输入的 Turn 使用规范 AssistantMessage，保留空回复。AssistantDelta 只记入省略报告，不重复拼接到完整回复。
- 实际 ToolCall 与 ToolResult 在同一 Turn 内配对，形成不可拆分的 ToolExchange 组。组按调用事件顺序排列，即使并行结果逆序完成也不改变顺序。
- 模型提议及 ModelRequest/ModelResult 作为审计记录保留来源，不直接当作执行过的工具。投影使用基于调用事件 seq 的合成 ID 关联 assistant 调用与 tool 结果，避免不同 Turn 重用原始 ID 时冲突。
- tool 消息保留完整 ToolRecordedOutcome，包括成功或失败状态、错误信息等。工具内容不会变成 system 消息，合成 ID 和历史消息也不提供执行授权。

失败或取消的 Turn 保留用户输入及已确认的工具交换，但不会输出该 Turn 的完整 assistant 回复。调用方应同时读取 turns 中的状态，不能把保留的工具结果理解为整个 Turn 已成功。

## 拒绝与旧日志

物理序列、Session 身份、重复事件、跨 Turn 的结果、孤立或重复结果、终止后的事件及重复完整回复会被拒绝，不自动排序或修补。未配对的模型或工具调用返回 Unconfirmed，失败或取消也不能掩盖未确认工作。

只有最后一个尚未终止且仅含用户消息的 Turn 可以投影为 Open；包含其他未终止工作时拒绝。投影不判断外部副作用是否发生，也不承担任务恢复。

旧的成功 Turn 缺少 AssistantMessage 时，默认返回 MissingReply。宿主可以明确设置 `ProjectionConfig.legacy_replies = LegacyReplyPolicy::Omit`：此时省略未知回复，并在 turns 中标记 missing_complete_reply。该策略不会将 delta 猜测为完整回复，也不回填日志。

## 来源与资源边界

ConversationProjection 包含算法版本、实际配置、输入事件数、尾事件 ID、每组来源事件、Turn 状态和按物理顺序排列的省略记录。messages() 仅提供消息迭代器，原始事件仍由宿主管理。

默认上限为 100,000 个事件、16 MiB 规范序列化输入和 50,000 条输出消息。零上限无效，超限返回错误；不会静默裁掉消息或半个工具组。输入字节统计覆盖审计元数据，但不是模型 token 计数，也不是整个进程的内存上限。

投影之后可使用[上下文预算与消息选择](context-budget.md)，计入 system、schema、模板和输出预留，按完整回合选择历史。受控结果精简与工具视图见[动作接入](tool-views.md)。完整消息的写入边界见[完整 assistant 回复](assistant-messages.md)。
