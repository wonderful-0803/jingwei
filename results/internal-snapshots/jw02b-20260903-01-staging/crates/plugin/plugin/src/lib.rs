//! Declarative plugin composition for Jingwei.
//!
//! Plugins mount declarations into private plans. Runtime callers receive a frozen
//! [`PluginRegistry`] only after every active plan is validated and selected typed factories have
//! been constructed and started in dependency order.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use futures::lock::Mutex as AsyncMutex;
use jingwei_agent::{AGENT_RUNTIME, Agent, AgentRuntime, TurnFinally};
pub use jingwei_core::CapabilityId;
use jingwei_core::{EventObserver, SessionId, TurnId};
use jingwei_llm::{LLM_PROVIDER, LLM_RUNTIME, Llm, LlmRuntime};
use jingwei_session::{SESSION_PERSISTENCE, SESSION_RUNTIME, SessionPersistence, SessionRuntime};
use jingwei_tool::{TOOL_RUNTIME, Tool, ToolAuthorizer, ToolGuard, ToolRuntime};

mod graph;

/// A stable identity for one statically composed plugin.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PluginId(&'static str);

impl PluginId {
    pub const fn new(value: &'static str) -> Self {
        Self(value)
    }

    pub const fn as_str(&self) -> &'static str {
        self.0
    }
}

impl fmt::Display for PluginId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

/// Pure metadata used to resolve a complete plugin set before mounting.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PluginDescriptor {
    id: PluginId,
    dependencies: &'static [PluginId],
    capability_dependencies: &'static [CapabilityId],
    optional_capability_dependencies: &'static [CapabilityId],
    provider: Option<ProviderCandidate>,
}

/// A singular provider candidate declared by a plugin descriptor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProviderCandidate {
    capability: CapabilityId,
    key: &'static str,
}

impl ProviderCandidate {
    pub const fn capability(self) -> CapabilityId {
        self.capability
    }

    pub const fn key(self) -> &'static str {
        self.key
    }
}

impl PluginDescriptor {
    pub const fn new(id: &'static str) -> Self {
        Self {
            id: PluginId::new(id),
            dependencies: &[],
            capability_dependencies: &[],
            optional_capability_dependencies: &[],
            provider: None,
        }
    }

    pub const fn provider(id: &'static str, capability: CapabilityId, key: &'static str) -> Self {
        Self {
            id: PluginId::new(id),
            dependencies: &[],
            capability_dependencies: &[],
            optional_capability_dependencies: &[],
            provider: Some(ProviderCandidate { capability, key }),
        }
    }

    #[must_use]
    pub const fn requires_plugins(mut self, dependencies: &'static [PluginId]) -> Self {
        self.dependencies = dependencies;
        self
    }

    #[must_use]
    pub const fn requires_capabilities(mut self, dependencies: &'static [CapabilityId]) -> Self {
        self.capability_dependencies = dependencies;
        self
    }

    #[must_use]
    pub const fn uses_capabilities(mut self, dependencies: &'static [CapabilityId]) -> Self {
        self.optional_capability_dependencies = dependencies;
        self
    }

    pub const fn id(self) -> PluginId {
        self.id
    }

    pub const fn plugin_dependencies(self) -> &'static [PluginId] {
        self.dependencies
    }

    pub const fn capability_dependencies(self) -> &'static [CapabilityId] {
        self.capability_dependencies
    }

    pub const fn optional_capability_dependencies(self) -> &'static [CapabilityId] {
        self.optional_capability_dependencies
    }

    pub const fn provider_candidate(self) -> Option<ProviderCandidate> {
        self.provider
    }
}

/// An owner-free failure returned by plugin declaration code.
#[derive(Clone, Debug, thiserror::Error)]
#[error("{message}")]
pub struct MountError {
    message: String,
}

impl MountError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// A boxed asynchronous lifecycle operation.
pub type LifecycleFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// An owner-free error returned by a factory or lifecycle implementation.
#[derive(Clone, Debug, thiserror::Error)]
#[error("{message}")]
pub struct RuntimeError {
    message: String,
}

impl RuntimeError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// Why the kernel is stopping a constructed service.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StopReason {
    ActivationRollback,
    RequestedShutdown,
}

/// Lifecycle operations owned by the composition kernel.
pub trait ServiceLifecycle: Send {
    fn start(&mut self) -> LifecycleFuture<'_, Result<(), RuntimeError>>;

    fn stop(
        self: Box<Self>,
        reason: StopReason,
    ) -> LifecycleFuture<'static, Result<(), RuntimeError>>;
}

struct ReadyLifecycle;

impl ServiceLifecycle for ReadyLifecycle {
    fn start(&mut self) -> LifecycleFuture<'_, Result<(), RuntimeError>> {
        Box::pin(async { Ok(()) })
    }

    fn stop(
        self: Box<Self>,
        _reason: StopReason,
    ) -> LifecycleFuture<'static, Result<(), RuntimeError>> {
        Box::pin(async { Ok(()) })
    }
}

/// A typed value paired with the one lifecycle owner the kernel will manage.
pub struct ManagedService<T: ?Sized + Send + Sync + 'static> {
    value: Arc<T>,
    lifecycle: Box<dyn ServiceLifecycle>,
}

impl<T: ?Sized + Send + Sync + 'static> ManagedService<T> {
    pub fn new(value: Arc<T>, lifecycle: Box<dyn ServiceLifecycle>) -> Self {
        Self { value, lifecycle }
    }

    pub fn ready(value: Arc<T>) -> Self {
        Self::new(value, Box::new(ReadyLifecycle))
    }

    fn into_parts(self) -> (Arc<T>, Box<dyn ServiceLifecycle>) {
        (self.value, self.lifecycle)
    }
}

/// Read-only construction context filtered by the provider's frozen descriptor.
pub struct FactoryContext<'a> {
    registry: &'a PluginRegistry,
    required_dependencies: &'static [CapabilityId],
    optional_dependencies: &'static [CapabilityId],
    provided_capability: Option<CapabilityId>,
}

impl<'a> FactoryContext<'a> {
    fn new(registry: &'a PluginRegistry, descriptor: PluginDescriptor) -> Self {
        Self {
            registry,
            required_dependencies: descriptor.capability_dependencies(),
            optional_dependencies: descriptor.optional_capability_dependencies(),
            provided_capability: descriptor
                .provider_candidate()
                .map(ProviderCandidate::capability),
        }
    }

    /// Raw adapter access is reserved for the selected LlmRuntime factory.
    pub fn llm(&self) -> Option<Arc<dyn Llm>> {
        (self.provided_capability == Some(LLM_RUNTIME)
            && self.required_dependencies.contains(&LLM_PROVIDER))
        .then(|| self.registry.llm())
        .flatten()
    }

    pub fn llm_runtime(&self) -> Option<Arc<dyn LlmRuntime>> {
        self.can_read(LLM_RUNTIME)
            .then(|| self.registry.llm_runtime())
            .flatten()
    }

    /// The controlled ToolRuntime is visible only while constructing the selected AgentRuntime.
    pub fn tool_runtime(&self) -> Option<Arc<dyn ToolRuntime>> {
        (self.provided_capability == Some(AGENT_RUNTIME) && self.can_read(TOOL_RUNTIME))
            .then(|| self.registry.tool_runtime())
            .flatten()
    }

    /// Frozen Agent contributions are visible only to the selected AgentRuntime factory.
    pub fn agent_bindings(&self) -> Option<Vec<AgentBinding>> {
        (self.provided_capability == Some(AGENT_RUNTIME)).then(|| self.registry.agent_bindings())
    }

    /// Frozen Hook contributions are visible only to the selected AgentRuntime factory.
    pub fn hook_bindings(&self) -> Option<Vec<HookBinding>> {
        (self.provided_capability == Some(AGENT_RUNTIME)).then(|| self.registry.hook_bindings())
    }

    /// Frozen raw Tool contributions are visible only to the selected ToolRuntime factory.
    pub fn tool_bindings(&self) -> Option<Vec<ToolBinding>> {
        (self.provided_capability == Some(TOOL_RUNTIME)).then(|| self.registry.tool_bindings())
    }

