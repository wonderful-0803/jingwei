//! OpenAI-compatible `/chat/completions` 适配器。
//!
//! 直接兼容 llama.cpp server（`http://127.0.0.1:8000/v1`）与小模型量化部署。
//! 本 crate 是 `jingwei_llm::Llm` 的第一个真实实现，reqwest 类型被隔离在此，
//! 不得泄漏进框架错误词汇。

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use async_stream::stream;
use futures::StreamExt;
use jingwei_llm::{ChatMessage, Llm, LlmCallOptions, LlmCompletion, LlmDeltaStream, LlmError};
use jingwei_plugin::{
    FactoryContext, LifecycleFuture, ManagedService, MountContext, MountError, Plugin,
    PluginDescriptor, RuntimeError, ServiceFactory,
};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

#[cfg(test)]
static OPENAI_LLM_CONSTRUCTIONS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Stable candidate key for the adapter-owned OpenAI-compatible provider.
pub const OPENAI_PROVIDER_KEY: &str = "openai";

const DEFAULT_BASE_URL: &str = "http://127.0.0.1:8000/v1";
const DEFAULT_MODEL: &str = "default";
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(600);
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Immutable, validated configuration for an OpenAI-compatible endpoint.
#[derive(Clone)]
pub struct OpenAiConfig {
    base_url: reqwest::Url,
    model: String,
    api_key: Option<String>,
    request_timeout: Duration,
    connect_timeout: Duration,
    /// 是否走系统代理；本地 llama.cpp 场景通常关闭。
    use_system_proxy: bool,
}

impl Default for OpenAiConfig {
    fn default() -> Self {
        Self {
            base_url: reqwest::Url::parse(DEFAULT_BASE_URL)
                .expect("the built-in OpenAI base URL must be valid"),
            model: DEFAULT_MODEL.to_string(),
            api_key: None,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            use_system_proxy: false,
        }
    }
}

impl fmt::Debug for OpenAiConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAiConfig")
            .field("base_url_scheme", &self.base_url.scheme())
            .field("model", &self.model)
            .field("has_api_key", &self.has_api_key())
            .field("request_timeout", &self.request_timeout)
            .field("connect_timeout", &self.connect_timeout)
            .field("use_system_proxy", &self.use_system_proxy)
            .finish()
    }
}

/// Structural configuration failure. Variants never retain rejected input or secrets.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum OpenAiConfigError {
    #[error("OpenAI base URL must be a valid HTTP(S) URL")]
    InvalidBaseUrl,
    #[error("OpenAI model must not be empty")]
    EmptyModel,
    #[error("OpenAI request timeout must be non-zero")]
    ZeroRequestTimeout,
    #[error("OpenAI connect timeout must be non-zero")]
    ZeroConnectTimeout,
}

impl OpenAiConfig {
    /// Creates a validated configuration with the default timeout and proxy policy.
    pub fn new(
        base_url: impl Into<String>,
        model: impl Into<String>,
    ) -> Result<Self, OpenAiConfigError> {
        Ok(Self {
            base_url: validate_base_url(base_url.into())?,
            model: validate_model(model.into())?,
            ..Self::default()
        })
    }

