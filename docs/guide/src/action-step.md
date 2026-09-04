# 单步动作执行

`jingwei-action` 是可选组件：它把一次模型决策解释成一个动作，并把工具调用交给现有受控 ToolRuntime。自定义 Agent 不需要采用它，也可以实现自己的 ActionProtocol。它不是完整参考 Agent、无限循环器、业务成功验证器或任务存储。

在 facade 上启用 actions feature；若只开发底层扩展，也可直接依赖 jingwei-action。当前仓库尚未发布到 crates.io，应用应使用经过核对的源码提交或本地路径，不能照抄一个虚构的发布版本。

## 三种默认动作

两种内置协议表达相同语义：

| 动作 | NativeToolProtocol | JsonActionProtocol |
| --- | --- | --- |
| CallTool | 一个原生 function call | `{"action":"call_tool","name":"…","arguments":{…}}` |
| Final | 没有工具调用的完整 assistant 文本 | `{"action":"final","text":"…"}` |
| AskUser | 虚拟控制调用 `jingwei_ask_user` | `{"action":"ask_user","question":"…"}` |

NativeToolProtocol 会把 `jingwei_ask_user` 加入本次模型候选，但不会把它送进 ToolRuntime；真实工具不能占用这个保留名称。JsonActionProtocol 生成 tagged JSON Schema，可用于不支持原生函数调用、但明确支持 JSON Schema 输出的后端。

首版每个 ActionStep 只接受一个动作。原生模型同时产生多个工具调用时，整步明确失败，执行次数为零；不会只取第一个，也不会自行推测顺序。后续参考 Agent 默认仍将“一步一个动作”，若新增批处理策略，会另行定义预校验、顺序和部分失败语义。

## 运行一步

下面的函数假设 AgentContext 已经由 canonical AgentRuntime 绑定了受控 model/tool scopes。它只推进一次，不包含 while 循环：

```rust
use std::sync::Arc;
use jingwei::action::{
    ActionStep, ActionStepOptions, ActionStepReport, NativeToolProtocol,
};
use jingwei::agent::AgentContext;
use jingwei::llm::ModelMessage;

async fn decide_once(
    messages: Vec<ModelMessage>,
    ctx: &dyn AgentContext,
) -> Result<ActionStepReport, Box<dyn std::error::Error>> {
    let model = ctx.model().ok_or("Agent 没有模型能力")?;
    let tools = ctx.tools().ok_or("Agent 没有工具能力")?;
    let task_id = ctx.budget().ok_or("Agent 没有预算作用域")?
        .report()?.identity.task_id;
    let step = ActionStep::new(Arc::new(NativeToolProtocol));
    Ok(step
        .run(
            task_id,
            messages,
            model,
            tools,
            ctx.cancellation(),
            ActionStepOptions::default(),
        )
        .await?)
}
```

每次 run 都生成新的 StepId；调用方提供可跨多个步骤复用的 TaskId。canonical Agent 内应从预算作用域取得 TaskId，显式绑定会拒绝其他 Task 的关联。ActionStep 会覆盖 ActionStepOptions 中的关联字段，避免调用方把另一步的来源误带过来。它不会替调用方保存 Task、关闭 model/tool scopes 或提交 Agent 的最终 Done/Error 事件，这些仍由宿主和 AgentRuntime 所有。

ActionStepReport 包含原请求、原响应、类型化动作、结果和 next_messages。next_messages 已闭合原生 assistant/tool 调用组；协议临时加入的 system 指令不会反复累积。受控模型/工具入口自动消费共享[任务预算](task-budget.md)。应用可以把 next_messages 交给下一次 ActionStep，但完整参考 Agent 还需实现修正策略、无进展检测和完成检查；模型声称 Final 不代表业务成功。

## 终态与继续条件

StepOutcome 的语义如下：

- Final：模型声称已经回答；不是业务条件已验证。
- AskUser：返回应展示的问题；不会保持一个长期等待的 Future。
- ToolCompleted：工具结果已经完成并有 ToolExecution 规范证据，可以由上层决定是否进入下一步。
- Halted：工具产生了受控失败结果。ActionStep 不自动重试、不换工具、不继续推理。

