//! Jingwei 框架核心：领域词汇与事件信封。
//!
//! 本 crate 只包含"语言"：ID、事件、错误分类与 post-commit EventObserver 契约。
//! 不依赖任何传输/IO 具体实现（不变式 6：依赖方向门禁在 CI 断言）。

pub mod cancellation;
pub mod capability;
pub mod event;
pub mod id;
pub mod observer;

pub use cancellation::{CancellationFuture, CancellationSignal};
pub use capability::CapabilityId;
pub use event::{
    ChatMessage, DoneStatus, ModelCallMode, ModelFailureCategory, ModelRecordedOutcome,
    ModelRequest, ModelRequestOptions, ModelResult, ModelTimeout, ModelTimeoutError, Role,
    SessionEvent, SessionEventKind, StageStatus, ToolCall, ToolFailureCategory,
    ToolRecordedOutcome, ToolResult,
};
pub use id::{EventId, GenerationId, MessageId, ModelCallId, SessionId, TurnId};
pub use observer::{EventObserver, ObserverError, ObserverFuture};
