//! Unified Exec: interactive PTY execution orchestrated with approvals + sandboxing.
//!
//! Responsibilities
//! - Manages interactive PTY sessions (create, reuse, buffer output with caps).
//! - Uses the shared ToolOrchestrator to handle approval, sandbox selection, and
//!   retry semantics in a single, descriptive flow.
//! - Spawns the PTY from a sandbox‑transformed `ExecEnv`; on sandbox denial,
//!   retries without sandbox when policy allows (no re‑prompt thanks to caching).
//! - Uses the shared `is_likely_sandbox_denied` heuristic to keep denial messages
//!   consistent with other exec paths.
//! - Supports PtyService backend for faster execution without sandboxing
//!
//! ## External PtyService Integration
//!
//! The unified exec system supports external PtyService backends through the
//! `PtyServiceBridge` trait. This allows for faster command execution without
//! sandboxing overhead, particularly useful for development environments.
//!
//! ### Usage Example
//!
//! ```rust,no_run
//! use std::sync::Arc;
//! use codex_rs::unified_exec::PtyServiceBridge;
//! use codex_rs::conversation_manager::ConversationManager;
//! use codex_rs::AuthManager;
//! use codex_protocol::protocol::SessionSource;
//!
//! // Implement your PtyService bridge
//! struct MyPtyServiceBridge {
//!     // Your implementation details
//! }
//!
//! #[async_trait::async_trait]
//! impl PtyServiceBridge for MyPtyServiceBridge {
//!     async fn execute(
//!         &self,
//!         command: &str,
//!         shell: &str,
//!         login: bool,
//!         display_in_panel: bool,
//!     ) -> Result<PtyServiceResult, String> {
//!         // Your implementation
//!         todo!()
//!     }
//!
//!     async fn write_stdin(&self, session_id: &str, input: &[u8]) -> Result<(), String> {
//!         // Your implementation
//!         todo!()
//!     }
//!
//!     fn is_available(&self) -> bool {
//!         // Your implementation
//!         true
//!     }
//! }
//!
//! // Create conversation manager with PtyService bridge
//! let auth_manager = Arc::new(AuthManager::new());
//! let pty_bridge = Arc::new(MyPtyServiceBridge {});
//! let conversation_manager = ConversationManager::new_with_pty_bridge(
//!     auth_manager,
//!     SessionSource::Cli,
//!     pty_bridge,
//! );
//! ```
//!
//! ### Backend Selection
//!
//! The system automatically selects the appropriate backend based on:
//! - Configuration settings (`UnifiedExecConfig::default_backend`)
//! - Command characteristics (for `ExecutionBackend::Auto`)
//! - PtyService availability
//!
//! When a PtyService bridge is configured, commands can be executed through
//! the external service for improved performance, bypassing the default
//! portable-pty implementation.
//!
//! Flow at a glance (open session)
//! 1) Build a small request `{ command, cwd }`.
//! 2) Select execution backend (Default or PtyService based on config)
//! 3) For Default backend:
//!    - Orchestrator: approval (bypass/cache/prompt) → select sandbox → run.
//!    - Runtime: transform `CommandSpec` → `ExecEnv` → spawn PTY.
//!    - If denial, orchestrator retries with `SandboxType::None`.
//! 4) For PtyService backend:
//!    - Execute directly through PtyService bridge (no sandbox)
//! 5) Session is returned with streaming output + metadata.
//!
//! This keeps policy logic and user interaction centralized while the PTY/session
//! concerns remain isolated here. The implementation is split between:
//! - `session.rs`: PTY session lifecycle + output buffering.
//! - `session_manager.rs`: orchestration (approvals, sandboxing, reuse) and request handling.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicI32;
use std::sync::OnceLock;
use std::time::Duration;

use rand::Rng;
use rand::rng;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio::sync::RwLock;

use crate::codex::Session;
use crate::codex::TurnContext;

