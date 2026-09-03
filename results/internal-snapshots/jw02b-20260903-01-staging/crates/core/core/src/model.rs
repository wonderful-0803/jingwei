//! Backend-neutral structured model vocabulary and deterministic preflight checks.
//!
//! Canonical Session model records reuse these values; the values alone are
//! neither recorded evidence nor tool-execution authority.
//! Shape validation does not compile JSON Schema, authorize tools, or validate
//! their arguments against a schema. Raw streaming fragments are deliberately
//! not represented as complete [`ModelToolCall`] values.

use std::collections::BTreeSet;

use crate::event::ModelCallMode;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// An untrusted provider's correlation ID, scoped to one assistant call group.
///
/// It is not a runtime invocation ID, a task identity, or an idempotency key.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub struct ProviderToolCallId(String);

impl ProviderToolCallId {
    pub fn new(value: impl Into<String>) -> Result<Self, ModelProtocolError> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(ModelProtocolError::EmptyToolCallId);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ProviderToolCallId {
    type Error = ModelProtocolError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

/// A fully received, parsed proposal; still untrusted and not authorized to run.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelToolCall {
    pub id: ProviderToolCallId,
    pub name: String,
    pub arguments: Value,
}

/// Role-specific messages prevent attaching tool-result IDs to system/user text.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "lowercase", deny_unknown_fields)]
pub enum ModelMessage {
    System {
        content: String,
    },
    User {
        content: String,
    },
    Assistant {
        content: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ModelToolCall>,
    },
    Tool {
        call_id: ProviderToolCallId,
        content: String,
    },
}

impl ModelMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self::System {
            content: content.into(),
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self::User {
            content: content.into(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self::Assistant {
            content: Some(content.into()),
            tool_calls: Vec::new(),
        }
    }
}

/// A model-visible candidate, not a grant of tool execution permission.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolChoice {
    #[default]
    Auto,
    Required,
    Named {
        name: String,
    },
}

/// An explicit output path. No variant silently falls back to plain text.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum GenerationConstraint {
    #[default]
    Text,
    NativeTools {
        tools: Vec<ModelToolDefinition>,
        #[serde(default)]
        choice: ToolChoice,
    },
    JsonSchema {
        name: String,
        schema: Value,
    },
}

/// Whether this adapter/configuration explicitly supports a protocol path.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilitySupport {
    Supported,
    Unsupported,
    #[default]
    Unknown,
}

/// Completion and streaming support are independent, for each output path.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GenerationSupport {
    pub complete: CapabilitySupport,
    pub stream: CapabilitySupport,
}

impl GenerationSupport {
    fn for_mode(self, mode: ModelCallMode) -> CapabilitySupport {
        match mode {
            ModelCallMode::Complete => self.complete,
            ModelCallMode::Stream => self.stream,
        }
    }
}

/// Pure metadata, supplied by the adapter and trusted host configuration.
///
/// JSON Schema support means the transport path exists, not that all keywords
/// are enforced. The host must confirm the backend's accepted dialect/subset;
/// the controlled runtime independently validates results. Missing per-call usage remains
/// unknown even when usage reporting is advertised as supported.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelCapabilities {
    pub text: GenerationSupport,
    pub native_tools: GenerationSupport,
    pub json_schema: GenerationSupport,
    pub token_usage: CapabilitySupport,
    pub token_counting: CapabilitySupport,
}

impl ModelCapabilities {
    /// Text-only capabilities, without token usage or counting guarantees.
    pub const fn text_only() -> Self {
        let unsupported = GenerationSupport {
            complete: CapabilitySupport::Unsupported,
            stream: CapabilitySupport::Unsupported,
        };
        Self {
            text: GenerationSupport {
                complete: CapabilitySupport::Supported,
                stream: CapabilitySupport::Supported,
            },
            native_tools: unsupported,
            json_schema: unsupported,
            token_usage: CapabilitySupport::Unsupported,
            token_counting: CapabilitySupport::Unsupported,
        }
    }
}

