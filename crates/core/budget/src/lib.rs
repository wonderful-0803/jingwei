//! Shared Task budget accounting, independent of executors and persistence.
//!
//! The host owns [`TaskBudget`] and a single [`BudgetRun`]. Trusted admission code
//! shares [`BudgetScope`] and retains each reservation until it can settle usage.
//! These handles do not interrupt work or establish durable recovery by themselves.

use futures::task::AtomicWaker;
use std::collections::BTreeMap;
use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::task::Poll;
use std::time::{Duration, Instant};

use jingwei_core::TurnId;
pub use jingwei_core::budget::*;

mod checkpoint;
pub use checkpoint::*;

/// A trusted, nonblocking monotonic time source, sampled under the ledger lock.
pub trait BudgetClock: Send + Sync {
    fn now(&self) -> Duration;
}

pub struct MonotonicBudgetClock(Instant);

impl Default for MonotonicBudgetClock {
    fn default() -> Self {
        Self(Instant::now())
    }
}

impl BudgetClock for MonotonicBudgetClock {
    fn now(&self) -> Duration {
        self.0.elapsed()
    }
}

#[derive(Clone, Debug, thiserror::Error, Eq, PartialEq)]
pub enum BudgetError {
    #[error("this ledger has been sealed for checkpoint transfer")]
    CheckpointSealed,
    #[error("task, session and agent identity must be nonempty")]
    InvalidIdentity,
    #[error("budget binding identity does not match the request or recorded event")]
    IdentityMismatch,
    #[error("a task run is already active")]
    RunActive,
    #[error("this run is closed")]
    RunClosed,
    #[error("budget stopped: {0:?}")]
    Stopped(BudgetStopReason),
    #[error("a reservation must consume at least one attempt")]
    EmptyRequest,
    #[error("missing verified token bound: {0:?}")]
    UnverifiedTokenBound(BudgetResource),
    #[error("an unknown token estimate cannot be zero: {0:?}")]
    ZeroTokenEstimate(BudgetResource),
    #[error("reservation has already started")]
    AlreadyStarted,
    #[error("reservation has not started")]
    NotStarted,
    #[error("reservation is unavailable")]
    ReservationMissing,
    #[error("pending reservations must be settled before finishing")]
    PendingReservations,
    #[error("budget lock was poisoned")]
    Poisoned,
}

#[derive(Clone)]
pub struct TaskBudget(Arc<Inner>);

struct Inner {
    identity: BudgetIdentity,
    limits: BudgetLimits,
    token_mode: TokenBudgetMode,
    clock: Arc<dyn BudgetClock>,
    state: Mutex<State>,
    waiters: Mutex<Vec<Weak<AtomicWaker>>>,
}

#[derive(Default)]
struct State {
    sealed: bool,
    recovery_frozen: bool,
    checkpoint_revision: u64,
    charged: BudgetAmounts,
    reserved: BudgetAmounts,
    usage: BudgetUsageReport,
    active_time: Duration,
    cleanup_time: Duration,
    stop: Option<BudgetStopReason>,
    run: Option<RunState>,
    pending: BTreeMap<u64, BudgetPendingReport>,
    next_id: u64,
    last_clock: Duration,
}

struct RunState {
    id: u64,
    report: BudgetRunReport,
    cleaning: bool,
}

/// Host-owned run lease. Dropping an unfinished lease freezes the task.
#[must_use = "retain the run until all reservations are settled and finish succeeds"]
pub struct BudgetRun {
    inner: Arc<Inner>,
    id: u64,
    finished: bool,
}

/// Consumption authority for one run. Clones share the same atomic ledger.
#[derive(Clone)]
pub struct BudgetScope {
    inner: Arc<Inner>,
    run_id: u64,
}

/// Non-cloneable settlement ownership. Drop preserves unresolved reservations.
#[must_use = "settle or explicitly cancel before start; drop freezes the task"]
pub struct BudgetReservation {
    inner: Arc<Inner>,
    run_id: u64,
    id: u64,
    resolved: bool,
}

