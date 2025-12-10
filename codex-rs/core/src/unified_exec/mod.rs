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
//!
//! Flow at a glance (open session)
//! 1) Build a small request `{ command, cwd }`.
//! 2) Orchestrator: approval (bypass/cache/prompt) → select sandbox → run.
//! 3) Runtime: transform `CommandSpec` → `ExecEnv` → spawn PTY.
//! 4) If denial, orchestrator retries with `SandboxType::None`.
//! 5) Session is returned with streaming output + metadata.
//!
//! This keeps policy logic and user interaction centralized while the PTY/session
//! concerns remain isolated here. The implementation is split between:
//! - `session.rs`: PTY session lifecycle + output buffering.
//! - `session_manager.rs`: orchestration (approvals, sandboxing, reuse) and request handling.

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use rand::Rng;
use rand::rng;
use tokio::sync::Mutex;

use crate::codex::Session;
use crate::codex::TurnContext;

mod errors;
mod session;
mod session_manager;

pub(crate) use errors::UnifiedExecError;
pub(crate) use session::UnifiedExecSession;

/// 🔧 智能格式化命令数组为可执行的命令字符串
///
/// 对于 `bash -c` / `bash -lc` / `sh -c` 等模式，需要将脚本参数用引号包裹
/// 例如：["bash", "-lc", "cat", "test.sh"] -> "bash -lc 'cat test.sh'"
///
/// ⚠️ 特殊处理 heredoc：包含 heredoc 语法的脚本不能被引用，
/// 因为 heredoc 依赖于命令行中的换行符和定界符结构
///
/// ⚠️ Shell 操作符（>, <, |, &&, ||, ;）不应被引用
pub(crate) fn format_command_for_execution(command: &[String]) -> String {
    if command.is_empty() {
        return String::new();
    }

    // 检测 shell 类型（支持绝对路径如 /bin/bash, /usr/bin/zsh 等）
    let shell_name = command[0].rsplit('/').next().unwrap_or(&command[0]);
    let is_shell = matches!(shell_name, "bash" | "sh" | "zsh");
    let is_shell_flag = command
        .get(1)
        .is_some_and(|flag| matches!(flag.as_str(), "-c" | "-lc" | "-ic"));

    // 检查是否是 bash -c / bash -lc / sh -c 等模式
    if command.len() >= 3 && is_shell && is_shell_flag {
        // 获取脚本内容
        let script = if command.len() == 3 {
            command[2].clone()
        } else {
            // 命令格式被错误拆分：["bash", "-lc", "cat", "test.sh"]
            // 将第三个及之后的参数合并为一个脚本字符串
            command[2..].join(" ")
        };

        // 🔧 检测 heredoc：如果脚本包含 heredoc 语法，不要对其进行引用
        // heredoc 语法依赖于命令行中的换行符和定界符，引用会破坏这种结构
        // 检测方式：查找 "<<" 后跟空白或定界符的模式
        if contains_heredoc(&script) {
            // heredoc 命令直接返回，不进行引用
            // 这允许 PTY 桥接器的 heredoc 解析器正确处理它
            return script;
        }

        // 非 heredoc 脚本，正常引用
        return format!("{} {} {}", command[0], command[1], shell_quote(&script));
    }

    // 对于普通命令，正确引用包含空格或特殊字符的参数
    // ⚠️ 但 shell 操作符不应被引用
    // 🔧 智能检测：只有在 shell 操作符后面的参数才检测是否是简单命令
    let mut result = Vec::new();
    let mut after_operator = true; // 开始时，第一个参数是命令

    for arg in command.iter() {
        if is_shell_operator(arg) {
            // Shell 操作符不引用
            result.push(arg.clone());
            after_operator = true; // 下一个参数是命令
        } else if after_operator && looks_like_simple_command(arg) {
            // 在操作符后面，检测是否是简单命令片段
            result.push(arg.clone());
            after_operator = false;
        } else {
            // 其他情况，使用标准引用
            result.push(shell_quote(arg));
            after_operator = false;
        }
    }

    result.join(" ")
}

