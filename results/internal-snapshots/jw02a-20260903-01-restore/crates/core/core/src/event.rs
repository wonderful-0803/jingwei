//! 事件信封：框架唯一的运行时真相。
//!
//! 运行时日志、前端 trace、落盘 JSON、评测指标都是同一事件流的投影。
//! 业务负载对框架是 `serde_json::Value`；插件内部可自行维护强类型，
//! 序列化时收敛进信封（对齐 ADR-0000 决策 2）。

use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize};

use crate::id::{EventId, GenerationId, MessageId, ModelCallId, SessionId, TurnId};

/// 阶段进度状态。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StageStatus {
    Started,
    Completed,
    Failed,
}

/// 回合终态。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DoneStatus {
    Completed,
    /// 等待用户补充输入后继续（对齐 writing-rust 的 grill-me 挂起语义）。
    WaitingForInput,
    Cancelled,
}

/// Transport-free role shared by raw model adapters and canonical Session records.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
        }
    }
}

/// One exact model-visible message.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: content.into(),
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
        }
    }
}

/// Whether the controlled model call requested one completion or a stream.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelCallMode {
    Complete,
    Stream,
}

/// Validation failure for the stable `{ secs, nanos }` timeout wire.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ModelTimeoutError {
    #[error("model timeout nanoseconds {nanos} must be less than 1000000000")]
    NanosecondsOutOfRange { nanos: u32 },
}

/// Exact timeout represented independently of serde's `Duration` shape.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct ModelTimeout {
    secs: u64,
    nanos: u32,
}

impl ModelTimeout {
    pub fn try_new(secs: u64, nanos: u32) -> Result<Self, ModelTimeoutError> {
        if nanos >= 1_000_000_000 {
            return Err(ModelTimeoutError::NanosecondsOutOfRange { nanos });
        }
        Ok(Self { secs, nanos })
    }

    pub fn from_duration(duration: Duration) -> Self {
        Self {
            secs: duration.as_secs(),
            nanos: duration.subsec_nanos(),
        }
    }

    pub const fn secs(&self) -> u64 {
        self.secs
    }

    pub const fn nanos(&self) -> u32 {
        self.nanos
    }
}

impl<'de> Deserialize<'de> for ModelTimeout {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            secs: u64,
            nanos: u32,
        }

        let wire = Wire::deserialize(deserializer)?;
        Self::try_new(wire.secs, wire.nanos).map_err(serde::de::Error::custom)
    }
}

/// Effective controlled options that were supplied to the raw model adapter.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ModelRequestOptions {
    pub max_tokens: Option<u32>,
    pub timeout: Option<ModelTimeout>,
}

/// Canonical request recorded before the raw model adapter is invoked.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ModelRequest {
    pub call_id: ModelCallId,
    pub mode: ModelCallMode,
    pub messages: Vec<ChatMessage>,
    pub options: ModelRequestOptions,
}

/// Stable semantic category for a failed controlled model call.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelFailureCategory {
    Upstream,
    Timeout,
    StreamParse,
    MissingContent,
    Cancelled,
    Adapter,
    Internal,
}

/// Replayable terminal outcome of one canonical model call.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ModelRecordedOutcome {
    Succeeded {
        content: String,
    },
    Failed {
        category: ModelFailureCategory,
        code: String,
        message: String,
        retryable: bool,
        upstream_status: Option<u16>,
        partial_content: String,
    },
}

/// Canonical result paired to one [`ModelRequest`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ModelResult {
    pub call_id: ModelCallId,
    pub outcome: ModelRecordedOutcome,
}

/// 工具调用的线格式（jingwei-tool 直接复用，避免重复定义）。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

/// 工具结果的线格式。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub call_id: String,
    pub outcome: ToolRecordedOutcome,
}

/// Replayable result of one canonical Tool invocation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ToolRecordedOutcome {
    Succeeded {
        output: String,
    },
    Failed {
        category: ToolFailureCategory,
        code: String,
        message: String,
        retryable: bool,
    },
}

