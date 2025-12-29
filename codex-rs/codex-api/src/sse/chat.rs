use crate::common::ResponseEvent;
use crate::common::ResponseStream;
use crate::error::ApiError;
use crate::telemetry::SseTelemetry;
use codex_client::StreamResponse;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ReasoningItemContent;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::TokenUsage;
use eventsource_stream::Eventsource;
use futures::Stream;
use futures::StreamExt;
use regex_lite::Regex;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::LazyLock;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio::time::timeout;
use tracing::debug;
use tracing::trace;
use tracing::warn;

/// 解析的 XML tool_call 结构
#[derive(Debug, Clone)]
struct XmlToolCall {
    function_name: String,
    parameters: HashMap<String, String>,
}

/// 解析 XML 格式的 tool_call
/// 格式: <tool_call><function=name><parameter=key>value</parameter>...</function></tool_call>
fn parse_xml_tool_call(text: &str) -> Option<XmlToolCall> {
    // 匹配 <tool_call>...</tool_call> 块
    static TOOL_CALL_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?s)<tool_call>\s*(.+?)\s*</tool_call>").unwrap_or_else(|e| {
            eprintln!("[codex-api] Failed to compile TOOL_CALL_RE: {e}");
            Regex::new(r"^\x00$").unwrap_or_else(|_| panic!("fallback regex should compile"))
        })
    });

    // 匹配 <function=name>...</function> 块
    static FUNCTION_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?s)<function=([^>]+)>\s*(.+?)\s*</function>").unwrap_or_else(|e| {
            eprintln!("[codex-api] Failed to compile FUNCTION_RE: {e}");
            Regex::new(r"^\x00$").unwrap_or_else(|_| panic!("fallback regex should compile"))
        })
    });

    // 匹配 <parameter=key>value</parameter>
    static PARAM_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?s)<parameter=([^>]+)>(.+?)</parameter>").unwrap_or_else(|e| {
            eprintln!("[codex-api] Failed to compile PARAM_RE: {e}");
            Regex::new(r"^\x00$").unwrap_or_else(|_| panic!("fallback regex should compile"))
        })
    });

    let tool_call_match = TOOL_CALL_RE.captures(text)?;
    let tool_call_content = tool_call_match.get(1)?.as_str();

    let function_match = FUNCTION_RE.captures(tool_call_content)?;
    let function_name = function_match.get(1)?.as_str().to_string();
    let function_content = function_match.get(2)?.as_str();

    let mut parameters = HashMap::new();
    for param_cap in PARAM_RE.captures_iter(function_content) {
        let key = param_cap.get(1).map(|m| m.as_str().to_string()).unwrap_or_default();
        let value = param_cap.get(2).map(|m| m.as_str().to_string()).unwrap_or_default();
        if !key.is_empty() {
            parameters.insert(key, value);
        }
    }

    Some(XmlToolCall {
        function_name,
        parameters,
    })
}

/// 检查文本是否包含 XML 格式的 tool_call
fn contains_xml_tool_call(text: &str) -> bool {
    text.contains("<tool_call>") && text.contains("</tool_call>")
}

/// Parse usage information from a Chat Completions API response chunk.
/// Returns a TokenUsage if the chunk contains usage data.
fn parse_usage_from_chunk(chunk: &serde_json::Value) -> Option<TokenUsage> {
    let usage = chunk.get("usage")?;

    let input_tokens = usage
        .get("prompt_tokens")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(0);

    let output_tokens = usage
        .get("completion_tokens")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(0);

    let total_tokens = usage
        .get("total_tokens")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(input_tokens + output_tokens);

    // Some providers may include prompt_tokens_details with cached_tokens
    let cached_input_tokens = usage
        .get("prompt_tokens_details")
        .and_then(|details| details.get("cached_tokens"))
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(0);

    // Some providers may include completion_tokens_details with reasoning_tokens
    let reasoning_output_tokens = usage
        .get("completion_tokens_details")
        .and_then(|details| details.get("reasoning_tokens"))
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(0);

    Some(TokenUsage {
        input_tokens,
        cached_input_tokens,
        output_tokens,
        reasoning_output_tokens,
        total_tokens,
    })
}

pub(crate) fn spawn_chat_stream(
    stream_response: StreamResponse,
    idle_timeout: Duration,
    telemetry: Option<std::sync::Arc<dyn SseTelemetry>>,
) -> ResponseStream {
    let (tx_event, rx_event) = mpsc::unbounded_channel::<Result<ResponseEvent, ApiError>>();
    tokio::spawn(async move {
        process_chat_sse(stream_response.bytes, tx_event, idle_timeout, telemetry).await;
    });
    ResponseStream { rx_event }
}

