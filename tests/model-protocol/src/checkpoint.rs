//! Checkpoint primitives, not an automatic runtime persistence implementation.

use std::collections::HashMap;
use std::sync::{Arc, Barrier, Mutex};
use std::time::Duration;

use jingwei::budget::*;
use jingwei::id::{EventId, SessionId, TaskId, TurnId};
use serde_json::{Value, json};

#[derive(Default)]
struct Clock(Mutex<Duration>);
impl Clock {
    fn set(&self, seconds: u64) {
        *self.0.lock().unwrap() = Duration::from_secs(seconds);
    }
}
impl BudgetClock for Clock {
    fn now(&self) -> Duration {
        *self.0.lock().unwrap()
    }
}

fn identity() -> BudgetIdentity {
    BudgetIdentity {
        task_id: TaskId::from("checkpoint-task"),
        session_id: SessionId::from("checkpoint-session"),
        agent_key: "checkpoint-agent".into(),
    }
}

fn limits() -> BudgetLimits {
    BudgetLimits {
        resources: BudgetAmounts {
            steps: 3,
            model_requests: 3,
            tool_calls: 3,
            corrections: 3,
            input_tokens: 1000,
            output_tokens: 1000,
            tool_output_bytes: 1000,
        },
        active_time: Duration::from_secs(20),
    }
}

fn task(clock: &Arc<Clock>) -> TaskBudget {
    TaskBudget::new(
        identity(),
        limits(),
        limits(),
        TokenBudgetMode::Hard,
        clock.clone(),
    )
    .unwrap()
}

fn request() -> BudgetRequest {
    BudgetRequest::new(BudgetAmounts {
        steps: 1,
        model_requests: 1,
        input_tokens: 7,
        output_tokens: 9,
        ..Default::default()
    })
    .with_token_evidence(
        TokenBoundEvidence::VerifiedUpperBound,
        TokenBoundEvidence::VerifiedUpperBound,
    )
}

fn cursor() -> BudgetEventCursor {
    BudgetEventCursor {
        session_id: identity().session_id,
        turn_id: TurnId::from("turn"),
        event_id: EventId::from("confirmed-event"),
        seq: 3,
    }
}

fn context(revision: u64) -> BudgetRestoreContext {
    BudgetRestoreContext {
        identity: identity(),
        revision,
        confirmed_anchor: Some(cursor()),
        host_limits: limits(),
    }
}

fn settled_image() -> BudgetCheckpoint {
    let clock = Arc::new(Clock::default());
    let budget = task(&clock);
    let mut run = budget.begin_run(TurnId::from("turn"), limits()).unwrap();
    let mut reservation = run.scope().reserve(request()).unwrap();
    reservation.mark_started().unwrap();
    clock.set(2);
    reservation
        .settle(BudgetUsage {
            input_tokens: UsageValue::Unknown,
            output_tokens: UsageValue::Actual(3),
            tool_output_bytes: UsageValue::Actual(0),
        })
        .unwrap();
    run.finish().unwrap();
    budget.seal_checkpoint(Some(cursor())).unwrap()
}

fn interrupted_image(started: bool) -> BudgetCheckpoint {
    let clock = Arc::new(Clock::default());
    let budget = task(&clock);
    let run = budget.begin_run(TurnId::from("turn"), limits()).unwrap();
    let mut reservation = run.scope().reserve(request()).unwrap();
    if started {
        reservation.mark_started().unwrap();
    }
    clock.set(2);
    budget.seal_checkpoint(Some(cursor())).unwrap()
}

fn restore(image: BudgetCheckpoint, clock: &Arc<Clock>) -> TaskBudget {
    let expected = context(image.revision());
    TaskBudget::restore_checkpoint(image, expected, clock.clone()).unwrap()
}

