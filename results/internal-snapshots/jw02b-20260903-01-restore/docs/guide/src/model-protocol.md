# 模型生成

Jingwei 用一套协议处理文本回答、原生工具调用和 JSON Schema 输出。业务 Agent 通过受控的 ModelGateway 调用；适配器实现 Llm。消息和结果词汇定义在 jingwei-core，统一从 jingwei::llm 导出，不绑定某个模型厂商。

## 生成文本

纯文本是 GenerationConstraint::Text 模式，不需要单独的文本接口。下面的函数可用于已经装配好的 ModelGateway；它本身不创建模型或连接服务：

```rust
use jingwei::llm::{
    GenerationOptions, GenerationRequest, ModelGateway,
    ModelGatewayError, ModelMessage,
};

async fn answer(model: &dyn ModelGateway) -> Result<ModelMessage, ModelGatewayError> {
    let request = GenerationRequest::text(vec![
        ModelMessage::system("请简短回答"),
        ModelMessage::user("什么是 Agent 运行时？"),
    ]);
    let response = model.generate(&request, GenerationOptions::default()).await?;
    // 推理结束不一定是完整回答；截断或未知结束原因在这里被拒绝。
    response.into_message().map_err(|e| jingwei::llm::LlmError::from(e).into())
}
```

GenerationOptions 提供 max_tokens、timeout 和收集上限 limits。受控运行时保留 admission、宿主取消、超时和收尾语义；单次 timeout 不能放宽运行时默认上限。max_tokens 传给后端，不等于宿主任务预算，也不能替代上下文 token 计数。

GenerationResponse 包含可选文本、完整工具提议、未完成的工具参数、finish_reason 和 usage。TokenUsage 的各字段独立可缺失：None 是未知，Some(0) 才是明确的零；框架不会自动推算缺失的 total_tokens。

## 消费流式输出

generate_stream 输出两种事件：Delta 用于增量展示或收集，Finished 携带最终响应。示例需要 futures 依赖：

```rust
use futures::StreamExt;
use jingwei::llm::*;

async fn collect(model: &dyn ModelGateway) -> Result<GenerationResponse, ModelGatewayError> {
    let request = GenerationRequest::text(vec![ModelMessage::user("你好")]);
    let mut stream = model.generate_stream(request, GenerationOptions::default());
    while let Some(event) = stream.next().await {
        match event? {
            GenerationStreamEvent::Delta(GenerationDelta::Text { text }) => {
                // 可以展示 text，但不能据此判定任务完成。
                let _ = text;
            }
            GenerationStreamEvent::Delta(GenerationDelta::ToolCall { .. }) => {
                // 工具 ID、名称和参数仍可能只是片段；这里不执行工具。
            }
            GenerationStreamEvent::Finished(response) => return Ok(response),
        }
    }
    Err(LlmError::from(ModelProtocolError::MissingStreamTerminal).into())
}
```

受控运行时逐条收集并检查片段，核对最终响应与已发出的片段一致；规范 ModelResult 记录成功后才交付 Finished。记录失败会返回 Recording 错误，不会先交付可用的最终结果。Delta 本身不是已持久化完成的证据。

Finished 只表示推理有了明确终态，不等于动作完整或任务成功。Length、ContentFiltered、Unknown 和其他未支持的结束原因作为诊断响应保留；into_message() 拒绝把它们变成完整 assistant 消息。截断工具参数保存在 incomplete_tool_calls，而不是 tool_calls。

流式 EOF 缺少终态会报错，不自动当作 Stop。取消、超时、协议失败保留已收集的文本和工具片段；消费者丢弃流时，受控运行时会取消驱动并完成结果记录。队列有界，慢消费者也受超时和取消约束。

## 显式选择结构化能力

GenerationConstraint 支持 Text、NativeTools 和 JsonSchema，互不静默降级。ModelCapabilities 区分完整生成与流式支持；Unknown 和 Unsupported 都会在受控推理前被拒绝。capabilities() 是纯元数据查询，不探测网络或激活未选中的 provider。

自带兼容 HTTP 适配器默认只启用文本路径。宿主核实所选服务端能力后，显式开启对应模式。下面只构造配置，不发请求；扩展项目需要 jingwei-openai 依赖：

