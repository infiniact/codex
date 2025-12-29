use async_trait::async_trait;
use codex_protocol::models::ShellCommandToolCallParams;
use codex_protocol::models::ShellToolCallParams;
use std::sync::Arc;

use crate::codex::TurnContext;
use crate::exec::ExecParams;
use crate::exec_env::create_env;
use crate::exec_policy::create_exec_approval_requirement_for_command;
use crate::function_tool::FunctionCallError;
use crate::is_safe_command::is_known_safe_command;
use crate::protocol::ExecCommandSource;
use crate::sandboxing::SandboxPermissions;
use crate::shell_utils::parse_json_with_recovery;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::events::ToolEmitter;
use crate::tools::events::ToolEventCtx;
use crate::tools::handlers::apply_patch::intercept_apply_patch;
use crate::tools::orchestrator::ToolOrchestrator;
use crate::tools::registry::ToolHandler;
use crate::tools::registry::ToolKind;
use crate::tools::runtimes::shell::ShellRequest;
use crate::tools::runtimes::shell::ShellRuntime;
use crate::tools::sandboxing::ToolCtx;

pub struct ShellHandler;

pub struct ShellCommandHandler;

impl ShellHandler {
    fn to_exec_params(
        params: ShellToolCallParams,
        session: &crate::codex::Session,
        turn_context: &TurnContext,
    ) -> ExecParams {
        // command 现在是字符串格式，与 ShellCommandHandler 相同
        // 检测命令是否包含 heredoc 语法或其他 shell 特性
        use crate::shell_utils::contains_heredoc;
        use crate::shell_utils::command_string_needs_shell;

        let command = if contains_heredoc(&params.command) || command_string_needs_shell(&params.command) {
            // 包含 heredoc 或 shell 特殊语法（重定向、管道等）的命令需要使用 shell 包装执行
            // 因为这些是 shell 特性，必须由 shell 解释
            tracing::info!("🔧 检测到 shell 特殊语法，使用 shell 包装执行: {}", &params.command);
            let shell = session.user_shell();
            shell.derive_exec_args(&params.command, true)
        } else {
            // 普通命令使用 shlex 解析
            shlex::split(&params.command)
                .unwrap_or_else(|| vec![params.command.clone()])
        };

        ExecParams {
            command,
            cwd: turn_context.resolve_path(params.workdir.clone()),
            expiration: params.timeout_ms.into(),
            env: create_env(&turn_context.shell_environment_policy),
            sandbox_permissions: params.with_escalated_permissions.unwrap_or(false).into(),
            justification: params.justification,
            arg0: None,
        }
    }
}

impl ShellCommandHandler {
    fn to_exec_params(
        params: ShellCommandToolCallParams,
        session: &crate::codex::Session,
        turn_context: &TurnContext,
    ) -> ExecParams {
        // 🔧 检测命令是否包含 shell 特殊语法
        use crate::shell_utils::contains_heredoc;
        use crate::shell_utils::command_string_needs_shell;

        let command = if contains_heredoc(&params.command) || command_string_needs_shell(&params.command) {
            // 包含 heredoc 或 shell 特殊语法（重定向、管道等）的命令需要使用 shell 包装执行
            // 因为这些是 shell 特性，必须由 shell 解释
            tracing::info!("🔧 检测到 shell 特殊语法，使用 shell 包装执行: {}", &params.command);
            let shell = session.user_shell();
            shell.derive_exec_args(&params.command, true)
        } else {
            // 普通命令使用 shlex 解析
            shlex::split(&params.command)
                .unwrap_or_else(|| vec![params.command.clone()])
        };

        ExecParams {
            command,
            cwd: turn_context.resolve_path(params.workdir.clone()),
            expiration: params.timeout_ms.into(),
            env: create_env(&turn_context.shell_environment_policy),
            sandbox_permissions: params.with_escalated_permissions.unwrap_or(false).into(),
            justification: params.justification,
            arg0: None,
        }
    }
}