pub async fn process_chat_sse<S>(
    stream: S,
    tx_event: mpsc::UnboundedSender<Result<ResponseEvent, ApiError>>,
    idle_timeout: Duration,
    telemetry: Option<std::sync::Arc<dyn SseTelemetry>>,
) where
    S: Stream<Item = Result<bytes::Bytes, codex_client::TransportError>> + Unpin,
{
    let mut stream = stream.eventsource();

    #[derive(Default, Debug)]
    struct ToolCallState {
        id: Option<String>,
        name: Option<String>,
        arguments: String,
        /// Gemini 3 thought signature for preserving reasoning state
        thought_signature: Option<String>,
    }

    let mut tool_calls: HashMap<usize, ToolCallState> = HashMap::new();
    let mut tool_call_order: Vec<usize> = Vec::new();
    let mut tool_call_order_seen: HashSet<usize> = HashSet::new();
    let mut tool_call_index_by_id: HashMap<String, usize> = HashMap::new();
    // 🆕 存储 OpenRouter/Gemini 的 reasoning_details（id -> data 映射）
    let mut reasoning_details_by_id: HashMap<String, String> = HashMap::new();
    let mut next_tool_call_index = 0usize;
    let mut last_tool_call_index: Option<usize> = None;
    let mut assistant_item: Option<ResponseItem> = None;
    let mut reasoning_item: Option<ResponseItem> = None;
    let completed_sent = false;
    let mut accumulated_usage: Option<TokenUsage> = None;

    // 🆕 XML tool_call 解析状态
    let mut xml_tool_call_buffer = String::new();
    let mut pending_xml_tool_calls: Vec<XmlToolCall> = Vec::new();
    let mut xml_tool_call_counter = 0usize;

    // 🔍 诊断计数器
    let mut event_count = 0u64;
    let mut content_delta_count = 0u64;
    let mut last_event_data: Option<String> = None;
    let stream_start = Instant::now();

    warn!("📥 [process_chat_sse] 开始处理 SSE 流, idle_timeout={:?}", idle_timeout);

    loop {
        let start = Instant::now();
        let response = timeout(idle_timeout, stream.next()).await;
        if let Some(t) = telemetry.as_ref() {
            t.on_sse_poll(&response, start.elapsed());
        }
        let sse = match response {
            Ok(Some(Ok(sse))) => {
                event_count += 1;
                last_event_data = Some(sse.data.clone());
                sse
            }
            Ok(Some(Err(e))) => {
                warn!(
                    "❌ [process_chat_sse] SSE 解析错误: {}, 已处理事件数={}, 流运行时间={:?}, 最后事件={:?}",
                    e, event_count, stream_start.elapsed(), last_event_data
                );
                let _ = tx_event.send(Err(ApiError::Stream(e.to_string())));
                return;
            }
            Ok(None) => {
                warn!(
                    "📥 [process_chat_sse] SSE 流结束, completed_sent={}, 事件数={}, content_delta数={}, 流运行时间={:?}, 最后事件={:?}",
                    completed_sent, event_count, content_delta_count, stream_start.elapsed(), last_event_data
                );
                // 确保在流结束时发送所有待处理的 items
                // 使用 take() 确保每个 item 只发送一次
                if let Some(reasoning) = reasoning_item.take() {
                    debug!("📤 [process_chat_sse] 流结束 - 发送 OutputItemDone(Reasoning)");
                    let _ = tx_event.send(Ok(ResponseEvent::OutputItemDone(reasoning)));
                }

                // 🆕 处理待处理的 XML tool_call
                for xml_tc in pending_xml_tool_calls.drain(..) {
                    xml_tool_call_counter += 1;
                    let call_id = format!("xml-tool-call-{xml_tool_call_counter}");

                    // 将 parameters 转换为 JSON 字符串
                    let arguments = serde_json::to_string(&xml_tc.parameters).unwrap_or_else(|_| "{}".to_string());

                    warn!(
                        "📤 [process_chat_sse] 流结束 - 发送 XML FunctionCall: name={}, call_id={}, args_len={}",
                        xml_tc.function_name,
                        call_id,
                        arguments.len()
                    );

                    let item = ResponseItem::FunctionCall {
                        id: None,
                        name: xml_tc.function_name,
                        arguments,
                        call_id,
                        thought_signature: None,
                    };
                    let _ = tx_event.send(Ok(ResponseEvent::OutputItemDone(item)));
                }

                if let Some(assistant) = assistant_item.take() {
                    debug!("📤 [process_chat_sse] 流结束 - 发送 OutputItemDone(Message)");
                    let _ = tx_event.send(Ok(ResponseEvent::OutputItemDone(assistant)));
                }
                // 确保总是发送 Completed 事件
                if !completed_sent {
                    debug!("📤 [process_chat_sse] 流结束 - 发送 Completed 事件");
                    let _ = tx_event.send(Ok(ResponseEvent::Completed {
                        response_id: String::new(),
                        token_usage: accumulated_usage.clone(),
                    }));
                } else {
                    debug!("📤 [process_chat_sse] 流结束 - Completed 事件已发送，跳过重复发送");
                }
                return;
            }
            Err(_) => {
                warn!(
                    "⏰ [process_chat_sse] SSE 空闲超时, 事件数={}, content_delta数={}, 流运行时间={:?}, 最后事件={:?}",
                    event_count, content_delta_count, stream_start.elapsed(), last_event_data
                );
                let _ = tx_event.send(Err(ApiError::Stream("idle timeout waiting for SSE".into())));
                return;
            }
        };

        trace!("SSE event: {}", sse.data);

        if sse.data.trim().is_empty() {
            continue;
        }

        // 处理 OpenAI 标准的 [DONE] 消息，表示流结束
        if sse.data.trim() == "[DONE]" {
            warn!("📥 [process_chat_sse] 收到 [DONE] 消息, 事件数={}, 流运行时间={:?}", event_count, stream_start.elapsed());
            if let Some(reasoning) = reasoning_item.take() {
                debug!("📤 [process_chat_sse] [DONE] 发送 OutputItemDone(Reasoning)");
                let _ = tx_event.send(Ok(ResponseEvent::OutputItemDone(reasoning)));
            }

            // 🆕 处理待处理的 XML tool_call
            for xml_tc in pending_xml_tool_calls.drain(..) {
                xml_tool_call_counter += 1;
                let call_id = format!("xml-tool-call-{xml_tool_call_counter}");

                // 将 parameters 转换为 JSON 字符串
                let arguments = serde_json::to_string(&xml_tc.parameters).unwrap_or_else(|_| "{}".to_string());

                warn!(
                    "📤 [process_chat_sse] [DONE] 发送 XML FunctionCall: name={}, call_id={}, args_len={}",
                    xml_tc.function_name,
                    call_id,
                    arguments.len()
                );

                let item = ResponseItem::FunctionCall {
                    id: None,
                    name: xml_tc.function_name,
                    arguments,
                    call_id,
                    thought_signature: None,
                };
                let _ = tx_event.send(Ok(ResponseEvent::OutputItemDone(item)));
            }

            if let Some(assistant) = assistant_item.take() {
                debug!("📤 [process_chat_sse] [DONE] 发送 OutputItemDone(Message)");
                let _ = tx_event.send(Ok(ResponseEvent::OutputItemDone(assistant)));
            }
            if !completed_sent {
                debug!("📤 [process_chat_sse] [DONE] 发送 Completed 事件");
                let _ = tx_event.send(Ok(ResponseEvent::Completed {
                    response_id: String::new(),
                    token_usage: accumulated_usage.clone(),
                }));
            }
            return;
        }

        let value: serde_json::Value = match serde_json::from_str(&sse.data) {
            Ok(val) => val,
            Err(err) => {
                debug!(
                    "Failed to parse ChatCompletions SSE event: {err}, data: {}",
                    &sse.data
                );
                continue;
            }
        };

        // Extract usage information if present (typically in the last chunk)
        if let Some(usage) = parse_usage_from_chunk(&value) {
            accumulated_usage = Some(usage);
        }

        // 🆕 尝试检测并处理直接序列化的 ResponseItem（非标准格式）
        // 某些 API 提供商可能直接返回 {"type": "reasoning", ...} 而不是标准的 Chat Completions 格式
        if let Some(item_type) = value.get("type").and_then(|t| t.as_str()) {
            match item_type {
                "reasoning" => {
                    // 尝试解析为 ResponseItem::Reasoning
                    if let Ok(item) = serde_json::from_value::<ResponseItem>(value.clone()) {
                        warn!(
                            "📥 [process_chat_sse] 检测到非标准格式的 Reasoning 响应，尝试直接处理"
                        );
                        debug!("📤 [process_chat_sse] 发送 OutputItemDone(Reasoning)");
                        let _ = tx_event.send(Ok(ResponseEvent::OutputItemDone(item)));

                        // 这可能是唯一的响应，发送完成事件
                        if !completed_sent {
                            debug!("📤 [process_chat_sse] 非标准格式后发送 Completed 事件");
                            let _ = tx_event.send(Ok(ResponseEvent::Completed {
                                response_id: String::new(),
                                token_usage: accumulated_usage.clone(),
                            }));
                        }
                        return;
                    }
                }
                "message" => {
                    // 尝试解析为 ResponseItem::Message
                    if let Ok(item) = serde_json::from_value::<ResponseItem>(value.clone()) {
                        warn!(
                            "📥 [process_chat_sse] 检测到非标准格式的 Message 响应，尝试直接处理"
                        );
                        debug!("📤 [process_chat_sse] 发送 OutputItemDone(Message)");
                        let _ = tx_event.send(Ok(ResponseEvent::OutputItemDone(item)));

                        if !completed_sent {
                            debug!("📤 [process_chat_sse] 非标准格式后发送 Completed 事件");
                            let _ = tx_event.send(Ok(ResponseEvent::Completed {
                                response_id: String::new(),
                                token_usage: accumulated_usage.clone(),
                            }));
                        }
                        return;
                    }
                }
                _ => {
                    // 其他类型，继续标准处理
                }
            }
        }

        let Some(choices) = value.get("choices").and_then(|c| c.as_array()) else {
            debug!("⚠️ [process_chat_sse] SSE 事件缺少 'choices' 字段，跳过");
            continue;
        };

        for choice in choices {
            // 🔍 DEBUG: 打印完整的 choice 数据，用于调试 thought_signature 位置
            if choice.get("delta").and_then(|d| d.get("tool_calls")).is_some() {
                warn!(
                    "🔧 [process_chat_sse] choice 完整数据 (包含 tool_calls): {}",
                    serde_json::to_string(choice).unwrap_or_else(|_| "序列化失败".to_string())
                );
            }

            // 🆕 解析 OpenRouter/Gemini 的 reasoning_details
            // 格式: "reasoning_details":[{"id":"tool_xxx", "type":"reasoning.encrypted", "data":"...", "format":"google-gemini-v1"}]
            // 注意：在流式响应中，reasoning_details 可能在 choice 级别或 delta 级别
            let reasoning_details_sources: Vec<Option<&serde_json::Value>> = vec![
                choice.get("reasoning_details"),
                choice.get("delta").and_then(|d| d.get("reasoning_details")),
            ];

            for reasoning_details_opt in reasoning_details_sources.into_iter().flatten() {
                if let Some(reasoning_details) = reasoning_details_opt.as_array() {
                    warn!(
                        "🧠 [process_chat_sse] 收到 reasoning_details: {} 项, 原始数据: {}",
                        reasoning_details.len(),
                        serde_json::to_string(reasoning_details_opt).unwrap_or_default()
                    );
                    for detail in reasoning_details {
                        // 尝试从多个位置获取 id
                        let id = detail.get("id").and_then(|v| v.as_str())
                            .or_else(|| detail.get("tool_call_id").and_then(|v| v.as_str()));

                        if let Some(id) = id {
                            // 优先使用 data 字段（加密的推理数据）
                            if let Some(data) = detail.get("data").and_then(|v| v.as_str()) {
                                warn!(
                                    "🎯 [process_chat_sse] reasoning_details: id={}, data_len={}",
                                    id,
                                    data.len()
                                );
                                reasoning_details_by_id.insert(id.to_string(), data.to_string());
                            }
                            // 也尝试提取 thought_signature 如果存在
                            else if let Some(sig) = detail.get("thought_signature").and_then(|v| v.as_str()) {
                                warn!(
                                    "🎯 [process_chat_sse] reasoning_details: id={}, thought_signature_len={}",
                                    id,
                                    sig.len()
                                );
                                reasoning_details_by_id.insert(id.to_string(), sig.to_string());
                            }
                            // 尝试 signature 字段
                            else if let Some(sig) = detail.get("signature").and_then(|v| v.as_str()) {
                                warn!(
                                    "🎯 [process_chat_sse] reasoning_details: id={}, signature_len={}",
                                    id,
                                    sig.len()
                                );
                                reasoning_details_by_id.insert(id.to_string(), sig.to_string());
                            }
                        } else {
                            // 如果没有 id，尝试将整个 detail 序列化存储（用于调试）
                            warn!(
                                "⚠️ [process_chat_sse] reasoning_details 项缺少 id: {}",
                                serde_json::to_string(detail).unwrap_or_default()
                            );
                        }
                    }
                }
            }

            if let Some(delta) = choice.get("delta") {
                // 处理 reasoning 内容（支持多种格式）
                // - delta.reasoning: OpenAI 标准格式
                // - delta.reasoning_content: 智谱 GLM 格式
                // - delta.reasoning.content: 数组格式，包含多个 reasoning_text 对象
                if let Some(reasoning) = delta.get("reasoning") {
                    if let Some(text) = reasoning.as_str() {
                        append_reasoning_text(&tx_event, &mut reasoning_item, text.to_string());
                    } else if let Some(text) = reasoning.get("text").and_then(|v| v.as_str()) {
                        append_reasoning_text(&tx_event, &mut reasoning_item, text.to_string());
                    } else if let Some(text) = reasoning.get("content").and_then(|v| v.as_str()) {
                        append_reasoning_text(&tx_event, &mut reasoning_item, text.to_string());
                    } else if let Some(content_array) = reasoning.get("content").and_then(|v| v.as_array()) {
                        // 处理 content 数组格式：遍历数组中的每个 reasoning_text 对象
                        for item in content_array {
                            if let Some(item_type) = item.get("type").and_then(|v| v.as_str())
                                && item_type == "reasoning_text"
                                && let Some(text) = item.get("text").and_then(|v| v.as_str())
                                && !text.trim().is_empty()
                            {
                                append_reasoning_text(&tx_event, &mut reasoning_item, text.to_string());
                            }
                        }
                    }
                }
                // 智谱 GLM 使用 reasoning_content 字段
                // 注意：只接受包含实际内容的文本，过滤掉只有空白字符（如 "\n"）的文本
                if let Some(text) = delta.get("reasoning_content").and_then(|v| v.as_str())
                    && !text.trim().is_empty()
                {
                    append_reasoning_text(&tx_event, &mut reasoning_item, text.to_string());
                }

                if let Some(content) = delta.get("content") {
                    content_delta_count += 1;
                    if content.is_array() {
                        for item in content.as_array().unwrap_or(&vec![]) {
                            if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                                append_assistant_text(
                                    &tx_event,
                                    &mut assistant_item,
                                    text.to_string(),
                                    &mut xml_tool_call_buffer,
                                    &mut pending_xml_tool_calls,
                                );
                            }
                        }
                    } else if let Some(text) = content.as_str() {
                        append_assistant_text(
                            &tx_event,
                            &mut assistant_item,
                            text.to_string(),
                            &mut xml_tool_call_buffer,
                            &mut pending_xml_tool_calls,
                        );
                    }
                }

                if let Some(tool_call_values) = delta.get("tool_calls").and_then(|c| c.as_array()) {
                    // 🔍 DEBUG: 打印完整的 tool_calls 数据，用于调试 thought_signature 解析
                    warn!(
                        "🔧 [process_chat_sse] tool_calls 原始数据: {}",
                        serde_json::to_string(tool_call_values).unwrap_or_else(|_| "序列化失败".to_string())
                    );
                    for tool_call in tool_call_values {
                        let mut index = tool_call
                            .get("index")
                            .and_then(serde_json::Value::as_u64)
                            .map(|i| i as usize);

                        let mut call_id_for_lookup = None;
                        if let Some(call_id) = tool_call.get("id").and_then(|i| i.as_str()) {
                            call_id_for_lookup = Some(call_id.to_string());
                            if let Some(existing) = tool_call_index_by_id.get(call_id) {
                                index = Some(*existing);
                            }
                        }

                        if index.is_none() && call_id_for_lookup.is_none() {
                            index = last_tool_call_index;
                        }

                        let index = index.unwrap_or_else(|| {
                            while tool_calls.contains_key(&next_tool_call_index) {
                                next_tool_call_index += 1;
                            }
                            let idx = next_tool_call_index;
                            next_tool_call_index += 1;
                            idx
                        });

                        let call_state = tool_calls.entry(index).or_default();
                        if tool_call_order_seen.insert(index) {
                            tool_call_order.push(index);
                        }

                        if let Some(id) = tool_call.get("id").and_then(|i| i.as_str()) {
                            call_state.id.get_or_insert_with(|| id.to_string());
                            tool_call_index_by_id.entry(id.to_string()).or_insert(index);
                        }

                        if let Some(func) = tool_call.get("function") {
                            if let Some(fname) = func.get("name").and_then(|n| n.as_str())
                                && !fname.is_empty()
                            {
                                call_state.name.get_or_insert_with(|| fname.to_string());
                            }
                            if let Some(arguments) = func.get("arguments").and_then(|a| a.as_str())
                            {
                                call_state.arguments.push_str(arguments);
                            }
                            // Extract Gemini 3 thought signature if present
                            if let Some(sig) = func.get("thought_signature").and_then(|s| s.as_str()) {
                                warn!("🎯 [process_chat_sse] 找到 thought_signature (function 级别): {}", sig);
                                call_state.thought_signature.get_or_insert_with(|| sig.to_string());
                            }
                        }

                        // Also check for thought_signature at tool_call level (some APIs put it there)
                        if let Some(sig) = tool_call.get("thought_signature").and_then(|s| s.as_str()) {
                            warn!("🎯 [process_chat_sse] 找到 thought_signature (tool_call 级别): {}", sig);
                            call_state.thought_signature.get_or_insert_with(|| sig.to_string());
                        }

                        // 检查 delta 级别是否有 thought_signature
                        if call_state.thought_signature.is_none()
                            && let Some(sig) = delta.get("thought_signature").and_then(|s| s.as_str())
                        {
                            warn!("🎯 [process_chat_sse] 找到 thought_signature (delta 级别): {}", sig);
                            call_state.thought_signature.get_or_insert_with(|| sig.to_string());
                        }

                        // 🆕 检查 choice 级别的 reasoning_details (OpenRouter 格式)
                        if call_state.thought_signature.is_none()
                            && let Some(reasoning_details) = choice.get("reasoning_details")
                        {
                            warn!("🔧 [process_chat_sse] 找到 reasoning_details: {}",
                                serde_json::to_string(reasoning_details).unwrap_or_default());
                            // OpenRouter 可能在 reasoning_details 中包含 thought_signature
                            if let Some(sig) = reasoning_details.get("thought_signature").and_then(|s| s.as_str()) {
                                warn!("🎯 [process_chat_sse] 找到 thought_signature (reasoning_details 级别): {}", sig);
                                call_state.thought_signature.get_or_insert_with(|| sig.to_string());
                            }
                            // 或者整个 reasoning_details 作为 thought_signature
                            if call_state.thought_signature.is_none()
                                && let Some(sig) = reasoning_details.as_str()
                            {
                                warn!("🎯 [process_chat_sse] 使用 reasoning_details 字符串作为 thought_signature");
                                call_state.thought_signature.get_or_insert_with(|| sig.to_string());
                            }
                        }

                        // 🆕 检查 message 级别的 thought_signature
                        if call_state.thought_signature.is_none()
                            && let Some(message) = choice.get("message")
                            && let Some(sig) = message.get("thought_signature").and_then(|s| s.as_str())
                        {
                            warn!("🎯 [process_chat_sse] 找到 thought_signature (message 级别): {}", sig);
                            call_state.thought_signature.get_or_insert_with(|| sig.to_string());
                        }

                        last_tool_call_index = Some(index);
                    }
                }
            }

            if let Some(message) = choice.get("message")
                && let Some(reasoning) = message.get("reasoning")
            {
                if let Some(text) = reasoning.as_str() {
                    append_reasoning_text(&tx_event, &mut reasoning_item, text.to_string());
                } else if let Some(text) = reasoning.get("text").and_then(|v| v.as_str()) {
                    append_reasoning_text(&tx_event, &mut reasoning_item, text.to_string());
                } else if let Some(text) = reasoning.get("content").and_then(|v| v.as_str()) {
                    append_reasoning_text(&tx_event, &mut reasoning_item, text.to_string());
                } else if let Some(content_array) = reasoning.get("content").and_then(|v| v.as_array()) {
                    // 处理 content 数组格式：遍历数组中的每个 reasoning_text 对象
                    for item in content_array {
                        if let Some(item_type) = item.get("type").and_then(|v| v.as_str())
                            && item_type == "reasoning_text"
                            && let Some(text) = item.get("text").and_then(|v| v.as_str())
                            && !text.trim().is_empty()
                        {
                            append_reasoning_text(&tx_event, &mut reasoning_item, text.to_string());
                        }
                    }
                }
            }

            let finish_reason = choice.get("finish_reason").and_then(|r| r.as_str());
            // 处理正常结束的 finish_reason
            // - "stop": OpenAI 标准
            // - "normal": 智谱 GLM API
            // - "end_turn": 某些 API 变体
            // - "length": 输出被截断（max_tokens 限制），也视为正常完成
            if matches!(finish_reason, Some("stop") | Some("normal") | Some("end_turn") | Some("length")) {
                warn!(
                    "📥 [process_chat_sse] 收到 finish_reason={:?}, 事件数={}, content_delta数={}, 流运行时间={:?}",
                    finish_reason, event_count, content_delta_count, stream_start.elapsed()
                );
                if let Some(reasoning) = reasoning_item.take() {
                    debug!("📤 [process_chat_sse] finish_reason 发送 OutputItemDone(Reasoning)");
                    let _ = tx_event.send(Ok(ResponseEvent::OutputItemDone(reasoning)));
                }

                // 🆕 处理待处理的 XML tool_call
                for xml_tc in pending_xml_tool_calls.drain(..) {
                    xml_tool_call_counter += 1;
                    let call_id = format!("xml-tool-call-{xml_tool_call_counter}");

                    // 将 parameters 转换为 JSON 字符串
                    let arguments = serde_json::to_string(&xml_tc.parameters).unwrap_or_else(|_| "{}".to_string());

                    warn!(
                        "📤 [process_chat_sse] finish_reason 发送 XML FunctionCall: name={}, call_id={}, args_len={}",
                        xml_tc.function_name,
                        call_id,
                        arguments.len()
                    );

                    let item = ResponseItem::FunctionCall {
                        id: None,
                        name: xml_tc.function_name,
                        arguments,
                        call_id,
                        thought_signature: None,
                    };
                    let _ = tx_event.send(Ok(ResponseEvent::OutputItemDone(item)));
                }

                if let Some(assistant) = assistant_item.take() {
                    debug!("📤 [process_chat_sse] finish_reason 发送 OutputItemDone(Message)");
                    let _ = tx_event.send(Ok(ResponseEvent::OutputItemDone(assistant)));
                }
                if !completed_sent {
                    debug!("📤 [process_chat_sse] finish_reason 发送 Completed 事件");
                    let _ = tx_event.send(Ok(ResponseEvent::Completed {
                        response_id: String::new(),
                        token_usage: accumulated_usage.clone(),
                    }));
                }
                // 🔧 修复：收到 finish_reason 后应该立即返回，而不是继续处理
                // 这样可以避免重复处理 [DONE] 消息或其他事件
                return;
            }

            if finish_reason == Some("tool_calls") {
                if let Some(reasoning) = reasoning_item.take() {
                    let _ = tx_event.send(Ok(ResponseEvent::OutputItemDone(reasoning)));
                }

                for index in tool_call_order.drain(..) {
                    let Some(state) = tool_calls.remove(&index) else {
                        continue;
                    };
                    tool_call_order_seen.remove(&index);
                    let ToolCallState {
                        id,
                        name,
                        arguments,
                        thought_signature,
                    } = state;
                    let Some(name) = name else {
                        debug!("Skipping tool call at index {index} because name is missing");
                        continue;
                    };
                    let call_id = id.unwrap_or_else(|| format!("tool-call-{index}"));

                    // 🆕 如果 thought_signature 为 None，尝试从 reasoning_details 中查找
                    let final_thought_signature = thought_signature.or_else(|| {
                        if let Some(sig) = reasoning_details_by_id.get(&call_id) {
                            warn!(
                                "🎯 [process_chat_sse] 从 reasoning_details 获取 thought_signature: call_id={}, sig_len={}",
                                call_id,
                                sig.len()
                            );
                            Some(sig.clone())
                        } else {
                            warn!(
                                "⚠️ [process_chat_sse] 未找到 thought_signature: call_id={}, reasoning_details_keys={:?}",
                                call_id,
                                reasoning_details_by_id.keys().collect::<Vec<_>>()
                            );
                            None
                        }
                    });

                    let item = ResponseItem::FunctionCall {
                        id: None,
                        name,
                        arguments,
                        call_id,
                        thought_signature: final_thought_signature,
                    };
                    let _ = tx_event.send(Ok(ResponseEvent::OutputItemDone(item)));
                }
            }
        }
    }
}

