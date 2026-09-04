//! Canonical controlled implementation of Jingwei's ToolRuntime contract.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use futures::FutureExt;
use jingwei_budget::{
    BudgetAmounts, BudgetClock, BudgetError, BudgetEventKind, BudgetIdentity, BudgetLimits,
    BudgetRequest, BudgetReservation, BudgetRun, BudgetScope, BudgetStopReason, BudgetUsage,
    TaskBudget, TokenBudgetMode, UsageValue,
};
use jingwei_core::{CancellationFuture, CancellationSignal, ToolFailureCategory};
use jingwei_plugin::{
    FactoryContext, LifecycleFuture, ManagedService, MountContext, MountError, Plugin,
    PluginDescriptor, PluginId, RuntimeError, ServiceFactory, ServiceLifecycle, StopReason,
    ToolAuthorizerBinding, ToolBinding, ToolGuardBinding,
};
use jingwei_tool::{
    MAX_TOOL_ARGUMENT_BYTES, MAX_TOOL_NAME_BYTES, TOOL_RUNTIME, Tool, ToolAuthorizationDecision,
    ToolAuthorizationRequest, ToolAuthorizer, ToolBodyRequest, ToolCall, ToolCallOptions,
    ToolClosureFailure, ToolEventRecorder, ToolExecution, ToolFinishMode, ToolFuture, ToolGateway,
    ToolGuard, ToolGuardRequest, ToolMetadata, ToolPreflightError, ToolRecord, ToolRecordError,
    ToolRecordedOutcome, ToolResult, ToolRuntime, ToolRuntimeError, ToolSchema, ToolTurn,
    ToolTurnBinding, ToolTurnFailure,
};
use jsonschema::Validator;
use tokio::runtime::Handle;
use tokio::sync::{Notify, oneshot};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

const DEFAULT_TOOL_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_TOOL_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_TOOL_DIAGNOSTIC_BYTES: usize = 1024;
const DEFAULT_MAX_IN_FLIGHT: usize = 1024;
const MAX_LIFECYCLE_FAILURES: usize = 32;

struct ExecutorBudgetClock(Instant);

impl BudgetClock for ExecutorBudgetClock {
    fn now(&self) -> Duration {
        self.0.elapsed()
    }
}

/// Explicit canonical ToolRuntime provider plugin.
#[derive(Clone)]
pub struct CanonicalToolRuntimePlugin {
    grants: BTreeMap<String, GrantSpec>,
    default_timeout: Duration,
    max_output_bytes: usize,
    max_in_flight: usize,
}

impl CanonicalToolRuntimePlugin {
    pub fn new() -> Self {
        Self {
            grants: BTreeMap::new(),
            default_timeout: DEFAULT_TOOL_TIMEOUT,
            max_output_bytes: DEFAULT_TOOL_OUTPUT_BYTES,
            max_in_flight: DEFAULT_MAX_IN_FLIGHT,
        }
    }

    /// Grant every frozen Tool to one plugin owner.
    #[must_use]
    pub fn grant_all(mut self, owner: PluginId) -> Self {
        self.grants
            .insert(owner.as_str().to_string(), GrantSpec::All);
        self
    }

    /// Grant one named Tool to one plugin owner.
    #[must_use]
    pub fn grant_tool(mut self, owner: PluginId, tool: impl Into<String>) -> Self {
        let tool = tool.into();
        match self.grants.entry(owner.as_str().to_string()).or_default() {
            GrantSpec::All => {}
            GrantSpec::Named(keys) => {
                keys.insert(tool);
            }
        }
        self
    }

    #[must_use]
    pub const fn with_default_timeout(mut self, timeout: Duration) -> Self {
        self.default_timeout = timeout;
        self
    }

    #[must_use]
    pub const fn with_max_output_bytes(mut self, max_output_bytes: usize) -> Self {
        self.max_output_bytes = max_output_bytes;
        self
    }

    #[must_use]
    pub const fn with_max_in_flight(mut self, max_in_flight: usize) -> Self {
        self.max_in_flight = max_in_flight;
        self
    }
}

impl Default for CanonicalToolRuntimePlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for CanonicalToolRuntimePlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor::provider("jingwei-tool-runtime-canonical", TOOL_RUNTIME, "canonical")
    }

    fn mount(&self, ctx: &mut MountContext<'_>) -> Result<(), MountError> {
        ctx.provide_tool_runtime_factory(Arc::new(CanonicalToolRuntimeFactory {
            grants: self.grants.clone(),
            default_timeout: self.default_timeout,
            max_output_bytes: self.max_output_bytes,
            max_in_flight: self.max_in_flight,
        }))
    }
}

#[derive(Clone)]
enum GrantSpec {
    Named(BTreeSet<String>),
    All,
}

impl Default for GrantSpec {
    fn default() -> Self {
        Self::Named(BTreeSet::new())
    }
}

struct CanonicalToolRuntimeFactory {
    grants: BTreeMap<String, GrantSpec>,
    default_timeout: Duration,
    max_output_bytes: usize,
    max_in_flight: usize,
}

impl ServiceFactory<dyn ToolRuntime> for CanonicalToolRuntimeFactory {
    fn construct<'a>(
        &'a self,
        ctx: FactoryContext<'a>,
    ) -> LifecycleFuture<'a, Result<ManagedService<dyn ToolRuntime>, RuntimeError>> {
        Box::pin(async move {
            if self.max_in_flight == 0 {
                return Err(RuntimeError::new(
                    "canonical ToolRuntime max_in_flight must be greater than zero",
                ));
            }
            if Instant::now().checked_add(self.default_timeout).is_none() {
                return Err(RuntimeError::new(
                    "canonical ToolRuntime default timeout exceeds the executor clock range",
                ));
            }
            let tool_bindings = ctx.tool_bindings().ok_or_else(|| {
                RuntimeError::new("Tool bindings are hidden from the ToolRuntime factory")
            })?;
            let guard_bindings = ctx.tool_guard_bindings().ok_or_else(|| {
                RuntimeError::new("Tool guard bindings are hidden from the ToolRuntime factory")
            })?;
            let authorizer_bindings = ctx.tool_authorizer_bindings().ok_or_else(|| {
                RuntimeError::new(
                    "Tool authorizer bindings are hidden from the ToolRuntime factory",
                )
            })?;
            let executor = Handle::try_current().map_err(|error| {
                RuntimeError::new(format!("ToolRuntime requires a Tokio executor: {error}"))
            })?;
            let authorizers = freeze_authorizers(authorizer_bindings);
            let tools = freeze_tools(tool_bindings, &authorizers)?;
            let guards = freeze_guards(guard_bindings);
            let grants = expand_grants(&self.grants, &tools)?;
            let state = Arc::new(RuntimeState::new(
                tools,
                guards,
                authorizers,
                grants,
                executor,
                self.default_timeout,
                self.max_output_bytes,
                self.max_in_flight,
            ));
            let runtime: Arc<dyn ToolRuntime> = Arc::new(CanonicalToolRuntime {
                state: Arc::clone(&state),
            });
            Ok(ManagedService::new(
                runtime,
                Box::new(ToolRuntimeLifecycle { state }),
            ))
        })
    }
}

