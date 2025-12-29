use crate::client_common::tools::ToolSpec;
use crate::codex::Session;
use crate::codex::TurnContext;
use crate::function_tool::FunctionCallError;
use crate::sandboxing::SandboxPermissions;
use crate::tools::context::SharedTurnDiffTracker;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::registry::ConfiguredToolSpec;
use crate::tools::registry::ToolRegistry;
use crate::tools::spec::ToolsConfig;
use crate::tools::spec::build_specs;
use codex_protocol::models::LocalShellAction;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::models::ShellToolCallParams;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::instrument;

#[derive(Clone, Debug)]
pub struct ToolCall {
    pub tool_name: String,
    pub call_id: String,
    pub payload: ToolPayload,
}

pub struct ToolRouter {
    registry: ToolRegistry,
    specs: Vec<ConfiguredToolSpec>,
}

impl ToolRouter {
    pub fn from_config(
        config: &ToolsConfig,
        mcp_tools: Option<HashMap<String, mcp_types::Tool>>,
    ) -> Self {
        // Log the loaded tools before moving mcp_tools
        Self::log_loaded_tools(&mcp_tools);

        let builder = build_specs(config, mcp_tools);
        let (specs, registry) = builder.build();

        // Log the loaded specs after they are built
        Self::log_loaded_specs(&specs);

        Self { registry, specs }
    }

    fn log_loaded_tools(mcp_tools: &Option<HashMap<String, mcp_types::Tool>>) {
        use tracing::info;

        info!("🔧 [ToolRouter] === MCP 工具信息 ===");

        // 输出 MCP 工具信息
        if let Some(mcp_tools) = mcp_tools {
            if !mcp_tools.is_empty() {
                info!("🌐 [ToolRouter] MCP 工具总数: {} 个", mcp_tools.len());
                let mut mcp_servers = std::collections::HashSet::new();
                for tool_name in mcp_tools.keys() {
                    if let Some((server_name, _)) = crate::mcp::split_qualified_tool_name(tool_name) {
                        mcp_servers.insert(server_name);
                    }
                }
                for server in mcp_servers {
                    info!("  • 服务器: {}", server);
                    let server_tools: Vec<_> = mcp_tools.keys()
                        .filter(|name| {
                            if let Some((srv, _)) = crate::mcp::split_qualified_tool_name(name) {
                                srv == server
                            } else {
                                false
                            }
                        })
                        .collect();
                    for tool in server_tools {
                        info!("    - {}", tool);
                    }
                }
            } else {
                info!("🌐 [ToolRouter] 未配置 MCP 工具");
            }
        } else {
            info!("🌐 [ToolRouter] 未配置 MCP 工具");
        }

        info!("🔧 [ToolRouter] === MCP 工具信息完成 ===");
    }

    fn log_loaded_specs(specs: &[ConfiguredToolSpec]) {
        use tracing::info;

        info!("🔧 [ToolRouter] === 已加载的工具规格 ===");
        info!("📊 [ToolRouter] 总工具数量: {}", specs.len());

        let mut categories = std::collections::HashMap::new();
        let mut parallel_tools = Vec::new();

        for spec in specs {
            let tool_name = spec.spec.name();
            let tool_type = match &spec.spec {
                crate::client_common::tools::ToolSpec::Function(_) => "Function",
                crate::client_common::tools::ToolSpec::LocalShell {} => "LocalShell",
                crate::client_common::tools::ToolSpec::WebSearch {} => "WebSearch",
                crate::client_common::tools::ToolSpec::Freeform(_) => "Freeform",
            };

            categories.entry(tool_type).or_insert_with(Vec::new).push(tool_name);

            if spec.supports_parallel_tool_calls {
                parallel_tools.push(tool_name);
            }
        }

        // 按类别输出工具
        for (category, tools) in categories {
            info!("📦 [ToolRouter] {}: {} 个", category, tools.len());
            for tool in tools {
                info!("  • {}", tool);
            }
        }

        // 输出支持并行的工具
        if !parallel_tools.is_empty() {
            info!("⚡ [ToolRouter] 支持并行调用的工具: {} 个", parallel_tools.len());
            for tool in parallel_tools {
                info!("  • {}", tool);
            }
        }

        info!("🔧 [ToolRouter] === 工具规格加载完成 ===");
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.specs
            .iter()
            .map(|config| config.spec.clone())
            .collect()
    }

