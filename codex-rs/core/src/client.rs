use std::sync::Arc;

use crate::api_bridge::auth_provider_from_auth;
use crate::api_bridge::map_api_error;
use codex_api::AggregateStreamExt;
use codex_api::ChatClient as ApiChatClient;
use codex_api::CompactClient as ApiCompactClient;
use codex_api::CompactionInput as ApiCompactionInput;
use codex_api::Prompt as ApiPrompt;
use codex_api::RequestTelemetry;
use codex_api::ReqwestTransport;
use codex_api::ResponseStream as ApiResponseStream;
use codex_api::ResponsesClient as ApiResponsesClient;
use codex_api::ResponsesOptions as ApiResponsesOptions;
use codex_api::SseTelemetry;
use codex_api::TransportError;
use codex_api::common::Reasoning;
use codex_api::create_text_param_for_request;
use codex_api::error::ApiError;
use codex_app_server_protocol::AuthMode;
use codex_otel::otel_event_manager::OtelEventManager;
use codex_protocol::ConversationId;
use codex_protocol::config_types::ReasoningSummary as ReasoningSummaryConfig;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ReasoningEffort as ReasoningEffortConfig;
use codex_protocol::protocol::SessionSource;
use eventsource_stream::Event;
use eventsource_stream::EventStreamError;
use futures::StreamExt;
use http::HeaderMap as ApiHeaderMap;
use http::HeaderValue;
use http::StatusCode as HttpStatusCode;
use reqwest::StatusCode;
use serde_json::Value;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::warn;

use crate::AuthManager;
use crate::auth::RefreshTokenError;
use crate::client_common::Prompt;
use crate::client_common::ResponseEvent;
use crate::client_common::ResponseStream;
use crate::config::Config;
use crate::default_client::build_reqwest_client;
use crate::error::CodexErr;
use crate::error::Result;
use crate::flags::CODEX_RS_SSE_FIXTURE;
use crate::model_family::ModelFamily;
use crate::model_provider_info::ModelProviderInfo;
use crate::model_provider_info::WireApi;
use crate::openai_model_info::get_model_info;
use crate::tools::spec::create_tools_json_for_chat_completions_api;
use crate::tools::spec::create_tools_json_for_responses_api;

#[derive(Debug, Clone)]
pub struct ModelClient {
    config: Arc<Config>,
    auth_manager: Option<Arc<AuthManager>>,
    otel_event_manager: OtelEventManager,
    provider: ModelProviderInfo,
    conversation_id: ConversationId,
    effort: Option<ReasoningEffortConfig>,
    summary: ReasoningSummaryConfig,
    session_source: SessionSource,
}

#[allow(clippy::too_many_arguments)]
impl ModelClient {
    pub fn new(
        config: Arc<Config>,
        auth_manager: Option<Arc<AuthManager>>,
        otel_event_manager: OtelEventManager,
        provider: ModelProviderInfo,
        effort: Option<ReasoningEffortConfig>,
        summary: ReasoningSummaryConfig,
        conversation_id: ConversationId,
        session_source: SessionSource,
    ) -> Self {
        Self {
            config,
            auth_manager,
            otel_event_manager,
            provider,
            conversation_id,
            effort,
            summary,
            session_source,
        }
    }

    pub fn get_model_context_window(&self) -> Option<i64> {
        let pct = self.config.model_family.effective_context_window_percent;
        self.config
            .model_context_window
            .or_else(|| get_model_info(&self.config.model_family).map(|info| info.context_window))
            .map(|w| w.saturating_mul(pct) / 100)
    }

    pub fn get_auto_compact_token_limit(&self) -> Option<i64> {
        self.config.model_auto_compact_token_limit.or_else(|| {
            get_model_info(&self.config.model_family).and_then(|info| info.auto_compact_token_limit)
        })
    }

    pub fn config(&self) -> Arc<Config> {
        Arc::clone(&self.config)
    }

    pub fn provider(&self) -> &ModelProviderInfo {
        &self.provider
    }

