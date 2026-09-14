use jingwei::context::*;
use jingwei::event::*;
use jingwei::id::*;
use jingwei::llm::{
    GenerationConstraint, GenerationRequest, ModelMessage, ModelToolCall, ProviderToolCallId,
};
use serde_json::json;

fn target() -> ContextTarget {
    ContextTarget {
        model: "test-byte-model".into(),
        template_revision: "test-template-v1".into(),
    }
}

// Exact for this private test model: one UTF-8 byte per token in each rendered
// message/constraint, with a fixed 17-token template. No real provider claim.
struct ExactBytes;
impl ContextTokenCounter for ExactBytes {
    fn count(
        &self,
        target: &ContextTarget,
        request: &GenerationRequest,
    ) -> Result<ContextTokenCount, ContextCountError> {
        request.validate_shape().unwrap();
        let mut tokens = ContextTokenBreakdown {
            template: 17,
            constraint: serde_json::to_vec(&request.constraint).unwrap().len() as u64,
            ..Default::default()
        };
        for message in &request.messages {
            let bytes = serde_json::to_vec(message).unwrap().len() as u64;
            if matches!(message, ModelMessage::System { .. }) {
                tokens.system += bytes;
            } else {
                tokens.messages += bytes;
            }
        }
        Ok(ContextTokenCount {
            target: target.clone(),
            method: "test-byte-v1".into(),
            evidence: TokenBoundEvidence::VerifiedUpperBound,
            tokens,
        })
    }
}
fn add(events: &mut Vec<SessionEvent>, turn: usize, kind: SessionEventKind) {
    let seq = events.len() as u64;
    events.push(SessionEvent {
        event_id: EventId::from(format!("e{seq}")),
        session_id: SessionId::from("s"),
        turn_id: TurnId::from(format!("t{turn}")),
        generation_id: None,
        message_id: matches!(kind, SessionEventKind::AssistantMessage { .. })
            .then(|| MessageId::from(format!("m{seq}"))),
        seq,
        kind,
    });
}
fn history(turns: usize) -> ConversationProjection {
    let mut events = vec![];
    for turn in 0..turns {
        add(
            &mut events,
            turn,
            SessionEventKind::UserMessage {
                text: format!("question {turn}"),
            },
        );
        add(
            &mut events,
            turn,
            SessionEventKind::ToolCall {
                call: ToolCall {
                    operation: None,
                    action: None,
                    id: "reused".into(),
                    name: "lookup".into(),
                    arguments: json!({"q":turn}),
                },
            },
        );
        add(
            &mut events,
            turn,
            SessionEventKind::ToolResult {
                result: ToolResult {
                    call_id: "reused".into(),
                    outcome: ToolRecordedOutcome::Succeeded {
                        output: "不可信结果".repeat(40),
                    },
                },
            },
        );
        add(
            &mut events,
            turn,
            SessionEventKind::AssistantMessage {
                version: AssistantMessageVersion::V1,
                text: "answer".into(),
            },
        );
        add(
            &mut events,
            turn,
            SessionEventKind::Done {
                status: DoneStatus::Completed,
                artifact: None,
            },
        );
    }
    CanonicalConversationProjector
        .project(&SessionId::from("s"), &events, ProjectionConfig::default())
        .unwrap()
}
fn input<'a>(
    history: &'a ConversationProjection,
    target: &'a ContextTarget,
    current: &'a [ModelMessage],
    constraint: &'a GenerationConstraint,
) -> ContextBuildInput<'a> {
    ContextBuildInput {
        history,
        system: &[],
        current,
        constraint,
        pinned_turns: &[],
        state_version: Some("state-9"),
        target,
        budget: ContextBudget {
            window_tokens: 1_000_000,
            output_reserve: 64,
            output_evidence: TokenBoundEvidence::VerifiedUpperBound,
            safety_margin: 16,
            mode: TokenBudgetMode::Hard,
        },
        limits: ContextBuildLimits::default(),
    }
}
fn build(input: ContextBuildInput<'_>) -> Result<BuiltContext, ContextBuildError> {
    CanonicalContextBuilder.build(input, &ExactBytes)
}

