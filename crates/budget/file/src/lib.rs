//! Opt-in local-file checkpoint storage, not execution ownership or automatic recovery.
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
//! [`FileBudgetCheckpointStore::close`] to drain accepted work.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use jingwei_budget::{
    BudgetCheckpoint, BudgetCheckpointCommit, BudgetCheckpointCommitCertainty,
    BudgetCheckpointError, BudgetCheckpointFuture, BudgetCheckpointStore,
    BudgetCheckpointStoreError, BudgetIdentity,
};
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

type Result<T> = std::result::Result<T, BudgetCheckpointStoreError>;

/// Finite per-store IO admission and on-disk JSONL limits. Record bytes include
/// the final newline. These are not a bound on the whole process's memory.
#[derive(Clone, Copy, Debug)]
pub struct FileBudgetCheckpointConfig {
    pub max_record_bytes: usize,
    pub max_log_bytes: usize,
    pub max_io_jobs: usize,
}

impl Default for FileBudgetCheckpointConfig {
    fn default() -> Self {
        Self {
            max_record_bytes: 8 * 1024 * 1024,
            max_log_bytes: 64 * 1024 * 1024,
            max_io_jobs: 4,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileBudgetCheckpointStatus {
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
    config: FileBudgetCheckpointConfig,
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
            return Err(BudgetCheckpointStoreError::Closed);
        }
        if control.in_flight >= self.config.max_io_jobs {
            return Err(BudgetCheckpointStoreError::Busy);
        }
        control.in_flight += 1;
        Ok(Permit(Arc::clone(self)))
    }

    fn locked_file(&self) -> Result<File> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.path)
            .map_err(not_committed)?;
        if !file.metadata().map_err(not_committed)?.is_file() {
            return Err(not_committed("checkpoint path is not a regular file"));
        }
        file.try_lock().map_err(|error| match error {
            TryLockError::WouldBlock => BudgetCheckpointStoreError::Busy,
            TryLockError::Error(error) => not_committed(error),
        })?;
        Ok(file)
    }

    fn read_latest(
        &self,
        file: &mut File,
        identity: &BudgetIdentity,
    ) -> Result<(Option<BudgetCheckpoint>, usize)> {
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
        let mut latest: Option<BudgetCheckpoint> = None;
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
            entry
                .checkpoint
                .validate_successor(entry.expected_revision)
                .map_err(|error| corrupt(record, error))?;
            let previous = latest.as_ref().map_or(0, BudgetCheckpoint::revision);
            if entry.expected_revision != previous {
                return Err(corrupt(record, "non-contiguous revision chain"));
            }
            if let Some(previous) = &latest
                && previous.identity() != entry.checkpoint.identity()
            {
                return Err(corrupt(record, "task binding changed inside log"));
            }
            latest = Some(entry.checkpoint);
        }
        if latest
            .as_ref()
            .is_some_and(|checkpoint| checkpoint.identity() != identity)
        {
            return Err(BudgetCheckpointError::IdentityMismatch.into());
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
pub struct FileBudgetCheckpointStore(Arc<Inner>);

impl FileBudgetCheckpointStore {
    /// Synchronously resolve an existing host-provisioned file. Does not create,
    /// truncate, validate log contents or acquire execution ownership.
    pub fn open(path: impl AsRef<Path>, config: FileBudgetCheckpointConfig) -> Result<Self> {
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

    pub fn status(&self) -> FileBudgetCheckpointStatus {
        let control = self.0.control();
        FileBudgetCheckpointStatus {
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

impl BudgetCheckpointStore for FileBudgetCheckpointStore {
    fn load<'a>(
        &'a self,
        identity: &'a BudgetIdentity,
    ) -> BudgetCheckpointFuture<'a, Result<Option<BudgetCheckpoint>>> {
        Box::pin(async move {
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
        checkpoint: &'a BudgetCheckpoint,
    ) -> BudgetCheckpointFuture<'a, Result<BudgetCheckpointCommit>> {
        Box::pin(async move {
            let checkpoint = checkpoint.clone();
            self.run(move |inner| {
                checkpoint.validate_successor(expected_revision)?;
                let mut file = inner.locked_file()?;
                let (latest, log_bytes) = inner.read_latest(&mut file, checkpoint.identity())?;
                if latest.as_ref() == Some(&checkpoint) {
                    file.sync_all().map_err(indeterminate)?;
                    return Ok(BudgetCheckpointCommit::ReplayedExact);
                }
                let actual = latest.as_ref().map_or(0, BudgetCheckpoint::revision);
                if actual != expected_revision {
                    return Err(BudgetCheckpointStoreError::Conflict {
                        expected: expected_revision,
                        actual,
                    });
                }
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
                Ok(BudgetCheckpointCommit::Committed)
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
    checkpoint: BudgetCheckpoint,
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

fn exceeded(resource: &'static str, limit: usize) -> BudgetCheckpointStoreError {
    BudgetCheckpointStoreError::LimitExceeded {
        resource,
        limit: limit as u64,
    }
}
fn corrupt(record: u64, message: impl ToString) -> BudgetCheckpointStoreError {
    BudgetCheckpointStoreError::Corrupt {
        record,
        message: message.to_string(),
    }
}
fn not_committed(error: impl ToString) -> BudgetCheckpointStoreError {
    BudgetCheckpointStoreError::Storage {
        certainty: BudgetCheckpointCommitCertainty::DefinitelyNotCommitted,
        message: error.to_string(),
    }
}
fn indeterminate(error: impl ToString) -> BudgetCheckpointStoreError {
    BudgetCheckpointStoreError::Storage {
        certainty: BudgetCheckpointCommitCertainty::Indeterminate,
        message: error.to_string(),
    }
}