fn append_assistant_text(
    tx_event: &mpsc::UnboundedSender<Result<ResponseEvent, ApiError>>,
    assistant_item: &mut Option<ResponseItem>,
    text: String,
    xml_tool_call_buffer: &mut String,
    pending_xml_tool_calls: &mut Vec<XmlToolCall>,
) {
    // 累积文本以检测 XML tool_call
    xml_tool_call_buffer.push_str(&text);

    // 检查是否有完整的 XML tool_call
    while contains_xml_tool_call(xml_tool_call_buffer) {
        if let Some(tool_call) = parse_xml_tool_call(xml_tool_call_buffer) {
            warn!(
                "🔧 [append_assistant_text] 检测到 XML tool_call: function={}, params={:?}",
                tool_call.function_name,
                tool_call.parameters.keys().collect::<Vec<_>>()
            );
            pending_xml_tool_calls.push(tool_call);

            // 从缓冲区中移除已解析的 tool_call
            if let Some(end_pos) = xml_tool_call_buffer.find("</tool_call>") {
                let remove_end = end_pos + "</tool_call>".len();
                // 也移除开始标签之前的内容（如果有）
                if let Some(start_pos) = xml_tool_call_buffer.find("<tool_call>") {
                    // 保留 tool_call 之前的文本作为普通文本输出
                    let before_text = xml_tool_call_buffer[..start_pos].to_string();
                    if !before_text.trim().is_empty() {
                        // 发送 tool_call 之前的文本
                        if assistant_item.is_none() {
                            let item = ResponseItem::Message {
                                id: None,
                                role: "assistant".to_string(),
                                content: vec![],
                            };
                            *assistant_item = Some(item.clone());
                            let _ = tx_event.send(Ok(ResponseEvent::OutputItemAdded(item)));
                        }
                        if let Some(ResponseItem::Message { content, .. }) = assistant_item {
                            content.push(ContentItem::OutputText { text: before_text.clone() });
                            let _ = tx_event.send(Ok(ResponseEvent::OutputTextDelta(before_text)));
                        }
                    }
                    *xml_tool_call_buffer = xml_tool_call_buffer[remove_end..].to_string();
                } else {
                    *xml_tool_call_buffer = xml_tool_call_buffer[remove_end..].to_string();
                }
            } else {
                break;
            }
        } else {
            break;
        }
    }

    // 如果缓冲区中没有待处理的 tool_call 开始标签，输出普通文本
    if !xml_tool_call_buffer.contains("<tool_call>") {
        let text_to_output = std::mem::take(xml_tool_call_buffer);
        if !text_to_output.is_empty() {
            if assistant_item.is_none() {
                let item = ResponseItem::Message {
                    id: None,
                    role: "assistant".to_string(),
                    content: vec![],
                };
                *assistant_item = Some(item.clone());
                let _ = tx_event.send(Ok(ResponseEvent::OutputItemAdded(item)));
            }

            if let Some(ResponseItem::Message { content, .. }) = assistant_item {
                content.push(ContentItem::OutputText { text: text_to_output.clone() });
                let _ = tx_event.send(Ok(ResponseEvent::OutputTextDelta(text_to_output)));
            }
        }
    }
}