    /// Streams a single model turn using either the Responses or Chat
    /// Completions wire API, depending on the configured provider.
    ///
    /// For Chat providers, the underlying stream is optionally aggregated
    /// based on the `show_raw_agent_reasoning` flag in the config.
    pub async fn stream(&self, prompt: &Prompt) -> Result<ResponseStream> {
        match self.provider.wire_api {
            WireApi::Responses => self.stream_responses_api(prompt).await,
            WireApi::Chat => {
                let api_stream = self.stream_chat_completions(prompt).await?;

                if self.config.show_raw_agent_reasoning {
                    Ok(map_response_stream(
                        api_stream.streaming_mode(),
                        self.otel_event_manager.clone(),
                    ))
                } else {
                    Ok(map_response_stream(
                        api_stream.aggregate(),
                        self.otel_event_manager.clone(),
                    ))
                }
            }
        }
    }

    /// Streams a turn via the OpenAI Chat Completions API.
    ///
    /// This path is only used when the provider is configured with
    /// `WireApi::Chat`; it does not support `output_schema` today.
    async fn stream_chat_completions(&self, prompt: &Prompt) -> Result<ApiResponseStream> {
        if prompt.output_schema.is_some() {
            return Err(CodexErr::UnsupportedOperation(
                "output_schema is not supported for Chat Completions API".to_string(),
            ));
        }

        let auth_manager = self.auth_manager.clone();
        let instructions = prompt
            .get_full_instructions(&self.config.model_family)
            .into_owned();
        let tools_json = create_tools_json_for_chat_completions_api(&prompt.tools)?;
        let api_prompt = build_api_prompt(prompt, instructions, tools_json);
        let conversation_id = self.conversation_id.to_string();
        let session_source = self.session_source.clone();

        let mut refreshed = false;
        loop {
            let auth = auth_manager.as_ref().and_then(|m| m.auth());
            let api_provider = self
                .provider
                .to_api_provider(auth.as_ref().map(|a| a.mode))?;
            let api_auth = auth_provider_from_auth(auth.clone(), &self.provider).await?;
            let transport = ReqwestTransport::new(build_reqwest_client());
            let (request_telemetry, sse_telemetry) = self.build_streaming_telemetry();
            let client = ApiChatClient::new(transport, api_provider, api_auth)
                .with_telemetry(Some(request_telemetry), Some(sse_telemetry));

            let stream_result = client
                .stream_prompt(
                    &self.config.model,
                    &api_prompt,
                    Some(conversation_id.clone()),
                    Some(session_source.clone()),
                )
                .await;

            match stream_result {
                Ok(stream) => return Ok(stream),
                Err(ApiError::Transport(TransportError::Http { status, .. }))
                    if status == StatusCode::UNAUTHORIZED =>
                {
                    handle_unauthorized(status, &mut refreshed, &auth_manager, &auth).await?;
                    continue;
                }
                Err(err) => return Err(map_api_error(err)),
            }
        }
    }

    /// Streams a turn via the OpenAI Responses API.
    ///
    /// Handles SSE fixtures, reasoning summaries, verbosity, and the
    /// `text` controls used for output schemas.
    async fn stream_responses_api(&self, prompt: &Prompt) -> Result<ResponseStream> {
        if let Some(path) = &*CODEX_RS_SSE_FIXTURE {
            warn!(path, "Streaming from fixture");
            let stream = codex_api::stream_from_fixture(path, self.provider.stream_idle_timeout())
                .map_err(map_api_error)?;
            return Ok(map_response_stream(stream, self.otel_event_manager.clone()));
        }

        let auth_manager = self.auth_manager.clone();
        let instructions = prompt
            .get_full_instructions(&self.config.model_family)
            .into_owned();
        let tools_json: Vec<Value> = create_tools_json_for_responses_api(&prompt.tools)?;

        let reasoning = if self.config.model_family.supports_reasoning_summaries {
            Some(Reasoning {
                effort: self
                    .effort
                    .or(self.config.model_family.default_reasoning_effort),
                summary: Some(self.summary),
            })
        } else {
            None
        };

        let include: Vec<String> = if reasoning.is_some() {
            vec!["reasoning.encrypted_content".to_string()]
        } else {
            vec![]
        };

        let verbosity = if self.config.model_family.support_verbosity {
            self.config
                .model_verbosity
                .or(self.config.model_family.default_verbosity)
        } else {
            if self.config.model_verbosity.is_some() {
                warn!(
                    "model_verbosity is set but ignored as the model does not support verbosity: {}",
                    self.config.model_family.family
                );
            }
            None
        };

        let text = create_text_param_for_request(verbosity, &prompt.output_schema);
        let api_prompt = build_api_prompt(prompt, instructions.clone(), tools_json);
        let conversation_id = self.conversation_id.to_string();
        let session_source = self.session_source.clone();

        let mut refreshed = false;
        loop {
            let auth = auth_manager.as_ref().and_then(|m| m.auth());
            let api_provider = self
                .provider
                .to_api_provider(auth.as_ref().map(|a| a.mode))?;
            let api_auth = auth_provider_from_auth(auth.clone(), &self.provider).await?;
            let transport = ReqwestTransport::new(build_reqwest_client());
            let (request_telemetry, sse_telemetry) = self.build_streaming_telemetry();
            let client = ApiResponsesClient::new(transport, api_provider, api_auth)
                .with_telemetry(Some(request_telemetry), Some(sse_telemetry));

            let options = ApiResponsesOptions {
                reasoning: reasoning.clone(),
                include: include.clone(),
                prompt_cache_key: Some(conversation_id.clone()),
                text: text.clone(),
                store_override: None,
                conversation_id: Some(conversation_id.clone()),
                session_source: Some(session_source.clone()),
            };

            let stream_result = client
                .stream_prompt(&self.config.model, &api_prompt, options)
                .await;

            match stream_result {
                Ok(stream) => {
                    return Ok(map_response_stream(stream, self.otel_event_manager.clone()));
                }
                Err(ApiError::Transport(TransportError::Http { status, .. }))
                    if status == StatusCode::UNAUTHORIZED =>
                {
                    handle_unauthorized(status, &mut refreshed, &auth_manager, &auth).await?;
                    continue;
                }
                Err(err) => return Err(map_api_error(err)),
            }
        }
    }