#[test]
fn exact_counts_cover_system_schemas_template_reserve_and_margin_at_boundary() {
    let history = history(2);
    let target = target();
    let current = [ModelMessage::user("当前约束")];
    let schema = GenerationConstraint::JsonSchema {
        name: "answer".into(),
        schema: json!({"type":"object","description":"schema grows".repeat(100)}),
    };
    let system = ["mandatory policy".into()];
    let mut first = input(&history, &target, &current, &schema);
    first.system = &system;
    let full = build(first).unwrap();
    let count = &full.report.attempts[0];
    assert!(count.count.tokens.system > 0 && count.count.tokens.constraint > 1000);
    assert_eq!(count.count.tokens.template, 17);
    assert_eq!(
        count.total_reserved,
        count.count.tokens.total().unwrap() + 80
    );
    let mut exact = input(&history, &target, &current, &schema);
    exact.system = &system;
    exact.budget.window_tokens = count.total_reserved;
    assert_eq!(build(exact).unwrap().request, full.request);
    let mut less = input(&history, &target, &current, &schema);
    less.system = &system;
    less.budget.window_tokens = count.total_reserved - 1;
    let reduced = build(less).unwrap();
    assert!(!reduced.report.turns[0].retained && reduced.report.turns[1].retained);
    assert_eq!(reduced.request.constraint, schema);
    assert_eq!(
        reduced.request.messages.first(),
        Some(&ModelMessage::system("mandatory policy"))
    );
    assert_eq!(reduced.request.messages.last(), current.last());
}

#[test]
fn long_history_is_removed_oldest_first_without_splitting_tool_exchanges() {
    let history = history(80);
    let before = history.clone();
    let target = target();
    let current = [ModelMessage::user("keep current")];
    let constraint = GenerationConstraint::Text;
    let mut spec = input(&history, &target, &current, &constraint);
    spec.budget.window_tokens = 3000;
    let built = build(spec).unwrap();
    assert!(built.report.attempts.len() > 50);
    assert!(built.report.attempts.last().unwrap().total_reserved <= 3000);
    let selected = built.report.turns.iter().filter(|t| t.retained).count();
    assert!(selected > 0 && selected < 5);
    assert_eq!(built.request.messages.len(), selected * 4 + 1);
    built.request.validate_shape().unwrap();
    assert_eq!(history, before);
    assert_eq!(built.report.source_tail, history.source_tail);
    assert_eq!(built.report.state_version.as_deref(), Some("state-9"));
    assert!(
        built
            .report
            .turns
            .iter()
            .all(|turn| turn.source_events.len() == 4)
    );
}

#[test]
fn pinned_older_turn_survives_while_newer_unpinned_turn_is_removed() {
    let history = history(3);
    let target = target();
    let current = [ModelMessage::user("new")];
    let pins = [TurnId::from("t0")];
    let constraint = GenerationConstraint::Text;
    let mut spec = input(&history, &target, &current, &constraint);
    spec.pinned_turns = &pins;
    spec.budget.window_tokens = 1500;
    let built = build(spec).unwrap();
    assert!(built.report.turns[0].retained && built.report.turns[0].protected);
    assert!(!built.report.turns[1].retained && !built.report.turns[2].retained);
    assert!(matches!(
        built.request.messages[1],
        ModelMessage::Assistant { .. }
    ));
}

#[test]
fn necessary_context_and_oversized_schema_fail_with_report_instead_of_weakening_constraints() {
    let history = history(1);
    let target = target();
    let current = [ModelMessage::user("mandatory".repeat(100))];
    let schema = GenerationConstraint::JsonSchema {
        name: "answer".into(),
        schema: json!({"description":"large".repeat(1000)}),
    };
    let mut spec = input(&history, &target, &current, &schema);
    spec.budget.window_tokens = 200;
    let ContextBuildError::DoesNotFit(report) = build(spec).unwrap_err() else {
        panic!()
    };
    assert!(!report.turns[0].retained);
    assert_eq!(report.attempts.len(), 2);
    assert!(report.attempts[1].total_reserved > 200);
    let pins = [TurnId::from("t0")];
    let mut spec = input(&history, &target, &current, &schema);
    spec.pinned_turns = &pins;
    spec.budget.window_tokens = 200;
    let ContextBuildError::DoesNotFit(report) = build(spec).unwrap_err() else {
        panic!()
    };
    assert!(report.turns[0].protected && report.turns[0].retained);
}