/// Model-visible input. Runtime timeouts and budgets remain separate concerns.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GenerationRequest {
    pub messages: Vec<ModelMessage>,
    #[serde(default)]
    pub constraint: GenerationConstraint,
}

impl GenerationRequest {
    /// Plain text is a mode of the same protocol, not a second model interface.
    pub fn text(messages: Vec<ModelMessage>) -> Self {
        Self {
            messages,
            constraint: GenerationConstraint::Text,
        }
    }

    /// Validate message groups and candidate definitions without any IO.
    ///
    /// Root JSON Schema shape is checked, but schema syntax/keywords and tool
    /// argument schemas are not compiled here. This is not an execution gate.
    pub fn validate_shape(&self) -> Result<(), ModelProtocolError> {
        validate_messages(&self.messages)?;
        match &self.constraint {
            GenerationConstraint::Text => Ok(()),
            GenerationConstraint::JsonSchema { name, schema } => {
                validate_name(name)?;
                validate_schema_shape(schema)
            }
            GenerationConstraint::NativeTools { tools, choice } => {
                if tools.is_empty() {
                    return Err(ModelProtocolError::EmptyToolSet);
                }
                let mut names = BTreeSet::new();
                for tool in tools {
                    validate_name(&tool.name)?;
                    validate_schema_shape(&tool.parameters)?;
                    if !names.insert(tool.name.as_str()) {
                        return Err(ModelProtocolError::DuplicateToolName);
                    }
                }
                if let ToolChoice::Named { name } = choice
                    && !names.contains(name.as_str())
                {
                    return Err(ModelProtocolError::ToolNotVisible);
                }
                Ok(())
            }
        }
    }

    /// Reject unsupported or unknown paths before submitting an inference.
    ///
    /// This does not submit the request or prove backend schema enforcement.
    pub fn preflight(
        &self,
        capabilities: &ModelCapabilities,
        mode: ModelCallMode,
    ) -> Result<(), ModelProtocolError> {
        self.validate_shape()?;
        let support = match &self.constraint {
            GenerationConstraint::Text => capabilities.text,
            GenerationConstraint::NativeTools { .. } => capabilities.native_tools,
            GenerationConstraint::JsonSchema { .. } => capabilities.json_schema,
        };
        match support.for_mode(mode) {
            CapabilitySupport::Supported => Ok(()),
            CapabilitySupport::Unsupported => Err(ModelProtocolError::UnsupportedCapability),
            CapabilitySupport::Unknown => Err(ModelProtocolError::UnknownCapability),
        }
    }
}

/// Provider-observed terminal information, not inferred from stream EOF.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum FinishReason {
    Stop,
    ToolCalls,
    Length,
    ContentFiltered,
    #[default]
    Unknown,
    Other {
        code: String,
    },
}

/// Missing values are unknown; input/output/total are never invented or summed.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TokenUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
}

/// A collected response; receiving this value alone does not authorize actions.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GenerationResponse {
    pub content: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<ModelToolCall>,
    /// Unexecutable fragments retained for truncated/unknown terminal responses.
    #[serde(default)]
    pub incomplete_tool_calls: Vec<ToolCallFragment>,
    #[serde(default)]
    pub finish_reason: FinishReason,
    #[serde(default)]
    pub usage: TokenUsage,
}

impl GenerationResponse {
    pub fn text(content: impl Into<String>, finish_reason: FinishReason) -> Self {
        Self {
            content: Some(content.into()),
            tool_calls: Vec::new(),
            incomplete_tool_calls: Vec::new(),
            finish_reason,
            usage: TokenUsage::default(),
        }
    }

