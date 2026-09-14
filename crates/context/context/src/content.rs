//! Host-scoped, bounded memory content references. No filesystem path resolution.
use std::collections::BTreeMap;
use std::sync::Mutex;

use crate::ViewError;
use jingwei_core::event::{ToolFailureCategory, ToolRecordedOutcome};
use jingwei_core::{EventId, SessionId, TaskId};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ContentScope {
    pub session_id: SessionId,
    pub task_id: TaskId,
    pub expires_at_ms: u64,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ContentReference {
    /// Opaque host namespace, not a path or URL.
    pub store_id: String,
    pub source_event: EventId,
    pub scope: ContentScope,
    pub total_bytes: usize,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ContentPage {
    pub text: String,
    pub next_offset: Option<usize>,
    pub total_bytes: usize,
}

/// The host supplies scope/time from trusted state, never from model arguments.
/// Implementations must enforce immutable source binding, expiry and read bounds.
pub trait ContentStore: Send + Sync {
    fn put(
        &self,
        scope: &ContentScope,
        source: &EventId,
        text: &str,
        now_ms: u64,
    ) -> Result<ContentReference, ViewError>;
    fn read(
        &self,
        reference: &ContentReference,
        scope: &ContentScope,
        now_ms: u64,
        offset: usize,
        max_bytes: usize,
    ) -> Result<ContentPage, ViewError>;
}
#[derive(Clone, Debug)]
pub struct ContentStoreConfig {
    pub store_id: String,
    pub max_entries: usize,
    pub max_bytes: usize,
    pub max_content_bytes: usize,
    pub max_page_bytes: usize,
    pub max_ttl_ms: u64,
}
struct Entry {
    reference: ContentReference,
    text: String,
}
/// Explicitly constructed by the host. No global store or automatic tool grant.
pub struct MemoryContentStore {
    config: ContentStoreConfig,
    entries: Mutex<BTreeMap<String, Entry>>,
}
impl MemoryContentStore {
    pub fn new(config: ContentStoreConfig) -> Result<Self, ViewError> {
        if config.store_id.is_empty()
            || config.store_id.len() > 64
            || !config
                .store_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            || config.max_entries == 0
            || config.max_bytes == 0
            || config.max_content_bytes == 0
            || config.max_page_bytes == 0
            || config.max_ttl_ms == 0
        {
            return Err(ViewError::Invalid);
        }
        Ok(Self {
            config,
            entries: Mutex::new(BTreeMap::new()),
        })
    }
    /// Expired data is removed on put and by this explicit host maintenance call.
    pub fn purge_expired(&self, now_ms: u64) -> Result<(), ViewError> {
        self.entries
            .lock()
            .map_err(|_| ViewError::Poisoned)?
            .retain(|_, e| e.reference.scope.expires_at_ms > now_ms);
        Ok(())
    }
}
fn valid_scope(scope: &ContentScope, now: u64) -> bool {
    !scope.session_id.as_str().trim().is_empty()
        && !scope.task_id.as_str().trim().is_empty()
        && scope.session_id.as_str().len() <= 1024
        && scope.task_id.as_str().len() <= 1024
        && scope.expires_at_ms > now
}
impl ContentStore for MemoryContentStore {
    fn put(
        &self,
        scope: &ContentScope,
        source: &EventId,
        text: &str,
        now_ms: u64,
    ) -> Result<ContentReference, ViewError> {
        if !valid_scope(scope, now_ms)
            || scope.expires_at_ms - now_ms > self.config.max_ttl_ms
            || source.as_str().trim().is_empty()
            || source.as_str().len() > 1024
        {
            return Err(ViewError::Invalid);
        }
        if text.len() > self.config.max_content_bytes {
            return Err(ViewError::Limit);
        }
        let reference = ContentReference {
            store_id: self.config.store_id.clone(),
            source_event: source.clone(),
            scope: scope.clone(),
            total_bytes: text.len(),
        };
        let mut entries = self.entries.lock().map_err(|_| ViewError::Poisoned)?;
        entries.retain(|_, e| e.reference.scope.expires_at_ms > now_ms);
        if let Some(entry) = entries.get(source.as_str()) {
            return if entry.reference == reference && entry.text == text {
                Ok(reference)
            } else {
                Err(ViewError::Conflict)
            };
        }
        let bytes = entries
            .values()
            .try_fold(text.len(), |sum, e| sum.checked_add(e.text.len()))
            .ok_or(ViewError::Limit)?;
        if entries.len() >= self.config.max_entries || bytes > self.config.max_bytes {
            return Err(ViewError::Limit);
        }
        entries.insert(
            source.as_str().into(),
            Entry {
                reference: reference.clone(),
                text: text.into(),
            },
        );
        Ok(reference)
    }
    fn read(
        &self,
        reference: &ContentReference,
        scope: &ContentScope,
        now_ms: u64,
        offset: usize,
        max_bytes: usize,
    ) -> Result<ContentPage, ViewError> {
        if !valid_scope(scope, now_ms)
            || reference.scope != *scope
            || reference.store_id != self.config.store_id
        {
            return Err(ViewError::Unavailable);
        }
        if max_bytes == 0 || max_bytes > self.config.max_page_bytes {
            return Err(ViewError::Limit);
        }
        let entries = self.entries.lock().map_err(|_| ViewError::Poisoned)?;
        let entry = entries
            .get(reference.source_event.as_str())
            .filter(|e| e.reference == *reference)
            .ok_or(ViewError::Unavailable)?;
        if offset > entry.text.len() || !entry.text.is_char_boundary(offset) {
            return Err(ViewError::Cursor);
        }
        let end = boundary(
            &entry.text,
            offset.saturating_add(max_bytes).min(entry.text.len()),
        );
        if end == offset && offset < entry.text.len() {
            return Err(ViewError::Cursor);
        }
        Ok(ContentPage {
            text: entry.text[offset..end].into(),
            next_offset: (end < entry.text.len()).then_some(end),
            total_bytes: entry.text.len(),
        })
    }
}
fn boundary(text: &str, mut bytes: usize) -> usize {
    bytes = bytes.min(text.len());
    while !text.is_char_boundary(bytes) {
        bytes -= 1;
    }
    bytes
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResultSelection {
    Inline,
    Prefix { bytes: usize },
}
/// Strategy can choose a slice length, but cannot invent text, status or roles.
pub trait ToolResultPolicy: Send + Sync {
    fn select(&self, text: &str) -> Result<ResultSelection, ViewError>;
}
#[derive(Clone, Copy, Debug)]
pub struct BoundedResultPolicy {
    pub inline_bytes: usize,
    pub preview_bytes: usize,
}
impl Default for BoundedResultPolicy {
    fn default() -> Self {
        Self {
            inline_bytes: 4096,
            preview_bytes: 512,
        }
    }
}
impl ToolResultPolicy for BoundedResultPolicy {
    fn select(&self, text: &str) -> Result<ResultSelection, ViewError> {
        if self.inline_bytes == 0 || self.preview_bytes > self.inline_bytes {
            return Err(ViewError::Invalid);
        }
        Ok(if text.len() <= self.inline_bytes {
            ResultSelection::Inline
        } else {
            ResultSelection::Prefix {
                bytes: self.preview_bytes,
            }
        })
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ToolContentView {
    pub text: String,
    pub total_bytes: usize,
    pub truncated: bool,
    pub reference: Option<ContentReference>,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ToolOutcomeView {
    Succeeded {
        output: ToolContentView,
    },
    Failed {
        category: ToolFailureCategory,
        code: String,
        message: ToolContentView,
        retryable: bool,
    },
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ToolResultView {
    pub algorithm_version: u16,
    pub source_event: EventId,
    pub outcome: ToolOutcomeView,
}
/// Preserves status/failure facts. Referenced content remains untrusted tool data.
pub fn project_tool_result(
    outcome: &ToolRecordedOutcome,
    source: &EventId,
    scope: &ContentScope,
    now_ms: u64,
    policy: &dyn ToolResultPolicy,
    store: &dyn ContentStore,
    max_preview_bytes: usize,
) -> Result<ToolResultView, ViewError> {
    if max_preview_bytes == 0 || !valid_scope(scope, now_ms) || source.as_str().trim().is_empty() {
        return Err(ViewError::Invalid);
    }
    let text = match outcome {
        ToolRecordedOutcome::Succeeded { output } => output,
        ToolRecordedOutcome::Failed { message, .. } => message,
    };
    let bytes = match policy.select(text)? {
        ResultSelection::Inline => text.len(),
        ResultSelection::Prefix { bytes } => bytes.min(text.len()),
    };
    if bytes > max_preview_bytes {
        return Err(ViewError::Limit);
    }
    let end = boundary(text, bytes);
    let truncated = end < text.len();
    let reference = if truncated {
        {
            let reference = store.put(scope, source, text, now_ms)?;
            if reference.scope != *scope
                || reference.source_event != *source
                || reference.total_bytes != text.len()
                || reference.store_id.is_empty()
            {
                return Err(ViewError::Conflict);
            }
            Some(reference)
        }
    } else {
        None
    };
    let content = ToolContentView {
        text: text[..end].into(),
        total_bytes: text.len(),
        truncated,
        reference,
    };
    let outcome = match outcome {
        ToolRecordedOutcome::Succeeded { .. } => ToolOutcomeView::Succeeded { output: content },
        ToolRecordedOutcome::Failed {
            category,
            code,
            retryable,
            ..
        } => ToolOutcomeView::Failed {
            category: *category,
            code: code.clone(),
            message: content,
            retryable: *retryable,
        },
    };
    Ok(ToolResultView {
        algorithm_version: 1,
        source_event: source.clone(),
        outcome,
    })
}