    #[must_use]
    pub fn with_api_key(mut self, api_key: Option<String>) -> Self {
        self.api_key = api_key;
        self
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Result<Self, OpenAiConfigError> {
        self.base_url = validate_base_url(base_url.into())?;
        Ok(self)
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Result<Self, OpenAiConfigError> {
        self.model = validate_model(model.into())?;
        Ok(self)
    }

    pub fn with_request_timeout(
        mut self,
        request_timeout: Duration,
    ) -> Result<Self, OpenAiConfigError> {
        if request_timeout.is_zero() {
            return Err(OpenAiConfigError::ZeroRequestTimeout);
        }
        self.request_timeout = request_timeout;
        Ok(self)
    }

    pub fn with_connect_timeout(
        mut self,
        connect_timeout: Duration,
    ) -> Result<Self, OpenAiConfigError> {
        if connect_timeout.is_zero() {
            return Err(OpenAiConfigError::ZeroConnectTimeout);
        }
        self.connect_timeout = connect_timeout;
        Ok(self)
    }

    #[must_use]
    pub fn with_system_proxy(mut self, use_system_proxy: bool) -> Self {
        self.use_system_proxy = use_system_proxy;
        self
    }

    pub fn base_url(&self) -> &str {
        self.base_url.as_str()
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub const fn has_api_key(&self) -> bool {
        self.api_key.is_some()
    }

    pub const fn request_timeout(&self) -> Duration {
        self.request_timeout
    }

    pub const fn connect_timeout(&self) -> Duration {
        self.connect_timeout
    }

    pub const fn use_system_proxy(&self) -> bool {
        self.use_system_proxy
    }
}

fn validate_base_url(base_url: String) -> Result<reqwest::Url, OpenAiConfigError> {
    let base_url = base_url.trim();
    let parsed = reqwest::Url::parse(base_url).map_err(|_| OpenAiConfigError::InvalidBaseUrl)?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err(OpenAiConfigError::InvalidBaseUrl);
    }
    Ok(parsed)
}

fn validate_model(model: String) -> Result<String, OpenAiConfigError> {
    let model = model.trim();
    if model.is_empty() {
        return Err(OpenAiConfigError::EmptyModel);
    }
    Ok(model.to_string())
}

/// OpenAI-compatible 模型适配器。
#[derive(Clone)]
pub struct OpenAiLlm {
    config: OpenAiConfig,
    client: reqwest::Client,
}

impl OpenAiLlm {
    pub fn new(config: OpenAiConfig) -> Result<Self, LlmError> {
        #[cfg(test)]
        OPENAI_LLM_CONSTRUCTIONS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        let mut builder = reqwest::Client::builder()
            .timeout(config.request_timeout)
            .connect_timeout(config.connect_timeout);
        if !config.use_system_proxy {
            builder = builder.no_proxy();
        }
        let client = builder
            .build()
            .map_err(|e| LlmError::Adapter(e.to_string()))?;
        Ok(Self { config, client })
    }

    fn endpoint(&self) -> String {
        format!(
            "{}/chat/completions",
            self.config.base_url.as_str().trim_end_matches('/')
        )
    }

    async fn request(
        &self,
        messages: &[ChatMessage],
        stream: bool,
        opts: &LlmCallOptions,
    ) -> Result<reqwest::Response, LlmError> {
        let payload = ChatCompletionRequest {
            model: &self.config.model,
            messages,
            stream,
            max_tokens: opts.max_tokens,
        };
        let mut builder = self.client.post(self.endpoint()).json(&payload);
        if let Some(timeout) = opts.timeout {
            builder = builder.timeout(timeout);
        }
        if let Some(key) = &self.config.api_key {
            builder = builder.bearer_auth(key);
        }
        let response = builder.send().await.map_err(classify_reqwest_error)?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(LlmError::Upstream {
                status: status.as_u16(),
                body,
            });
        }
        Ok(response)
    }
}

/// Adapter-owned plugin that contributes the OpenAI-compatible raw LLM provider.
#[derive(Clone, Debug)]
pub struct OpenAiLlmPlugin {
    config: OpenAiConfig,
}

impl OpenAiLlmPlugin {
    pub fn new(config: OpenAiConfig) -> Self {
        Self { config }
    }
}

struct OpenAiLlmFactory {
    config: OpenAiConfig,
}

impl ServiceFactory<dyn Llm> for OpenAiLlmFactory {
    fn construct<'a>(
        &'a self,
        _ctx: FactoryContext<'a>,
    ) -> LifecycleFuture<'a, Result<ManagedService<dyn Llm>, RuntimeError>> {
        Box::pin(async move {
            let llm = OpenAiLlm::new(self.config.clone())
                .map_err(|error| RuntimeError::new(error.to_string()))?;
            let llm: Arc<dyn Llm> = Arc::new(llm);
            Ok(ManagedService::ready(llm))
        })
    }
}

impl Plugin for OpenAiLlmPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor::provider("llm-openai", jingwei_llm::LLM_PROVIDER, OPENAI_PROVIDER_KEY)
    }

    fn mount(&self, ctx: &mut MountContext<'_>) -> Result<(), MountError> {
        ctx.provide_llm_factory(Arc::new(OpenAiLlmFactory {
            config: self.config.clone(),
        }))
    }
}