    /// Frozen deny-only guards are visible only to the selected ToolRuntime factory.
    pub fn tool_guard_bindings(&self) -> Option<Vec<ToolGuardBinding>> {
        (self.provided_capability == Some(TOOL_RUNTIME))
            .then(|| self.registry.tool_guard_bindings())
    }

    /// Frozen keyed authorizers are visible only to the selected ToolRuntime factory.
    pub fn tool_authorizer_bindings(&self) -> Option<Vec<ToolAuthorizerBinding>> {
        (self.provided_capability == Some(TOOL_RUNTIME))
            .then(|| self.registry.tool_authorizer_bindings())
    }

    pub fn session_persistence(&self) -> Option<Arc<dyn SessionPersistence>> {
        self.can_read(SESSION_PERSISTENCE)
            .then(|| self.registry.session_persistence())
            .flatten()
    }

    pub fn session_runtime(&self) -> Option<Arc<dyn SessionRuntime>> {
        self.can_read(SESSION_RUNTIME)
            .then(|| self.registry.session_runtime())
            .flatten()
    }

    /// Frozen Observer contributions, visible only while constructing the selected SessionRuntime.
    pub fn event_observers(&self) -> Option<Vec<EventObserverBinding>> {
        (self.provided_capability == Some(SESSION_RUNTIME))
            .then(|| self.registry.event_observer_bindings())
    }

    fn can_read(&self, capability: CapabilityId) -> bool {
        self.required_dependencies.contains(&capability)
            || (self.optional_dependencies.contains(&capability)
                && self.registry.binding(capability).is_some())
    }
}

/// One frozen post-commit Observer contribution with kernel-owned metadata.
#[derive(Clone)]
pub struct EventObserverBinding {
    key: String,
    owner: PluginId,
    observer: Arc<dyn EventObserver>,
}

impl EventObserverBinding {
    pub fn key(&self) -> &str {
        &self.key
    }

    pub const fn owner(&self) -> PluginId {
        self.owner
    }

    pub fn observer(&self) -> Arc<dyn EventObserver> {
        Arc::clone(&self.observer)
    }
}

/// One frozen Agent contribution with kernel-owned authority metadata.
#[derive(Clone)]
pub struct AgentBinding {
    key: String,
    owner: PluginId,
    required_capabilities: &'static [CapabilityId],
    agent: Arc<dyn Agent>,
}

impl AgentBinding {
    pub fn key(&self) -> &str {
        &self.key
    }

    pub const fn owner(&self) -> PluginId {
        self.owner
    }

    pub fn agent(&self) -> Arc<dyn Agent> {
        Arc::clone(&self.agent)
    }

    pub fn requires(&self, capability: CapabilityId) -> bool {
        self.required_capabilities.contains(&capability)
    }
}

/// One frozen raw Tool contribution with kernel-owned authority metadata.
#[derive(Clone)]
pub struct ToolBinding {
    key: String,
    owner: PluginId,
    tool: Arc<dyn Tool>,
}

impl ToolBinding {
    pub fn key(&self) -> &str {
        &self.key
    }

    pub const fn owner(&self) -> PluginId {
        self.owner
    }

    pub fn tool(&self) -> Arc<dyn Tool> {
        Arc::clone(&self.tool)
    }
}

/// One frozen deny-only Tool guard in deterministic global execution order.
#[derive(Clone)]
pub struct ToolGuardBinding {
    owner: PluginId,
    ordinal: usize,
    guard: Arc<dyn ToolGuard>,
}

impl ToolGuardBinding {
    pub const fn owner(&self) -> PluginId {
        self.owner
    }

    pub const fn ordinal(&self) -> usize {
        self.ordinal
    }

    pub fn guard(&self) -> Arc<dyn ToolGuard> {
        Arc::clone(&self.guard)
    }
}

/// One frozen keyed Tool authorizer contribution with kernel-owned authority metadata.
#[derive(Clone)]
pub struct ToolAuthorizerBinding {
    key: String,
    owner: PluginId,
    authorizer: Arc<dyn ToolAuthorizer>,
}

impl ToolAuthorizerBinding {
    pub fn key(&self) -> &str {
        &self.key
    }

    pub const fn owner(&self) -> PluginId {
        self.owner
    }

    pub fn authorizer(&self) -> Arc<dyn ToolAuthorizer> {
        Arc::clone(&self.authorizer)
    }
}

/// One frozen Hook contribution in deterministic global execution order.
#[derive(Clone)]
pub struct HookBinding {
    owner: PluginId,
    ordinal: usize,
    hook: Arc<dyn Hook>,
}

impl HookBinding {
    pub const fn owner(&self) -> PluginId {
        self.owner
    }

    pub const fn ordinal(&self) -> usize {
        self.ordinal
    }

    pub fn hook(&self) -> Arc<dyn Hook> {
        Arc::clone(&self.hook)
    }
}

/// A typed asynchronous constructor staged by a selected provider plugin.
pub trait ServiceFactory<T: ?Sized + Send + Sync + 'static>: Send + Sync {
    fn construct<'a>(
        &'a self,
        ctx: FactoryContext<'a>,
    ) -> LifecycleFuture<'a, Result<ManagedService<T>, RuntimeError>>;
}

/// The registry whose declaration collided.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegistryKind {
    Agent,
    Tool,
    ToolAuthorizer,
    EventObserver,
}

/// The kernel lifecycle phase that failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LifecyclePhase {
    Construct,
    Start,
    Stop,
}

/// Owner-rich lifecycle diagnostics produced by the kernel.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LifecycleFailure {
    capability: CapabilityId,
    candidate: &'static str,
    plugin: PluginId,
    phase: LifecyclePhase,
    message: String,
}

impl LifecycleFailure {
    fn new(
        capability: CapabilityId,
        candidate: &'static str,
        plugin: PluginId,
        phase: LifecyclePhase,
        error: RuntimeError,
    ) -> Self {
        Self {
            capability,
            candidate,
            plugin,
            phase,
            message: error.to_string(),
        }
    }

    pub const fn capability(&self) -> CapabilityId {
        self.capability
    }

    pub const fn candidate(&self) -> &'static str {
        self.candidate
    }

    pub const fn plugin(&self) -> PluginId {
        self.plugin
    }

    pub const fn phase(&self) -> LifecyclePhase {
        self.phase
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for LifecycleFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "plugin `{}` candidate `{}` for capability `{}` failed during {:?}: {}",
            self.plugin, self.candidate, self.capability, self.phase, self.message
        )
    }
}

/// Aggregated failures from an explicit registry shutdown.
#[derive(Debug, thiserror::Error)]
#[error(
    "service shutdown failed for {count} service(s)",
    count = .failures.len()
)]
pub struct ShutdownError {
    failures: Vec<LifecycleFailure>,
}

impl ShutdownError {
    pub fn failures(&self) -> &[LifecycleFailure] {
        &self.failures
    }
}

/// The composition participant that requires a capability.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum CapabilityRequirementSource {
    CompositionRoot,
    Plugin(PluginId),
}

/// A stable provider candidate entry used in composition diagnostics.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ProviderDiagnostic {
    key: &'static str,
    owner: PluginId,
}

impl ProviderDiagnostic {
    pub const fn key(self) -> &'static str {
        self.key
    }

    pub const fn owner(self) -> PluginId {
        self.owner
    }
}

impl fmt::Display for RegistryKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self {
            Self::Agent => "agent",
            Self::Tool => "tool",
            Self::ToolAuthorizer => "tool authorizer",
            Self::EventObserver => "event Observer",
        };
        formatter.write_str(label)
    }
}

