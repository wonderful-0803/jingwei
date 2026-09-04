//! Internal protocol verification, excluded from public Git and Cargo delivery.
#![doc = include_str!("../../../docs/guide/src/model-protocol.md")]
#![doc = include_str!("../../../docs/guide/src/action-step.md")]
#![doc = include_str!("../../../docs/guide/src/task-budget.md")]
#![doc = include_str!("../../../docs/guide/src/model-scheduling.md")]
#![doc = include_str!("../../../docs/guide/src/budget-checkpoints.md")]
#![doc = include_str!("../../../docs/guide/src/file-checkpoint-store.md")]

#[cfg(test)]
mod actions;

#[cfg(test)]
mod budget;

#[cfg(test)]
mod checkpoint;

#[cfg(test)]
mod checkpoint_file;

#[cfg(test)]
mod scheduler;

#[cfg(test)]
mod model_budget;

#[cfg(test)]
mod tool_budget;

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use jingwei::llm::*;
    use jingwei::plugin::*;
    use jingwei_core::{
        CancellationFuture, CancellationSignal, EventId, SessionEvent, SessionId, StepId, TaskId,
        TurnId,
    };
    use jingwei_llm_runtime::CanonicalLlmRuntimePlugin;
    use jingwei_openai::{OpenAiConfig, OpenAiLlm, OpenAiLlmPlugin};
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    fn call(id: &str, name: &str) -> ModelToolCall {
        ModelToolCall {
            id: ProviderToolCallId::new(id).unwrap(),
            name: name.into(),
            arguments: json!({"query": "你好"}),
        }
    }

    fn tool(name: &str) -> ModelToolDefinition {
        ModelToolDefinition {
            name: name.into(),
            description: "A test-only candidate".into(),
            parameters: json!({"type": "object", "required": ["query"]}),
        }
    }

    fn native_request(choice: ToolChoice) -> GenerationRequest {
        GenerationRequest {
            messages: vec![ModelMessage::user("hello")],
            constraint: GenerationConstraint::NativeTools {
                tools: vec![tool("lookup"), tool("calculate")],
                choice,
            },
        }
    }

    fn response(calls: Vec<ModelToolCall>, finish_reason: FinishReason) -> GenerationResponse {
        GenerationResponse {
            content: None,
            tool_calls: calls,
            incomplete_tool_calls: vec![],
            finish_reason,
            usage: TokenUsage::default(),
        }
    }

    fn result(id: &str) -> ModelMessage {
        ModelMessage::Tool {
            call_id: ProviderToolCallId::new(id).unwrap(),
            content: "untrusted tool output".into(),
        }
    }

    fn history(messages: Vec<ModelMessage>) -> GenerationRequest {
        GenerationRequest {
            messages,
            constraint: GenerationConstraint::Text,
        }
    }

    #[test]
    fn provider_ids_validate_on_construction_and_deserialization() {
        for value in ["", " ", "\n\t"] {
            assert!(ProviderToolCallId::new(value).is_err());
            assert!(serde_json::from_value::<ProviderToolCallId>(json!(value)).is_err());
        }
        let id = ProviderToolCallId::new("provider:调用-1").unwrap();
        assert_eq!(serde_json::to_value(&id).unwrap(), json!("provider:调用-1"));
        assert_eq!(
            serde_json::from_value::<ProviderToolCallId>(json!(id)).unwrap(),
            id
        );
    }

    #[test]
    fn role_payloads_reject_cross_role_fields() {
        for wire in [
            json!({"role":"system","content":"x","tool_calls":[]}),
            json!({"role":"user","content":"x","call_id":"id"}),
            json!({"role":"tool","content":"x"}),
            json!({"role":"unexpected","content":"x"}),
        ] {
            assert!(serde_json::from_value::<ModelMessage>(wire).is_err());
        }
    }

    #[test]
    fn unified_text_messages_and_request_roundtrip() {
        let request = GenerationRequest::text(vec![
            ModelMessage::system("系统"),
            ModelMessage::user("问"),
            ModelMessage::assistant("答"),
        ]);
        request.validate_shape().unwrap();
        assert_eq!(
            serde_json::from_value::<GenerationRequest>(json!(request)).unwrap(),
            request
        );
    }

    #[test]
    fn tool_association_survives_wire_roundtrip() {
        let message = result("provider-id");
        assert_eq!(
            json!(message),
            json!({"role":"tool","call_id":"provider-id","content":"untrusted tool output"})
        );
        assert_eq!(
            serde_json::from_value::<ModelMessage>(json!(message)).unwrap(),
            message
        );
    }

    #[test]
    fn multiple_calls_accept_reordered_results_without_dropping_any_call() {
        let assistant = response(
            vec![call("a", "lookup"), call("b", "calculate")],
            FinishReason::ToolCalls,
        )
        .into_message()
        .unwrap();
        let request = history(vec![
            ModelMessage::user("x"),
            assistant,
            result("b"),
            result("a"),
        ]);
        request.validate_shape().unwrap();
        assert_eq!(
            serde_json::from_value::<GenerationRequest>(serde_json::to_value(&request).unwrap())
                .unwrap(),
            request
        );
    }

    #[test]
    fn provider_ids_can_repeat_only_in_separate_closed_groups() {
        let assistant = response(vec![call("same", "lookup")], FinishReason::ToolCalls)
            .into_message()
            .unwrap();
        history(vec![
            assistant.clone(),
            result("same"),
            assistant,
            result("same"),
        ])
        .validate_shape()
        .unwrap();
        let duplicate = response(
            vec![call("same", "lookup"), call("same", "calculate")],
            FinishReason::ToolCalls,
        );
        assert_eq!(
            duplicate.into_message(),
            Err(ModelProtocolError::DuplicateToolCallId)
        );
    }

    #[test]
    fn orphan_duplicate_and_missing_results_fail() {
        assert_eq!(
            history(vec![result("id")]).validate_shape(),
            Err(ModelProtocolError::UnmatchedToolResult { index: 0 })
        );
        let assistant = response(vec![call("id", "lookup")], FinishReason::ToolCalls)
            .into_message()
            .unwrap();
        assert_eq!(
            history(vec![assistant.clone()]).validate_shape(),
            Err(ModelProtocolError::MissingToolResults)
        );
        assert_eq!(
            history(vec![assistant, result("id"), result("id")]).validate_shape(),
            Err(ModelProtocolError::UnmatchedToolResult { index: 2 })
        );
    }

    #[test]
    fn pending_groups_cannot_be_interrupted_by_instructions_or_user_text() {
        let assistant = response(vec![call("id", "lookup")], FinishReason::ToolCalls)
            .into_message()
            .unwrap();
        for interruption in [
            ModelMessage::user("x"),
            ModelMessage::system("x"),
            ModelMessage::assistant("x"),
        ] {
            assert_eq!(
                history(vec![assistant.clone(), interruption, result("id")]).validate_shape(),
                Err(ModelProtocolError::InterruptedToolGroup { index: 1 })
            );
        }
    }

    #[test]
    fn missing_assistant_content_and_unparsed_arguments_fail() {
        assert_eq!(
            history(vec![ModelMessage::Assistant {
                content: None,
                tool_calls: vec![]
            }])
            .validate_shape(),
            Err(ModelProtocolError::MissingAssistantContent)
        );
        for arguments in [json!(null), json!("{\"a\":1}"), json!([]), json!(1)] {
            let mut proposal = call("id", "lookup");
            proposal.arguments = arguments;
            assert_eq!(
                response(vec![proposal], FinishReason::ToolCalls).into_message(),
                Err(ModelProtocolError::ArgumentsNotObject)
            );
        }
    }

    #[test]
    fn empty_messages_and_invalid_candidate_sets_fail() {
        assert_eq!(
            history(vec![]).validate_shape(),
            Err(ModelProtocolError::EmptyMessages)
        );
        for (tools, expected) in [
            (vec![], ModelProtocolError::EmptyToolSet),
            (
                vec![tool("a"), tool("a")],
                ModelProtocolError::DuplicateToolName,
            ),
            (vec![tool(" ")], ModelProtocolError::EmptyName),
        ] {
            let mut request = native_request(ToolChoice::Auto);
            request.constraint = GenerationConstraint::NativeTools {
                tools,
                choice: ToolChoice::Auto,
            };
            assert_eq!(request.validate_shape(), Err(expected));
        }
    }

    #[test]
    fn schema_root_shape_is_checked_without_network_or_semantic_claims() {
        for (schema, valid) in [
            (json!({"$ref":"https://invalid.test/private.json"}), true),
            (json!(false), true),
            (json!([]), false),
            (json!(null), false),
        ] {
            let request = GenerationRequest {
                messages: vec![ModelMessage::user("x")],
                constraint: GenerationConstraint::JsonSchema {
                    name: "answer".into(),
                    schema,
                },
            };
            assert_eq!(request.validate_shape().is_ok(), valid);
        }
    }

    #[test]
    fn completion_and_streaming_capabilities_are_independent_per_path() {
        let request = native_request(ToolChoice::Auto);
        let mut capabilities = ModelCapabilities::text_only();
        assert_eq!(
            request.preflight(&capabilities, ModelCallMode::Complete),
            Err(ModelProtocolError::UnsupportedCapability)
        );
        capabilities.native_tools.complete = CapabilitySupport::Supported;
        request
            .preflight(&capabilities, ModelCallMode::Complete)
            .unwrap();
        assert_eq!(
            request.preflight(&capabilities, ModelCallMode::Stream),
            Err(ModelProtocolError::UnsupportedCapability)
        );
        capabilities.native_tools.stream = CapabilitySupport::Unknown;
        assert_eq!(
            request.preflight(&capabilities, ModelCallMode::Stream),
            Err(ModelProtocolError::UnknownCapability)
        );
    }

    #[test]
    fn missing_capabilities_are_unknown_not_supported() {
        let capabilities = serde_json::from_value::<ModelCapabilities>(json!({})).unwrap();
        assert_eq!(capabilities, ModelCapabilities::default());
        assert_eq!(
            history(vec![ModelMessage::user("x")])
                .preflight(&capabilities, ModelCallMode::Complete),
            Err(ModelProtocolError::UnknownCapability)
        );
    }

    #[test]
    fn named_choice_must_be_visible_at_request_and_response_boundaries() {
        assert_eq!(
            native_request(ToolChoice::Named {
                name: "hidden".into()
            })
            .validate_shape(),
            Err(ModelProtocolError::ToolNotVisible)
        );
        let output = response(vec![call("id", "calculate")], FinishReason::ToolCalls);
        assert_eq!(
            output.validate_shape_for(&native_request(ToolChoice::Named {
                name: "lookup".into()
            })),
            Err(ModelProtocolError::WrongToolChoice)
        );
        assert_eq!(
            response(vec![call("id", "hidden")], FinishReason::ToolCalls)
                .validate_shape_for(&native_request(ToolChoice::Auto)),
            Err(ModelProtocolError::ToolNotVisible)
        );
    }

    #[test]
    fn required_choice_does_not_accept_a_text_only_answer() {
        let output = GenerationResponse {
            content: Some("answer".into()),
            ..response(vec![], FinishReason::Stop)
        };
        output
            .validate_shape_for(&native_request(ToolChoice::Auto))
            .unwrap();
        assert_eq!(
            output.validate_shape_for(&native_request(ToolChoice::Required)),
            Err(ModelProtocolError::RequiredToolCallMissing)
        );
    }

    #[test]
    fn incomplete_responses_never_become_actionable_messages() {
        for reason in [
            FinishReason::Length,
            FinishReason::Unknown,
            FinishReason::ContentFiltered,
            FinishReason::Other {
                code: "server-extension".into(),
            },
        ] {
            let output = response(vec![call("id", "lookup")], reason);
            assert_eq!(
                output.validate_shape_for(&native_request(ToolChoice::Auto)),
                Err(ModelProtocolError::IncompleteResponse)
            );
            assert_eq!(
                output.into_message(),
                Err(ModelProtocolError::IncompleteResponse)
            );
        }
    }

    #[test]
    fn finish_reason_must_match_call_presence() {
        assert_eq!(
            response(vec![call("id", "lookup")], FinishReason::Stop).into_message(),
            Err(ModelProtocolError::InconsistentFinishReason)
        );
        assert_eq!(
            response(vec![], FinishReason::ToolCalls).into_message(),
            Err(ModelProtocolError::InconsistentFinishReason)
        );
    }

    #[test]
    fn text_and_json_paths_reject_native_calls_and_json_requires_valid_syntax() {
        let output = response(vec![call("id", "lookup")], FinishReason::ToolCalls);
        let mut request = history(vec![ModelMessage::user("x")]);
        assert_eq!(
            output.validate_shape_for(&request),
            Err(ModelProtocolError::UnexpectedToolCalls)
        );
        request.constraint = GenerationConstraint::JsonSchema {
            name: "answer".into(),
            schema: json!({"type":"object"}),
        };
        assert_eq!(
            output.validate_shape_for(&request),
            Err(ModelProtocolError::UnexpectedToolCalls)
        );
        for (content, valid) in [
            ("{\"答案\":42}", true),
            ("{", false),
            ("```json\n{}\n```", false),
        ] {
            let output = GenerationResponse {
                content: Some(content.into()),
                ..response(vec![], FinishReason::Stop)
            };
            assert_eq!(output.validate_shape_for(&request).is_ok(), valid);
        }
    }

    #[test]
    fn usage_and_finish_metadata_missing_is_unknown_not_zero_or_stop() {
        let output: GenerationResponse =
            serde_json::from_value(json!({"content":"hello"})).unwrap();
        assert_eq!(output.usage, TokenUsage::default());
        assert_eq!(output.finish_reason, FinishReason::Unknown);
        assert!(output.into_message().is_err());
        let partial: TokenUsage = serde_json::from_value(json!({"input_tokens":0})).unwrap();
        assert_eq!(partial.input_tokens, Some(0));
        assert_eq!(partial.output_tokens, None);
        assert_eq!(partial.total_tokens, None);
    }

    #[test]
    fn past_tools_need_not_be_visible_to_the_next_inference() {
        let assistant = response(
            vec![call("id", "no-longer-visible")],
            FinishReason::ToolCalls,
        )
        .into_message()
        .unwrap();
        let mut request = native_request(ToolChoice::Auto);
        request.messages = vec![assistant, result("id"), ModelMessage::user("continue")];
        request.validate_shape().unwrap();
    }

    #[test]
    fn task_and_step_ids_have_separate_identity_and_serde_roundtrips() {
        let task = TaskId::new();
        let step = StepId::new();
        assert!(task.as_str().starts_with("task_"));
        assert!(step.as_str().starts_with("step_"));
        assert_ne!(task.as_str(), TaskId::new().as_str());
        assert_eq!(serde_json::from_value::<TaskId>(json!(task)).unwrap(), task);
        assert_eq!(serde_json::from_value::<StepId>(json!(step)).unwrap(), step);
    }

    #[test]
    fn existing_adapter_declares_only_implemented_paths_without_probing() {
        let adapter =
            OpenAiLlm::new(OpenAiConfig::new("http://127.0.0.1:1/v1", "not-loaded").unwrap())
                .unwrap();
        assert_eq!(adapter.capabilities(), adapter_capabilities());
        assert_eq!(
            native_request(ToolChoice::Auto)
                .preflight(&adapter.capabilities(), ModelCallMode::Complete),
            Err(ModelProtocolError::UnsupportedCapability)
        );
    }

    fn adapter_capabilities() -> ModelCapabilities {
        ModelCapabilities {
            token_usage: CapabilitySupport::Supported,
            ..ModelCapabilities::text_only()
        }
    }

    struct CountingLlm(Arc<AtomicUsize>);

    impl Llm for CountingLlm {
        // Missing capability metadata must fail closed at the controlled gateway.
        fn generate<'a>(
            &'a self,
            _: &'a GenerationRequest,
            _: GenerationOptions,
            _: CancellationToken,
        ) -> LlmFuture<'a, Result<GenerationResponse, LlmError>> {
            Box::pin(async move {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(GenerationResponse::text("answer", FinishReason::Stop))
            })
        }
        fn generate_stream(
            &self,
            _: GenerationRequest,
            _: GenerationOptions,
            _: CancellationToken,
        ) -> GenerationStream {
            Box::pin(futures::stream::empty())
        }
    }

    struct TestProviderPlugin(Arc<AtomicUsize>);
    impl ServiceFactory<dyn Llm> for TestProviderPlugin {
        fn construct<'a>(
            &'a self,
            _: FactoryContext<'a>,
        ) -> LifecycleFuture<'a, Result<ManagedService<dyn Llm>, RuntimeError>> {
            Box::pin(async move {
                let provider: Arc<dyn Llm> = Arc::new(CountingLlm(Arc::clone(&self.0)));
                Ok(ManagedService::ready(provider))
            })
        }
    }
    impl Plugin for TestProviderPlugin {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor::provider("private-test-provider", LLM_PROVIDER, "counting")
        }
        fn mount(&self, ctx: &mut MountContext<'_>) -> Result<(), MountError> {
            ctx.provide_llm_factory(Arc::new(Self(Arc::clone(&self.0))))
        }
    }

    struct NeverCancelled;
    impl CancellationSignal for NeverCancelled {
        fn is_cancelled(&self) -> bool {
            false
        }
        fn cancelled(&self) -> CancellationFuture<'_> {
            Box::pin(std::future::pending())
        }
    }

    #[derive(Default)]
    struct Recorder(AtomicUsize);
    impl ModelEventRecorder for Recorder {
        fn append(
            &self,
            record: ModelRecord,
        ) -> ModelFuture<'_, Result<Arc<SessionEvent>, jingwei::session::SessionRuntimeError>>
        {
            Box::pin(async move {
                Ok(Arc::new(SessionEvent {
                    event_id: EventId::new(),
                    session_id: SessionId::from("private-session"),
                    turn_id: TurnId::from("private-turn"),
                    generation_id: None,
                    message_id: None,
                    seq: self.0.fetch_add(1, Ordering::SeqCst) as u64,
                    kind: record.into_session_event_kind(),
                }))
            })
        }
    }

    #[tokio::test]
    async fn canonical_gateway_rejects_unknown_capabilities_before_inference() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut registrar = Registrar::default();
        registrar.add(TestProviderPlugin(Arc::clone(&calls)));
        registrar.add(CanonicalLlmRuntimePlugin::new());
        registrar.select(LLM_RUNTIME, "canonical");
        let registry = registrar.finish().await.unwrap();
        let recorder = Arc::new(Recorder::default());
        let turn = registry
            .llm_runtime()
            .unwrap()
            .bind_turn(ModelTurnBinding::new(
                Arc::new(NeverCancelled),
                recorder.clone(),
            ))
            .unwrap();
        assert_eq!(turn.gateway().capabilities(), ModelCapabilities::default());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(recorder.0.load(Ordering::SeqCst), 0);
        let error = turn
            .gateway()
            .generate(
                &GenerationRequest::text(vec![ModelMessage::user("hello")]),
                GenerationOptions::default(),
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            ModelGatewayError::Model(LlmError::Protocol(ModelProtocolError::UnknownCapability))
        ));
        turn.finish(ModelFinishMode::Graceful).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(recorder.0.load(Ordering::SeqCst), 2);
        registry.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn canonical_gateway_forwards_nondefault_capabilities_without_records() {
        let mut registrar = Registrar::default();
        registrar.add(OpenAiLlmPlugin::new(
            OpenAiConfig::new("http://127.0.0.1:1/v1", "not-loaded").unwrap(),
        ));
        registrar.add(CanonicalLlmRuntimePlugin::new());
        registrar.select(LLM_RUNTIME, "canonical");
        let registry = registrar.finish().await.unwrap();
        let recorder = Arc::new(Recorder::default());
        let turn = registry
            .llm_runtime()
            .unwrap()
            .bind_turn(ModelTurnBinding::new(
                Arc::new(NeverCancelled),
                recorder.clone(),
            ))
            .unwrap();
        assert_eq!(turn.gateway().capabilities(), adapter_capabilities());
        assert_eq!(recorder.0.load(Ordering::SeqCst), 0);
        turn.finish(ModelFinishMode::Graceful).await.unwrap();
        registry.shutdown().await.unwrap();
    }
}

#[cfg(test)]
mod unified;

#[cfg(test)]
mod agent_budget;