/// 全局的会话到连接映射
/// 用于在命令执行时确定使用哪个连接
static GLOBAL_CONNECTION_MAP: OnceLock<Arc<RwLock<HashMap<String, String>>>> = OnceLock::new();

fn get_connection_map() -> &'static Arc<RwLock<HashMap<String, String>>> {
    GLOBAL_CONNECTION_MAP.get_or_init(|| Arc::new(RwLock::new(HashMap::new())))
}

/// 设置全局会话连接映射
pub async fn set_global_conversation_connection(conversation_id: &str, connection_id: String) {
    tracing::info!("Setting global conversation connection: {conversation_id} -> {connection_id}");
    let mut map = get_connection_map().write().await;
    map.insert(conversation_id.to_string(), connection_id);
    tracing::info!("✅ [GlobalMap] 映射已设置，当前总数: {}", map.len());
}

/// 获取全局会话连接映射
pub async fn get_global_conversation_connection(conversation_id: &str) -> Option<String> {
    let map = get_connection_map().read().await;
    let result = map.get(conversation_id).cloned();
    tracing::info!("🔍 [GlobalMap] 查询映射: {conversation_id} -> {result:?}, 映射表大小: {}", map.len());
    result
}

mod errors;
mod session;
mod session_manager;

pub(crate) use errors::UnifiedExecError;
pub(crate) use session::UnifiedExecSession;

pub(crate) const DEFAULT_YIELD_TIME_MS: u64 = 10_000;
pub(crate) const MIN_YIELD_TIME_MS: u64 = 250;
pub(crate) const MAX_YIELD_TIME_MS: u64 = 30_000;
pub(crate) const DEFAULT_MAX_OUTPUT_TOKENS: usize = 10_000;
pub(crate) const UNIFIED_EXEC_OUTPUT_MAX_BYTES: usize = 1024 * 1024; // 1 MiB

// ============================================================================
// 执行后端配置
// ============================================================================

/// 执行后端类型
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub enum ExecutionBackend {
    /// 默认后端：使用 portable-pty，带沙箱支持
    #[default]
    #[serde(rename = "default")]
    Default,

    /// PtyService 后端：使用外部 PtyService，更快但无沙箱
    #[serde(rename = "pty_service")]
    PtyService,

    /// 自动选择：根据命令特征自动决定
    #[serde(rename = "auto")]
    Auto,
}



/// PtyService 桥接接口
/// 
/// 此 trait 定义了与外部 PtyService 集成的接口。实现此 trait 可以让
/// codex-rs 使用外部的 PTY 服务来执行命令，而不是使用内置的 portable-pty。
/// 
/// ## 实现要求
/// 
/// - 所有方法都必须是线程安全的 (`Send + Sync`)
/// - `execute` 方法应该启动一个新的 PTY 会话并返回初始输出
/// - `write_stdin` 方法应该向指定会话写入数据
/// - `is_available` 方法应该快速检查服务是否可用
/// 
/// ## 错误处理
/// 
/// 方法返回 `Result<T, String>` 以便于错误传播。错误信息应该
/// 对用户友好，因为它们可能会显示在 UI 中。
#[async_trait::async_trait]
#[allow(dead_code)]
pub trait PtyServiceBridge: Send + Sync {
    /// 执行命令
    ///
    /// # 参数
    ///
    /// - `command`: 要执行的命令字符串
    /// - `shell`: 使用的 shell (如 "bash", "zsh")
    /// - `login`: 是否作为登录 shell 启动
    /// - `display_in_panel`: 是否在面板中显示输出
    /// - `connection_id`: 可选的连接 ID，如果提供则在该连接中执行，否则创建新连接
    /// - `stdin`: 可选的标准输入内容，如果提供则在命令执行后立即写入
    ///
    /// # 返回值
    ///
    /// 返回 `PtyServiceResult` 包含会话 ID、初始输出和退出码
    async fn execute(
        &self,
        command: &str,
        shell: &str,
        login: bool,
        display_in_panel: bool,
        connection_id: Option<&str>,
        stdin: Option<&str>,
    ) -> Result<PtyServiceResult, String>;

