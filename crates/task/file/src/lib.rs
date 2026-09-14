//! Opt-in local-file task snapshot storage, not execution ownership or automatic recovery.
//!
//! The host provisions one durable, regular file per Task binding before opening
//! the store. An empty existing file means no checkpoint; a missing file is an
//! error. Keep the file and its directory entry stable: no replacement, truncation,
//! unlink, or writers that bypass the OS lock. This is not a distributed store or
//! a sandbox for untrusted paths. Supported filesystems must honor exclusive file
//! locks and `sync_all`. OS/hardware durability and directory provisioning remain
//! deployment responsibilities.
//!
//! Each operation uses a fresh handle and a nonblocking exclusive OS lock through
//! validation, CAS and sync. Complete records are re-synced before load/retry
//! success; torn or invalid logs fail closed and are never repaired implicitly.
//! Operations require Tokio and run on its blocking pool. Once admitted, dropping
//! an awaiting future does not cancel its worker. Keep the runtime alive and call
//! [`FileTaskStateStore::close`] to drain accepted work.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use jingwei_task::{
    TaskCommitCertainty, TaskIdentity, TaskSnapshot, TaskStateError, TaskStateFuture,
    TaskStateStore, TaskStoreError, TaskWriteOutcome,
};
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

type Result<T> = std::result::Result<T, TaskStoreError>;

/// Finite per-store IO admission and on-disk JSONL limits. Record bytes include
/// the final newline. These are not a bound on the whole process's memory.
#[derive(Clone, Copy, Debug)]
pub struct FileTaskStateConfig {
    pub max_record_bytes: usize,
    pub max_log_bytes: usize,
    pub max_io_jobs: usize,
}