fn classify_reqwest_error(error: reqwest::Error) -> LlmError {
    if error.is_timeout() {
        LlmError::Timeout
    } else {
        LlmError::Adapter(error.to_string())
    }
}

impl Llm for OpenAiLlm {
    fn capabilities(&self) -> jingwei_llm::ModelCapabilities {
        // Declare the paths implemented by this adapter, not every feature
        // an arbitrary server with a compatible endpoint might support.
        jingwei_llm::ModelCapabilities::text_only()
    }

    fn complete<'a>(
        &'a self,
        messages: &'a [ChatMessage],
        opts: LlmCallOptions,
    ) -> Pin<Box<dyn Future<Output = Result<LlmCompletion, LlmError>> + Send + 'a>> {
        Box::pin(async move {
            let response = self.request(messages, false, &opts).await?;
            let payload: ChatCompletionResponse = response
                .json()
                .await
                .map_err(|e| LlmError::Adapter(e.to_string()))?;
            let content = payload
                .choices
                .into_iter()
                .next()
                .and_then(|c| c.message.content)
                .ok_or(LlmError::MissingContent)?;
            Ok(LlmCompletion { content })
        })
    }

    fn complete_stream(
        &self,
        messages: Vec<ChatMessage>,
        opts: LlmCallOptions,
        cancel: CancellationToken,
    ) -> LlmDeltaStream {
        let this = self.clone();
        Box::pin(stream! {
            let response = match this.request(&messages, true, &opts).await {
                Ok(response) => response,
                Err(error) => {
                    yield Err(error);
                    return;
                }
            };
            let mut sse = response.bytes_stream();
            let mut buffer = String::new();
            let mut saw_done = false;
            loop {
                if cancel.is_cancelled() {
                    yield Err(LlmError::Cancelled);
                    return;
                }
                let chunk = tokio::select! {
                    _ = cancel.cancelled() => {
                        yield Err(LlmError::Cancelled);
                        return;
                    }
                    chunk = sse.next() => chunk,
                };
                let Some(chunk) = chunk else { break };
                let bytes = match chunk {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        yield Err(classify_reqwest_error(error));
                        return;
                    }
                };
                buffer.push_str(&String::from_utf8_lossy(&bytes));
                // 按行切分，最后一个元素可能是残行。
                let mut lines: Vec<&str> = buffer.lines().collect();
                let incomplete = !buffer.ends_with('\n');
                let trailing = if incomplete { lines.pop() } else { None };
                for line in lines {
                    match parse_sse_line(line) {
                        SseLine::Delta(text) => {
                            if !text.is_empty() {
                                yield Ok(text);
                            }
                        }
                        SseLine::Done => {
                            saw_done = true;
                        }
                        SseLine::Ignore => {}
                    }
                }
                buffer = match trailing {
                    Some(rest) if incomplete => rest.to_string(),
                    _ => String::new(),
                };
                if saw_done {
                    return;
                }
            }
        })
    }
}

#[derive(Debug)]
enum SseLine {
    Delta(String),
    Done,
    Ignore,
}

/// 解析一行 SSE：`data: {...}` 取 choices[0].delta.content；`data: [DONE]` 结束。
fn parse_sse_line(line: &str) -> SseLine {
    let Some(data) = line.strip_prefix("data:") else {
        return SseLine::Ignore;
    };
    let data = data.trim();
    if data.is_empty() || data == "[DONE]" {
        return SseLine::Done;
    }
    let Ok(parsed) = serde_json::from_str::<StreamChunk>(data) else {
        return SseLine::Ignore;
    };
    match parsed
        .choices
        .into_iter()
        .next()
        .and_then(|c| c.delta.content)
    {
        Some(content) => SseLine::Delta(content),
        None => SseLine::Ignore,
    }
}

/// 请求体：只序列化，不反序列化（`&[ChatMessage]` 不支持借用反序列化）。
#[derive(Serialize)]
struct ChatCompletionRequest<'a> {
    model: &'a str,
    messages: &'a [ChatMessage],
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
}

#[derive(Deserialize)]
struct ChatCompletionResponse {
    choices: Vec<ResponseChoice>,
}

#[derive(Deserialize)]
struct ResponseChoice {
    message: ResponseMessage,
}

#[derive(Deserialize)]
struct ResponseMessage {
    content: Option<String>,
}

