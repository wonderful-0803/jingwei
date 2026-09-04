//! Independent accounting contract tests. No model, runtime, IO, or real-time sleeps.

use std::sync::{Arc, Barrier, Mutex};
use std::thread;
use std::time::Duration;

use jingwei::budget::*;
use jingwei::id::{SessionId, TaskId, TurnId};

#[derive(Default)]
struct FakeClock(Mutex<Duration>);

impl FakeClock {
    fn set(&self, now: Duration) {
        *self.0.lock().unwrap() = now;
    }
}

impl BudgetClock for FakeClock {
    fn now(&self) -> Duration {
        *self.0.lock().unwrap()
    }
}

fn identity() -> BudgetIdentity {
    BudgetIdentity {
        task_id: TaskId::from("private-budget-task"),
        session_id: SessionId::from("private-budget-session"),
        agent_key: "private-agent".into(),
    }
}

fn amounts(limit: u64) -> BudgetAmounts {
    BudgetAmounts {
        steps: limit,
        model_requests: limit,
        tool_calls: limit,
        corrections: limit,
        input_tokens: limit,
        output_tokens: limit,
        tool_output_bytes: limit,
    }
}

fn limits(limit: u64, seconds: u64) -> BudgetLimits {
    BudgetLimits {
        resources: amounts(limit),
        active_time: Duration::from_secs(seconds),
    }
}

fn task(clock: &Arc<FakeClock>, limit: BudgetLimits, mode: TokenBudgetMode) -> TaskBudget {
    TaskBudget::new(identity(), limit, limit, mode, clock.clone()).unwrap()
}

fn request(value: BudgetAmounts) -> BudgetRequest {
    BudgetRequest::new(value).with_token_evidence(
        TokenBoundEvidence::VerifiedUpperBound,
        TokenBoundEvidence::VerifiedUpperBound,
    )
}

fn model_request(input: u64, output: u64) -> BudgetRequest {
    request(BudgetAmounts {
        steps: 1,
        model_requests: 1,
        input_tokens: input,
        output_tokens: output,
        ..Default::default()
    })
}

fn assert_zero(value: &BudgetAmounts) {
    assert_eq!(value.steps, 0);
    assert_eq!(value.model_requests, 0);
    assert_eq!(value.tool_calls, 0);
    assert_eq!(value.corrections, 0);
    assert_eq!(value.input_tokens, 0);
    assert_eq!(value.output_tokens, 0);
    assert_eq!(value.tool_output_bytes, 0);
}

#[test]
fn a_failed_multi_resource_reservation_has_no_partial_accounting() {
    let clock = Arc::new(FakeClock::default());
    let budget = task(&clock, limits(10, 60), TokenBudgetMode::Hard);
    let mut run = budget.begin_run(TurnId::new(), limits(10, 60)).unwrap();
    let result = run.scope().reserve(request(BudgetAmounts {
        steps: 1,
        model_requests: 1,
        tool_calls: 1,
        corrections: 1,
        input_tokens: 2,
        output_tokens: 3,
        tool_output_bytes: 11,
    }));
    assert!(result.is_err());
    let report = budget.report().unwrap();
    assert_zero(&report.charged);
    assert_zero(&report.reserved);
    assert!(report.pending.is_empty());
    run.finish().unwrap();
}

#[test]
fn concurrent_requests_cannot_all_reserve_the_last_allowance() {
    let clock = Arc::new(FakeClock::default());
    let budget = task(&clock, limits(1, 60), TokenBudgetMode::Hard);
    let mut run = budget.begin_run(TurnId::new(), limits(1, 60)).unwrap();
    let barrier = Arc::new(Barrier::new(16));
    let mut workers = Vec::new();
    for _ in 0..16 {
        let scope = run.scope();
        let barrier = barrier.clone();
        workers.push(thread::spawn(move || {
            barrier.wait();
            match scope.reserve(request(amounts(1))) {
                Ok(reservation) => {
                    reservation.cancel_before_start().unwrap();
                    true
                }
                Err(_) => false,
            }
        }));
    }
    let accepted = workers
        .into_iter()
        .map(|worker| usize::from(worker.join().unwrap()))
        .sum::<usize>();
    assert_eq!(accepted, 1);
    let report = budget.report().unwrap();
    assert_eq!(report.charged.steps, 1);
    assert_eq!(report.charged.model_requests, 1);
    assert_eq!(report.charged.tool_calls, 1);
    assert_eq!(report.charged.corrections, 1);
    assert_eq!(report.charged.input_tokens, 0);
    assert_eq!(report.charged.output_tokens, 0);
    assert_eq!(report.charged.tool_output_bytes, 0);
    assert_zero(&report.reserved);
    assert!(report.pending.is_empty());
    run.finish().unwrap();
}