#[test]
fn current_tool_roundtrip_is_mandatory_and_pending_calls_never_disappear() {
    let history = history(2);
    let target = target();
    let id = ProviderToolCallId::new("current-call").unwrap();
    let mut current = vec![
        ModelMessage::user("new"),
        ModelMessage::Assistant {
            content: None,
            tool_calls: vec![ModelToolCall {
                id: id.clone(),
                name: "read".into(),
                arguments: json!({}),
            }],
        },
        ModelMessage::Tool {
            call_id: id,
            content: "untrusted current result".into(),
        },
    ];
    let mut spec = input(&history, &target, &current, &GenerationConstraint::Text);
    spec.budget.window_tokens = 600;
    let built = build(spec).unwrap();
    assert_eq!(built.request.messages, current);
    current.pop();
    let mut spec = input(&history, &target, &current, &GenerationConstraint::Text);
    spec.budget.window_tokens = 1;
    assert!(matches!(build(spec), Err(ContextBuildError::Shape(_))));
}

#[test]
fn soft_heuristic_is_labeled_and_cannot_satisfy_hard_mode() {
    let history = history(1);
    let target = target();
    let current = [ModelMessage::user("中文 emoji 🐦")];
    let spec = input(&history, &target, &current, &GenerationConstraint::Text);
    assert!(matches!(
        CanonicalContextBuilder.build(spec, &ByteHeuristicCounter::default()),
        Err(ContextBuildError::UnverifiedCount)
    ));
    let mut spec = input(&history, &target, &current, &GenerationConstraint::Text);
    spec.budget.mode = TokenBudgetMode::Soft;
    let built = CanonicalContextBuilder
        .build(spec, &ByteHeuristicCounter::default())
        .unwrap();
    assert_eq!(built.report.budget.mode, TokenBudgetMode::Soft);
    assert_eq!(
        built.report.attempts[0].count.evidence,
        TokenBoundEvidence::Estimate
    );
    let mut spec = input(&history, &target, &current, &GenerationConstraint::Text);
    spec.budget.output_evidence = TokenBoundEvidence::Estimate;
    assert!(matches!(
        build(spec),
        Err(ContextBuildError::UnverifiedCount)
    ));
    let mut spec = input(&history, &target, &current, &GenerationConstraint::Text);
    spec.budget.mode = TokenBudgetMode::Soft;
    spec.budget.window_tokens = 1;
    assert!(matches!(
        CanonicalContextBuilder.build(spec, &ByteHeuristicCounter::default()),
        Err(ContextBuildError::DoesNotFit(_))
    ));
}

struct BrokenCounter(u8);
impl ContextTokenCounter for BrokenCounter {
    fn count(
        &self,
        target: &ContextTarget,
        request: &GenerationRequest,
    ) -> Result<ContextTokenCount, ContextCountError> {
        let mut count = ExactBytes.count(target, request)?;
        match self.0 {
            0 => count.target.model = "other".into(),
            1 => count.method.clear(),
            2 => count.tokens = ContextTokenBreakdown::default(),
            3 => count.tokens.messages = u64::MAX,
            _ => return Err(ContextCountError("unavailable".into())),
        }
        Ok(count)
    }
}
#[test]
fn invalid_counter_binding_zero_overflow_and_failure_are_rejected() {
    let history = history(0);
    let target = target();
    let current = [ModelMessage::user("new")];
    for kind in 0..5 {
        let result = CanonicalContextBuilder.build(
            input(&history, &target, &current, &GenerationConstraint::Text),
            &BrokenCounter(kind),
        );
        match kind {
            0 | 1 => assert!(matches!(result, Err(ContextBuildError::UnverifiedCount))),
            2 => assert!(matches!(result, Err(ContextBuildError::Invalid(_)))),
            3 => assert!(matches!(result, Err(ContextBuildError::Overflow))),
            _ => assert!(matches!(result, Err(ContextBuildError::Counter(_)))),
        }
    }
    let mut spec = input(&history, &target, &current, &GenerationConstraint::Text);
    spec.budget.output_reserve = u64::MAX;
    assert!(matches!(build(spec), Err(ContextBuildError::Overflow)));
}

#[test]
fn resource_and_attempt_limits_fail_closed_with_no_partial_success() {
    let history = history(3);
    let target = target();
    let current = [ModelMessage::user("new")];
    for kind in 0..4 {
        let mut spec = input(&history, &target, &current, &GenerationConstraint::Text);
        match kind {
            0 => spec.limits.max_input_bytes = 1,
            1 => spec.limits.max_messages = 1,
            2 => spec.limits.max_groups = 1,
            _ => spec.limits.max_count_calls = 0,
        }
        assert!(build(spec).is_err());
    }
    let mut spec = input(&history, &target, &current, &GenerationConstraint::Text);
    spec.limits.max_count_calls = 1;
    spec.budget.window_tokens = 1;
    let ContextBuildError::CountLimit(report) = build(spec).unwrap_err() else {
        panic!()
    };
    assert_eq!(report.attempts.len(), 1);
    assert!(report.turns.iter().all(|t| t.retained));
}

