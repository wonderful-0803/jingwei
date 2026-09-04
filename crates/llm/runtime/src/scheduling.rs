//! Admission order and execution capacity, separate from JobGuard closure ownership.

use std::io::{self, Write};

use jingwei_llm::{ModelJobReport, ModelSchedulerSnapshot};
use serde::Serialize;

use super::*;

pub(super) struct ScheduledJob {
    pub cancellation: Arc<JobCancellation>,
    call_id: ModelCallId,
    phase: ModelJobPhase,
    admitted_at: Instant,
    slot_at: Option<Instant>,
    started_at: Option<Instant>,
    closed_at: Option<Instant>,
    deadline: Instant,
    timeout: Duration,
    recording: Option<ModelRecordStage>,
    request_recorded: bool,
    result_recorded: bool,
    stop_reason: Option<ModelJobStopReason>,
}

impl ScheduledJob {
    pub fn new(
        call_id: ModelCallId,
        cancellation: Arc<JobCancellation>,
        now: Instant,
        deadline: Instant,
        timeout: Duration,
        dispatched: bool,
    ) -> Self {
        Self {
            cancellation,
            call_id,
            phase: if dispatched {
                ModelJobPhase::Preparing
            } else {
                ModelJobPhase::Queued
            },
            admitted_at: now,
            slot_at: dispatched.then_some(now),
            started_at: None,
            closed_at: None,
            deadline,
            timeout,
            recording: None,
            request_recorded: false,
            result_recorded: false,
            stop_reason: None,
        }
    }

    fn report(&self, now: Instant) -> ModelJobReport {
        let end = self.closed_at.unwrap_or(now);
        ModelJobReport {
            call_id: self.call_id.clone(),
            phase: self.phase,
            recording: self.recording,
            request_recorded: self.request_recorded,
            result_recorded: self.result_recorded,
            stop_reason: self.stop_reason,
            timeout: self.timeout,
            remaining_time: self.deadline.saturating_duration_since(now),
            queue_time: self.slot_at.unwrap_or(end).duration_since(self.admitted_at),
            preparation_time: self.slot_at.map_or(Duration::ZERO, |at| {
                self.started_at.unwrap_or(end).duration_since(at)
            }),
            execution_time: self
                .started_at
                .map_or(Duration::ZERO, |at| end.duration_since(at)),
            cleanup_time: self
                .closed_at
                .map_or(Duration::ZERO, |at| now.duration_since(at)),
        }
    }
}

impl RuntimeState {
    pub(super) fn scheduler_snapshot(&self) -> ModelSchedulerSnapshot {
        let control = self.lock_control();
        let now = Instant::now();
        let jobs: Vec<_> = control.jobs.values().map(|job| job.report(now)).collect();
        let count = |phase| jobs.iter().filter(|job| job.phase == phase).count();
        ModelSchedulerSnapshot {
            config: self.scheduler,
            accepting: control.lifecycle == LifecycleState::Running,
            inflight: jobs.len(),
            queued: count(ModelJobPhase::Queued),
            preparing: count(ModelJobPhase::Preparing),
            executing: count(ModelJobPhase::Executing),
            cleaning: count(ModelJobPhase::Cleaning),
            jobs,
        }
    }

    /// Idempotent and also used by JobGuard::Drop. Does not remove accepted ownership.
    pub(super) fn release_execution(&self, id: u64, reason: Option<ModelJobStopReason>) {
        let mut control = self.lock_control();
        let Some(job) = control.jobs.get_mut(&id) else {
            return;
        };
        if job.stop_reason.is_none() {
            job.stop_reason = reason;
        }
        if job.phase == ModelJobPhase::Cleaning {
            return;
        }
        let queued = job.phase == ModelJobPhase::Queued;
        job.phase = ModelJobPhase::Cleaning;
        job.closed_at = Some(Instant::now());
        job.cancellation.dispatched.notify_one();
        if queued {
            control.queue.retain(|queued_id| *queued_id != id);
        } else {
            control.occupied -= 1;
        }
        self.dispatch_waiters(&mut control);
    }