impl Default for FileTaskStateConfig {
    fn default() -> Self {
        Self {
            max_record_bytes: 1024 * 1024,
            max_log_bytes: 64 * 1024 * 1024,
            max_io_jobs: 4,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileTaskStateStatus {
    pub accepting: bool,
    pub in_flight: usize,
    pub max_io_jobs: usize,
}

struct Control {
    accepting: bool,
    in_flight: usize,
}

struct Inner {
    path: PathBuf,
    config: FileTaskStateConfig,
    control: Mutex<Control>,
    drained: Notify,
}

impl Inner {
    fn control(&self) -> MutexGuard<'_, Control> {
        // No caller code runs while holding this bookkeeping-only lock.
        self.control.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn admit(self: &Arc<Self>) -> Result<Permit> {
        let mut control = self.control();
        if !control.accepting {
            return Err(TaskStoreError::Closed);
        }
        if control.in_flight >= self.config.max_io_jobs {
            return Err(TaskStoreError::Busy);
        }
        control.in_flight += 1;
        Ok(Permit(Arc::clone(self)))
    }

    fn locked_file(&self) -> Result<LockedFile> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.path)
            .map_err(not_committed)?;
        if !file.metadata().map_err(not_committed)?.is_file() {
            return Err(not_committed("checkpoint path is not a regular file"));
        }
        file.try_lock().map_err(|error| match error {
            TryLockError::WouldBlock => TaskStoreError::Busy,
            TryLockError::Error(error) => not_committed(error),
        })?;
        Ok(LockedFile(file))
    }

    fn read_latest(
        &self,
        file: &mut File,
        identity: &TaskIdentity,
    ) -> Result<(Option<TaskSnapshot>, usize)> {
        let limit = self.config.max_log_bytes;
        if file.metadata().map_err(not_committed)?.len() > limit as u64 {
            return Err(exceeded("log bytes", limit));
        }
        let mut bytes = Vec::new();
        file.take(limit as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(not_committed)?;
        if bytes.len() > limit {
            return Err(exceeded("log bytes", limit));
        }
        let mut latest: Option<TaskSnapshot> = None;
        for (index, line) in bytes.split_inclusive(|byte| *byte == b'\n').enumerate() {
            let record = index as u64 + 1;
            if line.len() > self.config.max_record_bytes {
                return Err(exceeded("record bytes", self.config.max_record_bytes));
            }
            if line.last() != Some(&b'\n') {
                return Err(corrupt(
                    record,
                    "unterminated record; reconciliation required",
                ));
            }
            let entry: Record =
                serde_json::from_slice(line).map_err(|error| corrupt(record, error))?;
            if entry.version != 1 {
                return Err(corrupt(record, "unsupported checkpoint log version"));
            }
            validate_revision(entry.expected_revision, &entry.checkpoint)
                .map_err(|error| corrupt(record, error))?;
            let previous = latest.as_ref().map_or(0, TaskSnapshot::revision);
            if entry.expected_revision != previous {
                return Err(corrupt(record, "non-contiguous revision chain"));
            }
            if let Some(previous) = &latest
                && previous.identity() != entry.checkpoint.identity()
            {
                return Err(corrupt(record, "task binding changed inside log"));
            }
            entry
                .checkpoint
                .validate_successor(latest.as_ref())
                .map_err(|error| corrupt(record, error))?;
            latest = Some(entry.checkpoint);
        }
        if latest
            .as_ref()
            .is_some_and(|checkpoint| checkpoint.identity() != identity)
        {
            return Err(TaskStateError::IdentityMismatch.into());
        }
        Ok((latest, bytes.len()))
    }
}

struct Permit(Arc<Inner>);
impl Drop for Permit {
    fn drop(&mut self) {
        let mut control = self.0.control();
        control.in_flight -= 1;
        if control.in_flight == 0 {
            self.0.drained.notify_waiters();
        }
    }
}

/// Clones share admission/close state. Independent instances still coordinate
/// their file operations through the OS lock, but do not share job limits.
#[derive(Clone)]
pub struct FileTaskStateStore(Arc<Inner>);

impl FileTaskStateStore {
    /// Synchronously resolve an existing host-provisioned file. Does not create,
    /// truncate, validate log contents or acquire execution ownership.
    pub fn open(path: impl AsRef<Path>, config: FileTaskStateConfig) -> Result<Self> {
        if config.max_record_bytes == 0
            || config.max_log_bytes < config.max_record_bytes
            || config.max_log_bytes == usize::MAX
            || config.max_io_jobs == 0
        {
            return Err(not_committed("invalid file checkpoint limits"));
        }
        let path = path.as_ref().canonicalize().map_err(not_committed)?;
        if !path.metadata().map_err(not_committed)?.is_file() {
            return Err(not_committed("checkpoint path is not a regular file"));
        }
        Ok(Self(Arc::new(Inner {
            path,
            config,
            control: Mutex::new(Control {
                accepting: true,
                in_flight: 0,
            }),
            drained: Notify::new(),
        })))
    }

    pub fn status(&self) -> FileTaskStateStatus {
        let control = self.0.control();
        FileTaskStateStatus {
            accepting: control.accepting,
            in_flight: control.in_flight,
            max_io_jobs: self.0.config.max_io_jobs,
        }
    }

    /// Stop admission for all clones and wait for every accepted worker, including
    /// those whose callers departed. Dropping this waiter does not reopen storage.
    /// A drained store is not proof that every write succeeded; reconcile any
    /// unknown outcomes with a newly opened store before granting execution.
    pub async fn close(&self) {
        self.0.control().accepting = false;
        loop {
            let notified = self.0.drained.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.0.control().in_flight == 0 {
                return;
            }
            notified.await;
        }
    }

    async fn run<T: Send + 'static>(
        &self,
        work: impl FnOnce(&Inner) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let runtime = tokio::runtime::Handle::try_current().map_err(not_committed)?;
        let permit = self.0.admit()?;
        runtime
            .spawn_blocking(move || {
                let _permit = permit;
                work(&_permit.0)
            })
            .await
            .map_err(indeterminate)?
    }
}

impl TaskStateStore for FileTaskStateStore {
    fn load<'a>(
        &'a self,
        identity: &'a TaskIdentity,
    ) -> TaskStateFuture<'a, Result<Option<TaskSnapshot>>> {
        Box::pin(async move {
            validate_identity(identity)?;
            let identity = identity.clone();
            self.run(move |inner| {
                let mut file = inner.locked_file()?;
                let (latest, _) = inner.read_latest(&mut file, &identity)?;
                // Confirm even a complete record left by an unacknowledged writer.
                file.sync_all().map_err(indeterminate)?;
                Ok(latest)
            })
            .await
        })
    }

    fn compare_exchange<'a>(
        &'a self,
        expected_revision: u64,
        checkpoint: &'a TaskSnapshot,
    ) -> TaskStateFuture<'a, Result<TaskWriteOutcome>> {
        Box::pin(async move {
            checkpoint.validate()?;
            let checkpoint = checkpoint.clone();
            self.run(move |inner| {
                validate_revision(expected_revision, &checkpoint)?;
                let mut file = inner.locked_file()?;
                let (latest, log_bytes) = inner.read_latest(&mut file, checkpoint.identity())?;
                if latest.as_ref() == Some(&checkpoint) {
                    file.sync_all().map_err(indeterminate)?;
                    return Ok(TaskWriteOutcome::AlreadyPresent);
                }
                let actual = latest.as_ref().map_or(0, TaskSnapshot::revision);
                if actual != expected_revision {
                    return Err(TaskStoreError::Conflict {
                        expected: expected_revision,
                        actual,
                    });
                }
                checkpoint.validate_successor(latest.as_ref())?;
                let mut encoded = BoundedBytes {
                    bytes: Vec::new(),
                    limit: inner.config.max_record_bytes,
                };
                serde_json::to_writer(
                    &mut encoded,
                    &Record {
                        version: 1,
                        expected_revision,
                        checkpoint,
                    },
                )
                .map_err(|_| exceeded("record bytes", encoded.limit))?;
                encoded
                    .write_all(b"\n")
                    .map_err(|_| exceeded("record bytes", encoded.limit))?;
                if log_bytes
                    .checked_add(encoded.bytes.len())
                    .is_none_or(|total| total > inner.config.max_log_bytes)
                {
                    return Err(exceeded("log bytes", inner.config.max_log_bytes));
                }
                file.seek(SeekFrom::End(0)).map_err(not_committed)?;
                // From the first write onward, errors must not claim rollback.
                file.write_all(&encoded.bytes).map_err(indeterminate)?;
                file.sync_all().map_err(indeterminate)?;
                Ok(TaskWriteOutcome::Applied)
            })
            .await
        })
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    version: u16,
    expected_revision: u64,
    checkpoint: TaskSnapshot,
}