impl TaskBudget {
    pub fn new(
        identity: BudgetIdentity,
        host_limits: BudgetLimits,
        task_limits: BudgetLimits,
        token_mode: TokenBudgetMode,
        clock: Arc<dyn BudgetClock>,
    ) -> Result<Self, BudgetError> {
        if identity.task_id.as_str().trim().is_empty()
            || identity.session_id.as_str().trim().is_empty()
            || identity.agent_key.trim().is_empty()
        {
            return Err(BudgetError::InvalidIdentity);
        }
        let last_clock = clock.now();
        Ok(Self(Arc::new(Inner {
            identity,
            limits: host_limits.tightened_by(task_limits),
            token_mode,
            clock,
            waiters: Mutex::new(Vec::new()),
            state: Mutex::new(State {
                last_clock,
                ..State::default()
            }),
        })))
    }

    pub fn begin_run(
        &self,
        turn_id: TurnId,
        limits: BudgetLimits,
    ) -> Result<BudgetRun, BudgetError> {
        self.begin_run_inner(Some(turn_id), limits)
    }

    /// Begin timing before Session admission has assigned a canonical TurnId.
    pub fn begin_admission(&self, limits: BudgetLimits) -> Result<BudgetRun, BudgetError> {
        self.begin_run_inner(None, limits)
    }

    pub fn identity(&self) -> &BudgetIdentity {
        &self.0.identity
    }

    fn begin_run_inner(
        &self,
        turn_id: Option<TurnId>,
        limits: BudgetLimits,
    ) -> Result<BudgetRun, BudgetError> {
        let mut state = self.0.lock()?;
        state.check_writable()?;
        self.0.tick(&mut state);
        if state.run.is_some() {
            return Err(BudgetError::RunActive);
        }
        state.check_stop()?;
        let id = state.allocate_id()?;
        state.run = Some(RunState {
            id,
            cleaning: false,
            report: BudgetRunReport {
                turn_id,
                metrics: BudgetExecutionMetrics::default(),
                limits: self.0.limits.tightened_by(limits),
                charged: BudgetAmounts::default(),
                reserved: BudgetAmounts::default(),
                active_time: Duration::ZERO,
                cleanup_time: Duration::ZERO,
                open: true,
                stop: None,
            },
        });
        self.0.tick(&mut state);
        Ok(BudgetRun {
            inner: self.0.clone(),
            id,
            finished: false,
        })
    }

    pub fn report(&self) -> Result<BudgetReport, BudgetError> {
        self.0.report()
    }
}

impl BudgetRun {
    /// Attach the real Session identity exactly once, before controlled work.
    pub fn bind_turn(&mut self, turn_id: TurnId) -> Result<(), BudgetError> {
        let mut state = self.inner.lock()?;
        self.inner.tick(&mut state);
        state.check_run(self.id)?;
        let run = state.run.as_mut().ok_or(BudgetError::RunClosed)?;
        if run.report.turn_id.is_some() {
            return Err(BudgetError::IdentityMismatch);
        }
        run.report.turn_id = Some(turn_id);
        Ok(())
    }

    pub fn scope(&self) -> BudgetScope {
        BudgetScope {
            inner: self.inner.clone(),
            run_id: self.id,
        }
    }

    /// Close consumption and sample a report while retaining the Task lease through
    /// report/terminal persistence. Call finish after the persistence barrier.
    pub fn prepare_report(&mut self) -> Result<BudgetReport, BudgetError> {
        let mut state = self.inner.lock()?;
        self.inner.tick(&mut state);
        state.check_run(self.id)?;
        if !state.pending.is_empty() {
            return Err(BudgetError::PendingReservations);
        }
        state
            .run
            .as_mut()
            .ok_or(BudgetError::RunClosed)?
            .report
            .open = false;
        Ok(self.inner.snapshot(&state))
    }

