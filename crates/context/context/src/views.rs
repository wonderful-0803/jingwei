//! Controlled tool selection: strategies return names, never replacement schemas.
use std::collections::{BTreeMap, BTreeSet};

use jingwei_core::{
    GenerationConstraint, GenerationRequest, ModelMessage, ModelToolDefinition, ToolChoice,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ViewError {
    #[error("invalid view configuration or identity")]
    Invalid,
    #[error("view resource limit exceeded")]
    Limit,
    #[error("selection contains an unauthorized, duplicate or unknown tool/group")]
    Unauthorized,
    #[error("content reference is missing, expired, or outside the host scope")]
    Unavailable,
    #[error("source identity already names different content")]
    Conflict,
    #[error("invalid UTF-8 cursor or page too small for one character")]
    Cursor,
    #[error("content store lock poisoned")]
    Poisoned,
}

pub trait ToolSelector: Send + Sync {
    fn select(&self, authorized: &[ModelToolDefinition]) -> Result<Vec<String>, ViewError>;
}

/// Union explicit names and active groups; None with no active groups selects all.
/// Group membership is host metadata, not authorization.
#[derive(Clone, Debug, Default)]
pub struct GroupedToolSelector {
    pub names: Option<BTreeSet<String>>,
    pub groups: BTreeMap<String, BTreeSet<String>>,
    pub active_groups: BTreeSet<String>,
}
impl ToolSelector for GroupedToolSelector {
    fn select(&self, authorized: &[ModelToolDefinition]) -> Result<Vec<String>, ViewError> {
        if self.names.is_none() && self.active_groups.is_empty() {
            return Ok(authorized.iter().map(|tool| tool.name.clone()).collect());
        }
        let mut selected = self.names.clone().unwrap_or_default();
        for group in &self.active_groups {
            selected.extend(
                self.groups
                    .get(group)
                    .ok_or(ViewError::Unauthorized)?
                    .iter()
                    .cloned(),
            );
        }
        Ok(selected.into_iter().collect())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ToolViewLimits {
    pub max_catalog: usize,
    pub max_visible: usize,
    pub max_input_bytes: usize,
}
impl Default for ToolViewLimits {
    fn default() -> Self {
        Self {
            max_catalog: 1024,
            max_visible: 64,
            max_input_bytes: 4 * 1024 * 1024,
        }
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ToolView {
    pub algorithm_version: u16,
    pub tools: Vec<ModelToolDefinition>,
    pub excluded: Vec<String>,
}

/// The gateway's granted snapshot is the authority; all definitions remain exact.
pub fn select_tool_view(
    authorized: &[ModelToolDefinition],
    selector: &dyn ToolSelector,
    limits: ToolViewLimits,
) -> Result<ToolView, ViewError> {
    if limits.max_catalog == 0 || limits.max_visible == 0 || limits.max_input_bytes == 0 {
        return Err(ViewError::Invalid);
    }
    if authorized.len() > limits.max_catalog {
        return Err(ViewError::Limit);
    }
    let mut sink = crate::CountBytes {
        remaining: limits.max_input_bytes,
    };
    serde_json::to_writer(&mut sink, authorized).map_err(|_| ViewError::Limit)?;
    if !authorized.is_empty() {
        GenerationRequest {
            messages: vec![ModelMessage::user("")],
            constraint: GenerationConstraint::NativeTools {
                tools: authorized.to_vec(),
                choice: ToolChoice::Auto,
            },
        }
        .validate_shape()
        .map_err(|_| ViewError::Invalid)?;
    }
    let mut sorted = authorized.to_vec();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    let selected = selector.select(&sorted)?;
    if selected.len() > limits.max_visible {
        return Err(ViewError::Limit);
    }
    let names: BTreeSet<_> = selected.iter().collect();
    if names.len() != selected.len()
        || names
            .iter()
            .any(|name| !sorted.iter().any(|t| &t.name == *name))
    {
        return Err(ViewError::Unauthorized);
    }
    let (tools, excluded): (Vec<_>, Vec<_>) =
        sorted.into_iter().partition(|t| names.contains(&t.name));
    Ok(ToolView {
        algorithm_version: 1,
        tools,
        excluded: excluded.into_iter().map(|t| t.name).collect(),
    })
}
