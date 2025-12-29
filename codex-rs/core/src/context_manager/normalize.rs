use std::collections::HashSet;

use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;

use tracing::warn;

pub(crate) fn ensure_call_outputs_present(items: &mut Vec<ResponseItem>) {
    // Collect synthetic outputs to insert immediately after their calls.
    // Store the insertion position (index of call) alongside the item so
    // we can insert in reverse order and avoid index shifting.
    let mut missing_outputs_to_insert: Vec<(usize, ResponseItem)> = Vec::new();

    for (idx, item) in items.iter().enumerate() {
        match item {
            ResponseItem::FunctionCall { call_id, .. } => {
                let has_output = items.iter().any(|i| match i {
                    ResponseItem::FunctionCallOutput {
                        call_id: existing, ..
                    } => existing == call_id,
                    _ => false,
                });

                if !has_output {
                    // 这是正常的恢复流程（例如 SSE 超时重试），不应该 panic
                    // 只记录警告并插入占位符输出
                    warn!(
                        "Function call output is missing for call id: {call_id}, inserting placeholder"
                    );
                    missing_outputs_to_insert.push((
                        idx,
                        ResponseItem::FunctionCallOutput {
                            call_id: call_id.clone(),
                            output: FunctionCallOutputPayload {
                                content: "aborted (tool execution interrupted)".to_string(),
                                ..Default::default()
                            },
                        },
                    ));
                }
            }
            ResponseItem::CustomToolCall { call_id, .. } => {
                let has_output = items.iter().any(|i| match i {
                    ResponseItem::CustomToolCallOutput {
                        call_id: existing, ..
                    } => existing == call_id,
                    _ => false,
                });

                if !has_output {
                    warn!(
                        "Custom tool call output is missing for call id: {call_id}, inserting placeholder"
                    );
                    missing_outputs_to_insert.push((
                        idx,
                        ResponseItem::CustomToolCallOutput {
                            call_id: call_id.clone(),
                            output: "aborted (tool execution interrupted)".to_string(),
                        },
                    ));
                }
            }
            // LocalShellCall is represented in upstream streams by a FunctionCallOutput
            ResponseItem::LocalShellCall { call_id, .. } => {
                if let Some(call_id) = call_id.as_ref() {
                    let has_output = items.iter().any(|i| match i {
                        ResponseItem::FunctionCallOutput {
                            call_id: existing, ..
                        } => existing == call_id,
                        _ => false,
                    });

                    if !has_output {
                        warn!(
                            "Local shell call output is missing for call id: {call_id}, inserting placeholder"
                        );
                        missing_outputs_to_insert.push((
                            idx,
                            ResponseItem::FunctionCallOutput {
                                call_id: call_id.clone(),
                                output: FunctionCallOutputPayload {
                                    content: "aborted (tool execution interrupted)".to_string(),
                                    ..Default::default()
                                },
                            },
                        ));
                    }
                }
            }
            _ => {}
        }
    }

    // Insert synthetic outputs in reverse index order to avoid re-indexing.
    for (idx, output_item) in missing_outputs_to_insert.into_iter().rev() {
        items.insert(idx + 1, output_item);
    }
}

pub(crate) fn remove_orphan_outputs(items: &mut Vec<ResponseItem>) {
    let function_call_ids: HashSet<String> = items
        .iter()
        .filter_map(|i| match i {
            ResponseItem::FunctionCall { call_id, .. } => Some(call_id.clone()),
            _ => None,
        })
        .collect();

    let local_shell_call_ids: HashSet<String> = items
        .iter()
        .filter_map(|i| match i {
            ResponseItem::LocalShellCall {
                call_id: Some(call_id),
                ..
            } => Some(call_id.clone()),
            _ => None,
        })
        .collect();

    let custom_tool_call_ids: HashSet<String> = items
        .iter()
        .filter_map(|i| match i {
            ResponseItem::CustomToolCall { call_id, .. } => Some(call_id.clone()),
            _ => None,
        })
        .collect();

    items.retain(|item| match item {
        ResponseItem::FunctionCallOutput { call_id, .. } => {
            let has_match =
                function_call_ids.contains(call_id) || local_shell_call_ids.contains(call_id);
            if !has_match {
                // 这是正常的恢复流程，不应该 panic
                warn!(
                    "Orphan function call output for call id: {call_id}, removing"
                );
            }
            has_match
        }
        ResponseItem::CustomToolCallOutput { call_id, .. } => {
            let has_match = custom_tool_call_ids.contains(call_id);
            if !has_match {
                warn!(
                    "Orphan custom tool call output for call id: {call_id}, removing"
                );
            }
            has_match
        }
        _ => true,
    });
}

pub(crate) fn remove_corresponding_for(items: &mut Vec<ResponseItem>, item: &ResponseItem) {
    match item {
        ResponseItem::FunctionCall { call_id, .. } => {
            remove_first_matching(items, |i| {
                matches!(
                    i,
                    ResponseItem::FunctionCallOutput {
                        call_id: existing, ..
                    } if existing == call_id
                )
            });
        }
        ResponseItem::FunctionCallOutput { call_id, .. } => {
            if let Some(pos) = items.iter().position(|i| {
                matches!(i, ResponseItem::FunctionCall { call_id: existing, .. } if existing == call_id)
            }) {
                items.remove(pos);
            } else if let Some(pos) = items.iter().position(|i| {
                matches!(i, ResponseItem::LocalShellCall { call_id: Some(existing), .. } if existing == call_id)
            }) {
                items.remove(pos);
            }
        }
        ResponseItem::CustomToolCall { call_id, .. } => {
            remove_first_matching(items, |i| {
                matches!(
                    i,
                    ResponseItem::CustomToolCallOutput {
                        call_id: existing, ..
                    } if existing == call_id
                )
            });
        }
        ResponseItem::CustomToolCallOutput { call_id, .. } => {
            remove_first_matching(
                items,
                |i| matches!(i, ResponseItem::CustomToolCall { call_id: existing, .. } if existing == call_id),
            );
        }
        ResponseItem::LocalShellCall {
            call_id: Some(call_id),
            ..
        } => {
            remove_first_matching(items, |i| {
                matches!(
                    i,
                    ResponseItem::FunctionCallOutput {
                        call_id: existing, ..
                    } if existing == call_id
                )
            });
        }
        _ => {}
    }
}

fn remove_first_matching<F>(items: &mut Vec<ResponseItem>, predicate: F)
where
    F: Fn(&ResponseItem) -> bool,
{
    if let Some(pos) = items.iter().position(predicate) {
        items.remove(pos);
    }
}
