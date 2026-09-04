//! Real local-file and child-process adapter verification; never published.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use jingwei::budget::*;
use jingwei::id::{SessionId, TaskId, TurnId};
use jingwei_budget_file::{FileBudgetCheckpointConfig, FileBudgetCheckpointStore};
use serde_json::{Value, json};

struct Clock;
impl BudgetClock for Clock {
    fn now(&self) -> Duration {
        Duration::ZERO
    }
}

fn identity() -> BudgetIdentity {
    BudgetIdentity {
        task_id: TaskId::from("file-task"),
        session_id: SessionId::from("file-session"),
        agent_key: "file-agent".into(),
    }
}

fn context(revision: u64) -> BudgetRestoreContext {
    BudgetRestoreContext {
        identity: identity(),
        revision,
        confirmed_anchor: None,
        host_limits: BudgetLimits::default(),
    }
}

fn image(steps: u64) -> BudgetCheckpoint {
    let task = TaskBudget::new(
        identity(),
        BudgetLimits::default(),
        BudgetLimits::default(),
        TokenBudgetMode::Hard,
        Arc::new(Clock),
    )
    .unwrap();
    let mut run = task
        .begin_run(TurnId::from("file-turn"), BudgetLimits::default())
        .unwrap();
    let mut reservation = run
        .scope()
        .reserve(
            BudgetRequest::new(BudgetAmounts {
                steps,
                input_tokens: 7,
                output_tokens: 9,
                model_requests: 1,
                ..Default::default()
            })
            .with_token_evidence(
                TokenBoundEvidence::VerifiedUpperBound,
                TokenBoundEvidence::VerifiedUpperBound,
            ),
        )
        .unwrap();
    reservation.mark_started().unwrap();
    reservation
        .settle(BudgetUsage {
            input_tokens: UsageValue::Unknown,
            output_tokens: UsageValue::Actual(3),
            tool_output_bytes: UsageValue::Actual(0),
        })
        .unwrap();
    run.finish().unwrap();
    task.seal_checkpoint(None).unwrap()
}

fn successor(checkpoint: BudgetCheckpoint) -> BudgetCheckpoint {
    let revision = checkpoint.revision();
    TaskBudget::restore_checkpoint(checkpoint, context(revision), Arc::new(Clock))
        .unwrap()
        .seal_checkpoint(None)
        .unwrap()
}

fn record(checkpoint: &BudgetCheckpoint) -> Value {
    json!({"version":1, "expected_revision":checkpoint.revision()-1, "checkpoint":checkpoint})
}

fn line(record: &Value) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(record).unwrap();
    bytes.push(b'\n');
    bytes
}

struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "jingwei-file-checkpoint-{}-{}",
            std::process::id(),
            TaskId::new().as_str()
        ));
        fs::create_dir(&path).unwrap();
        File::create_new(path.join("task.jsonl"))
            .unwrap()
            .sync_all()
            .unwrap();
        Self(path)
    }
    fn path(&self) -> PathBuf {
        self.0.join("task.jsonl")
    }
    fn store(&self) -> FileBudgetCheckpointStore {
        FileBudgetCheckpointStore::open(self.path(), FileBudgetCheckpointConfig::default()).unwrap()
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        for name in ["task.jsonl", "ready"] {
            let _ = fs::remove_file(self.0.join(name));
        }
        let _ = fs::remove_dir(&self.0);
    }
}

#[tokio::test]
async fn commit_load_reopen_and_exact_retry_preserve_bytes() {
    let temp = Temp::new();
    let store = temp.store();
    let checkpoint = image(1);
    assert!(store.load(&identity()).await.unwrap().is_none());
    assert_eq!(
        store.compare_exchange(0, &checkpoint).await.unwrap(),
        BudgetCheckpointCommit::Committed
    );
    let bytes = fs::read(temp.path()).unwrap();
    assert_eq!(
        store.compare_exchange(0, &checkpoint).await.unwrap(),
        BudgetCheckpointCommit::ReplayedExact
    );
    assert_eq!(fs::read(temp.path()).unwrap(), bytes);
    store.close().await;
    let reopened = temp.store();
    assert_eq!(
        reopened.load(&identity()).await.unwrap(),
        Some(checkpoint.clone())
    );
    let next = successor(checkpoint.clone());
    assert_eq!(
        reopened.compare_exchange(1, &next).await.unwrap(),
        BudgetCheckpointCommit::Committed
    );
    assert_eq!(reopened.load(&identity()).await.unwrap(), Some(next));
    assert!(matches!(
        reopened.compare_exchange(0, &checkpoint).await,
        Err(BudgetCheckpointStoreError::Conflict {
            expected: 0,
            actual: 2
        })
    ));
}

