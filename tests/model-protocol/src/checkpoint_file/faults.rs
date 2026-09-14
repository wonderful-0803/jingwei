//! Exercise real adapters in isolated Linux children, without production hooks.
use super::*;
use jingwei::session::{
    CommitCertainty, PersistAppendOutcome, SessionPersistence, SessionPersistenceError,
};
use jingwei_core::{EventId, SessionEvent, SessionEventKind};
use jingwei_journal_jsonl::{JsonlSessionPersistence, session_file_path};
use std::sync::OnceLock;

struct Harness(PathBuf);
impl Harness {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("jingwei-syscall-faults-{}", TaskId::new()));
        fs::create_dir(&root).unwrap();
        Self(root)
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn library() -> &'static Path {
    static LIBRARY: OnceLock<PathBuf> = OnceLock::new();
    LIBRARY.get_or_init(|| {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/storage-faults");
        fs::create_dir_all(&root).unwrap();
        let library = root.join(format!("faults-{}.so", std::process::id()));
        let output = Command::new("cc")
            .args(["-shared", "-fPIC", "-Wall", "-Wextra", "-Werror"])
            .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/storage_faults.c"))
            .arg("-o")
            .arg(&library)
            .arg("-ldl")
            .output()
            .expect("Linux storage fault tests require cc");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        library
    })
}

