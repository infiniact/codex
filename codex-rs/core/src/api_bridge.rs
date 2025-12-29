use chrono::DateTime;
use chrono::Utc;
use codex_api::AuthProvider as ApiAuthProvider;
use codex_api::TransportError;
use codex_api::error::ApiError;
use codex_api::rate_limits::parse_rate_limit;
use http::HeaderMap;
use serde::Deserialize;

use crate::auth::CodexAuth;
use crate::error::CodexErr;
use crate::error::RetryLimitReachedError;
use crate::error::UnexpectedResponseError;
use crate::error::UsageLimitReachedError;
use crate::model_provider_info::ModelProviderInfo;
use crate::token_data::PlanType;

pub(crate) fn map_api_error(err: ApiError) -> CodexErr {
    match err {
        ApiError::ContextWindowExceeded => CodexErr::ContextWindowExceeded,
        ApiError::QuotaExceeded => CodexErr::QuotaExceeded,
        ApiError::UsageNotIncluded => CodexErr::UsageNotIncluded,
        ApiError::Retryable { message, delay } => CodexErr::Stream(message, delay),
        ApiError::Stream(msg) => CodexErr::Stream(msg, None),
        ApiError::Api { status, message } => CodexErr::UnexpectedStatus(UnexpectedResponseError {
            status,
            body: message,
            request_id: None,
        }),
        ApiError::Transport(transport) => match transport {
            TransportError::Http {
                status,
                headers,
                body,
            } => {
                let body_text = body.unwrap_or_default();

                if status == http::StatusCode::BAD_REQUEST {
                    if body_text
                        .contains("The image data you provided does not represent a valid image")
                    {
                        CodexErr::InvalidImageRequest()
                    } else {
                        CodexErr::InvalidRequest(body_text)
                    }
                } else if status == http::StatusCode::INTERNAL_SERVER_ERROR {
                    CodexErr::InternalServerError
                } else if status == http::StatusCode::FORBIDDEN {
                    // IAAccount 代理服务使用 403 状态码返回 usage_limit_reached 错误
                    if let Ok(err) = serde_json::from_str::<UsageErrorResponse>(&body_text)
                        && err.error.error_type.as_deref() == Some("usage_limit_reached")
                    {
                        let rate_limits = headers.as_ref().and_then(parse_rate_limit);
                        let resets_at = err
                            .error
                            .resets_at
                            .and_then(|seconds| DateTime::<Utc>::from_timestamp(seconds, 0));
                        // 如果来源是 iaaccount，使用原始消息
                        let custom_message = if err.error.source.as_deref() == Some("iaaccount") {
                            err.error.message.clone()
                        } else {
                            None
                        };
                        return CodexErr::UsageLimitReached(UsageLimitReachedError {
                            plan_type: err.error.plan_type,
                            resets_at,
                            rate_limits,
                            custom_message,
                        });
                    }
                    CodexErr::UnexpectedStatus(UnexpectedResponseError {
                        status,
                        body: body_text,
                        request_id: extract_request_id(headers.as_ref()),
                    })
                } else if status == http::StatusCode::TOO_MANY_REQUESTS {
                    if let Ok(err) = serde_json::from_str::<UsageErrorResponse>(&body_text) {
                        if err.error.error_type.as_deref() == Some("usage_limit_reached") {
                            let rate_limits = headers.as_ref().and_then(parse_rate_limit);
                            let resets_at = err
                                .error
                                .resets_at
                                .and_then(|seconds| DateTime::<Utc>::from_timestamp(seconds, 0));
                            // 如果来源是 iaaccount，使用原始消息
                            let custom_message = if err.error.source.as_deref() == Some("iaaccount") {
                                err.error.message.clone()
                            } else {
                                None
                            };
                            return CodexErr::UsageLimitReached(UsageLimitReachedError {
                                plan_type: err.error.plan_type,
                                resets_at,
                                rate_limits,
                                custom_message,
                            });
                        } else if err.error.error_type.as_deref() == Some("usage_not_included") {
                            return CodexErr::UsageNotIncluded;
                        }
                    }

                    CodexErr::RetryLimit(RetryLimitReachedError {
                        status,
                        request_id: extract_request_id(headers.as_ref()),
                    })
                } else {
                    CodexErr::UnexpectedStatus(UnexpectedResponseError {
                        status,
                        body: body_text,
                        request_id: extract_request_id(headers.as_ref()),
                    })
                }
            }
            TransportError::RetryLimit => CodexErr::RetryLimit(RetryLimitReachedError {
                status: http::StatusCode::INTERNAL_SERVER_ERROR,
                request_id: None,
            }),
            TransportError::Timeout => CodexErr::Timeout,
            TransportError::Network(msg) | TransportError::Build(msg) => {
                CodexErr::Stream(msg, None)
            }
        },
        ApiError::RateLimit(msg) => CodexErr::Stream(msg, None),
    }
}