#[tokio::test]
async fn stale_competing_and_wrong_identity_never_mutate_log() {
    let temp = Temp::new();
    let store = temp.store();
    store.compare_exchange(0, &image(1)).await.unwrap();
    let bytes = fs::read(temp.path()).unwrap();
    assert!(matches!(
        store.compare_exchange(0, &image(2)).await,
        Err(BudgetCheckpointStoreError::Conflict {
            expected: 0,
            actual: 1
        })
    ));
    let mut wrong = identity();
    wrong.agent_key = "another-agent".into();
    assert!(matches!(
        store.load(&wrong).await,
        Err(BudgetCheckpointStoreError::Invalid(
            BudgetCheckpointError::IdentityMismatch
        ))
    ));
    let mut wire = serde_json::to_value(image(1)).unwrap();
    wire["report"]["identity"]["agent_key"] = json!("another-agent");
    let wrong = serde_json::from_value(wire).unwrap();
    assert!(matches!(
        store.compare_exchange(0, &wrong).await,
        Err(BudgetCheckpointStoreError::Invalid(
            BudgetCheckpointError::IdentityMismatch
        ))
    ));
    assert_eq!(fs::read(temp.path()).unwrap(), bytes);
}

#[tokio::test]
async fn invalid_successor_is_rejected_before_writing() {
    let temp = Temp::new();
    let store = temp.store();
    assert!(matches!(
        store.compare_exchange(1, &image(1)).await,
        Err(BudgetCheckpointStoreError::Invalid(
            BudgetCheckpointError::RevisionMismatch { .. }
        ))
    ));
    assert!(fs::read(temp.path()).unwrap().is_empty());
}

#[tokio::test]
async fn complete_unacknowledged_record_is_confirmed_without_duplicate_append() {
    let temp = Temp::new();
    let checkpoint = image(1);
    // A complete write with no success receipt. This is not a simulated fsync failure.
    let bytes = line(&record(&checkpoint));
    fs::write(temp.path(), &bytes).unwrap();
    let store = temp.store();
    assert_eq!(
        store.load(&identity()).await.unwrap(),
        Some(checkpoint.clone())
    );
    assert_eq!(
        store.compare_exchange(0, &checkpoint).await.unwrap(),
        BudgetCheckpointCommit::ReplayedExact
    );
    assert_eq!(fs::read(temp.path()).unwrap(), bytes);
}

#[tokio::test]
async fn corrupt_and_torn_logs_fail_closed_without_repair() {
    let temp = Temp::new();
    let first = record(&image(1));
    let second = record(&successor(image(1)));
    let mut cases = vec![
        b"{".to_vec(),
        b"\n".to_vec(),
        serde_json::to_vec(&first).unwrap(),
    ];
    for version in [json!(2), json!("1"), json!(null)] {
        let mut wrong = first.clone();
        wrong["version"] = version;
        cases.push(line(&wrong));
    }
    let mut missing = first.clone();
    missing.as_object_mut().unwrap().remove("version");
    cases.push(line(&missing));
    let mut extra = first.clone();
    extra["ignored"] = json!(true);
    cases.push(line(&extra));
    let mut wrong = first.clone();
    wrong["checkpoint"]["version"] = json!(2);
    cases.push(line(&wrong));
    let mut wrong = first.clone();
    wrong["checkpoint"]["report"]["reserved"]["input_tokens"] = json!(9);
    cases.push(line(&wrong));
    cases.push(line(&second)); // Must start at revision 1.
    cases.push([line(&first), line(&first)].concat());
    cases.push([line(&first), b"{\"version\":1".to_vec()].concat());
    cases.push([line(&first), b"\n".to_vec()].concat());
    let mut changed = second.clone();
    changed["checkpoint"]["report"]["identity"]["agent_key"] = json!("other");
    cases.push([line(&first), line(&changed)].concat());
    for bytes in cases {
        fs::write(temp.path(), &bytes).unwrap();
        let store = temp.store();
        assert!(
            matches!(
                store.load(&identity()).await,
                Err(BudgetCheckpointStoreError::Corrupt { .. })
            ),
            "{bytes:?}"
        );
        assert!(matches!(
            store.compare_exchange(0, &image(1)).await,
            Err(BudgetCheckpointStoreError::Corrupt { .. })
        ));
        assert_eq!(fs::read(temp.path()).unwrap(), bytes);
    }
}