#[test]
fn idle_restore_preserves_unknown_usage_and_rebases_the_clock() {
    let image = settled_image();
    let old = image.report().clone();
    let clock = Arc::new(Clock::default());
    clock.set(10_000);
    let budget = restore(image, &clock);
    assert_eq!(budget.report().unwrap(), old);
    clock.set(20_000); // Offline/user-wait time is not charged.
    let mut run = budget.begin_run(TurnId::new(), limits()).unwrap();
    clock.set(20_003);
    let mut reservation = run.scope().reserve(request()).unwrap();
    reservation.mark_started().unwrap();
    reservation
        .settle(BudgetUsage {
            input_tokens: UsageValue::Estimated(2),
            output_tokens: UsageValue::Unknown,
            tool_output_bytes: UsageValue::Actual(0),
        })
        .unwrap();
    run.finish().unwrap();
    let report = budget.report().unwrap();
    assert_eq!(report.charged.model_requests, 2);
    assert_eq!(report.charged.input_tokens, 14);
    assert_eq!(report.usage.input_tokens.unknown, 1);
    assert_eq!(report.usage.input_tokens.estimated, 2);
    assert_eq!(report.usage.output_tokens.actual, 3);
    assert_eq!(report.usage.output_tokens.unknown, 1);
    assert_eq!(report.active_time, Duration::from_secs(5));
    assert_eq!(report.token_mode, TokenBudgetMode::Hard);
    assert_eq!(
        budget.seal_checkpoint(Some(cursor())).unwrap().revision(),
        2
    );
}

#[test]
fn sealing_invalidates_all_old_handles_without_refunding_pending_resources() {
    let clock = Arc::new(Clock::default());
    let budget = task(&clock);
    let clone = budget.clone();
    let mut run = budget.begin_run(TurnId::new(), limits()).unwrap();
    let scope = run.scope();
    let pending = scope.reserve(request()).unwrap();
    let image = budget.seal_checkpoint(Some(cursor())).unwrap();
    assert_eq!(clone.report(), Err(BudgetError::CheckpointSealed));
    assert!(matches!(
        clone.begin_admission(limits()),
        Err(BudgetError::CheckpointSealed)
    ));
    assert_eq!(scope.check_active(), Err(BudgetError::CheckpointSealed));
    assert_eq!(
        pending.cancel_before_start(),
        Err(BudgetError::CheckpointSealed)
    );
    assert_eq!(run.finish(), Err(BudgetError::CheckpointSealed));
    assert!(matches!(
        clone.seal_checkpoint(None),
        Err(BudgetCheckpointError::Budget(BudgetError::CheckpointSealed))
    ));
    let recovered = restore(image, &clock);
    let report = recovered.report().unwrap();
    assert_eq!(report.charged.model_requests, 1);
    assert_eq!(report.reserved.input_tokens, 7);
    assert!(!report.pending[0].started);
    assert_eq!(report.stop, Some(BudgetStopReason::RecoveryRequired));
}

#[test]
fn started_reservation_cannot_settle_after_its_ledger_is_sealed() {
    let clock = Arc::new(Clock::default());
    let budget = task(&clock);
    let run = budget.begin_run(TurnId::new(), limits()).unwrap();
    let mut pending = run.scope().reserve(request()).unwrap();
    pending.mark_started().unwrap();
    let image = budget.seal_checkpoint(Some(cursor())).unwrap();
    assert_eq!(
        pending.settle(BudgetUsage::actual(1, 1, 0)),
        Err(BudgetError::CheckpointSealed)
    );
    let recovered = restore(image, &clock);
    assert!(recovered.report().unwrap().pending[0].started);
    assert!(matches!(
        recovered.begin_admission(limits()),
        Err(BudgetError::Stopped(BudgetStopReason::RecoveryRequired))
    ));
}

#[test]
fn interrupted_restore_stays_frozen_across_another_checkpoint_and_does_not_invent_cleanup() {
    for started in [false, true] {
        let clock = Arc::new(Clock::default());
        let budget = restore(interrupted_image(started), &clock);
        let before = budget.report().unwrap();
        clock.set(100);
        assert_eq!(before, budget.report().unwrap());
        let image = budget.seal_checkpoint(Some(cursor())).unwrap();
        let recovered = restore(image, &clock);
        assert_eq!(before, recovered.report().unwrap());
        assert!(matches!(
            recovered.begin_admission(limits()),
            Err(BudgetError::Stopped(BudgetStopReason::RecoveryRequired))
        ));
    }
}

