use crate::auth::AuthProvider;
use crate::auth::add_auth_headers;
use crate::common::ResponseStream;
use crate::error::ApiError;
use crate::provider::Provider;
use crate::telemetry::SseTelemetry;
use crate::telemetry::run_with_request_telemetry;
use codex_client::HttpTransport;
use codex_client::RequestTelemetry;
use codex_client::StreamResponse;
use http::HeaderMap;
use http::Method;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

pub(crate) struct StreamingClient<T: HttpTransport, A: AuthProvider> {
    transport: T,
    provider: Provider,
    auth: A,
    request_telemetry: Option<Arc<dyn RequestTelemetry>>,
    sse_telemetry: Option<Arc<dyn SseTelemetry>>,
}

impl<T: HttpTransport, A: AuthProvider> StreamingClient<T, A> {
    pub(crate) fn new(transport: T, provider: Provider, auth: A) -> Self {
        Self {
            transport,
            provider,
            auth,
            request_telemetry: None,
            sse_telemetry: None,
        }
    }

    pub(crate) fn with_telemetry(
        mut self,
        request: Option<Arc<dyn RequestTelemetry>>,
        sse: Option<Arc<dyn SseTelemetry>>,
    ) -> Self {
        self.request_telemetry = request;
        self.sse_telemetry = sse;
        self
    }

    pub(crate) fn provider(&self) -> &Provider {
        &self.provider
    }

    pub(crate) async fn stream(
        &self,
        path: &str,
        body: Value,
        extra_headers: HeaderMap,
        spawner: fn(StreamResponse, Duration, Option<Arc<dyn SseTelemetry>>) -> ResponseStream,
    ) -> Result<ResponseStream, ApiError> {
        // 🔍 DEBUG: 打印请求体，用于调试 GLM API 1213 错误
        // 检查 messages 是否存在和 user 消息
        let messages = body.get("messages");
        let has_user_msg = messages
            .and_then(|m| m.as_array())
            .map(|arr| arr.iter().any(|m| m.get("role").and_then(|r| r.as_str()) == Some("user")))
            .unwrap_or(false);
        let msg_count = messages
            .and_then(|m| m.as_array())
            .map(std::vec::Vec::len)
            .unwrap_or(0);

        tracing::warn!(
            "📤 [StreamingClient::stream] 请求概览: model={}, messages_count={}, has_user_msg={}, has_tools={}",
            body.get("model").and_then(|m| m.as_str()).unwrap_or("?"),
            msg_count,
            has_user_msg,
            body.get("tools").is_some()
        );

        // 如果没有 user 消息，打印完整请求体
        if !has_user_msg {
            tracing::warn!(
                "⚠️ [StreamingClient::stream] 警告：没有 user 消息！完整请求体: {}",
                serde_json::to_string_pretty(&body).unwrap_or_else(|_| body.to_string())
            );
        }

        let builder = || {
            let mut req = self.provider.build_request(Method::POST, path);
            req.headers.extend(extra_headers.clone());
            req.headers.insert(
                http::header::ACCEPT,
                http::HeaderValue::from_static("text/event-stream"),
            );
            req.body = Some(body.clone());
            add_auth_headers(&self.auth, req)
        };

        let stream_response = run_with_request_telemetry(
            self.provider.retry.to_policy(),
            self.request_telemetry.clone(),
            builder,
            |req| self.transport.stream(req),
        )
        .await?;

        Ok(spawner(
            stream_response,
            self.provider.stream_idle_timeout,
            self.sse_telemetry.clone(),
        ))
    }
}
