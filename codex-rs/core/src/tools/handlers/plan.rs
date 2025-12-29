use crate::client_common::tools::ResponsesApiTool;
use crate::client_common::tools::ToolSpec;
use crate::codex::Session;
use crate::codex::TurnContext;
use crate::function_tool::FunctionCallError;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::registry::ToolHandler;
use crate::tools::registry::ToolKind;
use crate::tools::spec::JsonSchema;
use async_trait::async_trait;
use codex_protocol::plan_tool::StepStatus;
use codex_protocol::plan_tool::UpdatePlanArgs;
use codex_protocol::protocol::EventMsg;
use std::collections::BTreeMap;
use std::sync::LazyLock;

pub struct PlanHandler;

pub static PLAN_TOOL: LazyLock<ToolSpec> = LazyLock::new(|| {
    let mut plan_item_props = BTreeMap::new();
    plan_item_props.insert("step".to_string(), JsonSchema::String { description: None });
    plan_item_props.insert(
        "status".to_string(),
        JsonSchema::String {
            description: Some("One of: pending, in_progress, completed".to_string()),
        },
    );

    let plan_items_schema = JsonSchema::Array {
        description: Some("The list of steps".to_string()),
        items: Box::new(JsonSchema::Object {
            properties: plan_item_props,
            required: Some(vec!["step".to_string(), "status".to_string()]),
            additional_properties: Some(false.into()),
        }),
    };

    let mut properties = BTreeMap::new();
    properties.insert(
        "explanation".to_string(),
        JsonSchema::String { description: None },
    );
    properties.insert("plan".to_string(), plan_items_schema);

    ToolSpec::Function(ResponsesApiTool {
        name: "update_plan".to_string(),
        description: r#"Updates the task plan.
Provide an optional explanation and a list of plan items, each with a step and status.
At most one step can be in_progress at a time.
"#
        .to_string(),
        strict: false,
        parameters: JsonSchema::Object {
            properties,
            required: Some(vec!["plan".to_string()]),
            additional_properties: Some(false.into()),
        },
    })
});

#[async_trait]
impl ToolHandler for PlanHandler {
    fn kind(&self) -> ToolKind {
        ToolKind::Function
    }

    async fn handle(&self, invocation: ToolInvocation) -> Result<ToolOutput, FunctionCallError> {
        let ToolInvocation {
            session,
            turn,
            call_id,
            payload,
            ..
        } = invocation;

        let arguments = match payload {
            ToolPayload::Function { arguments } => arguments,
            _ => {
                return Err(FunctionCallError::RespondToModel(
                    "update_plan handler received unsupported payload".to_string(),
                ));
            }
        };

        let content =
            handle_update_plan(session.as_ref(), turn.as_ref(), arguments, call_id).await?;

        Ok(ToolOutput::Function {
            content,
            content_items: None,
            success: Some(true),
        })
    }
}

/// This function doesn't do anything useful. However, it gives the model a structured way to record its plan that clients can read and render.
/// So it's the _inputs_ to this function that are useful to clients, not the outputs and neither are actually useful for the model other
/// than forcing it to come up and document a plan (TBD how that affects performance).
pub(crate) async fn handle_update_plan(
    session: &Session,
    turn_context: &TurnContext,
    arguments: String,
    _call_id: String,
) -> Result<String, FunctionCallError> {
    let args = parse_update_plan_arguments(&arguments)?;

    // Count pending and in_progress steps
    let pending_count = args
        .plan
        .iter()
        .filter(|s| matches!(s.status, StepStatus::Pending))
        .count();
    let in_progress_count = args
        .plan
        .iter()
        .filter(|s| matches!(s.status, StepStatus::InProgress))
        .count();

    // 持久化 plan 状态到 Session，用于 compact 后恢复
    session.set_current_plan(args.clone()).await;

    session
        .send_event(turn_context, EventMsg::PlanUpdate(args))
        .await;

    // Return a message that reminds the model to continue executing pending steps
    if pending_count > 0 || in_progress_count > 0 {
        Ok(format!(
            "Plan updated. {pending_count} step(s) pending, {in_progress_count} step(s) in progress. Continue executing the remaining steps."
        ))
    } else {
        // 所有步骤完成时，清除 plan 状态
        session.clear_current_plan().await;
        Ok("Plan updated. All steps completed.".to_string())
    }
}

fn parse_update_plan_arguments(arguments: &str) -> Result<UpdatePlanArgs, FunctionCallError> {
    // 首先尝试标准 JSON 解析
    if let Ok(args) = serde_json::from_str::<UpdatePlanArgs>(arguments) {
        return Ok(args);
    }

    // 如果 JSON 解析失败，尝试解析 XML 风格的格式
    // 格式如：<tool_call>update_plan<arg_key>explanation</arg_key><arg_value>...</arg_value>...
    if arguments.contains("<arg_key>") && arguments.contains("<arg_value>") {
        tracing::warn!("检测到 XML 风格的工具调用格式，尝试解析...");

        let mut explanation: Option<String> = None;
        let mut plan: Vec<codex_protocol::plan_tool::PlanItemArg> = Vec::new();

        // 提取所有 <arg_key>...</arg_key><arg_value>...</arg_value> 对
        let mut remaining = arguments;
        while let Some(key_start) = remaining.find("<arg_key>") {
            let key_end = remaining.find("</arg_key>").unwrap_or(remaining.len());
            let key = &remaining[key_start + 9..key_end];

            let value_section = &remaining[key_end..];
            let value_start = value_section.find("<arg_value>").unwrap_or(0);
            let value_end = value_section.find("</arg_value>").unwrap_or(value_section.len());
            let value = if value_start > 0 && value_end > value_start {
                &value_section[value_start + 11..value_end]
            } else {
                ""
            };

            match key {
                "explanation" => {
                    explanation = Some(value.to_string());
                }
                "plan" => {
                    // 尝试解析 plan 数组
                    if let Ok(parsed_plan) = serde_json::from_str::<Vec<codex_protocol::plan_tool::PlanItemArg>>(value) {
                        plan = parsed_plan;
                    } else {
                        tracing::warn!("无法解析 plan 值: {}", value);
                    }
                }
                _ => {
                    tracing::debug!("忽略未知的 arg_key: {}", key);
                }
            }

            // 移动到下一个 arg_key
            if let Some(next_key) = value_section.find("<arg_key>") {
                remaining = &value_section[next_key..];
            } else {
                break;
            }
        }

        if !plan.is_empty() {
            return Ok(UpdatePlanArgs { explanation, plan });
        }
    }

    // 如果两种格式都无法解析，返回错误
    Err(FunctionCallError::RespondToModel(format!(
        "failed to parse function arguments (expected JSON or XML format): {}",
        if arguments.len() > 200 {
            format!("{}...", &arguments[..200])
        } else {
            arguments.to_string()
        }
    )))
}
