//! Shell 命令工具模块
//!
//! 提供统一的 shell 命令验证、修复和处理功能：
//! - Shell 操作符检测（重定向、管道等）
//! - Heredoc 语法验证和修复
//! - 命令数组到字符串的转换
//! - 错误命令格式的自动修复
//! - 输入提示检测
//! - JSON 参数修复

use std::sync::LazyLock;
use regex::Regex;
use tracing::{debug, error, info, warn};

/// Type alias for JSON repair strategy function pointer
type JsonRepairStrategy = fn(&str) -> String;

// ============================================================================
// JSON 参数修复
// ============================================================================

/// 修复无效的 JSON 参数字符串
///
/// 某些 AI 模型可能生成包含以下问题的 JSON：
/// 1. 字符串中包含实际换行符（应使用 \n 转义）
/// 2. 使用单引号替代双引号
/// 3. 控制字符未转义
/// 4. 多行字符串格式问题
///
/// # Arguments
/// * `json_str` - 原始 JSON 字符串
///
/// # Returns
/// 修复后的 JSON 字符串
pub fn sanitize_json_arguments(json_str: &str) -> String {
    let mut result = String::with_capacity(json_str.len() * 2);
    let mut in_string = false;
    let mut escape_next = false;
    let chars: Vec<char> = json_str.chars().collect();
    let len = chars.len();

    for c in chars.iter().take(len) {

        if escape_next {
            // 上一个字符是 \，这个字符是转义字符
            result.push(*c);
            escape_next = false;
            continue;
        }

        if *c == '\\' && in_string {
            result.push(*c);
            escape_next = true;
            continue;
        }

        if *c == '"' && !escape_next {
            in_string = !in_string;
            result.push(*c);
            continue;
        }

        if in_string {
            // 在字符串内部，需要转义控制字符
            match *c {
                '\n' => {
                    result.push_str("\\n");
                }
                '\r' => {
                    result.push_str("\\r");
                }
                '\t' => {
                    result.push_str("\\t");
                }
                '\x00'..='\x1f' => {
                    // 其他控制字符，使用 Unicode 转义
                    result.push_str(&format!("\\u{:04x}", *c as u32));
                }
                _ => {
                    result.push(*c);
                }
            }
        } else {
            // 不在字符串内部，直接添加
            result.push(*c);
        }
    }

    // 如果进行了修改，记录日志
    if result != json_str {
        info!("🔧 修复了 JSON 参数中的控制字符");
        debug!("  原始长度: {}, 修复后长度: {}", json_str.len(), result.len());
    }

    result
}

/// 高级 JSON 修复函数，处理复杂的多行字符串和引号问题
fn advanced_json_fix(json_str: &str) -> String {
    // 首先尝试基本的控制字符修复
    let mut fixed = sanitize_json_arguments(json_str);

    // 如果仍然包含问题，尝试更激进的修复
    if fixed.contains('\n') || fixed.contains('\r') {
        // 将所有剩余的控制字符转义
        fixed = fixed.chars().map(|c| match c {
            '\n' => "\\n".to_string(),
            '\r' => "\\r".to_string(),
            '\t' => "\\t".to_string(),
            '\x00'..='\x1f' => format!("\\u{:04x}", c as u32),
            _ => c.to_string(),
        }).collect::<String>();
    }

    // 处理三重引号问题（Python 风格的多行字符串）
    if fixed.contains("'''") {
        fixed = fix_triple_quotes(json_str);
    }

    fixed
}

/// 修复字符串化的数组问题
fn fix_stringified_arrays(json_str: &str) -> String {
    // 对于大型 JSON，添加性能优化
    let json_len = json_str.len();
    if json_len > 10000 {
        debug!("处理大型 JSON ({} 字节)", json_len);
    }

    // 首先尝试解析 JSON
    match serde_json::from_str::<serde_json::Value>(json_str) {
        Ok(mut value) => {
            if let Some(obj) = value.as_object_mut() {
                // 检查所有可能的数组字段
                let array_fields = ["command", "args", "files", "lines"];

                for field in &array_fields {
                    // 先获取字段值的克隆，避免借用冲突
                    let field_value_opt = obj.get(*field).and_then(|v| v.as_str().map(std::borrow::ToOwned::to_owned));

                    if let Some(field_value) = field_value_opt {
                        // 检查是否包含未转义的换行符或其他问题字符
                        if field_value.contains('\n') && !field_value.contains("\\n") {
                            warn!("检测到字段 {} 包含未转义的换行符，尝试修复", field);

                            // 尝试修复：将未转义的换行符转为转义形式
                            let mut escaped = field_value.clone();
                            escaped = escaped.replace('\\', "\\\\"); // 先转义反斜杠
                            escaped = escaped.replace('"', "\\\""); // 转义双引号
                            escaped = escaped.replace('\n', "\\n"); // 转义换行符
                            escaped = escaped.replace('\r', "\\r"); // 转义回车符
                            escaped = escaped.replace('\t', "\\t"); // 转义制表符

                            // 将修复后的字符串重新插入
                            obj.insert(field.to_string(), serde_json::Value::String(escaped));
                            info!("🔧 修复了字段 {} 中的未转义字符", field);
                        }

                        // 尝试将字符串解析为 JSON 数组
                        match serde_json::from_str::<serde_json::Value>(&field_value) {
                            Ok(array_value) if array_value.is_array() => {
                                // 替换为真正的数组
                                obj.insert(field.to_string(), array_value);
                                info!("🔧 修复了字符串化的 {} 数组 (长度: {})", field, field_value.len());
                            }
                            _ => {}
                        }
                    }
                }
            }

            // 重新序列化
            match serde_json::to_string(&value) {
                Ok(fixed) => {
                    if fixed != json_str {
                        info!("✅ 字符串化数组修复成功");
                        return fixed;
                    }
                }
                Err(e) => {
                    warn!("重新序列化修复后的 JSON 失败: {}", e);
                }
            }

            json_str.to_string()
        }
        Err(e) => {
            // 如果 JSON 解析失败，记录详细错误
            warn!("无法解析 JSON 进行字符串化数组修复: {}", e);
            json_str.to_string()
        }
    }
}

/// 修复缺失字段问题
fn fix_missing_fields(json_str: &str, expected_fields: &[&str]) -> String {
    let mut result = json_str.trim().to_string();

    // 尝试解析为 JSON 值
    if let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&result)
        && let Some(obj) = value.as_object_mut()
    {
        for field in expected_fields {
            if !obj.contains_key(*field) {
                // 添加缺失的字段，使用空字符串作为默认值
                obj.insert(field.to_string(), serde_json::Value::String(String::new()));
                info!("🔧 添加了缺失的字段: {}", field);
            }
        }

        // 重新序列化
        result = serde_json::to_string(&value).unwrap_or(result);
    }

    result
}

/// 从错误消息中提取缺失的字段名
fn extract_missing_field_name(error_msg: &str) -> Option<String> {
    // 错误格式示例: "missing field `input` at line 1 column 100"
    if let Some(start) = error_msg.find("missing field `") {
        let start = start + "missing field `".len();
        if let Some(end) = error_msg[start..].find('`') {
            return Some(error_msg[start..start + end].to_string());
        }
    }
    None
}

/// 修复 Python 风格的三重引号
fn fix_triple_quotes(json_str: &str) -> String {
    let mut result = String::new();
    let mut i = 0;
    let chars: Vec<char> = json_str.chars().collect();

    while i < chars.len() {
        // 检查是否遇到三重引号
        if i + 2 < chars.len() && chars[i] == '\'' && chars[i+1] == '\'' && chars[i+2] == '\'' {
            // 将三重引号替换为普通字符串，并转义内部内容
            result.push('"');
            i += 3;

            // 找到结束的三重引号
            let mut in_content = true;
            while i + 2 < chars.len() && in_content {
                if chars[i] == '\'' && chars[i+1] == '\'' && chars[i+2] == '\'' {
                    result.push('"');
                    i += 3;
                    in_content = false;
                } else {
                    // 转义内容中的特殊字符
                    match chars[i] {
                        '\n' => result.push_str("\\n"),
                        '\r' => result.push_str("\\r"),
                        '\t' => result.push_str("\\t"),
                        '"' => result.push_str("\\\""),
                        '\\' => result.push_str("\\\\"),
                        c => result.push(c),
                    }
                    i += 1;
                }
            }
        } else {
            result.push(chars[i]);
            i += 1;
        }
    }

    result
}

/// 尝试修复并解析 JSON 参数
///
/// 首先尝试直接解析，如果失败则尝试多种修复策略后再解析
///
/// # Arguments
/// * `json_str` - JSON 字符串
///
/// # Returns
/// 解析结果
pub fn parse_json_with_recovery<T: serde::de::DeserializeOwned>(json_str: &str) -> Result<T, serde_json::Error> {
    // 记录原始 JSON 的前 200 个字符用于调试
    // 对于大型 JSON，显示更多信息
    let preview = if json_str.len() > 200 {
        format!("{}... (总长度: {} 字节)", &json_str[..200], json_str.len())
    } else if json_str.len() > 100 {
        format!("{}... (总长度: {} 字节)", &json_str, json_str.len())
    } else {
        format!("{} (总长度: {} 字节)", json_str, json_str.len())
    };

    // 首先尝试直接解析
    match serde_json::from_str(json_str) {
        Ok(result) => Ok(result),
        Err(e) => {
            // 解析失败，记录详细的错误信息
            let error_msg = e.to_string();
            let line = e.line();
            let column = e.column();

            warn!(
                "JSON 解析失败详情:\n  错误: {}\n  位置: 行 {}, 列 {}\n  JSON 预览: {}",
                error_msg, line, column, preview
            );

            // 尝试多种修复策略
            let repair_strategies: Vec<(&str, JsonRepairStrategy)> = vec![
                ("复杂字符串化JSON修复", fix_complex_stringified_json),
                ("未转义换行符修复", fix_unescaped_newlines),
                ("控制字符修复", sanitize_json_arguments),
                ("高级修复", advanced_json_fix),
                ("混合引号修复", fix_mixed_quotes_in_array),
                ("字符串化数组修复", fix_stringified_arrays),
                ("引号修复", fix_common_quote_issues),
                ("未闭合引号修复", fix_unclosed_quotes),
                ("括号修复", fix_bracket_issues),
                ("尾部补全", fix_trailing_issues),
            ];

            // 对于缺失字段错误，尝试特殊处理
            if error_msg.contains("missing field") {
                // 提取缺失的字段名
                if let Some(field) = extract_missing_field_name(&error_msg) {
                    debug!("检测到缺失字段: {}", field);
                    let fixed = fix_missing_fields(json_str, &[&field]);
                    if fixed != json_str {
                        match serde_json::from_str::<serde_json::Value>(&fixed) {
                            Ok(_) => {
                                info!("✅ JSON 修复成功 - 添加了缺失字段");
                                // 现在尝试反序列化为具体类型
                                return serde_json::from_str(&fixed);
                            }
                            Err(e) => {
                                debug!("  添加字段后仍然失败: {}", e);
                            }
                        }
                    }
                }
            }

            // 对于包含 heredoc 的特殊问题，先尝试专门的处理
            if json_str.contains("<<") && json_str.contains("'") {
                debug!("检测到可能的 heredoc 相关问题，尝试特殊处理");
                let fixed = fix_heredoc_array_issues(json_str);
                if fixed != json_str {
                    match serde_json::from_str(&fixed) {
                        Ok(result) => {
                            info!("✅ JSON 修复成功 - 使用策略: heredoc数组修复");
                            return Ok(result);
                        }
                        Err(e2) => {
                            debug!("  heredoc数组修复失败: {}", e2);
                        }
                    }
                }
            }

            for (strategy_name, repair_fn) in repair_strategies {
                debug!("尝试修复策略: {}", strategy_name);
                let repaired = repair_fn(json_str);

                if repaired != json_str {
                    match serde_json::from_str(&repaired) {
                        Ok(result) => {
                            info!("✅ JSON 修复成功 - 使用策略: {}", strategy_name);
                            if repaired.len() != json_str.len() {
                                debug!("  原始长度: {}, 修复后长度: {}", json_str.len(), repaired.len());
                            }
                            return Ok(result);
                        }
                        Err(e2) => {
                            debug!("  策略 {} 失败: {}", strategy_name, e2);
                        }
                    }
                }
            }

            // 所有修复策略都失败
            // 如果是缺失字段错误，尝试提供更有用的调试信息
            if error_msg.contains("missing field") {
                // 检查是否是字段名不匹配的问题
                if json_str.contains("\"command\"") {
                    error!(
                        "❌ JSON 字段不匹配\n  期望字段: input\n  实际包含: command\n  可能原因: 错误的工具类型被调用"
                    );
                } else {
                    error!(
                        "❌ JSON 缺少必需字段\n  缺失字段: {}\n  建议: 检查工具参数要求",
                        extract_missing_field_name(&error_msg).unwrap_or_else(|| "unknown".to_string())
                    );
                }
            } else {
                error!(
                    "❌ 所有 JSON 修复策略都失败\n  原始错误: {}\n  JSON 内容: {}",
                    e, json_str
                );
            }

            // 返回第一个错误（原始错误）
            Err(e)
        }
    }
}