struct ToolEntry {
    tool: Arc<dyn Tool>,
    metadata: ToolMetadata,
    validator: Validator,
}

fn freeze_tools(
    bindings: Vec<ToolBinding>,
    authorizers: &BTreeMap<String, Arc<dyn ToolAuthorizer>>,
) -> Result<BTreeMap<String, Arc<ToolEntry>>, RuntimeError> {
    let mut tools = BTreeMap::new();
    for binding in bindings {
        let key = binding.key().to_string();
        if key.is_empty() || key.len() > MAX_TOOL_NAME_BYTES {
            return Err(RuntimeError::new(format!(
                "Tool `{key}` from plugin `{}` has an invalid registration key",
                binding.owner()
            )));
        }
        let tool = binding.tool();
        let metadata = catch_unwind(AssertUnwindSafe(|| tool.metadata())).map_err(|_| {
            RuntimeError::new(format!(
                "Tool `{key}` from plugin `{}` panicked while providing metadata",
                binding.owner()
            ))
        })?;
        validate_local_schema(&key, metadata.input_schema())?;
        if let Some(authorizer) = metadata.approval().key()
            && !authorizers.contains_key(authorizer)
        {
            return Err(RuntimeError::new(format!(
                "Tool `{key}` requires missing authorizer `{authorizer}`"
            )));
        }
        let validator = catch_unwind(AssertUnwindSafe(|| {
            jsonschema::draft202012::new(metadata.input_schema())
        }))
        .map_err(|_| RuntimeError::new(format!("Tool `{key}` schema compiler panicked")))?
        .map_err(|error| {
            RuntimeError::new(format!("Tool `{key}` schema cannot be compiled: {error}"))
        })?;
        tools.insert(
            key,
            Arc::new(ToolEntry {
                tool,
                metadata,
                validator,
            }),
        );
    }
    Ok(tools)
}

fn validate_local_schema(key: &str, schema: &serde_json::Value) -> Result<(), RuntimeError> {
    catch_unwind(AssertUnwindSafe(|| {
        jsonschema::draft202012::meta::validate(schema)
    }))
    .map_err(|_| RuntimeError::new(format!("Tool `{key}` schema meta-validation panicked")))?
    .map_err(|error| {
        RuntimeError::new(format!(
            "Tool `{key}` schema is not valid Draft 2020-12: {error}"
        ))
    })
}

fn freeze_guards(mut bindings: Vec<ToolGuardBinding>) -> Vec<Arc<dyn ToolGuard>> {
    bindings.sort_by_key(ToolGuardBinding::ordinal);
    bindings
        .into_iter()
        .map(|binding| binding.guard())
        .collect()
}

fn freeze_authorizers(
    bindings: Vec<ToolAuthorizerBinding>,
) -> BTreeMap<String, Arc<dyn ToolAuthorizer>> {
    bindings
        .into_iter()
        .map(|binding| (binding.key().to_string(), binding.authorizer()))
        .collect()
}

fn expand_grants(
    configured: &BTreeMap<String, GrantSpec>,
    tools: &BTreeMap<String, Arc<ToolEntry>>,
) -> Result<BTreeMap<String, Arc<BTreeSet<String>>>, RuntimeError> {
    let all = tools.keys().cloned().collect::<BTreeSet<_>>();
    let mut grants = BTreeMap::new();
    for (owner, grant) in configured {
        let keys = match grant {
            GrantSpec::All => all.clone(),
            GrantSpec::Named(keys) => {
                if let Some(missing) = keys.iter().find(|key| !tools.contains_key(*key)) {
                    return Err(RuntimeError::new(format!(
                        "ToolRuntime grant for owner `{owner}` references missing Tool `{missing}`"
                    )));
                }
                keys.clone()
            }
        };
        grants.insert(owner.clone(), Arc::new(keys));
    }
    Ok(grants)
}

struct CanonicalToolRuntime {
    state: Arc<RuntimeState>,
}

impl ToolRuntime for CanonicalToolRuntime {
    fn bind_turn(&self, binding: ToolTurnBinding) -> Result<Box<dyn ToolTurn>, ToolRuntimeError> {
        self.state.bind_turn(binding)
    }
}

struct RuntimeState {
    tools: BTreeMap<String, Arc<ToolEntry>>,
    guards: Vec<Arc<dyn ToolGuard>>,
    authorizers: BTreeMap<String, Arc<dyn ToolAuthorizer>>,
    grants: BTreeMap<String, Arc<BTreeSet<String>>>,
    executor: Handle,
    default_timeout: Duration,
    max_output_bytes: usize,
    max_in_flight: usize,
    control: Mutex<RuntimeControl>,
    drained: Notify,
}

impl RuntimeState {
    #[allow(clippy::too_many_arguments)]
    fn new(
        tools: BTreeMap<String, Arc<ToolEntry>>,
        guards: Vec<Arc<dyn ToolGuard>>,
        authorizers: BTreeMap<String, Arc<dyn ToolAuthorizer>>,
        grants: BTreeMap<String, Arc<BTreeSet<String>>>,
        executor: Handle,
        default_timeout: Duration,
        max_output_bytes: usize,
        max_in_flight: usize,
    ) -> Self {
        Self {
            tools,
            guards,
            authorizers,
            grants,
            executor,
            default_timeout,
            max_output_bytes,
            max_in_flight,
            control: Mutex::new(RuntimeControl::new()),
            drained: Notify::new(),
        }
    }

    fn lock_control(&self) -> MutexGuard<'_, RuntimeControl> {
        self.control
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn start(&self) -> Result<(), RuntimeError> {
        let mut control = self.lock_control();
        if control.lifecycle != LifecycleState::Constructed {
            return Err(RuntimeError::new(
                "ToolRuntime lifecycle start was repeated",
            ));
        }
        control.lifecycle = LifecycleState::Running;
        Ok(())
    }

