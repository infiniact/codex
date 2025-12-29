use crate::error::ApiError;
use crate::provider::Provider;
use crate::requests::headers::build_conversation_headers;
use crate::requests::headers::insert_header;
use crate::requests::headers::subagent_header;
use crate::turn_signing::TurnSignature;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::ReasoningItemContent;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::SessionSource;
use http::HeaderMap;
use serde_json::Value;
use serde_json::json;
use std::collections::HashMap;

/// Assembled request body plus headers for Chat Completions streaming calls.
pub struct ChatRequest {
    pub body: Value,
    pub headers: HeaderMap,
}

pub struct ChatRequestBuilder<'a> {
    model: &'a str,
    instructions: &'a str,
    input: &'a [ResponseItem],
    tools: &'a [Value],
    conversation_id: Option<String>,
    session_source: Option<SessionSource>,
    /// 是否为用户主动发送（用于服务端统计）
    is_user_turn: bool,
}

impl<'a> ChatRequestBuilder<'a> {
    pub fn new(
        model: &'a str,
        instructions: &'a str,
        input: &'a [ResponseItem],
        tools: &'a [Value],
    ) -> Self {
        Self {
            model,
            instructions,
            input,
            tools,
            conversation_id: None,
            session_source: None,
            is_user_turn: true, // 默认为用户主动发送
        }
    }

    pub fn conversation_id(mut self, id: Option<String>) -> Self {
        self.conversation_id = id;
        self
    }

    pub fn session_source(mut self, source: Option<SessionSource>) -> Self {
        self.session_source = source;
        self
    }

    /// 设置是否为用户主动发送
    pub fn is_user_turn(mut self, value: bool) -> Self {
        self.is_user_turn = value;
        self
    }