/// An error raised while mounting a plugin.
#[derive(Clone, Debug, thiserror::Error)]
pub enum PluginError {
    #[error("plugin identity `{plugin}` is already registered")]
    DuplicatePlugin { plugin: PluginId },
    #[error("plugin `{plugin}` requires missing plugin `{dependency}`")]
    MissingDependency {
        plugin: PluginId,
        dependency: PluginId,
    },
    #[error(
        "plugin dependency cycle: {cycle}",
        cycle = format_plugin_ids(.plugins)
    )]
    DependencyCycle { plugins: Vec<PluginId> },
    #[error("required capability `{capability}` has no provider; required by {required_by:?}")]
    MissingCapability {
        capability: CapabilityId,
        required_by: Vec<CapabilityRequirementSource>,
    },
    #[error("required capability `{capability}` is ambiguous; candidates: {candidates:?}")]
    AmbiguousCapability {
        capability: CapabilityId,
        candidates: Vec<ProviderDiagnostic>,
    },
    #[error(
        "selected provider `{selected}` is not registered for capability `{capability}`; candidates: {candidates:?}"
    )]
    UnknownProvider {
        capability: CapabilityId,
        selected: String,
        candidates: Vec<ProviderDiagnostic>,
    },
    #[error("capability `{capability}` has conflicting explicit selections: {selections:?}")]
    ConflictingSelection {
        capability: CapabilityId,
        selections: Vec<String>,
    },
    #[error(
        "duplicate provider candidate `{candidate}` for capability `{capability}`: incumbent plugin `{incumbent}`, challenger plugin `{challenger}`"
    )]
    DuplicateProviderCandidate {
        capability: CapabilityId,
        candidate: &'static str,
        incumbent: PluginId,
        challenger: PluginId,
    },
    #[error(
        "plugin `{plugin}` requires provider plugin `{dependency}` for capability `{capability}` by exact identity"
    )]
    ExactDependencyOnProvider {
        plugin: PluginId,
        dependency: PluginId,
        capability: CapabilityId,
    },
    #[error(
        "selected provider plugin `{provider}` did not provide a factory for capability `{capability}`"
    )]
    MissingProviderFactory {
        capability: CapabilityId,
        provider: PluginId,
    },
    #[error(
        "provider plugin `{provider}` provided more than one factory for capability `{capability}`"
    )]
    DuplicateProviderFactory {
        capability: CapabilityId,
        provider: PluginId,
    },
    #[error(
        "plugin `{plugin}` declared provider capability {declared:?} but attempted to provide a factory for capability `{provided}`"
    )]
    UnexpectedProviderFactory {
        plugin: PluginId,
        declared: Option<CapabilityId>,
        provided: CapabilityId,
    },
    #[error("plugin `{plugin}` failed to mount: {message}")]
    MountFailed { plugin: PluginId, message: String },
    #[error(
        "duplicate {registry} key `{key}`: incumbent plugin `{incumbent}`, challenger plugin `{challenger}`"
    )]
    Duplicate {
        registry: RegistryKind,
        key: String,
        incumbent: PluginId,
        challenger: PluginId,
    },
    #[error("service activation failed: {primary}")]
    ActivationFailed {
        primary: LifecycleFailure,
        rollback_failures: Vec<LifecycleFailure>,
    },
}

fn format_plugin_ids(plugins: &[PluginId]) -> String {
    plugins
        .iter()
        .map(PluginId::as_str)
        .collect::<Vec<_>>()
        .join(", ")
}

/// A declarative plugin seam. `mount` must not perform runtime work.
pub trait Plugin: Send + Sync {
    fn descriptor(&self) -> PluginDescriptor;

    fn mount(&self, ctx: &mut MountContext<'_>) -> Result<(), MountError>;
}

struct Owned<T: ?Sized> {
    owner: PluginId,
    value: Arc<T>,
}

struct LifecycleNode {
    capability: CapabilityId,
    candidate: &'static str,
    plugin: PluginId,
    lifecycle: Box<dyn ServiceLifecycle>,
}

/// The provider selected for one capability in a frozen composition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProviderBinding {
    key: &'static str,
    owner: PluginId,
}

impl ProviderBinding {
    pub const fn key(self) -> &'static str {
        self.key
    }

    pub const fn owner(self) -> PluginId {
        self.owner
    }
}

/// The frozen result of successful plugin composition.
pub struct PluginRegistry {
    bindings: BTreeMap<CapabilityId, ProviderBinding>,
    agents: BTreeMap<String, Owned<dyn Agent>>,
    agent_requirements: BTreeMap<String, &'static [CapabilityId]>,
    agent_runtime_factory: Option<Owned<dyn ServiceFactory<dyn AgentRuntime>>>,
    agent_runtime: Option<Owned<dyn AgentRuntime>>,
    llm_factory: Option<Owned<dyn ServiceFactory<dyn Llm>>>,
    llm: Option<Owned<dyn Llm>>,
    llm_runtime_factory: Option<Owned<dyn ServiceFactory<dyn LlmRuntime>>>,
    llm_runtime: Option<Owned<dyn LlmRuntime>>,
    tools: BTreeMap<String, Owned<dyn Tool>>,
    tool_guards: Vec<Owned<dyn ToolGuard>>,
    tool_authorizers: BTreeMap<String, Owned<dyn ToolAuthorizer>>,
    tool_runtime_factory: Option<Owned<dyn ServiceFactory<dyn ToolRuntime>>>,
    tool_runtime: Option<Owned<dyn ToolRuntime>>,
    session_persistence_factory: Option<Owned<dyn ServiceFactory<dyn SessionPersistence>>>,
    session_persistence: Option<Owned<dyn SessionPersistence>>,
    session_runtime_factory: Option<Owned<dyn ServiceFactory<dyn SessionRuntime>>>,
    session_runtime: Option<Owned<dyn SessionRuntime>>,
    event_observers: BTreeMap<String, Owned<dyn EventObserver>>,
    hooks: Vec<Owned<dyn Hook>>,
    lifecycles: AsyncMutex<Vec<LifecycleNode>>,
}

impl PluginRegistry {
    fn empty() -> Self {
        Self {
            bindings: BTreeMap::new(),
            agents: BTreeMap::new(),
            agent_requirements: BTreeMap::new(),
            agent_runtime_factory: None,
            agent_runtime: None,
            llm_factory: None,
            llm: None,
            llm_runtime_factory: None,
            llm_runtime: None,
            tools: BTreeMap::new(),
            tool_guards: Vec::new(),
            tool_authorizers: BTreeMap::new(),
            tool_runtime_factory: None,
            tool_runtime: None,
            session_persistence_factory: None,
            session_persistence: None,
            session_runtime_factory: None,
            session_runtime: None,
            event_observers: BTreeMap::new(),
            hooks: Vec::new(),
            lifecycles: AsyncMutex::new(Vec::new()),
        }
    }

    pub fn binding(&self, capability: CapabilityId) -> Option<ProviderBinding> {
        self.bindings.get(&capability).copied()
    }

    #[cfg(test)]
    fn agent(&self, key: &str) -> Option<Arc<dyn Agent>> {
        self.agents.get(key).map(|entry| Arc::clone(&entry.value))
    }

    pub fn agent_runtime(&self) -> Option<Arc<dyn AgentRuntime>> {
        self.agent_runtime
            .as_ref()
            .map(|entry| Arc::clone(&entry.value))
    }

    fn llm(&self) -> Option<Arc<dyn Llm>> {
        self.llm.as_ref().map(|entry| Arc::clone(&entry.value))
    }

    pub fn llm_runtime(&self) -> Option<Arc<dyn LlmRuntime>> {
        self.llm_runtime
            .as_ref()
            .map(|entry| Arc::clone(&entry.value))
    }

    #[cfg(test)]
    fn tool(&self, key: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(key).map(|entry| Arc::clone(&entry.value))
    }

    fn tool_runtime(&self) -> Option<Arc<dyn ToolRuntime>> {
        self.tool_runtime
            .as_ref()
            .map(|entry| Arc::clone(&entry.value))
    }

    fn session_persistence(&self) -> Option<Arc<dyn SessionPersistence>> {
        self.session_persistence
            .as_ref()
            .map(|entry| Arc::clone(&entry.value))
    }

    pub fn session_runtime(&self) -> Option<Arc<dyn SessionRuntime>> {
        self.session_runtime
            .as_ref()
            .map(|entry| Arc::clone(&entry.value))
    }

