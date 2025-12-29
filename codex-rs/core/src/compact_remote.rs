use std::sync::Arc;

use crate::Prompt;
use crate::codex::Session;
use crate::codex::TurnContext;
use crate::error::Result as CodexResult;
use crate::protocol::CompactedItem;
use crate::protocol::ContextCompactedEvent;
use crate::protocol::EventMsg;
use crate::protocol::RolloutItem;
use crate::protocol::TaskStartedEvent;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;

pub(crate) async fn run_inline_remote_auto_compact_task(
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
) {
    run_remote_compact_task_inner(&sess, &turn_context).await;
}

pub(crate) async fn run_remote_compact_task(sess: Arc<Session>, turn_context: Arc<TurnContext>) {
    let start_event = EventMsg::TaskStarted(TaskStartedEvent {
        model_context_window: turn_context.client.get_model_context_window(),
    });
    sess.send_event(&turn_context, start_event).await;

    run_remote_compact_task_inner(&sess, &turn_context).await;
}

async fn run_remote_compact_task_inner(sess: &Arc<Session>, turn_context: &Arc<TurnContext>) {
    if let Err(err) = run_remote_compact_task_inner_impl(sess, turn_context).await {
        let event = EventMsg::Error(
            err.to_error_event(Some("Error running remote compact task".to_string())),
        );
        sess.send_event(turn_context, event).await;
    }
}

async fn run_remote_compact_task_inner_impl(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
) -> CodexResult<()> {
    let mut history = sess.clone_history().await;

    // 🔧 保存当前 plan 状态，在 compact 后恢复
    let current_plan = sess.get_current_plan().await;

    let prompt = Prompt {
        input: history.get_history_for_prompt(),
        tools: vec![],
        parallel_tool_calls: false,
        base_instructions_override: turn_context.base_instructions.clone(),
        output_schema: None,
        temperature: None,
        top_k: None,
        top_p: None,
        repetition_penalty: None,
        is_user_turn: false, // compact 是系统自动操作
    };

    let mut new_history = turn_context
        .client
        .compact_conversation_history(&prompt)
        .await?;

    // 🔧 如果有未完成的 plan，将 plan 状态注入到新历史中
    if let Some(ref plan) = current_plan {
        let pending_count = plan.plan.iter()
            .filter(|s| matches!(s.status, codex_protocol::plan_tool::StepStatus::Pending))
            .count();
        let in_progress_count = plan.plan.iter()
            .filter(|s| matches!(s.status, codex_protocol::plan_tool::StepStatus::InProgress))
            .count();
        let completed_count = plan.plan.iter()
            .filter(|s| matches!(s.status, codex_protocol::plan_tool::StepStatus::Completed))
            .count();

        if pending_count > 0 || in_progress_count > 0 {
            // 构建 plan 状态描述
            let plan_steps: Vec<String> = plan.plan.iter().map(|step| {
                let status_emoji = match step.status {
                    codex_protocol::plan_tool::StepStatus::Completed => "✅",
                    codex_protocol::plan_tool::StepStatus::InProgress => "🔄",
                    codex_protocol::plan_tool::StepStatus::Pending => "⏳",
                };
                format!("{} {}", status_emoji, step.step)
            }).collect();

            let plan_reminder = format!(
                "<system-reminder>\nYou have an active plan with {} steps ({} completed, {} in progress, {} pending).\n\nCurrent plan status:\n{}\n\nPlease continue executing the remaining steps. Do not restart or modify the plan unless necessary.\n</system-reminder>",
                plan.plan.len(),
                completed_count,
                in_progress_count,
                pending_count,
                plan_steps.join("\n")
            );

            new_history.push(ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputText { text: plan_reminder }],
            });

            tracing::info!(
                "📋 [remote_compact] 保留 plan 状态: {}/{} completed, {} in_progress, {} pending",
                completed_count, plan.plan.len(), in_progress_count, pending_count
            );
        }
    }

    // Required to keep `/undo` available after compaction
    let ghost_snapshots: Vec<ResponseItem> = history
        .get_history()
        .iter()
        .filter(|item| matches!(item, ResponseItem::GhostSnapshot { .. }))
        .cloned()
        .collect();

    if !ghost_snapshots.is_empty() {
        new_history.extend(ghost_snapshots);
    }
    sess.replace_history(new_history.clone()).await;
    sess.recompute_token_usage(turn_context).await;

    let compacted_item = CompactedItem {
        message: String::new(),
        replacement_history: Some(new_history),
    };
    sess.persist_rollout_items(&[RolloutItem::Compacted(compacted_item)])
        .await;

    let event = EventMsg::ContextCompacted(ContextCompactedEvent {});
    sess.send_event(turn_context, event).await;

    Ok(())
}