    /// 写入标准输入
    /// 
    /// # 参数
    /// 
    /// - `session_id`: 目标会话的 ID
    /// - `input`: 要写入的数据
    /// 
    /// # 错误
    /// 
    /// 如果会话不存在或写入失败，返回错误
    async fn write_stdin(&self, session_id: &str, input: &[u8]) -> Result<(), String>;

    /// 检查是否可用
    /// 
    /// 此方法应该快速返回，用于检查 PtyService 是否可用。
    /// 如果返回 `false`，系统将回退到默认的 portable-pty 后端。
    fn is_available(&self) -> bool;
}

/// PtyService 执行结果
///
/// 包含 PtyService 执行命令后返回的结果信息
#[derive(Debug)]
#[allow(dead_code)]
pub struct PtyServiceResult {
    /// 会话 ID，用于后续的 stdin 写入操作
    pub session_id: String,
    /// 命令的初始输出
    pub output: String,
    /// 退出码（如果命令已完成）
    pub exit_code: Option<i32>,
    /// 面板 ID（如果在面板中显示）
    pub panel_id: Option<String>,
    /// 实际使用的连接 ID（可能与传入的不同）
    pub connection_id: String,
}

/// 执行配置
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct UnifiedExecConfig {
    /// 默认使用的后端
    pub default_backend: ExecutionBackend,

    /// 是否强制使用指定后端
    pub force_backend: bool,

    /// PtyService 模式下是否跳过沙箱
    pub skip_sandbox_for_pty: bool,
}

impl Default for UnifiedExecConfig {
    fn default() -> Self {
        Self {
            default_backend: ExecutionBackend::PtyService,  // 默认使用 PtyService
            force_backend: false,
            skip_sandbox_for_pty: true,
        }
    }
}

pub(crate) struct UnifiedExecContext {
    pub session: Arc<Session>,
    pub turn: Arc<TurnContext>,
    pub call_id: String,
    /// 可选的连接 ID，用于在特定连接中执行命令
    pub connection_id: Option<String>,
    /// 会话 ID，用于查询 connection_map
    #[allow(dead_code)]
    pub conversation_id: String,
}

impl UnifiedExecContext {
    /// 创建执行上下文，可选地指定连接 ID 以复用现有连接
    pub fn with_connection_id(session: Arc<Session>, turn: Arc<TurnContext>, call_id: String, conversation_id: String, connection_id: Option<String>) -> Self {
        Self {
            session,
            turn,
            call_id,
            connection_id,
            conversation_id,
        }
    }
}

#[derive(Debug)]
pub(crate) struct ExecCommandRequest<'a> {
    pub command: &'a str,
    pub shell: &'a str,
    pub login: bool,
    pub yield_time_ms: Option<u64>,
    pub max_output_tokens: Option<usize>,
    /// 指定执行后端
    pub backend: Option<ExecutionBackend>,
    /// 是否在面板显示（仅 PtyService）
    pub display_in_panel: bool,
    /// 标准输入内容（如果有）
    pub stdin: Option<&'a str>,
}

#[derive(Debug)]
pub(crate) struct WriteStdinRequest<'a> {
    pub session_id: i32,
    pub input: &'a str,
    pub yield_time_ms: Option<u64>,
    pub max_output_tokens: Option<usize>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct UnifiedExecResponse {
    pub event_call_id: String,
    pub chunk_id: String,
    pub wall_time: Duration,
    pub output: String,
    pub session_id: Option<i32>,
    pub exit_code: Option<i32>,
    pub original_token_count: Option<usize>,
}

#[derive(Default)]
pub(crate) struct UnifiedExecSessionManager {
    next_session_id: AtomicI32,
    sessions: Mutex<HashMap<i32, SessionEntry>>,
    /// 执行配置
    config: Arc<RwLock<UnifiedExecConfig>>,
    /// PtyService 桥接器（如果可用）
    pty_bridge: Arc<RwLock<Option<Arc<dyn PtyServiceBridge>>>>,
}