    fn dispatch_waiters(&self, control: &mut RuntimeControl) {
        while control.lifecycle == LifecycleState::Running
            && control.occupied < self.scheduler.max_concurrency
        {
            let Some(id) = control.queue.pop_front() else {
                break;
            };
            let Some(job) = control.jobs.get_mut(&id) else {
                continue;
            };
            let now = Instant::now();
            // Expired/cancelled queue entries never acquire a slot, even if their
            // driver is still awaiting the original request-recording future.
            let stop = if job.cancellation.token.is_cancelled() {
                Some(ModelJobStopReason::Cancelled)
            } else if now >= job.deadline {
                Some(ModelJobStopReason::QueueTimeout)
            } else {
                None
            };
            if let Some(stop) = stop {
                job.phase = ModelJobPhase::Cleaning;
                job.stop_reason = Some(stop);
                job.closed_at = Some(now);
            } else {
                job.phase = ModelJobPhase::Preparing;
                job.slot_at = Some(now);
                control.occupied += 1;
            }
            job.cancellation.dispatched.notify_one();
        }
    }
}

pub(super) struct JobExecution {
    pub turn: Arc<TurnState>,
    pub job_id: u64,
    pub deadline: Instant,
    pub cancellation: Arc<dyn CancellationSignal>,
    pub dispatch: Arc<JobCancellation>,
}

impl JobExecution {
    fn interruption_now(
        &self,
        deltas: Option<&mpsc::Sender<GenerationDelta>>,
    ) -> Option<ModelJobStopReason> {
        if let Some(reason) = self
            .turn
            .runtime
            .lock_control()
            .jobs
            .get(&self.job_id)
            .and_then(|job| job.stop_reason)
        {
            return Some(reason);
        }
        if self.cancellation.is_cancelled() {
            return Some(ModelJobStopReason::Cancelled);
        }
        if Instant::now() >= self.deadline {
            let queued = self
                .turn
                .runtime
                .lock_control()
                .jobs
                .get(&self.job_id)
                .is_some_and(|job| job.phase == ModelJobPhase::Queued);
            return Some(if queued {
                ModelJobStopReason::QueueTimeout
            } else {
                ModelJobStopReason::Timeout
            });
        }
        if deltas.is_some_and(mpsc::Sender::is_closed) {
            return Some(ModelJobStopReason::ConsumerDropped);
        }
        None
    }

    async fn interrupted(
        &self,
        deltas: Option<&mpsc::Sender<GenerationDelta>>,
    ) -> ModelJobStopReason {
        let closed = async {
            if let Some(deltas) = deltas {
                deltas.closed().await;
            } else {
                std::future::pending::<()>().await;
            }
        };
        tokio::select! {
            biased;
            _ = self.cancellation.cancelled() => ModelJobStopReason::Cancelled,
            _ = tokio::time::sleep_until(self.deadline) => self.interruption_now(deltas).unwrap_or(ModelJobStopReason::Timeout),
            _ = closed => ModelJobStopReason::ConsumerDropped,
        }
    }

    fn recording(&self, stage: Option<ModelRecordStage>, confirmed: bool) {
        if let Some(job) = self.turn.runtime.lock_control().jobs.get_mut(&self.job_id) {
            if confirmed {
                match job.recording {
                    Some(ModelRecordStage::Request) => job.request_recorded = true,
                    Some(ModelRecordStage::Result) => job.result_recorded = true,
                    None => {}
                }
            }
            job.recording = stage;
        }
    }

    /// Cancellation releases execution/queue capacity, never the accepted append future.
    pub async fn record_request(
        &self,
        request: &ModelRequest,
        deltas: Option<&mpsc::Sender<GenerationDelta>>,
    ) -> Result<Option<ModelJobStopReason>, ModelGatewayError> {
        self.recording(Some(ModelRecordStage::Request), false);
        let append = append_request(&self.turn, request);
        tokio::pin!(append);
        let (result, stop) = tokio::select! {
            biased;
            stop = self.interrupted(deltas) => {
                self.release(Some(stop));
                (append.await, Some(stop))
            }
            result = &mut append => (result, None),
        };
        self.recording(None, result.is_ok());
        result?;
        Ok(stop.or_else(|| self.interruption_now(deltas)))
    }