    pub fn build(self, _provider: &Provider) -> Result<ChatRequest, ApiError> {
        let mut messages = Vec::<Value>::new();
        messages.push(json!({"role": "system", "content": self.instructions}));

        let input = self.input;

        // 预扫描：收集所有有对应 FunctionCallOutput 的 call_id
        // 这样我们可以确保每个 tool_calls 都有对应的 tool 响应
        let call_ids_with_output: std::collections::HashSet<String> = input
            .iter()
            .filter_map(|item| {
                if let ResponseItem::FunctionCallOutput { call_id, .. } = item {
                    Some(call_id.clone())
                } else if let ResponseItem::CustomToolCallOutput { call_id, .. } = item {
                    Some(call_id.clone())
                } else {
                    None
                }
            })
            .collect();

        let mut reasoning_by_anchor_index: HashMap<usize, String> = HashMap::new();
        let mut last_emitted_role: Option<&str> = None;
        for item in input {
            match item {
                ResponseItem::Message { role, .. } => last_emitted_role = Some(role.as_str()),
                ResponseItem::FunctionCall { .. } | ResponseItem::LocalShellCall { .. } => {
                    last_emitted_role = Some("assistant")
                }
                ResponseItem::FunctionCallOutput { .. } => last_emitted_role = Some("tool"),
                ResponseItem::Reasoning { .. } | ResponseItem::Other => {}
                ResponseItem::CustomToolCall { .. } => {}
                ResponseItem::CustomToolCallOutput { .. } => {}
                ResponseItem::WebSearchCall { .. } => {}
                ResponseItem::GhostSnapshot { .. } => {}
                ResponseItem::CompactionSummary { .. } => {}
            }
        }

        let mut last_user_index: Option<usize> = None;
        for (idx, item) in input.iter().enumerate() {
            if let ResponseItem::Message { role, .. } = item
                && role == "user"
            {
                last_user_index = Some(idx);
            }
        }

        if !matches!(last_emitted_role, Some("user")) {
            for (idx, item) in input.iter().enumerate() {
                if let Some(u_idx) = last_user_index
                    && idx <= u_idx
                {
                    continue;
                }

                if let ResponseItem::Reasoning {
                    content: Some(items),
                    ..
                } = item
                {
                    let mut text = String::new();
                    for entry in items {
                        match entry {
                            ReasoningItemContent::ReasoningText { text: segment }
                            | ReasoningItemContent::Text { text: segment } => {
                                text.push_str(segment)
                            }
                        }
                    }
                    if text.trim().is_empty() {
                        continue;
                    }

                    let mut attached = false;
                    if idx > 0
                        && let ResponseItem::Message { role, .. } = &input[idx - 1]
                        && role == "assistant"
                    {
                        reasoning_by_anchor_index
                            .entry(idx - 1)
                            .and_modify(|v| v.push_str(&text))
                            .or_insert(text.clone());
                        attached = true;
                    }

                    if !attached && idx + 1 < input.len() {
                        match &input[idx + 1] {
                            ResponseItem::FunctionCall { .. }
                            | ResponseItem::LocalShellCall { .. } => {
                                reasoning_by_anchor_index
                                    .entry(idx + 1)
                                    .and_modify(|v| v.push_str(&text))
                                    .or_insert(text.clone());
                            }
                            ResponseItem::Message { role, .. } if role == "assistant" => {
                                reasoning_by_anchor_index
                                    .entry(idx + 1)
                                    .and_modify(|v| v.push_str(&text))
                                    .or_insert(text.clone());
                            }
                            _ => {}
                        }
                    }
                }
            }
        }

        let mut last_assistant_text: Option<String> = None;

        for (idx, item) in input.iter().enumerate() {
            match item {
                ResponseItem::Message { role, content, .. } => {
                    let mut text = String::new();
                    let mut items: Vec<Value> = Vec::new();
                    let mut saw_image = false;

                    for c in content {
                        match c {
                            ContentItem::InputText { text: t }
                            | ContentItem::OutputText { text: t } => {
                                text.push_str(t);
                                items.push(json!({"type":"text","text": t}));
                            }
                            ContentItem::InputImage { image_url } => {
                                saw_image = true;
                                items.push(
                                    json!({"type":"image_url","image_url": {"url": image_url}}),
                                );
                            }
                        }
                    }

                    if role == "assistant" {
                        if let Some(prev) = &last_assistant_text
                            && prev == &text
                        {
                            continue;
                        }
                        last_assistant_text = Some(text.clone());
                    }

                    let content_value = if role == "assistant" {
                        json!(text)
                    } else if saw_image {
                        json!(items)
                    } else {
                        json!(text)
                    };

                    let mut msg = json!({"role": role, "content": content_value});
                    if role == "assistant"
                        && let Some(reasoning) = reasoning_by_anchor_index.get(&idx)
                        && let Some(obj) = msg.as_object_mut()
                    {
                        obj.insert("reasoning".to_string(), json!(reasoning));
                    }
                    messages.push(msg);
                }
                ResponseItem::FunctionCall {
                    name,
                    arguments,
                    call_id,
                    thought_signature,
                    ..
                } => {
                    // 尝试将 arguments 字符串解析为 JSON 对象
                    // 某些 API（如 Anthropic、Gemini）期望 arguments 是对象而不是字符串
                    // OpenAI 兼容 API 通常接受字符串格式
                    let arguments_value: Value = serde_json::from_str(arguments)
                        .unwrap_or_else(|_| json!(arguments));

                    // 检查这个 FunctionCall 是否有对应的 FunctionCallOutput
                    // 如果没有，则转换为文本消息，避免 OpenRouter 报错
                    // "insufficient tool messages following tool_calls message"
                    if !call_ids_with_output.contains(call_id) {
                        let description = format!(
                            "[Tool Call: {}]\nArguments: {}\nCall ID: {}\n(No output recorded)",
                            name,
                            serde_json::to_string_pretty(&arguments_value).unwrap_or_else(|_| arguments.clone()),
                            call_id
                        );
                        let mut msg = json!({
                            "role": "assistant",
                            "content": description
                        });
                        if let Some(reasoning) = reasoning_by_anchor_index.get(&idx)
                            && let Some(obj) = msg.as_object_mut()
                        {
                            obj.insert("reasoning".to_string(), json!(reasoning));
                        }
                        messages.push(msg);
                        continue;
                    }

                    let function_obj = json!({
                        "name": name,
                        "arguments": arguments_value,
                    });

                    // 🔧 修复：thought_signature 应该放在 tool_call 级别，而不是 function 级别
                    // 参考：https://openrouter.ai/docs/guides/best-practices/reasoning-tokens
                    let mut tool_call_obj = json!({
                        "id": call_id,
                        "type": "function",
                        "function": function_obj,
                    });

                    // Add thought_signature at tool_call level (NOT inside function object)
                    if let Some(sig) = &thought_signature {
                        if let Some(obj) = tool_call_obj.as_object_mut() {
                            obj.insert("thought_signature".to_string(), json!(sig));
                        }
                        tracing::warn!(
                            "🧠 [ChatRequestBuilder::build] 添加 thought_signature 到 tool_call: call_id={}, sig_len={}",
                            call_id,
                            sig.len()
                        );
                    }

                    let mut msg = json!({
                        "role": "assistant",
                        "content": null,
                        "tool_calls": [tool_call_obj]
                    });

                    // 🆕 为 OpenRouter/Gemini 添加 reasoning_details（在 message 级别）
                    // 格式: "reasoning_details":[{"id":"tool_xxx", "type":"reasoning.encrypted", "data":"...", "format":"google-gemini-v1"}]
                    if let Some(sig) = &thought_signature
                        && let Some(obj) = msg.as_object_mut()
                    {
                        obj.insert("reasoning_details".to_string(), json!([{
                            "id": call_id,
                            "type": "reasoning.encrypted",
                            "data": sig,
                            "format": "google-gemini-v1"
                        }]));
                        tracing::debug!(
                            "🧠 [ChatRequestBuilder::build] 添加 reasoning_details: call_id={}, sig_len={}",
                            call_id,
                            sig.len()
                        );
                    }

                    if let Some(reasoning) = reasoning_by_anchor_index.get(&idx)
                        && let Some(obj) = msg.as_object_mut()
                    {
                        obj.insert("reasoning".to_string(), json!(reasoning));
                    }
                    messages.push(msg);
                }
                ResponseItem::LocalShellCall {
                    id,
                    call_id: _,
                    status,
                    action,
                } => {
                    // LocalShellCall 没有对应的 tool 响应消息，所以不能作为 tool_calls 发送
                    // 否则会导致 OpenRouter 报错: "insufficient tool messages following tool_calls message"
                    // 将其转换为普通的 assistant 文本消息
                    let action_str = serde_json::to_string_pretty(action).unwrap_or_default();
                    let content = format!(
                        "[Local Shell Call]\nID: {}\nStatus: {:?}\nAction: {}",
                        id.clone().unwrap_or_default(),
                        status,
                        action_str
                    );
                    let mut msg = json!({
                        "role": "assistant",
                        "content": content
                    });
                    if let Some(reasoning) = reasoning_by_anchor_index.get(&idx)
                        && let Some(obj) = msg.as_object_mut()
                    {
                        obj.insert("reasoning".to_string(), json!(reasoning));
                    }
                    messages.push(msg);
                }
                ResponseItem::FunctionCallOutput { call_id, output } => {
                    let content_value = if let Some(items) = &output.content_items {
                        let mapped: Vec<Value> = items
                            .iter()
                            .map(|it| match it {
                                FunctionCallOutputContentItem::InputText { text } => {
                                    json!({"type":"text","text": text})
                                }
                                FunctionCallOutputContentItem::InputImage { image_url } => {
                                    json!({"type":"image_url","image_url": {"url": image_url}})
                                }
                            })
                            .collect();
                        json!(mapped)
                    } else {
                        json!(output.content)
                    };

                    messages.push(json!({
                        "role": "tool",
                        "tool_call_id": call_id,
                        "content": content_value,
                    }));
                }
                ResponseItem::CustomToolCall {
                    id,
                    call_id: _,
                    name,
                    input,
                    status: _,
                } => {
                    // 检查这个 CustomToolCall 是否有对应的 CustomToolCallOutput
                    // 注意：CustomToolCallOutput 使用 call_id，而 CustomToolCall 的 id 字段是对应的
                    let call_id_str = id.clone().unwrap_or_default();
                    if !call_ids_with_output.contains(&call_id_str) {
                        // 没有对应的输出，转换为文本消息
                        let input_str = serde_json::to_string_pretty(input).unwrap_or_default();
                        let description = format!(
                            "[Custom Tool Call: {name}]\nInput: {input_str}\nCall ID: {call_id_str}\n(No output recorded)"
                        );
                        messages.push(json!({
                            "role": "assistant",
                            "content": description
                        }));
                        continue;
                    }

                    // CustomToolCall 使用标准的 function 类型，而不是 custom 类型
                    // 因为 OpenRouter/OpenAI 只识别 function 类型的 tool_calls
                    let input_str = serde_json::to_string(input).unwrap_or_default();
                    messages.push(json!({
                        "role": "assistant",
                        "content": null,
                        "tool_calls": [{
                            "id": id,
                            "type": "function",
                            "function": {
                                "name": name,
                                "arguments": input_str,
                            }
                        }]
                    }));
                }
                ResponseItem::CustomToolCallOutput { call_id, output } => {
                    messages.push(json!({
                        "role": "tool",
                        "tool_call_id": call_id,
                        "content": output,
                    }));
                }
                ResponseItem::GhostSnapshot { .. } => {
                    continue;
                }
                ResponseItem::Reasoning { .. }
                | ResponseItem::WebSearchCall { .. }
                | ResponseItem::Other
                | ResponseItem::CompactionSummary { .. } => {
                    continue;
                }
            }
        }

        // 检查是否有 user 消息
        // 智谱 GLM API 要求 messages 中必须包含至少一条 user 角色的消息
        let has_user_message = messages.iter().any(|m| {
            m.get("role").and_then(|r| r.as_str()) == Some("user")
        });

        if !has_user_message {
            tracing::warn!(
                "⚠️ [ChatRequestBuilder::build] messages 中没有 user 消息，GLM API 可能会报错 1213"
            );
            // 添加一条空的 user 消息，防止 GLM API 报错
            // 注意：这是一个临时解决方案，真正的问题应该在上层解决
            messages.push(json!({"role": "user", "content": "请继续"}));
            tracing::warn!(
                "⚠️ [ChatRequestBuilder::build] 已添加默认 user 消息"
            );
        }

        // 构建基础 payload
        let mut payload = json!({
            "model": self.model,
            "messages": messages,
            "stream": true,
        });

        // 只有当 tools 非空时才添加 tools 参数
        // 智谱 GLM API 等不兼容空 tools 数组，会导致 1213 错误
        if !self.tools.is_empty() {
            payload["tools"] = json!(self.tools);
        }

        // 🆕 为 Gemini 模型启用推理功能（OpenRouter 需要）
        // 检测是否为 Gemini 3 模型（需要 thought_signature 支持）
        let is_gemini_3 = self.model.contains("gemini-3")
            || self.model.contains("gemini/gemini-3")
            || self.model.contains("google/gemini-3");
        if is_gemini_3 {
            // OpenRouter 需要 reasoning 参数来启用 Gemini 的推理功能
            // 参考: https://openrouter.ai/docs/guides/best-practices/reasoning-tokens
            payload["reasoning"] = json!({
                "enabled": true
            });
            // 同时添加 stream_options 以包含推理细节
            payload["stream_options"] = json!({
                "include_usage": true,
                "include_reasoning": true
            });
            tracing::warn!(
                "🧠 [ChatRequestBuilder::build] Gemini 3 模型检测到，已启用 reasoning 功能"
            );
        }

        // 🔍 DEBUG: 打印构建的请求体
        tracing::debug!(
            "📤 [ChatRequestBuilder::build] model={}, messages_count={}, tools_count={}, has_tools_in_payload={}",
            self.model,
            messages.len(),
            self.tools.len(),
            payload.get("tools").is_some()
        );

        let mut headers = build_conversation_headers(self.conversation_id.clone());
        if let Some(subagent) = subagent_header(&self.session_source) {
            insert_header(&mut headers, "x-openai-subagent", &subagent);
        }

        // 🔢 添加 is_user_turn 签名 header
        if let Some(ref conv_id) = self.conversation_id {
            let signature = TurnSignature::sign(conv_id, self.is_user_turn);
            insert_header(&mut headers, "x-iaterm-turn", signature.turn_value());
            insert_header(&mut headers, "x-iaterm-turn-timestamp", &signature.timestamp.to_string());
            insert_header(&mut headers, "x-iaterm-turn-signature", &signature.signature);
            tracing::debug!(
                "🔢 [ChatRequestBuilder::build] is_user_turn={}, turn={}, conv_id={}",
                self.is_user_turn,
                signature.turn_value(),
                conv_id
            );
        }

        Ok(ChatRequest {
            body: payload,
            headers,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::RetryConfig;
    use crate::provider::WireApi;
    use codex_protocol::protocol::SessionSource;
    use codex_protocol::protocol::SubAgentSource;
    use http::HeaderValue;
    use pretty_assertions::assert_eq;
    use std::time::Duration;

    fn provider() -> Provider {
        Provider {
            name: "openai".to_string(),
            base_url: "https://api.openai.com/v1".to_string(),
            query_params: None,
            wire: WireApi::Chat,
            headers: HeaderMap::new(),
            retry: RetryConfig {
                max_attempts: 1,
                base_delay: Duration::from_millis(10),
                retry_429: false,
                retry_5xx: true,
                retry_transport: true,
            },
            stream_idle_timeout: Duration::from_secs(1),
        }
    }

    #[test]
    fn attaches_conversation_and_subagent_headers() {
        let prompt_input = vec![ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "hi".to_string(),
            }],
        }];
        let req = ChatRequestBuilder::new("gpt-test", "inst", &prompt_input, &[])
            .conversation_id(Some("conv-1".into()))
            .session_source(Some(SessionSource::SubAgent(SubAgentSource::Review)))
            .build(&provider())
            .expect("request");

        assert_eq!(
            req.headers.get("conversation_id"),
            Some(&HeaderValue::from_static("conv-1"))
        );
        assert_eq!(
            req.headers.get("session_id"),
            Some(&HeaderValue::from_static("conv-1"))
        );
        assert_eq!(
            req.headers.get("x-openai-subagent"),
            Some(&HeaderValue::from_static("review"))
        );
    }
}