#[test]
fn cancellation_before_start_releases_variable_reservations_but_not_attempts() {
    let clock = Arc::new(FakeClock::default());
    let budget = task(&clock, limits(100, 60), TokenBudgetMode::Hard);
    let mut run = budget.begin_run(TurnId::new(), limits(100, 60)).unwrap();
    let reservation = run.scope().reserve(request(amounts(1))).unwrap();
    reservation.cancel_before_start().unwrap();
    let report = budget.report().unwrap();
    assert_eq!(report.charged.steps, 1);
    assert_eq!(report.charged.model_requests, 1);
    assert_eq!(report.charged.tool_calls, 1);
    assert_eq!(report.charged.corrections, 1);
    assert_eq!(report.charged.input_tokens, 0);
    assert_eq!(report.charged.output_tokens, 0);
    assert_eq!(report.charged.tool_output_bytes, 0);
    assert_zero(&report.reserved);
    assert!(report.pending.is_empty());
    run.finish().unwrap();
}

#[test]
fn cancellation_after_start_cannot_refund_an_accepted_request() {
    let clock = Arc::new(FakeClock::default());
    let budget = task(&clock, limits(100, 60), TokenBudgetMode::Hard);
    let run = budget.begin_run(TurnId::new(), limits(100, 60)).unwrap();
    let mut reservation = run.scope().reserve(model_request(10, 20)).unwrap();
    reservation.mark_started().unwrap();
    assert!(reservation.cancel_before_start().is_err());
    let report = budget.report().unwrap();
    assert_eq!(report.charged.model_requests, 1);
    assert_eq!(report.reserved.input_tokens, 10);
    assert_eq!(report.reserved.output_tokens, 20);
    assert_eq!(report.pending.len(), 1);
}

#[test]
fn settlement_requires_an_explicit_start() {
    let clock = Arc::new(FakeClock::default());
    let budget = task(&clock, limits(100, 60), TokenBudgetMode::Hard);
    let run = budget.begin_run(TurnId::new(), limits(100, 60)).unwrap();
    let reservation = run.scope().reserve(model_request(10, 20)).unwrap();
    assert!(reservation.settle(BudgetUsage::actual(0, 0, 0)).is_err());
    let report = budget.report().unwrap();
    assert_eq!(report.charged.model_requests, 1);
    assert_eq!(report.reserved.input_tokens, 10);
    assert_eq!(report.reserved.output_tokens, 20);
}

#[test]
fn actual_usage_releases_unused_variable_allowance_and_keeps_attempts() {
    let clock = Arc::new(FakeClock::default());
    let budget = task(&clock, limits(100, 60), TokenBudgetMode::Hard);
    let mut run = budget.begin_run(TurnId::new(), limits(100, 60)).unwrap();
    let mut reservation = run
        .scope()
        .reserve(request(BudgetAmounts {
            steps: 1,
            model_requests: 1,
            tool_calls: 1,
            corrections: 1,
            input_tokens: 10,
            output_tokens: 20,
            tool_output_bytes: 30,
        }))
        .unwrap();
    reservation.mark_started().unwrap();
    reservation.settle(BudgetUsage::actual(3, 4, 5)).unwrap();
    let report = budget.report().unwrap();
    assert_eq!(report.charged.model_requests, 1);
    assert_eq!(report.charged.tool_calls, 1);
    assert_eq!(report.charged.corrections, 1);
    assert_eq!(report.charged.input_tokens, 3);
    assert_eq!(report.charged.output_tokens, 4);
    assert_eq!(report.charged.tool_output_bytes, 5);
    assert_eq!(report.usage.input_tokens.actual, 3);
    assert_eq!(report.usage.output_tokens.actual, 4);
    assert_eq!(report.usage.tool_output_bytes.actual, 5);
    assert_zero(&report.reserved);
    assert!(report.pending.is_empty());
    run.finish().unwrap();
}

#[test]
fn unknown_usage_keeps_the_charge_and_records_unknown_observations() {
    let clock = Arc::new(FakeClock::default());
    let budget = task(&clock, limits(100, 60), TokenBudgetMode::Hard);
    let mut run = budget.begin_run(TurnId::new(), limits(100, 60)).unwrap();
    let mut reservation = run
        .scope()
        .reserve(request(BudgetAmounts {
            model_requests: 1,
            tool_calls: 1,
            input_tokens: 10,
            output_tokens: 20,
            tool_output_bytes: 30,
            ..Default::default()
        }))
        .unwrap();
    reservation.mark_started().unwrap();
    reservation
        .settle(BudgetUsage {
            input_tokens: UsageValue::Unknown,
            output_tokens: UsageValue::Unknown,
            tool_output_bytes: UsageValue::Unknown,
        })
        .unwrap();
    let report = budget.report().unwrap();
    assert_eq!(report.charged.input_tokens, 10);
    assert_eq!(report.charged.output_tokens, 20);
    assert_eq!(report.charged.tool_output_bytes, 30);
    assert_eq!(report.usage.input_tokens.actual, 0);
    assert_eq!(report.usage.input_tokens.estimated, 0);
    assert_eq!(report.usage.input_tokens.unknown, 1);
    assert_eq!(report.usage.output_tokens.unknown, 1);
    assert_eq!(report.usage.tool_output_bytes.unknown, 1);
    assert_zero(&report.reserved);
    assert!(report.pending.is_empty());
    run.finish().unwrap();
}

