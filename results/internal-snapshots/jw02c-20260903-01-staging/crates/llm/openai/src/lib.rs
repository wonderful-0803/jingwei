//! OpenAI-compatible `/chat/completions` 适配器。
//!
//! 直接兼容 llama.cpp server（`http://127.0.0.1:8000/v1`）与小模型量化部署。
//! 本 crate 是 `jingwei_llm::Llm` 的第一个真实实现，reqwest 类型被隔离在此，
//! 不得泄漏进框架错误词汇。

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use async_stream::stream;
use futures::StreamExt;
use jingwei_llm::*;

mod sse;
mod wire;
use jingwei_plugin::{
    FactoryContext, LifecycleFuture, ManagedService, MountContext, MountError, Plugin,
    PluginDescriptor, RuntimeError, ServiceFactory,
};
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
    capabilities: ModelCapabilities,
    stream_usage: bool,
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
            capabilities: ModelCapabilities {
                token_usage: CapabilitySupport::Supported,
                ..ModelCapabilities::text_only()
            },
            stream_usage: false,
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

    /// Explicit host declaration for the selected server; no probing or fallback.
    #[must_use]
    pub fn with_native_tools(mut self, support: GenerationSupport) -> Self {
        self.capabilities.native_tools = support;
        self
    }

    /// Enable JSON Schema transport only after confirming server support.
    /// Requests use strict=false; canonical runtime independently validates output.
    #[must_use]
    pub fn with_json_schema(mut self, support: GenerationSupport) -> Self {
        self.capabilities.json_schema = support;
        self
    }

    #[must_use]
    pub fn with_stream_usage(mut self, enabled: bool) -> Self {
        self.stream_usage = enabled;
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
        input: &GenerationRequest,
        stream: bool,
        opts: &GenerationOptions,
    ) -> Result<reqwest::Response, LlmError> {
        input.preflight(
            &self.config.capabilities,
            if stream {
                ModelCallMode::Stream
            } else {
                ModelCallMode::Complete
            },
        )?;
        let payload = wire::request(
            &self.config.model,
            input,
            opts,
            stream,
            self.config.stream_usage,
        )?;
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
            let body =
                String::from_utf8_lossy(&read_body(response, opts.limits.max_output_bytes).await?)
                    .into_owned();
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
    fn capabilities(&self) -> ModelCapabilities {
        self.config.capabilities
    }

    fn generate<'a>(
        &'a self,
        input: &'a GenerationRequest,
        opts: GenerationOptions,
        cancel: CancellationToken,
    ) -> LlmFuture<'a, Result<GenerationResponse, LlmError>> {
        Box::pin(async move {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => Err(LlmError::Cancelled),
                result = async {
                    let response = self.request(input, false, &opts).await?;
                    let bytes = read_body(response, opts.limits.max_output_bytes).await?;
                    let output = wire::decode(&bytes, opts.limits)?;
                    if matches!(output.finish_reason, FinishReason::Stop | FinishReason::ToolCalls) {
                        output.validate_shape_for(input)?;
                    }
                    Ok(output)
                } => result,
            }
        })
    }

    fn generate_stream(
        &self,
        input: GenerationRequest,
        opts: GenerationOptions,
        cancel: CancellationToken,
    ) -> GenerationStream {
        let this = self.clone();
        Box::pin(stream! {
            let response = tokio::select! {
                biased;
                _ = cancel.cancelled() => { yield Err(LlmError::Cancelled); return; }
                result = this.request(&input, true, &opts) => match result {
                    Ok(response) => response,
                    Err(error) => { yield Err(error); return; }
                },
            };
            let mut bytes = response.bytes_stream();
            let mut decoder = sse::Decoder::new(opts.limits);
            loop {
                let chunk = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => { yield Err(LlmError::Cancelled); return; }
                    chunk = bytes.next() => chunk,
                };
                let Some(chunk) = chunk else {
                    yield Err(ModelProtocolError::MissingStreamTerminal.into());
                    return;
                };
                let chunk = match chunk {
                    Ok(bytes) => bytes,
                    Err(error) => { yield Err(classify_reqwest_error(error)); return; }
                };
                let events = match decoder.push(&chunk) {
                    Ok(events) => events,
                    Err(error) => { yield Err(error); return; }
                };
                for event in events {
                    if cancel.is_cancelled() { yield Err(LlmError::Cancelled); return; }
                    if let GenerationStreamEvent::Finished(output) = &event
                        && matches!(output.finish_reason, FinishReason::Stop | FinishReason::ToolCalls)
                        && let Err(error) = output.validate_shape_for(&input)
                    {
                        yield Err(error.into()); return;
                    }
                    yield Ok(event);
                }
                if decoder.is_done() { return; }
            }
        })
    }
}

async fn read_body(response: reqwest::Response, limit: usize) -> Result<Vec<u8>, LlmError> {
    let mut bytes = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = bytes.next().await {
        let chunk = chunk.map_err(classify_reqwest_error)?;
        if body.len().saturating_add(chunk.len()) > limit {
            return Err(ModelProtocolError::OutputLimitExceeded.into());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
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
        fn generate<'a>(
            &'a self,
            _input: &'a GenerationRequest,
            _opts: GenerationOptions,
            _cancel: CancellationToken,
        ) -> LlmFuture<'a, Result<GenerationResponse, LlmError>> {
            Box::pin(async { Err(LlmError::Adapter("not called".to_string())) })
        }

        fn generate_stream(
            &self,
            _input: GenerationRequest,
            _opts: GenerationOptions,
            _cancel: CancellationToken,
        ) -> GenerationStream {
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
}