    fn validate_complete(&self) -> Result<(), ModelProtocolError> {
        if !self.incomplete_tool_calls.is_empty() {
            return Err(ModelProtocolError::IncompleteResponse);
        }
        match self.finish_reason {
            FinishReason::Stop if self.tool_calls.is_empty() => {}
            FinishReason::ToolCalls if !self.tool_calls.is_empty() => {}
            FinishReason::Stop | FinishReason::ToolCalls => {
                return Err(ModelProtocolError::InconsistentFinishReason);
            }
            _ => return Err(ModelProtocolError::IncompleteResponse),
        }
        validate_assistant(self.content.as_deref(), &self.tool_calls)
    }

    /// Check completion, candidate names/choice and JSON syntax, not schemas.
    ///
    /// ToolRuntime must still validate argument schemas and authorize every call.
    /// No returned proposal should bypass that runtime based on this check.
    pub fn validate_shape_for(
        &self,
        request: &GenerationRequest,
    ) -> Result<(), ModelProtocolError> {
        request.validate_shape()?;
        self.validate_complete()?;
        match &request.constraint {
            GenerationConstraint::Text | GenerationConstraint::JsonSchema { .. }
                if !self.tool_calls.is_empty() =>
            {
                Err(ModelProtocolError::UnexpectedToolCalls)
            }
            GenerationConstraint::Text => Ok(()),
            GenerationConstraint::JsonSchema { .. } => {
                serde_json::from_str::<Value>(self.content.as_deref().unwrap_or_default())
                    .map(|_| ())
                    .map_err(|_| ModelProtocolError::InvalidJsonOutput)
            }
            GenerationConstraint::NativeTools { tools, choice } => {
                if !matches!(choice, ToolChoice::Auto) && self.tool_calls.is_empty() {
                    return Err(ModelProtocolError::RequiredToolCallMissing);
                }
                for call in &self.tool_calls {
                    if !tools.iter().any(|tool| tool.name == call.name) {
                        return Err(ModelProtocolError::ToolNotVisible);
                    }
                    if let ToolChoice::Named { name } = choice
                        && call.name != *name
                    {
                        return Err(ModelProtocolError::WrongToolChoice);
                    }
                }
                Ok(())
            }
        }
    }

    /// Project only an explicitly complete response into model context.
    ///
    /// This checks neither the originating request nor execution permissions.
    pub fn into_message(self) -> Result<ModelMessage, ModelProtocolError> {
        self.validate_complete()?;
        Ok(ModelMessage::Assistant {
            content: self.content,
            tool_calls: self.tool_calls,
        })
    }
}

fn validate_name(name: &str) -> Result<(), ModelProtocolError> {
    if name.trim().is_empty() {
        Err(ModelProtocolError::EmptyName)
    } else {
        Ok(())
    }
}

fn validate_schema_shape(schema: &Value) -> Result<(), ModelProtocolError> {
    if schema.is_object() || schema.is_boolean() {
        Ok(())
    } else {
        Err(ModelProtocolError::InvalidSchemaShape)
    }
}

fn validate_assistant(
    content: Option<&str>,
    calls: &[ModelToolCall],
) -> Result<(), ModelProtocolError> {
    if content.is_none() && calls.is_empty() {
        return Err(ModelProtocolError::MissingAssistantContent);
    }
    let mut ids = BTreeSet::new();
    for call in calls {
        validate_name(&call.name)?;
        if !call.arguments.is_object() {
            return Err(ModelProtocolError::ArgumentsNotObject);
        }
        if !ids.insert(&call.id) {
            return Err(ModelProtocolError::DuplicateToolCallId);
        }
    }
    Ok(())
}