    /// Retry after settling pending reservations. A stopped run can still finish.
    pub fn finish(&mut self) -> Result<BudgetReport, BudgetError> {
        let mut state = self.inner.lock()?;
        state.check_writable()?;
        self.inner.tick(&mut state);
        if state.run.as_ref().is_none_or(|run| run.id != self.id) {
            return Err(BudgetError::RunClosed);
        }
        if !state.pending.is_empty() {
            return Err(BudgetError::PendingReservations);
        }
        state
            .run
            .as_mut()
            .ok_or(BudgetError::RunClosed)?
            .report
            .open = false;
        let report = self.inner.snapshot(&state);
        state.run = None;
        self.finished = true;
        Ok(report)
    }
}

impl Drop for BudgetRun {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        if let Ok(mut state) = self.inner.lock() {
            if state.check_writable().is_err() {
                return;
            }
            self.inner.tick(&mut state);
            if let Some(run) = state.run.as_mut().filter(|run| run.id == self.id) {
                run.report.open = false;
                state.stop.get_or_insert(BudgetStopReason::RunAbandoned);
            }
        }
    }
}

impl BudgetScope {
    pub fn identity(&self) -> &BudgetIdentity {
        &self.inner.identity
    }

    pub fn turn_id(&self) -> Result<Option<TurnId>, BudgetError> {
        let state = self.inner.lock()?;
        state.check_run(self.run_id)?;
        Ok(state
            .run
            .as_ref()
            .ok_or(BudgetError::RunClosed)?
            .report
            .turn_id
            .clone())
    }

    pub fn check_active(&self) -> Result<(), BudgetError> {
        let mut state = self.inner.lock()?;
        self.inner.tick(&mut state);
        state.check_run(self.run_id)?;
        state.check_stop()
    }

    pub fn remaining_time(&self) -> Result<Duration, BudgetError> {
        let mut state = self.inner.lock()?;
        self.inner.tick(&mut state);
        state.check_run(self.run_id)?;
        state.check_stop()?;
        let run = &state.run.as_ref().ok_or(BudgetError::RunClosed)?.report;
        Ok((self.inner.limits.active_time - state.active_time)
            .min(run.limits.active_time - run.active_time))
    }

    /// Wake on explicit/accounting stops. Executors also arm remaining_time timers.
    pub async fn stopped(&self) -> BudgetError {
        let waiter = Arc::new(AtomicWaker::new());
        {
            let mut waiters = self
                .inner
                .waiters
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            waiters.retain(|entry| entry.strong_count() > 0);
            waiters.push(Arc::downgrade(&waiter));
        }
        futures::future::poll_fn(|cx| {
            waiter.register(cx.waker());
            match self.check_active() {
                Ok(()) => Poll::Pending,
                Err(error) => Poll::Ready(error),
            }
        })
        .await
    }

    /// Executor timer evidence. Does not erase any consumed or pending amounts.
    pub fn expire(&self) -> Result<(), BudgetError> {
        let mut state = self.inner.lock()?;
        self.inner.tick(&mut state);
        state.check_run(self.run_id)?;
        if state.check_stop().is_ok() {
            let task_remaining = self.inner.limits.active_time - state.active_time;
            let run = state.run.as_mut().ok_or(BudgetError::RunClosed)?;
            if task_remaining <= run.report.limits.active_time - run.report.active_time {
                state.stop = Some(BudgetStopReason::ActiveTime);
            } else {
                run.report.stop = Some(BudgetStopReason::ActiveTime);
            }
        }
        Ok(())
    }

    pub fn stop_with(&self, reason: BudgetStopReason) -> Result<(), BudgetError> {
        let mut state = self.inner.lock()?;
        self.inner.tick(&mut state);
        state.check_run(self.run_id)?;
        state
            .run
            .as_mut()
            .ok_or(BudgetError::RunClosed)?
            .report
            .stop
            .get_or_insert(reason);
        Ok(())
    }

    /// Enter cleanup after admission has been closed by the owning runtime.
    pub fn begin_cleanup(&self) -> Result<(), BudgetError> {
        let mut state = self.inner.lock()?;
        self.inner.tick(&mut state);
        state.check_run(self.run_id)?;
        state.run.as_mut().ok_or(BudgetError::RunClosed)?.cleaning = true;
        Ok(())
    }