/// 修复包含 heredoc 的数组问题
fn fix_heredoc_array_issues(json_str: &str) -> String {
    // 首先尝试解析 JSON
    if let Ok(mut value) = serde_json::from_str::<serde_json::Value>(json_str)
        && let Some(obj) = value.as_object_mut()
        && let Some(command_val) = obj.get_mut("command")
        && let Some(arr) = command_val.as_array_mut()
    {
        // 检查数组是否需要处理
        let needs_fix = arr.iter().any(|item| {
            if let Some(s) = item.as_str() {
                s.contains("'") && (s.contains("<<") || s.contains('\n'))
            } else {
                false
            }
        });

        if needs_fix {
            // 将 command 数组转换为单个命令字符串
            let mut command_parts = Vec::new();
            let mut i = 0;
            while i < arr.len() {
                if let Some(s) = arr[i].as_str() {
                    if s.starts_with("'") && s.contains("<<") {
                        // 这是包含 heredoc 的复杂字符串
                        // 移除外层单引号并处理内容
                        let content = s.trim_matches('\'');
                        command_parts.push(content);
                    } else {
                        command_parts.push(s);
                    }
                }
                i += 1;
            }

            // 重构为单个命令
            let full_command = command_parts.join(" ");
            info!("🔧 将 heredoc 数组重构为单个命令");
            debug!("  原始数组元素数: {}", arr.len());
            debug!("  重构后命令: {}", full_command);

            // 替换为字符串
            obj.insert("command".to_string(),
                     serde_json::Value::String(full_command));
        }

        // 重新序列化
        if let Ok(fixed) = serde_json::to_string(&value) {
            return fixed;
        }
    }

    // 如果解析失败，尝试文本级修复
    let mut result = json_str.to_string();

    // 查找 command 数组
    if let Some(start) = result.find("\"command\":[") {
        let start = start + "\"command\":[".len();
        let mut bracket_count = 1;
        let mut end = start;
        let mut in_string = false;
        let mut escape_next = false;

        // 找到数组结束
        while end < result.len() && bracket_count > 0 {
            let Some(c) = result.chars().nth(end) else {
                break;
            };
            if escape_next {
                escape_next = false;
            } else if c == '\\' {
                escape_next = true;
            } else if c == '"' && !escape_next {
                in_string = !in_string;
            } else if !in_string {
                if c == '[' {
                    bracket_count += 1;
                } else if c == ']' {
                    bracket_count -= 1;
                }
            }
            end += 1;
        }

        if bracket_count == 0 {
            // 提取数组内容
            let array_content = &result[start..end-1];

            // 检查是否包含问题模式
            if array_content.contains("'") && array_content.contains("<<") {
                // 简单的文本修复
                let fixed_array = array_content
                    .replace("'", "\"")  // 替换单引号为双引号
                    .replace("\n", "\\n") // 转义换行符
                    .replace("\r", "\\r");

                result.replace_range(start..end-1, &fixed_array);
                info!("🔧 文本级修复 heredoc 数组");
            }
        }
    }

    result
}

/// 修复未闭合引号问题
fn fix_unclosed_quotes(json_str: &str) -> String {
    let mut result = String::with_capacity(json_str.len() + 10);
    let chars: Vec<char> = json_str.chars().collect();
    let len = chars.len();
    let mut i = 0;
    let mut in_string = false;
    let mut string_start_char = '\0';
    let mut escape_next = false;

    while i < len {
        let c = chars[i];

        if escape_next {
            result.push(c);
            escape_next = false;
            i += 1;
            continue;
        }

        if c == '\\' {
            result.push(c);
            escape_next = true;
            i += 1;
            continue;
        }

        if c == '"' || c == '\'' {
            if !in_string {
                // 字符串开始
                in_string = true;
                string_start_char = c;
                result.push(c);  // 总是使用双引号
                i += 1;

                // 如果是单引号开始，跳过它并使用双引号
                if c == '\'' {
                    result.pop();  // 移除刚添加的单引号
                    result.push('"');  // 使用双引号
                }
            } else {
                // 字符串结束
                if c == string_start_char {
                    in_string = false;
                    string_start_char = '\0';
                    result.push('"');  // 总是使用双引号结束
                    i += 1;
                } else {
                    // 内嵌的不同引号，转义它
                    result.push_str("\\\"");
                    i += 1;
                }
            }
        } else if in_string {
            // 在字符串内部
            match c {
                '\n' => result.push_str("\\n"),
                '\r' => result.push_str("\\r"),
                '\t' => result.push_str("\\t"),
                '\x00'..='\x1f' => result.push_str(&format!("\\u{:04x}", c as u32)),
                _ => result.push(c),
            }
            i += 1;
        } else {
            // 不在字符串内部
            result.push(c);
            i += 1;
        }
    }

    // 如果字符串未闭合，闭合它
    if in_string {
        result.push('"');
        warn!("🔧 修复了未闭合的字符串引号");
    }

    result
}

/// 修复常见的引号问题
fn fix_common_quote_issues(json_str: &str) -> String {
    let result = json_str.to_string();

    // 替换单引号为双引号（在字符串外部）
    let mut in_string = false;
    let mut escape_next = false;
    let chars: Vec<char> = result.chars().collect();
    let mut fixed = String::with_capacity(result.len());

    for &c in &chars {

        if escape_next {
            fixed.push(c);
            escape_next = false;
            continue;
        }

        if c == '\\' {
            fixed.push(c);
            escape_next = true;
            continue;
        }

        if c == '"' && !escape_next {
            in_string = !in_string;
            fixed.push(c);
            continue;
        }

        // 替换单引号为双引号（不在字符串内且不在转义状态）
        if c == '\'' && !in_string && !escape_next {
            fixed.push('"');
            continue;
        }

        fixed.push(c);
    }

    fixed
}

/// 修复 JSON 数组中的混合引号问题
fn fix_mixed_quotes_in_array(json_str: &str) -> String {
    let mut result = String::with_capacity(json_str.len() * 2);
    let chars: Vec<char> = json_str.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        // 查找数组开始
        if chars[i] == '[' {
            // 处理数组内容
            result.push('[');
            i += 1;

            while i < len && chars[i] != ']' {
                // 跳过空白字符
                if chars[i].is_whitespace() {
                    result.push(chars[i]);
                    i += 1;
                    continue;
                }

                // 处理数组元素
                if chars[i] == '\'' || chars[i] == '"' {
                    let quote_char = chars[i];
                    let mut element = String::new();
                    i += 1;
                    let mut has_embedded_quotes = false;

                    // 读取整个字符串
                    while i < len && chars[i] != quote_char {
                        // 处理转义
                        if chars[i] == '\\' {
                            element.push('\\');
                            i += 1;
                            if i < len {
                                element.push(chars[i]);
                                i += 1;
                            }
                        } else {
                            // 检测内嵌的引号
                            if (quote_char == '\'' && chars[i] == '"') ||
                               (quote_char == '"' && chars[i] == '\'') {
                                has_embedded_quotes = true;
                            }
                            element.push(chars[i]);
                            i += 1;
                        }
                    }

                    // 如果找到结束引号，跳过它
                    if i < len && chars[i] == quote_char {
                        i += 1;
                    } else {
                        // 没有找到结束引号，可能是未闭合的字符串
                        debug!("警告: 数组元素未闭合的引号");
                    }

                    // 特殊处理：如果单引号字符串包含双引号且内容像 heredoc
                    if quote_char == '\'' && has_embedded_quotes &&
                       (element.contains("<<") || element.contains("EOF")) {
                        // 这看起来像是 heredoc 命令，尝试重构
                        if let Some(refactored) = try_refactor_heredoc_element(&element) {
                            result.push_str(&refactored);
                        } else {
                            // 无法重构，则正常转义
                            append_escaped_string(&mut result, &element);
                        }
                    } else {
                        // 将元素转为 JSON 字符串（使用双引号并正确转义）
                        append_escaped_string(&mut result, &element);
                    }
                } else {
                    // 非字符串元素（如数字、布尔值等）
                    while i < len && chars[i] != ',' && chars[i] != ']' && !chars[i].is_whitespace() {
                        result.push(chars[i]);
                        i += 1;
                    }
                }

                // 处理元素后的逗号
                if i < len && chars[i] == ',' {
                    result.push(',');
                    i += 1;
                }

                // 跳过空白字符
                while i < len && chars[i].is_whitespace() {
                    result.push(chars[i]);
                    i += 1;
                }
            }

            if i < len {
                result.push(']');
                i += 1;
            }
        } else {
            // 非数组内容，直接复制
            result.push(chars[i]);
            i += 1;
        }
    }

    if result != json_str {
        info!("🔧 修复了 JSON 数组中的混合引号");
    }

    result
}

/// 将字符串作为 JSON 字符串添加到结果中，正确转义特殊字符
fn append_escaped_string(result: &mut String, content: &str) {
    if let Ok(json_str_value) = serde_json::to_string(content) {
        result.push_str(&json_str_value);
    } else {
        // 如果序列化失败，手动转义
        result.push('"');
        for c in content.chars() {
            match c {
                '"' => result.push_str("\\\""),
                '\\' => result.push_str("\\\\"),
                '\n' => result.push_str("\\n"),
                '\r' => result.push_str("\\r"),
                '\t' => result.push_str("\\t"),
                '\x00'..='\x1f' => result.push_str(&format!("\\u{:04x}", c as u32)),
                _ => result.push(c),
            }
        }
        result.push('"');
    }
}

/// 尝试重构看起来像 heredoc 的数组元素
fn try_refactor_heredoc_element(content: &str) -> Option<String> {
    // 检查是否是 heredoc 模式
    if content.contains("<<") {
        // 提取命令部分
        let parts: Vec<&str> = content.splitn(2, ">>").collect();
        if parts.len() == 2 {
            let cmd_part = parts[0].trim();
            let heredoc_part = parts[1].trim();

            // 构造完整的 heredoc 命令
            let full_command = format!("{cmd_part} >> {heredoc_part}");
            return serde_json::to_string(&full_command).ok();
        }
    }
    None
}

/// 修复括号不匹配的问题
fn fix_bracket_issues(json_str: &str) -> String {
    let mut result = json_str.to_string();
    let mut open_braces: usize = 0;
    let mut open_brackets: usize = 0;
    let mut in_string = false;
    let mut escape_next = false;

    for c in result.chars() {
        if escape_next {
            escape_next = false;
            continue;
        }

        match c {
            '\\' if in_string => escape_next = true,
            '"' => in_string = !in_string,
            '{' if !in_string => open_braces += 1,
            '}' if !in_string => open_braces = open_braces.saturating_sub(1),
            '[' if !in_string => open_brackets += 1,
            ']' if !in_string => open_brackets = open_brackets.saturating_sub(1),
            _ => {}
        }
    }

    // 补充缺失的右括号
    for _ in 0..open_brackets {
        result.push(']');
    }
    for _ in 0..open_braces {
        result.push('}');
    }

    result
}