    async fn stop(&self) -> Result<(), RuntimeError> {
        {
            let mut control = self.lock_control();
            match control.lifecycle {
                LifecycleState::Stopped => return Ok(()),
                LifecycleState::Constructed
                | LifecycleState::Running
                | LifecycleState::Stopping => {
                    control.lifecycle = LifecycleState::Stopping;
                    for cancellation in control.jobs.values() {
                        cancellation.cancel();
                    }
                }
            }
        }
        loop {
            let notified = self.drained.notified();
            if self.lock_control().jobs.is_empty() {
                break;
            }
            notified.await;
        }
        let failures = {
            let mut control = self.lock_control();
            control.lifecycle = LifecycleState::Stopped;
            std::mem::take(&mut control.unacknowledged_failures)
        };
        if failures.is_empty() {
            Ok(())
        } else {
            let count = failures.values().fold(0usize, |count, ledger| {
                count.saturating_add(ledger.total_count())
            });
            Err(RuntimeError::new(format!(
                "{count} Tool operation(s) failed canonical closure before shutdown"
            )))
        }
    }

    fn bind_turn(
        self: &Arc<Self>,
        binding: ToolTurnBinding,
    ) -> Result<Box<dyn ToolTurn>, ToolRuntimeError> {
        let (caller, cancellation, recorder, supplied_budget) = binding.into_budget_parts();
        if cancellation.is_cancelled() {
            return Err(ToolRuntimeError::TurnClosed);
        }
        let explicit_budget = supplied_budget.is_some();
        let (budget, owned_run) = if let Some(budget) = supplied_budget {
            budget.check_active()?;
            if budget.turn_id()?.is_none() {
                let _ = budget.stop_with(BudgetStopReason::IdentityMismatch);
                return Err(BudgetError::IdentityMismatch.into());
            }
            (budget, None)
        } else {
            let limits = BudgetLimits::default();
            let task = TaskBudget::new(
                BudgetIdentity {
                    task_id: jingwei_core::TaskId::new(),
                    session_id: jingwei_core::SessionId::new(),
                    agent_key: caller.as_str().to_string(),
                },
                limits,
                limits,
                TokenBudgetMode::Soft,
                Arc::new(ExecutorBudgetClock(Instant::now())),
            )?;
            let run = task.begin_run(jingwei_core::TurnId::new(), limits)?;
            (run.scope(), Some(run))
        };
        let turn_id =
            {
                let mut control = self.lock_control();
                if control.lifecycle != LifecycleState::Running {
                    return Err(ToolRuntimeError::Stopped);
                }
                let turn_id = control.next_turn_id;
                control.next_turn_id = control.next_turn_id.checked_add(1).ok_or_else(|| {
                    ToolRuntimeError::Internal {
                        code: "tool_turn_id_exhausted".to_string(),
                        message: "Tool turn ID space is exhausted".to_string(),
                    }
                })?;
                turn_id
            };
        let granted = self
            .grants
            .get(caller.as_str())
            .cloned()
            .unwrap_or_else(|| Arc::new(BTreeSet::new()));
        let schemas = granted
            .iter()
            .filter_map(|key| {
                self.tools.get(key).map(|entry| ToolSchema {
                    name: key.clone(),
                    description: entry.metadata.description().to_string(),
                    parameters: entry.metadata.input_schema().clone(),
                })
            })
            .collect();
        let state = Arc::new(TurnState {
            id: turn_id,
            runtime: Arc::clone(self),
            caller,
            cancellation,
            recorder,
            budget,
            explicit_budget,
            owned_run: Mutex::new(owned_run),
            granted,
            schemas,
            control: Mutex::new(TurnControl::new()),
            drained: Notify::new(),
        });
        Ok(Box::new(BoundToolTurn {
            gateway: TurnGateway { state },
        }))
    }

    fn admit(
        self: &Arc<Self>,
        turn: &Arc<TurnState>,
        output_limit: usize,
    ) -> Result<(u64, Arc<JobCancellation>, JobGuard, SharedJobBudget), ToolRuntimeError> {
        let mut runtime_control = self.lock_control();
        if runtime_control.lifecycle != LifecycleState::Running {
            return Err(ToolRuntimeError::Stopped);
        }
        if runtime_control.jobs.len() >= self.max_in_flight {
            return Err(ToolRuntimeError::Overloaded);
        }
        let mut turn_control = turn.lock_control();
        if !turn_control.open {
            return Err(ToolRuntimeError::Stopped);
        }
        let job_id = runtime_control.next_job_id;
        let next_job_id = runtime_control
            .next_job_id
            .checked_add(1)
            .ok_or(ToolRuntimeError::Overloaded)?;
        // Capacity and budget acceptance share one synchronous admission boundary.
        // A rejected runtime admission never consumes a budget attempt.
        let reservation = turn.budget.reserve(BudgetRequest::new(BudgetAmounts {
            tool_calls: 1,
            tool_output_bytes: output_limit as u64,
            ..BudgetAmounts::default()
        }))?;
        let budget = Arc::new(Mutex::new(JobBudget::new(reservation)));
        runtime_control.next_job_id = next_job_id;
        let cancellation = Arc::new(JobCancellation::new());
        runtime_control
            .jobs
            .insert(job_id, Arc::clone(&cancellation));
        turn_control.jobs.insert(job_id, Arc::clone(&cancellation));
        drop(turn_control);
        drop(runtime_control);
        Ok((
            job_id,
            cancellation,
            JobGuard::new(
                Arc::clone(self),
                Arc::clone(turn),
                job_id,
                Arc::clone(&budget),
            ),
            budget,
        ))
    }

    fn complete_job(
        &self,
        turn: &TurnState,
        job_id: u64,
        closure_failure: Option<ToolRuntimeError>,
    ) {
        let mut runtime_control = self.lock_control();
        let mut turn_control = turn.lock_control();
        if let Some(failure) = closure_failure {
            turn_control.failures.record(failure.clone());
            runtime_control
                .unacknowledged_failures
                .entry(turn.id)
                .or_default()
                .record(failure);
        }
        runtime_control.jobs.remove(&job_id);
        turn_control.jobs.remove(&job_id);
        let runtime_drained = runtime_control.jobs.is_empty();
        let turn_drained = turn_control.jobs.is_empty();
        drop(turn_control);
        drop(runtime_control);
        if runtime_drained {
            self.drained.notify_one();
        }
        if turn_drained {
            turn.drained.notify_one();
        }
    }