/// 检测脚本是否包含 heredoc 语法
///
/// heredoc 语法形式：
/// - `<< EOF`
/// - `<< 'EOF'`
/// - `<<EOF`
/// - `<<-EOF` (允许缩进)
fn contains_heredoc(script: &str) -> bool {
    // 查找 "<<" 模式
    if let Some(pos) = script.find("<<") {
        // 检查 << 后面是否跟着定界符（允许可选的 - 和空白）
        let after = &script[pos + 2..];
        let after_trimmed = after.trim_start_matches('-').trim_start();

        // 定界符应该是标识符或引号包裹的标识符
        // 例如：EOF, 'EOF', "EOF", SCRIPT_END
        if !after_trimmed.is_empty()
            && let Some(first_char) = after_trimmed.chars().next()
        {
            // 定界符可以是：字母、引号
            if first_char.is_alphabetic() || first_char == '\'' || first_char == '"' || first_char == '_' {
                return true;
            }
        }
    }
    false
}

/// 检查是否是 shell 操作符
/// 这些操作符不应该被引用
fn is_shell_operator(s: &str) -> bool {
    matches!(s,
        ">" | ">>" | "<" | "<<" | "<<<" |  // 重定向
        "|" | "||" | "&&" |                 // 管道和逻辑
        ";" | "&" |                         // 命令分隔
        "2>" | "2>>" | "&>" | "&>>" |       // 标准错误重定向
        "2>&1" | "1>&2" |                   // 文件描述符重定向
        "|&"                                // 管道
    )
}

/// 检测字符串是否看起来像一个简单的命令片段
/// 例如 "ls -la" 或 "grep -r pattern" 这样的命令，应该直接使用而不加引号
///
/// 🔧 修复：AI 可能错误地将命令参数合并成一个字符串
/// 例如发送 ["pwd", "&&", "ls -la"] 而不是 ["pwd", "&&", "ls", "-la"]
/// 这种情况下，"ls -la" 应该直接传递给 shell，而不是用引号包裹
///
/// ⚠️ 注意：这个函数应该只用于检测 shell 操作符后面的参数
/// 对于普通命令的参数（如 "echo hello world" 中的 "hello world"），应该使用引号
fn looks_like_simple_command(s: &str) -> bool {
    // 如果是空字符串或太长，不是简单命令
    if s.is_empty() || s.len() > 200 {
        return false;
    }

    // 如果包含 shell 危险字符（可能需要转义），不是简单命令
    // 危险字符：$, `, \, !, ", ', ;, |, &, <, >, (, ), {, }, [, ], *
    let dangerous_chars = ['$', '`', '\\', '!', '"', '\'', ';', '|', '&', '<', '>', '(', ')', '{', '}', '[', ']', '*', '?', '~'];
    if s.chars().any(|c| dangerous_chars.contains(&c)) {
        return false;
    }

    // 检查是否看起来像 "command arg1 arg2" 的格式
    // 第一部分应该是一个有效的命令名（字母数字、下划线、连字符）
    // 后续部分应该是简单的参数（以 - 或 -- 开头，或者是简单的值）
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.is_empty() {
        return false;
    }

    // 🔧 关键检查：第一部分必须看起来像一个命令名，而不是普通文本
    // 命令名通常是短的（少于 20 个字符）、以字母开头、不包含大写字母（除非是路径）
    // 另外，常见的命令参数值不应该被误识别为命令
    let cmd = parts[0];

    // 如果只有一个单词且不包含 - 或 /，可能是普通参数值而不是命令
    if parts.len() == 1 && !cmd.contains('-') && !cmd.contains('/') {
        return false;
    }

    // 命令名验证
    if !cmd.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.' || c == '/') {
        return false;
    }

    // 如果第一部分不是路径且长度大于 20，可能是普通文本
    if cmd.len() > 20 && !cmd.contains('/') {
        return false;
    }

    // 🔧 额外检查：如果只有一个参数且不以 - 开头，这可能是 "command value" 格式
    // 这种情况应该让调用者决定是否引用
    // 但 "ls -la" 或 "grep pattern" 这种格式应该返回 true
    if parts.len() == 2 && !parts[1].starts_with('-') {
        // 如果第二部分是短单词（可能是搜索模式或参数），仍然识别为简单命令
        // 但如果是长文本或包含多个单词，则不识别
        if parts[1].len() > 30 {
            return false;
        }
    }

    // 后续部分应该是简单的参数
    for arg in &parts[1..] {
        // 参数可以是:
        // 1. 选项: -x, --xxx, -xxx
        // 2. 简单值: 字母数字、下划线、连字符、点、斜杠、等号、冒号
        let is_valid_arg = arg.chars().all(|c| {
            c.is_alphanumeric() || c == '_' || c == '-' || c == '.' || c == '/' || c == '=' || c == ':' || c == ','
        });
        if !is_valid_arg {
            return false;
        }
    }

    true
}