fn validate_messages(messages: &[ModelMessage]) -> Result<(), ModelProtocolError> {
    if messages.is_empty() {
        return Err(ModelProtocolError::EmptyMessages);
    }
    let mut pending = BTreeSet::new();
    for (index, message) in messages.iter().enumerate() {
        match message {
            ModelMessage::Tool { call_id, .. } => {
                if !pending.remove(call_id) {
                    return Err(ModelProtocolError::UnmatchedToolResult { index });
                }
            }
            _ if !pending.is_empty() => {
                return Err(ModelProtocolError::InterruptedToolGroup { index });
            }
            ModelMessage::Assistant {
                content,
                tool_calls,
            } => {
                validate_assistant(content.as_deref(), tool_calls)?;
                pending.extend(tool_calls.iter().map(|call| call.id.clone()));
            }
            _ => {}
        }
    }
    if pending.is_empty() {
        Ok(())
    } else {
        Err(ModelProtocolError::MissingToolResults)
    }
}

/// Transport-free failures deliberately omit prompts, IDs and argument values.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum ModelProtocolError {
    #[error("provider tool-call ID must not be blank")]
    EmptyToolCallId,
    #[error("model request has no messages")]
    EmptyMessages,
    #[error("assistant message has neither content nor tool calls")]
    MissingAssistantContent,
    #[error("tool or schema name must not be blank")]
    EmptyName,
    #[error("tool-call arguments must be a parsed JSON object")]
    ArgumentsNotObject,
    #[error("duplicate tool-call ID within one assistant group")]
    DuplicateToolCallId,
    #[error("tool result at message {index} has no pending matching call")]
    UnmatchedToolResult { index: usize },
    #[error("message {index} interrupts a pending tool-call group")]
    InterruptedToolGroup { index: usize },
    #[error("model context is missing one or more tool results")]
    MissingToolResults,
    #[error("JSON Schema root must be an object or boolean")]
    InvalidSchemaShape,
    #[error("native-tool constraint requires at least one candidate")]
    EmptyToolSet,
    #[error("duplicate candidate tool name")]
    DuplicateToolName,
    #[error("requested or generated tool is not in the visible candidate set")]
    ToolNotVisible,
    #[error("requested generation path is unsupported")]
    UnsupportedCapability,
    #[error("requested generation path has unknown support")]
    UnknownCapability,
    #[error("response has no supported successful terminal reason")]
    IncompleteResponse,
    #[error("finish reason disagrees with the presence of tool calls")]
    InconsistentFinishReason,
    #[error("this generation path does not permit native tool calls")]
    UnexpectedToolCalls,
    #[error("structured output is not valid JSON")]
    InvalidJsonOutput,
    #[error("a required tool call is missing")]
    RequiredToolCallMissing,
    #[error("generated tool does not match the explicitly selected candidate")]
    WrongToolChoice,
    #[error("model output exceeded its configured size or tool-count limit")]
    OutputLimitExceeded,
    #[error("stream ended without a terminal response")]
    MissingStreamTerminal,
    #[error("terminal response does not match previously emitted deltas")]
    StreamTerminalMismatch,
    #[error("invalid JSON Schema or schema validation failed")]
    SchemaValidation,
}

/// Explicit version for new canonical model records. Old/unknown wires fail closed.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "u16", into = "u16")]
pub enum ModelRecordVersion {
    V1,
}

impl TryFrom<u16> for ModelRecordVersion {
    type Error = &'static str;
    fn try_from(value: u16) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::V1),
            _ => Err("unsupported model record version"),
        }
    }
}
impl From<ModelRecordVersion> for u16 {
    fn from(_: ModelRecordVersion) -> Self {
        1
    }
}

/// Trusted per-call collection limits, independent from task-level budgets.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GenerationLimits {
    pub max_output_bytes: usize,
    pub max_tool_calls: usize,
}
impl Default for GenerationLimits {
    fn default() -> Self {
        Self {
            max_output_bytes: 8 * 1024 * 1024,
            max_tool_calls: 32,
        }
    }
}

/// A fragment can never be confused with a parsed, complete tool proposal.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ToolCallFragment {
    pub index: u32,
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GenerationDelta {
    Text {
        text: String,
    },
    ToolCall {
        index: u32,
        id: Option<String>,
        name: Option<String>,
        arguments: String,
    },
}