#[async_trait]
impl ToolHandler for ShellHandler {
    fn kind(&self) -> ToolKind {
        ToolKind::Function
    }

    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(
            payload,
            ToolPayload::Function { .. } | ToolPayload::LocalShell { .. }
        )
    }

    async fn is_mutating(&self, invocation: &ToolInvocation) -> bool {
        match &invocation.payload {
            ToolPayload::Function { arguments } => {
                parse_json_with_recovery::<ShellToolCallParams>(arguments)
                    .map(|params| {
                        // command 现在是字符串，需要解析为数组以检查安全性
                        let command = shlex::split(&params.command)
                            .unwrap_or_else(|| vec![params.command.clone()]);
                        !is_known_safe_command(&command)
                    })
                    .unwrap_or(true)
            }
            ToolPayload::LocalShell { params } => {
                // LocalShell 的 params.command 现在也是 String，需要解析为数组
                let command = shlex::split(&params.command)
                    .unwrap_or_else(|| vec![params.command.clone()]);
                !is_known_safe_command(&command)
            }
            _ => true, // unknown payloads => assume mutating
        }
    }

    async fn handle(&self, invocation: ToolInvocation) -> Result<ToolOutput, FunctionCallError> {
        let ToolInvocation {
            session,
            turn,
            tracker,
            call_id,
            tool_name,
            payload,
        } = invocation;

        match payload {
            ToolPayload::Function { arguments } => {
                let params: ShellToolCallParams =
                    parse_json_with_recovery(&arguments).map_err(|e| {
                        FunctionCallError::RespondToModel(format!(
                            "failed to parse function arguments: {e:?}"
                        ))
                    })?;
                let exec_params = Self::to_exec_params(params, session.as_ref(), turn.as_ref());
                Self::run_exec_like(
                    tool_name.as_str(),
                    exec_params,
                    session,
                    turn,
                    tracker,
                    call_id,
                    false,
                )
                .await
            }
            ToolPayload::LocalShell { params } => {
                let exec_params = Self::to_exec_params(params, session.as_ref(), turn.as_ref());
                Self::run_exec_like(
                    tool_name.as_str(),
                    exec_params,
                    session,
                    turn,
                    tracker,
                    call_id,
                    false,
                )
                .await
            }
            _ => Err(FunctionCallError::RespondToModel(format!(
                "unsupported payload for shell handler: {tool_name}"
            ))),
        }
    }
}

#[async_trait]
impl ToolHandler for ShellCommandHandler {
    fn kind(&self) -> ToolKind {
        ToolKind::Function
    }

    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }

    async fn is_mutating(&self, invocation: &ToolInvocation) -> bool {
        let ToolPayload::Function { arguments } = &invocation.payload else {
            return true;
        };

        parse_json_with_recovery::<ShellCommandToolCallParams>(arguments)
            .map(|params| {
                use crate::shell_utils::contains_heredoc;

                // heredoc 命令通常用于写入文件，应视为 mutating
                if contains_heredoc(&params.command) {
                    return true;
                }

                // 普通命令使用 shlex 解析后检查
                let command = shlex::split(&params.command)
                    .unwrap_or_else(|| vec![params.command.clone()]);
                !is_known_safe_command(&command)
            })
            .unwrap_or(true)
    }

    async fn handle(&self, invocation: ToolInvocation) -> Result<ToolOutput, FunctionCallError> {
        let ToolInvocation {
            session,
            turn,
            tracker,
            call_id,
            tool_name,
            payload,
        } = invocation;

        let ToolPayload::Function { arguments } = payload else {
            return Err(FunctionCallError::RespondToModel(format!(
                "unsupported payload for shell_command handler: {tool_name}"
            )));
        };

        let params: ShellCommandToolCallParams = parse_json_with_recovery(&arguments).map_err(|e| {
            FunctionCallError::RespondToModel(format!("failed to parse function arguments: {e:?}"))
        })?;
        let exec_params = Self::to_exec_params(params, session.as_ref(), turn.as_ref());
        ShellHandler::run_exec_like(
            tool_name.as_str(),
            exec_params,
            session,
            turn,
            tracker,
            call_id,
            true,
        )
        .await
    }
}