/// 为 shell 参数添加适当的引号
fn shell_quote(s: &str) -> String {
    // 如果字符串不包含特殊字符，直接返回
    if s.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.' || c == '/') {
        return s.to_string();
    }

    let has_single_quote = s.contains('\'');
    let has_double_quote = s.contains('"');

    if !has_single_quote {
        // 不包含单引号，使用单引号包裹（最简单）
        format!("'{s}'")
    } else if !has_double_quote {
        // 包含单引号但不包含双引号，使用双引号包裹
        // 需要转义 $, `, \, !
        let escaped = s
            .replace('\\', "\\\\")
            .replace('$', "\\$")
            .replace('`', "\\`")
            .replace('!', "\\!");
        format!("\"{escaped}\"")
    } else {
        // 同时包含单引号和双引号，使用单引号并转义内部的单引号
        // 'don'\''t' -> don't
        let escaped = s.replace('\'', "'\\''");
        format!("'{escaped}'")
    }
}

pub(crate) const MIN_YIELD_TIME_MS: u64 = 250;
pub(crate) const MAX_YIELD_TIME_MS: u64 = 30_000;
pub(crate) const DEFAULT_MAX_OUTPUT_TOKENS: usize = 10_000;
pub(crate) const UNIFIED_EXEC_OUTPUT_MAX_BYTES: usize = 1024 * 1024; // 1 MiB
pub(crate) const UNIFIED_EXEC_OUTPUT_MAX_TOKENS: usize = UNIFIED_EXEC_OUTPUT_MAX_BYTES / 4;
pub(crate) const MAX_UNIFIED_EXEC_SESSIONS: usize = 64;

// Send a warning message to the models when it reaches this number of sessions.
pub(crate) const WARNING_UNIFIED_EXEC_SESSIONS: usize = 60;

pub(crate) struct UnifiedExecContext {
    pub session: Arc<Session>,
    pub turn: Arc<TurnContext>,
    pub call_id: String,
}

impl UnifiedExecContext {
    pub fn new(session: Arc<Session>, turn: Arc<TurnContext>, call_id: String) -> Self {
        Self {
            session,
            turn,
            call_id,
        }
    }
}

#[derive(Debug)]
pub(crate) struct ExecCommandRequest {
    pub command: Vec<String>,
    pub process_id: String,
    pub yield_time_ms: u64,
    pub max_output_tokens: Option<usize>,
    pub workdir: Option<PathBuf>,
    pub with_escalated_permissions: Option<bool>,
    pub justification: Option<String>,
}

#[derive(Debug)]
pub(crate) struct WriteStdinRequest<'a> {
    pub call_id: &'a str,
    pub process_id: &'a str,
    pub input: &'a str,
    pub yield_time_ms: u64,
    pub max_output_tokens: Option<usize>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct UnifiedExecResponse {
    pub event_call_id: String,
    pub chunk_id: String,
    pub wall_time: Duration,
    pub output: String,
    pub process_id: Option<String>,
    pub exit_code: Option<i32>,
    pub original_token_count: Option<usize>,
    pub session_command: Option<Vec<String>>,
}

#[derive(Default)]
pub(crate) struct UnifiedExecSessionManager {
    session_store: Mutex<SessionStore>,
}