struct BoundedBytes {
    bytes: Vec<u8>,
    limit: usize,
}
impl Write for BoundedBytes {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self
            .bytes
            .len()
            .checked_add(bytes.len())
            .is_none_or(|len| len > self.limit)
        {
            return Err(io::Error::other("checkpoint record exceeds byte limit"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn exceeded(resource: &'static str, limit: usize) -> TaskStoreError {
    TaskStoreError::LimitExceeded {
        resource,
        limit: limit as u64,
    }
}
fn corrupt(record: u64, message: impl ToString) -> TaskStoreError {
    TaskStoreError::Corrupt {
        record,
        message: message.to_string(),
    }
}
fn not_committed(error: impl ToString) -> TaskStoreError {
    TaskStoreError::Storage {
        certainty: TaskCommitCertainty::DefinitelyNotCommitted,
        message: error.to_string(),
    }
}
fn indeterminate(error: impl ToString) -> TaskStoreError {
    TaskStoreError::Storage {
        certainty: TaskCommitCertainty::Indeterminate,
        message: error.to_string(),
    }
}

// Explicit unlock also releases an inherited lock if another thread forks a
// process before this file handle closes. The parent operation has ended.
struct LockedFile(File);
impl std::ops::Deref for LockedFile {
    type Target = File;
    fn deref(&self) -> &File {
        &self.0
    }
}
impl std::ops::DerefMut for LockedFile {
    fn deref_mut(&mut self) -> &mut File {
        &mut self.0
    }
}
impl Drop for LockedFile {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

fn validate_revision(expected: u64, snapshot: &TaskSnapshot) -> Result<()> {
    snapshot.validate()?;
    let next = expected
        .checked_add(1)
        .ok_or(TaskStateError::RevisionExhausted)?;
    if snapshot.revision() != next {
        return Err(TaskStateError::RevisionMismatch {
            expected: next,
            actual: snapshot.revision(),
        }
        .into());
    }
    Ok(())
}
fn validate_identity(identity: &TaskIdentity) -> Result<()> {
    for value in [
        identity.task_id.as_str(),
        identity.session_id.as_str(),
        &identity.agent_key,
    ] {
        if value.trim().is_empty() || value.len() > 4096 {
            return Err(TaskStateError::Invalid("invalid task identity").into());
        }
    }
    Ok(())
}