/// Exactly one Finished event follows deltas; EOF without it is an error.
/// Finished means inference ended, not that an action is safe or complete.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GenerationStreamEvent {
    Delta(GenerationDelta),
    Finished(GenerationResponse),
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct GenerationPartial {
    pub content: Option<String>,
    pub tool_calls: Vec<ToolCallFragment>,
}

/// Deterministic bounded accumulator shared by adapters and the controlled runtime.
pub struct GenerationAccumulator {
    partial: GenerationPartial,
    limits: GenerationLimits,
    bytes: usize,
}

impl GenerationAccumulator {
    pub fn new(limits: GenerationLimits) -> Self {
        Self {
            partial: GenerationPartial::default(),
            limits,
            bytes: 0,
        }
    }

    pub fn partial(&self) -> &GenerationPartial {
        &self.partial
    }

    pub fn push(&mut self, delta: &GenerationDelta) -> Result<(), ModelProtocolError> {
        let added = match delta {
            GenerationDelta::Text { text } => text.len(),
            GenerationDelta::ToolCall {
                id,
                name,
                arguments,
                ..
            } => id
                .as_deref()
                .unwrap_or_default()
                .len()
                .saturating_add(name.as_deref().unwrap_or_default().len())
                .saturating_add(arguments.len()),
        };
        let total = self
            .bytes
            .checked_add(added)
            .ok_or(ModelProtocolError::OutputLimitExceeded)?;
        if total > self.limits.max_output_bytes {
            return Err(ModelProtocolError::OutputLimitExceeded);
        }
        match delta {
            GenerationDelta::Text { text } => self
                .partial
                .content
                .get_or_insert_with(String::new)
                .push_str(text),
            GenerationDelta::ToolCall {
                index,
                id,
                name,
                arguments,
            } => {
                let position = self
                    .partial
                    .tool_calls
                    .iter()
                    .position(|call| call.index == *index);
                let position = match position {
                    Some(position) => position,
                    None => {
                        if self.partial.tool_calls.len() >= self.limits.max_tool_calls {
                            return Err(ModelProtocolError::OutputLimitExceeded);
                        }
                        self.partial.tool_calls.push(ToolCallFragment {
                            index: *index,
                            ..Default::default()
                        });
                        self.partial.tool_calls.len() - 1
                    }
                };
                let call = &mut self.partial.tool_calls[position];
                if let Some(id) = id {
                    call.id.push_str(id);
                }
                if let Some(name) = name {
                    call.name.push_str(name);
                }
                call.arguments.push_str(arguments);
            }
        }
        self.bytes = total;
        Ok(())
    }

    pub fn response(
        &self,
        finish_reason: FinishReason,
        usage: TokenUsage,
    ) -> Result<GenerationResponse, ModelProtocolError> {
        let mut fragments = self.partial.tool_calls.clone();
        fragments.sort_by_key(|call| call.index);
        let mut tool_calls = Vec::new();
        let mut incomplete_tool_calls = Vec::new();
        if matches!(finish_reason, FinishReason::Stop | FinishReason::ToolCalls) {
            for fragment in fragments {
                tool_calls.push(ModelToolCall {
                    id: ProviderToolCallId::new(fragment.id)?,
                    name: fragment.name,
                    arguments: serde_json::from_str(&fragment.arguments)
                        .map_err(|_| ModelProtocolError::InvalidJsonOutput)?,
                });
            }
        } else {
            incomplete_tool_calls = fragments;
        }
        Ok(GenerationResponse {
            content: self.partial.content.clone(),
            tool_calls,
            incomplete_tool_calls,
            finish_reason,
            usage,
        })
    }

    pub fn verify_terminal(&self, response: &GenerationResponse) -> Result<(), ModelProtocolError> {
        if &self.response(response.finish_reason.clone(), response.usage.clone())? != response {
            return Err(ModelProtocolError::StreamTerminalMismatch);
        }
        Ok(())
    }
}