    fn event_observer_bindings(&self) -> Vec<EventObserverBinding> {
        self.event_observers
            .iter()
            .map(|(key, entry)| EventObserverBinding {
                key: key.clone(),
                owner: entry.owner,
                observer: Arc::clone(&entry.value),
            })
            .collect()
    }

    #[cfg(test)]
    fn hooks(&self) -> impl Iterator<Item = &Arc<dyn Hook>> {
        self.hooks.iter().map(|entry| &entry.value)
    }

    fn agent_bindings(&self) -> Vec<AgentBinding> {
        self.agents
            .iter()
            .map(|(key, entry)| AgentBinding {
                key: key.clone(),
                owner: entry.owner,
                required_capabilities: self.agent_requirements[key],
                agent: Arc::clone(&entry.value),
            })
            .collect()
    }

    fn tool_bindings(&self) -> Vec<ToolBinding> {
        self.tools
            .iter()
            .map(|(key, entry)| ToolBinding {
                key: key.clone(),
                owner: entry.owner,
                tool: Arc::clone(&entry.value),
            })
            .collect()
    }

    fn tool_guard_bindings(&self) -> Vec<ToolGuardBinding> {
        self.tool_guards
            .iter()
            .enumerate()
            .map(|(ordinal, entry)| ToolGuardBinding {
                owner: entry.owner,
                ordinal,
                guard: Arc::clone(&entry.value),
            })
            .collect()
    }

    fn tool_authorizer_bindings(&self) -> Vec<ToolAuthorizerBinding> {
        self.tool_authorizers
            .iter()
            .map(|(key, entry)| ToolAuthorizerBinding {
                key: key.clone(),
                owner: entry.owner,
                authorizer: Arc::clone(&entry.value),
            })
            .collect()
    }

    fn hook_bindings(&self) -> Vec<HookBinding> {
        self.hooks
            .iter()
            .enumerate()
            .map(|(ordinal, entry)| HookBinding {
                owner: entry.owner,
                ordinal,
                hook: Arc::clone(&entry.value),
            })
            .collect()
    }

    pub async fn shutdown(&self) -> Result<(), ShutdownError> {
        let mut ledger = self.lifecycles.lock().await;
        let nodes = std::mem::take(&mut *ledger);
        let failures = stop_nodes(nodes, StopReason::RequestedShutdown).await;
        if failures.is_empty() {
            Ok(())
        } else {
            Err(ShutdownError { failures })
        }
    }

    async fn activate(
        &mut self,
        submitted: &[SubmittedPlugin],
        order: &[usize],
    ) -> Result<(), PluginError> {
        for index in order {
            let descriptor = submitted[*index].descriptor;
            let Some(provider) = descriptor.provider_candidate() else {
                continue;
            };
            if provider.capability() == AGENT_RUNTIME {
                let owned_factory = self.agent_runtime_factory.as_ref().expect(
                    "a selected AgentRuntime provider plan was validated before activation",
                );
                debug_assert_eq!(owned_factory.owner, descriptor.id());
                let factory = Arc::clone(&owned_factory.value);
                let value = self
                    .construct_service(factory, descriptor, provider)
                    .await?;
                self.agent_runtime = Some(Owned {
                    owner: descriptor.id(),
                    value,
                });
            } else if provider.capability() == LLM_PROVIDER {
                let owned_factory = self
                    .llm_factory
                    .as_ref()
                    .expect("a selected LLM provider plan was validated before activation");
                debug_assert_eq!(owned_factory.owner, descriptor.id());
                let factory = Arc::clone(&owned_factory.value);
                let value = self
                    .construct_service(factory, descriptor, provider)
                    .await?;
                self.llm = Some(Owned {
                    owner: descriptor.id(),
                    value,
                });
            } else if provider.capability() == LLM_RUNTIME {
                let owned_factory = self
                    .llm_runtime_factory
                    .as_ref()
                    .expect("a selected LlmRuntime provider plan was validated before activation");
                debug_assert_eq!(owned_factory.owner, descriptor.id());
                let factory = Arc::clone(&owned_factory.value);
                let value = self
                    .construct_service(factory, descriptor, provider)
                    .await?;
                self.llm_runtime = Some(Owned {
                    owner: descriptor.id(),
                    value,
                });
            } else if provider.capability() == TOOL_RUNTIME {
                let owned_factory = self
                    .tool_runtime_factory
                    .as_ref()
                    .expect("a selected ToolRuntime provider plan was validated before activation");
                debug_assert_eq!(owned_factory.owner, descriptor.id());
                let factory = Arc::clone(&owned_factory.value);
                let value = self
                    .construct_service(factory, descriptor, provider)
                    .await?;
                self.tool_runtime = Some(Owned {
                    owner: descriptor.id(),
                    value,
                });
            } else if provider.capability() == SESSION_PERSISTENCE {
                let owned_factory = self.session_persistence_factory.as_ref().expect(
                    "a selected SessionPersistence provider plan was validated before activation",
                );
                debug_assert_eq!(owned_factory.owner, descriptor.id());
                let factory = Arc::clone(&owned_factory.value);
                let value = self
                    .construct_service(factory, descriptor, provider)
                    .await?;
                self.session_persistence = Some(Owned {
                    owner: descriptor.id(),
                    value,
                });
            } else if provider.capability() == SESSION_RUNTIME {
                let owned_factory = self.session_runtime_factory.as_ref().expect(
                    "a selected SessionRuntime provider plan was validated before activation",
                );
                debug_assert_eq!(owned_factory.owner, descriptor.id());
                let factory = Arc::clone(&owned_factory.value);
                let value = self
                    .construct_service(factory, descriptor, provider)
                    .await?;
                self.session_runtime = Some(Owned {
                    owner: descriptor.id(),
                    value,
                });
            }
        }

        self.agent_runtime_factory = None;
        self.llm_factory = None;
        self.llm_runtime_factory = None;
        self.tool_runtime_factory = None;
        self.session_persistence_factory = None;
        self.session_runtime_factory = None;
        let lifecycle_count = self.lifecycles.get_mut().len();
        for index in 0..lifecycle_count {
            let (capability, candidate, plugin, result) = {
                let node = &mut self.lifecycles.get_mut()[index];
                (
                    node.capability,
                    node.candidate,
                    node.plugin,
                    node.lifecycle.start().await,
                )
            };
            if let Err(error) = result {
                let primary = LifecycleFailure::new(
                    capability,
                    candidate,
                    plugin,
                    LifecyclePhase::Start,
                    error,
                );
                return Err(self.rollback_activation(primary).await);
            }
        }
        Ok(())
    }