fn append_reasoning_text(
    tx_event: &mpsc::UnboundedSender<Result<ResponseEvent, ApiError>>,
    reasoning_item: &mut Option<ResponseItem>,
    text: String,
) {
    if reasoning_item.is_none() {
        let item = ResponseItem::Reasoning {
            id: String::new(),
            summary: Vec::new(),
            content: Some(vec![ReasoningItemContent::ReasoningText { text: String::new() }]),
            encrypted_content: None,
        };
        *reasoning_item = Some(item.clone());
        let _ = tx_event.send(Ok(ResponseEvent::OutputItemAdded(item)));
    }

    if let Some(ResponseItem::Reasoning {
        content: Some(content),
        ..
    }) = reasoning_item
    {
        // 累积文本到第一个 ReasoningText 元素中，而不是创建新的元素
        if let Some(ReasoningItemContent::ReasoningText { text: accumulated_text }) = content.first_mut() {
            accumulated_text.push_str(&text);

            let _ = tx_event.send(Ok(ResponseEvent::ReasoningContentDelta {
                delta: text.clone(),
                content_index: 0,
            }));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;
    use codex_protocol::models::ResponseItem;
    use futures::TryStreamExt;
    use serde_json::json;
    use tokio::sync::mpsc;
    use tokio_util::io::ReaderStream;

    fn build_body(events: &[serde_json::Value]) -> String {
        let mut body = String::new();
        for e in events {
            body.push_str(&format!("event: message\ndata: {e}\n\n"));
        }
        body
    }

    async fn collect_events(body: &str) -> Vec<ResponseEvent> {
        let reader = ReaderStream::new(std::io::Cursor::new(body.to_string()))
            .map_err(|err| codex_client::TransportError::Network(err.to_string()));
        let (tx, mut rx) = mpsc::unbounded_channel::<Result<ResponseEvent, ApiError>>();
        tokio::spawn(process_chat_sse(
            reader,
            tx,
            Duration::from_millis(1000),
            None,
        ));

        let mut out = Vec::new();
        while let Some(ev) = rx.recv().await {
            out.push(ev.expect("stream error"));
        }
        out
    }

    #[tokio::test]
    async fn concatenates_tool_call_arguments_across_deltas() {
        let delta_name = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "id": "call_a",
                        "index": 0,
                        "function": { "name": "do_a" }
                    }]
                }
            }]
        });

        let delta_args_1 = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "function": { "arguments": "{ \"foo\":" }
                    }]
                }
            }]
        });

        let delta_args_2 = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "function": { "arguments": "1}" }
                    }]
                }
            }]
        });

        let finish = json!({
            "choices": [{
                "finish_reason": "tool_calls"
            }]
        });

        let body = build_body(&[delta_name, delta_args_1, delta_args_2, finish]);
        let events = collect_events(&body).await;
        assert_matches!(
            &events[..],
            [
                ResponseEvent::OutputItemDone(ResponseItem::FunctionCall { call_id, name, arguments, .. }),
                ResponseEvent::Completed { .. }
            ] if call_id == "call_a" && name == "do_a" && arguments == "{ \"foo\":1}"
        );
    }

    #[tokio::test]
    async fn emits_multiple_tool_calls() {
        let delta_a = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "id": "call_a",
                        "function": { "name": "do_a", "arguments": "{\"foo\":1}" }
                    }]
                }
            }]
        });

        let delta_b = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "id": "call_b",
                        "function": { "name": "do_b", "arguments": "{\"bar\":2}" }
                    }]
                }
            }]
        });

        let finish = json!({
            "choices": [{
                "finish_reason": "tool_calls"
            }]
        });

        let body = build_body(&[delta_a, delta_b, finish]);
        let events = collect_events(&body).await;
        assert_matches!(
            &events[..],
            [
                ResponseEvent::OutputItemDone(ResponseItem::FunctionCall { call_id: call_a, name: name_a, arguments: args_a, .. }),
                ResponseEvent::OutputItemDone(ResponseItem::FunctionCall { call_id: call_b, name: name_b, arguments: args_b, .. }),
                ResponseEvent::Completed { .. }
            ] if call_a == "call_a" && name_a == "do_a" && args_a == "{\"foo\":1}" && call_b == "call_b" && name_b == "do_b" && args_b == "{\"bar\":2}"
        );
    }

    #[tokio::test]
    async fn emits_tool_calls_for_multiple_choices() {
        let payload = json!({
            "choices": [
                {
                    "delta": {
                        "tool_calls": [{
                            "id": "call_a",
                            "index": 0,
                            "function": { "name": "do_a", "arguments": "{}" }
                        }]
                    },
                    "finish_reason": "tool_calls"
                },
                {
                    "delta": {
                        "tool_calls": [{
                            "id": "call_b",
                            "index": 0,
                            "function": { "name": "do_b", "arguments": "{}" }
                        }]
                    },
                    "finish_reason": "tool_calls"
                }
            ]
        });

        let body = build_body(&[payload]);
        let events = collect_events(&body).await;
        assert_matches!(
            &events[..],
            [
                ResponseEvent::OutputItemDone(ResponseItem::FunctionCall { call_id: call_a, name: name_a, arguments: args_a, .. }),
                ResponseEvent::OutputItemDone(ResponseItem::FunctionCall { call_id: call_b, name: name_b, arguments: args_b, .. }),
                ResponseEvent::Completed { .. }
            ] if call_a == "call_a" && name_a == "do_a" && args_a == "{}" && call_b == "call_b" && name_b == "do_b" && args_b == "{}"
        );
    }

    #[tokio::test]
    async fn merges_tool_calls_by_index_when_id_missing_on_subsequent_deltas() {
        let delta_with_id = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "call_a",
                        "function": { "name": "do_a", "arguments": "{ \"foo\":" }
                    }]
                }
            }]
        });

        let delta_without_id = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "function": { "arguments": "1}" }
                    }]
                }
            }]
        });

        let finish = json!({
            "choices": [{
                "finish_reason": "tool_calls"
            }]
        });

        let body = build_body(&[delta_with_id, delta_without_id, finish]);
        let events = collect_events(&body).await;
        assert_matches!(
            &events[..],
            [
                ResponseEvent::OutputItemDone(ResponseItem::FunctionCall { call_id, name, arguments, .. }),
                ResponseEvent::Completed { .. }
            ] if call_id == "call_a" && name == "do_a" && arguments == "{ \"foo\":1}"
        );
    }

    #[tokio::test]
    async fn preserves_tool_call_name_when_empty_deltas_arrive() {
        let delta_with_name = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "id": "call_a",
                        "function": { "name": "do_a" }
                    }]
                }
            }]
        });

        let delta_with_empty_name = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "id": "call_a",
                        "function": { "name": "", "arguments": "{}" }
                    }]
                }
            }]
        });

        let finish = json!({
            "choices": [{
                "finish_reason": "tool_calls"
            }]
        });

        let body = build_body(&[delta_with_name, delta_with_empty_name, finish]);
        let events = collect_events(&body).await;
        assert_matches!(
            &events[..],
            [
                ResponseEvent::OutputItemDone(ResponseItem::FunctionCall { name, arguments, .. }),
                ResponseEvent::Completed { .. }
            ] if name == "do_a" && arguments == "{}"
        );
    }

    #[tokio::test]
    async fn emits_tool_calls_even_when_content_and_reasoning_present() {
        let delta_content_and_tools = json!({
            "choices": [{
                "delta": {
                    "content": [{"text": "hi"}],
                    "reasoning": "because",
                    "tool_calls": [{
                        "id": "call_a",
                        "function": { "name": "do_a", "arguments": "{}" }
                    }]
                }
            }]
        });

        let finish = json!({
            "choices": [{
                "finish_reason": "tool_calls"
            }]
        });

        let body = build_body(&[delta_content_and_tools, finish]);
        let events = collect_events(&body).await;

        assert_matches!(
            &events[..],
            [
                ResponseEvent::OutputItemAdded(ResponseItem::Reasoning { .. }),
                ResponseEvent::ReasoningContentDelta { .. },
                ResponseEvent::OutputItemAdded(ResponseItem::Message { .. }),
                ResponseEvent::OutputTextDelta(delta),
                ResponseEvent::OutputItemDone(ResponseItem::Reasoning { .. }),
                ResponseEvent::OutputItemDone(ResponseItem::FunctionCall { call_id, name, .. }),
                ResponseEvent::OutputItemDone(ResponseItem::Message { .. }),
                ResponseEvent::Completed { .. }
            ] if delta == "hi" && call_id == "call_a" && name == "do_a"
        );
    }

    #[tokio::test]
    async fn drops_partial_tool_calls_on_stop_finish_reason() {
        let delta_tool = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "id": "call_a",
                        "function": { "name": "do_a", "arguments": "{}" }
                    }]
                }
            }]
        });

        let finish_stop = json!({
            "choices": [{
                "finish_reason": "stop"
            }]
        });

        let body = build_body(&[delta_tool, finish_stop]);
        let events = collect_events(&body).await;

        assert!(!events.iter().any(|ev| {
            matches!(
                ev,
                ResponseEvent::OutputItemDone(ResponseItem::FunctionCall { .. })
            )
        }));
        assert_matches!(events.last(), Some(ResponseEvent::Completed { .. }));
    }

    // ========== XML tool_call 解析测试 ==========

    #[test]
    fn parses_xml_tool_call_format() {
        let xml = r#"<tool_call>
<function=mcp__sequentialthinking__sequentialthinking>
<parameter=thought>I need to provide an actionable response</parameter>
<parameter=nextThoughtNeeded>True</parameter>
<parameter=thoughtNumber>1</parameter>
<parameter=totalThoughts>5</parameter>
</function>
</tool_call>"#;

        let result = parse_xml_tool_call(xml);
        assert!(result.is_some());

        let tool_call = result.unwrap();
        assert_eq!(tool_call.function_name, "mcp__sequentialthinking__sequentialthinking");
        assert_eq!(tool_call.parameters.get("thought"), Some(&"I need to provide an actionable response".to_string()));
        assert_eq!(tool_call.parameters.get("nextThoughtNeeded"), Some(&"True".to_string()));
        assert_eq!(tool_call.parameters.get("thoughtNumber"), Some(&"1".to_string()));
        assert_eq!(tool_call.parameters.get("totalThoughts"), Some(&"5".to_string()));
    }

    #[test]
    fn contains_xml_tool_call_detects_presence() {
        let with_tool_call = "Some text <tool_call><function=test></function></tool_call> more text";
        let without_tool_call = "Just some regular text";

        assert!(contains_xml_tool_call(with_tool_call));
        assert!(!contains_xml_tool_call(without_tool_call));
    }

    #[test]
    fn parses_empty_parameters() {
        let xml = r#"<tool_call>
<function=simple_function>
</function>
</tool_call>"#;

        let result = parse_xml_tool_call(xml);
        assert!(result.is_some());

        let tool_call = result.unwrap();
        assert_eq!(tool_call.function_name, "simple_function");
        assert!(tool_call.parameters.is_empty());
    }

    #[tokio::test]
    async fn extracts_xml_tool_call_from_content() {
        let delta_with_xml_tool_call = json!({
            "choices": [{
                "delta": {
                    "content": "<tool_call>\n<function=test_function>\n<parameter=arg1>value1</parameter>\n</function>\n</tool_call>"
                }
            }]
        });

        let finish_stop = json!({
            "choices": [{
                "finish_reason": "stop"
            }]
        });

        let body = build_body(&[delta_with_xml_tool_call, finish_stop]);
        let events = collect_events(&body).await;

        // 应该有一个 FunctionCall 事件
        let has_function_call = events.iter().any(|ev| {
            matches!(
                ev,
                ResponseEvent::OutputItemDone(ResponseItem::FunctionCall { name, .. }) if name == "test_function"
            )
        });
        assert!(has_function_call, "Expected XML tool_call to be parsed as FunctionCall, got: {events:?}");
    }
}
