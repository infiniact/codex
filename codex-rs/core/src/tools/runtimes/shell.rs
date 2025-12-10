/*
Runtime: shell

Executes shell requests under the orchestrator: asks for approval when needed,
builds a CommandSpec, and runs it under the current SandboxAttempt.
*/
use crate::exec::ExecToolCallOutput;
use crate::sandboxing::execute_env;
use crate::tools::runtimes::build_command_spec;
use crate::tools::sandboxing::Approvable;
use crate::tools::sandboxing::ApprovalCtx;
use crate::tools::sandboxing::ExecApprovalRequirement;
use crate::tools::sandboxing::ProvidesSandboxRetryData;
use crate::tools::sandboxing::SandboxAttempt;
use crate::tools::sandboxing::SandboxOverride;
use crate::tools::sandboxing::SandboxRetryData;
use crate::tools::sandboxing::Sandboxable;
use crate::tools::sandboxing::SandboxablePreference;
use crate::tools::sandboxing::ToolCtx;
use crate::tools::sandboxing::ToolError;
use crate::tools::sandboxing::ToolRuntime;
use crate::tools::sandboxing::with_cached_approval;
use crate::unified_exec::format_command_for_execution;
use codex_protocol::protocol::ReviewDecision;
use futures::future::BoxFuture;
use std::path::PathBuf;

#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct ShellRequest {
    pub command: Vec<String>,
    pub cwd: PathBuf,
    pub timeout_ms: Option<u64>,
    pub env: std::collections::HashMap<String, String>,
    pub with_escalated_permissions: Option<bool>,
    pub justification: Option<String>,
    pub exec_approval_requirement: ExecApprovalRequirement,
}

impl ProvidesSandboxRetryData for ShellRequest {
    fn sandbox_retry_data(&self) -> Option<SandboxRetryData> {
        Some(SandboxRetryData {
            command: self.command.clone(),
            cwd: self.cwd.clone(),
        })
    }
}

#[derive(Default)]
#[allow(dead_code)]
pub struct ShellRuntime;

#[derive(serde::Serialize, Clone, Debug, Eq, PartialEq, Hash)]
pub(crate) struct ApprovalKey {
    command: Vec<String>,
    cwd: PathBuf,
    escalated: bool,
}

impl ShellRuntime {
    #[allow(dead_code)]
    pub fn new() -> Self {
        Self
    }

    #[allow(dead_code)]
    fn stdout_stream(ctx: &ToolCtx<'_>) -> Option<crate::exec::StdoutStream> {
        Some(crate::exec::StdoutStream {
            sub_id: ctx.turn.sub_id.clone(),
            call_id: ctx.call_id.clone(),
            tx_event: ctx.session.get_tx_event(),
        })
    }
}

impl Sandboxable for ShellRuntime {
    fn sandbox_preference(&self) -> SandboxablePreference {
        SandboxablePreference::Auto
    }
    fn escalate_on_failure(&self) -> bool {
        true
    }
}

impl Approvable<ShellRequest> for ShellRuntime {
    type ApprovalKey = ApprovalKey;

    fn approval_key(&self, req: &ShellRequest) -> Self::ApprovalKey {
        ApprovalKey {
            command: req.command.clone(),
            cwd: req.cwd.clone(),
            escalated: req.with_escalated_permissions.unwrap_or(false),
        }
    }

    fn start_approval_async<'a>(
        &'a mut self,
        req: &'a ShellRequest,
        ctx: ApprovalCtx<'a>,
    ) -> BoxFuture<'a, ReviewDecision> {
        let key = self.approval_key(req);
        let command = req.command.clone();
        let cwd = req.cwd.clone();
        let reason = ctx
            .retry_reason
            .clone()
            .or_else(|| req.justification.clone());
        let risk = ctx.risk.clone();
        let session = ctx.session;
        let turn = ctx.turn;
        let call_id = ctx.call_id.to_string();
        Box::pin(async move {
            with_cached_approval(&session.services, key, move || async move {
                session
                    .request_command_approval(
                        turn,
                        call_id,
                        command,
                        cwd,
                        reason,
                        risk,
                        req.exec_approval_requirement
                            .proposed_execpolicy_amendment()
                            .cloned(),
                    )
                    .await
            })
            .await
        })
    }

    fn exec_approval_requirement(&self, req: &ShellRequest) -> Option<ExecApprovalRequirement> {
        Some(req.exec_approval_requirement.clone())
    }

    fn sandbox_mode_for_first_attempt(&self, req: &ShellRequest) -> SandboxOverride {
        if req.with_escalated_permissions.unwrap_or(false)
            || matches!(
                req.exec_approval_requirement,
                ExecApprovalRequirement::Skip {
                    bypass_sandbox: true,
                    ..
                }
            )
        {
            SandboxOverride::BypassSandboxFirstAttempt
        } else {
            SandboxOverride::NoOverride
        }
    }
}

impl ToolRuntime<ShellRequest, ExecToolCallOutput> for ShellRuntime {
    async fn run(
        &mut self,
        req: &ShellRequest,
        attempt: &SandboxAttempt<'_>,
        ctx: &ToolCtx<'_>,
    ) -> Result<ExecToolCallOutput, ToolError> {
        // Check if PTY bridge is available and has associated connection
        if let Some(ref pty_bridge) = ctx.session.services.pty_bridge {
            // Get the connection_id for this conversation
            let conversation_id = ctx.session.get_conversation_id().to_string();
            if let Some(connection_id) = crate::unified_exec::get_global_conversation_connection(&conversation_id).await {
                tracing::info!(
                    "Using PTY bridge for command execution. conversation_id={}, connection_id={}",
                    conversation_id,
                    connection_id
                );

                // Build command string from command vec - 使用智能格式化处理 bash -c 等命令
                let command_str = format_command_for_execution(&req.command);
                let shell = ctx.session.user_shell().shell_path.to_string_lossy().to_string();

                // Execute through PTY bridge
                match pty_bridge.execute(
                    &command_str,
                    &shell,
                    true,  // login shell
                    true,  // display_in_panel
                    Some(&connection_id),
                    None,  // no stdin
                ).await {
                    Ok(result) => {
                        tracing::info!(
                            "PTY bridge execution completed. exit_code={:?}, output_len={}",
                            result.exit_code,
                            result.output.len()
                        );
                        return Ok(ExecToolCallOutput {
                            exit_code: result.exit_code.unwrap_or(0),
                            stdout: crate::exec::StreamOutput::new(result.output.clone()),
                            stderr: crate::exec::StreamOutput::new(String::new()),
                            aggregated_output: crate::exec::StreamOutput::new(result.output),
                            duration: std::time::Duration::ZERO,
                            timed_out: false,
                        });
                    }
                    Err(e) => {
                        tracing::warn!(
                            "PTY bridge execution failed, falling back to local execution: {}",
                            e
                        );
                        // Fall through to local execution
                    }
                }
            } else {
                tracing::debug!(
                    "No connection_id found for conversation {}, using local execution",
                    conversation_id
                );
            }
        }

        // Fall back to local execution (original implementation)
        let spec = build_command_spec(
            &req.command,
            &req.cwd,
            &req.env,
            req.timeout_ms.into(),
            req.with_escalated_permissions,
            req.justification.clone(),
        )?;
        let env = attempt
            .env_for(spec)
            .map_err(|err| ToolError::Codex(err.into()))?;
        let out = execute_env(env, attempt.policy, Self::stdout_stream(ctx))
            .await
            .map_err(ToolError::Codex)?;
        Ok(out)
    }
}