impl UnifiedExecSessionManager {
    /// 设置统一执行配置
    /// 
    /// 允许在运行时更改执行后端和其他配置选项
    #[allow(dead_code)]
    pub fn set_config(&self, config: UnifiedExecConfig) {
        let mut cfg = self.config.blocking_write();
        *cfg = config;
    }

    /// 获取当前的统一执行配置
    #[allow(dead_code)]
    pub async fn get_config(&self) -> UnifiedExecConfig {
        self.config.read().await.clone()
    }

    /// 设置 PtyService 桥接器
    /// 
    /// 设置外部 PtyService 桥接器，用于执行命令。设置后，
    /// 当选择 PtyService 后端时，将使用此桥接器执行命令。
    /// 
    /// # 参数
    /// 
    /// - `bridge`: 实现了 `PtyServiceBridge` trait 的桥接器实例
    /// 
    /// # 示例
    ///
    /// ```rust,no_run
    /// use std::sync::Arc;
    /// use codex_rs::unified_exec::{UnifiedExecSessionManager, PtyServiceBridge};
    ///
    /// # async fn example() {
    /// let manager = UnifiedExecSessionManager::default();
    /// let bridge = Arc::new(MyPtyServiceBridge::new());
    /// manager.set_pty_bridge(bridge).await;
    /// # }
    /// ```
    pub async fn set_pty_bridge(&self, bridge: Arc<dyn PtyServiceBridge>) {
        let mut pb = self.pty_bridge.write().await;
        *pb = Some(bridge);
    }

    /// 获取当前的 PtyService 桥接器
    ///
    /// 返回当前设置的 PtyService 桥接器，如果没有设置则返回 `None`
    ///
    /// # 返回值
    ///
    /// - `Some(bridge)`: 如果已设置桥接器
    /// - `None`: 如果未设置桥接器
    #[allow(dead_code)]
    pub async fn get_pty_bridge(&self) -> Option<Arc<dyn PtyServiceBridge>> {
        self.pty_bridge.read().await.clone()
    }
}

struct SessionEntry {
    session: session::UnifiedExecSession,
    session_ref: Arc<Session>,
    turn_ref: Arc<TurnContext>,
    call_id: String,
    command: String,
    cwd: PathBuf,
    started_at: tokio::time::Instant,
}

pub(crate) fn clamp_yield_time(yield_time_ms: Option<u64>) -> u64 {
    match yield_time_ms {
        Some(value) => value.clamp(MIN_YIELD_TIME_MS, MAX_YIELD_TIME_MS),
        None => DEFAULT_YIELD_TIME_MS,
    }
}

pub(crate) fn resolve_max_tokens(max_tokens: Option<usize>) -> usize {
    max_tokens.unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS)
}

pub(crate) fn generate_chunk_id() -> String {
    let mut rng = rng();
    (0..6)
        .map(|_| format!("{:x}", rng.random_range(0..16)))
        .collect()
}

