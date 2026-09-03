use crate::ActionError;
use jingwei_llm::{GenerationConstraint, GenerationRequest, GenerationResponse};
use serde_json::Value;

struct NoExternalResources;
impl jsonschema::Retrieve for NoExternalResources {
    fn retrieve(
        &self,
        _: &jsonschema::Uri<String>,
    ) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        Err("external schema resources are disabled".into())
    }
}

pub(crate) fn compile(schema: &Value) -> Result<jsonschema::Validator, ActionError> {
    jsonschema::options()
        .with_retriever(NoExternalResources)
        .build(schema)
        .map_err(|_| ActionError::InvalidSchema)
}

pub(crate) fn validate_response(
    request: &GenerationRequest,
    response: &GenerationResponse,
) -> Result<(), ActionError> {
    response.validate_shape_for(request)?;
    if let GenerationConstraint::JsonSchema { schema, .. } = &request.constraint {
        let value: Value = serde_json::from_str(response.content.as_deref().unwrap_or_default())
            .map_err(|_| ActionError::InvalidAction)?;
        if !compile(schema)?.is_valid(&value) {
            return Err(ActionError::InvalidAction);
        }
    }
    Ok(())
}

pub(crate) fn text(text: &str) -> Result<(), ActionError> {
    if text.trim().is_empty() {
        Err(ActionError::EmptyText)
    } else {
        Ok(())
    }
}