    fn acknowledge_turn(&self, turn_id: u64) {
        self.lock_control().unacknowledged_failures.remove(&turn_id);
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum LifecycleState {
    Constructed,
    Running,
    Stopping,
    Stopped,
}

struct RuntimeControl {
    lifecycle: LifecycleState,
    next_turn_id: u64,
    next_job_id: u64,
    jobs: BTreeMap<u64, Arc<JobCancellation>>,
    unacknowledged_failures: BTreeMap<u64, FailureLedger>,
}

impl RuntimeControl {
    fn new() -> Self {
        Self {
            lifecycle: LifecycleState::Constructed,
            next_turn_id: 0,
            next_job_id: 0,
            jobs: BTreeMap::new(),
            unacknowledged_failures: BTreeMap::new(),
        }
    }
}

#[derive(Default)]
struct FailureLedger {
    failures: Vec<ToolRuntimeError>,
    omitted: usize,
}

impl FailureLedger {
    fn record(&mut self, failure: ToolRuntimeError) {
        if self.failures.len() < MAX_LIFECYCLE_FAILURES {
            self.failures.push(failure);
        } else {
            self.omitted = self.omitted.saturating_add(1);
        }
    }

    fn is_empty(&self) -> bool {
        self.failures.is_empty() && self.omitted == 0
    }

    fn total_count(&self) -> usize {
        self.failures.len().saturating_add(self.omitted)
    }
}

struct TurnState {
    id: u64,
    runtime: Arc<RuntimeState>,
    caller: jingwei_tool::ToolCaller,
    cancellation: Arc<dyn CancellationSignal>,
    recorder: Arc<dyn ToolEventRecorder>,
    budget: BudgetScope,
    explicit_budget: bool,
    owned_run: Mutex<Option<BudgetRun>>,
    granted: Arc<BTreeSet<String>>,
    schemas: Vec<ToolSchema>,
    control: Mutex<TurnControl>,
    drained: Notify,
}

impl TurnState {
    fn lock_control(&self) -> MutexGuard<'_, TurnControl> {
        self.control
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    async fn finish(&self, mode: ToolFinishMode) -> Result<(), ToolTurnFailure> {
        {
            let mut control = self.lock_control();
            control.open = false;
            if mode == ToolFinishMode::Cancel {
                for cancellation in control.jobs.values() {
                    cancellation.cancel();
                }
            }
        }
        loop {
            let notified = self.drained.notified();
            if self.lock_control().jobs.is_empty() {
                break;
            }
            notified.await;
        }
        let mut failure_ledger = {
            let mut control = self.lock_control();
            std::mem::take(&mut control.failures)
        };
        if let Some(mut run) = self
            .owned_run
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            && let Err(error) = run.finish()
        {
            failure_ledger.record(ToolRuntimeError::Internal {
                code: "tool_budget_finish_failed".into(),
                message: error.to_string(),
            });
        }
        self.runtime.acknowledge_turn(self.id);
        if failure_ledger.is_empty() {
            Ok(())
        } else {
            Err(ToolTurnFailure::from_bounded_failures(
                failure_ledger.failures,
                failure_ledger.omitted,
            )
            .expect("canonical ledger stores only closure failures")
            .expect("a non-empty closure ledger must produce ToolTurnFailure"))
        }
    }
}

struct TurnControl {
    open: bool,
    jobs: BTreeMap<u64, Arc<JobCancellation>>,
    failures: FailureLedger,
}

impl TurnControl {
    fn new() -> Self {
        Self {
            open: true,
            jobs: BTreeMap::new(),
            failures: FailureLedger::default(),
        }
    }
}

impl TurnState {
    fn call(
        self: &Arc<Self>,
        name: &str,
        arguments: serde_json::Value,
        options: ToolCallOptions,
    ) -> ToolFuture<'static, Result<ToolExecution, ToolRuntimeError>> {
        if name.is_empty() {
            return ready_error(ToolPreflightError::EmptyName.into());
        }
        if name.len() > MAX_TOOL_NAME_BYTES {
            return ready_error(
                ToolPreflightError::NameTooLong {
                    actual: name.len(),
                    max: MAX_TOOL_NAME_BYTES,
                }
                .into(),
            );
        }
        let argument_bytes = match serialized_len(&arguments) {
            Ok(bytes) => bytes,
            Err(error) => {
                return ready_error(ToolRuntimeError::Internal {
                    code: "tool_arguments_serialization".to_string(),
                    message: bounded_text(&error.to_string(), MAX_TOOL_DIAGNOSTIC_BYTES),
                });
            }
        };
        if argument_bytes > MAX_TOOL_ARGUMENT_BYTES {
            return ready_error(
                ToolPreflightError::ArgumentsTooLarge {
                    actual: argument_bytes,
                    max: MAX_TOOL_ARGUMENT_BYTES,
                }
                .into(),
            );
        }
        if self.cancellation.is_cancelled() {
            return ready_error(ToolRuntimeError::TurnClosed);
        }
        if self.explicit_budget
            && options
                .action
                .as_ref()
                .is_some_and(|action| action.decision.task_id != self.budget.identity().task_id)
        {
            let _ = self.budget.stop_with(BudgetStopReason::IdentityMismatch);
            return ready_error(BudgetError::IdentityMismatch.into());
        }
        let remaining_time = match self.budget.remaining_time() {
            Ok(remaining) => remaining,
            Err(error) => return ready_error(error.into()),
        };
        let admitted_at = Instant::now();
        let timeout = effective_timeout(
            self.runtime.default_timeout,
            self.runtime.tools.get(name),
            options.timeout,
        );
        let deadline = admitted_at
            .checked_add(timeout)
            .expect("effective Tool timeout cannot exceed its validated ceiling");
        let budget_deadline = admitted_at.checked_add(remaining_time);
        let output_limit = effective_output_limit(
            self.runtime.max_output_bytes,
            self.runtime.tools.get(name),
            options.max_output_bytes,
        );
        let (job_id, local_cancellation, guard, budget) =
            match self.runtime.admit(self, output_limit) {
                Ok(admission) => admission,
                Err(error) => return ready_error(error),
            };
        let call = ToolCall {
            action: options.action.clone(),
            id: format!("toolcall_{}", uuid::Uuid::new_v4()),
            name: name.to_string(),
            arguments,
        };
        let (sender, receiver) = oneshot::channel();
        let turn = Arc::clone(self);
        let executor = self.runtime.executor.clone();
        let operation_cancellation: Arc<dyn CancellationSignal> = Arc::new(CombinedCancellation {
            parent: Arc::clone(&self.cancellation),
            local: local_cancellation.token.clone(),
        });
        let task = async move {
            let result = run_operation(
                turn,
                call,
                operation_cancellation,
                OperationBudget {
                    budget,
                    deadline,
                    budget_deadline,
                    output_limit,
                },
            )
            .await;
            guard.complete(sender, result);
        };
        if catch_unwind(AssertUnwindSafe(|| executor.spawn(task))).is_err() {
            tracing::error!(job_id, "ToolRuntime failed to spawn an admitted operation");
            return ready_error(ToolRuntimeError::Internal {
                code: "tool_spawn_failed".to_string(),
                message: "ToolRuntime executor rejected an admitted operation".to_string(),
            });
        }
        Box::pin(async move {
            receiver.await.unwrap_or_else(|_| {
                Err(ToolRuntimeError::Internal {
                    code: "tool_driver_lost".to_string(),
                    message: format!("Tool operation {job_id} ended without a completion report"),
                })
            })
        })
    }
}

#[derive(Default)]
struct ByteCounter {
    bytes: usize,
}

impl Write for ByteCounter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.bytes = self.bytes.saturating_add(buffer.len());
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn serialized_len(value: &serde_json::Value) -> Result<usize, serde_json::Error> {
    let mut counter = ByteCounter::default();
    serde_json::to_writer(&mut counter, value)?;
    Ok(counter.bytes)
}

struct BoundToolTurn {
    gateway: TurnGateway,
}

impl ToolTurn for BoundToolTurn {
    fn budget_report(&self) -> Option<Result<jingwei_budget::BudgetReport, BudgetError>> {
        Some(self.gateway.state.budget.report())
    }