#[test]
fn smaller_estimates_do_not_refund_a_larger_reservation() {
    let clock = Arc::new(FakeClock::default());
    let budget = task(&clock, limits(100, 60), TokenBudgetMode::Soft);
    let mut run = budget.begin_run(TurnId::new(), limits(100, 60)).unwrap();
    let mut reservation = run
        .scope()
        .reserve(BudgetRequest::new(BudgetAmounts {
            model_requests: 1,
            tool_calls: 1,
            input_tokens: 10,
            output_tokens: 20,
            tool_output_bytes: 30,
            ..Default::default()
        }))
        .unwrap();
    reservation.mark_started().unwrap();
    reservation
        .settle(BudgetUsage {
            input_tokens: UsageValue::Estimated(3),
            output_tokens: UsageValue::Estimated(4),
            tool_output_bytes: UsageValue::Estimated(5),
        })
        .unwrap();
    let report = budget.report().unwrap();
    assert_eq!(report.charged.input_tokens, 10);
    assert_eq!(report.charged.output_tokens, 20);
    assert_eq!(report.charged.tool_output_bytes, 30);
    assert_eq!(report.usage.input_tokens.actual, 0);
    assert_eq!(report.usage.input_tokens.estimated, 3);
    assert_eq!(report.usage.output_tokens.estimated, 4);
    assert_eq!(report.usage.tool_output_bytes.estimated, 5);
    assert_zero(&report.reserved);
    run.finish().unwrap();
}

#[test]
fn estimates_above_the_reservation_are_not_reported_as_actual_usage() {
    let clock = Arc::new(FakeClock::default());
    let budget = task(&clock, limits(100, 60), TokenBudgetMode::Soft);
    let mut run = budget.begin_run(TurnId::new(), limits(100, 60)).unwrap();
    let mut reservation = run.scope().reserve(model_request(10, 20)).unwrap();
    reservation.mark_started().unwrap();
    assert!(
        reservation
            .settle(BudgetUsage {
                input_tokens: UsageValue::Estimated(12),
                output_tokens: UsageValue::Estimated(4),
                tool_output_bytes: UsageValue::Actual(0),
            })
            .is_err()
    );
    let report = budget.report().unwrap();
    assert_eq!(report.charged.input_tokens, 12);
    assert_eq!(report.charged.output_tokens, 20);
    assert_eq!(report.usage.input_tokens.actual, 0);
    assert_eq!(report.usage.input_tokens.estimated, 12);
    assert_zero(&report.reserved);
    run.finish().unwrap();
}

#[test]
fn usage_above_the_reservation_is_preserved_and_stops_new_admission() {
    let clock = Arc::new(FakeClock::default());
    let budget = task(&clock, limits(100, 60), TokenBudgetMode::Hard);
    let mut run = budget.begin_run(TurnId::new(), limits(100, 60)).unwrap();
    let scope = run.scope();
    let mut reservation = scope.reserve(model_request(10, 20)).unwrap();
    reservation.mark_started().unwrap();
    assert!(reservation.settle(BudgetUsage::actual(11, 4, 0)).is_err());
    let report = budget.report().unwrap();
    assert_eq!(report.charged.input_tokens, 11);
    assert_eq!(report.usage.input_tokens.actual, 11);
    assert!(matches!(
        report.stop,
        Some(BudgetStopReason::UsageExceededReservation(
            BudgetResource::InputTokens
        ))
    ));
    assert_zero(&report.reserved);
    assert!(report.pending.is_empty());
    assert!(scope.reserve(model_request(1, 1)).is_err());
    run.finish().unwrap();
}

#[test]
fn abandoning_a_reservation_preserves_pending_charge_and_freezes_admission() {
    let clock = Arc::new(FakeClock::default());
    let budget = task(&clock, limits(100, 60), TokenBudgetMode::Hard);
    let mut run = budget.begin_run(TurnId::new(), limits(100, 60)).unwrap();
    let scope = run.scope();
    let reservation = scope.reserve(model_request(10, 20)).unwrap();
    drop(reservation);
    let report = budget.report().unwrap();
    assert_eq!(report.charged.model_requests, 1);
    assert_eq!(report.reserved.input_tokens, 10);
    assert_eq!(report.reserved.output_tokens, 20);
    assert_eq!(report.pending.len(), 1);
    assert!(matches!(
        report.stop,
        Some(BudgetStopReason::AbandonedReservation)
    ));
    assert!(scope.reserve(model_request(1, 1)).is_err());
    assert!(run.finish().is_err());
}