// Required for mutex sharing.
#[derive(Default)]
pub(crate) struct SessionStore {
    sessions: HashMap<String, SessionEntry>,
    reserved_sessions_id: HashSet<String>,
}

impl SessionStore {
    fn remove(&mut self, session_id: &str) -> Option<SessionEntry> {
        self.reserved_sessions_id.remove(session_id);
        self.sessions.remove(session_id)
    }

    pub(crate) fn clear(&mut self) {
        self.reserved_sessions_id.clear();
        self.sessions.clear();
    }
}

struct SessionEntry {
    session: UnifiedExecSession,
    session_ref: Arc<Session>,
    turn_ref: Arc<TurnContext>,
    call_id: String,
    process_id: String,
    command: Vec<String>,
    cwd: PathBuf,
    started_at: tokio::time::Instant,
    last_used: tokio::time::Instant,
}

pub(crate) fn clamp_yield_time(yield_time_ms: u64) -> u64 {
    yield_time_ms.clamp(MIN_YIELD_TIME_MS, MAX_YIELD_TIME_MS)
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

// === iaterm compatibility shims ===

#[derive(Debug, Clone)]
pub struct PtyServiceResult {
    pub session_id: String,
    pub output: String,
    pub exit_code: Option<i32>,
    pub panel_id: Option<String>,
    pub connection_id: String,
}

#[async_trait::async_trait]
pub trait PtyServiceBridge: Send + Sync {
    async fn execute(
        &self,
        command: &str,
        shell: &str,
        login: bool,
        display_in_panel: bool,
        connection_id: Option<&str>,
        stdin: Option<&str>,
    ) -> Result<PtyServiceResult, String>;

    async fn write_stdin(&self, session_id: &str, input: &[u8]) -> Result<(), String>;

    fn is_available(&self) -> bool;
}

static GLOBAL_CONVERSATION_CONNECTIONS: tokio::sync::OnceCell<Mutex<HashMap<String, String>>> =
    tokio::sync::OnceCell::const_new();

async fn connections() -> &'static Mutex<HashMap<String, String>> {
    GLOBAL_CONVERSATION_CONNECTIONS
        .get_or_init(|| async { Mutex::new(HashMap::new()) })
        .await
}

pub async fn set_global_conversation_connection(conversation_id: &str, connection_id: String) {
    let map = connections().await;
    let mut guard = map.lock().await;
    guard.insert(conversation_id.to_string(), connection_id);
}

pub async fn get_global_conversation_connection(conversation_id: &str) -> Option<String> {
    let map = connections().await;
    let guard = map.lock().await;
    guard.get(conversation_id).cloned()
}

// === 同步单元测试 - 用于测试 format_command_for_execution 和 contains_heredoc ===
#[cfg(test)]
mod format_tests {
    use super::*;

    #[test]
    fn test_contains_heredoc_basic() {
        // 基本 heredoc 格式
        assert!(contains_heredoc("cat << EOF\ncontent\nEOF"));
        assert!(contains_heredoc("cat << 'EOF'\ncontent\nEOF"));
        assert!(contains_heredoc("cat <<EOF\ncontent\nEOF"));
        assert!(contains_heredoc("cat <<-EOF\ncontent\nEOF"));
    }

    #[test]
    fn test_contains_heredoc_with_redirection() {
        // 带重定向的 heredoc
        assert!(contains_heredoc("cat > file.sh << EOF\ncontent\nEOF"));
        assert!(contains_heredoc("cat > /tmp/test.sh << 'EOF'\ncontent\nEOF"));
    }

    #[test]
    fn test_contains_heredoc_false_positives() {
        // 不应该被识别为 heredoc 的情况
        assert!(!contains_heredoc("echo hello"));
        assert!(!contains_heredoc("cat file.txt"));
        assert!(!contains_heredoc("x << 1")); // 数字不是有效定界符
        assert!(!contains_heredoc("result << ")); // << 后面没有内容
        // 注意：`a << b` 会被识别为 heredoc，因为 b 是有效的定界符
        // 这是可接受的，因为这种语法在实际使用中很少见
    }