    async fn construct_service<T: ?Sized + Send + Sync + 'static>(
        &mut self,
        factory: Arc<dyn ServiceFactory<T>>,
        descriptor: PluginDescriptor,
        provider: ProviderCandidate,
    ) -> Result<Arc<T>, PluginError> {
        let managed = match factory
            .construct(FactoryContext::new(self, descriptor))
            .await
        {
            Ok(managed) => managed,
            Err(error) => {
                let primary = LifecycleFailure::new(
                    provider.capability(),
                    provider.key(),
                    descriptor.id(),
                    LifecyclePhase::Construct,
                    error,
                );
                return Err(self.rollback_activation(primary).await);
            }
        };
        let (value, lifecycle) = managed.into_parts();
        self.lifecycles.get_mut().push(LifecycleNode {
            capability: provider.capability(),
            candidate: provider.key(),
            plugin: descriptor.id(),
            lifecycle,
        });
        Ok(value)
    }

    async fn rollback_activation(&mut self, primary: LifecycleFailure) -> PluginError {
        let nodes = std::mem::take(self.lifecycles.get_mut());
        let rollback_failures = stop_nodes(nodes, StopReason::ActivationRollback).await;
        PluginError::ActivationFailed {
            primary,
            rollback_failures,
        }
    }

    fn validate(&self, plan: &RegistrationPlan) -> Result<(), PluginError> {
        debug_assert!(plan.declaration_error.is_none());
        validate_keys(RegistryKind::Agent, &self.agents, &plan.agents, plan.owner)?;
        validate_keys(RegistryKind::Tool, &self.tools, &plan.tools, plan.owner)?;
        validate_keys(
            RegistryKind::ToolAuthorizer,
            &self.tool_authorizers,
            &plan.tool_authorizers,
            plan.owner,
        )?;
        validate_keys(
            RegistryKind::EventObserver,
            &self.event_observers,
            &plan.event_observers,
            plan.owner,
        )?;
        Ok(())
    }

    fn commit(&mut self, plan: RegistrationPlan) {
        let RegistrationPlan {
            owner,
            required_capabilities,
            agents,
            agent_runtime_factory,
            llm_factory,
            llm_runtime_factory,
            tools,
            tool_guards,
            tool_authorizers,
            tool_runtime_factory,
            session_persistence_factory,
            session_runtime_factory,
            event_observers,
            hooks,
            declaration_error: _,
            provider: _,
        } = plan;

        for (key, value) in agents {
            let previous = self
                .agent_requirements
                .insert(key.clone(), required_capabilities);
            debug_assert!(previous.is_none());
            insert_owned(&mut self.agents, key, value, owner);
        }
        if let Some(value) = agent_runtime_factory {
            assert!(
                self.agent_runtime_factory.is_none(),
                "a staged composition must not overwrite the selected AgentRuntime factory slot"
            );
            self.agent_runtime_factory = Some(Owned { owner, value });
        }
        if let Some(value) = llm_factory {
            assert!(
                self.llm_factory.is_none(),
                "a staged composition must not overwrite the selected LLM factory slot"
            );
            self.llm_factory = Some(Owned { owner, value });
        }
        if let Some(value) = llm_runtime_factory {
            assert!(
                self.llm_runtime_factory.is_none(),
                "a staged composition must not overwrite the selected LlmRuntime factory slot"
            );
            self.llm_runtime_factory = Some(Owned { owner, value });
        }
        for (key, value) in tools {
            insert_owned(&mut self.tools, key, value, owner);
        }
        self.tool_guards
            .extend(tool_guards.into_iter().map(|value| Owned { owner, value }));
        for (key, value) in tool_authorizers {
            insert_owned(&mut self.tool_authorizers, key, value, owner);
        }
        if let Some(value) = tool_runtime_factory {
            assert!(
                self.tool_runtime_factory.is_none(),
                "a staged composition must not overwrite the selected ToolRuntime factory slot"
            );
            self.tool_runtime_factory = Some(Owned { owner, value });
        }
        if let Some(value) = session_persistence_factory {
            assert!(
                self.session_persistence_factory.is_none(),
                "a staged composition must not overwrite the selected SessionPersistence factory slot"
            );
            self.session_persistence_factory = Some(Owned { owner, value });
        }
        if let Some(value) = session_runtime_factory {
            assert!(
                self.session_runtime_factory.is_none(),
                "a staged composition must not overwrite the selected SessionRuntime factory slot"
            );
            self.session_runtime_factory = Some(Owned { owner, value });
        }
        for (key, value) in event_observers {
            insert_owned(&mut self.event_observers, key, value, owner);
        }
        self.hooks
            .extend(hooks.into_iter().map(|value| Owned { owner, value }));
    }
}

async fn stop_nodes(nodes: Vec<LifecycleNode>, reason: StopReason) -> Vec<LifecycleFailure> {
    let mut failures = Vec::new();
    for node in nodes.into_iter().rev() {
        let LifecycleNode {
            capability,
            candidate,
            plugin,
            lifecycle,
        } = node;
        if let Err(error) = lifecycle.stop(reason).await {
            failures.push(LifecycleFailure::new(
                capability,
                candidate,
                plugin,
                LifecyclePhase::Stop,
                error,
            ));
        }
    }
    failures
}

fn validate_keys<T: ?Sized>(
    registry: RegistryKind,
    committed: &BTreeMap<String, Owned<T>>,
    staged: &BTreeMap<String, Arc<T>>,
    challenger: PluginId,
) -> Result<(), PluginError> {
    for key in staged.keys() {
        if let Some(incumbent) = committed.get(key) {
            return Err(PluginError::Duplicate {
                registry,
                key: key.clone(),
                incumbent: incumbent.owner,
                challenger,
            });
        }
    }
    Ok(())
}

fn insert_owned<T: ?Sized>(
    entries: &mut BTreeMap<String, Owned<T>>,
    key: String,
    value: Arc<T>,
    owner: PluginId,
) {
    let previous = entries.insert(key, Owned { owner, value });
    debug_assert!(
        previous.is_none(),
        "commit must be validated before mutation"
    );
}

struct RegistrationPlan {
    owner: PluginId,
    required_capabilities: &'static [CapabilityId],
    provider: Option<ProviderCandidate>,
    agents: BTreeMap<String, Arc<dyn Agent>>,
    agent_runtime_factory: Option<Arc<dyn ServiceFactory<dyn AgentRuntime>>>,
    llm_factory: Option<Arc<dyn ServiceFactory<dyn Llm>>>,
    llm_runtime_factory: Option<Arc<dyn ServiceFactory<dyn LlmRuntime>>>,
    tools: BTreeMap<String, Arc<dyn Tool>>,
    tool_guards: Vec<Arc<dyn ToolGuard>>,
    tool_authorizers: BTreeMap<String, Arc<dyn ToolAuthorizer>>,
    tool_runtime_factory: Option<Arc<dyn ServiceFactory<dyn ToolRuntime>>>,
    session_persistence_factory: Option<Arc<dyn ServiceFactory<dyn SessionPersistence>>>,
    session_runtime_factory: Option<Arc<dyn ServiceFactory<dyn SessionRuntime>>>,
    event_observers: BTreeMap<String, Arc<dyn EventObserver>>,
    hooks: Vec<Arc<dyn Hook>>,
    declaration_error: Option<PluginError>,
}

impl RegistrationPlan {
    fn new(descriptor: PluginDescriptor) -> Self {
        Self {
            owner: descriptor.id(),
            required_capabilities: descriptor.capability_dependencies(),
            provider: descriptor.provider_candidate(),
            agents: BTreeMap::new(),
            agent_runtime_factory: None,
            llm_factory: None,
            llm_runtime_factory: None,
            tools: BTreeMap::new(),
            tool_guards: Vec::new(),
            tool_authorizers: BTreeMap::new(),
            tool_runtime_factory: None,
            session_persistence_factory: None,
            session_runtime_factory: None,
            event_observers: BTreeMap::new(),
            hooks: Vec::new(),
            declaration_error: None,
        }
    }

    fn reject(&mut self, error: PluginError) -> MountError {
        let message = error.to_string();
        if self.declaration_error.is_none() {
            self.declaration_error = Some(error);
        }
        MountError::new(message)
    }
}

/// A plugin-local declaration context.
pub struct MountContext<'a> {
    plan: &'a mut RegistrationPlan,
}

macro_rules! register_keyed {
    ($method:ident, $field:ident, $trait:path, $kind:expr) => {
        pub fn $method(&mut self, key: &str, value: Arc<dyn $trait>) -> Result<(), MountError> {
            if self.plan.$field.contains_key(key) {
                let error = PluginError::Duplicate {
                    registry: $kind,
                    key: key.to_string(),
                    incumbent: self.plan.owner,
                    challenger: self.plan.owner,
                };
                return Err(self.plan.reject(error));
            }
            self.plan.$field.insert(key.to_string(), value);
            Ok(())
        }
    };
}