impl ShellHandler {
    async fn run_exec_like(
        tool_name: &str,
        exec_params: ExecParams,
        session: Arc<crate::codex::Session>,
        turn: Arc<TurnContext>,
        tracker: crate::tools::context::SharedTurnDiffTracker,
        call_id: String,
        freeform: bool,
    ) -> Result<ToolOutput, FunctionCallError> {
        // Approval policy guard for explicit escalation in non-OnRequest modes.
        if exec_params.sandbox_permissions == SandboxPermissions::RequireEscalated
            && !matches!(
                turn.approval_policy,
                codex_protocol::protocol::AskForApproval::OnRequest
            )
        {
            return Err(FunctionCallError::RespondToModel(format!(
                "approval policy is {policy:?}; reject command — you should not ask for escalated permissions if the approval policy is {policy:?}",
                policy = turn.approval_policy
            )));
        }

        // Intercept apply_patch if present.
        if let Some(output) = intercept_apply_patch(
            &exec_params.command,
            &exec_params.cwd,
            exec_params.expiration.timeout_ms(),
            session.as_ref(),
            turn.as_ref(),
            Some(&tracker),
            &call_id,
            tool_name,
        )
        .await?
        {
            return Ok(output);
        }

        let source = ExecCommandSource::Agent;
        let emitter = ToolEmitter::shell(
            exec_params.command.clone(),
            exec_params.cwd.clone(),
            source,
            freeform,
        );
        let event_ctx = ToolEventCtx::new(session.as_ref(), turn.as_ref(), &call_id, None);
        emitter.begin(event_ctx).await;

        let features = session.features();
        let exec_approval_requirement = create_exec_approval_requirement_for_command(
            &turn.exec_policy,
            &features,
            &exec_params.command,
            turn.approval_policy,
            &turn.sandbox_policy,
            exec_params.sandbox_permissions,
        );

        let req = ShellRequest {
            command: exec_params.command.clone(),
            cwd: exec_params.cwd.clone(),
            timeout_ms: exec_params.expiration.timeout_ms(),
            env: exec_params.env.clone(),
            with_escalated_permissions: Some(exec_params.sandbox_permissions == SandboxPermissions::RequireEscalated),
            justification: exec_params.justification.clone(),
            exec_approval_requirement,
        };
        let mut orchestrator = ToolOrchestrator::new();
        let mut runtime = ShellRuntime::new();
        let tool_ctx = ToolCtx {
            session: session.as_ref(),
            turn: turn.as_ref(),
            call_id: call_id.clone(),
            tool_name: tool_name.to_string(),
        };
        let out = orchestrator
            .run(&mut runtime, &req, &tool_ctx, &turn, turn.approval_policy)
            .await;
        let event_ctx = ToolEventCtx::new(session.as_ref(), turn.as_ref(), &call_id, None);
        let content = emitter.finish(event_ctx, out).await?;
        Ok(ToolOutput::Function {
            content,
            content_items: None,
            success: Some(true),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use codex_protocol::models::ShellCommandToolCallParams;
    use codex_protocol::models::ShellToolCallParams;
    use pretty_assertions::assert_eq;

    use crate::codex::make_session_and_context;
    use crate::exec_env::create_env;
    use crate::is_safe_command::is_known_safe_command;
    use crate::sandboxing::SandboxPermissions;
    use crate::shell::Shell;
    use crate::shell::ShellType;
    use crate::shell_utils::{command_needs_shell_wrapping, join_command_for_shell};
    use crate::tools::handlers::ShellCommandHandler;
    use crate::tools::handlers::ShellHandler;

    /// The logic for is_known_safe_command() has heuristics for known shells,
    /// so we must ensure the commands generated by [ShellCommandHandler] can be
    /// recognized as safe if the `command` is safe.
    #[test]
    fn commands_generated_by_shell_command_handler_can_be_matched_by_is_known_safe_command() {
        let bash_shell = Shell {
            shell_type: ShellType::Bash,
            shell_path: PathBuf::from("/bin/bash"),
            shell_snapshot: None,
        };
        assert_safe(&bash_shell, "ls -la");

        let zsh_shell = Shell {
            shell_type: ShellType::Zsh,
            shell_path: PathBuf::from("/bin/zsh"),
            shell_snapshot: None,
        };
        assert_safe(&zsh_shell, "ls -la");

        let powershell = Shell {
            shell_type: ShellType::PowerShell,
            shell_path: PathBuf::from("pwsh.exe"),
            shell_snapshot: None,
        };
        assert_safe(&powershell, "ls -Name");
    }

    fn assert_safe(shell: &Shell, command: &str) {
        assert!(is_known_safe_command(
            &shell.derive_exec_args(command, /* use_login_shell */ true)
        ));
        assert!(is_known_safe_command(
            &shell.derive_exec_args(command, /* use_login_shell */ false)
        ));
    }

    #[test]
    fn shell_command_handler_to_exec_params_uses_session_shell_and_turn_context() {
        let (session, turn_context) = make_session_and_context();

        let command = "echo hello".to_string();
        let workdir = Some("subdir".to_string());
        let login = None;
        let timeout_ms = Some(1234);
        let with_escalated_permissions = Some(true);
        let justification = Some("because tests".to_string());

        // 🔧 现在命令直接使用 shlex 解析，不再用 session shell 包装
        let expected_command = shlex::split(&command).unwrap();
        let expected_cwd = turn_context.resolve_path(workdir.clone());
        let expected_env = create_env(&turn_context.shell_environment_policy);

        let params = ShellCommandToolCallParams {
            command,
            workdir,
            login,
            timeout_ms,
            with_escalated_permissions,
            justification: justification.clone(),
        };

        let exec_params = ShellCommandHandler::to_exec_params(params, &session, &turn_context);

        // ExecParams cannot derive Eq due to the CancellationToken field, so we manually compare the fields.
        assert_eq!(exec_params.command, expected_command);
        assert_eq!(exec_params.cwd, expected_cwd);
        assert_eq!(exec_params.env, expected_env);
        assert_eq!(exec_params.expiration.timeout_ms(), timeout_ms);
        assert_eq!(
            exec_params.sandbox_permissions,
            SandboxPermissions::RequireEscalated
        );
        assert_eq!(exec_params.justification, justification);
        assert_eq!(exec_params.arg0, None);
    }

    #[test]
    fn test_command_needs_shell_wrapping() {
        // 包含 shell 操作符的命令
        assert!(command_needs_shell_wrapping(&[
            "cat".to_string(),
            ">".to_string(),
            "file.txt".to_string()
        ]));
        assert!(command_needs_shell_wrapping(&[
            "cat".to_string(),
            ">>".to_string(),
            "file.txt".to_string()
        ]));
        assert!(command_needs_shell_wrapping(&[
            "echo".to_string(),
            "hello".to_string(),
            "|".to_string(),
            "grep".to_string(),
            "h".to_string()
        ]));
        assert!(command_needs_shell_wrapping(&[
            "ls".to_string(),
            "&&".to_string(),
            "pwd".to_string()
        ]));
        assert!(command_needs_shell_wrapping(&[
            "cmd1".to_string(),
            "||".to_string(),
            "cmd2".to_string()
        ]));

        // 不包含 shell 操作符的普通命令
        assert!(!command_needs_shell_wrapping(&[
            "ls".to_string(),
            "-la".to_string()
        ]));
        assert!(!command_needs_shell_wrapping(&[
            "cat".to_string(),
            "file.txt".to_string()
        ]));
        assert!(!command_needs_shell_wrapping(&["pwd".to_string()]));
    }

    #[test]
    fn test_join_command_for_shell() {
        // 简单命令
        assert_eq!(
            join_command_for_shell(&[
                "cat".to_string(),
                ">".to_string(),
                "file.txt".to_string()
            ]),
            "cat > file.txt"
        );

        // 带空格的参数 - shell_utils 使用单引号
        assert_eq!(
            join_command_for_shell(&[
                "echo".to_string(),
                "hello world".to_string(),
                ">".to_string(),
                "file.txt".to_string()
            ]),
            "echo 'hello world' > file.txt"
        );

        // 管道命令
        assert_eq!(
            join_command_for_shell(&[
                "cat".to_string(),
                "file.txt".to_string(),
                "|".to_string(),
                "grep".to_string(),
                "pattern".to_string()
            ]),
            "cat file.txt | grep pattern"
        );
    }

    #[test]
    fn shell_handler_wraps_command_with_shell_operators() {
        let (session, turn_context) = make_session_and_context();

        // 测试包含重定向操作符的命令（现在使用字符串格式）
        let params = ShellToolCallParams {
            command: "cat > test.txt".to_string(),
            workdir: None,
            timeout_ms: None,
            with_escalated_permissions: None,
            sandbox_permissions: None,
            justification: None,
            stdin: None,
        };

        let exec_params = ShellHandler::to_exec_params(params, &session, &turn_context);

        // 包含 shell 操作符的命令应该被 shell 包装
        // 因为重定向操作符需要由 shell 解释
        let shell = session.user_shell();
        let expected = shell.derive_exec_args("cat > test.txt", true);
        assert_eq!(exec_params.command, expected);
    }

    #[test]
    fn shell_handler_does_not_wrap_simple_commands() {
        let (session, turn_context) = make_session_and_context();

        // 测试不包含 shell 操作符的简单命令（现在使用字符串格式）
        let params = ShellToolCallParams {
            command: "ls -la".to_string(),
            workdir: None,
            timeout_ms: None,
            with_escalated_permissions: None,
            sandbox_permissions: None,
            justification: None,
            stdin: None,
        };

        let exec_params = ShellHandler::to_exec_params(params, &session, &turn_context);

        // 简单命令不应该被包装
        assert_eq!(exec_params.command, vec!["ls".to_string(), "-la".to_string()]);
    }

    #[test]
    fn shell_handler_wraps_heredoc_commands() {
        let (session, turn_context) = make_session_and_context();

        // 测试 heredoc 命令会被 shell 包装
        let params = ShellToolCallParams {
            command: "cat > file.txt << 'EOF'\nhello\nEOF".to_string(),
            workdir: None,
            timeout_ms: None,
            with_escalated_permissions: None,
            sandbox_permissions: None,
            justification: None,
            stdin: None,
        };

        let exec_params = ShellHandler::to_exec_params(params, &session, &turn_context);

        // heredoc 命令应该被 shell 包装
        let shell = session.user_shell();
        let expected = shell.derive_exec_args("cat > file.txt << 'EOF'\nhello\nEOF", true);
        assert_eq!(exec_params.command, expected);
    }

    #[test]
    fn shell_command_handler_parses_shell_wrapped_command() {
        let (session, turn_context) = make_session_and_context();

        // 测试 shell 包装格式的命令会被正确解析
        let params = ShellCommandToolCallParams {
            command: "bash -lc 'pacman -Qk 2>/dev/null | head -20'".to_string(),
            workdir: None,
            login: None,
            timeout_ms: None,
            with_escalated_permissions: None,
            justification: None,
        };

        let exec_params = ShellCommandHandler::to_exec_params(params, &session, &turn_context);

        // 命令应该被正确解析为参数数组，不添加额外包装
        assert_eq!(exec_params.command, vec![
            "bash".to_string(),
            "-lc".to_string(),
            "pacman -Qk 2>/dev/null | head -20".to_string()
        ]);
    }

    #[test]
    fn shell_command_handler_parses_simple_command() {
        let (session, turn_context) = make_session_and_context();

        // 测试普通命令会被直接解析，不添加 shell 包装
        let params = ShellCommandToolCallParams {
            command: "ls -la".to_string(),
            workdir: None,
            login: None,
            timeout_ms: None,
            with_escalated_permissions: None,
            justification: None,
        };

        let exec_params = ShellCommandHandler::to_exec_params(params, &session, &turn_context);

        // 🔧 命令应该被直接解析为参数数组，不添加任何 shell 包装
        assert_eq!(exec_params.command, vec!["ls".to_string(), "-la".to_string()]);
    }
}