#[tokio::test]
async fn byte_limits_cover_read_and_append_without_partial_writes() {
    let temp = Temp::new();
    let checkpoint = image(1);
    let config = FileBudgetCheckpointConfig {
        max_record_bytes: 8,
        max_log_bytes: 64,
        max_io_jobs: 1,
    };
    let store = FileBudgetCheckpointStore::open(temp.path(), config).unwrap();
    assert!(matches!(
        store.compare_exchange(0, &checkpoint).await,
        Err(BudgetCheckpointStoreError::LimitExceeded {
            resource: "record bytes",
            ..
        })
    ));
    assert!(fs::read(temp.path()).unwrap().is_empty());
    temp.store().compare_exchange(0, &checkpoint).await.unwrap();
    let bytes = fs::read(temp.path()).unwrap();
    let size = bytes.len();
    assert!(matches!(
        store.load(&identity()).await,
        Err(BudgetCheckpointStoreError::LimitExceeded {
            resource: "log bytes",
            ..
        })
    ));
    let store = FileBudgetCheckpointStore::open(
        temp.path(),
        FileBudgetCheckpointConfig {
            max_record_bytes: 8,
            max_log_bytes: size,
            max_io_jobs: 1,
        },
    )
    .unwrap();
    assert!(matches!(
        store.load(&identity()).await,
        Err(BudgetCheckpointStoreError::LimitExceeded {
            resource: "record bytes",
            ..
        })
    ));
    let store = FileBudgetCheckpointStore::open(
        temp.path(),
        FileBudgetCheckpointConfig {
            max_record_bytes: size,
            max_log_bytes: size,
            max_io_jobs: 1,
        },
    )
    .unwrap();
    assert_eq!(
        store.load(&identity()).await.unwrap(),
        Some(checkpoint.clone())
    );
    assert!(matches!(
        store.compare_exchange(1, &successor(checkpoint)).await,
        Err(BudgetCheckpointStoreError::LimitExceeded {
            resource: "log bytes",
            ..
        })
    ));
    assert_eq!(fs::read(temp.path()).unwrap(), bytes);
}

#[test]
fn opening_never_creates_files_and_rejects_invalid_configuration() {
    let temp = Temp::new();
    let missing = temp.0.join("missing.jsonl");
    assert!(
        FileBudgetCheckpointStore::open(&missing, FileBudgetCheckpointConfig::default()).is_err()
    );
    assert!(!missing.exists());
    assert!(
        FileBudgetCheckpointStore::open(&temp.0, FileBudgetCheckpointConfig::default()).is_err()
    );
    for config in [
        FileBudgetCheckpointConfig {
            max_record_bytes: 0,
            ..Default::default()
        },
        FileBudgetCheckpointConfig {
            max_io_jobs: 0,
            ..Default::default()
        },
        FileBudgetCheckpointConfig {
            max_log_bytes: 1,
            ..Default::default()
        },
        FileBudgetCheckpointConfig {
            max_log_bytes: usize::MAX,
            ..Default::default()
        },
    ] {
        assert!(FileBudgetCheckpointStore::open(temp.path(), config).is_err());
    }
}

#[tokio::test]
async fn missing_file_after_open_is_error_not_absence() {
    let temp = Temp::new();
    let store = temp.store();
    fs::remove_file(temp.path()).unwrap();
    assert!(matches!(
        store.load(&identity()).await,
        Err(BudgetCheckpointStoreError::Storage {
            certainty: BudgetCheckpointCommitCertainty::DefinitelyNotCommitted,
            ..
        })
    ));
    assert!(!temp.path().exists());
}

#[test]
fn calls_outside_tokio_return_error_without_panicking() {
    let temp = Temp::new();
    let store = temp.store();
    assert!(matches!(
        futures::executor::block_on(store.load(&identity())),
        Err(BudgetCheckpointStoreError::Storage {
            certainty: BudgetCheckpointCommitCertainty::DefinitelyNotCommitted,
            ..
        })
    ));
    assert_eq!(store.status().in_flight, 0);
}