    #[test]
    fn test_format_command_simple() {
        let cmd = vec![
            "bash".to_string(),
            "-lc".to_string(),
            "echo hello".to_string(),
        ];
        let result = format_command_for_execution(&cmd);
        assert_eq!(result, "bash -lc 'echo hello'");
    }

    #[test]
    fn test_format_command_with_special_chars() {
        let cmd = vec![
            "bash".to_string(),
            "-c".to_string(),
            "echo $HOME && ls -la".to_string(),
        ];
        let result = format_command_for_execution(&cmd);
        // 应该用单引号包裹
        assert_eq!(result, "bash -c 'echo $HOME && ls -la'");
    }

    #[test]
    fn test_format_command_heredoc_not_quoted() {
        // heredoc 命令不应该被引用
        let cmd = vec![
            "bash".to_string(),
            "-lc".to_string(),
            "cat > file.sh << EOF\n#!/bin/bash\necho hello\nEOF".to_string(),
        ];
        let result = format_command_for_execution(&cmd);
        // heredoc 应该直接返回脚本内容，不带 bash -lc 前缀
        assert_eq!(result, "cat > file.sh << EOF\n#!/bin/bash\necho hello\nEOF");
    }

    #[test]
    fn test_format_command_heredoc_with_single_quotes() {
        // 带单引号定界符的 heredoc
        let cmd = vec![
            "bash".to_string(),
            "-lc".to_string(),
            "cat > test.sh << 'EOF'\n#!/bin/bash\necho \"hello\"\nEOF".to_string(),
        ];
        let result = format_command_for_execution(&cmd);
        // heredoc 应该直接返回脚本内容
        assert_eq!(result, "cat > test.sh << 'EOF'\n#!/bin/bash\necho \"hello\"\nEOF");
    }

    #[test]
    fn test_format_command_normal_command() {
        // 普通命令（非 bash -c 模式）
        let cmd = vec!["ls".to_string(), "-la".to_string(), "/tmp".to_string()];
        let result = format_command_for_execution(&cmd);
        assert_eq!(result, "ls -la /tmp");
    }

    #[test]
    fn test_format_command_with_spaces() {
        // 参数包含空格
        let cmd = vec!["echo".to_string(), "hello world".to_string()];
        let result = format_command_for_execution(&cmd);
        assert_eq!(result, "echo 'hello world'");
    }

    #[test]
    fn test_shell_quote_simple() {
        // 简单字符串不需要引用
        assert_eq!(shell_quote("hello"), "hello");
        assert_eq!(shell_quote("file.txt"), "file.txt");
        assert_eq!(shell_quote("/path/to/file"), "/path/to/file");
    }

    #[test]
    fn test_shell_quote_with_spaces() {
        // 包含空格的字符串需要引用
        assert_eq!(shell_quote("hello world"), "'hello world'");
    }

    #[test]
    fn test_shell_quote_with_single_quotes() {
        // 包含单引号的字符串使用双引号
        assert_eq!(shell_quote("it's"), "\"it's\"");
    }

    #[test]
    fn test_shell_quote_with_both_quotes() {
        // 同时包含单引号和双引号
        let result = shell_quote("say \"it's\"");
        // 应该使用单引号并转义内部单引号
        assert!(result.starts_with('\'') && result.ends_with('\''));
    }

    #[test]
    fn test_is_shell_operator() {
        // 测试 shell 操作符识别
        assert!(is_shell_operator(">"));
        assert!(is_shell_operator(">>"));
        assert!(is_shell_operator("<"));
        assert!(is_shell_operator("<<"));
        assert!(is_shell_operator("|"));
        assert!(is_shell_operator("||"));
        assert!(is_shell_operator("&&"));
        assert!(is_shell_operator(";"));
        assert!(is_shell_operator("&"));
        assert!(is_shell_operator("2>"));
        assert!(is_shell_operator("2>&1"));

        // 非操作符
        assert!(!is_shell_operator("cat"));
        assert!(!is_shell_operator("file.txt"));
        assert!(!is_shell_operator("-la"));
    }

