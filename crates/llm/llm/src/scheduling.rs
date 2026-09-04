//! In-memory scheduling configuration and observations, not canonical event records.

use std::time::Duration;

use jingwei_core::ModelCallId;

use crate::ModelRecordStage;

/// Host limits shared by complete and streaming calls across all bound turns.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ModelSchedulerConfig {
    /// Slots reserved for request recording/preflight or provider execution. Must be positive.
    pub max_concurrency: usize,
    /// Calls waiting for a slot. Zero disables waiting.
    pub max_queued: usize,
    /// All accepted calls, including unfinished recording. At least max_concurrency.
    pub max_inflight: usize,
    /// Combined JSON bytes of the generation request and effective recorded options.
    /// Excludes fixed event envelope/ID overhead and caller-owned input allocations.
    pub max_request_bytes: usize,
}

impl ModelSchedulerConfig {
    pub const fn new() -> Self {
        Self {
            max_concurrency: 4,
            max_queued: 32,
            max_inflight: 64,
            max_request_bytes: 8 * 1024 * 1024,
        }
    }
}

impl Default for ModelSchedulerConfig {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelOverloadKind {
    QueueFull,
    InflightFull,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelJobPhase {
    Queued,
    /// A slot is assigned, but raw provider execution has not begun.
    Preparing,
    Executing,
    /// No execution slot is held; accepted recording/closure is still owned.
    Cleaning,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelJobStopReason {
    Cancelled,
    QueueTimeout,
    Timeout,
    ConsumerDropped,
    Failed,
}

/// Observation of one accepted, not yet fully settled job. Never includes model input.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelJobReport {
    pub call_id: ModelCallId,
    pub phase: ModelJobPhase,
    pub recording: Option<ModelRecordStage>,
    pub request_recorded: bool,
    pub result_recorded: bool,
    pub stop_reason: Option<ModelJobStopReason>,
    /// Original effective timeout, unchanged in the canonical request and adapter options.
    pub timeout: Duration,
    pub remaining_time: Duration,
    pub queue_time: Duration,
    pub preparation_time: Duration,
    pub execution_time: Duration,
    pub cleanup_time: Duration,
}

/// Bounded observation of current jobs; completed history is not retained here.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelSchedulerSnapshot {
    pub config: ModelSchedulerConfig,
    pub accepting: bool,
    pub inflight: usize,
    pub queued: usize,
    pub preparing: usize,
    pub executing: usize,
    pub cleaning: usize,
    pub jobs: Vec<ModelJobReport>,
}