#[tokio::test]
async fn exclusive_os_lock_returns_busy_and_never_writes() {
    let temp = Temp::new();
    let locked = OpenOptions::new()
        .read(true)
        .write(true)
        .open(temp.path())
        .unwrap();
    locked.try_lock().unwrap();
    let store = temp.store();
    assert!(matches!(
        store.load(&identity()).await,
        Err(BudgetCheckpointStoreError::Busy)
    ));
    assert!(matches!(
        store.compare_exchange(0, &image(1)).await,
        Err(BudgetCheckpointStoreError::Busy)
    ));
    drop(locked);
    assert!(fs::read(temp.path()).unwrap().is_empty());
    store.compare_exchange(0, &image(1)).await.unwrap();
}

#[tokio::test]
async fn close_is_shared_by_clones_and_reopening_is_explicit() {
    let temp = Temp::new();
    let store = temp.store();
    let clone = store.clone();
    store.close().await;
    clone.close().await;
    assert!(!clone.status().accepting);
    assert!(matches!(
        clone.load(&identity()).await,
        Err(BudgetCheckpointStoreError::Closed)
    ));
    assert!(matches!(
        clone.compare_exchange(0, &image(1)).await,
        Err(BudgetCheckpointStoreError::Closed)
    ));
    assert!(temp.store().load(&identity()).await.unwrap().is_none());
}

#[test]
fn dropped_waiter_keeps_job_owned_bounded_and_drainable() {
    let temp = Temp::new();
    let store = FileBudgetCheckpointStore::open(
        temp.path(),
        FileBudgetCheckpointConfig {
            max_io_jobs: 1,
            ..Default::default()
        },
    )
    .unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    let (release, blocked) = std::sync::mpsc::channel();
    let (started, ready) = std::sync::mpsc::channel();
    let blocker = runtime.spawn_blocking(move || {
        started.send(()).unwrap();
        blocked.recv_timeout(Duration::from_secs(10)).unwrap();
    });
    ready.recv_timeout(Duration::from_secs(10)).unwrap();
    runtime.block_on(async {
        let checkpoint = image(1);
        let mut write = store.compare_exchange(0, &checkpoint);
        assert!(futures::poll!(write.as_mut()).is_pending());
        assert_eq!(store.status().in_flight, 1);
        assert!(matches!(
            store.load(&identity()).await,
            Err(BudgetCheckpointStoreError::Busy)
        ));
        drop(write);
        let mut close = Box::pin(store.close());
        assert!(futures::poll!(close.as_mut()).is_pending());
        drop(close);
        assert!(!store.status().accepting);
        assert_eq!(store.status().in_flight, 1);
        assert!(fs::read(temp.path()).unwrap().is_empty());
        release.send(()).unwrap();
        blocker.await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), store.close())
            .await
            .unwrap();
        assert_eq!(store.status().in_flight, 0);
        assert_eq!(
            temp.store().load(&identity()).await.unwrap(),
            Some(checkpoint)
        );
    });
}

#[tokio::test]
async fn unpolled_request_has_no_ownership_and_cannot_bypass_close() {
    let temp = Temp::new();
    let store = temp.store();
    let checkpoint = image(1);
    let write = store.compare_exchange(0, &checkpoint);
    assert_eq!(store.status().in_flight, 0);
    store.close().await;
    assert!(matches!(
        write.await,
        Err(BudgetCheckpointStoreError::Closed)
    ));
    assert!(fs::read(temp.path()).unwrap().is_empty());
}

