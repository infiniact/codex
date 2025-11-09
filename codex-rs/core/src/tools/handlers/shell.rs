use async_trait::async_trait;
use codex_protocol::models::ShellToolCallParams;
use std::sync::Arc;

use crate::apply_patch;
use crate::apply_patch::InternalApplyPatchInvocation;
use crate::apply_patch::convert_apply_patch_to_protocol;
use crate::codex::TurnContext;
use crate::exec::ExecParams;
use crate::exec_env::create_env;
use crate::function_tool::FunctionCallError;
use crate::protocol::EventMsg;
use crate::protocol::ExecCommandEndEvent;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::events::ToolEmitter;
use crate::tools::events::ToolEventCtx;
use crate::tools::orchestrator::ToolOrchestrator;
use crate::tools::registry::ToolHandler;
use crate::tools::registry::ToolKind;
use crate::tools::runtimes::apply_patch::ApplyPatchRequest;
use crate::tools::runtimes::apply_patch::ApplyPatchRuntime;
use crate::tools::sandboxing::ToolCtx;
use crate::unified_exec::ExecCommandRequest;
use crate::unified_exec::ExecutionBackend;
use crate::unified_exec::UnifiedExecContext;
use crate::unified_exec::UnifiedExecSessionManager;

pub struct ShellHandler;

impl ShellHandler {
    fn to_exec_params(params: ShellToolCallParams, turn_context: &TurnContext) -> ExecParams {
        ExecParams {
            command: params.command,
            cwd: turn_context.resolve_path(params.workdir.clone()),
            timeout_ms: params.timeout_ms,
            env: create_env(&turn_context.shell_environment_policy),
            with_escalated_permissions: params.with_escalated_permissions,
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
                    serde_json::from_str(&arguments).map_err(|e| {
                        FunctionCallError::RespondToModel(format!(
                            "failed to parse function arguments: {e:?}"
                        ))
                    })?;
                let stdin = params.stdin.clone();
                let exec_params = Self::to_exec_params(params, turn.as_ref());
                Self::run_exec_like(
                    tool_name.as_str(),
                    exec_params,
                    stdin,
                    session,
                    turn,
                    tracker,
                    call_id,
                    false,
                )
                .await
            }
            ToolPayload::LocalShell { params } => {
                let stdin = params.stdin.clone();
                let exec_params = Self::to_exec_params(params, turn.as_ref());
                Self::run_exec_like(
                    tool_name.as_str(),
                    exec_params,
                    stdin,
                    session,
                    turn,
                    tracker,
                    call_id,
                    true,
                )
                .await
            }
            _ => Err(FunctionCallError::RespondToModel(format!(
                "unsupported payload for shell handler: {tool_name}"
            ))),
        }
    }
}