    pub fn record_session_wait(&self, elapsed: Duration) {
        self.metrics(|m| m.session_wait = elapsed);
    }
    pub fn record_model_timing(&self, queue: Duration, execution: Duration) {
        self.metrics(|m| {
            m.model_queue = m.model_queue.saturating_add(queue);
            m.model_execution = m.model_execution.saturating_add(execution);
        });
    }
    pub fn record_tool_time(&self, execution: Duration) {
        self.metrics(|m| m.tool_execution = m.tool_execution.saturating_add(execution));
    }
    pub fn record_evidence(&self, kind: BudgetEventKind, confirmed: bool) {
        self.metrics(|m| {
            let counts = if confirmed {
                &mut m.confirmed
            } else {
                &mut m.unconfirmed
            };
            let count = match kind {
                BudgetEventKind::ModelRequest => &mut counts.model_requests,
                BudgetEventKind::ModelResult => &mut counts.model_results,
                BudgetEventKind::ToolCall => &mut counts.tool_calls,
                BudgetEventKind::ToolResult => &mut counts.tool_results,
            };
            *count = count.saturating_add(1);
        });
    }
    fn metrics(&self, update: impl FnOnce(&mut BudgetExecutionMetrics)) {
        if let Ok(mut state) = self.inner.lock()
            && state.check_writable().is_ok()
            && let Some(run) = state.run.as_mut().filter(|run| run.id == self.run_id)
        {
            update(&mut run.report.metrics);
        }
    }

    pub fn report(&self) -> Result<BudgetReport, BudgetError> {
        self.inner.report()
    }

    /// Reserve all dimensions atomically. Admission counts are never refunded.
    /// Token evidence must be established by trusted code before this call.
    pub fn reserve(&self, request: BudgetRequest) -> Result<BudgetReservation, BudgetError> {
        let mut state = self.inner.lock()?;
        self.inner.tick(&mut state);
        state.check_run(self.run_id)?;
        state.check_stop()?;
        validate_request(request, self.inner.token_mode)?;
        // No counters change until every resource and the reservation ID fit.
        for resource in BudgetResource::ALL {
            let amount = request.amounts.get(resource);
            if !fits(
                state.charged.get(resource),
                state.reserved.get(resource),
                amount,
                self.inner.limits.resources.get(resource),
            ) {
                let reason = BudgetStopReason::ResourceLimit(resource);
                state.stop = Some(reason);
                return Err(BudgetError::Stopped(reason));
            }
            let run = &state.run.as_ref().ok_or(BudgetError::RunClosed)?.report;
            if !fits(
                run.charged.get(resource),
                run.reserved.get(resource),
                amount,
                run.limits.resources.get(resource),
            ) {
                let reason = BudgetStopReason::ResourceLimit(resource);
                state
                    .run
                    .as_mut()
                    .ok_or(BudgetError::RunClosed)?
                    .report
                    .stop = Some(reason);
                return Err(BudgetError::Stopped(reason));
            }
        }
        let id = state.allocate_id()?;
        for resource in BudgetResource::ALL {
            let amount = request.amounts.get(resource);
            // Checked by fits above; attempts are charged at acceptance, variables reserved.
            let target = if resource.is_attempt() {
                &mut state.charged
            } else {
                &mut state.reserved
            };
            target.set(resource, target.get(resource) + amount);
            let run = &mut state.run.as_mut().ok_or(BudgetError::RunClosed)?.report;
            let target = if resource.is_attempt() {
                &mut run.charged
            } else {
                &mut run.reserved
            };
            target.set(resource, target.get(resource) + amount);
        }
        state.pending.insert(
            id,
            BudgetPendingReport {
                id,
                request,
                started: false,
                abandoned: false,
                failed_usage: None,
            },
        );
        Ok(BudgetReservation {
            inner: self.inner.clone(),
            run_id: self.run_id,
            id,
            resolved: false,
        })
    }
}