#[derive(Deserialize)]
struct StreamChunk {
    choices: Vec<StreamChoice>,
}

#[derive(Deserialize)]
struct StreamChoice {
    delta: StreamDelta,
}

#[derive(Deserialize)]
struct StreamDelta {
    content: Option<String>,
}

#[cfg(test)]
mod tests {
    use std::io::ErrorKind;
    use std::net::TcpListener;
    use std::sync::Arc;

    use jingwei_llm::{LLM_PROVIDER, Llm};
    use jingwei_plugin::{
        FactoryContext, LifecycleFuture, ManagedService, MountContext, MountError, Plugin,
        PluginDescriptor, Registrar, RuntimeError, ServiceFactory,
    };

    use super::*;

    #[test]
    fn config_validation_and_redaction_matrix() {
        let defaults = OpenAiConfig::default();
        assert_eq!(defaults.base_url(), "http://127.0.0.1:8000/v1");
        assert_eq!(defaults.model(), "default");
        assert_eq!(defaults.request_timeout(), Duration::from_secs(600));
        assert_eq!(defaults.connect_timeout(), Duration::from_secs(10));
        assert!(!defaults.has_api_key());
        assert!(!defaults.use_system_proxy());

        let configured = OpenAiConfig::new(" https://example.test/v1/ ", " model-a ")
            .expect("valid HTTP(S) configuration")
            .with_api_key(Some("matrix-secret".to_string()))
            .with_request_timeout(Duration::from_secs(30))
            .expect("non-zero request timeout")
            .with_connect_timeout(Duration::from_secs(5))
            .expect("non-zero connect timeout")
            .with_system_proxy(true);
        assert_eq!(configured.base_url(), "https://example.test/v1/");
        assert_eq!(configured.model(), "model-a");
        assert_eq!(configured.request_timeout(), Duration::from_secs(30));
        assert_eq!(configured.connect_timeout(), Duration::from_secs(5));
        assert!(configured.has_api_key());
        assert!(configured.use_system_proxy());
        let debug = format!("{configured:?}");
        assert!(!debug.contains("matrix-secret"));
        assert!(debug.contains("has_api_key: true"));
        assert!(
            !format!("{:?}", OpenAiLlmPlugin::new(configured.clone())).contains("matrix-secret")
        );
        assert!(
            OpenAiConfig::default()
                .with_api_key(Some("none".to_string()))
                .has_api_key(),
            "adapter configuration must not interpret shell sentinels"
        );

        let url_secret = "url-secret-that-must-not-be-debugged";
        let sensitive_url = OpenAiConfig::new(
            format!("https://user:{url_secret}@example.test/v1?token={url_secret}"),
            "model-a",
        )
        .expect("HTTP URL with userinfo remains structurally valid");
        assert!(!format!("{sensitive_url:?}").contains(url_secret));

        let invalid = [
            (
                "unparseable base URL",
                OpenAiConfig::new("not a URL", "model").expect_err("URL must be rejected"),
                OpenAiConfigError::InvalidBaseUrl,
            ),
            (
                "non-HTTP base URL",
                OpenAiConfig::new("ftp://example.test/v1", "model")
                    .expect_err("scheme must be rejected"),
                OpenAiConfigError::InvalidBaseUrl,
            ),
            (
                "blank model",
                OpenAiConfig::new("https://example.test/v1", "  ")
                    .expect_err("blank model must be rejected"),
                OpenAiConfigError::EmptyModel,
            ),
            (
                "zero request timeout",
                configured
                    .clone()
                    .with_request_timeout(Duration::ZERO)
                    .expect_err("zero request timeout must be rejected"),
                OpenAiConfigError::ZeroRequestTimeout,
            ),
            (
                "zero connect timeout",
                configured
                    .clone()
                    .with_connect_timeout(Duration::ZERO)
                    .expect_err("zero connect timeout must be rejected"),
                OpenAiConfigError::ZeroConnectTimeout,
            ),
        ];
        for (case, actual, expected) in invalid {
            assert_eq!(actual, expected, "{case}");
            let diagnostic = format!("{actual:?}: {actual}");
            assert!(!diagnostic.contains("matrix-secret"), "{case}");
        }
    }

    struct ReadyLlm;

