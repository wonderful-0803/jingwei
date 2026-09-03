//! Explicit composition of Jingwei's official core-runtime provider candidates.

use jingwei_agent_runtime::CanonicalAgentRuntimePlugin;
use jingwei_llm_runtime::CanonicalLlmRuntimePlugin;
use jingwei_runtime::HarnessBuilder;
use jingwei_session_runtime::CanonicalSessionRuntimePlugin;
use jingwei_tool_runtime::CanonicalToolRuntimePlugin;

/// The four official core-runtime candidates, without provider selection or activation.
pub struct StandardCoreBundle {
    session: CanonicalSessionRuntimePlugin,
    llm: CanonicalLlmRuntimePlugin,
    tool: CanonicalToolRuntimePlugin,
    agent: CanonicalAgentRuntimePlugin,
}

impl StandardCoreBundle {
    /// Create the standard candidate set with fail-closed runtime defaults.
    pub fn new() -> Self {
        Self {
            session: CanonicalSessionRuntimePlugin::new(),
            llm: CanonicalLlmRuntimePlugin::new(),
            tool: CanonicalToolRuntimePlugin::new(),
            agent: CanonicalAgentRuntimePlugin::new(),
        }
    }

    /// Replace the canonical AgentRuntime candidate configuration.
    #[must_use]
    pub fn with_agent_runtime(mut self, plugin: CanonicalAgentRuntimePlugin) -> Self {
        self.agent = plugin;
        self
    }

    /// Replace the canonical LlmRuntime candidate configuration.
    #[must_use]
    pub fn with_llm_runtime(mut self, plugin: CanonicalLlmRuntimePlugin) -> Self {
        self.llm = plugin;
        self
    }

    /// Replace the canonical ToolRuntime candidate configuration.
    #[must_use]
    pub fn with_tool_runtime(mut self, plugin: CanonicalToolRuntimePlugin) -> Self {
        self.tool = plugin;
        self
    }

    /// Submit the four candidates through the ordinary Harness plugin path.
    #[must_use]
    pub fn install_into(self, builder: HarnessBuilder) -> HarnessBuilder {
        builder
            .plugin(self.session)
            .plugin(self.llm)
            .plugin(self.tool)
            .plugin(self.agent)
    }
}

impl Default for StandardCoreBundle {
    fn default() -> Self {
        Self::new()
    }
}