    pub async fn record_result(
        &self,
        request: &ModelRequest,
        result: &ModelResult,
    ) -> Result<(), ModelGatewayError> {
        self.recording(Some(ModelRecordStage::Result), false);
        let output = append_result(&self.turn, request, result).await;
        self.recording(None, output.is_ok());
        output
    }

    pub async fn wait_for_slot(
        &self,
        deltas: Option<&mpsc::Sender<GenerationDelta>>,
    ) -> Result<(), ModelJobStopReason> {
        loop {
            let notified = self.dispatch.dispatched.notified();
            if let Some(stop) = self.interruption_now(deltas) {
                self.release(Some(stop));
                return Err(stop);
            }
            if self
                .turn
                .runtime
                .lock_control()
                .jobs
                .get(&self.job_id)
                .is_some_and(|job| job.phase == ModelJobPhase::Preparing)
            {
                return Ok(());
            }
            tokio::select! {
                biased;
                stop = self.interrupted(deltas) => { self.release(Some(stop)); return Err(stop); }
                _ = notified => {}
            }
        }
    }

    /// Linearizes raw provider entry against runtime shutdown after preflight.
    pub fn start_provider(
        &self,
        deltas: Option<&mpsc::Sender<GenerationDelta>>,
    ) -> Result<(), ModelJobStopReason> {
        if let Some(stop) = self.interruption_now(deltas) {
            return Err(stop);
        }
        let mut control = self.turn.runtime.lock_control();
        if control.lifecycle != LifecycleState::Running || self.dispatch.token.is_cancelled() {
            return Err(ModelJobStopReason::Cancelled);
        }
        let job = control
            .jobs
            .get_mut(&self.job_id)
            .expect("admitted driver retains its job");
        if job.phase != ModelJobPhase::Preparing {
            return Err(job.stop_reason.unwrap_or(ModelJobStopReason::Failed));
        }
        let now = Instant::now();
        if now >= job.deadline {
            return Err(ModelJobStopReason::Timeout);
        }
        job.phase = ModelJobPhase::Executing;
        job.started_at = Some(now);
        Ok(())
    }

    pub fn release(&self, reason: Option<ModelJobStopReason>) {
        self.turn.runtime.release_execution(self.job_id, reason);
    }
}

// Match ModelRequestOptions' JSON without cloning an arbitrarily large context
// merely to reject it. Count with a bounded writer, never a full JSON buffer.
#[derive(Serialize)]
struct BorrowedOptions<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    context: &'a Option<jingwei_core::DecisionContext>,
    max_tokens: Option<u32>,
    timeout: Option<ModelTimeout>,
    limits: jingwei_llm::GenerationLimits,
}

struct SizeLimitWriter {
    remaining: usize,
}

impl Write for SizeLimitWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.remaining = self
            .remaining
            .checked_sub(bytes.len())
            .ok_or_else(|| io::Error::other("model request size limit exceeded"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) fn validate_request_size(
    request: &GenerationRequest,
    options: &GenerationOptions,
    limit_bytes: usize,
) -> Result<(), ModelGatewayError> {
    let mut counter = SizeLimitWriter {
        remaining: limit_bytes,
    };
    let options = BorrowedOptions {
        context: &options.context,
        max_tokens: options.max_tokens,
        timeout: options.timeout.map(ModelTimeout::from_duration),
        limits: options.limits,
    };
    serde_json::to_writer(&mut counter, request)
        .and_then(|()| serde_json::to_writer(&mut counter, &options))
        .map_err(|_| ModelRuntimeError::RequestTooLarge { limit_bytes }.into())
}
