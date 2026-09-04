//! Model accounting ownership, independent of scheduling and canonical recording.

use std::io::{self, Write};

use jingwei_budget::{
    BudgetAmounts, BudgetClock, BudgetError, BudgetIdentity, BudgetLimits, BudgetRequest,
    BudgetReservation, BudgetRun, BudgetScope, BudgetStopReason, BudgetUsage, TaskBudget,
    TokenBoundEvidence, TokenBudgetMode, UsageValue,
};
use jingwei_core::{SessionEvent, SessionId, TaskId, TurnId};
use jingwei_llm::{ModelBudgetEstimator, ModelTokenEstimate, TokenUsage};

use super::*;

struct ExecutorClock(Instant);

impl BudgetClock for ExecutorClock {
    fn now(&self) -> Duration {
        self.0.elapsed()
    }
}

pub(super) struct TurnBudget {
    pub scope: BudgetScope,
    explicit: bool,
    expected_turn: Option<TurnId>,
    owned_run: Mutex<Option<BudgetRun>>,
}

impl TurnBudget {
    pub fn new(scope: Option<BudgetScope>) -> Result<Self, BudgetError> {
        if let Some(scope) = scope {
            scope.check_active()?;
            let expected_turn = scope.turn_id()?;
            if expected_turn.is_none() {
                let _ = scope.stop_with(BudgetStopReason::IdentityMismatch);
                return Err(BudgetError::IdentityMismatch);
            }
            return Ok(Self {
                scope,
                explicit: true,
                expected_turn,
                owned_run: Mutex::new(None),
            });
        }
        // A legacy low-level binding still has a finite, isolated run. Its
        // synthetic identity is not asserted to be the recorder's Session ID.
        let limits = BudgetLimits::default();
        let task = TaskBudget::new(
            BudgetIdentity {
                task_id: TaskId::new(),
                session_id: SessionId::new(),
                agent_key: "canonical-model-runtime".into(),
            },
            limits,
            limits,
            TokenBudgetMode::Soft,
            Arc::new(ExecutorClock(Instant::now())),
        )?;
        let run = task.begin_run(TurnId::new(), limits)?;
        Ok(Self {
            scope: run.scope(),
            explicit: false,
            expected_turn: None,
            owned_run: Mutex::new(Some(run)),
        })
    }

    pub fn check_context(&self, options: &GenerationOptions) -> Result<(), BudgetError> {
        self.scope.check_active()?;
        if self.explicit
            && options
                .context
                .as_ref()
                .is_some_and(|context| context.task_id != self.scope.identity().task_id)
        {
            let _ = self.scope.stop_with(BudgetStopReason::IdentityMismatch);
            return Err(BudgetError::IdentityMismatch);
        }
        Ok(())
    }

    pub fn check_event(&self, event: &SessionEvent) -> Result<(), BudgetError> {
        if self.explicit
            && (event.session_id != self.scope.identity().session_id
                || self.expected_turn.as_ref() != Some(&event.turn_id))
        {
            let _ = self.scope.stop_with(BudgetStopReason::IdentityMismatch);
            return Err(BudgetError::IdentityMismatch);
        }
        Ok(())
    }

    pub fn finish_owned(&self) -> Result<(), BudgetError> {
        let mut run = self
            .owned_run
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(run) = run.as_mut() {
            run.finish()?;
        }
        *run = None;
        Ok(())
    }
}

/// Explicitly soft heuristic: UTF-8 JSON bytes plus 1024 tokens for provider
/// framing, and the caller's output cap or 4096 tokens. Neither is a hard bound.
pub(super) struct SoftModelBudgetEstimator;

impl ModelBudgetEstimator for SoftModelBudgetEstimator {
    fn estimate(
        &self,
        request: &GenerationRequest,
        options: &GenerationOptions,
    ) -> Result<ModelTokenEstimate, BudgetError> {
        struct Counter(u64);
        impl Write for Counter {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.0 = self
                    .0
                    .checked_add(bytes.len() as u64)
                    .ok_or_else(|| io::Error::other("model token estimate overflow"))?;
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut counter = Counter(0);
        serde_json::to_writer(&mut counter, request)
            .map_err(|_| BudgetError::Stopped(BudgetStopReason::AccountingOverflow))?;
        Ok(ModelTokenEstimate {
            input_tokens: counter
                .0
                .checked_add(1024)
                .ok_or(BudgetError::Stopped(BudgetStopReason::AccountingOverflow))?,
            output_tokens: u64::from(options.max_tokens.unwrap_or(4096)).max(1),
            input_evidence: TokenBoundEvidence::Estimate,
            output_evidence: TokenBoundEvidence::Estimate,
        })
    }
}

pub(super) fn request_estimate(
    turn: &TurnState,
    input: &GenerationRequest,
    options: &GenerationOptions,
) -> Result<BudgetRequest, ModelGatewayError> {
    turn.budget
        .check_context(options)
        .map_err(ModelRuntimeError::Budget)?;
    let estimate = catch_unwind(AssertUnwindSafe(|| {
        turn.runtime.estimator.estimate(input, options)
    }));
    let estimate = match estimate {
        Ok(Ok(estimate)) => estimate,
        Ok(Err(error)) => {
            let _ = turn
                .budget
                .scope
                .stop_with(BudgetStopReason::InvalidRequest);
            return Err(ModelRuntimeError::Budget(error).into());
        }
        Err(_) => {
            let _ = turn
                .budget
                .scope
                .stop_with(BudgetStopReason::InvalidRequest);
            return Err(ModelGatewayError::Internal {
                code: "model_budget_estimator_panicked".into(),
                message: "Trusted model budget estimator panicked".into(),
            });
        }
    };
    Ok(BudgetRequest::new(BudgetAmounts {
        steps: 1,
        model_requests: 1,
        input_tokens: estimate.input_tokens,
        output_tokens: estimate.output_tokens,
        ..Default::default()
    })
    .with_token_evidence(estimate.input_evidence, estimate.output_evidence))
}

pub(super) struct JobBudget {
    reservation: Option<BudgetReservation>,
    started: bool,
    usage: BudgetUsage,
}

impl JobBudget {
    pub fn new(reservation: BudgetReservation) -> Self {
        Self {
            reservation: Some(reservation),
            started: false,
            usage: BudgetUsage {
                input_tokens: UsageValue::Unknown,
                output_tokens: UsageValue::Unknown,
                tool_output_bytes: UsageValue::Actual(0),
            },
        }
    }

    pub fn start(&mut self) -> Result<(), BudgetError> {
        self.reservation
            .as_mut()
            .ok_or(BudgetError::ReservationMissing)?
            .mark_started()?;
        self.started = true;
        Ok(())
    }

    pub fn observe(&mut self, usage: &TokenUsage) {
        self.usage.input_tokens = usage
            .input_tokens
            .map_or(UsageValue::Unknown, UsageValue::Actual);
        self.usage.output_tokens = usage
            .output_tokens
            .map_or(UsageValue::Unknown, UsageValue::Actual);
    }

    pub fn settle(&mut self) -> Result<(), BudgetError> {
        let Some(reservation) = self.reservation.take() else {
            return Ok(());
        };
        if self.started {
            reservation.settle(self.usage)
        } else {
            reservation.cancel_before_start()
        }
    }
}

impl Drop for JobBudget {
    fn drop(&mut self) {
        // A driver lost before entry provably did not execute. After entry,
        // captured usage (or Unknown) remains charged even if recording unwinds.
        let _ = self.settle();
    }
}