/// 修复尾部问题（如缺少的引号、逗号等）
fn fix_trailing_issues(json_str: &str) -> String {
    let mut result = json_str.trim().to_string();

    // 移除尾部的逗号
    while result.ends_with(',') {
        result.pop();
        result = result.trim().to_string();
    }

    // 移除多余的尾部引号（例如："]}" 后面还有引号）
    while result.len() > 2 {
        let last_chars = &result[result.len()-2..];
        if (last_chars == "]}" || last_chars == "]]" || last_chars == "}}")
            && result.ends_with('"') {
            result.pop();
        } else {
            break;
        }
    }

    // 检查是否在字符串中间结束
    let mut in_string = false;
    let mut escape_next = false;
    let mut string_quote = '\0';

    for c in result.chars() {
        if escape_next {
            escape_next = false;
            continue;
        }

        match c {
            '\\' if in_string => escape_next = true,
            '"' | '\'' if !escape_next => {
                if in_string && c == string_quote {
                    in_string = false;
                    string_quote = '\0';
                } else if !in_string {
                    in_string = true;
                    string_quote = c;
                }
            }
            _ => {}
        }
    }

    // 如果字符串未关闭，关闭它
    if in_string {
        result.push(string_quote);
    }

    result
}

/// 修复包含未转义换行符的JSON字符串值
fn fix_unescaped_newlines(json_str: &str) -> String {
    debug!("尝试修复未转义的换行符");

    // 这是一个更激进的修复策略，用于处理包含大量文本的情况
    // 特别适合处理包含patch、代码或其他多行文本的JSON

    // 首先尝试直接解析
    if serde_json::from_str::<serde_json::Value>(json_str).is_ok() {
        return json_str.to_string();
    }

    // 对于超大JSON，先尝试简单的字符串替换修复
    if json_str.len() > 10000 {
        debug!("检测到超大JSON ({} 字节)，尝试快速修复", json_str.len());

        // 查找 "command":"[ 模式，这通常表示字符串化的数组
        if json_str.contains("\"command\":\"[") {
            debug!("检测到字符串化的command数组，尝试修复");

            // 提取command字段的值
            if let Some(start) = json_str.find("\"command\":\"[") {
                let start = start + 12; // 跳过 "command":"

                // 查找结束的引号
                let mut end = None;
                let mut brace_count = 0;
                let mut in_string = false;
                let mut escape_next = false;

                for (i, c) in json_str[start..].chars().enumerate() {
                    if escape_next {
                        escape_next = false;
                        continue;
                    }

                    match c {
                        '\\' => escape_next = true,
                        '"' if !escape_next => {
                            in_string = !in_string;
                        }
                        '[' if !in_string => brace_count += 1,
                        ']' if !in_string => {
                            brace_count -= 1;
                            if brace_count == 0 {
                                // 找到了匹配的括号
                                end = Some(start + i + 1);
                                break;
                            }
                        }
                        _ => {}
                    }
                }

                if let Some(end_pos) = end {
                    // 提取整个JSON直到command字段结束
                    let before = &json_str[..start];
                    let command_str = &json_str[start..end_pos];
                    let after = &json_str[end_pos..];

                    debug!("Command字段长度: {}", command_str.len());

                    // 尝试解析这个字符串化的JSON数组
                    match serde_json::from_str::<serde_json::Value>(command_str) {
                        Ok(parsed_array) => {
                            info!("✅ 成功解析字符串化的command数组");
                            // 重建JSON，用解析后的数组替换字符串
                            let rebuilt = format!("{}{}{}",
                                before,
                                serde_json::to_string(&parsed_array).unwrap_or_default(),
                                after
                            );

                            // 验证重建的JSON是否有效
                            match serde_json::from_str::<serde_json::Value>(&rebuilt) {
                                Ok(_) => {
                                    info!("✅ 成功重建JSON结构");
                                    return rebuilt;
                                }
                                Err(e) => {
                                    debug!("重建JSON失败: {}", e);
                                }
                            }
                        }
                        Err(e) => {
                            debug!("解析command数组失败: {}", e);
                        }
                    }
                }
            }
        }
    }

    // 寻找并修复未转义的换行符
    let mut result = String::with_capacity(json_str.len() * 2);
    let mut in_string = false;
    let mut escape_next = false;
    let mut string_start_char = '\0';

    for (i, c) in json_str.chars().enumerate() {
        if escape_next {
            result.push(c);
            escape_next = false;
            continue;
        }

        match c {
            '\\' if in_string => {
                result.push('\\');
                escape_next = true;
            }
            '"' | '\'' if !escape_next => {
                if !in_string {
                    in_string = true;
                    string_start_char = c;
                } else if c == string_start_char {
                    in_string = false;
                    string_start_char = '\0';
                }
                result.push(c);
            }
            '\n' if in_string => {
                // 在字符串内遇到未转义的换行符
                debug!("在位置 {} 发现未转义的换行符，进行转义", i);
                result.push_str("\\n");
            }
            '\r' if in_string => {
                // 在字符串内遇到未转义的回车符
                result.push_str("\\r");
            }
            '\t' if in_string => {
                // 在字符串内遇到未转义的制表符
                result.push_str("\\t");
            }
            _ => {
                result.push(c);
            }
        }
    }

    // 尝试解析修复后的JSON
    match serde_json::from_str::<serde_json::Value>(&result) {
        Ok(_) => {
            info!("✅ 成功修复了未转义的换行符");
            result
        }
        Err(e) => {
            debug!("修复未转义换行符失败: {e}");
            json_str.to_string()
        }
    }
}

/// 修复包含复杂字符串化JSON的情况
fn fix_complex_stringified_json(json_str: &str) -> String {
    debug!("尝试修复复杂的字符串化JSON");

    // 对于特别大的JSON（包含大量patch内容），需要特殊处理
    if json_str.len() > 5000 {
        // 尝试使用正则表达式查找并修复字符串化的JSON数组
        // 这是一个简化的方法，专门处理常见的问题模式

        // 查找所有可能的字符串化数组字段
        let fields = ["command", "args", "files", "input"];

        for field in &fields {
            let pattern = format!("\"{field}\":\"\\[\"");
            if json_str.contains(&pattern) {
                debug!("检测到字段 {field} 包含字符串化的数组");

                // 使用更强大的方法来提取和修复
                if let Some(fixed) = attempt_fix_stringified_field(json_str, field) {
                    match serde_json::from_str::<serde_json::Value>(&fixed) {
                        Ok(_) => {
                            info!("✅ 成功修复字段 {field} 的字符串化JSON");
                            return fixed;
                        }
                        Err(e) => {
                            debug!("修复后仍然失败: {e}");
                        }
                    }
                }
            }
        }
    }

    json_str.to_string()
}

/// 尝试修复特定字段的字符串化JSON
fn attempt_fix_stringified_field(json_str: &str, field_name: &str) -> Option<String> {
    // 构建查找模式
    let start_pattern = format!("\"{field_name}\":\"");

    // 找到字段开始位置
    let field_start = json_str.find(&start_pattern)?;
    let value_start = field_start + start_pattern.len();

    // 找到值的结束位置（需要处理转义引号）
    let mut pos = value_start;
    let mut escape_count = 0;
    let mut in_string = true;

    while pos < json_str.len() {
        let ch = json_str.chars().nth(pos)?;

        if ch == '\\' && in_string {
            escape_count += 1;
            pos += 1;
        } else if ch == '"' && escape_count % 2 == 0 {
            // 找到非转义的引号
            in_string = false;
            break;
        } else {
            escape_count = 0;
        }

        pos += 1;
    }

    if in_string {
        // 没有找到结束引号
        return None;
    }

    // 提取字符串化的值
    let stringified_value = &json_str[value_start..pos];

    // 尝试解析这个字符串化的JSON
    match serde_json::from_str::<serde_json::Value>(stringified_value) {
        Ok(parsed_value) => {
            // 成功解析，重建JSON
            let before = &json_str[..field_start];
            let after = &json_str[pos + 1..];
            let parsed_str = serde_json::to_string(&parsed_value).ok()?;

            let rebuilt = format!("{before}{field_name}:{parsed_str}{after}");

            Some(rebuilt)
        }
        Err(_) => {
            // 如果直接解析失败，尝试先修复转义字符
            let mut fixed_value = stringified_value.to_string();

            // 修复常见的转义问题
            fixed_value = fixed_value.replace("\\\"", "\"");
            fixed_value = fixed_value.replace("\\\\", "\\");

            // 再次尝试解析
            match serde_json::from_str::<serde_json::Value>(&fixed_value) {
                Ok(parsed_value) => {
                    let before = &json_str[..field_start];
                    let after = &json_str[pos + 1..];
                    let parsed_str = serde_json::to_string(&parsed_value).ok()?;

                    let rebuilt = format!("{before}:{field_name}:{parsed_str}{after}");

                    Some(rebuilt)
                }
                Err(_) => None
            }
        }
    }
}

// ============================================================================
// Shell 操作符相关
// ============================================================================

/// Shell 操作符列表
pub const SHELL_OPERATORS: &[&str] = &[
    // 重定向操作符
    ">", ">>", "<", "<<", "<<<",
    // 文件描述符重定向
    "2>", "2>>", "&>", "&>>", "1>", "1>>",
    "2>&1", "1>&2",
    // 管道和逻辑操作符
    "|", "&&", "||", ";", "&",
    // 进程替换
    "<(", ">(",
];

/// 需要 shell 包装才能正确执行的特殊字符
pub const SHELL_SPECIAL_CHARS: &[char] = &[
    '>', '<', '|', '&', ';', '(', ')', '$', '`', '"', '\'', '\\', '\n',
    '*', '?', '[', ']', '#', '~', '!', '{', '}',
];

/// 检测命令字符串是否包含 shell 特殊语法
///
/// # Arguments
/// * `command` - 命令字符串
///
/// # Returns
/// 如果命令包含 shell 特殊语法（重定向、管道等）则返回 true
pub fn command_string_needs_shell(command: &str) -> bool {
    // 检查 shell 操作符
    for op in SHELL_OPERATORS {
        // 对于重定向操作符，检查是否在引号外
        if command.contains(op) && !is_in_quotes(command, op) {
            return true;
        }
    }

    // 检查 heredoc
    if contains_heredoc(command) {
        return true;
    }

    false
}

/// 检查操作符是否在引号内
fn is_in_quotes(command: &str, op: &str) -> bool {
    if let Some(pos) = command.find(op) {
        let before = &command[..pos];
        // 计算引号数量
        let single_quotes = before.chars().filter(|&c| c == '\'').count();
        let double_quotes = before.chars().filter(|&c| c == '"').count();
        // 如果引号数量是奇数，说明操作符在引号内
        single_quotes % 2 == 1 || double_quotes % 2 == 1
    } else {
        false
    }
}

/// 检测命令数组中是否包含 shell 操作符
///
/// # Arguments
/// * `command` - 命令参数数组
///
/// # Returns
/// 如果命令需要 shell 包装则返回 true
pub fn command_needs_shell_wrapping(command: &[String]) -> bool {
    command.iter().any(|arg| {
        // 精确匹配操作符
        SHELL_OPERATORS.contains(&arg.as_str()) ||
        // 检查以操作符开头的参数（如 ">file"、"2>&1"）
        SHELL_OPERATORS.iter().any(|op| {
            arg.starts_with(op) && arg.len() > op.len()
        }) ||
        // 检查包含需要 shell 解释的特殊字符
        arg.chars().any(|c| SHELL_SPECIAL_CHARS.contains(&c))
    })
}