pub(crate) fn truncate_output_to_tokens(
    output: &str,
    max_tokens: usize,
) -> (String, Option<usize>) {
    if max_tokens == 0 {
        let total_tokens = output.chars().count();
        let message = format!("…{total_tokens} tokens truncated…");
        return (message, Some(total_tokens));
    }

    let tokens: Vec<char> = output.chars().collect();
    let total_tokens = tokens.len();
    if total_tokens <= max_tokens {
        return (output.to_string(), None);
    }

    let half = max_tokens / 2;
    if half == 0 {
        let truncated = total_tokens.saturating_sub(max_tokens);
        let message = format!("…{truncated} tokens truncated…");
        return (message, Some(total_tokens));
    }

    let truncated = total_tokens.saturating_sub(half * 2);
    let mut truncated_output = String::new();
    truncated_output.extend(&tokens[..half]);
    truncated_output.push_str(&format!("…{truncated} tokens truncated…"));
    truncated_output.extend(&tokens[total_tokens - half..]);
    (truncated_output, Some(total_tokens))
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;
    use crate::codex::Session;
    use crate::codex::TurnContext;
    use crate::codex::make_session_and_context;
    use crate::protocol::AskForApproval;
    use crate::protocol::SandboxPolicy;
    use crate::unified_exec::ExecCommandRequest;
    use crate::unified_exec::WriteStdinRequest;
    use core_test_support::skip_if_sandbox;
    use std::sync::Arc;
    use tokio::time::Duration;

    use super::session::OutputBufferState;

    fn test_session_and_turn() -> (Arc<Session>, Arc<TurnContext>) {
        let (session, mut turn) = make_session_and_context();
        turn.approval_policy = AskForApproval::Never;
        turn.sandbox_policy = SandboxPolicy::DangerFullAccess;
        (Arc::new(session), Arc::new(turn))
    }

    async fn exec_command(
        session: &Arc<Session>,
        turn: &Arc<TurnContext>,
        cmd: &str,
        yield_time_ms: Option<u64>,
    ) -> Result<UnifiedExecResponse, UnifiedExecError> {
        let context =
            UnifiedExecContext::with_connection_id(Arc::clone(session), Arc::clone(turn), "call".to_string(), session.conversation_id().to_string(), None);

        session
            .services
            .unified_exec_manager
            .exec_command(
                ExecCommandRequest {
                    command: cmd,
                    shell: "/bin/bash",
                    login: true,
                    yield_time_ms,
                    max_output_tokens: None,
                    backend: Some(ExecutionBackend::Default),  // 测试时使用默认后端
                    display_in_panel: false,  // 测试时不显示面板
                    stdin: None,
                },
                &context,
            )
            .await
    }

    async fn write_stdin(
        session: &Arc<Session>,
        session_id: i32,
        input: &str,
        yield_time_ms: Option<u64>,
    ) -> Result<UnifiedExecResponse, UnifiedExecError> {
        session
            .services
            .unified_exec_manager
            .write_stdin(WriteStdinRequest {
                session_id,
                input,
                yield_time_ms,
                max_output_tokens: None,
            })
            .await
    }

    #[test]
    fn push_chunk_trims_only_excess_bytes() {
        let mut buffer = OutputBufferState::default();
        buffer.push_chunk(vec![b'a'; UNIFIED_EXEC_OUTPUT_MAX_BYTES]);
        buffer.push_chunk(vec![b'b']);
        buffer.push_chunk(vec![b'c']);

        assert_eq!(buffer.total_bytes, UNIFIED_EXEC_OUTPUT_MAX_BYTES);
        let snapshot = buffer.snapshot();
        assert_eq!(snapshot.len(), 3);
        assert_eq!(
            snapshot.first().unwrap().len(),
            UNIFIED_EXEC_OUTPUT_MAX_BYTES - 2
        );
        assert_eq!(snapshot.get(2).unwrap(), &vec![b'c']);
        assert_eq!(snapshot.get(1).unwrap(), &vec![b'b']);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unified_exec_persists_across_requests() -> anyhow::Result<()> {
        skip_if_sandbox!(Ok(()));

        let (session, turn) = test_session_and_turn();

        let open_shell = exec_command(&session, &turn, "bash -i", Some(2_500)).await?;
        let session_id = open_shell.session_id.expect("expected session_id");

        write_stdin(
            &session,
            session_id,
            "export CODEX_INTERACTIVE_SHELL_VAR=codex\n",
            Some(2_500),
        )
        .await?;

        let out_2 = write_stdin(
            &session,
            session_id,
            "echo $CODEX_INTERACTIVE_SHELL_VAR\n",
            Some(2_500),
        )
        .await?;
        assert!(
            out_2.output.contains("codex"),
            "expected environment variable output"
        );

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn multi_unified_exec_sessions() -> anyhow::Result<()> {
        skip_if_sandbox!(Ok(()));

        let (session, turn) = test_session_and_turn();

        let shell_a = exec_command(&session, &turn, "bash -i", Some(2_500)).await?;
        let session_a = shell_a.session_id.expect("expected session id");

        write_stdin(
            &session,
            session_a,
            "export CODEX_INTERACTIVE_SHELL_VAR=codex\n",
            Some(2_500),
        )
        .await?;

        let out_2 = exec_command(
            &session,
            &turn,
            "echo $CODEX_INTERACTIVE_SHELL_VAR",
            Some(2_500),
        )
        .await?;
        assert!(
            out_2.session_id.is_none(),
            "short command should not retain a session"
        );
        assert!(
            !out_2.output.contains("codex"),
            "short command should run in a fresh shell"
        );

        let out_3 = write_stdin(
            &session,
            session_a,
            "echo $CODEX_INTERACTIVE_SHELL_VAR\n",
            Some(2_500),
        )
        .await?;
        assert!(
            out_3.output.contains("codex"),
            "session should preserve state"
        );

        Ok(())
    }

    #[tokio::test]
    async fn unified_exec_timeouts() -> anyhow::Result<()> {
        skip_if_sandbox!(Ok(()));

        let (session, turn) = test_session_and_turn();

        let open_shell = exec_command(&session, &turn, "bash -i", Some(2_500)).await?;
        let session_id = open_shell.session_id.expect("expected session id");

        write_stdin(
            &session,
            session_id,
            "export CODEX_INTERACTIVE_SHELL_VAR=codex\n",
            Some(2_500),
        )
        .await?;

        let out_2 = write_stdin(
            &session,
            session_id,
            "sleep 5 && echo $CODEX_INTERACTIVE_SHELL_VAR\n",
            Some(10),
        )
        .await?;
        assert!(
            !out_2.output.contains("codex"),
            "timeout too short should yield incomplete output"
        );

        tokio::time::sleep(Duration::from_secs(7)).await;

        let out_3 = write_stdin(&session, session_id, "", Some(100)).await?;

        assert!(
            out_3.output.contains("codex"),
            "subsequent poll should retrieve output"
        );

        Ok(())
    }

    #[tokio::test]
    #[ignore] // Ignored while we have a better way to test this.
    async fn requests_with_large_timeout_are_capped() -> anyhow::Result<()> {
        let (session, turn) = test_session_and_turn();

        let result = exec_command(&session, &turn, "echo codex", Some(120_000)).await?;

        assert!(result.session_id.is_none());
        assert!(result.output.contains("codex"));

        Ok(())
    }

    #[tokio::test]
    #[ignore] // Ignored while we have a better way to test this.
    async fn completed_commands_do_not_persist_sessions() -> anyhow::Result<()> {
        let (session, turn) = test_session_and_turn();
        let result = exec_command(&session, &turn, "echo codex", Some(2_500)).await?;

        assert!(
            result.session_id.is_none(),
            "completed command should not retain session"
        );
        assert!(result.output.contains("codex"));

        assert!(
            session
                .services
                .unified_exec_manager
                .sessions
                .lock()
                .await
                .is_empty()
        );

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reusing_completed_session_returns_unknown_session() -> anyhow::Result<()> {
        skip_if_sandbox!(Ok(()));

        let (session, turn) = test_session_and_turn();

        let open_shell = exec_command(&session, &turn, "bash -i", Some(2_500)).await?;
        let session_id = open_shell.session_id.expect("expected session id");

        write_stdin(&session, session_id, "exit\n", Some(2_500)).await?;

        tokio::time::sleep(Duration::from_millis(200)).await;

        let err = write_stdin(&session, session_id, "", Some(100))
            .await
            .expect_err("expected unknown session error");

        match err {
            UnifiedExecError::UnknownSessionId { session_id: err_id } => {
                assert_eq!(err_id, session_id);
            }
            other => panic!("expected UnknownSessionId, got {other:?}"),
        }

        assert!(
            !session
                .services
                .unified_exec_manager
                .sessions
                .lock()
                .await
                .contains_key(&session_id)
        );

        Ok(())
    }
}