    impl Llm for ReadyLlm {
        fn complete<'a>(
            &'a self,
            _messages: &'a [ChatMessage],
            _opts: LlmCallOptions,
        ) -> Pin<Box<dyn Future<Output = Result<LlmCompletion, LlmError>> + Send + 'a>> {
            Box::pin(async { Err(LlmError::Adapter("not called".to_string())) })
        }

        fn complete_stream(
            &self,
            _messages: Vec<ChatMessage>,
            _opts: LlmCallOptions,
            _cancel: CancellationToken,
        ) -> LlmDeltaStream {
            Box::pin(futures::stream::empty())
        }
    }

    struct ReadyLlmFactory;

    impl ServiceFactory<dyn Llm> for ReadyLlmFactory {
        fn construct<'a>(
            &'a self,
            _ctx: FactoryContext<'a>,
        ) -> LifecycleFuture<'a, Result<ManagedService<dyn Llm>, RuntimeError>> {
            Box::pin(async {
                let llm: Arc<dyn Llm> = Arc::new(ReadyLlm);
                Ok(ManagedService::ready(llm))
            })
        }
    }

    struct ReadyLlmPlugin;

    impl Plugin for ReadyLlmPlugin {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor::provider("openai-test-ready", LLM_PROVIDER, "ready")
        }

        fn mount(&self, ctx: &mut MountContext<'_>) -> Result<(), MountError> {
            ctx.provide_llm_factory(Arc::new(ReadyLlmFactory))
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn provider_is_inert_until_selected_and_construction_performs_no_endpoint_request() {
        OPENAI_LLM_CONSTRUCTIONS.store(0, std::sync::atomic::Ordering::SeqCst);
        let endpoint = TcpListener::bind("127.0.0.1:0").expect("bind isolated endpoint probe");
        endpoint
            .set_nonblocking(true)
            .expect("make endpoint probe non-blocking");
        let base_url = format!(
            "http://{}/v1",
            endpoint.local_addr().expect("read endpoint probe address")
        );
        let config =
            OpenAiConfig::new(base_url, "probe-model").expect("local endpoint URL must be valid");

        let mut inactive = Registrar::default();
        inactive.add(OpenAiLlmPlugin::new(config.clone()));
        inactive.add(ReadyLlmPlugin);
        inactive.select(LLM_PROVIDER, "ready");
        let inactive_registry = inactive
            .finish()
            .await
            .expect("unselected OpenAI candidate must remain inert");
        assert_eq!(
            inactive_registry
                .binding(LLM_PROVIDER)
                .expect("ready provider must be bound")
                .key(),
            "ready"
        );
        assert_eq!(
            OPENAI_LLM_CONSTRUCTIONS.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "an unselected OpenAI candidate must not construct its client"
        );
        assert_eq!(
            endpoint
                .accept()
                .expect_err("inactive provider must not connect")
                .kind(),
            ErrorKind::WouldBlock
        );
        inactive_registry
            .shutdown()
            .await
            .expect("ready provider shutdown");

        let mut selected = Registrar::default();
        selected.add(OpenAiLlmPlugin::new(config));
        selected.select(LLM_PROVIDER, OPENAI_PROVIDER_KEY);
        let selected_registry = selected
            .finish()
            .await
            .expect("selected OpenAI provider must construct without probing its endpoint");
        let binding = selected_registry
            .binding(LLM_PROVIDER)
            .expect("OpenAI provider must be bound");
        assert_eq!(binding.key(), OPENAI_PROVIDER_KEY);
        assert_eq!(binding.owner().as_str(), "llm-openai");
        assert_eq!(
            OPENAI_LLM_CONSTRUCTIONS.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the selected provider must construct exactly once"
        );
        assert_eq!(
            endpoint
                .accept()
                .expect_err("client construction must not connect")
                .kind(),
            ErrorKind::WouldBlock
        );
        selected_registry
            .shutdown()
            .await
            .expect("OpenAI provider shutdown");
    }

    #[test]
    fn sse_delta_line_parses_content() {
        let line = r#"data: {"choices":[{"delta":{"content":"你好"}}]}"#;
        match parse_sse_line(line) {
            SseLine::Delta(text) => assert_eq!(text, "你好"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn sse_done_marker_terminates() {
        assert!(matches!(parse_sse_line("data: [DONE]"), SseLine::Done));
    }
}