#[test]
fn pending_work_blocks_finish_but_can_be_settled_before_retrying_finish() {
    let clock = Arc::new(FakeClock::default());
    let budget = task(&clock, limits(100, 60), TokenBudgetMode::Hard);
    let mut run = budget.begin_run(TurnId::new(), limits(100, 60)).unwrap();
    let mut reservation = run.scope().reserve(model_request(10, 20)).unwrap();
    reservation.mark_started().unwrap();
    assert!(run.finish().is_err());
    reservation.settle(BudgetUsage::actual(3, 4, 0)).unwrap();
    let report = run.finish().unwrap();
    assert!(report.pending.is_empty());
    assert!(!report.run.unwrap().open);
    assert!(budget.report().unwrap().run.is_none());
}

#[test]
fn a_task_has_one_active_run_and_finished_scopes_cannot_be_reused() {
    let clock = Arc::new(FakeClock::default());
    let budget = task(&clock, limits(100, 60), TokenBudgetMode::Hard);
    let mut first = budget.begin_run(TurnId::new(), limits(100, 60)).unwrap();
    let expired_scope = first.scope();
    assert!(budget.begin_run(TurnId::new(), limits(100, 60)).is_err());
    first.finish().unwrap();
    let mut second = budget.begin_run(TurnId::new(), limits(100, 60)).unwrap();
    assert!(expired_scope.reserve(model_request(1, 1)).is_err());
    let mut accepted = second.scope().reserve(model_request(1, 1)).unwrap();
    accepted.mark_started().unwrap();
    accepted.settle(BudgetUsage::actual(1, 1, 0)).unwrap();
    assert_eq!(budget.report().unwrap().charged.model_requests, 1);
    second.finish().unwrap();
}

#[test]
fn dropping_a_run_does_not_leave_a_reusable_scope_or_reset_the_task() {
    let clock = Arc::new(FakeClock::default());
    let budget = task(&clock, limits(100, 60), TokenBudgetMode::Hard);
    let run = budget.begin_run(TurnId::new(), limits(100, 60)).unwrap();
    let scope = run.scope();
    drop(run);
    assert!(scope.reserve(model_request(1, 1)).is_err());
    assert!(budget.begin_run(TurnId::new(), limits(100, 60)).is_err());
    let report = budget.report().unwrap();
    assert!(matches!(report.stop, Some(BudgetStopReason::RunAbandoned)));
}

#[test]
fn abandoned_run_cleanup_is_measured_until_its_last_pending_work_settles() {
    let clock = Arc::new(FakeClock::default());
    let budget = task(&clock, limits(100, 60), TokenBudgetMode::Hard);
    let run = budget.begin_run(TurnId::new(), limits(100, 60)).unwrap();
    let mut reservation = run.scope().reserve(model_request(10, 20)).unwrap();
    reservation.mark_started().unwrap();
    clock.set(Duration::from_secs(1));
    drop(run);
    clock.set(Duration::from_secs(11));
    reservation.settle(BudgetUsage::actual(3, 4, 0)).unwrap();
    clock.set(Duration::from_secs(21));
    let report = budget.report().unwrap();
    assert_eq!(report.active_time, Duration::from_secs(1));
    assert_eq!(report.cleanup_time, Duration::from_secs(10));
    assert_eq!(report.charged.model_requests, 1);
    assert!(report.pending.is_empty());
    let run_report = report.run.unwrap();
    assert_eq!(run_report.active_time, Duration::from_secs(1));
    assert_eq!(run_report.cleanup_time, Duration::from_secs(10));
    assert!(!run_report.open);
    assert!(matches!(report.stop, Some(BudgetStopReason::RunAbandoned)));
}

#[test]
fn host_task_and_run_limits_are_combined_without_enlarging_any_dimension() {
    let clock = Arc::new(FakeClock::default());
    let mut host = limits(100, 60);
    host.resources.model_requests = 2;
    let mut task_limits = limits(100, 60);
    task_limits.resources.tool_calls = 2;
    let budget =
        TaskBudget::new(identity(), host, task_limits, TokenBudgetMode::Hard, clock).unwrap();
    let mut run_limits = limits(1_000, 600);
    run_limits.resources.corrections = 1;
    let mut run = budget.begin_run(TurnId::new(), run_limits).unwrap();
    let mut reservation = run.scope().reserve(request(amounts(1))).unwrap();
    reservation.mark_started().unwrap();
    reservation.settle(BudgetUsage::actual(1, 1, 1)).unwrap();
    assert!(
        run.scope()
            .reserve(request(BudgetAmounts {
                corrections: 1,
                ..Default::default()
            }))
            .is_err()
    );
    let report = budget.report().unwrap();
    assert_eq!(report.limits.resources.model_requests, 2);
    assert_eq!(report.limits.resources.tool_calls, 2);
    assert_eq!(report.charged.corrections, 1);
    run.finish().unwrap();
}