#[test]
fn admission_and_prepared_but_unfinished_runs_also_require_reconciliation() {
    for prepared in [false, true] {
        let clock = Arc::new(Clock::default());
        let budget = task(&clock);
        let mut run = budget.begin_admission(limits()).unwrap();
        if prepared {
            run.prepare_report().unwrap();
        }
        let image = budget.seal_checkpoint(None).unwrap();
        let mut expected = context(1);
        expected.confirmed_anchor = None;
        let recovered = TaskBudget::restore_checkpoint(image, expected, clock).unwrap();
        let report = recovered.report().unwrap();
        assert_eq!(report.run.unwrap().turn_id, None);
        assert_eq!(report.stop, Some(BudgetStopReason::RecoveryRequired));
        assert!(matches!(
            recovered.begin_admission(limits()),
            Err(BudgetError::Stopped(BudgetStopReason::RecoveryRequired))
        ));
    }
}

#[test]
fn invalid_cursor_rejects_export_without_sealing_the_ledger() {
    let clock = Arc::new(Clock::default());
    let budget = task(&clock);
    let mut wrong = cursor();
    wrong.session_id = SessionId::new();
    assert!(budget.seal_checkpoint(Some(wrong)).is_err());
    let mut run = budget.begin_admission(limits()).unwrap();
    run.finish().unwrap();
    assert_eq!(budget.seal_checkpoint(None).unwrap().revision(), 1);
}

#[test]
fn checkpoint_version_is_required_numeric_and_known() {
    let original = serde_json::to_value(settled_image()).unwrap();
    assert_eq!(original["version"], 1);
    for version in [Value::Null, json!(0), json!(2), json!("1"), json!(1.5)] {
        let mut wire = original.clone();
        wire["version"] = version;
        assert!(serde_json::from_value::<BudgetCheckpoint>(wire).is_err());
    }
    let mut wire = original.clone();
    wire.as_object_mut().unwrap().remove("version");
    assert!(serde_json::from_value::<BudgetCheckpoint>(wire).is_err());
    let mut wire = original;
    wire["unexpected_authority"] = json!(true);
    assert!(serde_json::from_value::<BudgetCheckpoint>(wire).is_err());
}

#[test]
fn restore_rejects_wrong_identity_revision_and_event_cursor() {
    let image = settled_image();
    let clock = Arc::new(Clock::default());
    for field in 0..3 {
        let mut expected = context(1);
        match field {
            0 => expected.identity.task_id = TaskId::new(),
            1 => expected.identity.session_id = SessionId::new(),
            _ => expected.identity.agent_key = "other".into(),
        }
        assert!(matches!(
            TaskBudget::restore_checkpoint(image.clone(), expected, clock.clone()),
            Err(BudgetCheckpointError::IdentityMismatch)
        ));
    }
    assert!(matches!(
        TaskBudget::restore_checkpoint(image.clone(), context(2), clock.clone()),
        Err(BudgetCheckpointError::RevisionMismatch { .. })
    ));
    for field in 0..5 {
        let mut expected = context(1);
        let anchor = expected.confirmed_anchor.as_mut().unwrap();
        match field {
            0 => anchor.seq += 1,
            1 => anchor.event_id = EventId::new(),
            2 => anchor.turn_id = TurnId::new(),
            3 => anchor.session_id = SessionId::new(),
            _ => expected.confirmed_anchor = None,
        }
        assert!(matches!(
            TaskBudget::restore_checkpoint(image.clone(), expected, clock.clone()),
            Err(BudgetCheckpointError::AnchorMismatch)
        ));
    }
}

#[test]
fn malformed_accounting_is_rejected_before_reconstruction() {
    let original = serde_json::to_value(interrupted_image(true)).unwrap();
    for field in 0..12 {
        let mut wire = original.clone();
        match field {
            0 => wire["revision"] = json!(0),
            1 => wire["next_id"] = json!(0),
            2 => wire["report"]["reserved"]["input_tokens"] = json!(0),
            3 => wire["report"]["reserved"]["steps"] = json!(1),
            4 => wire["report"]["charged"]["model_requests"] = json!(0),
            5 => wire["report"]["pending"][0]["id"] = wire["run_id"].clone(),
            6 => wire["report"]["pending"]
                .as_array_mut()
                .unwrap()
                .push(original["report"]["pending"][0].clone()),
            7 => wire["report"]["run"] = Value::Null,
            8 => wire["report"]["run"]["charged"]["steps"] = json!(100),
            9 => wire["report"]["run"]["active_time"]["secs"] = json!(100),
            10 => wire["report"]["pending"][0]["request"]["input_tokens"] = json!("Estimate"),
            _ => wire["report"]["usage"]["input_tokens"]["actual"] = json!(100),
        }
        let image: BudgetCheckpoint = serde_json::from_value(wire).unwrap();
        assert!(image.validate().is_err(), "mutation {field}");
        assert!(
            TaskBudget::restore_checkpoint(image, context(1), Arc::new(Clock::default())).is_err()
        );
    }
}