    fn gateway(&self) -> &dyn ToolGateway {
        &self.gateway
    }

    fn finish(
        self: Box<Self>,
        mode: ToolFinishMode,
    ) -> ToolFuture<'static, Result<(), ToolTurnFailure>> {
        Box::pin(async move { self.gateway.state.finish(mode).await })
    }
}

struct TurnGateway {
    state: Arc<TurnState>,
}

impl ToolGateway for TurnGateway {
    fn schemas(&self) -> &[ToolSchema] {
        &self.state.schemas
    }

    fn call_with_options(
        &self,
        name: &str,
        arguments: serde_json::Value,
        options: ToolCallOptions,
    ) -> ToolFuture<'static, Result<ToolExecution, ToolRuntimeError>> {
        self.state.call(name, arguments, options)
    }
}

fn ready_error(
    error: ToolRuntimeError,
) -> ToolFuture<'static, Result<ToolExecution, ToolRuntimeError>> {
    Box::pin(async move { Err(error) })
}

struct JobCancellation {
    token: CancellationToken,
}

impl JobCancellation {
    fn new() -> Self {
        Self {
            token: CancellationToken::new(),
        }
    }

    fn cancel(&self) {
        self.token.cancel();
    }
}

struct CombinedCancellation {
    parent: Arc<dyn CancellationSignal>,
    local: CancellationToken,
}

impl CancellationSignal for CombinedCancellation {
    fn is_cancelled(&self) -> bool {
        self.parent.is_cancelled() || self.local.is_cancelled()
    }

    fn cancelled(&self) -> CancellationFuture<'_> {
        Box::pin(async move {
            tokio::select! {
                _ = self.parent.cancelled() => {},
                _ = self.local.cancelled() => {},
            }
        })
    }
}

struct JobGuard {
    runtime: Arc<RuntimeState>,
    turn: Arc<TurnState>,
    job_id: u64,
    budget: SharedJobBudget,
    armed: bool,
}

impl JobGuard {
    fn new(
        runtime: Arc<RuntimeState>,
        turn: Arc<TurnState>,
        job_id: u64,
        budget: SharedJobBudget,
    ) -> Self {
        Self {
            runtime,
            turn,
            job_id,
            budget,
            armed: true,
        }
    }

    fn complete(
        mut self,
        sender: oneshot::Sender<Result<ToolExecution, ToolRuntimeError>>,
        result: Result<ToolExecution, ToolRuntimeError>,
    ) {
        let _ = self
            .budget
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .settle();
        let closure_failure = result
            .as_ref()
            .err()
            .filter(|error| is_closure_failure(error))
            .cloned();
        self.runtime
            .complete_job(&self.turn, self.job_id, closure_failure);
        self.armed = false;
        let _ = sender.send(result);
    }
}

impl Drop for JobGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // The guard also owns accounting before the driver future is first polled.
        // Settle before removing the job so a turn cannot drain with unowned usage.
        let _ = self
            .budget
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .settle();
        let failure = ToolRuntimeError::Internal {
            code: "tool_driver_unwound".to_string(),
            message: format!(
                "Tool operation {} unwound before canonical closure",
                self.job_id
            ),
        };
        self.runtime
            .complete_job(&self.turn, self.job_id, Some(failure));
        tracing::error!(job_id = self.job_id, "Tool operation driver unwound");
    }
}

fn is_closure_failure(error: &ToolRuntimeError) -> bool {
    matches!(
        error,
        ToolRuntimeError::Recording(_) | ToolRuntimeError::Internal { .. }
    )
}

async fn run_operation(
    turn: Arc<TurnState>,
    call: ToolCall,
    cancellation: Arc<dyn CancellationSignal>,
    mut budget: OperationBudget,
) -> Result<ToolExecution, ToolRuntimeError> {
    let call_event = match append_record(&turn, ToolRecord::Call(call.clone())).await {
        Ok(event) => event,
        Err(source) => {
            // No body can start before the confirmed Call barrier. Keep the attempt,
            // release the unused variable reservation, and retain the recording cause.
            let _ = budget.finish();
            return Err(ToolRuntimeError::Recording(Arc::new(
                ToolClosureFailure::call_error(call, source),
            )));
        }
    };
    let mut semantic = resolve_semantic(&turn, &call, &cancellation, &mut budget).await;
    if let Err(error) = budget.finish() {
        semantic = SemanticResolution::budget(error);
    }
    let result = ToolResult {
        call_id: call.id.clone(),
        outcome: semantic.outcome,
    };
    let result_event = append_record(&turn, ToolRecord::Result(result.clone()))
        .await
        .map_err(|source| {
            ToolRuntimeError::Recording(Arc::new(ToolClosureFailure::result_error(
                call.clone(),
                result.clone(),
                source,
            )))
        })?;
    let execution =
        ToolExecution::new(call, result, call_event, result_event).map_err(|error| {
            ToolRuntimeError::Internal {
                code: "tool_record_evidence_invalid".to_string(),
                message: bounded_text(&error.to_string(), MAX_TOOL_DIAGNOSTIC_BYTES),
            }
        })?;
    if let Some(error) = semantic.budget_error {
        Err(error.into())
    } else if semantic.cancelled {
        Err(ToolRuntimeError::cancelled(execution))
    } else {
        Ok(execution)
    }
}

struct OperationBudget {
    budget: SharedJobBudget,
    deadline: Instant,
    budget_deadline: Option<Instant>,
    output_limit: usize,
}

impl OperationBudget {
    fn start(&self) -> Result<(), BudgetError> {
        self.budget
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .start()
    }

    fn finish(&self) -> Result<(), BudgetError> {
        self.budget
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .settle()
    }

    fn observe_output(&self, bytes: u64) {
        self.budget
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .output_bytes = UsageValue::Actual(bytes);
    }
}

type SharedJobBudget = Arc<Mutex<JobBudget>>;

struct JobBudget {
    reservation: Option<BudgetReservation>,
    started: bool,
    output_bytes: UsageValue,
}