/// 将命令数组正确转义并连接成 shell 命令字符串
///
/// # Arguments
/// * `command` - 命令参数数组
///
/// # Returns
/// 可以直接传给 shell -c 的命令字符串
pub fn join_command_for_shell(command: &[String]) -> String {
    // 首先预处理命令数组，分割合并的操作符参数
    let expanded = expand_operator_arguments(command);
    expanded
        .iter()
        .map(|arg| escape_shell_arg(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

/// 预处理命令数组，分割包含操作符的合并参数
///
/// 例如：
/// - `"> test.py"` -> `[">", "test.py"]`
/// - `"<< 'EOF'"` -> `["<<", "'EOF'"]`
/// - `">>output.txt"` -> `[">>", "output.txt"]`
fn expand_operator_arguments(command: &[String]) -> Vec<String> {
    let mut result = Vec::new();

    // 按优先级排序的重定向操作符（先匹配长的）
    let redirect_ops = ["<<<", "<<-", "<<", ">>", ">", "<"];

    for arg in command {
        let trimmed = arg.trim();

        // 检查是否以重定向操作符开头
        let mut found_op = None;
        for op in &redirect_ops {
            if trimmed.starts_with(op) {
                found_op = Some(*op);
                break;
            }
        }

        if let Some(op) = found_op {
            let rest = trimmed[op.len()..].trim();
            if !rest.is_empty() {
                // 操作符和参数合并在一起，需要分割
                result.push(op.to_string());
                result.push(rest.to_string());
                continue;
            }
        }

        result.push(arg.clone());
    }

    result
}

/// 转义单个 shell 参数
fn escape_shell_arg(arg: &str) -> String {
    // 如果是 shell 操作符，不需要转义
    if SHELL_OPERATORS.contains(&arg) {
        return arg.to_string();
    }

    // 如果参数已经被单引号或双引号包裹，保持原样
    // 例如 'EOF', "EOF", "'EOF'" 等
    if is_quoted_string(arg) {
        return arg.to_string();
    }

    // 如果不包含特殊字符，直接返回
    if !arg.chars().any(|c| SHELL_SPECIAL_CHARS.contains(&c) || c.is_whitespace()) {
        return arg.to_string();
    }

    // 使用单引号包裹，并转义内部的单引号
    format!("'{}'", arg.replace('\'', "'\\''"))
}

/// 检查字符串是否已经被引号包裹
fn is_quoted_string(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.len() < 2 {
        return false;
    }

    // 检查是否被单引号包裹
    if bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\'' {
        return true;
    }

    // 检查是否被双引号包裹
    if bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"' {
        return true;
    }

    false
}

// ============================================================================
// Heredoc 相关
// ============================================================================

/// Heredoc 警告类型
#[derive(Debug, Clone, PartialEq)]
pub enum HeredocWarning {
    /// 建议使用单引号包裹定界符以防止变量展开
    SuggestQuotedDelimiter {
        delimiter: String,
        reason: String,
    },
    /// 定界符包含特殊字符
    DelimiterHasSpecialChars {
        delimiter: String,
    },
    /// 内容中包含与定界符相似的行
    ContentMayConflictWithDelimiter {
        line: String,
        line_number: usize,
    },
    /// 标准输入相关警告（curl @-, git apply -, 等）
    StdinWarning {
        message: String,
    },
}

/// Heredoc 错误类型
#[derive(Debug, Clone, PartialEq)]
pub enum HeredocError {
    /// 多余的引号
    ExtraQuotes {
        found: String,
        suggestion: String,
    },
    /// 引号不匹配
    MismatchedQuotes {
        found: String,
        suggestion: String,
    },
    /// 缺少定界符
    MissingDelimiter,
    /// 空定界符
    EmptyDelimiter,
    /// 定界符格式无效
    InvalidDelimiterFormat {
        found: String,
        reason: String,
    },
    /// 缺少结束定界符
    MissingEndDelimiter {
        expected: String,
    },
    /// 结束定界符不匹配
    EndDelimiterMismatch {
        expected: String,
        found: String,
    },
    /// 定界符后有多余内容
    ExtraContentAfterDelimiter {
        delimiter: String,
        extra_content: String,
    },
    /// 结束符行有多余内容
    ExtraContentWithEndDelimiter {
        line: String,
        delimiter: String,
        line_number: usize,
    },
}

impl std::fmt::Display for HeredocWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HeredocWarning::SuggestQuotedDelimiter { delimiter, reason } => {
                write!(f, "建议使用 << '{delimiter}' 而不是 << {delimiter} ({reason})")
            }
            HeredocWarning::DelimiterHasSpecialChars { delimiter } => {
                write!(f, "定界符 '{delimiter}' 包含特殊字符，可能导致解析问题")
            }
            HeredocWarning::ContentMayConflictWithDelimiter { line, line_number } => {
                write!(f, "第 {line_number} 行内容 '{line}' 与定界符相似，可能导致提前结束")
            }
            HeredocWarning::StdinWarning { message } => {
                write!(f, "{message}")
            }
        }
    }
}

impl std::fmt::Display for HeredocError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HeredocError::ExtraQuotes { found, suggestion } => {
                write!(f, "多余的引号: '{found}', 建议修改为: '{suggestion}'")
            }
            HeredocError::MismatchedQuotes { found, suggestion } => {
                write!(f, "引号不匹配: '{found}', 建议修改为: '{suggestion}'")
            }
            HeredocError::MissingDelimiter => {
                write!(f, "缺少 heredoc 定界符")
            }
            HeredocError::EmptyDelimiter => {
                write!(f, "定界符不能为空")
            }
            HeredocError::InvalidDelimiterFormat { found, reason } => {
                write!(f, "定界符格式无效 '{found}': {reason}")
            }
            HeredocError::MissingEndDelimiter { expected } => {
                write!(f, "缺少结束定界符 '{expected}'")
            }
            HeredocError::EndDelimiterMismatch { expected, found } => {
                write!(f, "结束定界符不匹配: 期望 '{expected}', 实际 '{found}'")
            }
            HeredocError::ExtraContentAfterDelimiter { delimiter, extra_content } => {
                write!(f, "定界符 '{delimiter}' 后有多余内容: '{extra_content}', 应该在定界符后换行")
            }
            HeredocError::ExtraContentWithEndDelimiter { line, delimiter, line_number } => {
                write!(f, "第 {line_number} 行结束符 '{delimiter}' 不是单独一行: '{line}', 应该单独一行")
            }
        }
    }
}

/// Heredoc 验证结果
#[derive(Debug, Clone)]
pub struct HeredocValidationResult {
    /// 是否有效
    pub is_valid: bool,
    /// 检测到的警告
    pub warnings: Vec<HeredocWarning>,
    /// 检测到的错误
    pub errors: Vec<HeredocError>,
    /// 修复后的命令（如果可以自动修复）
    pub fixed_command: Option<String>,
    /// 原始命令
    pub original_command: String,
}

/// 匹配多余引号的 heredoc 模式
#[allow(clippy::unwrap_used)] // LazyLock regex patterns are compile-time constants
static HEREDOC_EXTRA_QUOTES: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"<<(-?)\s*(['"])(['"])+(\w+)(['"])*"#).unwrap()
});

/// 匹配引号不匹配的 heredoc 模式
#[allow(clippy::unwrap_used)] // LazyLock regex patterns are compile-time constants
static HEREDOC_MISMATCHED_QUOTES: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"<<(-?)\s*(['"])(\w+)(['"])"#).unwrap()
});

/// 解析的 Heredoc 结构
#[derive(Debug, Clone)]
pub struct ParsedHeredoc {
    /// 命令前缀（<< 之前的部分）
    pub command_prefix: String,
    /// 定界符
    pub delimiter: String,
    /// 原始定界符（可能带引号）
    pub original_delimiter: String,
    /// Heredoc 内容
    pub content: String,
    /// 是否使用 <<- 语法
    pub strip_tabs: bool,
}

/// 解析 heredoc 命令
///
/// # Arguments
/// * `command` - 完整的命令字符串
///
/// # Returns
/// 如果是有效的 heredoc 命令，返回解析结果
pub fn parse_heredoc(command: &str) -> Option<ParsedHeredoc> {
    let lines: Vec<&str> = command.lines().collect();
    if lines.is_empty() {
        return None;
    }

    let first_line = lines[0];

    // 查找 << 的位置
    let heredoc_pos = first_line.find("<<")?;
    let command_prefix = first_line[..heredoc_pos].to_string();
    let after_heredoc = &first_line[heredoc_pos + 2..];

    // 检查是否是 <<-
    let (strip_tabs, rest) = if let Some(after_dash) = after_heredoc.strip_prefix('-') {
        (true, after_dash.trim_start())
    } else {
        (false, after_heredoc.trim_start())
    };

    // 提取定界符
    let (delimiter, original_delimiter) = extract_delimiter(rest)?;

    // 提取内容（从第二行开始，到定界符行之前）
    let mut content_lines = Vec::new();
    let mut found_end = false;

    for line in lines.iter().skip(1) {
        let trimmed = if strip_tabs {
            line.trim_start_matches('\t')
        } else {
            *line
        };

        if trimmed.trim() == delimiter {
            found_end = true;
            break;
        }
        content_lines.push(*line);
    }

    if !found_end && lines.len() > 1 {
        // 没有找到结束定界符，但有内容
        debug!("Heredoc 缺少结束定界符: {delimiter}");
    }

    Some(ParsedHeredoc {
        command_prefix,
        delimiter,
        original_delimiter,
        content: content_lines.join("\n"),
        strip_tabs,
    })
}

/// 从字符串中提取定界符
fn extract_delimiter(s: &str) -> Option<(String, String)> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }

    // 检查是否有引号包裹
    if (s.starts_with('\'') && s.contains('\'')) || (s.starts_with('"') && s.contains('"')) {
        let quote = s.chars().next()?;
        let end_pos = s[1..].find(quote)?;
        let delimiter = s[1..=end_pos].to_string();
        let original = s[..=end_pos + 1].to_string();
        return Some((delimiter, original));
    }

    // 没有引号，取到第一个空白字符或行尾
    let delimiter: String = s.chars()
        .take_while(|c| !c.is_whitespace())
        .collect();

    if delimiter.is_empty() {
        return None;
    }

    Some((delimiter.clone(), delimiter))
}

/// 验证 heredoc 命令
///
/// # Arguments
/// * `command` - 命令字符串
///
/// # Returns
/// 验证结果，包含错误、警告和可能的修复
pub fn validate_heredoc(command: &str) -> HeredocValidationResult {
    let mut warnings = Vec::new();
    let mut errors = Vec::new();
    let mut fixed_command = None;
    let original_command = command.to_string();

    // 快速检查：不包含 << 则不是 heredoc
    if !command.contains("<<") {
        return HeredocValidationResult {
            is_valid: true,
            warnings,
            errors,
            fixed_command: None,
            original_command,
        };
    }

    let first_line = command.lines().next().unwrap_or("");

    // 检查多余引号
    if let Some(caps) = HEREDOC_EXTRA_QUOTES.captures(first_line) {
        let delimiter = caps.get(4).map(|m| m.as_str()).unwrap_or("");
        let suggestion = format!("'{delimiter}'");
        errors.push(HeredocError::ExtraQuotes {
            found: delimiter.to_string(),
            suggestion,
        });

        // 尝试修复
        let dash = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        let prefix = first_line.split("<<").next().unwrap_or("");
        let fixed_first = format!("{prefix}<<{dash} '{delimiter}'");
        let rest: Vec<&str> = command.lines().skip(1).collect();
        let joined = rest.join("\n");
        fixed_command = Some(format!("{fixed_first}\n{joined}"));
    }

    // 检查引号不匹配
    if let Some(caps) = HEREDOC_MISMATCHED_QUOTES.captures(first_line) {
        let open_quote = caps.get(2).map(|m| m.as_str()).unwrap_or("");
        let close_quote = caps.get(4).map(|m| m.as_str()).unwrap_or("");

        if open_quote != close_quote {
            let delimiter = caps.get(3).map(|m| m.as_str()).unwrap_or("");
            let found = format!("{open_quote}{delimiter}{close_quote}");
            let suggestion = format!("'{delimiter}'");
            errors.push(HeredocError::MismatchedQuotes {
                found,
                suggestion,
            });

            // 尝试修复
            let dash = caps.get(1).map(|m| m.as_str()).unwrap_or("");
            let prefix = first_line.split("<<").next().unwrap_or("");
            let fixed_first = format!("{prefix}<<{dash} '{delimiter}'");
            let rest: Vec<&str> = command.lines().skip(1).collect();
            let joined = rest.join("\n");
            fixed_command = Some(format!("{fixed_first}\n{joined}"));
        }
    }

    // 检查结束定界符
    if let Some(mut parsed) = parse_heredoc(command) {
        let content_lines: Vec<&str> = command.lines().skip(1).collect();
        let has_end_delimiter = content_lines.iter().any(|line| {
            let trimmed = if parsed.strip_tabs {
                line.trim_start_matches('\t')
            } else {
                *line
            };
            trimmed.trim() == parsed.delimiter
        });

        if !has_end_delimiter {
            errors.push(HeredocError::MissingEndDelimiter {
                expected: parsed.delimiter.clone(),
            });
        }

        // 检查内容中是否有变量但定界符没有引号
        if !parsed.original_delimiter.starts_with('\'') {
            let has_variables = parsed.content.contains('$') ||
                               parsed.content.contains('`');
            if has_variables {
                warnings.push(HeredocWarning::SuggestQuotedDelimiter {
                    delimiter: std::mem::take(&mut parsed.delimiter),
                    reason: "内容包含变量或命令替换，使用引号可防止意外展开".to_string(),
                });
            }
        }
    }

    HeredocValidationResult {
        is_valid: errors.is_empty(),
        warnings,
        errors,
        fixed_command,
        original_command,
    }
}