#[test]
fn repeated_runs_keep_consumed_allowance_and_exclude_idle_time() {
    let clock = Arc::new(FakeClock::default());
    clock.set(Duration::from_secs(10));
    let budget = task(&clock, limits(100, 60), TokenBudgetMode::Hard);
    let mut first = budget.begin_run(TurnId::new(), limits(100, 60)).unwrap();
    let mut reservation = first.scope().reserve(model_request(10, 20)).unwrap();
    reservation.mark_started().unwrap();
    reservation.settle(BudgetUsage::actual(3, 4, 0)).unwrap();
    clock.set(Duration::from_secs(13));
    assert_eq!(budget.report().unwrap().active_time, Duration::from_secs(3));
    assert_eq!(budget.report().unwrap().active_time, Duration::from_secs(3));
    assert_eq!(first.finish().unwrap().active_time, Duration::from_secs(3));
    clock.set(Duration::from_secs(1_000));
    assert_eq!(budget.report().unwrap().active_time, Duration::from_secs(3));
    let mut second = budget.begin_run(TurnId::new(), limits(100, 60)).unwrap();
    let mut reservation = second.scope().reserve(model_request(10, 20)).unwrap();
    reservation.mark_started().unwrap();
    reservation.settle(BudgetUsage::actual(2, 3, 0)).unwrap();
    clock.set(Duration::from_secs(1_004));
    let report = second.finish().unwrap();
    assert_eq!(report.active_time, Duration::from_secs(7));
    assert_eq!(report.charged.model_requests, 2);
    assert_eq!(report.charged.input_tokens, 5);
    assert_eq!(report.charged.output_tokens, 7);
}

#[test]
fn opening_a_new_run_does_not_restore_the_task_request_allowance() {
    let clock = Arc::new(FakeClock::default());
    let mut task_limits = limits(100, 60);
    task_limits.resources.model_requests = 2;
    let budget = task(&clock, task_limits, TokenBudgetMode::Hard);
    let mut first = budget.begin_run(TurnId::new(), limits(100, 60)).unwrap();
    let mut reservation = first.scope().reserve(model_request(1, 1)).unwrap();
    reservation.mark_started().unwrap();
    reservation.settle(BudgetUsage::actual(1, 1, 0)).unwrap();
    first.finish().unwrap();
    let mut second = budget.begin_run(TurnId::new(), limits(100, 60)).unwrap();
    let mut reservation = second.scope().reserve(model_request(1, 1)).unwrap();
    reservation.mark_started().unwrap();
    reservation.settle(BudgetUsage::actual(1, 1, 0)).unwrap();
    assert!(second.scope().reserve(model_request(1, 1)).is_err());
    let report = second.finish().unwrap();
    assert_eq!(report.charged.model_requests, 2);
    assert_eq!(report.charged.input_tokens, 2);
    assert_eq!(report.charged.output_tokens, 2);
}

#[test]
fn missing_usage_eventually_exhausts_reserved_tokens_instead_of_costing_zero() {
    let clock = Arc::new(FakeClock::default());
    let mut task_limits = limits(100, 60);
    task_limits.resources.output_tokens = 2;
    let budget = task(&clock, task_limits, TokenBudgetMode::Hard);
    let mut run = budget.begin_run(TurnId::new(), limits(100, 60)).unwrap();
    for _ in 0..2 {
        let mut reservation = run.scope().reserve(model_request(1, 1)).unwrap();
        reservation.mark_started().unwrap();
        reservation
            .settle(BudgetUsage {
                input_tokens: UsageValue::Unknown,
                output_tokens: UsageValue::Unknown,
                tool_output_bytes: UsageValue::Actual(0),
            })
            .unwrap();
    }
    assert!(run.scope().reserve(model_request(1, 1)).is_err());
    let report = run.finish().unwrap();
    assert_eq!(report.charged.model_requests, 2);
    assert_eq!(report.charged.output_tokens, 2);
    assert_eq!(report.usage.output_tokens.actual, 0);
    assert_eq!(report.usage.output_tokens.unknown, 2);
}

