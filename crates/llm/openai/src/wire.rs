//! Private mapping to the compatible HTTP wire, never a framework public type.
use jingwei_llm::*;
use serde::Deserialize;
use serde_json::{Value, json};

pub(super) fn request(
    model: &str,
    input: &GenerationRequest,
    options: &GenerationOptions,
    stream: bool,
    stream_usage: bool,
) -> Result<Value, LlmError> {
    let messages = input
        .messages
        .iter()
        .map(|message| match message {
            ModelMessage::System { content } => json!({"role":"system", "content":content}),
            ModelMessage::User { content } => json!({"role":"user", "content":content}),
            ModelMessage::Assistant {
                content,
                tool_calls,
            } => {
                let mut message = json!({"role":"assistant", "content":content});
                if !tool_calls.is_empty() {
                    message["tool_calls"] = json!(
                        tool_calls
                            .iter()
                            .map(|call| json!({
                                "id":call.id.as_str(), "type":"function", "function":{
                                    "name":call.name, "arguments":call.arguments.to_string()
                                }
                            }))
                            .collect::<Vec<_>>()
                    );
                }
                message
            }
            ModelMessage::Tool { call_id, content } => {
                json!({"role":"tool", "tool_call_id":call_id.as_str(), "content":content})
            }
        })
        .collect::<Vec<_>>();
    let mut payload = json!({"model":model,"messages":messages,"stream":stream});
    if let Some(max_tokens) = options.max_tokens {
        payload["max_tokens"] = json!(max_tokens);
    }
    if stream && stream_usage {
        payload["stream_options"] = json!({"include_usage":true});
    }
    match &input.constraint {
        GenerationConstraint::Text => {}
        GenerationConstraint::NativeTools { tools, choice } => {
            payload["tools"] = json!(tools.iter().map(|tool| json!({"type":"function", "function":{
                "name":tool.name, "description":tool.description, "parameters":tool.parameters
            }})).collect::<Vec<_>>());
            payload["tool_choice"] = match choice {
                ToolChoice::Auto => json!("auto"),
                ToolChoice::Required => json!("required"),
                ToolChoice::Named { name } => json!({"type":"function","function":{"name":name}}),
            };
        }
        // Host explicitly opts in to this transport. Enforcement of arbitrary
        // keywords is not promised; the controlled runtime validates the result.
        GenerationConstraint::JsonSchema { name, schema } => {
            payload["response_format"] = json!({"type":"json_schema", "json_schema":{
                "name":name,"schema":schema,"strict":false
            }});
        }
    }
    Ok(payload)
}

#[derive(Deserialize, Default)]
pub(super) struct Usage {
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
    total_tokens: Option<u64>,
}
impl From<Usage> for TokenUsage {
    fn from(usage: Usage) -> Self {
        Self {
            input_tokens: usage.prompt_tokens,
            output_tokens: usage.completion_tokens,
            total_tokens: usage.total_tokens,
        }
    }
}

pub(super) fn reason(reason: Option<String>) -> FinishReason {
    match reason.as_deref() {
        Some("stop") => FinishReason::Stop,
        Some("tool_calls") => FinishReason::ToolCalls,
        Some("length") => FinishReason::Length,
        Some("content_filter") => FinishReason::ContentFiltered,
        None => FinishReason::Unknown,
        Some(_) => FinishReason::Other {
            code: reason.unwrap(),
        },
    }
}

#[derive(Deserialize)]
struct Response {
    choices: Vec<Choice>,
    usage: Option<Usage>,
}
#[derive(Deserialize)]
struct Choice {
    #[serde(default)]
    index: u32,
    message: Message,
    finish_reason: Option<String>,
}
#[derive(Deserialize)]
struct Message {
    role: Option<String>,
    content: Option<String>,
    tool_calls: Option<Vec<Call>>,
    function_call: Option<Value>,
}
#[derive(Deserialize)]
struct Call {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    function: Function,
}
#[derive(Deserialize)]
struct Function {
    name: String,
    arguments: String,
}

pub(super) fn decode(
    bytes: &[u8],
    limits: GenerationLimits,
) -> Result<GenerationResponse, LlmError> {
    let payload: Response =
        serde_json::from_slice(bytes).map_err(|_| invalid("invalid completion JSON"))?;
    if payload.choices.len() != 1 {
        return Err(invalid("exactly one completion choice is required"));
    }
    let choice = payload.choices.into_iter().next().unwrap();
    if choice.index != 0
        || choice
            .message
            .role
            .as_deref()
            .is_some_and(|role| role != "assistant")
        || choice.message.function_call.is_some()
    {
        return Err(invalid("unsupported completion choice or message"));
    }
    let mut accumulator = GenerationAccumulator::new(limits);
    if let Some(text) = choice.message.content {
        accumulator.push(&GenerationDelta::Text { text })?;
    }
    for (index, call) in choice
        .message
        .tool_calls
        .unwrap_or_default()
        .into_iter()
        .enumerate()
    {
        if call.kind != "function" {
            return Err(invalid("unsupported tool-call kind"));
        }
        accumulator.push(&GenerationDelta::ToolCall {
            index: u32::try_from(index).map_err(|_| ModelProtocolError::OutputLimitExceeded)?,
            id: Some(call.id),
            name: Some(call.function.name),
            arguments: call.function.arguments,
        })?;
    }
    accumulator
        .response(
            reason(choice.finish_reason),
            payload.usage.unwrap_or_default().into(),
        )
        .map_err(Into::into)
}

pub(super) fn invalid(message: &str) -> LlmError {
    LlmError::StreamParse(message.into())
}

#[derive(Deserialize)]
pub(super) struct Chunk {
    pub choices: Vec<ChunkChoice>,
    pub usage: Option<Usage>,
}
#[derive(Deserialize)]
pub(super) struct ChunkChoice {
    pub index: u32,
    pub delta: Delta,
    pub finish_reason: Option<String>,
}
#[derive(Deserialize)]
pub(super) struct Delta {
    pub role: Option<String>,
    pub content: Option<String>,
    pub tool_calls: Option<Vec<DeltaCall>>,
    pub function_call: Option<Value>,
}
#[derive(Deserialize)]
pub(super) struct DeltaCall {
    pub index: u32,
    pub id: Option<String>,
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub function: Option<DeltaFunction>,
}
#[derive(Deserialize, Default)]
pub(super) struct DeltaFunction {
    pub name: Option<String>,
    pub arguments: Option<String>,
}