/// 验证并自动修复 heredoc 命令
///
/// # Arguments
/// * `command` - 命令字符串
///
/// # Returns
/// (修复后的命令, 验证结果)
pub fn validate_and_fix_heredoc(command: &str) -> (String, HeredocValidationResult) {
    let result = validate_heredoc(command);

    let final_command = if let Some(ref fixed) = result.fixed_command {
        info!("🔧 自动修复 heredoc 命令");
        debug!("  原始: {}", command.lines().next().unwrap_or(""));
        debug!("  修复: {}", fixed.lines().next().unwrap_or(""));
        fixed.clone()
    } else {
        command.to_string()
    };

    (final_command, result)
}

// ============================================================================
// 命令格式修复
// ============================================================================

/// 修复引号错误包裹重定向操作符的命令
///
/// 检测并修复形如 `cat '> file'` 或 `cat "> file"` 的错误格式
/// 这种情况下重定向操作符被错误地包含在引号内，被当作文件名参数
///
/// # Arguments
/// * `command` - 命令字符串
///
/// # Returns
/// 如果检测到错误格式，返回修复后的命令；否则返回 None
#[allow(clippy::unwrap_used)] // LazyLock regex patterns are compile-time constants
pub fn fix_quoted_redirect_operator(command: &str) -> Option<String> {
    let trimmed = command.trim();

    // 匹配 cat '> file' 模式（单引号）
    static SINGLE_QUOTE_REDIRECT: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"^(\S+)\s+'(>{1,2})\s*([^']+)'\s*$"#).unwrap()
    });

    // 匹配 cat "> file" 模式（双引号）
    static DOUBLE_QUOTE_REDIRECT: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"^(\S+)\s+"(>{1,2})\s*([^"]+)"\s*$"#).unwrap()
    });

    // 尝试匹配单引号模式
    if let Some(caps) = SINGLE_QUOTE_REDIRECT.captures(trimmed) {
        let cmd = caps.get(1)?.as_str();
        let redirect_op = caps.get(2)?.as_str();
        let file_path = caps.get(3)?.as_str().trim();

        let fixed = format!("{cmd} {redirect_op} {file_path}");
        info!("🔧 检测到单引号错误包裹重定向操作符，自动修复");
        debug!("  原始: {trimmed}");
        debug!("  修复: {fixed}");
        return Some(fixed);
    }

    // 尝试匹配双引号模式
    if let Some(caps) = DOUBLE_QUOTE_REDIRECT.captures(trimmed) {
        let cmd = caps.get(1)?.as_str();
        let redirect_op = caps.get(2)?.as_str();
        let file_path = caps.get(3)?.as_str().trim();

        let fixed = format!("{cmd} {redirect_op} {file_path}");
        info!("🔧 检测到双引号错误包裹重定向操作符，自动修复");
        debug!("  原始: {trimmed}");
        debug!("  修复: {fixed}");
        return Some(fixed);
    }

    // 匹配更复杂的情况（单引号）：cat 'content' '> file'
    static TRAILING_SINGLE_QUOTE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"^(.+)\s+'(>{1,2})\s*([^']+)'\s*$"#).unwrap()
    });

    // 匹配更复杂的情况（双引号）：cat "content" "> file"
    static TRAILING_DOUBLE_QUOTE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"^(.+)\s+"(>{1,2})\s*([^"]+)"\s*$"#).unwrap()
    });

    if let Some(caps) = TRAILING_SINGLE_QUOTE.captures(trimmed) {
        let prefix = caps.get(1)?.as_str();
        let redirect_op = caps.get(2)?.as_str();
        let file_path = caps.get(3)?.as_str().trim();

        let fixed = format!("{prefix} {redirect_op} {file_path}");
        info!("🔧 检测到末尾单引号错误包裹重定向操作符，自动修复");
        debug!("  原始: {trimmed}");
        debug!("  修复: {fixed}");
        return Some(fixed);
    }

    if let Some(caps) = TRAILING_DOUBLE_QUOTE.captures(trimmed) {
        let prefix = caps.get(1)?.as_str();
        let redirect_op = caps.get(2)?.as_str();
        let file_path = caps.get(3)?.as_str().trim();

        let fixed = format!("{prefix} {redirect_op} {file_path}");
        info!("🔧 检测到末尾双引号错误包裹重定向操作符，自动修复");
        debug!("  原始: {trimmed}");
        debug!("  修复: {fixed}");
        return Some(fixed);
    }

    None
}

/// 修复错误格式的 cat 命令
///
/// 检测并修复 `cat > file '内容'` 这种错误格式
/// 应该是 heredoc 或使用 echo
///
/// # Arguments
/// * `command` - 命令字符串
///
/// # Returns
/// 如果检测到错误格式，返回 (修复后的命令, stdin内容)
#[allow(clippy::unwrap_used)] // LazyLock regex patterns are compile-time constants
pub fn fix_malformed_cat_command(command: &str) -> Option<(String, String)> {
    let trimmed = command.trim();

    // 匹配 cat > file '内容' 或 cat >> file '内容' 模式
    static CAT_REDIRECT_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^(cat\s*>{1,2}\s*\S+)\s+").unwrap()
    });

    let caps = CAT_REDIRECT_PATTERN.captures(trimmed)?;
    let cat_command = caps.get(1)?.as_str().to_string();
    let remaining = &trimmed[caps.get(0)?.end()..];

    // 检查是否有引号包裹的内容
    let content = if let Some(after_single) = remaining.strip_prefix('\'') {
        // 单引号包裹
        let end = after_single.find('\'')?;
        after_single[..end].to_string()
    } else if let Some(after_double) = remaining.strip_prefix('"') {
        // 双引号包裹
        let end = after_double.find('"')?;
        after_double[..end].to_string()
    } else {
        return None;
    };

    // 处理转义字符
    let content = content
        .replace("\\n", "\n")
        .replace("\\t", "\t")
        .replace("\\\\", "\\");

    info!("🔧 检测到错误的 cat 命令格式，自动修复");
    debug!("  命令: {cat_command}");
    debug!("  内容长度: {} 字节", content.len());

    Some((cat_command, content))
}

/// 检测命令是否需要 stdin 输入
///
/// # Arguments
/// * `command` - 命令字符串
///
/// # Returns
/// (是否是 heredoc, 定界符)
#[allow(clippy::unwrap_used)] // LazyLock regex patterns are compile-time constants
pub fn detect_stdin_input_command(command: &str) -> Option<(bool, Option<String>)> {
    let trimmed = command.trim();

    // 检测 heredoc
    if let Some(parsed) = parse_heredoc(trimmed) {
        return Some((true, Some(parsed.delimiter)));
    }

    // 检测简单重定向 cat > file, cat >> file
    static SIMPLE_REDIRECT: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^cat\s*>{1,2}\s*\S+\s*$").unwrap()
    });

    if SIMPLE_REDIRECT.is_match(trimmed) {
        return Some((false, None));
    }

    // 检测 echo '...' > file 这类一次性命令
    static ONESHOT_REDIRECT: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^(?:echo|printf)\s+.*>{1,2}\s*\S+\s*$").unwrap()
    });

    if ONESHOT_REDIRECT.is_match(trimmed) {
        return Some((false, Some("__ONESHOT__".to_string())));
    }

    None
}

// ============================================================================
// 命令验证和处理
// ============================================================================

/// 命令处理结果
#[derive(Debug, Clone)]
pub struct CommandProcessResult {
    /// 处理后的命令
    pub command: Vec<String>,
    /// 是否需要 shell 包装
    pub needs_shell: bool,
    /// 如果需要 shell，这是完整的命令字符串
    pub shell_command: Option<String>,
    /// 检测到的 stdin 内容
    pub stdin_content: Option<String>,
    /// 警告信息
    pub warnings: Vec<String>,
    /// 错误信息
    pub errors: Vec<String>,
}

/// 检测并重构数组格式的 heredoc 命令
///
/// 当 AI 模型将 heredoc 命令拆分为数组元素时，需要重构为正确的 heredoc 格式。
/// 例如：["cat", "<<", "EOF", "line1", "line2", "EOF"]
/// 应重构为：cat << EOF\nline1\nline2\nEOF
///
/// # Arguments
/// * `command` - 命令参数数组
///
/// # Returns
/// 如果检测到数组格式的 heredoc，返回重构后的命令字符串
fn reconstruct_array_heredoc(command: &[String]) -> Option<String> {
    // 查找 << 或 <<- 的位置
    let heredoc_idx = command.iter().position(|arg| {
        arg == "<<" || arg == "<<-" || arg.starts_with("<<")
    })?;

    let heredoc_op = &command[heredoc_idx];

    // 提取定界符
    let (strip_tabs, delimiter_idx, delimiter) = if heredoc_op == "<<" || heredoc_op == "<<-" {
        // 定界符在下一个元素
        if heredoc_idx + 1 >= command.len() {
            return None;
        }
        let strip = heredoc_op == "<<-";
        let delim = command[heredoc_idx + 1].trim_matches(|c| c == '\'' || c == '"').to_string();
        (strip, heredoc_idx + 1, delim)
    } else {
        // << 后面直接跟定界符，如 "<<EOF" 或 "<<'EOF'"
        let rest = heredoc_op.strip_prefix("<<-")
            .or_else(|| heredoc_op.strip_prefix("<<"))
            .unwrap_or("");
        let strip = heredoc_op.starts_with("<<-");
        let delim = rest.trim().trim_matches(|c| c == '\'' || c == '"').to_string();
        if delim.is_empty() {
            return None;
        }
        (strip, heredoc_idx, delim)
    };

    // 查找结束定界符的位置
    let end_idx = command.iter()
        .skip(delimiter_idx + 1)
        .position(|arg| arg.trim() == delimiter)
        .map(|i| i + delimiter_idx + 1)?;

    // 提取命令前缀
    let prefix: Vec<&str> = command[..heredoc_idx].iter().map(String::as_str).collect();
    let prefix_str = if prefix.is_empty() {
        String::new()
    } else {
        prefix.join(" ") + " "
    };

    // 提取 heredoc 内容（定界符之后到结束定界符之前）
    let content_start = delimiter_idx + 1;
    let content: Vec<&str> = command[content_start..end_idx].iter().map(String::as_str).collect();

    // 构建正确的 heredoc 命令
    let heredoc_prefix = if strip_tabs { "<<-" } else { "<<" };
    let quoted_delimiter = format!("'{delimiter}'");

    let mut result = format!("{prefix_str}{heredoc_prefix} {quoted_delimiter}");
    for line in content {
        result.push('\n');
        result.push_str(line);
    }
    result.push('\n');
    result.push_str(&delimiter);

    info!("🔧 检测到数组格式的 heredoc，重构命令");
    debug!("  原始数组: {command:?}");
    let first_line = result.lines().next().unwrap_or("");
    debug!("  重构后: {first_line}");

    Some(result)
}