    pub fn tool_supports_parallel(&self, tool_name: &str) -> bool {
        self.specs
            .iter()
            .filter(|config| config.supports_parallel_tool_calls)
            .any(|config| config.spec.name() == tool_name)
    }

    #[instrument(level = "trace", skip_all, err)]
    pub async fn build_tool_call(
        session: &Session,
        item: ResponseItem,
    ) -> Result<Option<ToolCall>, FunctionCallError> {
        match item {
            ResponseItem::FunctionCall {
                name,
                arguments,
                call_id,
                ..
            } => {
                if let Some((server, tool)) = session.parse_mcp_tool_name(&name).await {
                    Ok(Some(ToolCall {
                        tool_name: name,
                        call_id,
                        payload: ToolPayload::Mcp {
                            server,
                            tool,
                            raw_arguments: arguments,
                        },
                    }))
                } else {
                    Ok(Some(ToolCall {
                        tool_name: name,
                        call_id,
                        payload: ToolPayload::Function { arguments },
                    }))
                }
            }
            ResponseItem::CustomToolCall {
                name,
                input,
                call_id,
                ..
            } => Ok(Some(ToolCall {
                tool_name: name,
                call_id,
                payload: ToolPayload::Custom { input },
            })),
            ResponseItem::LocalShellCall {
                id,
                call_id,
                action,
                ..
            } => {
                let call_id = call_id
                    .or(id)
                    .ok_or(FunctionCallError::MissingLocalShellCallId)?;

                match action {
                    LocalShellAction::Exec(exec) => {
                        let params = ShellToolCallParams {
                            // Convert Vec<String> to space-separated String
                            command: exec.command.join(" "),
                            workdir: exec.working_directory,
                            timeout_ms: exec.timeout_ms,
                            with_escalated_permissions: None,
                            sandbox_permissions: Some(SandboxPermissions::UseDefault),
                            justification: None,
                            stdin: None,
                        };
                        Ok(Some(ToolCall {
                            tool_name: "local_shell".to_string(),
                            call_id,
                            payload: ToolPayload::LocalShell { params },
                        }))
                    }
                }
            }
            _ => Ok(None),
        }
    }

    #[instrument(level = "trace", skip_all, err)]
    pub async fn dispatch_tool_call(
        &self,
        session: Arc<Session>,
        turn: Arc<TurnContext>,
        tracker: SharedTurnDiffTracker,
        call: ToolCall,
    ) -> Result<ResponseInputItem, FunctionCallError> {
        let ToolCall {
            tool_name,
            call_id,
            payload,
        } = call;
        let payload_outputs_custom = matches!(payload, ToolPayload::Custom { .. });
        let failure_call_id = call_id.clone();

        let invocation = ToolInvocation {
            session,
            turn,
            tracker,
            call_id,
            tool_name,
            payload,
        };

        match self.registry.dispatch(invocation).await {
            Ok(response) => Ok(response),
            Err(FunctionCallError::Fatal(message)) => Err(FunctionCallError::Fatal(message)),
            Err(err) => Ok(Self::failure_response(
                failure_call_id,
                payload_outputs_custom,
                err,
            )),
        }
    }

    fn failure_response(
        call_id: String,
        payload_outputs_custom: bool,
        err: FunctionCallError,
    ) -> ResponseInputItem {
        let message = err.to_string();
        if payload_outputs_custom {
            ResponseInputItem::CustomToolCallOutput {
                call_id,
                output: message,
            }
        } else {
            ResponseInputItem::FunctionCallOutput {
                call_id,
                output: codex_protocol::models::FunctionCallOutputPayload {
                    content: message,
                    success: Some(false),
                    ..Default::default()
                },
            }
        }
    }
}