fn extract_request_id(headers: Option<&HeaderMap>) -> Option<String> {
    headers.and_then(|map| {
        ["cf-ray", "x-request-id", "x-oai-request-id"]
            .iter()
            .find_map(|name| {
                map.get(*name)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string)
            })
    })
}

pub(crate) async fn auth_provider_from_auth(
    auth: Option<CodexAuth>,
    provider: &ModelProviderInfo,
) -> crate::error::Result<CoreAuthProvider> {
    // 首先检查是否是 UserAccessToken 模式
    // 如果是，优先使用 user_access_token，不强制要求 env_key
    if let Some(ref auth_ref) = auth
        && auth_ref.mode == codex_app_server_protocol::AuthMode::UserAccessToken
    {
        let user_token = auth_ref.get_user_access_token();
        tracing::warn!("🔐 [auth_provider_from_auth] UserAccessToken 模式");
        tracing::warn!("   - user_token 存在: {}", user_token.is_some());
        if let Some(ref token) = user_token {
            tracing::warn!("   - user_token 长度: {}", token.len());
        }

        // UserAccessToken 模式下，尝试获取 API key 但不强制要求
        // 如果有 env_key 配置且环境变量存在，使用它作为 Bearer token
        // 否则使用 auth 中的 api_key（如果有的话）
        // 如果都没有，使用 user_access_token 作为 Bearer token（用于 IATerm 代理服务）
        let bearer_token = provider.api_key().ok().flatten()
            .or_else(|| provider.experimental_bearer_token.clone())
            .or_else(|| auth_ref.api_key.clone())
            .or_else(|| user_token.clone()); // 回退到 user_access_token

        tracing::warn!("   - bearer_token 存在: {}", bearer_token.is_some());

        return Ok(CoreAuthProvider {
            token: bearer_token,
            account_id: None,
            user_access_token: user_token,
        });
    }

    // 非 UserAccessToken 模式：原有逻辑
    if let Some(api_key) = provider.api_key()? {
        // 如果有 auth 且是 UserAccessToken 模式，同时传递 user_access_token
        let user_token = auth.as_ref().and_then(|a| {
            if a.mode == codex_app_server_protocol::AuthMode::UserAccessToken {
                a.get_user_access_token()
            } else {
                None
            }
        });
        return Ok(CoreAuthProvider {
            token: Some(api_key),
            account_id: None,
            user_access_token: user_token,
        });
    }

    if let Some(token) = provider.experimental_bearer_token.clone() {
        // 如果有 auth 且是 UserAccessToken 模式，同时传递 user_access_token
        let user_token = auth.as_ref().and_then(|a| {
            if a.mode == codex_app_server_protocol::AuthMode::UserAccessToken {
                a.get_user_access_token()
            } else {
                None
            }
        });
        return Ok(CoreAuthProvider {
            token: Some(token),
            account_id: None,
            user_access_token: user_token,
        });
    }

    if let Some(auth) = auth {
        match auth.mode {
            codex_app_server_protocol::AuthMode::UserAccessToken => {
                // UserAccessToken 模式：
                // - user_access_token 用于 X-User-Access-Token header（身份验证）
                // - api_key 如果存在则用于 Bearer token（代理服务认证）
                let user_token = auth.get_user_access_token();
                let bearer_token = auth.api_key.clone();
                Ok(CoreAuthProvider {
                    token: bearer_token,
                    account_id: None,
                    user_access_token: user_token,
                })
            }
            _ => {
                // ApiKey 或 ChatGPT 模式
                let token = auth.get_token().await?;
                Ok(CoreAuthProvider {
                    token: Some(token),
                    account_id: auth.get_account_id(),
                    user_access_token: None,
                })
            }
        }
    } else {
        Ok(CoreAuthProvider {
            token: None,
            account_id: None,
            user_access_token: None,
        })
    }
}

#[derive(Debug, Deserialize)]
struct UsageErrorResponse {
    error: UsageErrorBody,
}

#[derive(Debug, Deserialize)]
struct UsageErrorBody {
    #[serde(rename = "type")]
    error_type: Option<String>,
    plan_type: Option<PlanType>,
    resets_at: Option<i64>,
    /// 原始错误消息（来自 IAAccount 等服务）
    message: Option<String>,
    /// 错误来源（如 "iaaccount"）
    source: Option<String>,
}

#[derive(Clone, Default)]
pub(crate) struct CoreAuthProvider {
    token: Option<String>,
    account_id: Option<String>,
    /// 用户 Access Token（IAAccount OAuth JWT）
    /// 用于 X-User-Access-Token header，代理服务认证和用量追踪
    user_access_token: Option<String>,
}

impl ApiAuthProvider for CoreAuthProvider {
    fn bearer_token(&self) -> Option<String> {
        self.token.clone()
    }

    fn account_id(&self) -> Option<String> {
        self.account_id.clone()
    }

    fn user_access_token(&self) -> Option<String> {
        self.user_access_token.clone()
    }
}