struct Worker(Option<Child>);
impl Worker {
    fn spawn(temp: &Temp, mode: &str, steps: u64) -> Self {
        Self(Some(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "checkpoint_file::file_checkpoint_child_worker",
                    "--nocapture",
                ])
                .env("JINGWEI_FILE_CHECKPOINT_TEST_PATH", temp.path())
                .env("JINGWEI_FILE_CHECKPOINT_TEST_MODE", mode)
                .env("JINGWEI_FILE_CHECKPOINT_TEST_STEPS", steps.to_string())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        ))
    }
    fn finish(mut self) -> String {
        let start = Instant::now();
        while self.0.as_mut().unwrap().try_wait().unwrap().is_none() {
            assert!(start.elapsed() < Duration::from_secs(30), "child timed out");
            std::thread::sleep(Duration::from_millis(10));
        }
        let output = self.0.take().unwrap().wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[test]
fn file_checkpoint_child_worker() {
    let Some(path) = std::env::var_os("JINGWEI_FILE_CHECKPOINT_TEST_PATH") else {
        return;
    };
    let path = Path::new(&path);
    let mode = std::env::var("JINGWEI_FILE_CHECKPOINT_TEST_MODE").unwrap();
    if mode == "hold" {
        let locked = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        locked.try_lock().unwrap();
        let mut ready = File::create_new(path.with_file_name("ready")).unwrap();
        ready.write_all(b"locked").unwrap();
        ready.sync_all().unwrap();
        std::thread::sleep(Duration::from_secs(30));
        drop(locked);
        return;
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let store =
            FileBudgetCheckpointStore::open(path, FileBudgetCheckpointConfig::default()).unwrap();
        if mode == "load" {
            let checkpoint = store.load(&identity()).await.unwrap().unwrap();
            let expected_steps: u64 = std::env::var("JINGWEI_FILE_CHECKPOINT_TEST_STEPS")
                .unwrap()
                .parse()
                .unwrap();
            assert_eq!(checkpoint.report().charged.steps, expected_steps);
            assert_eq!(checkpoint.report().charged.input_tokens, 7);
            let task =
                TaskBudget::restore_checkpoint(checkpoint, context(1), Arc::new(Clock)).unwrap();
            assert_eq!(task.report().unwrap().charged.steps, expected_steps);
            println!("RESULT=loaded");
        } else if mode == "busy" {
            assert!(matches!(
                store.compare_exchange(0, &image(1)).await,
                Err(BudgetCheckpointStoreError::Busy)
            ));
            println!("RESULT=busy");
        } else {
            let steps = std::env::var("JINGWEI_FILE_CHECKPOINT_TEST_STEPS")
                .unwrap()
                .parse()
                .unwrap();
            let checkpoint = image(steps);
            let start = Instant::now();
            loop {
                match store.compare_exchange(0, &checkpoint).await {
                    Ok(BudgetCheckpointCommit::Committed) => {
                        println!("RESULT=committed");
                        break;
                    }
                    Err(BudgetCheckpointStoreError::Conflict {
                        expected: 0,
                        actual: 1,
                    }) => {
                        println!("RESULT=conflict");
                        break;
                    }
                    Err(BudgetCheckpointStoreError::Busy) => {
                        assert!(start.elapsed() < Duration::from_secs(10));
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                    other => panic!("unexpected child CAS result: {other:?}"),
                }
            }
        }
        store.close().await;
    });
}

#[test]
fn distinct_process_writers_have_one_cas_winner_and_new_process_loads_charge() {
    let temp = Temp::new();
    let left = Worker::spawn(&temp, "cas", 1);
    let right = Worker::spawn(&temp, "cas", 2);
    let outputs = [left.finish(), right.finish()];
    assert_eq!(
        outputs
            .iter()
            .filter(|s| s.contains("RESULT=committed"))
            .count(),
        1
    );
    assert_eq!(
        outputs
            .iter()
            .filter(|s| s.contains("RESULT=conflict"))
            .count(),
        1
    );
    let steps = if outputs[0].contains("RESULT=committed") {
        1
    } else {
        2
    };
    assert!(
        Worker::spawn(&temp, "load", steps)
            .finish()
            .contains("RESULT=loaded")
    );
    assert_eq!(
        fs::read(temp.path())
            .unwrap()
            .iter()
            .filter(|b| **b == b'\n')
            .count(),
        1
    );
}

#[test]
fn parent_os_lock_is_visible_in_child_process() {
    let temp = Temp::new();
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(temp.path())
        .unwrap();
    file.try_lock().unwrap();
    assert!(
        Worker::spawn(&temp, "busy", 1)
            .finish()
            .contains("RESULT=busy")
    );
    drop(file);
    assert!(fs::read(temp.path()).unwrap().is_empty());
}

#[test]
fn process_death_releases_os_lock_without_a_stale_lockfile() {
    let temp = Temp::new();
    let holder = Worker::spawn(&temp, "hold", 1);
    let start = Instant::now();
    while !temp.0.join("ready").exists() {
        assert!(start.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        Worker::spawn(&temp, "busy", 1)
            .finish()
            .contains("RESULT=busy")
    );
    drop(holder); // Kill only this test's own child and wait for process exit.
    assert!(
        Worker::spawn(&temp, "cas", 1)
            .finish()
            .contains("RESULT=committed")
    );
}