/// 处理命令数组，进行必要的验证和修复
///
/// # Arguments
/// * `command` - 命令参数数组
///
/// # Returns
/// 处理结果，包含修复后的命令和相关信息
pub fn process_command(command: Vec<String>) -> CommandProcessResult {
    let mut warnings = Vec::new();
    let mut errors = Vec::new();
    let mut stdin_content = None;

    // 首先检测并修复 apply_patch 命令格式
    if let Some(fixed_command) = fix_apply_patch_command(&command) {
        info!("🔧 检测到 apply_patch 命令，修复格式");
        return CommandProcessResult {
            command: fixed_command,
            needs_shell: false,
            shell_command: None,
            stdin_content: None,
            warnings,
            errors,
        };
    }

    // 🔧 特殊处理：单元素命令且包含 heredoc
    // 当命令反序列化时检测到 heredoc，会保持为单个字符串
    // 这种情况下直接使用该字符串作为 shell_command，不需要额外处理
    if command.len() == 1 && contains_heredoc(&command[0]) {
        let cmd_str = &command[0];
        info!("🔧 检测到单元素 heredoc 命令，直接使用");

        // 验证 heredoc 格式
        let (fixed_command, result) = validate_and_fix_heredoc(cmd_str);
        warnings.extend(result.warnings.iter().map(ToString::to_string));
        errors.extend(result.errors.iter().map(ToString::to_string));

        return CommandProcessResult {
            command: vec![fixed_command.clone()],
            needs_shell: true,
            shell_command: Some(fixed_command),
            stdin_content: None,
            warnings,
            errors,
        };
    }

    // 检测数组格式的 heredoc
    if let Some(reconstructed) = reconstruct_array_heredoc(&command) {
        // 对重构后的命令进行 heredoc 验证
        let (fixed_command, result) = validate_and_fix_heredoc(&reconstructed);
        warnings.extend(result.warnings.iter().map(ToString::to_string));
        errors.extend(result.errors.iter().map(ToString::to_string));

        return CommandProcessResult {
            command: vec![fixed_command.clone()],
            needs_shell: true,
            shell_command: Some(fixed_command),
            stdin_content: None,
            warnings,
            errors,
        };
    }

    // 检查是否需要 shell 包装
    let needs_shell = command_needs_shell_wrapping(&command);

    if needs_shell {
        // 将命令数组连接成字符串
        let command_str = join_command_for_shell(&command);

        // 首先检测引号错误包裹重定向操作符的情况
        // 如 cat '> file' 应该修复为 cat > file
        if let Some(fixed) = fix_quoted_redirect_operator(&command_str) {
            return CommandProcessResult {
                command: vec![fixed.clone()],
                needs_shell: true,
                shell_command: Some(fixed),
                stdin_content: None,
                warnings,
                errors,
            };
        }

        // 检测是否是错误格式的 cat 命令
        if let Some((fixed_cmd, content)) = fix_malformed_cat_command(&command_str) {
            stdin_content = Some(content);
            return CommandProcessResult {
                command: vec![fixed_cmd],
                needs_shell: true,
                shell_command: None,
                stdin_content,
                warnings,
                errors,
            };
        }

        // 验证 heredoc
        let (fixed_command, result) = validate_and_fix_heredoc(&command_str);
        // 将 HeredocWarning 和 HeredocError 转换为字符串
        warnings.extend(result.warnings.iter().map(ToString::to_string));
        errors.extend(result.errors.iter().map(ToString::to_string));

        return CommandProcessResult {
            command,
            needs_shell: true,
            shell_command: Some(fixed_command),
            stdin_content,
            warnings,
            errors,
        };
    }

    // 不需要 shell 包装的简单命令
    CommandProcessResult {
        command,
        needs_shell: false,
        shell_command: None,
        stdin_content,
        warnings,
        errors,
    }
}

/// 快速检查命令是否包含 heredoc
pub fn contains_heredoc(command: &str) -> bool {
    command.contains("<<")
}

/// 快速修复 heredoc 命令
pub fn quick_fix_heredoc(command: &str) -> String {
    let (fixed, _) = validate_and_fix_heredoc(command);
    fixed
}

/// 检查 heredoc 命令是否有效
pub fn is_valid_heredoc(command: &str) -> bool {
    if !contains_heredoc(command) {
        return true; // 非 heredoc 命令视为有效
    }
    validate_heredoc(command).is_valid
}

// ============================================================================
// 输入提示检测
// ============================================================================

/// 输入提示类型
#[derive(Debug, Clone, PartialEq)]
pub enum InputPromptType {
    /// 选择提示（Choose (0-7):）
    Choice,
    /// 确认提示（Are you sure? [y/N]）
    Confirmation,
    /// 按键继续（Press Enter to continue）
    PressToContinue,
    /// 普通输入（Enter filename:）
    Input,
    /// 密码输入（Password:）
    Password,
    /// Yes/No/Cancel 选择
    YesNoCancel,
    /// 分页继续（-- more --）
    Pagination,
    /// 等待状态（Waiting for...）
    Waiting,
    /// 调试器提示（(gdb)）
    Debugger,
    /// 其他
    Other,
}

/// 提示严重程度
#[derive(Debug, Clone, PartialEq)]
pub enum PromptSeverity {
    /// 信息提示
    Info,
    /// 警告
    Warning,
    /// 错误
    Error,
}

/// 输入提示
#[derive(Debug, Clone)]
pub struct InputPrompt {
    /// 提示行内容
    pub line: String,
    /// 提示类型
    pub prompt_type: InputPromptType,
    /// 严重程度
    pub severity: PromptSeverity,
}

/// 输入提示结果
#[derive(Debug, Clone)]
pub struct InputPromptResult {
    /// 是否检测到等待输入
    pub is_waiting: bool,
    /// 检测到的所有提示
    pub prompts: Vec<InputPrompt>,
    /// 完整的输出内容
    pub last_output: String,
}

/// 匹配等待用户输入的提示模式
#[allow(clippy::unwrap_used)] // LazyLock regex patterns are compile-time constants
static INPUT_PROMPT_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    vec![
        // 选择提示
        Regex::new(r"(?i)choose\s*\(\d+-\d+\):\s*$").unwrap(),
        Regex::new(r"(?i)select\s+\d+-\d+.*:\s*$").unwrap(),
        Regex::new(r"(?i)enter\s+choice\s*\d+-\d+.*:\s*$").unwrap(),
        Regex::new(r"(?i)please\s+choose.*:\s*$").unwrap(),
        Regex::new(r"(?i)option\s*\d+-\d+.*:\s*$").unwrap(),

        // 确认提示
        Regex::new(r"(?i)are\s+you\s+sure\?\s*\[\s*[ynYN][/\)]?\s*$").unwrap(),
        Regex::new(r"(?i)confirm\?\s*\[\s*[ynYN][/\)]?\s*$").unwrap(),
        Regex::new(r"(?i)continue\?\s*\[\s*[ynYN][/\)]?\s*$").unwrap(),
        Regex::new(r"(?i)proceed\?\s*\[\s*[ynYN][/\)]?\s*$").unwrap(),
        Regex::new(r"(?i)\[y/n\]\s*$").unwrap(),
        Regex::new(r"(?i)\(y/n\)\s*$").unwrap(),
        Regex::new(r"(?i)y/n\s*$").unwrap(),

        // 按键继续提示
        Regex::new(r"(?i)press\s+(?:any )?key\s+to\s+continue.*\s*$").unwrap(),
        Regex::new(r"(?i)press\s+enter\s+to\s+continue.*\s*$").unwrap(),
        Regex::new(r"(?i)press\s+return\s+to\s+continue.*\s*$").unwrap(),
        Regex::new(r"(?i)continue\s+by\s+pressing.*\s*$").unwrap(),
        Regex::new(r"(?i)hit\s+enter\s+to\s+continue.*\s*$").unwrap(),
        Regex::new(r"(?i)press\s+space\s+to\s+continue.*\s*$").unwrap(),
        Regex::new(r"(?i)space\s+to\s+continue.*\s*$").unwrap(),

        // 输入提示
        Regex::new(r"(?i)enter\s+.*:\s*$").unwrap(),
        Regex::new(r"(?i)input\s+.*:\s*$").unwrap(),
        Regex::new(r"(?i)please\s+enter\s+.*:\s*$").unwrap(),
        Regex::new(r"(?i)provide\s+.*:\s*$").unwrap(),
        Regex::new(r"(?i)specify\s+.*:\s*$").unwrap(),
        Regex::new(r"(?i)type\s+.*:\s*$").unwrap(),

        // 密码提示
        Regex::new(r"(?i)password[:\s]*$").unwrap(),
        Regex::new(r"(?i)enter\s+password[:\s]*$").unwrap(),
        Regex::new(r"(?i)passphrase[:\s]*$").unwrap(),
        Regex::new(r"(?i)enter\s+passphrase[:\s]*$").unwrap(),

        // 文件名提示
        Regex::new(r"(?i)filename[:\s]*$").unwrap(),
        Regex::new(r"(?i)enter\s+filename[:\s]*$").unwrap(),
        Regex::new(r"(?i)file\s+name[:\s]*$").unwrap(),

        // 路径提示
        Regex::new(r"(?i)path[:\s]*$").unwrap(),
        Regex::new(r"(?i)directory[:\s]*$").unwrap(),
        Regex::new(r"(?i)folder[:\s]*$").unwrap(),
        Regex::new(r"(?i)destination[:\s]*$").unwrap(),

        // Yes/No/Cancel 选择
        Regex::new(r"(?i)\[yes\]\s*$").unwrap(),
        Regex::new(r"(?i)\[no\]\s*$").unwrap(),
        Regex::new(r"(?i)\[cancel\]\s*$").unwrap(),
        Regex::new(r"(?i)\(yes\)\s*$").unwrap(),
        Regex::new(r"(?i)\(no\)\s*$").unwrap(),
        Regex::new(r"(?i)\(cancel\)\s*$").unwrap(),

        // 更多/继续提示
        Regex::new(r"(?i)--\s*more\s*--\s*$").unwrap(),
        Regex::new(r"(?i)\(more\)\s*$").unwrap(),
        Regex::new(r"(?i)\[more\]\s*$").unwrap(),

        // 分页提示
        Regex::new(r"(?i)q(uit)?\s+to\s+continue.*\s*$").unwrap(),
        Regex::new(r"(?i)next\s+page.*\s*$").unwrap(),
        Regex::new(r"(?i)page\s+\d+.*\s*$").unwrap(),

        // 安装/配置提示
        Regex::new(r"(?i)install.*\?\s*$").unwrap(),
        Regex::new(r"(?i)configure.*\?\s*$").unwrap(),
        Regex::new(r"(?i)setup.*\?\s*$").unwrap(),
        Regex::new(r"(?i)proceed\s+with\s+installation.*\?\s*$").unwrap(),

        // 覆盖/删除确认
        Regex::new(r"(?i)overwrite.*\?\s*$").unwrap(),
        Regex::new(r"(?i)delete.*\?\s*$").unwrap(),
        Regex::new(r"(?i)remove.*\?\s*$").unwrap(),
        Regex::new(r"(?i)confirm\s+delete.*\?\s*$").unwrap(),

        // 网络相关提示
        Regex::new(r"(?i)connect.*\?\s*$").unwrap(),
        Regex::new(r"(?i)download.*\?\s*$").unwrap(),
        Regex::new(r"(?i)fetch.*\?\s*$").unwrap(),

        // 更精确的通用提示模式
        Regex::new(r"(?i)^(do|does|did|is|are|was|were|will|would|can|could|should|shall|may|might|have|has|had)\s+.*\?\s*$").unwrap(),
        Regex::new(r"(?i)^(what|which|who|where|when|why|how)\s+.*\?\s*$").unwrap(),
        Regex::new(r"(?i)(want|like|wish|need|ready|sure|agree|accept|allow|enable|disable|create|update|modify|change|replace|save|load|use|run|execute|start|stop|quit|exit|abort|retry|skip|ignore)\s*.*\?\s*$").unwrap(),

        // 冒号提示
        Regex::new(r"(?i)^.{0,50}(name|value|input|answer|response|reply|text|string|number|code|key|token|id|user|username|login|email|address|host|port|url|uri|server|database|table|file|dir|folder)s?[:\s]*$").unwrap(),
        Regex::new(r"(?i)^[a-z][a-z0-9\s]{0,30}:\s*$").unwrap(),
        Regex::new(r"(?i)^\s*>\s*$").unwrap(),
        Regex::new(r"(?i)^\s*\$\s*$").unwrap(),
        Regex::new(r"(?i)^\s*#\s*$").unwrap(),
    ]
});

/// 匹配等待命令状态（没有明确的输入提示）
#[allow(clippy::unwrap_used)] // LazyLock regex patterns are compile-time constants
static WAITING_STATES: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    vec![
        // 等待状态模式
        Regex::new(r"(?i)^waiting\s+for.*$").unwrap(),
        Regex::new(r"(?i)^please\s+wait.*$").unwrap(),
        Regex::new(r"(?i)^processing\s*\.{0,3}\s*$").unwrap(),
        Regex::new(r"(?i)^loading\s*\.{0,3}\s*$").unwrap(),

        // 暂停模式
        Regex::new(r"(?i)paused\s*$").unwrap(),
        Regex::new(r"(?i)press\s+any\s+key\s+to\s+resume.*\s*$").unwrap(),

        // 调试器提示
        Regex::new(r"\([a-zA-Z_][a-zA-Z0-9_]*\)").unwrap(),
        Regex::new(r"(gdb|lldb|dbx)\s*[>\\$]").unwrap(),
        Regex::new(r"^\s*>?\s*$").unwrap(),
    ]
});

