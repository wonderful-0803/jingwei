use super::Error;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub backend: String,
    pub model: String,
    pub endpoint: Option<String>,
    pub api_key_env: Option<String>,
    pub template_revision: String,
    pub weights: String,
    pub quantization: String,
    pub device: String,
    pub protocol: String,
    pub tasks: String,
    pub runs: u32,
    pub max_steps: u32,
    pub max_tokens: u32,
    pub timeout_seconds: u64,
}
impl Config {
    pub fn validate(&self) -> Result<(), Error> {
        if !matches!(self.backend.as_str(), "fake" | "openai")
            || !matches!(self.protocol.as_str(), "json" | "native")
            || !(1..=100).contains(&self.runs)
            || !(1..=64).contains(&self.max_steps)
            || !(1..=4096).contains(&self.max_tokens)
            || !(1..=600).contains(&self.timeout_seconds)
            || [
                &self.model,
                &self.template_revision,
                &self.weights,
                &self.quantization,
                &self.device,
                &self.tasks,
            ]
            .iter()
            .any(|s| s.trim().is_empty())
        {
            return Err("invalid evaluation configuration".into());
        }
        if self.backend == "fake" && (self.endpoint.is_some() || self.api_key_env.is_some()) {
            return Err("fake backend must not specify endpoint or credentials".into());
        }
        Ok(())
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Case {
    pub id: String,
    pub domain: String,
    pub prompt: String,
    pub initial: BTreeMap<String, String>,
    pub expected: BTreeMap<String, String>,
    pub writable: Vec<String>,
    /// Script is used exclusively by the fake provider; never sent to a real model.
    pub fake_actions: Vec<serde_json::Value>,
}
pub fn validate_cases(cases: &[Case], selected: &[String]) -> Result<(), Error> {
    let mut ids = BTreeSet::new();
    if cases.is_empty() {
        return Err("empty task set".into());
    }
    for case in cases {
        if case.id.is_empty()
            || !case
                .id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            || !ids.insert(case.id.clone())
            || case.prompt.trim().is_empty()
            || case.domain.trim().is_empty()
            || case.expected.is_empty()
            || case.fake_actions.is_empty()
        {
            return Err("invalid or duplicate case".into());
        }
    }
    if selected.iter().any(|id| !ids.contains(id)) {
        return Err("unknown selected case".into());
    }
    Ok(())
}