impl BudgetReservation {
    /// Call immediately before entering work that may consume variable resources.
    pub fn mark_started(&mut self) -> Result<(), BudgetError> {
        let mut state = self.inner.lock()?;
        self.inner.tick(&mut state);
        state.check_run(self.run_id)?;
        state.check_stop()?;
        let pending = state
            .pending
            .get_mut(&self.id)
            .ok_or(BudgetError::ReservationMissing)?;
        if pending.started {
            return Err(BudgetError::AlreadyStarted);
        }
        pending.started = true;
        Ok(())
    }

    /// Only the trusted admission owner can assert that execution has not begun.
    /// Attempt charges remain even when variable reservations are released.
    pub fn cancel_before_start(mut self) -> Result<(), BudgetError> {
        let mut state = self.inner.lock()?;
        state.check_writable()?;
        self.inner.tick(&mut state);
        let pending = state
            .pending
            .get(&self.id)
            .ok_or(BudgetError::ReservationMissing)?;
        if pending.started {
            return Err(BudgetError::AlreadyStarted);
        }
        let request = pending.request;
        release(&mut state, request.amounts)?;
        state.pending.remove(&self.id);
        self.resolved = true;
        Ok(())
    }

    /// Cleanup remains possible after a budget stop or a dropped run lease.
    /// Unknown usage keeps the full reservation; actual usage is never clamped.
    pub fn settle(mut self, usage: BudgetUsage) -> Result<(), BudgetError> {
        let mut state = self.inner.lock()?;
        state.check_writable()?;
        self.inner.tick(&mut state);
        let pending = state
            .pending
            .get(&self.id)
            .ok_or(BudgetError::ReservationMissing)?;
        if !pending.started {
            return Err(BudgetError::NotStarted);
        }
        let request = pending.request;
        match settlement(&state, request, usage) {
            Ok((charged, run_charged, evidence, breach)) => {
                release(&mut state, request.amounts)?;
                state.charged = charged;
                state
                    .run
                    .as_mut()
                    .ok_or(BudgetError::RunClosed)?
                    .report
                    .charged = run_charged;
                state.usage = evidence;
                state.pending.remove(&self.id);
                self.resolved = true;
                if let Some(resource) = breach {
                    let reason = BudgetStopReason::UsageExceededReservation(resource);
                    state.stop.get_or_insert(reason);
                    return Err(BudgetError::Stopped(reason));
                }
                Ok(())
            }
            Err(error) => {
                state
                    .stop
                    .get_or_insert(BudgetStopReason::AccountingOverflow);
                if let Some(pending) = state.pending.get_mut(&self.id) {
                    pending.failed_usage = Some(usage);
                }
                Err(error)
            }
        }
    }
}

impl Drop for BudgetReservation {
    fn drop(&mut self) {
        if self.resolved {
            return;
        }
        if let Ok(mut state) = self.inner.lock() {
            if state.check_writable().is_err() {
                return;
            }
            self.inner.tick(&mut state);
            if let Some(pending) = state.pending.get_mut(&self.id) {
                pending.abandoned = true;
                state
                    .stop
                    .get_or_insert(BudgetStopReason::AbandonedReservation);
            }
        }
    }
}