#[test]
fn reaching_the_deadline_blocks_new_work_but_allows_settlement_and_finish() {
    let clock = Arc::new(FakeClock::default());
    let budget = task(&clock, limits(100, 5), TokenBudgetMode::Hard);
    let mut run = budget.begin_run(TurnId::new(), limits(100, 60)).unwrap();
    let scope = run.scope();
    let mut reservation = scope.reserve(model_request(10, 20)).unwrap();
    reservation.mark_started().unwrap();
    clock.set(Duration::from_secs(5));
    assert!(scope.reserve(model_request(1, 1)).is_err());
    reservation.settle(BudgetUsage::actual(3, 4, 0)).unwrap();
    let report = run.finish().unwrap();
    assert_eq!(report.active_time, Duration::from_secs(5));
    assert_eq!(report.charged.model_requests, 1);
    assert!(matches!(report.stop, Some(BudgetStopReason::ActiveTime)));
    assert!(budget.begin_run(TurnId::new(), limits(100, 60)).is_err());
}

#[test]
fn a_clock_that_moves_backwards_cannot_create_more_allowance() {
    let clock = Arc::new(FakeClock::default());
    clock.set(Duration::from_secs(10));
    let budget = task(&clock, limits(100, 60), TokenBudgetMode::Hard);
    let run = budget.begin_run(TurnId::new(), limits(100, 60)).unwrap();
    clock.set(Duration::from_secs(12));
    assert_eq!(budget.report().unwrap().active_time, Duration::from_secs(2));
    clock.set(Duration::from_secs(11));
    assert!(run.scope().reserve(model_request(1, 1)).is_err());
    if let Ok(report) = budget.report() {
        assert!(matches!(
            report.stop,
            Some(BudgetStopReason::ClockMovedBackwards)
        ));
        assert_eq!(report.charged.model_requests, 0);
    }
}

#[test]
fn hard_token_budgets_require_verified_bounds_for_both_sides() {
    for (input, output) in [
        (TokenBoundEvidence::Estimate, TokenBoundEvidence::Estimate),
        (
            TokenBoundEvidence::VerifiedUpperBound,
            TokenBoundEvidence::Estimate,
        ),
        (
            TokenBoundEvidence::Estimate,
            TokenBoundEvidence::VerifiedUpperBound,
        ),
    ] {
        let clock = Arc::new(FakeClock::default());
        let budget = task(&clock, limits(100, 60), TokenBudgetMode::Hard);
        let mut run = budget.begin_run(TurnId::new(), limits(100, 60)).unwrap();
        let request = BudgetRequest::new(BudgetAmounts {
            model_requests: 1,
            input_tokens: 10,
            output_tokens: 20,
            ..Default::default()
        })
        .with_token_evidence(input, output);
        assert!(run.scope().reserve(request).is_err());
        let report = budget.report().unwrap();
        assert_zero(&report.charged);
        assert_zero(&report.reserved);
        run.finish().unwrap();
    }
}

#[test]
fn soft_estimates_must_be_nonzero_but_verified_zero_is_allowed() {
    for (input_tokens, output_tokens) in [(0, 1), (1, 0), (0, 0)] {
        let clock = Arc::new(FakeClock::default());
        let budget = task(&clock, limits(100, 60), TokenBudgetMode::Soft);
        let mut run = budget.begin_run(TurnId::new(), limits(100, 60)).unwrap();
        assert!(
            run.scope()
                .reserve(BudgetRequest::new(BudgetAmounts {
                    model_requests: 1,
                    input_tokens,
                    output_tokens,
                    ..Default::default()
                }))
                .is_err()
        );
        assert_zero(&budget.report().unwrap().charged);
        run.finish().unwrap();
    }
    let clock = Arc::new(FakeClock::default());
    let budget = task(&clock, limits(100, 60), TokenBudgetMode::Soft);
    let mut run = budget.begin_run(TurnId::new(), limits(100, 60)).unwrap();
    let mut reservation = run.scope().reserve(model_request(0, 0)).unwrap();
    reservation.mark_started().unwrap();
    reservation.settle(BudgetUsage::actual(0, 0, 0)).unwrap();
    assert_eq!(budget.report().unwrap().charged.model_requests, 1);
    run.finish().unwrap();
}

#[test]
fn arithmetic_overflow_cannot_wrap_consumption_or_partially_charge_a_request() {
    let clock = Arc::new(FakeClock::default());
    let budget = task(&clock, limits(u64::MAX, 60), TokenBudgetMode::Hard);
    let mut run = budget
        .begin_run(TurnId::new(), limits(u64::MAX, 60))
        .unwrap();
    let scope = run.scope();
    let reservation = scope
        .reserve(request(BudgetAmounts {
            steps: u64::MAX,
            ..Default::default()
        }))
        .unwrap();
    reservation.cancel_before_start().unwrap();
    assert!(
        scope
            .reserve(request(BudgetAmounts {
                steps: 1,
                tool_calls: 1,
                ..Default::default()
            }))
            .is_err()
    );
    let report = budget.report().unwrap();
    assert_eq!(report.charged.steps, u64::MAX);
    assert_eq!(report.charged.tool_calls, 0);
    assert_zero(&report.reserved);
    assert!(report.pending.is_empty());
    run.finish().unwrap();
}