    #[test]
    fn test_format_command_with_redirection() {
        // 测试带重定向的命令 - shell 操作符不应被引用
        let cmd = vec!["cat".to_string(), ">".to_string(), "file.txt".to_string()];
        let result = format_command_for_execution(&cmd);
        assert_eq!(result, "cat > file.txt");
    }

    #[test]
    fn test_format_command_with_pipe() {
        // 测试带管道的命令
        let cmd = vec!["ls".to_string(), "|".to_string(), "grep".to_string(), "test".to_string()];
        let result = format_command_for_execution(&cmd);
        assert_eq!(result, "ls | grep test");
    }

    #[test]
    fn test_format_command_with_logical_operators() {
        // 测试带逻辑操作符的命令
        let cmd = vec!["cmd1".to_string(), "&&".to_string(), "cmd2".to_string()];
        let result = format_command_for_execution(&cmd);
        assert_eq!(result, "cmd1 && cmd2");
    }

    #[test]
    fn test_format_command_with_stderr_redirect() {
        // 测试标准错误重定向
        let cmd = vec!["cmd".to_string(), "2>".to_string(), "error.log".to_string()];
        let result = format_command_for_execution(&cmd);
        assert_eq!(result, "cmd 2> error.log");
    }

    #[test]
    fn test_format_command_complex_redirection() {
        // 复杂重定向：cmd > out.txt 2>&1
        let cmd = vec![
            "cmd".to_string(),
            ">".to_string(),
            "out.txt".to_string(),
            "2>&1".to_string(),
        ];
        let result = format_command_for_execution(&cmd);
        assert_eq!(result, "cmd > out.txt 2>&1");
    }

    #[test]
    fn test_format_command_pwd_and_ls() {
        // 测试用户报告的问题：["pwd", "&&", "ls -la"]
        let cmd = vec!["pwd".to_string(), "&&".to_string(), "ls -la".to_string()];
        let result = format_command_for_execution(&cmd);
        // "ls -la" 应该被 looks_like_simple_command 识别，不加引号
        assert_eq!(result, "pwd && ls -la");
    }