```rust
use jingwei::llm::{CapabilitySupport, GenerationSupport};
use jingwei_openai::OpenAiConfig;

let config = OpenAiConfig::new("http://127.0.0.1:8000/v1", "local-model")?
    .with_native_tools(GenerationSupport {
        complete: CapabilitySupport::Supported,
        stream: CapabilitySupport::Supported,
    })
    .with_json_schema(GenerationSupport {
        complete: CapabilitySupport::Supported,
        stream: CapabilitySupport::Unsupported,
    });
// 仅在服务端支持 stream_options.include_usage 时开启 with_stream_usage(true)。
# Ok::<(), jingwei_openai::OpenAiConfigError>(())
```

NativeTools 携带当前可见的 ModelToolDefinition 集合与 ToolChoice（Auto、Required 或 Named）；JsonSchema 携带输出名称和 schema。兼容适配器分别映射到 tools/tool_choice 与 response_format；JSON Schema 请求使用 strict=false，不宣称后端强制实现所有关键字，受控运行时独立验证结果。字段映射依据 [Chat Completions 请求协议](https://developers.openai.com/api/reference/resources/chat/subresources/completions/methods/create)和[结构化输出说明](https://developers.openai.com/api/docs/guides/structured-outputs)。

## 工具调用与结果必须成组

一个 assistant 消息可以提出多个调用，后续 tool 消息逐一关联。同组结果可以乱序返回，但不能缺失、重复，也不能在未闭合时插入用户或其他 assistant 消息。

ProviderToolCallId 只用于关联同一 assistant 组中的调用与结果，不是框架运行 ID、幂等键或权限凭证。不同已闭合组可以复用同一个 provider ID。

下面仅构造教学数据，不执行工具；需要 serde_json 依赖：

```rust
use jingwei::llm::*;
use serde_json::json;

let id = ProviderToolCallId::new("provider-call-1")?;
let response = GenerationResponse {
    content: None,
    tool_calls: vec![ModelToolCall {
        id: id.clone(),
        name: "lookup".into(),
        arguments: json!({"query": "框架接口"}),
    }],
    incomplete_tool_calls: vec![],
    finish_reason: FinishReason::ToolCalls,
    usage: TokenUsage::default(),
};
let next = GenerationRequest::text(vec![
    ModelMessage::user("查询框架接口"),
    response.into_message()?,
    ModelMessage::Tool {
        call_id: id,
        content: "工具结果仍是不受信任的数据".into(),
    },
]);
next.validate_shape()?;
# Ok::<(), ModelProtocolError>(())
```

已完成的历史工具不必仍在下一次请求的可见集合中。新提议则必须匹配当前候选和选择约束。候选集合不等于授权集合：宿主提供候选，ToolRuntime 执行时仍独立检查参数与权限。

## 校验和资源边界

| 边界 | 检查内容 |
| --- | --- |
| validate_shape / preflight | 消息组、ID、候选唯一性、schema 根形状、路径能力 |
| validate_shape_for / into_message | 完成原因、内容形状、候选/选择约束、JSON 语法；不提供执行授权 |
| Canonical ModelGateway | 推理前编译 schema，推理后检查 JSON 输出/工具参数是否满足 schema |
| ToolRuntime | 独立的工具执行校验、权限与审计 |

受控 schema 校验禁止从网络或文件获取外部 $ref；schema 内部引用可用。支持的 JSON Schema 方言与关键字由所用验证器决定；宿主仍需确认后端能接受其约束格式，不能把协议传输支持等同于严格约束生成。直接调用原始 Llm 不享有受控 schema、规范记录和宿主策略保证，业务 Agent 应使用 ModelGateway。

GenerationLimits 默认限制每次收集 8 MiB、最多 32 个工具调用。限制同时作用于适配器响应体/流式传输字节、收集内容和规范响应序列化大小，各阶段分别检查，包含的元数据开销可能不同。这是拒绝过大输出的边界，不是整个进程的峰值内存上限；任务预算、上下文 token 预算和模型 admission 队列仍在后续里程碑实现。

## 开发期破坏性替换

未上线阶段直接移除了旧的 complete/complete_stream、ChatMessage、LlmCompletion 和字符串流，没有兼容别名。调用方改用 generate/generate_stream、GenerationRequest、GenerationResponse 和类型化流事件；纯文本功能由 Text 模式覆盖。

规范 ModelRequest/ModelResult 使用显式 version: 1，记录结构化输入、有效调用参数、完整结果或失败片段。不读取旧的无版本模型负载，也不接受未知版本；旧开发日志不会被自动改写或删除。需要旧数据时，应先备份，再由宿主显式决定迁移或用原版本读取。

消息、参数和规范事件可能含敏感业务数据。不要把 Debug/序列化内容当作脱敏日志。TaskId/StepId 当前只是身份词汇；参考 Agent 的动作循环、任务存储和恢复尚未完成。
