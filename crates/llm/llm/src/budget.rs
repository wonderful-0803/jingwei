//! Trusted host input to model token admission; never supplied by model call options.

use jingwei_budget::{BudgetError, TokenBoundEvidence};

use crate::{GenerationOptions, GenerationRequest};

/// Independent input/output token bounds and the evidence supporting each value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ModelTokenEstimate {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub input_evidence: TokenBoundEvidence,
    pub output_evidence: TokenBoundEvidence,
}

/// Host-configured, synchronous, nonblocking token accounting policy.
///
/// Hard budgets require verified upper bounds for both dimensions. Implementors
/// must account for provider serialization and tokenizer behavior; a configured
/// `max_tokens` value alone is not proof of an enforced output bound.
pub trait ModelBudgetEstimator: Send + Sync {
    fn estimate(
        &self,
        request: &GenerationRequest,
        options: &GenerationOptions,
    ) -> Result<ModelTokenEstimate, BudgetError>;
}