impl ShellHandler {
    #[allow(clippy::too_many_arguments)]
    async fn run_exec_like(
        tool_name: &str,
        exec_params: ExecParams,
        stdin_content: Option<String>,
        session: Arc<crate::codex::Session>,
        turn: Arc<TurnContext>,
        tracker: crate::tools::context::SharedTurnDiffTracker,
        call_id: String,
        is_user_shell_command: bool,
    ) -> Result<ToolOutput, FunctionCallError> {
        // Approval policy guard for explicit escalation in non-OnRequest modes.
        if exec_params.with_escalated_permissions.unwrap_or(false)
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
        match codex_apply_patch::maybe_parse_apply_patch_verified(
            &exec_params.command,
            &exec_params.cwd,
        ) {
            codex_apply_patch::MaybeApplyPatchVerified::Body(changes) => {
                match apply_patch::apply_patch(session.as_ref(), turn.as_ref(), &call_id, changes)
                    .await
                {
                    InternalApplyPatchInvocation::Output(item) => {
                        // Programmatic apply_patch path; return its result.
                        let content = item?;
                        return Ok(ToolOutput::Function {
                            content,
                            content_items: None,
                            success: Some(true),
                        });
                    }
                    InternalApplyPatchInvocation::DelegateToExec(apply) => {
                        let emitter = ToolEmitter::apply_patch(
                            convert_apply_patch_to_protocol(&apply.action),
                            !apply.user_explicitly_approved_this_action,
                        );
                        let event_ctx = ToolEventCtx::new(
                            session.as_ref(),
                            turn.as_ref(),
                            &call_id,
                            Some(&tracker),
                        );
                        emitter.begin(event_ctx).await;

                        let req = ApplyPatchRequest {
                            patch: apply.action.patch.clone(),
                            cwd: apply.action.cwd.clone(),
                            timeout_ms: exec_params.timeout_ms,
                            user_explicitly_approved: apply.user_explicitly_approved_this_action,
                            codex_exe: turn.codex_linux_sandbox_exe.clone(),
                        };
                        let mut orchestrator = ToolOrchestrator::new();
                        let mut runtime = ApplyPatchRuntime::new();
                        let tool_ctx = ToolCtx {
                            session: session.as_ref(),
                            turn: turn.as_ref(),
                            call_id: call_id.clone(),
                            tool_name: tool_name.to_string(),
                        };
                        let out = orchestrator
                            .run(&mut runtime, &req, &tool_ctx, &turn, turn.approval_policy)
                            .await;
                        let event_ctx = ToolEventCtx::new(
                            session.as_ref(),
                            turn.as_ref(),
                            &call_id,
                            Some(&tracker),
                        );
                        let content = emitter.finish(event_ctx, out).await?;
                        return Ok(ToolOutput::Function {
                            content,
                            content_items: None,
                            success: Some(true),
                        });
                    }
                }
            }
            codex_apply_patch::MaybeApplyPatchVerified::CorrectnessError(parse_error) => {
                return Err(FunctionCallError::RespondToModel(format!(
                    "apply_patch verification failed: {parse_error}"
                )));
            }
            codex_apply_patch::MaybeApplyPatchVerified::ShellParseError(error) => {
                tracing::trace!("Failed to parse shell command, {error:?}");
                // Fall through to regular shell execution.
            }
            codex_apply_patch::MaybeApplyPatchVerified::NotApplyPatch => {
                // Fall through to regular shell execution.
            }
        }

        // Regular shell execution path.
        let emitter = ToolEmitter::shell(
            exec_params.command.clone(),
            exec_params.cwd.clone(),
            is_user_shell_command,
        );
        let event_ctx = ToolEventCtx::new(session.as_ref(), turn.as_ref(), &call_id, None);
        emitter.begin(event_ctx).await;

        // 获取 unified_exec_manager 并创建执行上下文
        let manager: &UnifiedExecSessionManager = &session.services.unified_exec_manager;

        // 从 global connection map 中查询 connection_id
        let conversation_id = session.conversation_id().to_string();
        tracing::info!("🔍 [shell handler] 查询会话的连接ID - conversation_id: {conversation_id}");

        let connection_id = crate::unified_exec::get_global_conversation_connection(&conversation_id).await;
        if let Some(ref conn_id) = connection_id {
            tracing::info!("🔗 [shell handler] ✅ 找到会话的连接ID: {conn_id}");
        } else {
            tracing::warn!("⚠️ [shell handler] ❌ 未找到会话的连接ID，将创建新连接");
        }

        let context = UnifiedExecContext::with_connection_id(
            session.clone(),
            turn.clone(),
            call_id.clone(),
            conversation_id,
            connection_id,
        );

        // 将 Vec<String> 命令转换为单个字符串
        let command_str = exec_params.command.join(" ");

        // 添加调试日志
        tracing::info!("🔍 [shell handler] 原始命令数组: {:?}", exec_params.command);
        tracing::info!("🔍 [shell handler] 连接后的命令字符串: '{command_str}'");
        tracing::info!("🔍 [shell handler] 命令数组长度: {}, 内容: {:?}",
            exec_params.command.len(), exec_params.command);
        if let Some(ref stdin) = stdin_content {
            tracing::info!("🔍 [shell handler] Stdin 内容长度: {}", stdin.len());
            tracing::info!("🔍 [shell handler] Stdin 内容（前200字符）: {:?}",
                stdin.chars().take(200).collect::<String>());
        } else {
            tracing::info!("🔍 [shell handler] 无 Stdin 内容");
        }

        // 调用 unified_exec 执行命令，直接传递 stdin 参数
        let response = manager
            .exec_command(
                ExecCommandRequest {
                    command: &command_str,
                    shell: "/bin/bash",
                    login: true,
                    yield_time_ms: None,
                    max_output_tokens: None,
                    backend: Some(ExecutionBackend::PtyService),
                    display_in_panel: true,
                    stdin: stdin_content.as_deref(),
                },
                &context,
            )
            .await
            .map_err(|err| {
                FunctionCallError::RespondToModel(format!("shell execution failed: {err:?}"))
            })?;

        // 发送 ExecCommandEnd 事件
        let end_event = ExecCommandEndEvent {
            call_id: call_id.clone(),
            stdout: response.output.clone(),
            stderr: String::new(),
            aggregated_output: response.output.clone(),
            exit_code: response.exit_code.unwrap_or(0),
            duration: response.wall_time,
            formatted_output: response.output.clone(),
        };
        session
            .send_event(turn.as_ref(), EventMsg::ExecCommandEnd(end_event))
            .await;

        // 将 UnifiedExecResponse 转换为 shell 工具的输出格式
        let content = format!(
            r#"{{"output":"{}","exit_code":{}}}"#,
            response.output.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n"),
            response.exit_code.unwrap_or(0)
        );

        Ok(ToolOutput::Function {
            content,
            content_items: None,
            success: Some(response.exit_code.is_none() || response.exit_code == Some(0)),
        })
    }
}
