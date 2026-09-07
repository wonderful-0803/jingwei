//! Jingwei Rust Agent Harness 的公共 façade。
//!
//! 应用优先依赖本 crate；adapter 和框架内部实现继续依赖最窄的能力 crate。

/// Optional single-decision action protocols and controlled execution.
#[cfg(feature = "actions")]
pub mod action {
    pub use jingwei_action::*;
}

/// Agent 词汇与执行 seam。
pub mod agent {
    pub use jingwei_agent::*;
}

/// 事件信封与业务无关事件词汇。
pub mod event {
    pub use jingwei_core::event::*;
}

/// 关联标识词汇。
pub mod id {
    pub use jingwei_core::id::*;
}

/// LLM 能力词汇。
pub mod llm {
    pub use jingwei_llm::*;
}

/// 插件装配词汇。
pub mod plugin {
    pub use jingwei_plugin::*;
}

/// Harness 组合与运行入口。
pub mod runtime {
    pub use jingwei_runtime::*;
}

/// Session 持久化 seam。
pub mod session {
    pub use jingwei_session::*;
}

/// Tool 能力词汇。
pub mod tool {
    pub use jingwei_tool::*;
}

pub use jingwei_agent::AgentTurnReport;
pub use jingwei_runtime::{Harness, HarnessBuilder, HarnessError};