impl MountContext<'_> {
    register_keyed!(register_agent, agents, Agent, RegistryKind::Agent);
    register_keyed!(register_tool, tools, Tool, RegistryKind::Tool);
    register_keyed!(
        register_tool_authorizer,
        tool_authorizers,
        ToolAuthorizer,
        RegistryKind::ToolAuthorizer
    );
    register_keyed!(
        register_event_observer,
        event_observers,
        EventObserver,
        RegistryKind::EventObserver
    );

    pub fn provide_agent_runtime_factory(
        &mut self,
        factory: Arc<dyn ServiceFactory<dyn AgentRuntime>>,
    ) -> Result<(), MountError> {
        let declared = self.plan.provider.map(ProviderCandidate::capability);
        if declared != Some(AGENT_RUNTIME) {
            let error = PluginError::UnexpectedProviderFactory {
                plugin: self.plan.owner,
                declared,
                provided: AGENT_RUNTIME,
            };
            return Err(self.plan.reject(error));
        }
        if self.plan.agent_runtime_factory.is_some() {
            let error = PluginError::DuplicateProviderFactory {
                capability: AGENT_RUNTIME,
                provider: self.plan.owner,
            };
            return Err(self.plan.reject(error));
        }
        self.plan.agent_runtime_factory = Some(factory);
        Ok(())
    }

    pub fn provide_llm_factory(
        &mut self,
        factory: Arc<dyn ServiceFactory<dyn Llm>>,
    ) -> Result<(), MountError> {
        let declared = self.plan.provider.map(ProviderCandidate::capability);
        if declared != Some(LLM_PROVIDER) {
            let error = PluginError::UnexpectedProviderFactory {
                plugin: self.plan.owner,
                declared,
                provided: LLM_PROVIDER,
            };
            return Err(self.plan.reject(error));
        }
        if self.plan.llm_factory.is_some() {
            let error = PluginError::DuplicateProviderFactory {
                capability: LLM_PROVIDER,
                provider: self.plan.owner,
            };
            return Err(self.plan.reject(error));
        }
        self.plan.llm_factory = Some(factory);
        Ok(())
    }

    pub fn provide_llm_runtime_factory(
        &mut self,
        factory: Arc<dyn ServiceFactory<dyn LlmRuntime>>,
    ) -> Result<(), MountError> {
        let declared = self.plan.provider.map(ProviderCandidate::capability);
        if declared != Some(LLM_RUNTIME) {
            let error = PluginError::UnexpectedProviderFactory {
                plugin: self.plan.owner,
                declared,
                provided: LLM_RUNTIME,
            };
            return Err(self.plan.reject(error));
        }
        if self.plan.llm_runtime_factory.is_some() {
            let error = PluginError::DuplicateProviderFactory {
                capability: LLM_RUNTIME,
                provider: self.plan.owner,
            };
            return Err(self.plan.reject(error));
        }
        self.plan.llm_runtime_factory = Some(factory);
        Ok(())
    }

    pub fn provide_tool_runtime_factory(
        &mut self,
        factory: Arc<dyn ServiceFactory<dyn ToolRuntime>>,
    ) -> Result<(), MountError> {
        let declared = self.plan.provider.map(ProviderCandidate::capability);
        if declared != Some(TOOL_RUNTIME) {
            let error = PluginError::UnexpectedProviderFactory {
                plugin: self.plan.owner,
                declared,
                provided: TOOL_RUNTIME,
            };
            return Err(self.plan.reject(error));
        }
        if self.plan.tool_runtime_factory.is_some() {
            let error = PluginError::DuplicateProviderFactory {
                capability: TOOL_RUNTIME,
                provider: self.plan.owner,
            };
            return Err(self.plan.reject(error));
        }
        self.plan.tool_runtime_factory = Some(factory);
        Ok(())
    }

    pub fn provide_session_persistence_factory(
        &mut self,
        factory: Arc<dyn ServiceFactory<dyn SessionPersistence>>,
    ) -> Result<(), MountError> {
        let declared = self.plan.provider.map(ProviderCandidate::capability);
        if declared != Some(SESSION_PERSISTENCE) {
            let error = PluginError::UnexpectedProviderFactory {
                plugin: self.plan.owner,
                declared,
                provided: SESSION_PERSISTENCE,
            };
            return Err(self.plan.reject(error));
        }
        if self.plan.session_persistence_factory.is_some() {
            let error = PluginError::DuplicateProviderFactory {
                capability: SESSION_PERSISTENCE,
                provider: self.plan.owner,
            };
            return Err(self.plan.reject(error));
        }
        self.plan.session_persistence_factory = Some(factory);
        Ok(())
    }

    pub fn provide_session_runtime_factory(
        &mut self,
        factory: Arc<dyn ServiceFactory<dyn SessionRuntime>>,
    ) -> Result<(), MountError> {
        let declared = self.plan.provider.map(ProviderCandidate::capability);
        if declared != Some(SESSION_RUNTIME) {
            let error = PluginError::UnexpectedProviderFactory {
                plugin: self.plan.owner,
                declared,
                provided: SESSION_RUNTIME,
            };
            return Err(self.plan.reject(error));
        }
        if self.plan.session_runtime_factory.is_some() {
            let error = PluginError::DuplicateProviderFactory {
                capability: SESSION_RUNTIME,
                provider: self.plan.owner,
            };
            return Err(self.plan.reject(error));
        }
        self.plan.session_runtime_factory = Some(factory);
        Ok(())
    }

    pub fn register_hook(&mut self, hook: Arc<dyn Hook>) {
        self.plan.hooks.push(hook);
    }

    pub fn register_tool_guard(&mut self, guard: Arc<dyn ToolGuard>) {
        self.plan.tool_guards.push(guard);
    }
}

/// Builds a frozen [`PluginRegistry`] from plugins.
#[derive(Default)]
pub struct Registrar {
    plugins: Vec<Box<dyn Plugin>>,
    requirements: Vec<CapabilityId>,
    selections: Vec<CapabilitySelection>,
}

struct CapabilitySelection {
    capability: CapabilityId,
    candidate: String,
}