fn run(adapter: &str, mode: &str) {
    let root = Harness::new();
    let target = if adapter == "budget" {
        root.0.join("task.jsonl")
    } else {
        session_file_path(&root.0, &identity().session_id)
    };
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "checkpoint_file::faults::fault_child",
            "--nocapture",
        ])
        .env("LD_PRELOAD", library())
        .env("JW_FAULT_ROOT", &root.0)
        .env("JW_FAULT_ADAPTER", adapter)
        .env("JW_FAULT_MODE", mode)
        .env("JW_FAULT_TARGET", &target)
        .env("JW_FAULT_ARM", root.0.join("arm"))
        .env("JW_FAULT_HIT", root.0.join("hit"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let started = Instant::now();
    while child.try_wait().unwrap().is_none() {
        if started.elapsed() > Duration::from_secs(20) {
            let _ = child.kill();
            let _ = child.wait();
            panic!("fault child timed out: {adapter}/{mode}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        root.0.join("hit").exists(),
        "fault did not reach {adapter}/{mode}: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        output.status.code(),
        Some(if mode == "crash-after-write" { 86 } else { 0 }),
        "{adapter}/{mode}: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if mode == "crash-after-write" {
        // Verify in another executable, with no shim or inherited writer handle.
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "checkpoint_file::faults::restart_child",
                "--nocapture",
            ])
            .env("JW_FAULT_ROOT", &root.0)
            .env("JW_FAULT_ADAPTER", adapter)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn budget_kernel_write_failures_preserve_or_freeze_the_log() {
    for mode in ["write-zero", "write-partial"] {
        run("budget", mode);
    }
}
#[test]
fn budget_sync_errors_never_acknowledge_a_visible_candidate() {
    for mode in ["sync-before", "sync-after"] {
        run("budget", mode);
    }
}
#[test]
fn session_kernel_write_failures_latch_uncertainty() {
    for mode in ["write-zero", "write-partial"] {
        run("session", mode);
    }
}
#[test]
fn session_sync_errors_and_direct_retry_require_confirmation() {
    for mode in ["sync-before", "sync-after"] {
        run("session", mode);
    }
}
#[test]
fn complete_write_then_process_exit_requires_restart_confirmation() {
    for adapter in ["budget", "session"] {
        run(adapter, "crash-after-write");
    }
}

fn event() -> SessionEvent {
    SessionEvent {
        event_id: EventId::from("fault-event"),
        session_id: identity().session_id,
        turn_id: TurnId::from("fault-turn"),
        generation_id: None,
        message_id: None,
        seq: 0,
        kind: SessionEventKind::AssistantDelta {
            text: "confirmed only after sync".into(),
        },
    }
}
fn store(root: &Path) -> FileBudgetCheckpointStore {
    FileBudgetCheckpointStore::open(
        root.join("task.jsonl"),
        FileBudgetCheckpointConfig::default(),
    )
    .unwrap()
}
fn uncertain(result: Result<BudgetCheckpointCommit, BudgetCheckpointStoreError>) {
    assert!(matches!(
        result,
        Err(BudgetCheckpointStoreError::Storage {
            certainty: BudgetCheckpointCommitCertainty::Indeterminate,
            ..
        })
    ));
}
fn session_uncertain<T: std::fmt::Debug>(result: Result<T, SessionPersistenceError>) {
    assert!(
        matches!(
            result,
            Err(SessionPersistenceError::Io {
                certainty: CommitCertainty::Indeterminate,
                ..
            })
        ),
        "{result:?}"
    );
}

#[test]
fn fault_child() {
    let Ok(root) = std::env::var("JW_FAULT_ROOT") else {
        return;
    };
    let root = PathBuf::from(root);
    let mode = std::env::var("JW_FAULT_MODE").unwrap();
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        if std::env::var("JW_FAULT_ADAPTER").unwrap() == "budget" {
            File::create_new(root.join("task.jsonl")).unwrap();
            let store = store(&root);
            let base = image(1);
            let next = successor(base.clone());
            store.compare_exchange(0, &base).await.unwrap();
            let before = fs::read(root.join("task.jsonl")).unwrap();
            fs::write(root.join("arm"), "armed").unwrap();
            uncertain(store.compare_exchange(1, &next).await);
            if mode.starts_with("sync-") {
                uncertain(store.compare_exchange(1, &next).await);
                assert!(store.load(&identity()).await.is_err());
            }
            fs::remove_file(root.join("arm")).unwrap();
            let bytes = fs::read(root.join("task.jsonl")).unwrap();
            if mode == "write-partial" {
                assert_eq!(bytes.len(), before.len() + 17);
                assert!(matches!(
                    store.load(&identity()).await,
                    Err(BudgetCheckpointStoreError::Corrupt { .. })
                ));
                assert!(store.compare_exchange(1, &next).await.is_err());
            } else {
                let expected = if mode == "write-zero" {
                    base
                } else {
                    next.clone()
                };
                assert_eq!(store.load(&identity()).await.unwrap(), Some(expected));
                let retry = store.compare_exchange(1, &next).await.unwrap();
                assert_eq!(
                    retry,
                    if mode == "write-zero" {
                        BudgetCheckpointCommit::Committed
                    } else {
                        BudgetCheckpointCommit::ReplayedExact
                    }
                );
                assert_eq!(store.load(&identity()).await.unwrap(), Some(next));
                if mode.starts_with("sync-") {
                    assert_eq!(fs::read(root.join("task.jsonl")).unwrap(), bytes);
                }
            }
            store.close().await;
        } else {
            let persistence = JsonlSessionPersistence::new(&root);
            persistence.load(&identity().session_id).await.unwrap();
            fs::write(root.join("arm"), "armed").unwrap();
            session_uncertain(persistence.commit_durable(&event()).await);
            session_uncertain(persistence.load(&identity().session_id).await);
            session_uncertain(persistence.commit_durable(&event()).await);
            drop(persistence);
            if mode.starts_with("sync-") {
                let retry = JsonlSessionPersistence::new(&root);
                // No intervening load: exact retry must re-confirm durability.
                session_uncertain(retry.commit_durable(&event()).await);
            }
            fs::remove_file(root.join("arm")).unwrap();
            let retry = JsonlSessionPersistence::new(&root);
            if mode == "write-partial" {
                assert_eq!(
                    fs::metadata(session_file_path(&root, &identity().session_id))
                        .unwrap()
                        .len(),
                    17
                );
                assert!(retry.load(&identity().session_id).await.is_err());
                assert!(retry.commit_durable(&event()).await.is_err());
            } else {
                let result = retry.commit_durable(&event()).await.unwrap();
                assert_eq!(
                    result,
                    if mode == "write-zero" {
                        PersistAppendOutcome::Appended
                    } else {
                        PersistAppendOutcome::ReplayedExact
                    }
                );
                assert_eq!(
                    retry.load(&identity().session_id).await.unwrap(),
                    vec![event()]
                );
            }
        }
    });
}

#[test]
fn restart_child() {
    let Ok(root) = std::env::var("JW_FAULT_ROOT") else {
        return;
    };
    let root = PathBuf::from(root);
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        if std::env::var("JW_FAULT_ADAPTER").unwrap() == "budget" {
            let store = store(&root);
            let candidate = successor(image(1));
            assert_eq!(
                store.load(&identity()).await.unwrap(),
                Some(candidate.clone())
            );
            assert_eq!(
                store.compare_exchange(1, &candidate).await.unwrap(),
                BudgetCheckpointCommit::ReplayedExact
            );
        } else {
            let persistence = JsonlSessionPersistence::new(&root);
            assert_eq!(
                persistence.commit_durable(&event()).await.unwrap(),
                PersistAppendOutcome::ReplayedExact
            );
            assert_eq!(
                persistence.load(&identity().session_id).await.unwrap(),
                vec![event()]
            );
        }
    });
}