    pub fn get_provider(&self) -> ModelProviderInfo {
        self.provider.clone()
    }

    pub fn get_otel_event_manager(&self) -> OtelEventManager {
        self.otel_event_manager.clone()
    }

    pub fn get_session_source(&self) -> SessionSource {
        self.session_source.clone()
    }

    /// Returns the currently configured model slug.
    pub fn get_model(&self) -> String {
        self.config.model.clone()
    }

    /// Returns the currently configured model family.
    pub fn get_model_family(&self) -> ModelFamily {
        self.config.model_family.clone()
    }

    /// Returns the current reasoning effort setting.
    pub fn get_reasoning_effort(&self) -> Option<ReasoningEffortConfig> {
        self.effort
    }

    /// Returns the current reasoning summary setting.
    pub fn get_reasoning_summary(&self) -> ReasoningSummaryConfig {
        self.summary
    }

    pub fn get_auth_manager(&self) -> Option<Arc<AuthManager>> {
        self.auth_manager.clone()
    }

    /// Compacts the current conversation history using the Compact endpoint.
    ///
    /// This is a unary call (no streaming) that returns a new list of
    /// `ResponseItem`s representing the compacted transcript.
    pub async fn compact_conversation_history(&self, prompt: &Prompt) -> Result<Vec<ResponseItem>> {
        if prompt.input.is_empty() {
            return Ok(Vec::new());
        }
        let auth_manager = self.auth_manager.clone();
        let auth = auth_manager.as_ref().and_then(|m| m.auth());
        let api_provider = self
            .provider
            .to_api_provider(auth.as_ref().map(|a| a.mode))?;
        let api_auth = auth_provider_from_auth(auth.clone(), &self.provider).await?;
        let transport = ReqwestTransport::new(build_reqwest_client());
        let request_telemetry = self.build_request_telemetry();
        let client = ApiCompactClient::new(transport, api_provider, api_auth)
            .with_telemetry(Some(request_telemetry));

        let instructions = prompt
            .get_full_instructions(&self.config.model_family)
            .into_owned();
        let payload = ApiCompactionInput {
            model: &self.config.model,
            input: &prompt.input,
            instructions: &instructions,
        };

        let mut extra_headers = ApiHeaderMap::new();
        if let SessionSource::SubAgent(sub) = &self.session_source {
            let subagent = if let crate::protocol::SubAgentSource::Other(label) = sub {
                label.clone()
            } else {
                serde_json::to_value(sub)
                    .ok()
                    .and_then(|v| v.as_str().map(std::string::ToString::to_string))
                    .unwrap_or_else(|| "other".to_string())
            };
            if let Ok(val) = HeaderValue::from_str(&subagent) {
                extra_headers.insert("x-openai-subagent", val);
            }
        }

        client
            .compact_input(&payload, extra_headers)
            .await
            .map_err(map_api_error)
    }
}