#[test]
fn host_caps_can_only_tighten_and_cannot_reset_consumption() {
    let image = settled_image();
    let clock = Arc::new(Clock::default());
    let mut expected = context(1);
    expected.host_limits.resources.steps = 100;
    expected.host_limits.active_time = Duration::from_secs(1000);
    let budget = TaskBudget::restore_checkpoint(image.clone(), expected, clock.clone()).unwrap();
    assert_eq!(budget.report().unwrap().limits, limits());
    let mut expected = context(1);
    expected.host_limits.resources.input_tokens = 6;
    let budget = TaskBudget::restore_checkpoint(image.clone(), expected, clock.clone()).unwrap();
    let report = budget.report().unwrap();
    assert_eq!(report.charged.input_tokens, 7);
    assert_eq!(report.limits.resources.input_tokens, 6);
    assert_eq!(
        report.stop,
        Some(BudgetStopReason::ResourceLimit(BudgetResource::InputTokens))
    );
    assert!(budget.begin_admission(limits()).is_err());
    let mut expected = context(1);
    expected.host_limits.active_time = Duration::from_secs(1);
    let budget = TaskBudget::restore_checkpoint(image, expected, clock.clone()).unwrap();
    assert_eq!(budget.report().unwrap().active_time, Duration::from_secs(2));
    assert_eq!(
        budget.report().unwrap().stop,
        Some(BudgetStopReason::ActiveTime)
    );
    let image = budget.seal_checkpoint(Some(cursor())).unwrap();
    image.validate().unwrap();
    assert!(restore(image, &clock).begin_admission(limits()).is_err());
}

#[test]
fn interrupted_restore_with_tighter_time_limit_never_subtracts_below_zero() {
    let clock = Arc::new(Clock::default());
    let mut expected = context(1);
    expected.host_limits.active_time = Duration::ZERO;
    let budget =
        TaskBudget::restore_checkpoint(interrupted_image(true), expected, clock.clone()).unwrap();
    clock.set(100);
    assert_eq!(budget.report().unwrap().active_time, Duration::from_secs(2));
    let image = budget.seal_checkpoint(Some(cursor())).unwrap();
    image.validate().unwrap();
    assert!(restore(image, &clock).begin_admission(limits()).is_err());
}

#[test]
fn overrun_stop_and_original_actual_usage_survive_restore() {
    let clock = Arc::new(Clock::default());
    let budget = task(&clock);
    let mut run = budget.begin_run(TurnId::new(), limits()).unwrap();
    let mut reservation = run.scope().reserve(request()).unwrap();
    reservation.mark_started().unwrap();
    assert!(reservation.settle(BudgetUsage::actual(1001, 1, 0)).is_err());
    run.finish().unwrap();
    let image = budget.seal_checkpoint(Some(cursor())).unwrap();
    let recovered = restore(image, &clock);
    let report = recovered.report().unwrap();
    assert_eq!(report.charged.input_tokens, 1001);
    assert_eq!(report.usage.input_tokens.actual, 1001);
    assert_eq!(
        report.stop,
        Some(BudgetStopReason::UsageExceededReservation(
            BudgetResource::InputTokens
        ))
    );
    assert!(recovered.begin_admission(limits()).is_err());
}