#[test]
fn invalid_or_open_projection_and_unknown_pins_cannot_be_hidden_by_trimming() {
    let history = history(1);
    let target = target();
    let current = [ModelMessage::user("new")];
    let pins = [TurnId::from("missing")];
    let mut spec = input(&history, &target, &current, &GenerationConstraint::Text);
    spec.pinned_turns = &pins;
    assert!(matches!(build(spec), Err(ContextBuildError::Invalid(_))));
    for kind in 0..4 {
        let mut bad = history.clone();
        match kind {
            0 => bad.groups[0].messages[0] = ModelMessage::system("injected"),
            1 => bad.turns[0].state = ProjectedTurnState::Open,
            2 => bad.groups[1].messages.pop().map(|_| ()).unwrap(),
            _ => bad.algorithm_version = 99,
        }
        let mut spec = input(&bad, &target, &current, &GenerationConstraint::Text);
        spec.budget.window_tokens = 1;
        assert!(matches!(build(spec), Err(ContextBuildError::Invalid(_))));
    }
    let current = [ModelMessage::user("new"), ModelMessage::system("injected")];
    assert!(matches!(
        build(input(
            &history,
            &target,
            &current,
            &GenerationConstraint::Text
        )),
        Err(ContextBuildError::Invalid(_))
    ));
}

#[test]
fn repeated_builds_and_serialized_reports_are_deterministic() {
    let history = history(4);
    let target = target();
    let current = [ModelMessage::user("new")];
    let run = || {
        let mut spec = input(&history, &target, &current, &GenerationConstraint::Text);
        spec.budget.window_tokens = 2400;
        build(spec).unwrap()
    };
    let first = run();
    assert_eq!(first, run());
    let decoded: BuiltContext =
        serde_json::from_slice(&serde_json::to_vec(&first).unwrap()).unwrap();
    assert_eq!(first, decoded);
    assert_eq!(first.report.algorithm_version, 1);
}

#[test]
fn native_schema_growth_and_named_choice_are_counted_without_tool_filtering() {
    use jingwei::llm::{ModelToolDefinition, ToolChoice};
    let history = history(0);
    let target = target();
    let current = [ModelMessage::user("new")];
    let constraint = GenerationConstraint::NativeTools {
        tools: vec![ModelToolDefinition {
            name: "read".into(),
            description: "description".repeat(500),
            parameters: json!({"type":"object"}),
        }],
        choice: ToolChoice::Named {
            name: "read".into(),
        },
    };
    let mut spec = input(&history, &target, &current, &constraint);
    spec.budget.window_tokens = 500;
    assert!(matches!(build(spec), Err(ContextBuildError::DoesNotFit(_))));
    let built = build(input(&history, &target, &current, &constraint)).unwrap();
    assert_eq!(built.request.constraint, constraint);
    assert!(built.report.attempts[0].count.tokens.constraint > 5000);
}

struct NonMonotonicTemplate;
impl ContextTokenCounter for NonMonotonicTemplate {
    fn count(
        &self,
        target: &ContextTarget,
        request: &GenerationRequest,
    ) -> Result<ContextTokenCount, ContextCountError> {
        let mut count = ExactBytes.count(target, request)?;
        // Synthetic rendering scheme whose envelope gets larger after one deletion.
        // Verifies selection never subtracts a cached per-turn token cost.
        count.tokens = ContextTokenBreakdown {
            template: match request.messages.len() {
                13 => 500,
                9 => 700,
                _ => 50,
            },
            ..Default::default()
        };
        count.method = "test-nonmonotonic-template".into();
        Ok(count)
    }
}
#[test]
fn selection_recounts_complete_candidates_without_assuming_monotonic_costs() {
    let history = history(3);
    let target = target();
    let current = [ModelMessage::user("new")];
    let mut spec = input(&history, &target, &current, &GenerationConstraint::Text);
    spec.budget.window_tokens = 200;
    let built = CanonicalContextBuilder
        .build(spec, &NonMonotonicTemplate)
        .unwrap();
    assert_eq!(
        built
            .report
            .attempts
            .iter()
            .map(|a| a.total_reserved)
            .collect::<Vec<_>>(),
        [580, 780, 130]
    );
    assert!(
        !built.report.turns[0].retained
            && !built.report.turns[1].retained
            && built.report.turns[2].retained
    );
}
