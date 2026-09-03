//! Private end-to-end contract tests. Only in-process fakes and loopback HTTP.

#[tokio::test]
async fn backpressured_stream_still_times_out_and_drains() {
    let provider = Arc::new(Fake::with_events(completed(
        vec![text_delta("x"); 100],
        FinishReason::Stop,
    )));
    let recorder = Arc::new(Recorder::default());
    let (registry, turn) = bind(provider.clone(), recorder.clone(), CancellationToken::new()).await;
    // Keep the consumer alive but do not poll, filling the 16-entry output queue.
    let events = turn.gateway().generate_stream(
        input(),
        GenerationOptions {
            timeout: Some(Duration::from_millis(10)),
            ..Default::default()
        },
    );
    timeout(Duration::from_secs(1), async {
        loop {
            if recorder.records.lock().unwrap().len() == 2 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(
        provider
            .tokens
            .lock()
            .unwrap()
            .iter()
            .all(CancellationToken::is_cancelled)
    );
    let result: Vec<_> = events.collect().await;
    assert!(matches!(
        result.last(),
        Some(Err(ModelGatewayError::Model(LlmError::Timeout)))
    ));
    assert!(
        !result
            .iter()
            .any(|e| matches!(e, Ok(GenerationStreamEvent::Finished(_))))
    );
    close(registry, turn).await;
}

#[tokio::test]
async fn compatible_tool_stream_runs_through_canonical_gateway_end_to_end() {
    let body = chunk(
        json!({"tool_calls":[{"index":0,"id":"call","type":"function","function":{"name":"lookup","arguments":"{\"query\":\"你好\"}"}}]}),
        None,
    ) + &chunk(json!({}), Some("tool_calls"))
        + "data: [DONE]\n\n";
    let (adapter, server) = server(vec![body.into_bytes()], true).await;
    let recorder = Arc::new(Recorder::default());
    let (registry, turn) = bind(
        Arc::new(adapter),
        recorder.clone(),
        CancellationToken::new(),
    )
    .await;
    let events: Vec<_> = turn
        .gateway()
        .generate_stream(native(), GenerationOptions::default())
        .collect()
        .await;
    let Ok(GenerationStreamEvent::Finished(output)) = events.last().unwrap() else {
        panic!("{events:?}")
    };
    assert_eq!(output.tool_calls[0].arguments, json!({"query":"你好"}));
    assert_eq!(
        server.await.unwrap()["stream_options"],
        json!({"include_usage":true})
    );
    close(registry, turn).await;
    assert_eq!(recorder.records.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn compatible_adapter_cancellation_interrupts_headers_and_body_waits() {
    for streaming in [false, true] {
        for send_headers in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let (ready, ready_rx) = tokio::sync::oneshot::channel();
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = [0; 4096];
                assert!(socket.read(&mut bytes).await.unwrap() > 0);
                if send_headers {
                    socket
                        .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n")
                        .await
                        .unwrap();
                }
                ready.send(()).unwrap();
                std::future::pending::<()>().await;
            });
            let adapter =
                OpenAiLlm::new(OpenAiConfig::new(format!("http://{address}/v1"), "mock").unwrap())
                    .unwrap();
            let cancel = CancellationToken::new();
            let worker_cancel = cancel.clone();
            let request = input();
            let worker = tokio::spawn(async move {
                if streaming {
                    adapter
                        .generate_stream(request, GenerationOptions::default(), worker_cancel)
                        .next()
                        .await
                        .unwrap()
                        .unwrap_err()
                } else {
                    adapter
                        .generate(&request, GenerationOptions::default(), worker_cancel)
                        .await
                        .unwrap_err()
                }
            });
            timeout(Duration::from_secs(2), ready_rx)
                .await
                .unwrap()
                .unwrap();
            cancel.cancel();
            let result = timeout(Duration::from_secs(1), worker)
                .await
                .unwrap()
                .unwrap();
            server.abort();
            let _ = server.await;
            assert_eq!(result, LlmError::Cancelled);
        }
    }
}
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::{StreamExt, stream};
use jingwei::llm::*;
use jingwei::plugin::*;
use jingwei_core::{
    CancellationFuture, CancellationSignal, EventId, ModelCallId, SessionEvent, SessionId, TurnId,
};
use jingwei_llm_runtime::CanonicalLlmRuntimePlugin;
use jingwei_openai::{OpenAiConfig, OpenAiLlm};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

fn input() -> GenerationRequest {
    GenerationRequest::text(vec![
        ModelMessage::system("简短回答"),
        ModelMessage::user("你好"),
    ])
}
fn support() -> GenerationSupport {
    GenerationSupport {
        complete: CapabilitySupport::Supported,
        stream: CapabilitySupport::Supported,
    }
}
fn capabilities() -> ModelCapabilities {
    ModelCapabilities {
        native_tools: support(),
        json_schema: support(),
        ..ModelCapabilities::text_only()
    }
}
fn native() -> GenerationRequest {
    GenerationRequest {
        constraint: GenerationConstraint::NativeTools {
            tools: vec![ModelToolDefinition {
                name: "lookup".into(),
                description: "test candidate".into(),
                parameters: json!({"type":"object","properties":{"query":{"type":"string"}},"required":["query"],"additionalProperties":false}),
            }],
            choice: ToolChoice::Required,
        },
        ..input()
    }
}
fn schema() -> GenerationRequest {
    GenerationRequest {
        constraint: GenerationConstraint::JsonSchema {
            name: "answer".into(),
            schema: json!({"type":"object","properties":{"answer":{"type":"integer"}},"required":["answer"],"additionalProperties":false}),
        },
        ..input()
    }
}
fn text_delta(text: &str) -> GenerationDelta {
    GenerationDelta::Text { text: text.into() }
}
fn tool_delta(
    index: u32,
    id: Option<&str>,
    name: Option<&str>,
    arguments: &str,
) -> GenerationDelta {
    GenerationDelta::ToolCall {
        index,
        id: id.map(Into::into),
        name: name.map(Into::into),
        arguments: arguments.into(),
    }
}
fn completed(
    deltas: Vec<GenerationDelta>,
    reason: FinishReason,
) -> Vec<Result<GenerationStreamEvent, LlmError>> {
    let mut accumulator = GenerationAccumulator::new(GenerationLimits::default());
    for delta in &deltas {
        accumulator.push(delta).unwrap();
    }
    let response = accumulator.response(reason, TokenUsage::default()).unwrap();
    let mut events: Vec<_> = deltas
        .into_iter()
        .map(|d| Ok(GenerationStreamEvent::Delta(d)))
        .collect();
    events.push(Ok(GenerationStreamEvent::Finished(response)));
    events
}

#[test]
fn accumulator_reassembles_parallel_calls_and_preserves_truncated_arguments() {
    let mut a = GenerationAccumulator::new(GenerationLimits::default());
    a.push(&tool_delta(1, Some("b"), Some("lookup"), "{\"query\":"))
        .unwrap();
    a.push(&tool_delta(
        0,
        Some("a"),
        Some("lookup"),
        "{\"query\":\"甲\"}",
    ))
    .unwrap();
    a.push(&tool_delta(1, None, None, "\"乙\"}")).unwrap();
    let output = a
        .response(FinishReason::ToolCalls, TokenUsage::default())
        .unwrap();
    assert_eq!(output.tool_calls[0].id.as_str(), "a");
    assert_eq!(output.tool_calls[1].arguments, json!({"query":"乙"}));
    a.verify_terminal(&output).unwrap();
    output.validate_shape_for(&native()).unwrap();
    let partial = a
        .response(FinishReason::Length, TokenUsage::default())
        .unwrap();
    assert!(partial.tool_calls.is_empty());
    assert_eq!(partial.incomplete_tool_calls.len(), 2);
    assert!(partial.into_message().is_err());
}

#[test]
fn accumulator_bounds_bytes_and_call_count_before_mutation() {
    let mut a = GenerationAccumulator::new(GenerationLimits {
        max_output_bytes: 3,
        max_tool_calls: 0,
    });
    a.push(&text_delta("你")).unwrap();
    assert_eq!(
        a.push(&text_delta("好")),
        Err(ModelProtocolError::OutputLimitExceeded)
    );
    assert_eq!(a.partial().content.as_deref(), Some("你"));
    assert_eq!(
        a.push(&tool_delta(0, None, None, "")),
        Err(ModelProtocolError::OutputLimitExceeded)
    );
    assert!(a.partial().tool_calls.is_empty());
}
#[test]
fn malformed_arguments_and_forged_stream_terminals_fail() {
    let mut a = GenerationAccumulator::new(GenerationLimits::default());
    a.push(&tool_delta(0, Some("id"), Some("lookup"), "{"))
        .unwrap();
    assert!(matches!(
        a.response(FinishReason::ToolCalls, TokenUsage::default()),
        Err(ModelProtocolError::InvalidJsonOutput)
    ));
    assert!(
        a.response(FinishReason::Length, TokenUsage::default())
            .unwrap()
            .into_message()
            .is_err()
    );
    let mut a = GenerationAccumulator::new(GenerationLimits::default());
    a.push(&text_delta("real")).unwrap();
    assert_eq!(
        a.verify_terminal(&GenerationResponse::text("forged", FinishReason::Stop)),
        Err(ModelProtocolError::StreamTerminalMismatch)
    );
}
#[test]
fn canonical_request_has_exact_versioned_json_and_nanosecond_timeout() {
    let request = ModelRequest {
        version: ModelRecordVersion::V1,
        call_id: ModelCallId::from("model-fixed"),
        mode: ModelCallMode::Complete,
        input: GenerationRequest::text(vec![ModelMessage::user("你好")]),
        options: ModelRequestOptions {
            max_tokens: Some(7),
            timeout: Some(ModelTimeout::from_duration(Duration::new(3, 17))),
            limits: GenerationLimits {
                max_output_bytes: 1024,
                max_tool_calls: 2,
            },
        },
    };
    let wire = json!({
        "version":1,"call_id":"model-fixed","mode":"complete",
        "input":{"messages":[{"role":"user","content":"你好"}],"constraint":{"mode":"text"}},
        "options":{"max_tokens":7,"timeout":{"secs":3,"nanos":17},"limits":{"max_output_bytes":1024,"max_tool_calls":2}}
    });
    assert_eq!(json!(request), wire);
    assert_eq!(
        serde_json::from_value::<ModelRequest>(wire.clone()).unwrap(),
        request
    );
    for version in [None, Some(json!(0)), Some(json!(2))] {
        let mut bad = wire.clone();
        match version {
            Some(v) => bad["version"] = v,
            None => {
                bad.as_object_mut().unwrap().remove("version");
            }
        }
        assert!(serde_json::from_value::<ModelRequest>(bad).is_err());
    }
}
#[test]
fn canonical_result_wire_retains_success_metadata_and_failure_partial() {
    let response = GenerationResponse {
        usage: TokenUsage {
            input_tokens: Some(0),
            output_tokens: Some(2),
            total_tokens: None,
        },
        ..GenerationResponse::text("你好", FinishReason::Stop)
    };
    let result = ModelResult {
        version: ModelRecordVersion::V1,
        call_id: ModelCallId::from("m"),
        outcome: ModelRecordedOutcome::Succeeded {
            response: response.clone(),
        },
    };
    let wire =
        json!({"version":1,"call_id":"m","outcome":{"status":"succeeded","response":response}});
    assert_eq!(json!(result), wire);
    assert_eq!(serde_json::from_value::<ModelResult>(wire).unwrap(), result);
    let failed = ModelResult {
        version: ModelRecordVersion::V1,
        call_id: ModelCallId::from("m"),
        outcome: ModelRecordedOutcome::Failed {
            category: ModelFailureCategory::Cancelled,
            code: "model_cancelled".into(),
            message: "cancelled".into(),
            retryable: false,
            upstream_status: None,
            partial: GenerationPartial {
                content: Some("半".into()),
                tool_calls: vec![ToolCallFragment {
                    index: 0,
                    id: "call".into(),
                    name: "lookup".into(),
                    arguments: "{".into(),
                }],
            },
        },
    };
    let wire = json!(failed);
    assert_eq!(wire["outcome"]["status"], "failed");
    assert_eq!(wire["outcome"]["category"], "cancelled");
    assert_eq!(
        wire["outcome"]["partial"]["tool_calls"][0]["arguments"],
        "{"
    );
    assert_eq!(
        serde_json::from_value::<ModelResult>(wire.clone()).unwrap(),
        failed
    );
    let mut old = wire;
    old.as_object_mut().unwrap().remove("version");
    assert!(serde_json::from_value::<ModelResult>(old).is_err());
}

struct Fake {
    capabilities: ModelCapabilities,
    output: GenerationResponse,
    events: Vec<Result<GenerationStreamEvent, LlmError>>,
    pending: bool,
    calls: AtomicUsize,
    tokens: Mutex<Vec<CancellationToken>>,
}
impl Fake {
    fn new(output: GenerationResponse) -> Self {
        Self {
            capabilities: capabilities(),
            output,
            events: vec![],
            pending: false,
            calls: AtomicUsize::new(0),
            tokens: Mutex::new(vec![]),
        }
    }
    fn with_events(events: Vec<Result<GenerationStreamEvent, LlmError>>) -> Self {
        Self {
            events,
            ..Self::new(GenerationResponse::text("unused", FinishReason::Stop))
        }
    }
}
impl Llm for Fake {
    fn capabilities(&self) -> ModelCapabilities {
        self.capabilities
    }
    fn generate<'a>(
        &'a self,
        _: &'a GenerationRequest,
        _: GenerationOptions,
        cancel: CancellationToken,
    ) -> LlmFuture<'a, Result<GenerationResponse, LlmError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.tokens.lock().unwrap().push(cancel);
        Box::pin(async move {
            if self.pending {
                std::future::pending::<()>().await;
            }
            Ok(self.output.clone())
        })
    }
    fn generate_stream(
        &self,
        _: GenerationRequest,
        _: GenerationOptions,
        cancel: CancellationToken,
    ) -> GenerationStream {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.tokens.lock().unwrap().push(cancel);
        let events = stream::iter(self.events.clone());
        if self.pending {
            Box::pin(events.chain(stream::pending()))
        } else {
            Box::pin(events)
        }
    }
}
struct Provider(Arc<dyn Llm>);
impl ServiceFactory<dyn Llm> for Provider {
    fn construct<'a>(
        &'a self,
        _: FactoryContext<'a>,
    ) -> LifecycleFuture<'a, Result<ManagedService<dyn Llm>, RuntimeError>> {
        Box::pin(async { Ok(ManagedService::ready(Arc::clone(&self.0))) })
    }
}
impl Plugin for Provider {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor::provider("private-unified", LLM_PROVIDER, "fake")
    }
    fn mount(&self, ctx: &mut MountContext<'_>) -> Result<(), MountError> {
        ctx.provide_llm_factory(Arc::new(Self(Arc::clone(&self.0))))
    }
}
struct Signal(CancellationToken);
impl CancellationSignal for Signal {
    fn is_cancelled(&self) -> bool {
        self.0.is_cancelled()
    }
    fn cancelled(&self) -> CancellationFuture<'_> {
        Box::pin(self.0.cancelled())
    }
}
#[derive(Default)]
struct Recorder {
    records: Mutex<Vec<ModelRecord>>,
    fail: Option<ModelRecordStage>,
}
impl ModelEventRecorder for Recorder {
    fn append(
        &self,
        record: ModelRecord,
    ) -> ModelFuture<'_, Result<Arc<SessionEvent>, jingwei::session::SessionRuntimeError>> {
        Box::pin(async move {
            let stage = match record {
                ModelRecord::Request(_) => ModelRecordStage::Request,
                ModelRecord::Result(_) => ModelRecordStage::Result,
            };
            if self.fail == Some(stage) {
                return Err(jingwei::session::SessionRuntimeError::Stopped);
            }
            let mut records = self.records.lock().unwrap();
            let seq = records.len() as u64;
            records.push(record.clone());
            Ok(Arc::new(SessionEvent {
                event_id: EventId::new(),
                session_id: SessionId::from("s"),
                turn_id: TurnId::from("t"),
                generation_id: None,
                message_id: None,
                seq,
                kind: record.into_session_event_kind(),
            }))
        })
    }
}
async fn bind(
    provider: Arc<dyn Llm>,
    recorder: Arc<Recorder>,
    cancel: CancellationToken,
) -> (PluginRegistry, Box<dyn ModelTurn>) {
    let mut registrar = Registrar::default();
    registrar.add(Provider(provider));
    registrar.add(CanonicalLlmRuntimePlugin::new().with_default_timeout(Duration::from_secs(2)));
    registrar.select(LLM_RUNTIME, "canonical");
    let registry = registrar.finish().await.unwrap();
    let turn = registry
        .llm_runtime()
        .unwrap()
        .bind_turn(ModelTurnBinding::new(Arc::new(Signal(cancel)), recorder))
        .unwrap();
    (registry, turn)
}
async fn close(registry: PluginRegistry, turn: Box<dyn ModelTurn>) {
    timeout(
        Duration::from_secs(3),
        turn.finish(ModelFinishMode::Graceful),
    )
    .await
    .unwrap()
    .unwrap();
    registry.shutdown().await.unwrap();
}
fn assert_protocol(error: ModelGatewayError, expected: ModelProtocolError) {
    match error {
        ModelGatewayError::Model(LlmError::Protocol(actual)) => assert_eq!(actual, expected),
        other => panic!("{other:?}"),
    }
}
#[tokio::test]
async fn controlled_text_response_and_effective_options_are_recorded_without_loss() {
    let output = GenerationResponse {
        usage: TokenUsage {
            total_tokens: Some(4),
            ..Default::default()
        },
        ..GenerationResponse::text("你好", FinishReason::Stop)
    };
    let provider = Arc::new(Fake::new(output.clone()));
    let recorder = Arc::new(Recorder::default());
    let (registry, turn) = bind(provider, recorder.clone(), CancellationToken::new()).await;
    assert_eq!(
        turn.gateway()
            .generate(
                &input(),
                GenerationOptions {
                    max_tokens: Some(12),
                    timeout: Some(Duration::from_secs(20)),
                    ..Default::default()
                }
            )
            .await
            .unwrap(),
        output
    );
    close(registry, turn).await;
    let records = recorder.records.lock().unwrap();
    let ModelRecord::Request(request) = &records[0] else {
        panic!()
    };
    assert_eq!(request.input, input());
    assert_eq!(request.options.max_tokens, Some(12));
    assert_eq!(
        request.options.timeout,
        Some(ModelTimeout::from_duration(Duration::from_secs(2)))
    );
    let ModelRecord::Result(result) = &records[1] else {
        panic!()
    };
    assert_eq!(
        result.outcome,
        ModelRecordedOutcome::Succeeded { response: output }
    );
}
#[tokio::test]
async fn controlled_schema_validation_and_external_ref_rejection_precede_inference() {
    for bad_schema in [
        json!({"type":27}),
        json!({"$ref":"https://invalid.test/private.json"}),
        json!({"$ref":"file:///private/schema.json"}),
    ] {
        let request = GenerationRequest {
            constraint: GenerationConstraint::JsonSchema {
                name: "x".into(),
                schema: bad_schema,
            },
            ..input()
        };
        let provider = Arc::new(Fake::new(GenerationResponse::text(
            "{}",
            FinishReason::Stop,
        )));
        let (registry, turn) = bind(
            provider.clone(),
            Arc::new(Recorder::default()),
            CancellationToken::new(),
        )
        .await;
        assert_protocol(
            turn.gateway()
                .generate(&request, GenerationOptions::default())
                .await
                .unwrap_err(),
            ModelProtocolError::SchemaValidation,
        );
        assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
        close(registry, turn).await;
    }
}
#[tokio::test]
async fn controlled_json_output_is_validated_against_schema_not_only_syntax() {
    for (value, valid) in [
        ("{\"answer\":42}", true),
        ("{\"answer\":\"wrong\"}", false),
        ("{}", false),
    ] {
        let (registry, turn) = bind(
            Arc::new(Fake::new(GenerationResponse::text(
                value,
                FinishReason::Stop,
            ))),
            Arc::new(Recorder::default()),
            CancellationToken::new(),
        )
        .await;
        let result = turn
            .gateway()
            .generate(&schema(), GenerationOptions::default())
            .await;
        if valid {
            assert_eq!(result.unwrap().content.as_deref(), Some(value));
        } else {
            assert_protocol(result.unwrap_err(), ModelProtocolError::SchemaValidation);
        }
        close(registry, turn).await;
    }
}
#[tokio::test]
async fn controlled_native_tool_arguments_are_schema_checked() {
    for (arguments, valid) in [(json!({"query":"x"}), true), (json!({"query":2}), false)] {
        let output = GenerationResponse {
            content: None,
            tool_calls: vec![ModelToolCall {
                id: ProviderToolCallId::new("id").unwrap(),
                name: "lookup".into(),
                arguments,
            }],
            incomplete_tool_calls: vec![],
            finish_reason: FinishReason::ToolCalls,
            usage: TokenUsage::default(),
        };
        let (registry, turn) = bind(
            Arc::new(Fake::new(output)),
            Arc::new(Recorder::default()),
            CancellationToken::new(),
        )
        .await;
        let result = turn
            .gateway()
            .generate(&native(), GenerationOptions::default())
            .await;
        assert_eq!(result.is_ok(), valid);
        if !valid {
            assert_protocol(result.unwrap_err(), ModelProtocolError::SchemaValidation);
        }
        close(registry, turn).await;
    }
}
#[tokio::test]
async fn controlled_stream_records_before_delivering_finished() {
    let provider = Arc::new(Fake::with_events(completed(
        vec![text_delta("你"), text_delta("好")],
        FinishReason::Stop,
    )));
    let recorder = Arc::new(Recorder::default());
    let (registry, turn) = bind(provider, recorder.clone(), CancellationToken::new()).await;
    let mut stream = turn
        .gateway()
        .generate_stream(input(), GenerationOptions::default());
    let mut terminals = 0;
    while let Some(event) = stream.next().await {
        if let GenerationStreamEvent::Finished(response) = event.unwrap() {
            terminals += 1;
            assert_eq!(response.content.as_deref(), Some("你好"));
            assert_eq!(recorder.records.lock().unwrap().len(), 2);
        }
    }
    assert_eq!(terminals, 1);
    drop(stream);
    close(registry, turn).await;
}
#[tokio::test]
async fn controlled_stream_rejects_missing_and_forged_terminals_and_keeps_partial() {
    for (events, expected) in [
        (
            vec![Ok(GenerationStreamEvent::Delta(text_delta("partial")))],
            ModelProtocolError::MissingStreamTerminal,
        ),
        (
            vec![
                Ok(GenerationStreamEvent::Delta(text_delta("partial"))),
                Ok(GenerationStreamEvent::Finished(GenerationResponse::text(
                    "forged",
                    FinishReason::Stop,
                ))),
            ],
            ModelProtocolError::StreamTerminalMismatch,
        ),
    ] {
        let recorder = Arc::new(Recorder::default());
        let (registry, turn) = bind(
            Arc::new(Fake::with_events(events)),
            recorder.clone(),
            CancellationToken::new(),
        )
        .await;
        let events: Vec<_> = turn
            .gateway()
            .generate_stream(input(), GenerationOptions::default())
            .collect()
            .await;
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, Ok(GenerationStreamEvent::Finished(_))))
        );
        assert_protocol(events.last().unwrap().clone().unwrap_err(), expected);
        close(registry, turn).await;
        let records = recorder.records.lock().unwrap();
        let ModelRecord::Result(ModelResult {
            outcome: ModelRecordedOutcome::Failed { partial, .. },
            ..
        }) = &records[1]
        else {
            panic!()
        };
        assert_eq!(partial.content.as_deref(), Some("partial"));
    }
}
#[tokio::test]
async fn recording_failure_never_releases_a_successful_terminal() {
    for stage in [ModelRecordStage::Request, ModelRecordStage::Result] {
        let provider = Arc::new(Fake::with_events(completed(
            vec![text_delta("ok")],
            FinishReason::Stop,
        )));
        let recorder = Arc::new(Recorder {
            fail: Some(stage),
            ..Default::default()
        });
        let (registry, turn) = bind(provider.clone(), recorder, CancellationToken::new()).await;
        let events: Vec<_> = turn
            .gateway()
            .generate_stream(input(), GenerationOptions::default())
            .collect()
            .await;
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, Ok(GenerationStreamEvent::Finished(_))))
        );
        assert!(matches!(
            events.last(),
            Some(Err(ModelGatewayError::Recording(_)))
        ));
        if stage == ModelRecordStage::Request {
            assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
        }
        assert!(turn.finish(ModelFinishMode::Graceful).await.is_err());
        registry.shutdown().await.unwrap();
    }
}
#[tokio::test]
async fn controlled_timeout_cancels_provider_for_both_modes() {
    for streaming in [false, true] {
        let provider = Arc::new(Fake {
            pending: true,
            ..Fake::new(GenerationResponse::text("unused", FinishReason::Stop))
        });
        let (registry, turn) = bind(
            provider.clone(),
            Arc::new(Recorder::default()),
            CancellationToken::new(),
        )
        .await;
        let options = GenerationOptions {
            timeout: Some(Duration::from_millis(10)),
            ..Default::default()
        };
        let error = if streaming {
            turn.gateway()
                .generate_stream(input(), options)
                .next()
                .await
                .unwrap()
                .unwrap_err()
        } else {
            turn.gateway()
                .generate(&input(), options)
                .await
                .unwrap_err()
        };
        assert!(matches!(error, ModelGatewayError::Model(LlmError::Timeout)));
        assert!(
            provider
                .tokens
                .lock()
                .unwrap()
                .iter()
                .all(CancellationToken::is_cancelled)
        );
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        close(registry, turn).await;
    }
}
#[tokio::test]
async fn dropping_stream_consumer_cancels_driver_and_retains_tool_fragment() {
    let provider = Arc::new(Fake {
        pending: true,
        ..Fake::with_events(vec![Ok(GenerationStreamEvent::Delta(tool_delta(
            0,
            Some("id"),
            Some("lookup"),
            "{",
        )))])
    });
    let recorder = Arc::new(Recorder::default());
    let (registry, turn) = bind(provider.clone(), recorder.clone(), CancellationToken::new()).await;
    let mut events = turn
        .gateway()
        .generate_stream(native(), GenerationOptions::default());
    assert!(matches!(
        events.next().await,
        Some(Ok(GenerationStreamEvent::Delta(_)))
    ));
    drop(events);
    close(registry, turn).await;
    assert!(
        provider
            .tokens
            .lock()
            .unwrap()
            .iter()
            .all(CancellationToken::is_cancelled)
    );
    let records = recorder.records.lock().unwrap();
    let ModelRecord::Result(ModelResult {
        outcome: ModelRecordedOutcome::Failed { partial, .. },
        ..
    }) = &records[1]
    else {
        panic!()
    };
    assert_eq!(partial.tool_calls[0].arguments, "{");
}
#[tokio::test]
async fn host_cancellation_stops_stream_after_partial_output() {
    let provider = Arc::new(Fake {
        pending: true,
        ..Fake::with_events(vec![Ok(GenerationStreamEvent::Delta(text_delta(
            "partial",
        )))])
    });
    let cancel = CancellationToken::new();
    let (registry, turn) = bind(
        provider.clone(),
        Arc::new(Recorder::default()),
        cancel.clone(),
    )
    .await;
    let mut events = turn
        .gateway()
        .generate_stream(input(), GenerationOptions::default());
    events.next().await.unwrap().unwrap();
    cancel.cancel();
    let error = timeout(Duration::from_secs(1), events.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(matches!(
        error,
        ModelGatewayError::Model(LlmError::Cancelled)
    ));
    drop(events);
    close(registry, turn).await;
    assert!(
        provider
            .tokens
            .lock()
            .unwrap()
            .iter()
            .all(CancellationToken::is_cancelled)
    );
}
#[tokio::test]
async fn controlled_limits_reject_oversized_complete_and_stream_outputs() {
    for streaming in [false, true] {
        let provider = Fake {
            events: completed(vec![text_delta(&"x".repeat(500))], FinishReason::Stop),
            ..Fake::new(GenerationResponse::text(
                "x".repeat(500),
                FinishReason::Stop,
            ))
        };
        let (registry, turn) = bind(
            Arc::new(provider),
            Arc::new(Recorder::default()),
            CancellationToken::new(),
        )
        .await;
        let options = GenerationOptions {
            limits: GenerationLimits {
                max_output_bytes: 100,
                max_tool_calls: 1,
            },
            ..Default::default()
        };
        let error = if streaming {
            turn.gateway()
                .generate_stream(input(), options)
                .next()
                .await
                .unwrap()
                .unwrap_err()
        } else {
            turn.gateway()
                .generate(&input(), options)
                .await
                .unwrap_err()
        };
        assert_protocol(error, ModelProtocolError::OutputLimitExceeded);
        close(registry, turn).await;
    }
}

// A one-request loopback server; no external model endpoint or authentication.
async fn server(body: Vec<Vec<u8>>, sse: bool) -> (OpenAiLlm, tokio::task::JoinHandle<Value>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        timeout(Duration::from_secs(5), async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let header_end = loop {
                let mut chunk = [0; 2048];
                let n = socket.read(&mut chunk).await.unwrap();
                assert!(n > 0);
                request.extend_from_slice(&chunk[..n]);
                assert!(request.len() < 1024 * 1024);
                if let Some(end) = request.windows(4).position(|p| p == b"\r\n\r\n") { break end + 4; }
            };
            let headers = std::str::from_utf8(&request[..header_end]).unwrap();
            assert!(headers.starts_with("POST /v1/chat/completions "));
            let length: usize = headers.lines().find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse().unwrap())
            }).unwrap();
            while request.len() < header_end + length {
                let mut chunk = [0; 2048];
                let n = socket.read(&mut chunk).await.unwrap();
                assert!(n > 0);
                request.extend_from_slice(&chunk[..n]);
            }
            let input: Value = serde_json::from_slice(&request[header_end..header_end + length]).unwrap();
            let kind = if sse { "text/event-stream" } else { "application/json" };
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
            for chunk in body {
                if chunk.is_empty() { continue; }
                if socket.write_all(format!("{:x}\r\n", chunk.len()).as_bytes()).await.is_err() { return input; }
                if socket.write_all(&chunk).await.is_err() { return input; }
                if socket.write_all(b"\r\n").await.is_err() { return input; }
                tokio::task::yield_now().await;
            }
            let _ = socket.write_all(b"0\r\n\r\n").await;
            input
        }).await.unwrap()
    });
    let adapter = OpenAiLlm::new(
        OpenAiConfig::new(format!("http://{address}/v1"), "private-mock")
            .unwrap()
            .with_native_tools(support())
            .with_json_schema(support())
            .with_stream_usage(true)
            .with_request_timeout(Duration::from_secs(3))
            .unwrap(),
    )
    .unwrap();
    (adapter, task)
}
fn completion(content: Option<&str>, calls: Value, reason: Option<&str>) -> Vec<u8> {
    json!({"choices":[{"index":0,"message":{"role":"assistant","content":content,"tool_calls":calls},"finish_reason":reason}],
        "usage":{"prompt_tokens":0,"completion_tokens":2}}).to_string().into_bytes()
}
fn chunk(delta: Value, reason: Option<&str>) -> String {
    format!(
        "data: {}\n\n",
        json!({"choices":[{"index":0,"delta":delta,"finish_reason":reason}]})
    )
}
async fn raw_events(
    body: String,
    request: GenerationRequest,
    bytewise: bool,
) -> Vec<Result<GenerationStreamEvent, LlmError>> {
    let chunks = if bytewise {
        body.into_bytes()
            .into_iter()
            .map(|byte| vec![byte])
            .collect()
    } else {
        vec![body.into_bytes()]
    };
    let (adapter, server) = server(chunks, true).await;
    let events = adapter
        .generate_stream(
            request,
            GenerationOptions::default(),
            CancellationToken::new(),
        )
        .collect()
        .await;
    server.await.unwrap();
    events
}
#[tokio::test]
async fn compatible_completion_preserves_text_usage_and_request_options() {
    let (adapter, server) = server(
        vec![completion(Some("你好"), Value::Null, Some("stop"))],
        false,
    )
    .await;
    let output = adapter
        .generate(
            &input(),
            GenerationOptions {
                max_tokens: Some(8),
                ..Default::default()
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(output.content.as_deref(), Some("你好"));
    assert_eq!(output.usage.input_tokens, Some(0));
    assert_eq!(output.usage.output_tokens, Some(2));
    assert_eq!(output.usage.total_tokens, None);
    let wire = server.await.unwrap();
    assert_eq!(
        wire["messages"][0],
        json!({"role":"system","content":"简短回答"})
    );
    assert_eq!(wire["max_tokens"], 8);
    assert!(wire.get("tools").is_none());
    assert!(wire.get("response_format").is_none());
}
#[tokio::test]
async fn compatible_native_calls_and_history_map_ids_without_loss() {
    let calls = json!([{"id":"new-id","type":"function","function":{"name":"lookup","arguments":"{\"query\":\"你好\"}"}}]);
    let (adapter, server) = server(vec![completion(None, calls, Some("tool_calls"))], false).await;
    let mut request = native();
    request.messages.extend([
        ModelMessage::Assistant {
            content: None,
            tool_calls: vec![ModelToolCall {
                id: ProviderToolCallId::new("past-id").unwrap(),
                name: "lookup".into(),
                arguments: json!({"query":"old"}),
            }],
        },
        ModelMessage::Tool {
            call_id: ProviderToolCallId::new("past-id").unwrap(),
            content: "result".into(),
        },
    ]);
    let output = adapter
        .generate(
            &request,
            GenerationOptions::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(output.tool_calls[0].id.as_str(), "new-id");
    assert_eq!(output.tool_calls[0].arguments, json!({"query":"你好"}));
    let wire = server.await.unwrap();
    assert_eq!(wire["tool_choice"], "required");
    assert_eq!(wire["messages"][2]["tool_calls"][0]["id"], "past-id");
    assert_eq!(
        wire["messages"][2]["tool_calls"][0]["function"]["arguments"],
        "{\"query\":\"old\"}"
    );
    assert_eq!(wire["messages"][3]["tool_call_id"], "past-id");
}
#[tokio::test]
async fn compatible_json_schema_transport_is_explicit_and_not_strict_claimed() {
    let (adapter, server) = server(
        vec![completion(
            Some("{\"answer\":42}"),
            Value::Null,
            Some("stop"),
        )],
        false,
    )
    .await;
    adapter
        .generate(
            &schema(),
            GenerationOptions::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let wire = server.await.unwrap();
    assert_eq!(wire["response_format"]["type"], "json_schema");
    assert_eq!(wire["response_format"]["json_schema"]["strict"], false);
    assert_eq!(
        wire["response_format"]["json_schema"]["schema"]["required"],
        json!(["answer"])
    );
}
#[tokio::test]
async fn compatible_incomplete_completion_retains_unexecutable_fragments() {
    let calls = json!([{"id":"id","type":"function","function":{"name":"lookup","arguments":"{"}}]);
    let (adapter, server) = server(vec![completion(None, calls, Some("length"))], false).await;
    let output = adapter
        .generate(
            &native(),
            GenerationOptions::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(output.tool_calls.is_empty());
    assert_eq!(output.incomplete_tool_calls[0].arguments, "{");
    assert_eq!(output.finish_reason, FinishReason::Length);
    assert!(output.into_message().is_err());
    server.await.unwrap();
}
#[tokio::test]
async fn compatible_stream_handles_split_utf8_multiline_and_usage_only_tail() {
    let mut body = ": heartbeat\r\n\r\ndata:\r\n\r\n".to_string();
    body += "data: {\"choices\":[\r\ndata: {\"index\":0,\"delta\":{\"content\":\"你好🌱\"},\"finish_reason\":null}]}\r\n\r\n";
    body += &chunk(json!({}), Some("stop"));
    body += "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":4,\"completion_tokens\":3,\"total_tokens\":7}}\n\ndata: [DONE]\n\n";
    let events = raw_events(body, input(), true).await;
    assert_eq!(
        events[0],
        Ok(GenerationStreamEvent::Delta(text_delta("你好🌱")))
    );
    let Ok(GenerationStreamEvent::Finished(output)) = events.last().unwrap() else {
        panic!("{events:?}")
    };
    assert_eq!(output.content.as_deref(), Some("你好🌱"));
    assert_eq!(output.usage.total_tokens, Some(7));
    assert_eq!(events.len(), 2);
}
#[tokio::test]
async fn compatible_stream_assembles_multiple_tool_argument_fragments() {
    let body = chunk(
        json!({"tool_calls":[
            {"index":1,"id":"b","type":"function","function":{"name":"lookup","arguments":"{\"query\":"}},
            {"index":0,"id":"a","type":"function","function":{"name":"lookup","arguments":"{\"query\":\"甲\"}"}}
        ]}),
        None,
    ) + &chunk(
        json!({"tool_calls":[{"index":1,"function":{"arguments":"\"乙\"}"}}]}),
        None,
    ) + &chunk(json!({}), Some("tool_calls"))
        + "data: [DONE]\n\n";
    let events = raw_events(body, native(), true).await;
    let Ok(GenerationStreamEvent::Finished(output)) = events.last().unwrap() else {
        panic!("{events:?}")
    };
    assert_eq!(output.tool_calls.len(), 2);
    assert_eq!(output.tool_calls[0].id.as_str(), "a");
    assert_eq!(output.tool_calls[1].arguments, json!({"query":"乙"}));
}
#[tokio::test]
async fn compatible_stream_rejects_eof_without_done_or_without_finish_reason() {
    for body in [
        chunk(json!({"content":"text"}), Some("stop")),
        chunk(json!({"content":"text"}), None) + "data: [DONE]\n\n",
        "data: {not json}\n\ndata: [DONE]\n\n".into(),
    ] {
        let events = raw_events(body, input(), false).await;
        assert!(events.last().unwrap().is_err());
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, Ok(GenerationStreamEvent::Finished(_))))
        );
    }
}
#[tokio::test]
async fn compatible_unknown_finish_reason_is_not_assumed_stop() {
    let (adapter, server) =
        server(vec![completion(Some("partial"), Value::Null, None)], false).await;
    let output = adapter
        .generate(
            &input(),
            GenerationOptions::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(output.finish_reason, FinishReason::Unknown);
    assert!(output.into_message().is_err());
    server.await.unwrap();
}
#[tokio::test]
async fn adapter_preflight_and_precancel_never_connect_to_endpoint() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adapter = OpenAiLlm::new(
        OpenAiConfig::new(
            format!("http://{}/v1", listener.local_addr().unwrap()),
            "unused",
        )
        .unwrap(),
    )
    .unwrap();
    for request in [native(), schema()] {
        assert!(matches!(
            adapter
                .generate(
                    &request,
                    GenerationOptions::default(),
                    CancellationToken::new()
                )
                .await,
            Err(LlmError::Protocol(
                ModelProtocolError::UnsupportedCapability
            ))
        ));
        assert!(matches!(
            adapter
                .generate_stream(
                    request,
                    GenerationOptions::default(),
                    CancellationToken::new()
                )
                .next()
                .await,
            Some(Err(LlmError::Protocol(
                ModelProtocolError::UnsupportedCapability
            )))
        ));
    }
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert_eq!(
        adapter
            .generate(&input(), GenerationOptions::default(), cancel.clone())
            .await,
        Err(LlmError::Cancelled)
    );
    assert!(matches!(
        adapter
            .generate_stream(input(), GenerationOptions::default(), cancel)
            .next()
            .await,
        Some(Err(LlmError::Cancelled))
    ));
    assert!(
        timeout(Duration::from_millis(20), listener.accept())
            .await
            .is_err()
    );
}