#[test]
fn failed_settlement_raw_usage_and_abandonment_survive_checkpoint_transfer() {
    let clock = Arc::new(Clock::default());
    let mut large = limits();
    large.resources.input_tokens = u64::MAX;
    let budget = TaskBudget::new(
        identity(),
        large,
        large,
        TokenBudgetMode::Hard,
        clock.clone(),
    )
    .unwrap();
    let run = budget.begin_run(TurnId::new(), large).unwrap();
    let mut first_request = request();
    first_request.amounts.input_tokens = u64::MAX - 2;
    let mut first = run.scope().reserve(first_request).unwrap();
    first.mark_started().unwrap();
    first
        .settle(BudgetUsage::actual(u64::MAX - 2, 0, 0))
        .unwrap();
    let mut second_request = request();
    second_request.amounts.input_tokens = 1;
    let mut second = run.scope().reserve(second_request).unwrap();
    second.mark_started().unwrap();
    assert!(second.settle(BudgetUsage::actual(10, 0, 0)).is_err());
    drop(run);
    let image = budget.seal_checkpoint(Some(cursor())).unwrap();
    let bytes = serde_json::to_vec(&image).unwrap();
    let image: BudgetCheckpoint = serde_json::from_slice(&bytes).unwrap();
    let mut expected = context(1);
    expected.host_limits = large;
    let recovered = TaskBudget::restore_checkpoint(image, expected, clock).unwrap();
    let report = recovered.report().unwrap();
    assert_eq!(report.charged.input_tokens, u64::MAX - 2);
    assert_eq!(report.reserved.input_tokens, 1);
    assert_eq!(report.stop, Some(BudgetStopReason::AccountingOverflow));
    assert_eq!(
        report.pending[0].failed_usage,
        Some(BudgetUsage::actual(10, 0, 0))
    );
    assert!(report.pending[0].abandoned);
    assert!(recovered.begin_admission(large).is_err());
}

#[test]
fn memory_report_is_not_a_checkpoint_wire_or_restore_authority() {
    let image = settled_image();
    assert!(
        serde_json::from_value::<BudgetCheckpoint>(serde_json::to_value(image.report()).unwrap())
            .is_err()
    );
}

#[test]
fn exactly_one_clone_can_export_a_given_ledger() {
    let clock = Arc::new(Clock::default());
    let budget = task(&clock);
    let barrier = Arc::new(Barrier::new(16));
    let threads: Vec<_> = (0..16)
        .map(|_| {
            let budget = budget.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                budget.seal_checkpoint(None).is_ok()
            })
        })
        .collect();
    assert_eq!(
        threads
            .into_iter()
            .filter_map(|t| t.join().unwrap().then_some(()))
            .count(),
        1
    );
}

#[tokio::test]
async fn sealing_wakes_all_registered_stop_observers() {
    let clock = Arc::new(Clock::default());
    let budget = task(&clock);
    let run = budget.begin_admission(limits()).unwrap();
    let scope = run.scope();
    let first = scope.stopped();
    let second = scope.stopped();
    futures::pin_mut!(first, second);
    assert!(futures::poll!(&mut first).is_pending());
    assert!(futures::poll!(&mut second).is_pending());
    budget.seal_checkpoint(None).unwrap();
    assert_eq!(first.await, BudgetError::CheckpointSealed);
    assert_eq!(second.await, BudgetError::CheckpointSealed);
}

#[test]
fn checkpoint_revision_and_reservation_watermarks_do_not_reset() {
    let clock = Arc::new(Clock::default());
    let image = settled_image();
    image.validate_successor(0).unwrap();
    assert!(image.validate_successor(1).is_err());
    let first_id = serde_json::to_value(&image).unwrap()["next_id"]
        .as_u64()
        .unwrap();
    let budget = restore(image, &clock);
    let run = budget.begin_run(TurnId::new(), limits()).unwrap();
    let _pending = run.scope().reserve(request()).unwrap();
    let image = budget.seal_checkpoint(Some(cursor())).unwrap();
    image.validate_successor(1).unwrap();
    assert!(image.report().pending[0].id > first_id);
    let mut wire = serde_json::to_value(settled_image()).unwrap();
    wire["revision"] = json!(u64::MAX);
    let image: BudgetCheckpoint = serde_json::from_value(wire).unwrap();
    assert_eq!(
        image.validate_successor(u64::MAX),
        Err(BudgetCheckpointError::RevisionExhausted)
    );
    let mut expected = context(u64::MAX);
    expected.confirmed_anchor = Some(cursor());
    let budget = TaskBudget::restore_checkpoint(image, expected, clock).unwrap();
    assert!(matches!(
        budget.seal_checkpoint(None),
        Err(BudgetCheckpointError::RevisionExhausted)
    ));
    assert!(budget.report().is_ok());
}