impl JobBudget {
    fn new(reservation: BudgetReservation) -> Self {
        Self {
            reservation: Some(reservation),
            started: false,
            output_bytes: UsageValue::Unknown,
        }
    }

    fn start(&mut self) -> Result<(), BudgetError> {
        self.reservation
            .as_mut()
            .expect("unsettled operation")
            .mark_started()?;
        self.started = true;
        Ok(())
    }

    fn settle(&mut self) -> Result<(), BudgetError> {
        let Some(reservation) = self.reservation.take() else {
            return Ok(());
        };
        if self.started {
            reservation.settle(BudgetUsage {
                input_tokens: UsageValue::Actual(0),
                output_tokens: UsageValue::Actual(0),
                tool_output_bytes: self.output_bytes,
            })
        } else {
            reservation.cancel_before_start()
        }
    }
}

impl Drop for JobBudget {
    fn drop(&mut self) {
        let _ = self.settle();
    }
}

async fn append_record(
    turn: &TurnState,
    record: ToolRecord,
) -> Result<Arc<jingwei_core::SessionEvent>, ToolRecordError> {
    let kind = match &record {
        ToolRecord::Call(_) => BudgetEventKind::ToolCall,
        ToolRecord::Result(_) => BudgetEventKind::ToolResult,
    };
    // Once append has begun, neither deadlines nor cancellation may discard it.
    let result = match catch_unwind(AssertUnwindSafe(|| turn.recorder.append(record))) {
        Ok(future) => match AssertUnwindSafe(future).catch_unwind().await {
            Ok(result) => result.map_err(ToolRecordError::Session),
            Err(_) => Err(ToolRecordError::RecorderPanicked),
        },
        Err(_) => Err(ToolRecordError::RecorderPanicked),
    };
    let result = result.and_then(|event| {
        if turn.explicit_budget
            && (event.session_id != turn.budget.identity().session_id
                || turn.budget.turn_id().ok().flatten().as_ref() != Some(&event.turn_id))
        {
            let _ = turn.budget.stop_with(BudgetStopReason::IdentityMismatch);
            Err(ToolRecordError::IdentityMismatch { event })
        } else {
            Ok(event)
        }
    });
    turn.budget.record_evidence(kind, result.is_ok());
    result
}

fn effective_timeout(
    runtime: Duration,
    tool: Option<&Arc<ToolEntry>>,
    requested: Option<Duration>,
) -> Duration {
    let mut timeout = runtime;
    if let Some(tool_timeout) = tool.and_then(|entry| entry.metadata.timeout_ceiling()) {
        timeout = timeout.min(tool_timeout);
    }
    if let Some(requested) = requested {
        timeout = timeout.min(requested);
    }
    timeout
}

fn effective_output_limit(
    runtime: usize,
    tool: Option<&Arc<ToolEntry>>,
    requested: Option<usize>,
) -> usize {
    let mut limit = runtime;
    if let Some(tool_limit) = tool.and_then(|tool| tool.metadata.output_ceiling_bytes()) {
        limit = limit.min(tool_limit);
    }
    if let Some(requested) = requested {
        limit = limit.min(requested);
    }
    limit
}

struct SemanticResolution {
    outcome: ToolRecordedOutcome,
    cancelled: bool,
    budget_error: Option<BudgetError>,
}

impl SemanticResolution {
    fn success(output: String) -> Self {
        Self {
            outcome: ToolRecordedOutcome::Succeeded { output },
            cancelled: false,
            budget_error: None,
        }
    }

    fn failure(
        category: ToolFailureCategory,
        code: impl AsRef<str>,
        message: impl AsRef<str>,
        retryable: bool,
    ) -> Self {
        Self {
            outcome: ToolRecordedOutcome::Failed {
                category,
                code: bounded_text(code.as_ref(), MAX_TOOL_DIAGNOSTIC_BYTES),
                message: bounded_text(message.as_ref(), MAX_TOOL_DIAGNOSTIC_BYTES),
                retryable,
            },
            cancelled: false,
            budget_error: None,
        }
    }

    fn cancelled() -> Self {
        let mut resolution = Self::failure(
            ToolFailureCategory::Cancelled,
            "tool_cancelled",
            "Tool operation was cancelled",
            false,
        );
        resolution.cancelled = true;
        resolution
    }

    fn timeout() -> Self {
        Self::failure(
            ToolFailureCategory::Timeout,
            "tool_timeout",
            "Tool operation exceeded its deadline",
            true,
        )
    }

    fn budget(error: BudgetError) -> Self {
        let mut resolution = Self::failure(
            ToolFailureCategory::Budget,
            "tool_budget_stopped",
            error.to_string(),
            false,
        );
        resolution.budget_error = Some(error);
        resolution
    }
}

