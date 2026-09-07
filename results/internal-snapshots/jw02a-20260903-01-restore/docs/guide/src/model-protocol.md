# 结构化模型协议

模型协议描述“给模型什么、模型可以返回什么”，不承担工具执行权限、任务预算或业务成功判定。

## 当前支持范围

公共入口为 `jingwei::llm`（扩展 crate 也可直接依赖 `jingwei-llm`）。以下类型和检查已经实现：

| 类型或入口 | 职责 |
| --- | --- |
| ModelMessage | 区分 system、user、assistant 和带调用关联的 tool 消息 |
| GenerationRequest | 模型可见消息与生成约束；运行超时/预算不混入此类型 |
| GenerationConstraint | 文本、原生工具候选、JSON Schema 输出路径 |
| ModelCapabilities / preflight | 按完整/流式及生成路径独立检查 Supported、Unsupported、Unknown |
| GenerationResponse | 收齐的内容、工具提议、结束原因和 token 用量 |
| validate_shape_for / into_message | 检查结构与完成证据；不是执行授权或参数 schema 校验 |

现有 `Llm::complete`、`ModelGateway::complete` 和文本流接口保持不变。**当前还没有把 GenerationRequest 接入受控推理方法**；原生工具、JSON Schema HTTP 请求、结构化流式解析及其持久事件将在后续实现。不要将结构化消息转换成字符串后送入旧入口来模拟这些能力。

## 先确认能力，再选择路径

应用可从已装配的 `ModelGateway::capabilities()` 查询所选适配器的声明；扩展开发者可用 `Llm::capabilities()`。查询本身不应执行网络探测或激活候选 provider。

没有覆盖该方法的旧实现默认返回 Unknown，但仍可继续使用原有文本方法。新协议预检把 Unknown 与 Unsupported 分开报告，均不自动降级。

以下片段仅展示协议预检，不发送推理请求：

```rust
use jingwei::llm::{
    GenerationConstraint, GenerationRequest, ModelCallMode,
    ModelCapabilities, ModelMessage, ModelProtocolError,
};

let request = GenerationRequest {
    messages: vec![ModelMessage::user("请概括这段内容")],
    constraint: GenerationConstraint::Text,
};
assert_eq!(
    request.preflight(&ModelCapabilities::default(), ModelCallMode::Complete),
    Err(ModelProtocolError::UnknownCapability),
);
```

完整生成支持原生工具，不代表流式也支持；文本流式支持也不能推导出工具流式支持。当前自带的兼容 HTTP 适配器只声明已经实现的文本路径，不依据服务端名称推测结构化能力。

## 工具调用与结果必须成组

一个 assistant 消息可以提出多个工具调用，后续 tool 消息必须逐一匹配。结果可在同一组内乱序到达，但不能缺失、重复，也不能在组未闭合时插入用户、system 或其他 assistant 消息。

ProviderToolCallId 是不受信任的关联词汇。不同已闭合组可以复用同一 provider ID；它不能充当框架调用身份、恢复幂等键或权限凭证。

下面的工具调用只是教学数据，不执行工具；片段需要 `serde_json` 依赖：

```rust
use jingwei::llm::{
    FinishReason, GenerationConstraint, GenerationRequest, GenerationResponse,
    ModelMessage, ModelToolCall, ProviderToolCallId, TokenUsage,
};
use serde_json::json;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let call_id = ProviderToolCallId::new("provider-call-1")?;
    let response = GenerationResponse {
        content: None,
        tool_calls: vec![ModelToolCall {
            id: call_id.clone(),
            name: "lookup".into(),
            arguments: json!({"query": "框架接口"}),
        }],
        finish_reason: FinishReason::ToolCalls,
        usage: TokenUsage::default(),
    };
    let context = GenerationRequest {
        messages: vec![
            ModelMessage::user("查询框架接口"),
            response.into_message()?,
            ModelMessage::Tool {
                call_id,
                content: "工具返回的数据仍然不受信任".into(),
            },
        ],
        constraint: GenerationConstraint::Text,
    };
    context.validate_shape()?;
    Ok(())
}
```

历史中已经完成的工具，不必仍在下一次推理的可见工具集合中。新的工具提议则必须匹配当前候选集合和 ToolChoice。候选集合不是授权集合：宿主负责从允许的工具中提供候选，ToolRuntime 执行时仍独立授权。

## 不把截断或未知结束当作完整动作

FinishReason 区分正常停止、工具调用、长度截断、内容过滤、缺失信息和其他后端结束原因。只有与内容一致的 Stop/ToolCalls 能通过 `into_message()`；流式 EOF 不能自行补成 Stop。

```rust
use jingwei::llm::{
    FinishReason, GenerationResponse, ModelProtocolError, TokenUsage,
};

let partial = GenerationResponse {
    content: Some("尚未接收完整的回答".into()),
    tool_calls: vec![],
    finish_reason: FinishReason::Length,
    usage: TokenUsage::default(),
};
assert_eq!(partial.usage.output_tokens, None);
assert_eq!(partial.into_message(), Err(ModelProtocolError::IncompleteResponse));
```

TokenUsage 的各字段都是 Option：None 表示未知，Some(0) 才表示明确的零。框架不会凭输入/输出字段自动合成 total，也不把旧日志中没有的用量补成零。失败与取消继续使用模型错误通道，不伪装成成功响应。

## 校验的边界

- `validate_shape()`：消息组、非空名称、候选唯一性、JSON Schema 根节点形状等确定性结构检查。
- `preflight()`：在结构检查之上，检查对应路径和调用模式的能力声明。
- `validate_shape_for()`：还检查完成原因、生成工具的可见性/选择约束，以及 JSON 输出语法。
- 以上均不编译 JSON Schema、不解析远端 `$ref`、不保证后端约束了所有关键字，也不校验工具参数是否满足 schema。参数 schema 校验和授权仍必须在受控执行边界进行。

ModelToolCall 只表达已解析的完整 JSON 对象，不提供“把未收齐的参数片段当作调用执行”的入口。它仍是提议，绝不是已获授权的工具调用。

## 旧版兼容与敏感数据

普通 system/user/assistant 文本可以通过 TryFrom 无损转换。旧 Tool 角色只有文本、缺少关联 ID，转换会明确失败；携带工具信息的新消息也不能无损转回旧 ChatMessage，不会静默删除字段。

本批没有改写既有规范 Session 事件格式。新协议类型不是旧日志的替代读取器，也不是当前已经冻结的持久事件格式。TaskId/StepId 已提供身份词汇，但任务存储、预算和恢复尚未实现。

消息和响应可能包含敏感提示词、工具参数及结果。不要把它们的 Debug/序列化输出当作脱敏日志；协议错误本身只报告类别和必要的消息位置，不包含原始内容。