/// Test-only provider: exercises the public async seam, not a durable storage claim.
#[derive(Default)]
struct MemoryStore(Mutex<HashMap<TaskId, BudgetCheckpoint>>);
impl BudgetCheckpointStore for MemoryStore {
    fn load<'a>(
        &'a self,
        identity: &'a BudgetIdentity,
    ) -> BudgetCheckpointFuture<'a, Result<Option<BudgetCheckpoint>, BudgetCheckpointStoreError>>
    {
        Box::pin(async move {
            let guard = self.0.lock().unwrap();
            let image = guard.get(&identity.task_id);
            if image.is_some_and(|image| image.identity() != identity) {
                return Err(BudgetCheckpointError::IdentityMismatch.into());
            }
            Ok(image.cloned())
        })
    }
    fn compare_exchange<'a>(
        &'a self,
        expected: u64,
        image: &'a BudgetCheckpoint,
    ) -> BudgetCheckpointFuture<'a, Result<BudgetCheckpointCommit, BudgetCheckpointStoreError>>
    {
        Box::pin(async move {
            image.validate_successor(expected)?;
            let mut guard = self.0.lock().unwrap();
            let prior = guard.get(&image.identity().task_id);
            if prior == Some(image) {
                return Ok(BudgetCheckpointCommit::ReplayedExact);
            }
            if prior.is_some_and(|prior| prior.identity() != image.identity()) {
                return Err(BudgetCheckpointError::IdentityMismatch.into());
            }
            let actual = prior.map_or(0, BudgetCheckpoint::revision);
            if actual != expected {
                return Err(BudgetCheckpointStoreError::Conflict { expected, actual });
            }
            guard.insert(image.identity().task_id.clone(), image.clone());
            Ok(BudgetCheckpointCommit::Committed)
        })
    }
}

#[tokio::test]
async fn storage_seam_rejects_stale_writes_and_supports_exact_retry() {
    let store = MemoryStore::default();
    let image = settled_image();
    assert!(store.load(&identity()).await.unwrap().is_none());
    assert_eq!(
        store.compare_exchange(0, &image).await.unwrap(),
        BudgetCheckpointCommit::Committed
    );
    assert_eq!(
        store.compare_exchange(0, &image).await.unwrap(),
        BudgetCheckpointCommit::ReplayedExact
    );
    let clock = Arc::new(Clock::default());
    let competing = task(&clock).seal_checkpoint(Some(cursor())).unwrap();
    assert!(matches!(
        store.compare_exchange(0, &competing).await,
        Err(BudgetCheckpointStoreError::Conflict {
            expected: 0,
            actual: 1
        })
    ));
    let image = store.load(&identity()).await.unwrap().unwrap();
    let budget = restore(image, &clock);
    let next = budget.seal_checkpoint(Some(cursor())).unwrap();
    assert_eq!(
        store.compare_exchange(1, &next).await.unwrap(),
        BudgetCheckpointCommit::Committed
    );
    assert_eq!(
        store.load(&identity()).await.unwrap().unwrap().revision(),
        2
    );
}

struct TransferFiles(std::path::PathBuf);
impl TransferFiles {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "jingwei-checkpoint-{}-{}",
            std::process::id(),
            TaskId::new().as_str()
        ));
        std::fs::create_dir(&dir).unwrap();
        Self(dir)
    }
    fn path(&self, name: &str) -> std::path::PathBuf {
        self.0.join(name)
    }
}
impl Drop for TransferFiles {
    fn drop(&mut self) {
        for name in ["one.json", "two.json", "three.json"] {
            let _ = std::fs::remove_file(self.path(name));
        }
        let _ = std::fs::remove_dir(&self.0);
    }
}