impl Registrar {
    pub fn add<P: Plugin + 'static>(&mut self, plugin: P) {
        self.plugins.push(Box::new(plugin));
    }

    pub fn select(&mut self, capability: CapabilityId, candidate: impl Into<String>) {
        self.selections.push(CapabilitySelection {
            capability,
            candidate: candidate.into(),
        });
    }

    pub fn require(&mut self, capability: CapabilityId) {
        self.requirements.push(capability);
    }

    pub async fn finish(self) -> Result<PluginRegistry, PluginError> {
        let Registrar {
            plugins,
            requirements,
            selections,
        } = self;
        let submitted = plugins
            .into_iter()
            .map(|plugin| SubmittedPlugin {
                descriptor: plugin.descriptor(),
                plugin,
            })
            .collect::<Vec<_>>();
        let descriptors = submitted
            .iter()
            .map(|entry| entry.descriptor)
            .collect::<Vec<_>>();
        graph::validate_duplicate_plugins(&descriptors)?;
        validate_provider_candidates(&descriptors)?;
        graph::validate_missing_dependencies(&descriptors)?;
        validate_exact_provider_dependencies(&descriptors)?;
        graph::resolve_exact(&descriptors)?;

        let mut explicit_selections = BTreeMap::<CapabilityId, BTreeSet<String>>::new();
        for selection in selections {
            explicit_selections
                .entry(selection.capability)
                .or_default()
                .insert(selection.candidate);
        }
        if let Some((capability, selections)) = explicit_selections
            .iter()
            .find(|(_, selections)| selections.len() > 1)
        {
            return Err(PluginError::ConflictingSelection {
                capability: *capability,
                selections: selections.iter().cloned().collect(),
            });
        }
        let explicitly_selected = explicit_selections.keys().copied().collect::<BTreeSet<_>>();
        let mut bindings = BTreeMap::new();
        for (capability, selected) in &explicit_selections {
            if selected.len() != 1 {
                continue;
            }
            let selected = selected
                .first()
                .expect("a single explicit selection must have one candidate");
            if let Some(descriptor) = descriptors.iter().find(|descriptor| {
                descriptor.provider_candidate().is_some_and(|candidate| {
                    candidate.capability() == *capability && candidate.key() == selected
                })
            }) {
                let candidate = descriptor
                    .provider_candidate()
                    .expect("the selected descriptor was matched by its provider candidate");
                bindings.insert(
                    *capability,
                    ProviderBinding {
                        key: candidate.key(),
                        owner: descriptor.id(),
                    },
                );
            } else {
                return Err(PluginError::UnknownProvider {
                    capability: *capability,
                    selected: selected.clone(),
                    candidates: provider_diagnostics(&descriptors, *capability),
                });
            }
        }
        let mut required_capabilities =
            BTreeMap::<CapabilityId, BTreeSet<CapabilityRequirementSource>>::new();
        for capability in requirements {
            required_capabilities
                .entry(capability)
                .or_default()
                .insert(CapabilityRequirementSource::CompositionRoot);
        }
        loop {
            let mut changed = false;
            for descriptor in descriptors.iter().filter(|descriptor| {
                descriptor.provider_candidate().is_none()
                    || bindings
                        .values()
                        .any(|binding| binding.owner() == descriptor.id())
            }) {
                for capability in descriptor.capability_dependencies() {
                    changed |= required_capabilities
                        .entry(*capability)
                        .or_default()
                        .insert(CapabilityRequirementSource::Plugin(descriptor.id()));
                }
            }
            for capability in required_capabilities.keys().copied().collect::<Vec<_>>() {
                if explicitly_selected.contains(&capability) || bindings.contains_key(&capability) {
                    continue;
                }
                let mut candidates = descriptors.iter().filter_map(|descriptor| {
                    descriptor
                        .provider_candidate()
                        .filter(|candidate| candidate.capability() == capability)
                        .map(|candidate| (candidate, descriptor.id()))
                });
                let candidate = candidates.next();
                if let (Some((candidate, owner)), None) = (candidate, candidates.next()) {
                    bindings.insert(
                        capability,
                        ProviderBinding {
                            key: candidate.key(),
                            owner,
                        },
                    );
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        for (capability, required_by) in &required_capabilities {
            if explicitly_selected.contains(capability) || bindings.contains_key(capability) {
                continue;
            }
            let has_candidate = descriptors.iter().any(|descriptor| {
                descriptor
                    .provider_candidate()
                    .is_some_and(|candidate| candidate.capability() == *capability)
            });
            if !has_candidate {
                return Err(PluginError::MissingCapability {
                    capability: *capability,
                    required_by: required_by.iter().copied().collect(),
                });
            }
        }
        for capability in required_capabilities.keys() {
            if explicitly_selected.contains(capability) || bindings.contains_key(capability) {
                continue;
            }
            let candidates = provider_diagnostics(&descriptors, *capability);
            if candidates.len() > 1 {
                return Err(PluginError::AmbiguousCapability {
                    capability: *capability,
                    candidates,
                });
            }
        }

        let active = descriptors
            .iter()
            .filter_map(|descriptor| {
                (descriptor.provider_candidate().is_none()
                    || bindings
                        .values()
                        .any(|binding| binding.owner() == descriptor.id()))
                .then_some(descriptor.id())
            })
            .collect::<BTreeSet<_>>();
        let capability_dependencies = descriptors
            .iter()
            .filter(|descriptor| active.contains(&descriptor.id()))
            .map(|descriptor| {
                let dependencies = descriptor
                    .capability_dependencies()
                    .iter()
                    .chain(descriptor.optional_capability_dependencies())
                    .filter_map(|capability| bindings.get(capability))
                    .map(|binding| binding.owner())
                    .collect::<BTreeSet<_>>();
                (descriptor.id(), dependencies)
            })
            .collect::<BTreeMap<_, _>>();
        let order = graph::resolve_active(&descriptors, &active, &capability_dependencies)?;
        let mut registry = PluginRegistry::empty();
        registry.bindings = bindings;

        for index in &order {
            let entry = &submitted[*index];
            mount_plugin(&mut registry, entry.descriptor, entry.plugin.as_ref())?;
        }

        registry.activate(&submitted, &order).await?;

        Ok(registry)
    }
}

fn provider_diagnostics(
    descriptors: &[PluginDescriptor],
    capability: CapabilityId,
) -> Vec<ProviderDiagnostic> {
    descriptors
        .iter()
        .filter_map(|descriptor| {
            descriptor
                .provider_candidate()
                .filter(|candidate| candidate.capability() == capability)
                .map(|candidate| ProviderDiagnostic {
                    key: candidate.key(),
                    owner: descriptor.id(),
                })
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn validate_provider_candidates(descriptors: &[PluginDescriptor]) -> Result<(), PluginError> {
    let mut providers = BTreeMap::<(CapabilityId, &'static str), BTreeSet<PluginId>>::new();
    for descriptor in descriptors {
        if let Some(candidate) = descriptor.provider_candidate() {
            providers
                .entry((candidate.capability(), candidate.key()))
                .or_default()
                .insert(descriptor.id());
        }
    }
    if let Some(((capability, candidate), owners)) =
        providers.iter().find(|(_, owners)| owners.len() > 1)
    {
        let mut owners = owners.iter().copied();
        let incumbent = owners
            .next()
            .expect("a duplicate provider candidate must have an incumbent");
        let challenger = owners
            .next()
            .expect("a duplicate provider candidate must have a challenger");
        return Err(PluginError::DuplicateProviderCandidate {
            capability: *capability,
            candidate,
            incumbent,
            challenger,
        });
    }
    Ok(())
}

fn validate_exact_provider_dependencies(
    descriptors: &[PluginDescriptor],
) -> Result<(), PluginError> {
    let providers = descriptors
        .iter()
        .filter_map(|descriptor| {
            descriptor
                .provider_candidate()
                .map(|candidate| (descriptor.id(), candidate.capability()))
        })
        .collect::<BTreeMap<_, _>>();
    let mut invalid = BTreeSet::new();
    for descriptor in descriptors {
        for dependency in descriptor.plugin_dependencies() {
            if let Some(capability) = providers.get(dependency) {
                invalid.insert((descriptor.id(), *dependency, *capability));
            }
        }
    }
    if let Some((plugin, dependency, capability)) = invalid.first().copied() {
        return Err(PluginError::ExactDependencyOnProvider {
            plugin,
            dependency,
            capability,
        });
    }
    Ok(())
}

struct SubmittedPlugin {
    descriptor: PluginDescriptor,
    plugin: Box<dyn Plugin>,
}

fn mount_plugin(
    registry: &mut PluginRegistry,
    descriptor: PluginDescriptor,
    plugin: &dyn Plugin,
) -> Result<(), PluginError> {
    let owner = descriptor.id();
    let mut plan = RegistrationPlan::new(descriptor);
    let mount_result = plugin.mount(&mut MountContext { plan: &mut plan });

    if let Some(error) = plan.declaration_error.take() {
        return Err(error);
    }
    if let Err(error) = mount_result {
        return Err(PluginError::MountFailed {
            plugin: owner,
            message: error.to_string(),
        });
    }
    if plan
        .provider
        .is_some_and(|provider| provider.capability() == AGENT_RUNTIME)
        && plan.agent_runtime_factory.is_none()
    {
        return Err(PluginError::MissingProviderFactory {
            capability: AGENT_RUNTIME,
            provider: owner,
        });
    }
    if plan
        .provider
        .is_some_and(|provider| provider.capability() == LLM_PROVIDER)
        && plan.llm_factory.is_none()
    {
        return Err(PluginError::MissingProviderFactory {
            capability: LLM_PROVIDER,
            provider: owner,
        });
    }
    if plan
        .provider
        .is_some_and(|provider| provider.capability() == LLM_RUNTIME)
        && plan.llm_runtime_factory.is_none()
    {
        return Err(PluginError::MissingProviderFactory {
            capability: LLM_RUNTIME,
            provider: owner,
        });
    }
    if plan
        .provider
        .is_some_and(|provider| provider.capability() == TOOL_RUNTIME)
        && plan.tool_runtime_factory.is_none()
    {
        return Err(PluginError::MissingProviderFactory {
            capability: TOOL_RUNTIME,
            provider: owner,
        });
    }
    if plan
        .provider
        .is_some_and(|provider| provider.capability() == SESSION_PERSISTENCE)
        && plan.session_persistence_factory.is_none()
    {
        return Err(PluginError::MissingProviderFactory {
            capability: SESSION_PERSISTENCE,
            provider: owner,
        });
    }
    if plan
        .provider
        .is_some_and(|provider| provider.capability() == SESSION_RUNTIME)
        && plan.session_runtime_factory.is_none()
    {
        return Err(PluginError::MissingProviderFactory {
            capability: SESSION_RUNTIME,
            provider: owner,
        });
    }

    registry.validate(&plan)?;
    registry.commit(plan);
    Ok(())
}

/// Lifecycle observation hook. Hooks cannot change runtime semantics.
pub trait Hook: Send + Sync {
    fn on_turn_start(&self, _session_id: &SessionId, _turn_id: &TurnId) {}
    fn on_turn_finally(&self, _observation: &TurnFinally) {}
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::pin::Pin;

    use jingwei_agent::{AgentContext, AgentError, AgentTurnInput, AgentTurnOutput};
    use jingwei_core::{
        CancellationSignal, EventObserver, ObserverError, ObserverFuture, SessionEvent,
    };
    use jingwei_session::{PersistAppendOutcome, SessionPersistence, SessionPersistenceError};
    use jingwei_tool::{ToolBodyError, ToolBodyRequest, ToolMetadata};

    use super::*;

    struct TestAgent;

    impl Agent for TestAgent {
        fn run_turn<'a>(
            &'a self,
            _input: AgentTurnInput<'a>,
            _ctx: &'a dyn AgentContext,
        ) -> Pin<Box<dyn Future<Output = Result<AgentTurnOutput, AgentError>> + Send + 'a>>
        {
            Box::pin(async {
                Err(AgentError::failed(
                    "unused_test_agent",
                    "unused test agent",
                    false,
                ))
            })
        }
    }

    struct TestTool;

    impl Tool for TestTool {
        fn metadata(&self) -> ToolMetadata {
            ToolMetadata::new("unused test tool", Default::default())
        }

        fn execute<'a>(
            &'a self,
            _request: ToolBodyRequest<'a>,
            _cancellation: Arc<dyn CancellationSignal>,
        ) -> Pin<Box<dyn Future<Output = Result<String, ToolBodyError>> + Send + 'a>> {
            Box::pin(async { Err(ToolBodyError::new("unused", "unused test tool", false)) })
        }
    }

    struct TestObserver;

    impl EventObserver for TestObserver {
        fn observe(
            &self,
            _event: Arc<SessionEvent>,
        ) -> ObserverFuture<'_, Result<(), ObserverError>> {
            Box::pin(async { Ok(()) })
        }
    }

    struct TestHook;

    impl Hook for TestHook {}

    struct TestPersistence;

    impl SessionPersistence for TestPersistence {
        fn load<'a>(
            &'a self,
            _session_id: &'a SessionId,
        ) -> Pin<
            Box<
                dyn Future<Output = Result<Vec<SessionEvent>, SessionPersistenceError>> + Send + 'a,
            >,
        > {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn commit_durable<'a>(
            &'a self,
            _event: &'a SessionEvent,
        ) -> Pin<
            Box<
                dyn Future<Output = Result<PersistAppendOutcome, SessionPersistenceError>>
                    + Send
                    + 'a,
            >,
        > {
            Box::pin(async { Ok(PersistAppendOutcome::Appended) })
        }
    }

    struct TestPersistenceFactory;

    impl ServiceFactory<dyn SessionPersistence> for TestPersistenceFactory {
        fn construct<'a>(
            &'a self,
            _ctx: FactoryContext<'a>,
        ) -> LifecycleFuture<'a, Result<ManagedService<dyn SessionPersistence>, RuntimeError>>
        {
            Box::pin(async {
                let persistence: Arc<dyn SessionPersistence> = Arc::new(TestPersistence);
                Ok(ManagedService::ready(persistence))
            })
        }
    }

    struct FailsAfterStaging;

    impl Plugin for FailsAfterStaging {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor::new("fails-after-staging")
        }

        fn mount(&self, ctx: &mut MountContext<'_>) -> Result<(), MountError> {
            ctx.register_agent("staged-agent", Arc::new(TestAgent))?;
            Err(MountError::new("mount aborted"))
        }
    }

    #[test]
    fn custom_mount_failure_does_not_commit_staged_contributions() {
        let mut registry = PluginRegistry::empty();
        let error = mount_plugin(
            &mut registry,
            PluginDescriptor::new("fails-after-staging"),
            &FailsAfterStaging,
        )
        .expect_err("the test plugin should fail after staging");

        assert!(matches!(error, PluginError::MountFailed { .. }));
        assert!(registry.agent("staged-agent").is_none());
    }

    struct ObserverOwner;

    impl Plugin for ObserverOwner {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor::new("observer-owner")
        }

        fn mount(&self, ctx: &mut MountContext<'_>) -> Result<(), MountError> {
            ctx.register_event_observer("shared", Arc::new(TestObserver))
        }
    }

    struct CrossRegistryChallenger;

    impl Plugin for CrossRegistryChallenger {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor::provider(
                "cross-registry-challenger",
                SESSION_PERSISTENCE,
                "challenger",
            )
        }

        fn mount(&self, ctx: &mut MountContext<'_>) -> Result<(), MountError> {
            ctx.provide_session_persistence_factory(Arc::new(TestPersistenceFactory))?;
            ctx.register_agent("challenger-agent", Arc::new(TestAgent))?;
            ctx.register_tool("challenger-tool", Arc::new(TestTool))?;
            ctx.register_event_observer("shared", Arc::new(TestObserver))?;
            ctx.register_hook(Arc::new(TestHook));
            Ok(())
        }
    }

    #[test]
    fn keyed_commit_conflict_does_not_commit_staged_typed_value_maps_or_hooks() {
        let mut registry = PluginRegistry::empty();
        mount_plugin(
            &mut registry,
            PluginDescriptor::new("observer-owner"),
            &ObserverOwner,
        )
        .expect("the incumbent should mount");

        mount_plugin(
            &mut registry,
            CrossRegistryChallenger.descriptor(),
            &CrossRegistryChallenger,
        )
        .expect_err("the duplicate Observer should reject the challenger plan");

        assert!(registry.agent("challenger-agent").is_none());
        assert!(registry.tool("challenger-tool").is_none());
        assert!(registry.session_persistence().is_none());
        assert!(registry.session_persistence_factory.is_none());
        assert_eq!(registry.hooks().count(), 0);
    }

    struct IgnoresDeclarationError;

    impl Plugin for IgnoresDeclarationError {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor::new("ignores-declaration-error")
        }

        fn mount(&self, ctx: &mut MountContext<'_>) -> Result<(), MountError> {
            ctx.register_agent("duplicate", Arc::new(TestAgent))?;
            let _ = ctx.register_agent("duplicate", Arc::new(TestAgent));
            Ok(())
        }
    }

    #[test]
    fn ignored_registration_error_does_not_commit_the_plan() {
        let mut registry = PluginRegistry::empty();
        mount_plugin(
            &mut registry,
            PluginDescriptor::new("ignores-declaration-error"),
            &IgnoresDeclarationError,
        )
        .expect_err("the latched declaration error should reject the plan");

        assert!(registry.agent("duplicate").is_none());
    }

    struct MissingLlmAfterStagingAgent;

    impl Plugin for MissingLlmAfterStagingAgent {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor::provider("missing-llm-after-agent", LLM_PROVIDER, "missing")
        }

        fn mount(&self, ctx: &mut MountContext<'_>) -> Result<(), MountError> {
            ctx.register_agent("must-not-commit", Arc::new(TestAgent))
        }
    }

    #[test]
    fn missing_typed_provider_factory_does_not_commit_ancillary_contributions() {
        let mut registry = PluginRegistry::empty();
        let error = mount_plugin(
            &mut registry,
            MissingLlmAfterStagingAgent.descriptor(),
            &MissingLlmAfterStagingAgent,
        )
        .expect_err("the selected typed provider omitted its required factory");

        assert!(matches!(
            error,
            PluginError::MissingProviderFactory {
                capability: LLM_PROVIDER,
                provider
            } if provider == PluginId::new("missing-llm-after-agent")
        ));
        assert!(registry.agent("must-not-commit").is_none());
        assert!(registry.llm().is_none());
    }
}