自定义 Agent 可以把 Final 映射成 TurnOutcome::Completed，把 AskUser 映射成 WaitingForInput，并把 TaskId、问题和应用状态放入自己的持久状态。当前 ActionStep 只把问题返回给宿主；F3/F6 的官方参考 Agent、待答 Task 存储和恢复尚未实现。

对于 Halted，调用方应依据 ToolRecordedOutcome 的 category 决定终止或交给后续明确策略。权限拒绝、用户拒绝、非幂等执行不确定性不能当成普通格式错误自动修正。retryable 只是诊断/策略输入，不是自动重试许可。

## 执行前检查

ActionStep 的检查顺序是：

1. 从 ToolGateway::schemas() 取得该 Agent caller 已获授权的冻结目录。
2. 可选的 with_visible_tools 只能缩小目录；包含未知名称会在推理前失败。
3. 编译候选参数 schema，构建协议请求，并检查模型路径能力。
4. 等受控 ModelGateway 提交完整响应，再校验结束原因、协议结构、候选名称和参数 schema。
5. 仅 CallTool 进入 ToolGateway；ToolRuntime 再次做大小、schema、grant、deny-only guard、审批、超时、取消、输出限制和规范记录。

ActionProtocol 是受信任宿主策略，但自定义实现也不能借由 parse 返回隐藏工具或错误参数来绕过第 4、5 层检查。直接调用 protocol.parse() 只是在解析数据，不是执行授权；只有 ActionStep 加受控 ToolGateway 构成执行路径。

schema 编译禁止从网络或文件读取外部引用。JsonActionProtocol 嵌入工具参数 schema，并为没有根 $id 的 schema 设置独立资源 ID，使本地 #/$defs 引用仍以工具 schema 为根。具体关键字、方言和后端生成约束范围仍需适配器/宿主核实；执行前以框架校验和 ToolRuntime 复检为准。

## 可追踪身份

DecisionContext 由 TaskId 和 StepId 组成。它被写入规范 ModelRequest.options.context；实际 ToolCall.action 写入同一 DecisionContext、action_index 和可选 provider_call_id。关联关系为：

```text
TaskId + StepId
├── ModelRequest ── ModelCallId ── ModelResult
└── Action[0] ── ToolCall runtime ID ── ToolResult
                 └── provider_call_id（仅来源信息）
```

ProviderToolCallId 不会变成 ToolCall 的 runtime ID、权限凭证或恢复幂等键。无模型工具执行可继续不带 action 字段；缺少关联是明确的 None，不会捏造 Task/Step。

ActionStepError 在所有错误上保留 DecisionContext。若工具已经提交了 Call/Result 后，反馈投影失败或取消，completed_execution 会保留已确认执行证据；ToolRuntime 的 Recording 错误也保留调用/结果尝试证据。调用方不得因为没有 ActionStepReport 就自动重放。

## 原生和 JSON 反馈

原生协议把工具结果投影为匹配 provider_call_id 的 ModelMessage::Tool。JSON-only 协议把结果投影成带 `type: jingwei_tool_result` 的 JSON 数据消息，以免要求后端支持原生 tool role；规范来源仍在 ToolExecution / Session 事件中。

JSON-only 数据在传输上使用 user role 只是兼容后端，并不代表新的用户意图。系统指令会标注它是不可信数据，但 prompt 指令不能提供安全隔离。应用应控制工具输出和上下文投影，不要把模型服从“忽略工具数据里的指令”当作安全边界。

## 已验证与未完成

内部验证覆盖两种协议的工具→结果→最终回答、AskUser、custom Agent 组合、局部 $ref、不可见工具、非法参数、多动作、截断、取消、审批/guard、工具失败、超时以及四个规范记录屏障。测试只使用假模型、内存记录和现有 canonical runtimes；未连接真实模型。

当前仍未提供动作流式展示、自动修正策略、官方有限循环、上下文裁剪、完成检查、持久待答状态和恢复。ActionStep 复用 canonical runtimes 的共享任务预算；不要把单步组合和预算停止当作完整参考 Agent 或恢复实现。