fn child(mode: &str, input: &std::path::Path, output: &std::path::Path) {
    use std::process::{Command, Stdio};
    let mut process = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "checkpoint::checkpoint_child_process_worker",
            "--nocapture",
        ])
        .env("JINGWEI_CHECKPOINT_TEST_MODE", mode)
        .env("JINGWEI_CHECKPOINT_TEST_INPUT", input)
        .env("JINGWEI_CHECKPOINT_TEST_OUTPUT", output)
        .env(
            "JINGWEI_CHECKPOINT_TEST_PARENT",
            std::process::id().to_string(),
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        if process.try_wait().unwrap().is_some() {
            break;
        }
        if std::time::Instant::now() >= deadline {
            let _ = process.kill();
            let _ = process.wait();
            panic!("checkpoint child timed out");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let result = process.wait_with_output().unwrap();
    assert!(
        result.status.success(),
        "child failed: {} {}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
fn checkpoint_child_process_worker() {
    let Ok(mode) = std::env::var("JINGWEI_CHECKPOINT_TEST_MODE") else {
        return;
    };
    assert_ne!(
        std::env::var("JINGWEI_CHECKPOINT_TEST_PARENT").unwrap(),
        std::process::id().to_string()
    );
    let bytes = std::fs::read(std::env::var_os("JINGWEI_CHECKPOINT_TEST_INPUT").unwrap()).unwrap();
    let image: BudgetCheckpoint = serde_json::from_slice(&bytes).unwrap();
    let clock = Arc::new(Clock::default());
    clock.set(500);
    let budget = restore(image, &clock);
    if mode == "frozen" {
        assert_eq!(budget.report().unwrap().reserved.input_tokens, 7);
        assert_eq!(budget.report().unwrap().pending.len(), 1);
        assert!(budget.begin_admission(limits()).is_err());
    } else {
        let before = budget.report().unwrap();
        assert_eq!(
            before.charged.model_requests,
            if mode == "resume" { 1 } else { 2 }
        );
        assert_eq!(
            before.active_time.as_secs(),
            if mode == "resume" { 2 } else { 5 }
        );
        assert_eq!(before.usage.input_tokens.unknown, 1);
        let mut run = budget.begin_run(TurnId::new(), limits()).unwrap();
        let mut reservation = run.scope().reserve(request()).unwrap();
        reservation.mark_started().unwrap();
        clock.set(if mode == "resume" { 503 } else { 502 });
        reservation
            .settle(BudgetUsage {
                input_tokens: UsageValue::Actual(5),
                output_tokens: UsageValue::Unknown,
                tool_output_bytes: UsageValue::Actual(0),
            })
            .unwrap();
        if mode == "exhaust" {
            assert!(run.scope().reserve(request()).is_err());
        }
        run.finish().unwrap();
    }
    let image = budget.seal_checkpoint(Some(cursor())).unwrap();
    std::fs::write(
        std::env::var_os("JINGWEI_CHECKPOINT_TEST_OUTPUT").unwrap(),
        serde_json::to_vec(&image).unwrap(),
    )
    .unwrap();
}

#[test]
fn two_new_processes_preserve_usage_and_exhaust_the_original_allowance() {
    let files = TransferFiles::new();
    std::fs::write(
        files.path("one.json"),
        serde_json::to_vec(&settled_image()).unwrap(),
    )
    .unwrap();
    child("resume", &files.path("one.json"), &files.path("two.json"));
    child(
        "exhaust",
        &files.path("two.json"),
        &files.path("three.json"),
    );
    let image: BudgetCheckpoint =
        serde_json::from_slice(&std::fs::read(files.path("three.json")).unwrap()).unwrap();
    assert_eq!(image.revision(), 3);
    let clock = Arc::new(Clock::default());
    let budget = restore(image, &clock);
    let report = budget.report().unwrap();
    assert_eq!(report.charged.model_requests, 3);
    assert_eq!(report.charged.input_tokens, 17);
    assert_eq!(report.usage.input_tokens.unknown, 1);
    assert_eq!(report.usage.output_tokens.unknown, 2);
    assert_eq!(report.active_time, Duration::from_secs(7));
    assert!(budget.begin_admission(limits()).is_err());
}

#[test]
fn new_process_keeps_interrupted_work_frozen_and_reserved() {
    let files = TransferFiles::new();
    std::fs::write(
        files.path("one.json"),
        serde_json::to_vec(&interrupted_image(true)).unwrap(),
    )
    .unwrap();
    child("frozen", &files.path("one.json"), &files.path("two.json"));
    let image: BudgetCheckpoint =
        serde_json::from_slice(&std::fs::read(files.path("two.json")).unwrap()).unwrap();
    assert_eq!(image.report().reserved.input_tokens, 7);
    assert_eq!(image.report().charged.model_requests, 1);
    assert_eq!(image.report().active_time, Duration::from_secs(2));
    assert_eq!(image.report().cleanup_time, Duration::ZERO);
}
