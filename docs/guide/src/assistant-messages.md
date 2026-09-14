# 持久化完整 assistant 回复

canonical AgentRuntime 会把成功完成或等待用户输入的 `AgentTurnOutput.final_text` 写为一个 `AssistantMessage` 事件。Agent 不必先 emit delta；完整文本会随 Session 历史进入下一回合，在 JSONL provider 下也可跨进程读取。

## Agent 返回正文即可

```rust,no_run
use jingwei::agent::{
    Agent, AgentContext, AgentFuture, AgentTurnInput, AgentTurnOutput, TurnOutcome,
};

struct Answer;
impl Agent for Answer {
    fn run_turn<'a>(
        &'a self,
        _input: AgentTurnInput<'a>,
        _ctx: &'a dyn AgentContext,
    ) -> AgentFuture<'a, Result<AgentTurnOutput, jingwei::agent::AgentError>> {
        Box::pin(async {
            Ok(AgentTurnOutput {
                final_text: "这是需要保留的完整回复。".into(),
                outcome: TurnOutcome::Completed,
                artifact: None,
            })
        })
    }
}
```

把 Agent 按既有插件方式交给 canonical runtime 后，runtime 负责持久化。`AgentEventKind` 不提供完整消息事件入口，避免 Agent 与 runtime 各写一份。自定义 AgentRuntime 则必须自己遵循相同写入和失败屏障。

## delta 和完整消息的关系

`AssistantDelta` 是流式过程记录；`AssistantMessage.text` 是 Agent 明确返回的最终正文，两者可以不同。完整消息有独立的 `message_id`，信封的 Session/Turn 地址由 SessionRuntime 提供。每次正常关闭的回合只写一个完整消息，空字符串也保留，不自动拼入 delta 或替换为累计增量。

生成对话视图时，同回合完整消息应替代 delta 的拼接结果，不能把两者相加。日志保留两类原始证据，不删除或改写增量。标准对话投影、工具调用/结果分组和上下文 token 裁剪仍在后续开发。

## 写入顺序和成功条件

顺序为：Agent 正文结束 → 模型/工具排空 → 进入清理 → 完整消息 → TaskRunReport → 唯一 Done/Error → Session settle → 可选最终预算检查点。

只有完成或等待输入的意图进入完整消息提交。此前发生的取消、Agent 失败或预算停止不生成完整回复；已有 delta 仍是诊断证据。进入收尾提交后，取消等待或 shutdown 不会丢弃已接收的持久化工作，也不会撤回已写入的消息。

完整消息本身不是整个回合成功的证明。报告、终态、结算或最终预算提交仍可能失败；消费者还应核对同回合的终态和关闭证据。遇到 Error 或未关闭回合，不能仅凭完整消息宣称业务成功。

- 消息写入失败：回合返回错误，保留 `DriveFailure::AssistantMessage` 中的原始 draft 和持久化错误，不把 Agent 的返回值当作已保存。
- Session 返回不匹配的事件 ID、message_id、Session/Turn、正文或 generation_id：保留 `InvalidReceipt`；持久预算 claim 继续冻结。
- settle 遗漏确认过的消息或出现多份完整消息：返回 `SessionRuntimeError::InvalidMessageSettlement`，保留期望事件和实际结算窗口，不返回成功报告。
- 报告或终态之后失败：已写入的消息保留，不自动重复执行 Agent。恢复继续遵守[预算候选恢复](budget-recovery.md)中的原始证据要求。

## 版本与旧日志

新负载为 `{"type":"assistant_message","version":1,"text":"..."}`，version 必填；未知数字版本拒绝读取。旧 UserMessage/AssistantDelta 负载保持不变，已有日志不回填完整消息。

旧程序可能无法读取新事件，升级消费者时需处理 `SessionEventKind::AssistantMessage`、新增 DriveFailure 和结算错误。旧日志只有 delta 时不能假定其中包含完整 final_text，也不能从预算报告反推正文。本批未改变预算检查点版本，未增加框架依赖或新 crate。
