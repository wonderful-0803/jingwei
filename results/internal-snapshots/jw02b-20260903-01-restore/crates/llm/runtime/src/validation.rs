use jingwei_llm::{
    FinishReason, GenerationConstraint, GenerationLimits, GenerationRequest, GenerationResponse,
    LlmError, ModelCallMode, ModelCapabilities, ModelProtocolError,
};
use serde_json::Value;

// Count serialized bytes without allocating a second, potentially oversized copy.
struct BoundedSize(usize);
impl std::io::Write for BoundedSize {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self
            .0
            .checked_sub(bytes.len())
            .ok_or_else(|| std::io::Error::other("model output limit exceeded"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct NoExternalSchemas;
impl jsonschema::Retrieve for NoExternalSchemas {
    fn retrieve(
        &self,
        _: &jsonschema::Uri<String>,
    ) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        Err("external schema retrieval is disabled".into())
    }
}

fn compile(schema: &Value) -> Result<jsonschema::Validator, LlmError> {
    jsonschema::options()
        .with_retriever(NoExternalSchemas)
        .build(schema)
        .map_err(|_| ModelProtocolError::SchemaValidation.into())
}

pub(super) fn prepare(
    request: &GenerationRequest,
    capabilities: &ModelCapabilities,
    mode: ModelCallMode,
) -> Result<(), LlmError> {
    request.preflight(capabilities, mode)?;
    match &request.constraint {
        GenerationConstraint::Text => {}
        GenerationConstraint::JsonSchema { schema, .. } => {
            compile(schema)?;
        }
        GenerationConstraint::NativeTools { tools, .. } => {
            for tool in tools {
                compile(&tool.parameters)?;
            }
        }
    }
    Ok(())
}

pub(super) fn response(
    request: &GenerationRequest,
    response: &GenerationResponse,
    limits: GenerationLimits,
) -> Result<(), LlmError> {
    serde_json::to_writer(BoundedSize(limits.max_output_bytes), response)
        .map_err(|_| ModelProtocolError::OutputLimitExceeded)?;
    if response
        .tool_calls
        .len()
        .saturating_add(response.incomplete_tool_calls.len())
        > limits.max_tool_calls
    {
        return Err(ModelProtocolError::OutputLimitExceeded.into());
    }
    // A transport-complete but truncated/unknown response is diagnostic data,
    // never a validated executable message. Preserve its explicit metadata.
    if !matches!(
        response.finish_reason,
        FinishReason::Stop | FinishReason::ToolCalls
    ) {
        return Ok(());
    }
    response.validate_shape_for(request)?;
    match &request.constraint {
        GenerationConstraint::Text => {}
        GenerationConstraint::JsonSchema { schema, .. } => {
            let value: Value =
                serde_json::from_str(response.content.as_deref().unwrap_or_default())
                    .map_err(|_| ModelProtocolError::InvalidJsonOutput)?;
            if !compile(schema)?.is_valid(&value) {
                return Err(ModelProtocolError::SchemaValidation.into());
            }
        }
        GenerationConstraint::NativeTools { tools, .. } => {
            for call in &response.tool_calls {
                let tool = tools
                    .iter()
                    .find(|tool| tool.name == call.name)
                    .ok_or(ModelProtocolError::ToolNotVisible)?;
                if !compile(&tool.parameters)?.is_valid(&call.arguments) {
                    return Err(ModelProtocolError::SchemaValidation.into());
                }
            }
        }
    }
    Ok(())
}