impl Inner {
    fn lock(&self) -> Result<StateGuard<'_>, BudgetError> {
        Ok(StateGuard {
            inner: self,
            guard: Some(self.state.lock().map_err(|_| BudgetError::Poisoned)?),
        })
    }

    fn report(&self) -> Result<BudgetReport, BudgetError> {
        let mut state = self.lock()?;
        if state.sealed {
            return Err(BudgetError::CheckpointSealed);
        }
        self.tick(&mut state);
        Ok(self.snapshot(&state))
    }

    fn snapshot(&self, state: &State) -> BudgetReport {
        BudgetReport {
            identity: self.identity.clone(),
            limits: self.limits,
            token_mode: self.token_mode,
            charged: state.charged,
            reserved: state.reserved,
            active_time: state.active_time,
            cleanup_time: state.cleanup_time,
            stop: state.stop,
            usage: state.usage,
            run: state.run.as_ref().map(|run| run.report.clone()),
            pending: state.pending.values().cloned().collect(),
        }
    }

    // Tick never prevents settlement. Time faults stop admission and remain observable.
    fn tick(&self, state: &mut State) {
        if state.sealed || state.recovery_frozen {
            return;
        }
        let now = self.clock.now();
        let Some(elapsed) = now.checked_sub(state.last_clock) else {
            state
                .stop
                .get_or_insert(BudgetStopReason::ClockMovedBackwards);
            return;
        };
        state.last_clock = now;
        // An abandoned run cannot admit work, but its outstanding owners can
        // still settle. Measure that cleanup until the last pending item leaves.
        let has_pending = !state.pending.is_empty();
        let Some(run) = state
            .run
            .as_mut()
            .filter(|run| run.report.open || has_pending)
        else {
            return;
        };
        let cleaning = run.cleaning;
        let run = &mut run.report;
        let stopped = state.stop.is_some() || run.stop.is_some();
        let task_remaining = self.limits.active_time.saturating_sub(state.active_time);
        let run_remaining = run.limits.active_time.saturating_sub(run.active_time);
        let remaining = task_remaining.min(run_remaining);
        let active = if stopped || cleaning {
            Duration::ZERO
        } else {
            elapsed.min(remaining)
        };
        let cleanup = elapsed - active;
        // Active additions are bounded by their limits; cleanup may overflow independently.
        state.active_time += active;
        run.active_time += active;
        if !stopped && !cleaning && elapsed >= remaining {
            if task_remaining <= run_remaining {
                state.stop = Some(BudgetStopReason::ActiveTime);
            } else {
                run.stop = Some(BudgetStopReason::ActiveTime);
            }
        }
        match (
            state.cleanup_time.checked_add(cleanup),
            run.cleanup_time.checked_add(cleanup),
        ) {
            (Some(task_time), Some(run_time)) => {
                state.cleanup_time = task_time;
                run.cleanup_time = run_time;
            }
            _ => {
                state
                    .stop
                    .get_or_insert(BudgetStopReason::AccountingOverflow);
            }
        }
    }
}

// Publish stops after releasing the ledger lock; never call a waker under it.
struct StateGuard<'a> {
    inner: &'a Inner,
    guard: Option<MutexGuard<'a, State>>,
}
impl Deref for StateGuard<'_> {
    type Target = State;
    fn deref(&self) -> &State {
        self.guard.as_ref().unwrap()
    }
}
impl DerefMut for StateGuard<'_> {
    fn deref_mut(&mut self) -> &mut State {
        self.guard.as_mut().unwrap()
    }
}
impl Drop for StateGuard<'_> {
    fn drop(&mut self) {
        let notify = self.sealed
            || self.recovery_frozen
            || self.stop.is_some()
            || self
                .run
                .as_ref()
                .is_none_or(|run| run.report.stop.is_some() || !run.report.open);
        drop(self.guard.take());
        if notify {
            let waiters: Vec<_> = self
                .inner
                .waiters
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .filter_map(Weak::upgrade)
                .collect();
            for waiter in waiters {
                waiter.wake();
            }
        }
    }
}

impl State {
    fn check_writable(&self) -> Result<(), BudgetError> {
        if self.sealed {
            Err(BudgetError::CheckpointSealed)
        } else if self.recovery_frozen {
            Err(BudgetError::Stopped(BudgetStopReason::RecoveryRequired))
        } else {
            Ok(())
        }
    }

    fn check_run(&self, id: u64) -> Result<(), BudgetError> {
        self.check_writable()?;
        if self
            .run
            .as_ref()
            .is_some_and(|run| run.id == id && run.report.open)
        {
            Ok(())
        } else {
            Err(BudgetError::RunClosed)
        }
    }

    fn check_stop(&self) -> Result<(), BudgetError> {
        self.check_writable()?;
        if let Some(reason) = self
            .stop
            .or_else(|| self.run.as_ref().and_then(|run| run.report.stop))
        {
            Err(BudgetError::Stopped(reason))
        } else {
            Ok(())
        }
    }

    fn allocate_id(&mut self) -> Result<u64, BudgetError> {
        match self.next_id.checked_add(1) {
            Some(id) => {
                self.next_id = id;
                Ok(id)
            }
            None => {
                self.stop
                    .get_or_insert(BudgetStopReason::AccountingOverflow);
                Err(overflow())
            }
        }
    }
}

