//! Runtime composition root and thin host-facing AgentRuntime harness.

use std::sync::Arc;

use jingwei_agent::{
    AGENT_RUNTIME, AgentRuntime, AgentRuntimeError, AgentTurnController, AgentTurnReport,
    AgentTurnRequest,
};
use jingwei_core::SessionId;
use jingwei_llm::{LLM_PROVIDER, LLM_RUNTIME};
use jingwei_plugin::{Plugin, PluginError, PluginRegistry, Registrar, ShutdownError};
use jingwei_session::{SESSION_PERSISTENCE, SESSION_RUNTIME};
use jingwei_tool::TOOL_RUNTIME;

/// Harness composition, delegated turn, and shutdown failures.
#[derive(Debug, thiserror::Error)]
pub enum HarnessError {
    #[error("plugin composition failed: {0}")]
    Plugin(#[from] PluginError),
    #[error(transparent)]
    AgentRuntime(#[from] AgentRuntimeError),
    #[error("service shutdown failed: {0}")]
    Shutdown(#[from] ShutdownError),
}

struct HarnessInner {
    registry: PluginRegistry,
    agent_runtime: Arc<dyn AgentRuntime>,
}

/// A cloneable handle to the composed Jingwei runtime.
#[derive(Clone)]
pub struct Harness {
    inner: Arc<HarnessInner>,
}

/// Collection-only builder for the Harness composition root.
pub struct HarnessBuilder {
    registrar: Registrar,
}

impl Default for HarnessBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl HarnessBuilder {
    pub fn new() -> Self {
        let mut registrar = Registrar::default();
        registrar.require(AGENT_RUNTIME);
        Self { registrar }
    }

    pub fn plugin<P: Plugin + 'static>(mut self, plugin: P) -> Self {
        self.registrar.add(plugin);
        self
    }

    pub fn select_agent_runtime(mut self, key: impl Into<String>) -> Self {
        self.registrar.select(AGENT_RUNTIME, key);
        self
    }

    pub fn select_llm(mut self, key: impl Into<String>) -> Self {
        self.registrar.select(LLM_PROVIDER, key);
        self
    }

    pub fn select_llm_runtime(mut self, key: impl Into<String>) -> Self {
        self.registrar.select(LLM_RUNTIME, key);
        self
    }

    pub fn select_persistence(mut self, key: impl Into<String>) -> Self {
        self.registrar.select(SESSION_PERSISTENCE, key);
        self
    }

    pub fn select_session_runtime(mut self, key: impl Into<String>) -> Self {
        self.registrar.select(SESSION_RUNTIME, key);
        self
    }

    pub fn select_tool_runtime(mut self, key: impl Into<String>) -> Self {
        self.registrar.select(TOOL_RUNTIME, key);
        self
    }

    pub async fn build(self) -> Result<Harness, HarnessError> {
        let registry = self.registrar.finish().await?;
        let agent_runtime = registry
            .agent_runtime()
            .expect("AGENT_RUNTIME is required and validated before the registry is frozen");
        Ok(Harness {
            inner: Arc::new(HarnessInner {
                registry,
                agent_runtime,
            }),
        })
    }
}

impl Harness {
    /// Start a host-owned request, including its optional shared Task budget.
    pub fn start_turn_request(
        &self,
        request: AgentTurnRequest,
    ) -> Result<Box<dyn AgentTurnController>, HarnessError> {
        Ok(self.inner.agent_runtime.start_turn(request)?)
    }

    pub fn start_turn(
        &self,
        session_id: &SessionId,
        agent_name: &str,
        user_message: &str,
    ) -> Result<Box<dyn AgentTurnController>, HarnessError> {
        Ok(self.inner.agent_runtime.start_turn(AgentTurnRequest::new(
            session_id.clone(),
            agent_name,
            user_message,
        ))?)
    }

    pub async fn run_turn(
        &self,
        session_id: &SessionId,
        agent_name: &str,
        user_message: &str,
    ) -> Result<AgentTurnReport, HarnessError> {
        let controller = self.start_turn(session_id, agent_name, user_message)?;
        Ok(controller.wait().await?)
    }

    pub async fn shutdown(&self) -> Result<(), HarnessError> {
        self.inner.registry.shutdown().await?;
        Ok(())
    }
}