#[test]
fn settlement_overflow_keeps_pending_raw_evidence_and_does_not_partially_settle() {
    let clock = Arc::new(FakeClock::default());
    let budget = task(&clock, limits(u64::MAX, 60), TokenBudgetMode::Hard);
    let mut run = budget
        .begin_run(TurnId::new(), limits(u64::MAX, 60))
        .unwrap();
    let scope = run.scope();
    let mut first = scope.reserve(model_request(u64::MAX, 0)).unwrap();
    first.mark_started().unwrap();
    first.settle(BudgetUsage::actual(u64::MAX, 0, 0)).unwrap();
    let mut second = scope.reserve(model_request(0, 2)).unwrap();
    second.mark_started().unwrap();
    let failed_usage = BudgetUsage::actual(1, 1, 0);
    assert!(second.settle(failed_usage).is_err());
    let report = budget.report().unwrap();
    assert_eq!(report.charged.model_requests, 2);
    assert_eq!(report.charged.input_tokens, u64::MAX);
    assert_eq!(report.charged.output_tokens, 0);
    assert_eq!(report.reserved.output_tokens, 2);
    assert_eq!(report.usage.input_tokens.actual, u64::MAX);
    assert_eq!(report.usage.output_tokens.actual, 0);
    assert_eq!(report.pending.len(), 1);
    assert_eq!(report.pending[0].failed_usage, Some(failed_usage));
    assert!(report.pending[0].abandoned);
    assert!(matches!(
        report.stop,
        Some(BudgetStopReason::AccountingOverflow)
    ));
    assert!(run.finish().is_err());
}

#[test]
fn cleanup_time_is_separate_from_active_time_after_the_exact_deadline() {
    let clock = Arc::new(FakeClock::default());
    let budget = task(&clock, limits(100, 5), TokenBudgetMode::Hard);
    let mut run = budget.begin_run(TurnId::new(), limits(100, 60)).unwrap();
    let mut reservation = run.scope().reserve(model_request(10, 20)).unwrap();
    reservation.mark_started().unwrap();
    clock.set(Duration::from_secs(8));
    let report = budget.report().unwrap();
    assert_eq!(report.active_time, Duration::from_secs(5));
    assert_eq!(report.cleanup_time, Duration::from_secs(3));
    reservation.settle(BudgetUsage::actual(3, 4, 0)).unwrap();
    clock.set(Duration::from_secs(10));
    let report = run.finish().unwrap();
    assert_eq!(report.active_time, Duration::from_secs(5));
    assert_eq!(report.cleanup_time, Duration::from_secs(5));
    let run_report = report.run.unwrap();
    assert_eq!(run_report.active_time, Duration::from_secs(5));
    assert_eq!(run_report.cleanup_time, Duration::from_secs(5));
    assert!(!run_report.open);
}

#[test]
fn a_run_limit_can_stop_one_run_without_exhausting_the_task() {
    let clock = Arc::new(FakeClock::default());
    let budget = task(&clock, limits(100, 60), TokenBudgetMode::Hard);
    let mut run_limits = limits(100, 60);
    run_limits.resources.model_requests = 1;
    let mut first = budget.begin_run(TurnId::new(), run_limits).unwrap();
    let mut reservation = first.scope().reserve(model_request(1, 1)).unwrap();
    reservation.mark_started().unwrap();
    reservation.settle(BudgetUsage::actual(1, 1, 0)).unwrap();
    assert!(first.scope().reserve(model_request(1, 1)).is_err());
    let report = first.finish().unwrap();
    assert!(report.stop.is_none());
    assert!(matches!(
        report.run.unwrap().stop,
        Some(BudgetStopReason::ResourceLimit(
            BudgetResource::ModelRequests
        ))
    ));
    let mut second = budget.begin_run(TurnId::new(), run_limits).unwrap();
    let mut reservation = second.scope().reserve(model_request(1, 1)).unwrap();
    reservation.mark_started().unwrap();
    reservation.settle(BudgetUsage::actual(1, 1, 0)).unwrap();
    let report = second.finish().unwrap();
    assert_eq!(report.charged.model_requests, 2);
    assert!(report.stop.is_none());
}

#[test]
fn a_clock_fault_stops_new_work_but_does_not_block_existing_settlement() {
    let clock = Arc::new(FakeClock::default());
    clock.set(Duration::from_secs(10));
    let budget = task(&clock, limits(100, 60), TokenBudgetMode::Hard);
    let mut run = budget.begin_run(TurnId::new(), limits(100, 60)).unwrap();
    let mut reservation = run.scope().reserve(model_request(10, 20)).unwrap();
    reservation.mark_started().unwrap();
    clock.set(Duration::from_secs(12));
    assert_eq!(budget.report().unwrap().active_time, Duration::from_secs(2));
    clock.set(Duration::from_secs(11));
    assert!(run.scope().reserve(model_request(1, 1)).is_err());
    reservation.settle(BudgetUsage::actual(3, 4, 0)).unwrap();
    let report = run.finish().unwrap();
    assert_eq!(report.charged.model_requests, 1);
    assert_eq!(report.charged.input_tokens, 3);
    assert_eq!(report.charged.output_tokens, 4);
    assert_eq!(report.active_time, Duration::from_secs(2));
    assert_zero(&report.reserved);
    assert!(report.pending.is_empty());
    assert!(matches!(
        report.stop,
        Some(BudgetStopReason::ClockMovedBackwards)
    ));
}