/// 检查输出是否包含等待用户输入的提示
///
/// 用于识别命令正在等待输入的情况，例如：
/// - Choose (0-7): Press Enter to continue...
/// - Are you sure? [y/N]
/// - Password:
/// - Enter filename:
pub fn detect_input_prompt(output: &str) -> InputPromptResult {
    let mut prompts = Vec::new();
    let mut is_waiting = false;

    // 按行检查输出
    for line in output.lines() {
        let trimmed = line.trim();

        // 跳过空行
        if trimmed.is_empty() {
            continue;
        }

        // 先检查是否是调试器提示
        if trimmed.starts_with('(') && trimmed.ends_with(')') ||
           trimmed.to_lowercase().contains("gdb") ||
           trimmed.to_lowercase().contains("lldb") ||
           trimmed.to_lowercase().contains("dbx") {
            prompts.push(InputPrompt {
                line: trimmed.to_string(),
                prompt_type: InputPromptType::Debugger,
                severity: PromptSeverity::Info,
            });
            is_waiting = true;
            continue;
        }

        // 检查是否匹配输入提示模式
        let mut matched_prompt = false;
        for pattern in INPUT_PROMPT_PATTERNS.iter() {
            if pattern.is_match(trimmed) {
                prompts.push(InputPrompt {
                    line: trimmed.to_string(),
                    prompt_type: classify_prompt_type(trimmed),
                    severity: PromptSeverity::Info,
                });
                is_waiting = true;
                matched_prompt = true;
                break;
            }
        }

        // 如果没有匹配输入提示，再检查等待状态
        if !matched_prompt {
            for pattern in WAITING_STATES.iter() {
                // 跳过调试器提示模式，避免重复
                if pattern.as_str().contains("(gdb)") ||
                   pattern.as_str().contains("gdb>") {
                    continue;
                }

                if pattern.is_match(trimmed) {
                    prompts.push(InputPrompt {
                        line: trimmed.to_string(),
                        prompt_type: InputPromptType::Waiting,
                        severity: PromptSeverity::Warning,
                    });
                    is_waiting = true;
                    break;
                }
            }
        }
    }

    InputPromptResult {
        is_waiting,
        prompts,
        last_output: output.to_string(),
    }
}

/// 分类提示类型
fn classify_prompt_type(line: &str) -> InputPromptType {
    let lower = line.to_lowercase();

    // 调试器提示 - 需要最先检查
    if line.starts_with('(') && line.ends_with(')') ||
       lower.contains("gdb") || lower.contains("lldb") || lower.contains("dbx") {
        return InputPromptType::Debugger;
    }

    // 选择提示
    if lower.contains("choose") || lower.contains("select") ||
       lower.contains("option") || lower.contains("choice") {
        return InputPromptType::Choice;
    }

    // 按键继续 - 需要在确认提示之前检查
    if (lower.contains("press") && (lower.contains("continue") || lower.contains("enter"))) ||
       lower.contains("hit enter") || lower.contains("space to continue") {
        return InputPromptType::PressToContinue;
    }

    // 确认提示
    if lower.contains("are you sure") || lower.contains("confirm") ||
       lower.contains("[y/n]") || lower.contains("(y/n)") {
        return InputPromptType::Confirmation;
    }

    // 通用 continue 检查需要更严格
    if lower.contains("continue?") && !lower.contains("press") {
        return InputPromptType::Confirmation;
    }

    // 分页
    if lower.contains("-- more --") || lower.contains("next page") ||
       lower.contains("page ") {
        return InputPromptType::Pagination;
    }

    // 密码提示
    if lower.contains("password") || lower.contains("passphrase") {
        return InputPromptType::Password;
    }

    // 文件名提示
    if lower.contains("filename") || lower.contains("file name") {
        return InputPromptType::Input;
    }

    // 路径提示
    if lower.contains("path") || lower.contains("directory") ||
       lower.contains("folder") || lower.contains("destination") {
        return InputPromptType::Input;
    }

    // Yes/No/Cancel
    if lower.contains("[yes]") || lower.contains("[no]") || lower.contains("[cancel]") ||
       lower.contains("(yes)") || lower.contains("(no)") || lower.contains("(cancel)") {
        return InputPromptType::YesNoCancel;
    }

    // 等待状态
    if lower.contains("waiting") || lower.contains("please wait") ||
       lower.contains("processing") || lower.contains("loading") ||
       lower.contains("paused") {
        return InputPromptType::Waiting;
    }

    // 分页
    if lower.contains("-- more --") || lower.contains("next page") ||
       lower.contains("page ") {
        return InputPromptType::Pagination;
    }

    // 默认为普通输入
    InputPromptType::Input
}

// ============================================================================
// apply_patch 命令修复
// ============================================================================

/// 修复 apply_patch 命令格式
///
/// AI 模型可能发送以下格式的 apply_patch 命令：
/// 1. 单个字符串: ["apply_patch '*** Begin Patch..."]
/// 2. 错误分割的数组: ["apply_patch", "'***", "Begin", "Patch", ...]
/// 3. 带引号的 patch: ["apply_patch", "'*** Begin Patch...'"]
///
/// 此函数检测这些格式并重构为正确的 ["apply_patch", "patch_content"] 格式
fn fix_apply_patch_command(command: &[String]) -> Option<Vec<String>> {
    if command.is_empty() {
        return None;
    }

    // 检查第一个元素是否是 apply_patch 命令
    let first = command[0].trim();

    // 情况 1: 单个字符串包含整个命令
    // 如 "apply_patch '*** Begin Patch..."
    if first.starts_with("apply_patch ") || first.starts_with("applypatch ") {
        // 分离命令和参数
        let parts: Vec<&str> = first.splitn(2, char::is_whitespace).collect();
        if parts.len() == 2 {
            let patch_content = parts[1].trim();
            // 移除可能的外层引号
            let patch = strip_outer_quotes(patch_content);
            return Some(vec!["apply_patch".to_string(), patch]);
        }
    }

    // 情况 2 和 3: 第一个元素是 "apply_patch" 或 "applypatch"
    if first == "apply_patch" || first == "applypatch" {
        if command.len() == 2 {
            // 已经是正确格式，检查是否需要去除引号
            let patch = strip_outer_quotes(&command[1]);
            if patch != command[1] {
                return Some(vec!["apply_patch".to_string(), patch]);
            }
            // 已经是正确格式
            return None;
        }

        if command.len() > 2 {
            // 错误分割的数组，需要重新合并
            let patch_parts: Vec<&str> = command[1..].iter().map(String::as_str).collect();
            let patch_content = patch_parts.join(" ");
            let patch = strip_outer_quotes(&patch_content);
            return Some(vec!["apply_patch".to_string(), patch]);
        }
    }

    None
}

