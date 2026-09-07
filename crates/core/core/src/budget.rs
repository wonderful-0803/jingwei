//! Budget vocabulary. Reports are observations, not durable snapshots or authority.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::{SessionId, TaskId, TurnId};

/// Independently limited resources; counters and variable usage are never interchangeable.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum BudgetResource {
    Steps,
    ModelRequests,
    ToolCalls,
    Corrections,
    InputTokens,
    OutputTokens,
    ToolOutputBytes,
}

impl BudgetResource {
    pub const ALL: [Self; 7] = [
        Self::Steps,
        Self::ModelRequests,
        Self::ToolCalls,
        Self::Corrections,
        Self::InputTokens,
        Self::OutputTokens,
        Self::ToolOutputBytes,
    ];

    pub fn is_attempt(self) -> bool {
        matches!(
            self,
            Self::Steps | Self::ModelRequests | Self::ToolCalls | Self::Corrections
        )
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct BudgetAmounts {
    pub steps: u64,
    pub model_requests: u64,
    pub tool_calls: u64,
    pub corrections: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub tool_output_bytes: u64,
}

impl BudgetAmounts {
    pub fn get(&self, resource: BudgetResource) -> u64 {
        match resource {
            BudgetResource::Steps => self.steps,
            BudgetResource::ModelRequests => self.model_requests,
            BudgetResource::ToolCalls => self.tool_calls,
            BudgetResource::Corrections => self.corrections,
            BudgetResource::InputTokens => self.input_tokens,
            BudgetResource::OutputTokens => self.output_tokens,
            BudgetResource::ToolOutputBytes => self.tool_output_bytes,
        }
    }

    pub fn set(&mut self, resource: BudgetResource, value: u64) {
        match resource {
            BudgetResource::Steps => self.steps = value,
            BudgetResource::ModelRequests => self.model_requests = value,
            BudgetResource::ToolCalls => self.tool_calls = value,
            BudgetResource::Corrections => self.corrections = value,
            BudgetResource::InputTokens => self.input_tokens = value,
            BudgetResource::OutputTokens => self.output_tokens = value,
            BudgetResource::ToolOutputBytes => self.tool_output_bytes = value,
        }
    }
}

/// All limits are finite and explicit. Zero prohibits that resource.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BudgetLimits {
    pub resources: BudgetAmounts,
    pub active_time: Duration,
}

impl BudgetLimits {
    pub fn tightened_by(self, other: Self) -> Self {
        let mut resources = self.resources;
        for resource in BudgetResource::ALL {
            resources.set(
                resource,
                resources.get(resource).min(other.resources.get(resource)),
            );
        }
        Self {
            resources,
            active_time: self.active_time.min(other.active_time),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BudgetIdentity {
    pub task_id: TaskId,
    pub session_id: SessionId,
    pub agent_key: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum TokenBudgetMode {
    Hard,
    Soft,
}

/// A trusted admission layer supplies evidence; this enum does not verify a tokenizer.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum TokenBoundEvidence {
    VerifiedUpperBound,
    Estimate,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BudgetRequest {
    pub amounts: BudgetAmounts,
    pub input_tokens: TokenBoundEvidence,
    pub output_tokens: TokenBoundEvidence,
}

impl BudgetRequest {
    pub fn new(amounts: BudgetAmounts) -> Self {
        Self {
            amounts,
            input_tokens: TokenBoundEvidence::Estimate,
            output_tokens: TokenBoundEvidence::Estimate,
        }
    }

    pub fn with_token_evidence(
        mut self,
        input: TokenBoundEvidence,
        output: TokenBoundEvidence,
    ) -> Self {
        self.input_tokens = input;
        self.output_tokens = output;
        self
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum UsageValue {
    Actual(u64),
    Estimated(u64),
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BudgetUsage {
    pub input_tokens: UsageValue,
    pub output_tokens: UsageValue,
    pub tool_output_bytes: UsageValue,
}

impl BudgetUsage {
    pub fn actual(input_tokens: u64, output_tokens: u64, tool_output_bytes: u64) -> Self {
        Self {
            input_tokens: UsageValue::Actual(input_tokens),
            output_tokens: UsageValue::Actual(output_tokens),
            tool_output_bytes: UsageValue::Actual(tool_output_bytes),
        }
    }
}

/// Evidence totals, separate from the conservative charged amount. Unknown is a count.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct UsageTotals {
    pub actual: u64,
    pub estimated: u64,
    pub unknown: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct BudgetUsageReport {
    pub input_tokens: UsageTotals,
    pub output_tokens: UsageTotals,
    pub tool_output_bytes: UsageTotals,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum BudgetStopReason {
    /// A restored interrupted run requires host reconciliation; never auto-replay it.
    RecoveryRequired,
    ResourceLimit(BudgetResource),
    ActiveTime,
    UsageExceededReservation(BudgetResource),
    AbandonedReservation,
    RunAbandoned,
    ClockMovedBackwards,
    AccountingOverflow,
    IdentityMismatch,
    InvalidRequest,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BudgetRunReport {
    pub turn_id: Option<TurnId>,
    pub metrics: BudgetExecutionMetrics,
    pub limits: BudgetLimits,
    pub charged: BudgetAmounts,
    pub reserved: BudgetAmounts,
    pub active_time: Duration,
    pub cleanup_time: Duration,
    pub open: bool,
    pub stop: Option<BudgetStopReason>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BudgetPendingReport {
    pub id: u64,
    pub request: BudgetRequest,
    pub started: bool,
    pub abandoned: bool,
    /// Raw evidence retained if arithmetic prevents settlement.
    pub failed_usage: Option<BudgetUsage>,
}

/// In-memory observation only. Deserializing this type cannot create a live ledger.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BudgetReport {
    pub identity: BudgetIdentity,
    pub limits: BudgetLimits,
    pub token_mode: TokenBudgetMode,
    pub charged: BudgetAmounts,
    pub reserved: BudgetAmounts,
    pub active_time: Duration,
    pub cleanup_time: Duration,
    pub stop: Option<BudgetStopReason>,
    pub usage: BudgetUsageReport,
    pub run: Option<BudgetRunReport>,
    pub pending: Vec<BudgetPendingReport>,
}

/// Finite compatibility limits used by canonical runtimes for an unbound turn.
impl Default for BudgetLimits {
    fn default() -> Self {
        Self {
            resources: BudgetAmounts {
                steps: 128,
                model_requests: 128,
                tool_calls: 128,
                corrections: 32,
                input_tokens: 4_000_000,
                output_tokens: 1_000_000,
                tool_output_bytes: 16 * 1024 * 1024,
            },
            active_time: Duration::from_secs(600),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum BudgetEventKind {
    ModelRequest,
    ModelResult,
    ToolCall,
    ToolResult,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct BudgetEventCounts {
    pub model_requests: u64,
    pub model_results: u64,
    pub tool_calls: u64,
    pub tool_results: u64,
}

/// Duration sums for capability work; parallel durations are not task active time.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct BudgetExecutionMetrics {
    pub session_wait: Duration,
    pub model_queue: Duration,
    pub model_execution: Duration,
    pub tool_execution: Duration,
    pub confirmed: BudgetEventCounts,
    pub unconfirmed: BudgetEventCounts,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "u16", into = "u16")]
pub enum TaskRunReportVersion {
    V1,
}
impl TryFrom<u16> for TaskRunReportVersion {
    type Error = &'static str;
    fn try_from(value: u16) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::V1),
            _ => Err("unsupported task run report version"),
        }
    }
}
impl From<TaskRunReportVersion> for u16 {
    fn from(_: TaskRunReportVersion) -> Self {
        1
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum TaskRunStop {
    Completed,
    WaitingForInput,
    CallerCancelled,
    RuntimeStopping,
    Budget(BudgetStopReason),
    Failed,
}

/// Observation after capability drain, before this report and the terminal are written.
/// This record is not a durable authority or a recovery snapshot.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TaskRunReport {
    pub version: TaskRunReportVersion,
    pub budget: BudgetReport,
    pub stop: TaskRunStop,
    pub capabilities_drained: bool,
}