fn fits(charged: u64, reserved: u64, requested: u64, limit: u64) -> bool {
    charged
        .checked_add(reserved)
        .and_then(|value| value.checked_add(requested))
        .is_some_and(|value| value <= limit)
}

fn validate_request(request: BudgetRequest, mode: TokenBudgetMode) -> Result<(), BudgetError> {
    if !BudgetResource::ALL
        .into_iter()
        .any(|resource| resource.is_attempt() && request.amounts.get(resource) > 0)
    {
        return Err(BudgetError::EmptyRequest);
    }
    for (resource, evidence) in [
        (BudgetResource::InputTokens, request.input_tokens),
        (BudgetResource::OutputTokens, request.output_tokens),
    ] {
        let amount = request.amounts.get(resource);
        if (request.amounts.model_requests > 0 || amount > 0)
            && evidence != TokenBoundEvidence::VerifiedUpperBound
        {
            if mode == TokenBudgetMode::Hard {
                return Err(BudgetError::UnverifiedTokenBound(resource));
            }
            if amount == 0 {
                return Err(BudgetError::ZeroTokenEstimate(resource));
            }
        }
    }
    Ok(())
}

fn release(state: &mut State, amounts: BudgetAmounts) -> Result<(), BudgetError> {
    let run = &mut state.run.as_mut().ok_or(BudgetError::RunClosed)?.report;
    for resource in BudgetResource::ALL
        .into_iter()
        .filter(|resource| !resource.is_attempt())
    {
        // Each pending allocation was added atomically and can be resolved only once.
        state.reserved.set(
            resource,
            state.reserved.get(resource) - amounts.get(resource),
        );
        run.reserved
            .set(resource, run.reserved.get(resource) - amounts.get(resource));
    }
    Ok(())
}

type Settlement = (
    BudgetAmounts,
    BudgetAmounts,
    BudgetUsageReport,
    Option<BudgetResource>,
);

fn settlement(
    state: &State,
    request: BudgetRequest,
    usage: BudgetUsage,
) -> Result<Settlement, BudgetError> {
    let mut charged = state.charged;
    let mut run_charged = state
        .run
        .as_ref()
        .ok_or(BudgetError::RunClosed)?
        .report
        .charged;
    let mut evidence = state.usage;
    let mut breach = None;
    for (resource, value, totals, applicable) in [
        (
            BudgetResource::InputTokens,
            usage.input_tokens,
            &mut evidence.input_tokens,
            request.amounts.model_requests > 0,
        ),
        (
            BudgetResource::OutputTokens,
            usage.output_tokens,
            &mut evidence.output_tokens,
            request.amounts.model_requests > 0,
        ),
        (
            BudgetResource::ToolOutputBytes,
            usage.tool_output_bytes,
            &mut evidence.tool_output_bytes,
            request.amounts.tool_calls > 0,
        ),
    ] {
        let reserved = request.amounts.get(resource);
        let debit = match value {
            UsageValue::Actual(amount) => {
                totals.actual = totals.actual.checked_add(amount).ok_or_else(overflow)?;
                amount
            }
            UsageValue::Estimated(amount) => {
                totals.estimated = totals.estimated.checked_add(amount).ok_or_else(overflow)?;
                amount.max(reserved)
            }
            UsageValue::Unknown => {
                if applicable || reserved > 0 {
                    totals.unknown = totals.unknown.checked_add(1).ok_or_else(overflow)?;
                }
                reserved
            }
        };
        if debit > reserved {
            breach.get_or_insert(resource);
        }
        charged.set(
            resource,
            charged
                .get(resource)
                .checked_add(debit)
                .ok_or_else(overflow)?,
        );
        run_charged.set(
            resource,
            run_charged
                .get(resource)
                .checked_add(debit)
                .ok_or_else(overflow)?,
        );
    }
    Ok((charged, run_charged, evidence, breach))
}

fn overflow() -> BudgetError {
    BudgetError::Stopped(BudgetStopReason::AccountingOverflow)
}