impl ModelClient {
    /// Builds request and SSE telemetry for streaming API calls (Chat/Responses).
    fn build_streaming_telemetry(&self) -> (Arc<dyn RequestTelemetry>, Arc<dyn SseTelemetry>) {
        let telemetry = Arc::new(ApiTelemetry::new(self.otel_event_manager.clone()));
        let request_telemetry: Arc<dyn RequestTelemetry> = telemetry.clone();
        let sse_telemetry: Arc<dyn SseTelemetry> = telemetry;
        (request_telemetry, sse_telemetry)
    }

    /// Builds request telemetry for unary API calls (e.g., Compact endpoint).
    fn build_request_telemetry(&self) -> Arc<dyn RequestTelemetry> {
        let telemetry = Arc::new(ApiTelemetry::new(self.otel_event_manager.clone()));
        let request_telemetry: Arc<dyn RequestTelemetry> = telemetry;
        request_telemetry
    }
}

/// Adapts the core `Prompt` type into the `codex-api` payload shape.
fn build_api_prompt(prompt: &Prompt, instructions: String, tools_json: Vec<Value>) -> ApiPrompt {
    ApiPrompt {
        instructions,
        input: prompt.get_formatted_input(),
        tools: tools_json,
        parallel_tool_calls: prompt.parallel_tool_calls,
        output_schema: prompt.output_schema.clone(),
    }
}