    #[test]
    fn test_looks_like_simple_command() {
        // 测试 looks_like_simple_command 函数
        assert!(looks_like_simple_command("ls -la"));
        assert!(looks_like_simple_command("grep -r pattern"));
        assert!(looks_like_simple_command("find . -name foo"));
        assert!(looks_like_simple_command("/usr/bin/ls -la"));

        // 单独的命令名（不带参数）不应该被识别为简单命令
        // 因为它可能只是一个普通的参数值
        assert!(!looks_like_simple_command("ls"));
        assert!(!looks_like_simple_command("hello"));
        assert!(!looks_like_simple_command("world"));

        // 包含危险字符的不是简单命令
        assert!(!looks_like_simple_command("echo $HOME"));
        assert!(!looks_like_simple_command("cat && ls"));  // 包含 &
        assert!(!looks_like_simple_command("ls | grep"));   // 包含 |
    }
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
        yield_time_ms: u64,
    ) -> Result<UnifiedExecResponse, UnifiedExecError> {
        let context =
            UnifiedExecContext::new(Arc::clone(session), Arc::clone(turn), "call".to_string());
        let process_id = session
            .services
            .unified_exec_manager
            .allocate_process_id()
            .await;

        session
            .services
            .unified_exec_manager
            .exec_command(
                ExecCommandRequest {
                    command: vec!["bash".to_string(), "-lc".to_string(), cmd.to_string()],
                    process_id,
                    yield_time_ms,
                    max_output_tokens: None,
                    workdir: None,
                    with_escalated_permissions: None,
                    justification: None,
                },
                &context,
            )
            .await
    }

    async fn write_stdin(
        session: &Arc<Session>,
        process_id: &str,
        input: &str,
        yield_time_ms: u64,
    ) -> Result<UnifiedExecResponse, UnifiedExecError> {
        session
            .services
            .unified_exec_manager
            .write_stdin(WriteStdinRequest {
                call_id: "write-stdin",
                process_id,
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

        let open_shell = exec_command(&session, &turn, "bash -i", 2_500).await?;
        let process_id = open_shell
            .process_id
            .as_ref()
            .expect("expected process_id")
            .as_str();

        write_stdin(
            &session,
            process_id,
            "export CODEX_INTERACTIVE_SHELL_VAR=codex\n",
            2_500,
        )
        .await?;

        let out_2 = write_stdin(
            &session,
            process_id,
            "echo $CODEX_INTERACTIVE_SHELL_VAR\n",
            2_500,
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

        let shell_a = exec_command(&session, &turn, "bash -i", 2_500).await?;
        let session_a = shell_a
            .process_id
            .as_ref()
            .expect("expected process id")
            .clone();

        write_stdin(
            &session,
            session_a.as_str(),
            "export CODEX_INTERACTIVE_SHELL_VAR=codex\n",
            2_500,
        )
        .await?;

        let out_2 =
            exec_command(&session, &turn, "echo $CODEX_INTERACTIVE_SHELL_VAR", 2_500).await?;
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(
            out_2.process_id.is_none(),
            "short command should not report a process id if it exits quickly"
        );
        assert!(
            !out_2.output.contains("codex"),
            "short command should run in a fresh shell"
        );

        let out_3 = write_stdin(
            &session,
            shell_a
                .process_id
                .as_ref()
                .expect("expected process id")
                .as_str(),
            "echo $CODEX_INTERACTIVE_SHELL_VAR\n",
            2_500,
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

        let open_shell = exec_command(&session, &turn, "bash -i", 2_500).await?;
        let process_id = open_shell
            .process_id
            .as_ref()
            .expect("expected process id")
            .as_str();

        write_stdin(
            &session,
            process_id,
            "export CODEX_INTERACTIVE_SHELL_VAR=codex\n",
            2_500,
        )
        .await?;

        let out_2 = write_stdin(
            &session,
            process_id,
            "sleep 5 && echo $CODEX_INTERACTIVE_SHELL_VAR\n",
            10,
        )
        .await?;
        assert!(
            !out_2.output.contains("codex"),
            "timeout too short should yield incomplete output"
        );

        tokio::time::sleep(Duration::from_secs(7)).await;

        let out_3 = write_stdin(&session, process_id, "", 100).await?;

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

        let result = exec_command(&session, &turn, "echo codex", 120_000).await?;

        assert!(result.process_id.is_some());
        assert!(result.output.contains("codex"));

        Ok(())
    }

    #[tokio::test]
    #[ignore] // Ignored while we have a better way to test this.
    async fn completed_commands_do_not_persist_sessions() -> anyhow::Result<()> {
        let (session, turn) = test_session_and_turn();
        let result = exec_command(&session, &turn, "echo codex", 2_500).await?;

        assert!(
            result.process_id.is_some(),
            "completed command should report a process id"
        );
        assert!(result.output.contains("codex"));

        assert!(
            session
                .services
                .unified_exec_manager
                .session_store
                .lock()
                .await
                .sessions
                .is_empty()
        );

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reusing_completed_session_returns_unknown_session() -> anyhow::Result<()> {
        skip_if_sandbox!(Ok(()));

        let (session, turn) = test_session_and_turn();

        let open_shell = exec_command(&session, &turn, "bash -i", 2_500).await?;
        let process_id = open_shell
            .process_id
            .as_ref()
            .expect("expected process id")
            .as_str();

        write_stdin(&session, process_id, "exit\n", 2_500).await?;

        tokio::time::sleep(Duration::from_millis(200)).await;

        let err = write_stdin(&session, process_id, "", 100)
            .await
            .expect_err("expected unknown session error");

        match err {
            UnifiedExecError::UnknownSessionId { process_id: err_id } => {
                assert_eq!(err_id, process_id, "process id should match request");
            }
            other => panic!("expected UnknownSessionId, got {other:?}"),
        }

        assert!(
            session
                .services
                .unified_exec_manager
                .session_store
                .lock()
                .await
                .sessions
                .is_empty()
        );

        Ok(())
    }
}
