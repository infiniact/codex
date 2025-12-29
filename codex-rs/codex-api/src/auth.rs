use codex_client::Request;

/// Provides bearer and account identity information for API requests.
///
/// Implementations should be cheap and non-blocking; any asynchronous
/// refresh or I/O should be handled by higher layers before requests
/// reach this interface.
pub trait AuthProvider: Send + Sync {
    fn bearer_token(&self) -> Option<String>;
    fn account_id(&self) -> Option<String> {
        None
    }
    /// 返回用户的 access token（IAAccount OAuth JWT）
    /// 用于 X-User-Access-Token header，代理服务认证和用量追踪
    fn user_access_token(&self) -> Option<String> {
        None
    }
}

pub(crate) fn add_auth_headers<A: AuthProvider>(auth: &A, mut req: Request) -> Request {
    tracing::trace!("🔐 [add_auth_headers] 开始添加认证 headers");

    if let Some(token) = auth.bearer_token()
        && let Ok(header) = format!("Bearer {token}").parse()
    {
        tracing::trace!("   ✅ 添加 Authorization header (Bearer token 长度: {})", token.len());
        let _ = req.headers.insert(http::header::AUTHORIZATION, header);
    }

    if let Some(account_id) = auth.account_id()
        && let Ok(header) = account_id.parse()
    {
        let _ = req.headers.insert("ChatGPT-Account-ID", header);
    }

    // 添加用户 Access Token header（用于 IAAccount 代理服务）
    if let Some(user_token) = auth.user_access_token()
        && let Ok(header) = user_token.parse()
    {
        tracing::trace!("   ✅ 添加 X-User-Access-Token header (token 长度: {})", user_token.len());
        let _ = req.headers.insert("X-User-Access-Token", header);
    }

    req
}