async fn resolve_semantic(
    turn: &TurnState,
    call: &ToolCall,
    cancellation: &Arc<dyn CancellationSignal>,
    budget: &mut OperationBudget,
) -> SemanticResolution {
    if let Some(interrupted) = interruption(cancellation.as_ref(), budget, &turn.budget) {
        return interrupted;
    }
    if !turn.granted.contains(&call.name) {
        return SemanticResolution::failure(
            ToolFailureCategory::Unavailable,
            "tool_unavailable",
            "Tool is unavailable to this caller",
            false,
        );
    }
    let Some(entry) = turn.runtime.tools.get(&call.name) else {
        return SemanticResolution::failure(
            ToolFailureCategory::Unavailable,
            "tool_unavailable",
            "Tool is unavailable to this caller",
            false,
        );
    };
    let validation = catch_unwind(AssertUnwindSafe(|| {
        entry.validator.validate(&call.arguments)
    }));
    match validation {
        Err(_) => {
            return SemanticResolution::failure(
                ToolFailureCategory::InvalidArguments,
                "tool_arguments_validation_fault",
                "Tool argument validation failed internally",
                false,
            );
        }
        Ok(Err(error)) => {
            return SemanticResolution::failure(
                ToolFailureCategory::InvalidArguments,
                "tool_arguments_invalid",
                error.to_string(),
                false,
            );
        }
        Ok(Ok(())) => {}
    }
    if let Some(interrupted) = interruption(cancellation.as_ref(), budget, &turn.budget) {
        return interrupted;
    }
    for guard in &turn.runtime.guards {
        let future = match catch_unwind(AssertUnwindSafe(|| {
            guard.evaluate(ToolGuardRequest::new(&turn.caller, call))
        })) {
            Ok(future) => future,
            Err(_) => {
                return SemanticResolution::failure(
                    ToolFailureCategory::GuardFault,
                    "tool_guard_panicked",
                    "Tool guard panicked",
                    false,
                );
            }
        };
        match await_control(
            AssertUnwindSafe(future).catch_unwind(),
            cancellation.as_ref(),
            budget,
            &turn.budget,
        )
        .await
        {
            Controlled::Cancelled => return SemanticResolution::cancelled(),
            Controlled::TimedOut => return SemanticResolution::timeout(),
            Controlled::Budget(error) => return SemanticResolution::budget(error),
            Controlled::Ready(Err(_)) => {
                return SemanticResolution::failure(
                    ToolFailureCategory::GuardFault,
                    "tool_guard_panicked",
                    "Tool guard panicked",
                    false,
                );
            }
            Controlled::Ready(Ok(Err(error))) => {
                let diagnostic = error.diagnostic();
                return SemanticResolution::failure(
                    ToolFailureCategory::GuardFault,
                    diagnostic.code(),
                    diagnostic.message(),
                    diagnostic.retryable(),
                );
            }
            Controlled::Ready(Ok(Ok(Some(denial)))) => {
                let diagnostic = denial.diagnostic();
                return SemanticResolution::failure(
                    ToolFailureCategory::Denied,
                    diagnostic.code(),
                    diagnostic.message(),
                    diagnostic.retryable(),
                );
            }
            Controlled::Ready(Ok(Ok(None))) => {}
        }
    }
    if let Some(authorizer_key) = entry.metadata.approval().key() {
        let Some(authorizer) = turn.runtime.authorizers.get(authorizer_key) else {
            return SemanticResolution::failure(
                ToolFailureCategory::ApprovalUnavailable,
                "tool_approval_unavailable",
                "Required Tool authorizer is unavailable",
                false,
            );
        };
        let future = match catch_unwind(AssertUnwindSafe(|| {
            authorizer.authorize(ToolAuthorizationRequest::new(&turn.caller, call))
        })) {
            Ok(future) => future,
            Err(_) => {
                return SemanticResolution::failure(
                    ToolFailureCategory::ApprovalFault,
                    "tool_authorizer_panicked",
                    "Tool authorizer panicked",
                    false,
                );
            }
        };
        match await_control(
            AssertUnwindSafe(future).catch_unwind(),
            cancellation.as_ref(),
            budget,
            &turn.budget,
        )
        .await
        {
            Controlled::Cancelled => return SemanticResolution::cancelled(),
            Controlled::TimedOut => return SemanticResolution::timeout(),
            Controlled::Budget(error) => return SemanticResolution::budget(error),
            Controlled::Ready(Err(_)) => {
                return SemanticResolution::failure(
                    ToolFailureCategory::ApprovalFault,
                    "tool_authorizer_panicked",
                    "Tool authorizer panicked",
                    false,
                );
            }
            Controlled::Ready(Ok(Err(error))) => {
                let diagnostic = error.diagnostic();
                return SemanticResolution::failure(
                    ToolFailureCategory::ApprovalFault,
                    diagnostic.code(),
                    diagnostic.message(),
                    diagnostic.retryable(),
                );
            }
            Controlled::Ready(Ok(Ok(ToolAuthorizationDecision::Denied(denial)))) => {
                let diagnostic = denial.diagnostic();
                return SemanticResolution::failure(
                    ToolFailureCategory::ApprovalDenied,
                    diagnostic.code(),
                    diagnostic.message(),
                    diagnostic.retryable(),
                );
            }
            Controlled::Ready(Ok(Ok(ToolAuthorizationDecision::Approved))) => {}
        }
    }
    if let Some(interrupted) = interruption(cancellation.as_ref(), budget, &turn.budget) {
        return interrupted;
    }
    if let Err(error) = budget.start() {
        return SemanticResolution::budget(error);
    }
    let _timing = ToolExecutionTimer {
        started: Instant::now(),
        scope: turn.budget.clone(),
    };
    let request = ToolBodyRequest::new(&call.id, &call.arguments);
    let adapter_cancellation = CancellationToken::new();
    let _adapter_guard = ToolAdapterCancellationGuard(adapter_cancellation.clone());
    let body_cancellation: Arc<dyn CancellationSignal> = Arc::new(CombinedCancellation {
        parent: Arc::clone(cancellation),
        local: adapter_cancellation.clone(),
    });
    let future = match catch_unwind(AssertUnwindSafe(|| {
        entry.tool.execute(request, body_cancellation)
    })) {
        Ok(future) => future,
        Err(_) => {
            adapter_cancellation.cancel();
            return SemanticResolution::failure(
                ToolFailureCategory::BodyPanic,
                "tool_body_panicked",
                "Tool body panicked",
                false,
            );
        }
    };
    let controlled = await_control(
        AssertUnwindSafe(future).catch_unwind(),
        cancellation.as_ref(),
        budget,
        &turn.budget,
    )
    .await;
    adapter_cancellation.cancel();
    match controlled {
        Controlled::Cancelled => SemanticResolution::cancelled(),
        Controlled::TimedOut => SemanticResolution::timeout(),
        Controlled::Budget(error) => SemanticResolution::budget(error),
        Controlled::Ready(Err(_)) => SemanticResolution::failure(
            ToolFailureCategory::BodyPanic,
            "tool_body_panicked",
            "Tool body panicked",
            false,
        ),
        Controlled::Ready(Ok(Err(error))) => {
            let diagnostic = error.diagnostic();
            SemanticResolution::failure(
                ToolFailureCategory::BodyFailure,
                diagnostic.code(),
                diagnostic.message(),
                diagnostic.retryable(),
            )
        }
        Controlled::Ready(Ok(Ok(output))) => {
            budget.observe_output(output.len() as u64);
            if output.len() > budget.output_limit {
                SemanticResolution::failure(
                    ToolFailureCategory::OutputLimit,
                    "tool_output_limit",
                    "Tool output exceeded the configured byte limit",
                    false,
                )
            } else {
                SemanticResolution::success(output)
            }
        }
    }
}

fn interruption(
    cancellation: &dyn CancellationSignal,
    operation: &OperationBudget,
    budget: &BudgetScope,
) -> Option<SemanticResolution> {
    if let Err(error) = budget.check_active() {
        Some(SemanticResolution::budget(error))
    } else if operation
        .budget_deadline
        .is_some_and(|deadline| Instant::now() >= deadline)
    {
        Some(SemanticResolution::budget(expire_budget(budget)))
    } else if cancellation.is_cancelled() {
        Some(SemanticResolution::cancelled())
    } else if Instant::now() >= operation.deadline {
        Some(SemanticResolution::timeout())
    } else {
        None
    }
}

enum Controlled<T> {
    Ready(T),
    Cancelled,
    TimedOut,
    Budget(BudgetError),
}