fn map_response_stream<S>(api_stream: S, otel_event_manager: OtelEventManager) -> ResponseStream
where
    S: futures::Stream<Item = std::result::Result<ResponseEvent, ApiError>>
        + Unpin
        + Send
        + 'static,
{
    let (tx_event, rx_event) = mpsc::channel::<Result<ResponseEvent>>(1600);
    let manager = otel_event_manager;
    tokio::spawn(async move {
        let mut logged_error = false;
        let mut api_stream = api_stream;
        while let Some(event) = api_stream.next().await {
            match event {
                Ok(ResponseEvent::Completed {
                    response_id,
                    token_usage,
                }) => {
                    if let Some(usage) = &token_usage {
                        manager.sse_event_completed(
                            usage.input_tokens,
                            usage.output_tokens,
                            Some(usage.cached_input_tokens),
                            Some(usage.reasoning_output_tokens),
                            usage.total_tokens,
                        );
                    }
                    if tx_event
                        .send(Ok(ResponseEvent::Completed {
                            response_id,
                            token_usage,
                        }))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                Ok(event) => {
                    if tx_event.send(Ok(event)).await.is_err() {
                        return;
                    }
                }
                Err(err) => {
                    let mapped = map_api_error(err);
                    if !logged_error {
                        manager.see_event_completed_failed(&mapped);
                        logged_error = true;
                    }
                    if tx_event.send(Err(mapped)).await.is_err() {
                        return;
                    }
                }
            }
        }
    });

    ResponseStream { rx_event }
}

async fn handle_unauthorized(
    status: StatusCode,
    refreshed: &mut bool,
    auth_manager: &Option<Arc<AuthManager>>,
    auth: &Option<crate::auth::CodexAuth>,
) -> Result<()> {
    if *refreshed {
        return Err(map_unauthorized_status(status));
    }

    if let Some(manager) = auth_manager.as_ref()
        && let Some(auth) = auth.as_ref()
        && auth.mode == AuthMode::ChatGPT
    {
        match manager.refresh_token().await {
            Ok(_) => {
                *refreshed = true;
                Ok(())
            }
            Err(RefreshTokenError::Permanent(failed)) => Err(CodexErr::RefreshTokenFailed(failed)),
            Err(RefreshTokenError::Transient(other)) => Err(CodexErr::Io(other)),
        }
    } else {
        Err(map_unauthorized_status(status))
    }
}

fn map_unauthorized_status(status: StatusCode) -> CodexErr {
    map_api_error(ApiError::Transport(TransportError::Http {
        status,
        headers: None,
        body: None,
    }))
}

struct ApiTelemetry {
    otel_event_manager: OtelEventManager,
}

impl ApiTelemetry {
    fn new(otel_event_manager: OtelEventManager) -> Self {
        Self { otel_event_manager }
    }
}

impl RequestTelemetry for ApiTelemetry {
    fn on_request(
        &self,
        attempt: u64,
        status: Option<HttpStatusCode>,
        error: Option<&TransportError>,
        duration: Duration,
    ) {
        let error_message = error.map(std::string::ToString::to_string);
        self.otel_event_manager.record_api_request(
            attempt,
            status.map(|s| s.as_u16()),
            error_message.as_deref(),
            duration,
        );
    }
}

impl SseTelemetry for ApiTelemetry {
    fn on_sse_poll(
        &self,
        result: &std::result::Result<
            Option<std::result::Result<Event, EventStreamError<TransportError>>>,
            tokio::time::error::Elapsed,
        >,
        duration: Duration,
    ) {
        self.otel_event_manager.log_sse_event(result, duration);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_api::sse::process_sse;
    use futures::TryStreamExt;
    use serde_json::json;
    use tokio::sync::mpsc;
    use tokio_test::io::Builder as IoBuilder;
    use tokio_util::io::ReaderStream;

    // ────────────────────────────
    // Helpers
    // ────────────────────────────

    /// Runs the SSE parser on pre-chunked byte slices and returns every event
    /// (including any final `Err` from a stream-closure check).
    async fn collect_events(
        chunks: &[&[u8]],
        provider: ModelProviderInfo,
        _otel_event_manager: OtelEventManager,
    ) -> Vec<Result<ResponseEvent>> {
        let mut builder = IoBuilder::new();
        for chunk in chunks {
            builder.read(chunk);
        }

        let reader = builder.build();
        let stream = ReaderStream::new(reader)
            .map_err(|err| codex_api::TransportError::Network(err.to_string()));
        let (tx, mut rx) =
            mpsc::channel::<std::result::Result<ResponseEvent, codex_api::error::ApiError>>(16);
        tokio::spawn(process_sse(
            Box::pin(stream),
            tx,
            provider.stream_idle_timeout(),
            None,
        ));

        let mut events = Vec::new();
        while let Some(ev) = rx.recv().await {
            events.push(ev.map_err(map_api_error));
        }
        events
    }

    /// Builds an in-memory SSE stream from JSON fixtures and returns only the
    /// successfully parsed events (panics on internal channel errors).
    #[allow(dead_code)]
    async fn run_sse(
        events: Vec<serde_json::Value>,
        provider: ModelProviderInfo,
        _otel_event_manager: OtelEventManager,
    ) -> Vec<ResponseEvent> {
        let mut body = String::new();
        for e in events {
            let kind = e
                .get("type")
                .and_then(|v| v.as_str())
                .expect("fixture event missing type");
            if e.as_object().map(|o| o.len() == 1).unwrap_or(false) {
                body.push_str(&format!("event: {kind}\n\n"));
            } else {
                body.push_str(&format!("event: {kind}\ndata: {e}\n\n"));
            }
        }

        let (tx, mut rx) =
            mpsc::channel::<std::result::Result<ResponseEvent, codex_api::error::ApiError>>(8);
        let stream = ReaderStream::new(std::io::Cursor::new(body))
            .map_err(|err| codex_api::TransportError::Network(err.to_string()));
        tokio::spawn(process_sse(
            Box::pin(stream),
            tx,
            provider.stream_idle_timeout(),
            None,
        ));

        let mut out = Vec::new();
        while let Some(ev) = rx.recv().await {
            out.push(ev.map_err(map_api_error).expect("channel closed"));
        }
        out
    }

    fn otel_event_manager() -> OtelEventManager {
        OtelEventManager::new(
            ConversationId::new(),
            "test",
            "test",
            None,
            Some("test@test.com".to_string()),
            Some(AuthMode::ChatGPT),
            false,
            "test".to_string(),
        )
    }

    // ────────────────────────────
    // Tests from `implement-test-for-responses-api-sse-parser`
    // ────────────────────────────

    #[tokio::test]
    async fn parses_items_and_completed() {
        let item1 = json!({
            "type": "response.output_item.done",
            "item": {
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "Hello"}]
            }
        })
        .to_string();

        let item2 = json!({
            "type": "response.output_item.done",
            "item": {
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "World"}]
            }
        })
        .to_string();

        let completed = json!({
            "type": "response.completed",
            "response": { "id": "resp1" }
        })
        .to_string();

        let sse1 = format!("event: response.output_item.done\ndata: {item1}\n\n");
        let sse2 = format!("event: response.output_item.done\ndata: {item2}\n\n");
        let sse3 = format!("event: response.completed\ndata: {completed}\n\n");

        let provider = ModelProviderInfo {
            name: "test".to_string(),
            base_url: Some("https://test.com".to_string()),
            env_key: Some("TEST_API_KEY".to_string()),
            env_key_instructions: None,
            experimental_bearer_token: None,
            wire_api: WireApi::Responses,
            query_params: None,
            http_headers: None,
            env_http_headers: None,
            request_max_retries: Some(0),
            stream_max_retries: Some(0),
            stream_idle_timeout_ms: Some(1000),
            requires_openai_auth: false,
        };

        let otel_event_manager = otel_event_manager();

        let events = collect_events(
            &[sse1.as_bytes(), sse2.as_bytes(), sse3.as_bytes()],
            provider,
            otel_event_manager,
        )
        .await;

        assert_eq!(events.len(), 3);

        matches!(
            &events[0],
            Ok(ResponseEvent::OutputItemDone(ResponseItem::Message { role, .. }))
                if role == "assistant"
        );

        matches!(
            &events[1],
            Ok(ResponseEvent::OutputItemDone(ResponseItem::Message { role, .. }))
                if role == "assistant"
        );

        match &events[2] {
            Ok(ResponseEvent::Completed {
                response_id,
                token_usage,
            }) => {
                assert_eq!(response_id, "resp1");
                assert!(token_usage.is_none());
            }
            other => panic!("unexpected third event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn error_when_missing_completed() {
        let item1 = json!({
            "type": "response.output_item.done",
            "item": {
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "Hello"}]
            }
        })
        .to_string();

        let sse1 = format!("event: response.output_item.done\ndata: {item1}\n\n");
        let provider = ModelProviderInfo {
            name: "test".to_string(),
            base_url: Some("https://test.com".to_string()),
            env_key: Some("TEST_API_KEY".to_string()),
            env_key_instructions: None,
            experimental_bearer_token: None,
            wire_api: WireApi::Responses,
            query_params: None,
            http_headers: None,
            env_http_headers: None,
            request_max_retries: Some(0),
            stream_max_retries: Some(0),
            stream_idle_timeout_ms: Some(1000),
            requires_openai_auth: false,
        };

        let otel_event_manager = otel_event_manager();

        let events = collect_events(&[sse1.as_bytes()], provider, otel_event_manager).await;

        assert_eq!(events.len(), 2);

        matches!(events[0], Ok(ResponseEvent::OutputItemDone(_)));

        match &events[1] {
            Err(CodexErr::Stream(msg, _)) => {
                assert_eq!(msg, "stream closed before response.completed")
            }
            other => panic!("unexpected second event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn error_when_error_event() {
        let raw_error = r#"{"type":"response.failed","sequence_number":3,"response":{"id":"resp_689bcf18d7f08194bf3440ba62fe05d803fee0cdac429894","object":"response","created_at":1755041560,"status":"failed","background":false,"error":{"code":"rate_limit_exceeded","message":"Rate limit reached for gpt-5.1 in organization org-AAA on tokens per min (TPM): Limit 30000, Used 22999, Requested 12528. Please try again in 11.054s. Visit https://platform.openai.com/account/rate-limits to learn more."}, "usage":null,"user":null,"metadata":{}}}"#;

        let sse1 = format!("event: response.failed\ndata: {raw_error}\n\n");
        let provider = ModelProviderInfo {
            name: "test".to_string(),
            base_url: Some("https://test.com".to_string()),
            env_key: Some("TEST_API_KEY".to_string()),
            env_key_instructions: None,
            experimental_bearer_token: None,
            wire_api: WireApi::Responses,
            query_params: None,
            http_headers: None,
            env_http_headers: None,
            request_max_retries: Some(0),
            stream_max_retries: Some(0),
            stream_idle_timeout_ms: Some(1000),
            requires_openai_auth: false,
        };

        let otel_event_manager = otel_event_manager();

        let events = collect_events(&[sse1.as_bytes()], provider, otel_event_manager).await;

        assert_eq!(events.len(), 1);

        match &events[0] {
            Err(CodexErr::Stream(msg, delay)) => {
                assert_eq!(
                    msg,
                    "Rate limit reached for gpt-5.1 in organization org-AAA on tokens per min (TPM): Limit 30000, Used 22999, Requested 12528. Please try again in 11.054s. Visit https://platform.openai.com/account/rate-limits to learn more."
                );
                assert_eq!(*delay, Some(Duration::from_secs_f64(11.054)));
            }
            other => panic!("unexpected second event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn context_window_error_is_fatal() {
        let raw_error = r#"{"type":"response.failed","sequence_number":3,"response":{"id":"resp_5c66275b97b9baef1ed95550adb3b7ec13b17aafd1d2f11b","object":"response","created_at":1759510079,"status":"failed","background":false,"error":{"code":"context_length_exceeded","message":"Your input exceeds the context window of this model. Please adjust your input and try again."},"usage":null,"user":null,"metadata":{}}}"#;

        let sse1 = format!("event: response.failed\ndata: {raw_error}\n\n");
        let provider = ModelProviderInfo {
            name: "test".to_string(),
            base_url: Some("https://test.com".to_string()),
            env_key: Some("TEST_API_KEY".to_string()),
            env_key_instructions: None,
            experimental_bearer_token: None,
            wire_api: WireApi::Responses,
            query_params: None,
            http_headers: None,
            env_http_headers: None,
            request_max_retries: Some(0),
            stream_max_retries: Some(0),
            stream_idle_timeout_ms: Some(1000),
            requires_openai_auth: false,
        };

        let otel_event_manager = otel_event_manager();

        let events = collect_events(&[sse1.as_bytes()], provider, otel_event_manager).await;

        assert_eq!(events.len(), 1);

        match &events[0] {
            Err(err @ CodexErr::ContextWindowExceeded) => {
                assert_eq!(err.to_string(), CodexErr::ContextWindowExceeded.to_string());
            }
            other => panic!("unexpected context window event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn context_window_error_with_newline_is_fatal() {
        let raw_error = r#"{"type":"response.failed","sequence_number":4,"response":{"id":"resp_fatal_newline","object":"response","created_at":1759510080,"status":"failed","background":false,"error":{"code":"context_length_exceeded","message":"Your input exceeds the context window of this model. Please adjust your input and try\nagain."},"usage":null,"user":null,"metadata":{}}}"#;

        let sse1 = format!("event: response.failed\ndata: {raw_error}\n\n");
        let provider = ModelProviderInfo {
            name: "test".to_string(),
            base_url: Some("https://test.com".to_string()),
            env_key: Some("TEST_API_KEY".to_string()),
            env_key_instructions: None,
            experimental_bearer_token: None,
            wire_api: WireApi::Responses,
            query_params: None,
            http_headers: None,
            env_http_headers: None,
            request_max_retries: Some(0),
            stream_max_retries: Some(0),
            stream_idle_timeout_ms: Some(1000),
            requires_openai_auth: false,
        };

        let otel_event_manager = otel_event_manager();

        let events = collect_events(&[sse1.as_bytes()], provider, otel_event_manager).await;

        assert_eq!(events.len(), 1);

        match &events[0] {
            Err(err @ CodexErr::ContextWindowExceeded) => {
                assert_eq!(err.to_string(), CodexErr::ContextWindowExceeded.to_string());
            }
            other => panic!("unexpected context window event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn quota_exceeded_error_is_fatal() {
        let raw_error = r#"{"type":"response.failed","sequence_number":3,"response":{"id":"resp_fatal_quota","object":"response","created_at":1759771626,"status":"failed","background":false,"error":{"code":"insufficient_quota","message":"You exceeded your current quota, please check your plan and billing details. For more information on this error, read the docs: https://platform.openai.com/docs/guides/error-codes/api-errors."},"incomplete_details":null}}"#;

        let sse1 = format!("event: response.failed\ndata: {raw_error}\n\n");
        let provider = ModelProviderInfo {
            name: "test".to_string(),
            base_url: Some("https://test.com".to_string()),
            env_key: Some("TEST_API_KEY".to_string()),
            env_key_instructions: None,
            experimental_bearer_token: None,
            wire_api: WireApi::Responses,
            query_params: None,
            http_headers: None,
            env_http_headers: None,
            request_max_retries: Some(0),
            stream_max_retries: Some(0),
            stream_idle_timeout_ms: Some(1000),
            requires_openai_auth: false,
        };

        let otel_event_manager = otel_event_manager();

        let events = collect_events(&[sse1.as_bytes()], provider, otel_event_manager).await;

        assert_eq!(events.len(), 1);

        match &events[0] {
            Err(err @ CodexErr::QuotaExceeded) => {
                assert_eq!(err.to_string(), CodexErr::QuotaExceeded.to_string());
            }
            other => panic!("unexpected quota exceeded event: {other:?}"),
        }
    }
}

//
