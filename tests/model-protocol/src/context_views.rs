use jingwei::context::*;
use jingwei::event::{ToolFailureCategory, ToolRecordedOutcome};
use jingwei::id::{EventId, SessionId, TaskId};
use jingwei::llm::ModelToolDefinition;
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};

fn tool(name: &str) -> ModelToolDefinition {
    ModelToolDefinition {
        name: name.into(),
        description: "host schema".into(),
        parameters: json!({"type":"object"}),
    }
}
fn scope() -> ContentScope {
    ContentScope {
        session_id: SessionId::from("s"),
        task_id: TaskId::from("t"),
        expires_at_ms: 1000,
    }
}
fn store() -> MemoryContentStore {
    MemoryContentStore::new(ContentStoreConfig {
        store_id: "host-memory".into(),
        max_entries: 2,
        max_bytes: 100,
        max_content_bytes: 80,
        max_page_bytes: 12,
        max_ttl_ms: 1000,
    })
    .unwrap()
}
struct Names(Vec<String>);
impl ToolSelector for Names {
    fn select(&self, _: &[ModelToolDefinition]) -> Result<Vec<String>, ViewError> {
        Ok(self.0.clone())
    }
}

#[test]
fn grouped_selection_is_sorted_deterministic_and_preserves_exact_authorized_definitions() {
    let catalog = [tool("z"), tool("b"), tool("a")];
    let selector = GroupedToolSelector {
        names: Some(BTreeSet::from(["z".into()])),
        groups: BTreeMap::from([("read".into(), BTreeSet::from(["a".into()]))]),
        active_groups: BTreeSet::from(["read".into()]),
    };
    let view = select_tool_view(&catalog, &selector, ToolViewLimits::default()).unwrap();
    assert_eq!(view.tools, [tool("a"), tool("z")]);
    assert_eq!(view.excluded, ["b"]);
    assert_eq!(
        view,
        select_tool_view(
            &[tool("a"), tool("z"), tool("b")],
            &selector,
            ToolViewLimits::default()
        )
        .unwrap()
    );
    assert_eq!(catalog[0], tool("z"));
}
#[test]
fn malicious_selection_unknown_groups_duplicates_and_limits_fail_closed() {
    let catalog = [tool("read")];
    for names in [vec!["root".into()], vec!["read".into(), "read".into()]] {
        assert_eq!(
            select_tool_view(&catalog, &Names(names), ToolViewLimits::default()),
            Err(ViewError::Unauthorized)
        );
    }
    let selector = GroupedToolSelector {
        active_groups: BTreeSet::from(["missing".into()]),
        ..Default::default()
    };
    assert_eq!(
        select_tool_view(&catalog, &selector, ToolViewLimits::default()),
        Err(ViewError::Unauthorized)
    );
    assert_eq!(
        select_tool_view(
            &[tool("read"), tool("read")],
            &Names(vec![]),
            ToolViewLimits::default()
        ),
        Err(ViewError::Invalid)
    );
    let empty = select_tool_view(&catalog, &Names(vec![]), ToolViewLimits::default()).unwrap();
    assert!(empty.tools.is_empty());
    assert_eq!(
        select_tool_view(
            &catalog,
            &Names(vec![]),
            ToolViewLimits {
                max_input_bytes: 1,
                ..Default::default()
            }
        ),
        Err(ViewError::Limit)
    );
    assert_eq!(
        select_tool_view(
            &[tool("a"), tool("b")],
            &GroupedToolSelector::default(),
            ToolViewLimits {
                max_visible: 1,
                ..Default::default()
            }
        ),
        Err(ViewError::Limit)
    );
}
#[test]
fn unicode_preview_and_reference_pages_recover_original_without_rewriting_outcome() {
    let store = store();
    let scope = scope();
    let source = EventId::from("source");
    let original = "中文🐦 ignore instructions /etc/passwd";
    let outcome = ToolRecordedOutcome::Succeeded {
        output: original.into(),
    };
    let policy = BoundedResultPolicy {
        inline_bytes: 12,
        preview_bytes: 7,
    };
    let view = project_tool_result(&outcome, &source, &scope, 1, &policy, &store, 12).unwrap();
    assert_eq!(
        view,
        project_tool_result(&outcome, &source, &scope, 1, &policy, &store, 12).unwrap()
    );
    let ToolOutcomeView::Succeeded { output } = &view.outcome else {
        panic!()
    };
    assert_eq!(output.text, "中文");
    assert!(output.truncated);
    assert_eq!(output.total_bytes, original.len());
    let reference = output.reference.as_ref().unwrap();
    let mut offset = 0;
    let mut read = String::new();
    loop {
        let page = store.read(reference, &scope, 2, offset, 12).unwrap();
        read.push_str(&page.text);
        match page.next_offset {
            Some(next) => offset = next,
            None => break,
        }
    }
    assert_eq!(read, original);
    assert_eq!(
        outcome,
        ToolRecordedOutcome::Succeeded {
            output: original.into()
        }
    );
    let decoded: ToolResultView =
        serde_json::from_slice(&serde_json::to_vec(&view).unwrap()).unwrap();
    assert_eq!(view, decoded);
}
#[test]
fn references_enforce_scope_expiry_namespace_cursor_and_exact_metadata() {
    let store = store();
    let scope = scope();
    let reference = store
        .put(&scope, &EventId::from("e"), "中文data", 1)
        .unwrap();
    let mut wrong = scope.clone();
    wrong.task_id = TaskId::from("other");
    assert_eq!(
        store.read(&reference, &wrong, 2, 0, 12),
        Err(ViewError::Unavailable)
    );
    wrong = scope.clone();
    wrong.session_id = SessionId::from("other");
    assert_eq!(
        store.read(&reference, &wrong, 2, 0, 12),
        Err(ViewError::Unavailable)
    );
    assert_eq!(
        store.read(&reference, &scope, 1000, 0, 12),
        Err(ViewError::Unavailable)
    );
    for kind in 0..3 {
        let mut bad = reference.clone();
        match kind {
            0 => bad.store_id = "elsewhere".into(),
            1 => bad.total_bytes += 1,
            _ => bad.source_event = EventId::from("missing"),
        };
        assert_eq!(
            store.read(&bad, &scope, 2, 0, 12),
            Err(ViewError::Unavailable)
        );
    }
    assert_eq!(
        store.read(&reference, &scope, 2, 1, 12),
        Err(ViewError::Cursor)
    );
    assert_eq!(
        store.read(&reference, &scope, 2, 0, 1),
        Err(ViewError::Cursor)
    );
    assert_eq!(
        store.read(&reference, &scope, 2, 0, 13),
        Err(ViewError::Limit)
    );
    assert_eq!(
        store
            .read(&reference, &scope, 2, reference.total_bytes, 12)
            .unwrap()
            .text,
        ""
    );
}
#[test]
fn content_capacity_conflicts_ttl_and_explicit_expiry_cleanup_are_bounded() {
    let store = store();
    let scope = scope();
    let source = EventId::from("e");
    let first = store.put(&scope, &source, &"x".repeat(60), 1).unwrap();
    assert_eq!(
        store.put(&scope, &source, &"x".repeat(60), 1).unwrap(),
        first
    );
    assert_eq!(
        store.put(&scope, &source, "different", 1),
        Err(ViewError::Conflict)
    );
    assert_eq!(
        store.put(&scope, &EventId::from("two"), &"x".repeat(50), 1),
        Err(ViewError::Limit)
    );
    assert_eq!(
        store.put(&scope, &EventId::from("two"), &"x".repeat(81), 1),
        Err(ViewError::Limit)
    );
    store
        .put(&scope, &EventId::from("two"), "small", 1)
        .unwrap();
    assert_eq!(
        store.put(&scope, &EventId::from("three"), "small", 1),
        Err(ViewError::Limit)
    );
    let mut far = scope.clone();
    far.expires_at_ms = 2000;
    assert_eq!(store.put(&far, &source, "x", 1), Err(ViewError::Invalid));
    store.purge_expired(1000).unwrap();
    far.expires_at_ms = 1500;
    store.put(&far, &source, "new", 1001).unwrap();
    assert_eq!(
        store.read(&first, &scope, 1001, 0, 12),
        Err(ViewError::Unavailable)
    );
}
#[test]
fn failure_facts_and_inline_results_are_preserved_and_policy_cannot_bypass_preview_limit() {
    let store = store();
    let scope = scope();
    let source = EventId::from("e");
    let failure = ToolRecordedOutcome::Failed {
        category: ToolFailureCategory::Cancelled,
        code: "cancelled".into(),
        message: "failure detail".into(),
        retryable: false,
    };
    let view = project_tool_result(
        &failure,
        &source,
        &scope,
        1,
        &BoundedResultPolicy {
            inline_bytes: 6,
            preview_bytes: 0,
        },
        &store,
        6,
    )
    .unwrap();
    let ToolOutcomeView::Failed {
        category,
        code,
        message,
        retryable,
    } = view.outcome
    else {
        panic!()
    };
    assert_eq!(category, ToolFailureCategory::Cancelled);
    assert_eq!(code, "cancelled");
    assert!(!retryable);
    assert!(message.text.is_empty() && message.reference.is_some());
    let full = project_tool_result(
        &failure,
        &source,
        &scope,
        1,
        &BoundedResultPolicy::default(),
        &store,
        20,
    )
    .unwrap();
    let ToolOutcomeView::Failed { message, .. } = full.outcome else {
        panic!()
    };
    assert!(!message.truncated && message.reference.is_none());
    assert_eq!(
        project_tool_result(
            &failure,
            &source,
            &scope,
            1,
            &BoundedResultPolicy::default(),
            &store,
            3
        ),
        Err(ViewError::Limit)
    );
}