async fn await_control<F, T>(
    future: F,
    cancellation: &dyn CancellationSignal,
    operation: &OperationBudget,
    budget: &BudgetScope,
) -> Controlled<T>
where
    F: std::future::Future<Output = T> + Send,
{
    tokio::select! {
        biased;
        error = budget.stopped() => Controlled::Budget(error),
        _ = wait_budget_deadline(operation.budget_deadline) => Controlled::Budget(expire_budget(budget)),
        _ = cancellation.cancelled() => Controlled::Cancelled,
        _ = tokio::time::sleep_until(operation.deadline) => Controlled::TimedOut,
        output = future => Controlled::Ready(output),
    }
}

async fn wait_budget_deadline(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

fn expire_budget(budget: &BudgetScope) -> BudgetError {
    budget
        .expire()
        .err()
        .unwrap_or(BudgetError::Stopped(BudgetStopReason::ActiveTime))
}

struct ToolExecutionTimer {
    started: Instant,
    scope: BudgetScope,
}

struct ToolAdapterCancellationGuard(CancellationToken);

impl Drop for ToolAdapterCancellationGuard {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

impl Drop for ToolExecutionTimer {
    fn drop(&mut self) {
        self.scope.record_tool_time(self.started.elapsed());
    }
}

fn bounded_text(value: &str, limit: usize) -> String {
    if value.len() <= limit {
        return value.to_string();
    }
    let mut end = limit.min(value.len());
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_string()
}

struct ToolRuntimeLifecycle {
    state: Arc<RuntimeState>,
}

impl ServiceLifecycle for ToolRuntimeLifecycle {
    fn start(&mut self) -> LifecycleFuture<'_, Result<(), RuntimeError>> {
        Box::pin(async { self.state.start() })
    }

    fn stop(
        self: Box<Self>,
        _reason: StopReason,
    ) -> LifecycleFuture<'static, Result<(), RuntimeError>> {
        Box::pin(async move { self.state.stop().await })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use jingwei_core::{EventId, SessionEvent, SessionId, TurnId};
    use jingwei_session::SessionRuntimeError;
    use jingwei_tool::{ToolCaller, ToolRecordStage};
    use serde_json::json;

    use super::*;

    struct NeverCancelled;

    impl CancellationSignal for NeverCancelled {
        fn is_cancelled(&self) -> bool {
            false
        }

        fn cancelled(&self) -> CancellationFuture<'_> {
            Box::pin(std::future::pending())
        }
    }

    struct SuccessfulTool {
        calls: AtomicUsize,
    }

    impl Tool for SuccessfulTool {
        fn metadata(&self) -> ToolMetadata {
            ToolMetadata::new("test Tool", json!({"type": "object"}))
        }

        fn execute<'a>(
            &'a self,
            _request: ToolBodyRequest<'a>,
            _cancellation: Arc<dyn CancellationSignal>,
        ) -> ToolFuture<'a, Result<String, jingwei_tool::ToolBodyError>> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Ok("ok".to_string())
            })
        }
    }

    struct ResultFailingRecorder {
        result_attempts: AtomicUsize,
    }

    impl ToolEventRecorder for ResultFailingRecorder {
        fn append(
            &self,
            record: ToolRecord,
        ) -> ToolFuture<'_, Result<Arc<SessionEvent>, SessionRuntimeError>> {
            Box::pin(async move {
                match record {
                    ToolRecord::Call(call) => Ok(Arc::new(SessionEvent {
                        event_id: EventId::new(),
                        session_id: SessionId::from("tool-runtime-unit"),
                        turn_id: TurnId::from("turn_1"),
                        generation_id: None,
                        message_id: None,
                        seq: 0,
                        kind: jingwei_core::SessionEventKind::ToolCall { call },
                    })),
                    ToolRecord::Result(_) => {
                        self.result_attempts.fetch_add(1, Ordering::SeqCst);
                        Err(SessionRuntimeError::Stopped)
                    }
                }
            })
        }
    }

    #[tokio::test]
    async fn result_recording_failure_reaches_waiter_and_host_drain_once() {
        const ATTEMPTS: usize = MAX_LIFECYCLE_FAILURES + 1;

        assert!(
            ToolTurnFailure::from_failures(vec![ToolRuntimeError::Stopped]).is_err(),
            "semantic/admission errors cannot masquerade as closure evidence"
        );
        let tool = Arc::new(SuccessfulTool {
            calls: AtomicUsize::new(0),
        });
        let metadata = tool.metadata();
        let validator = jsonschema::draft202012::new(metadata.input_schema()).unwrap();
        let mut tools = BTreeMap::new();
        tools.insert(
            "tool.ok".to_string(),
            Arc::new(ToolEntry {
                tool: tool.clone(),
                metadata,
                validator,
            }),
        );
        let mut grants = BTreeMap::new();
        grants.insert(
            "agent-owner".to_string(),
            Arc::new(BTreeSet::from(["tool.ok".to_string()])),
        );
        let state = Arc::new(RuntimeState::new(
            tools,
            Vec::new(),
            BTreeMap::new(),
            grants,
            Handle::current(),
            Duration::from_secs(1),
            DEFAULT_TOOL_OUTPUT_BYTES,
            4,
        ));
        state.start().unwrap();
        let recorder = Arc::new(ResultFailingRecorder {
            result_attempts: AtomicUsize::new(0),
        });
        let runtime = CanonicalToolRuntime {
            state: Arc::clone(&state),
        };
        let turn = runtime
            .bind_turn(ToolTurnBinding::new(
                ToolCaller::new("agent-owner"),
                Arc::new(NeverCancelled),
                recorder.clone(),
            ))
            .unwrap();

        for _ in 0..ATTEMPTS {
            let waiter_error = turn.gateway().call("tool.ok", json!({})).await.unwrap_err();
            let ToolRuntimeError::Recording(waiter_failure) = waiter_error else {
                panic!("expected result recording failure");
            };
            assert_eq!(waiter_failure.stage(), ToolRecordStage::Result);
        }

        let drain = turn.finish(ToolFinishMode::Graceful).await.unwrap_err();
        assert_eq!(drain.failures().len(), jingwei_tool::MAX_TOOL_TURN_FAILURES);
        assert_eq!(
            drain.omitted(),
            ATTEMPTS - jingwei_tool::MAX_TOOL_TURN_FAILURES
        );
        assert_eq!(drain.total_count(), ATTEMPTS);
        let ToolRuntimeError::Recording(drain_failure) = &drain.failures()[0] else {
            panic!("expected the same typed result recording failure at drain");
        };
        assert_eq!(drain_failure.stage(), ToolRecordStage::Result);
        assert_eq!(tool.calls.load(Ordering::SeqCst), ATTEMPTS);
        assert_eq!(recorder.result_attempts.load(Ordering::SeqCst), ATTEMPTS);
        state.stop().await.unwrap();
    }
}