/// 移除字符串的外层引号
fn strip_outer_quotes(s: &str) -> String {
    let trimmed = s.trim();
    let len = trimmed.len();

    if len < 2 {
        return trimmed.to_string();
    }

    // 检查是否有匹配的外层引号
    // Safety: len >= 2, so first and last chars exist
    let Some(first_char) = trimmed.chars().next() else {
        return trimmed.to_string();
    };
    let Some(last_char) = trimmed.chars().last() else {
        return trimmed.to_string();
    };

    if (first_char == '\'' && last_char == '\'') ||
       (first_char == '"' && last_char == '"') {
        return trimmed[1..len-1].to_string();
    }

    // 检查是否只有开始引号（未闭合的引号）
    if first_char == '\'' || first_char == '"' {
        // 可能是未闭合的引号，移除开始引号
        return trimmed[1..].to_string();
    }

    trimmed.to_string()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_command_needs_shell_wrapping() {
        // 需要 shell 包装的命令
        assert!(command_needs_shell_wrapping(&[
            "cat".to_string(),
            ">".to_string(),
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

        // 不需要 shell 包装的简单命令
        assert!(!command_needs_shell_wrapping(&[
            "ls".to_string(),
            "-la".to_string()
        ]));

        assert!(!command_needs_shell_wrapping(&[
            "cat".to_string(),
            "file.txt".to_string()
        ]));
    }

    #[test]
    fn test_join_command_for_shell() {
        assert_eq!(
            join_command_for_shell(&[
                "cat".to_string(),
                ">".to_string(),
                "file.txt".to_string()
            ]),
            "cat > file.txt"
        );

        assert_eq!(
            join_command_for_shell(&[
                "echo".to_string(),
                "hello world".to_string(),
                ">".to_string(),
                "file.txt".to_string()
            ]),
            "echo 'hello world' > file.txt"
        );
    }

    #[test]
    fn test_escape_shell_arg() {
        assert_eq!(escape_shell_arg("simple"), "simple");
        assert_eq!(escape_shell_arg(">"), ">");
        assert_eq!(escape_shell_arg("hello world"), "'hello world'");
        assert_eq!(escape_shell_arg("it's"), "'it'\\''s'");
    }

    #[test]
    fn test_parse_heredoc() {
        let cmd = "cat > file.txt << 'EOF'\nhello\nworld\nEOF";
        let parsed = parse_heredoc(cmd);
        assert!(parsed.is_some());
        let parsed = parsed.unwrap();
        assert_eq!(parsed.delimiter, "EOF");
        assert_eq!(parsed.content, "hello\nworld");
    }

    #[test]
    fn test_validate_heredoc() {
        // 有效的 heredoc
        let result = validate_heredoc("cat << EOF\nhello\nEOF");
        assert!(result.is_valid);

        // 缺少结束定界符
        let result = validate_heredoc("cat << EOF\nhello");
        assert!(!result.is_valid);
    }

    #[test]
    fn test_fix_malformed_cat_command() {
        let result = fix_malformed_cat_command("cat > file.txt 'hello\\nworld'");
        assert!(result.is_some());
        let (cmd, content) = result.unwrap();
        assert_eq!(cmd, "cat > file.txt");
        assert_eq!(content, "hello\nworld");
    }

    #[test]
    fn test_process_command() {
        // 简单命令
        let result = process_command(vec!["ls".to_string(), "-la".to_string()]);
        assert!(!result.needs_shell);

        // 需要 shell 的命令
        let result = process_command(vec![
            "cat".to_string(),
            ">".to_string(),
            "file.txt".to_string()
        ]);
        assert!(result.needs_shell);
        assert!(result.shell_command.is_some());
    }

    #[test]
    fn test_fix_quoted_redirect_operator() {
        // 测试 cat '> file' 模式
        let result = fix_quoted_redirect_operator("cat '> scripts/newdoc.sh'");
        assert!(result.is_some());
        assert_eq!(result.unwrap(), "cat > scripts/newdoc.sh");

        // 测试 cat "> file" 模式（双引号）
        let result = fix_quoted_redirect_operator("cat \"> scripts/newdoc.sh\"");
        assert!(result.is_some());
        assert_eq!(result.unwrap(), "cat > scripts/newdoc.sh");

        // 测试 cat '>> file' 模式（追加）
        let result = fix_quoted_redirect_operator("cat '>> scripts/newdoc.sh'");
        assert!(result.is_some());
        assert_eq!(result.unwrap(), "cat >> scripts/newdoc.sh");

        // 正常命令不应该被修改
        let result = fix_quoted_redirect_operator("cat > scripts/newdoc.sh");
        assert!(result.is_none());

        // 正常的 cat 读取文件命令不应该被修改
        let result = fix_quoted_redirect_operator("cat file.txt");
        assert!(result.is_none());
    }

    #[test]
    fn test_process_command_with_quoted_redirect() {
        // 测试 process_command 处理引号错误包裹重定向的情况
        // 模拟 AI 发送 ["cat", "'> scripts/newdoc.sh'"] 的情况
        let result = process_command(vec![
            "cat".to_string(),
            "'> scripts/newdoc.sh'".to_string()
        ]);
        assert!(result.needs_shell);
        assert!(result.shell_command.is_some());
        // 应该被修复为 cat > scripts/newdoc.sh
        assert_eq!(result.shell_command.unwrap(), "cat > scripts/newdoc.sh");
    }

    #[test]
    fn test_reconstruct_array_heredoc() {
        // 测试数组格式的 heredoc 重构
        // 模拟 AI 发送 ["cat", "<<", "EOF", "line1", "line2", "EOF"] 的情况
        let result = reconstruct_array_heredoc(&[
            "cat".to_string(),
            "<<".to_string(),
            "EOF".to_string(),
            "hello world".to_string(),
            "goodbye".to_string(),
            "EOF".to_string(),
        ]);
        assert!(result.is_some());
        let cmd = result.unwrap();
        assert!(cmd.contains("cat << 'EOF'"));
        assert!(cmd.contains("hello world"));
        assert!(cmd.contains("goodbye"));
        assert!(cmd.ends_with("EOF"));

        // 测试带文件重定向的 heredoc
        let result = reconstruct_array_heredoc(&[
            "cat".to_string(),
            ">".to_string(),
            "test.py".to_string(),
            "<<".to_string(),
            "'EOF'".to_string(),
            "import unittest".to_string(),
            "def test():".to_string(),
            "    pass".to_string(),
            "EOF".to_string(),
        ]);
        assert!(result.is_some());
        let cmd = result.unwrap();
        assert!(cmd.contains("cat > test.py << 'EOF'"));
        assert!(cmd.contains("import unittest"));

        // 测试 <<EOF 格式（无空格）
        let result = reconstruct_array_heredoc(&[
            "cat".to_string(),
            "<<EOF".to_string(),
            "content".to_string(),
            "EOF".to_string(),
        ]);
        assert!(result.is_some());
        let cmd = result.unwrap();
        assert!(cmd.contains("<< 'EOF'"));
        assert!(cmd.contains("content"));
    }

    #[test]
    fn test_process_command_with_array_heredoc() {
        // 测试 process_command 处理数组格式的 heredoc
        let result = process_command(vec![
            "cat".to_string(),
            "<<".to_string(),
            "EOF".to_string(),
            "hello".to_string(),
            "world".to_string(),
            "EOF".to_string(),
        ]);
        assert!(result.needs_shell);
        assert!(result.shell_command.is_some());
        let cmd = result.shell_command.unwrap();
        // 验证 heredoc 被正确重构
        assert!(cmd.contains("<< 'EOF'"));
        assert!(cmd.contains("\nhello\n"));
        assert!(cmd.contains("\nworld\n"));
    }

    #[test]
    fn test_process_command_with_single_element_heredoc() {
        // 测试 process_command 处理单元素 heredoc 命令
        // 这种情况发生在 command_deserializer 检测到 heredoc 后保持命令完整时
        let heredoc_cmd = "cat > templates/readme.md << 'EOF'\n# {{TITLE}}\nEOF";
        let result = process_command(vec![heredoc_cmd.to_string()]);

        assert!(result.needs_shell, "单元素 heredoc 命令应该需要 shell");
        assert!(result.shell_command.is_some(), "应该有 shell_command");

        let cmd = result.shell_command.unwrap();
        // 验证命令没有被错误地用引号包裹
        assert!(!cmd.starts_with("'"), "命令不应该以单引号开头");
        // 验证 heredoc 内容保持完整
        assert!(cmd.contains("<<"), "命令应该包含 heredoc 操作符");
        assert!(cmd.contains("EOF"), "命令应该包含 EOF 定界符");
        assert!(cmd.contains("{{TITLE}}"), "命令应该包含 heredoc 内容");
    }

    #[test]
    fn test_sanitize_json_arguments() {
        // 测试正常 JSON 不变
        let normal = r#"{"command":["ls","-la"]}"#;
        assert_eq!(sanitize_json_arguments(normal), normal);

        // 测试修复字符串中的换行符
        let with_newline = "{\"command\":[\"echo\",\"hello\nworld\"]}";
        let sanitized = sanitize_json_arguments(with_newline);
        assert!(sanitized.contains("hello\\nworld"));
        assert!(!sanitized.contains("\n"));

        // 测试修复制表符
        let with_tab = "{\"command\":[\"echo\",\"hello\tworld\"]}";
        let sanitized = sanitize_json_arguments(with_tab);
        assert!(sanitized.contains("hello\\tworld"));

        // 测试不修改字符串外的换行
        let json_with_formatting = "{\n  \"command\": [\"ls\"]\n}";
        let sanitized = sanitize_json_arguments(json_with_formatting);
        // 外部换行应该保留
        assert!(sanitized.contains("\n"));
    }

    #[test]
    fn test_parse_json_with_recovery() {
        use codex_protocol::models::ShellToolCallParams;

        // 测试正常解析
        let normal = r#"{"command":["ls","-la"]}"#;
        let result: Result<ShellToolCallParams, _> = parse_json_with_recovery(normal);
        assert!(result.is_ok());

        // 测试包含换行符的 JSON (模拟 AI 模型的错误输出)
        // 注意：这里我们需要构造一个包含实际换行符的字符串
        let with_newline = format!(
            "{{\"command\":[\"echo\",\"hello{}world\"]}}",
            '\n'  // 实际换行符
        );
        let result: Result<ShellToolCallParams, _> = parse_json_with_recovery(&with_newline);
        assert!(result.is_ok());
        let params = result.unwrap();
        assert_eq!(params.command.len(), 2);

        // 测试引号不匹配的情况
        let missing_quote = r#"{"command":["ls","-la"#;
        let result: Result<ShellToolCallParams, _> = parse_json_with_recovery(missing_quote);
        assert!(result.is_ok());

        // 测试单引号问题
        let single_quotes = r#"{'command': ['ls', '-la']}"#;
        let result: Result<ShellToolCallParams, _> = parse_json_with_recovery(single_quotes);
        assert!(result.is_ok());

        // 测试尾部逗号问题
        let trailing_comma = r#"{"command":["ls","-la"],}"#;
        let result: Result<ShellToolCallParams, _> = parse_json_with_recovery(trailing_comma);
        assert!(result.is_ok());

        // 测试括号不匹配
        let unmatched_braces = r#"{"command":["ls","-la""#;
        let result: Result<ShellToolCallParams, _> = parse_json_with_recovery(unmatched_braces);
        assert!(result.is_ok());

        // 测试综合问题
        let complex_issue = r#"{"command": ["echo", "Hello
world"], 'directory': "/tmp",}"#;
        let result: Result<ShellToolCallParams, _> = parse_json_with_recovery(complex_issue);
        assert!(result.is_ok());
    }

    #[test]
    fn test_fix_common_quote_issues() {
        // 测试单引号转双引号
        let input = r#"{'command': ['ls', '-la']}"#;
        let output = fix_common_quote_issues(input);
        assert_eq!(output, r#"{"command": ["ls", "-la"]}"#);

        // 测试字符串内的单引号不应被替换
        let input = r#"{"command": ["echo", "It's OK"]}"#;
        let output = fix_common_quote_issues(input);
        assert_eq!(output, input);

        // 测试转义字符处理
        let input = r#"{"command": ["echo", "Quote: \""]}"#;
        let output = fix_common_quote_issues(input);
        assert_eq!(output, input);
    }

    #[test]
    fn test_fix_bracket_issues() {
        // 测试缺少右括号
        let input = r#"{"command": ["ls", "-la""#;
        let output = fix_bracket_issues(input);
        assert_eq!(output, r#"{"command": ["ls", "-la"]}"#);

        // 测试缺少多个右括号
        let input = r#"{"command": ["ls", "-la"], "options": {"recursive": true"#;
        let output = fix_bracket_issues(input);
        assert_eq!(output, r#"{"command": ["ls", "-la"], "options": {"recursive": true}}"#);
    }

    #[test]
    fn test_fix_trailing_issues() {
        // 测试尾部逗号
        let input = r#"{"command": ["ls", "-la"],}"#;
        let output = fix_trailing_issues(input);
        assert_eq!(output, r#"{"command": ["ls", "-la"] }"#);

        // 测试未关闭的字符串
        let input = r#"{"command": ["echo", "hello world"#;
        let output = fix_trailing_issues(input);
        assert_eq!(output, r#"{"command": ["echo", "hello world"]}"#);
    }

    #[test]
    fn test_fix_stringified_arrays() {
        // 测试字符串化的数组
        let stringified_array = r#"{"command":"[\"ls\", \"-la\"]"}"#;
        let result = fix_stringified_arrays(stringified_array);

        // 验证修复结果
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert!(parsed["command"].is_array());

        let command_array = parsed["command"].as_array().unwrap();
        assert_eq!(command_array[0], "ls");
        assert_eq!(command_array[1], "-la");
    }

    #[test]
    fn test_fix_missing_fields() {
        // 测试缺失字段修复
        let incomplete = r#"{"command": ["ls"]}"#;
        let expected_fields = vec!["input", "directory"];
        let result = fix_missing_fields(incomplete, &expected_fields);

        // 验证修复结果
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert!(parsed.get("input").is_some());
        assert!(parsed.get("directory").is_some());
    }

    #[test]
    fn test_extract_missing_field_name() {
        // 测试字段名提取
        let error_msg1 = "missing field `input` at line 1 column 100";
        assert_eq!(extract_missing_field_name(error_msg1), Some("input".to_string()));

        let error_msg2 = "missing field `directory` at line 2 column 45";
        assert_eq!(extract_missing_field_name(error_msg2), Some("directory".to_string()));

        let error_msg3 = "some other error";
        assert_eq!(extract_missing_field_name(error_msg3), None);
    }

  #[test]
    fn test_fix_mixed_quotes_in_array() {
        // 测试混合引号的数组
        let mixed_quotes = r#"{"command":["sed", '-i.bak', 's/old/new/g', "test.go"]}"#;
        let result = fix_mixed_quotes_in_array(mixed_quotes);

        // 验证修复结果
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert!(parsed["command"].is_array());

        let command_array = parsed["command"].as_array().unwrap();
        assert_eq!(command_array[0], "sed");
        assert_eq!(command_array[1], "-i.bak");
        assert_eq!(command_array[2], "s/old/new/g");
        assert_eq!(command_array[3], "test.go");
    }

    #[test]
    fn test_complex_mixed_quotes() {
        // 测试复杂的混合引号，包含转义字符
        let complex = r#"{"command":["python3", "-c", "print('Hello \"World\"')", "test.py"]}"#;
        let result = fix_mixed_quotes_in_array(complex);

        // 验证修复结果
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        let command_array = parsed["command"].as_array().unwrap();
        assert_eq!(command_array[0], "python3");
        assert_eq!(command_array[1], "-c");
        assert_eq!(command_array[2], "print('Hello \"World\"')");
        assert_eq!(command_array[3], "test.py");
    }

  #[test]
    fn test_expand_operator_arguments() {
        // 测试 > file 合并的情况
        let input = vec![
            "cat".to_string(),
            "> test.py".to_string(),
        ];
        let expanded = expand_operator_arguments(&input);
        assert_eq!(expanded, vec!["cat", ">", "test.py"]);

        // 测试 << 'EOF' 合并的情况
        let input = vec![
            "cat".to_string(),
            "<< 'EOF'".to_string(),
        ];
        let expanded = expand_operator_arguments(&input);
        assert_eq!(expanded, vec!["cat", "<<", "'EOF'"]);

        // 测试多个合并操作符
        let input = vec![
            "cat".to_string(),
            "> test.py".to_string(),
            "<< 'EOF'".to_string(),
        ];
        let expanded = expand_operator_arguments(&input);
        assert_eq!(expanded, vec!["cat", ">", "test.py", "<<", "'EOF'"]);

        // 测试已经正确分割的情况（不应该改变）
        let input = vec![
            "cat".to_string(),
            ">".to_string(),
            "test.py".to_string(),
        ];
        let expanded = expand_operator_arguments(&input);
        assert_eq!(expanded, vec!["cat", ">", "test.py"]);

        // 测试追加操作符
        let input = vec![
            "echo".to_string(),
            "hello".to_string(),
            ">>output.txt".to_string(),
        ];
        let expanded = expand_operator_arguments(&input);
        assert_eq!(expanded, vec!["echo", "hello", ">>", "output.txt"]);
    }

    #[test]
    fn test_join_command_for_shell_with_merged_operators() {
        // 测试合并操作符的情况
        let input = vec![
            "cat".to_string(),
            "> test.py".to_string(),
            "<< 'EOF'".to_string(),
        ];
        let result = join_command_for_shell(&input);
        // 应该正确分割操作符
        assert_eq!(result, "cat > test.py << 'EOF'");

        // 测试带空格的文件名
        let input = vec![
            "cat".to_string(),
            "> my file.txt".to_string(),
        ];
        let result = join_command_for_shell(&input);
        // 文件名需要引号
        assert_eq!(result, "cat > 'my file.txt'");
    }
}