#[test]
fn empty_or_variable_only_requests_cannot_create_reservations() {
    let clock = Arc::new(FakeClock::default());
    let budget = task(&clock, limits(100, 60), TokenBudgetMode::Hard);
    let mut run = budget.begin_run(TurnId::new(), limits(100, 60)).unwrap();
    for value in [
        BudgetAmounts::default(),
        BudgetAmounts {
            input_tokens: 1,
            output_tokens: 1,
            tool_output_bytes: 1,
            ..Default::default()
        },
    ] {
        assert_eq!(
            run.scope().reserve(request(value)).err(),
            Some(BudgetError::EmptyRequest)
        );
    }
    let report = run.finish().unwrap();
    assert_zero(&report.charged);
    assert_zero(&report.reserved);
    assert!(report.pending.is_empty());
}

#[test]
fn task_session_and_agent_identity_must_each_be_nonblank() {
    let clock = Arc::new(FakeClock::default());
    for invalid in [
        BudgetIdentity {
            task_id: TaskId::from(""),
            ..identity()
        },
        BudgetIdentity {
            session_id: SessionId::from(" \n"),
            ..identity()
        },
        BudgetIdentity {
            agent_key: "\t".into(),
            ..identity()
        },
    ] {
        assert_eq!(
            TaskBudget::new(
                invalid,
                limits(100, 60),
                limits(100, 60),
                TokenBudgetMode::Hard,
                clock.clone(),
            )
            .err(),
            Some(BudgetError::InvalidIdentity)
        );
    }
}

#[test]
fn admission_counts_wait_and_retains_task_lease_until_report_barrier_finishes() {
    let clock = Arc::new(FakeClock::default());
    let budget = task(&clock, limits(10, 60), TokenBudgetMode::Hard);
    let mut run = budget.begin_admission(limits(10, 60)).unwrap();
    let scope = run.scope();
    assert_eq!(scope.turn_id().unwrap(), None);
    clock.set(Duration::from_secs(3));
    run.bind_turn(TurnId::from("canonical-turn")).unwrap();
    assert_eq!(scope.remaining_time().unwrap(), Duration::from_secs(57));
    assert_eq!(
        run.bind_turn(TurnId::from("replacement")),
        Err(BudgetError::IdentityMismatch)
    );
    scope.record_session_wait(Duration::from_secs(3));
    let report = run.prepare_report().unwrap();
    assert_eq!(
        report.run.as_ref().unwrap().turn_id,
        Some(TurnId::from("canonical-turn"))
    );
    assert!(!report.run.as_ref().unwrap().open);
    assert_eq!(scope.check_active(), Err(BudgetError::RunClosed));
    assert!(matches!(
        budget.begin_admission(limits(10, 60)),
        Err(BudgetError::RunActive)
    ));
    clock.set(Duration::from_secs(10));
    run.finish().unwrap();
    let mut next = budget.begin_admission(limits(10, 60)).unwrap();
    assert_eq!(
        next.scope().remaining_time().unwrap(),
        Duration::from_secs(57)
    );
    next.finish().unwrap();
}

#[tokio::test]
async fn budget_stop_wakes_all_registered_observers_without_timer_polling() {
    use futures::FutureExt;
    let clock = Arc::new(FakeClock::default());
    let budget = task(&clock, limits(1, 60), TokenBudgetMode::Hard);
    let mut run = budget.begin_run(TurnId::new(), limits(1, 60)).unwrap();
    let scope = run.scope();
    let first = scope.stopped();
    let second = scope.stopped();
    tokio::pin!(first, second);
    assert!(first.as_mut().now_or_never().is_none());
    assert!(second.as_mut().now_or_never().is_none());
    scope
        .reserve(request(BudgetAmounts {
            steps: 1,
            ..Default::default()
        }))
        .unwrap()
        .cancel_before_start()
        .unwrap();
    assert!(
        scope
            .reserve(request(BudgetAmounts {
                steps: 1,
                ..Default::default()
            }))
            .is_err()
    );
    let (a, b) = tokio::join!(first, second);
    assert_eq!(
        a,
        BudgetError::Stopped(BudgetStopReason::ResourceLimit(BudgetResource::Steps))
    );
    assert_eq!(a, b);
    run.finish().unwrap();
}
