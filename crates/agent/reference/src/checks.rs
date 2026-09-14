//! Host observations have no runtime, tool, event-writing or budget authority.
use std::{collections::BTreeMap, sync::Arc};

use jingwei_core::{TaskId, TurnId};
use serde::{Deserialize, Serialize};

use crate::ReferenceConfigError;

pub struct CheckContext<'a> {
    pub task_id: &'a TaskId,
    pub turn_id: &'a TurnId,
}
/// Nonblocking snapshot of host-owned business state, not an inference counter.
pub trait ReferenceState: Send + Sync {
    fn version(&self, context: &CheckContext<'_>) -> Result<Option<String>, ReferenceConfigError>;
}
#[derive(Default)]
pub struct UnknownReferenceState;
impl ReferenceState for UnknownReferenceState {
    fn version(&self, _: &CheckContext<'_>) -> Result<Option<String>, ReferenceConfigError> {
        Ok(None)
    }
}
pub struct CompletionInput<'a> {
    pub context: CheckContext<'a>,
    pub claim: &'a str,
    /// Snapshot taken before the Final inference. Checker must validate freshness
    /// against its own host evidence if business state can change concurrently.
    pub state_version: Option<&'a str>,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum CompletionDecision {
    Unverified,
    /// Opaque host evidence reference; the framework cannot verify its truth.
    Verified {
        evidence: String,
    },
    Rejected {
        reason: String,
    },
}
impl CompletionDecision {
    pub(crate) fn valid(&self) -> bool {
        match self {
            Self::Unverified => true,
            Self::Verified { evidence: value } | Self::Rejected { reason: value } => {
                !value.trim().is_empty() && value.len() <= 4096
            }
        }
    }
}
/// A synchronous, data-only decision. Rejection stops immediately, without retry.
pub trait CompletionChecker: Send + Sync {
    fn check(&self, input: CompletionInput<'_>)
    -> Result<CompletionDecision, ReferenceConfigError>;
}
#[derive(Default)]
pub struct UnverifiedCompletion;
impl CompletionChecker for UnverifiedCompletion {
    fn check(&self, _: CompletionInput<'_>) -> Result<CompletionDecision, ReferenceConfigError> {
        Ok(CompletionDecision::Unverified)
    }
}
/// Consecutive identical completed tool observations, including the first one.
/// IDs are excluded. Full arguments/result and host state are compared exactly.
/// Only one bounded previous observation is retained per turn.
#[derive(Clone, Debug)]
pub struct ProgressPolicy {
    pub max_repetitions: u32,
    /// Explicit per-tool polling thresholds; never override step or Task limits.
    pub polling_limits: BTreeMap<String, u32>,
    pub max_observation_bytes: usize,
}
impl Default for ProgressPolicy {
    fn default() -> Self {
        Self {
            max_repetitions: 3,
            polling_limits: BTreeMap::new(),
            max_observation_bytes: 1024 * 1024,
        }
    }
}
impl ProgressPolicy {
    pub(crate) fn validate(&self) -> Result<(), ReferenceConfigError> {
        if !(2..=1024).contains(&self.max_repetitions)
            || self.max_observation_bytes == 0
            || self.max_observation_bytes > 16 * 1024 * 1024
            || self.polling_limits.len() > 1024
            || self.polling_limits.iter().any(|(name, limit)| {
                name.trim().is_empty() || name.len() > 4096 || !(2..=1024).contains(limit)
            })
        {
            return Err(ReferenceConfigError);
        }
        Ok(())
    }
    pub(crate) fn limit(&self, name: &str) -> u32 {
        self.polling_limits
            .get(name)
            .copied()
            .unwrap_or(self.max_repetitions)
    }
}
pub struct ReferenceChecks {
    pub completion: Arc<dyn CompletionChecker>,
    pub state: Arc<dyn ReferenceState>,
    pub progress: ProgressPolicy,
}

impl Default for ReferenceChecks {
    fn default() -> Self {
        Self {
            completion: Arc::new(UnverifiedCompletion),
            state: Arc::new(UnknownReferenceState),
            progress: ProgressPolicy::default(),
        }
    }
}