/// Stable semantic category for a failed Tool invocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolFailureCategory {
    Unavailable,
    InvalidArguments,
    Denied,
    GuardFault,
    ApprovalUnavailable,
    ApprovalDenied,
    ApprovalFault,
    Timeout,
    Cancelled,
    BodyFailure,
    BodyPanic,
    OutputLimit,
}

/// 一次会话事件。所有字段由 Harness 的回合运行时补齐；
/// 业务代码只填 `kind`。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionEvent {
    /// 幂等去重键（不变式 5）。
    pub event_id: EventId,
    pub session_id: SessionId,
    pub turn_id: TurnId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation_id: Option<GenerationId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<MessageId>,
    /// 同 turn 内严格递增（不变式 2）。
    /// Zero-based physical index in the Session-global canonical event log.
    pub seq: u64,
    pub kind: SessionEventKind,
}

/// 事件负载。`Custom` 是插件自有事件的通道（信封仍带全量关联 ID）。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEventKind {
    UserMessage {
        text: String,
    },
    AssistantDelta {
        text: String,
    },
    ModelRequest {
        request: ModelRequest,
    },
    ModelResult {
        result: ModelResult,
    },
    ToolCall {
        call: ToolCall,
    },
    ToolResult {
        result: ToolResult,
    },
    StageProgress {
        stage: String,
        status: StageStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    Mode {
        mode: String,
        decided_by: String,
        reason: String,
    },
    PlanReady {
        plan: serde_json::Value,
    },
    Done {
        status: DoneStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        artifact: Option<serde_json::Value>,
    },
    Error {
        code: String,
        message: String,
        retryable: bool,
    },
    Custom {
        plugin: String,
        kind: String,
        payload: serde_json::Value,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(kind: SessionEventKind) -> SessionEvent {
        SessionEvent {
            event_id: EventId::from("evt_1"),
            session_id: SessionId::from("sess_1"),
            turn_id: TurnId::from("turn_1"),
            generation_id: None,
            message_id: None,
            seq: 0,
            kind,
        }
    }

    #[test]
    fn event_envelope_roundtrips_json() {
        let event = event(SessionEventKind::Done {
            status: DoneStatus::Completed,
            artifact: None,
        });
        let json = serde_json::to_string(&event).unwrap();
        assert_eq!(
            json,
            r#"{"event_id":"evt_1","session_id":"sess_1","turn_id":"turn_1","seq":0,"kind":{"type":"done","status":"completed"}}"#
        );
        let back: SessionEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back.session_id, SessionId::from("sess_1"));
        assert!(matches!(
            back.kind,
            SessionEventKind::Done {
                status: DoneStatus::Completed,
                ..
            }
        ));
    }

    #[test]
    fn model_request_has_exact_json_and_precise_timeout() {
        let timeout = ModelTimeout::try_new(12, 345_678_901).unwrap();
        assert_eq!(timeout.secs(), 12);
        assert_eq!(timeout.nanos(), 345_678_901);
        assert_eq!(
            ModelTimeout::from_duration(Duration::new(12, 345_678_901)),
            timeout
        );

        let event = event(SessionEventKind::ModelRequest {
            request: ModelRequest {
                call_id: ModelCallId::from("mcall_1"),
                mode: ModelCallMode::Complete,
                messages: vec![
                    ChatMessage::system("You are exact."),
                    ChatMessage::user("hello"),
                ],
                options: ModelRequestOptions {
                    max_tokens: Some(512),
                    timeout: Some(timeout),
                },
            },
        });
        let json = serde_json::to_string(&event).unwrap();
        assert_eq!(
            json,
            r#"{"event_id":"evt_1","session_id":"sess_1","turn_id":"turn_1","seq":0,"kind":{"type":"model_request","request":{"call_id":"mcall_1","mode":"complete","messages":[{"role":"system","content":"You are exact."},{"role":"user","content":"hello"}],"options":{"max_tokens":512,"timeout":{"secs":12,"nanos":345678901}}}}}"#
        );
        assert_eq!(serde_json::from_str::<SessionEvent>(&json).unwrap(), event);
    }

    #[test]
    fn model_results_have_exact_tagged_json() {
        let cases = [
            (
                SessionEventKind::ModelResult {
                    result: ModelResult {
                        call_id: ModelCallId::from("mcall_success"),
                        outcome: ModelRecordedOutcome::Succeeded {
                            content: "hello".to_string(),
                        },
                    },
                },
                r#"{"type":"model_result","result":{"call_id":"mcall_success","outcome":{"status":"succeeded","content":"hello"}}}"#,
            ),
            (
                SessionEventKind::ModelResult {
                    result: ModelResult {
                        call_id: ModelCallId::from("mcall_upstream"),
                        outcome: ModelRecordedOutcome::Failed {
                            category: ModelFailureCategory::Upstream,
                            code: "model_upstream".to_string(),
                            message: "Model provider rejected the request".to_string(),
                            retryable: true,
                            upstream_status: Some(429),
                            partial_content: String::new(),
                        },
                    },
                },
                r#"{"type":"model_result","result":{"call_id":"mcall_upstream","outcome":{"status":"failed","category":"upstream","code":"model_upstream","message":"Model provider rejected the request","retryable":true,"upstream_status":429,"partial_content":""}}}"#,
            ),
            (
                SessionEventKind::ModelResult {
                    result: ModelResult {
                        call_id: ModelCallId::from("mcall_cancelled"),
                        outcome: ModelRecordedOutcome::Failed {
                            category: ModelFailureCategory::Cancelled,
                            code: "stream_consumer_dropped".to_string(),
                            message: "Model stream consumer dropped".to_string(),
                            retryable: false,
                            upstream_status: None,
                            partial_content: "hel".to_string(),
                        },
                    },
                },
                r#"{"type":"model_result","result":{"call_id":"mcall_cancelled","outcome":{"status":"failed","category":"cancelled","code":"stream_consumer_dropped","message":"Model stream consumer dropped","retryable":false,"upstream_status":null,"partial_content":"hel"}}}"#,
            ),
        ];

        for (kind, expected) in cases {
            let json = serde_json::to_string(&kind).unwrap();
            assert_eq!(json, expected);
            assert_eq!(
                serde_json::from_str::<SessionEventKind>(&json).unwrap(),
                kind
            );
        }
    }

    #[test]
    fn model_timeout_rejects_invalid_nanoseconds_on_all_inputs() {
        assert_eq!(
            ModelTimeout::try_new(1, 1_000_000_000),
            Err(ModelTimeoutError::NanosecondsOutOfRange {
                nanos: 1_000_000_000
            })
        );
        assert!(serde_json::from_str::<ModelTimeout>(r#"{"secs":1,"nanos":1000000000}"#).is_err());
        assert_eq!(
            serde_json::from_str::<ModelTimeout>(r#"{"secs":1,"nanos":999999999}"#).unwrap(),
            ModelTimeout::try_new(1, 999_999_999).unwrap()
        );
    }

    #[test]
    fn structured_tool_results_roundtrip_without_display_parsing() {
        let succeeded = ToolResult {
            call_id: "call_success".to_string(),
            outcome: ToolRecordedOutcome::Succeeded {
                output: "indexed 3 files".to_string(),
            },
        };
        assert_eq!(
            serde_json::to_string(&succeeded).unwrap(),
            r#"{"call_id":"call_success","outcome":{"status":"succeeded","output":"indexed 3 files"}}"#
        );

        let failed = ToolResult {
            call_id: "call_failure".to_string(),
            outcome: ToolRecordedOutcome::Failed {
                category: ToolFailureCategory::ApprovalDenied,
                code: "approval_denied".to_string(),
                message: "the requested operation was not approved".to_string(),
                retryable: false,
            },
        };
        let json = serde_json::to_string(&failed).unwrap();
        assert_eq!(
            json,
            r#"{"call_id":"call_failure","outcome":{"status":"failed","category":"approval_denied","code":"approval_denied","message":"the requested operation was not approved","retryable":false}}"#
        );
        assert_eq!(serde_json::from_str::<ToolResult>(&json).unwrap(), failed);
    }
}
