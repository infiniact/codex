//! Turn signing utilities for server-side verification
//!
//! This module provides HMAC-SHA256 signing for the `is_user_turn` header
//! to prevent client-side tampering with turn statistics.

use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::time::{SystemTime, UNIX_EPOCH};

type HmacSha256 = Hmac<Sha256>;

/// 签名密钥（与服务端共享）
/// 生产环境应从环境变量或配置文件读取
const TURN_SIGNING_SECRET: &str = "iaterm_turn_signing_secret_v1";

/// 签名有效期（秒）
/// 服务端应拒绝超过此时间的签名
pub const SIGNATURE_VALIDITY_SECONDS: u64 = 300; // 5 分钟

/// Turn 签名结果
#[derive(Debug, Clone)]
pub struct TurnSignature {
    /// 是否为用户主动发送
    pub is_user_turn: bool,
    /// Unix 时间戳（秒）
    pub timestamp: u64,
    /// HMAC-SHA256 签名（hex 编码）
    pub signature: String,
}

impl TurnSignature {
    /// 生成签名
    ///
    /// # Arguments
    /// * `conversation_id` - 会话 ID
    /// * `is_user_turn` - 是否为用户主动发送
    ///
    /// # Returns
    /// 包含签名信息的 TurnSignature
    pub fn sign(conversation_id: &str, is_user_turn: bool) -> Self {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let signature = compute_signature(conversation_id, is_user_turn, timestamp);

        Self {
            is_user_turn,
            timestamp,
            signature,
        }
    }

    /// 获取 is_user_turn 的字符串表示
    pub fn turn_value(&self) -> &'static str {
        if self.is_user_turn {
            "user"
        } else {
            "system"
        }
    }
}

/// 计算 HMAC-SHA256 签名
///
/// 签名内容格式: `{conversation_id}:{is_user_turn}:{timestamp}`
fn compute_signature(conversation_id: &str, is_user_turn: bool, timestamp: u64) -> String {
    let turn_str = if is_user_turn { "user" } else { "system" };
    let message = format!("{conversation_id}:{turn_str}:{timestamp}");

    let mut mac = HmacSha256::new_from_slice(TURN_SIGNING_SECRET.as_bytes())
        .unwrap_or_else(|_| panic!("HMAC should accept any key size"));
    mac.update(message.as_bytes());

    let result = mac.finalize();
    hex::encode(result.into_bytes())
}

/// 服务端验证签名（参考实现）
///
/// # Arguments
/// * `conversation_id` - 会话 ID
/// * `is_user_turn` - 声称的 is_user_turn 值
/// * `timestamp` - 签名时间戳
/// * `signature` - 客户端提供的签名
///
/// # Returns
/// * `Ok(())` - 签名有效
/// * `Err(String)` - 签名无效或已过期
#[allow(dead_code)]
pub fn verify_signature(
    conversation_id: &str,
    is_user_turn: bool,
    timestamp: u64,
    signature: &str,
) -> Result<(), String> {
    // 检查时间戳是否过期
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    if now > timestamp + SIGNATURE_VALIDITY_SECONDS {
        return Err("Signature expired".to_string());
    }

    // 检查时间戳是否在未来（防止时钟漂移攻击）
    if timestamp > now + 60 {
        return Err("Timestamp in future".to_string());
    }

    // 计算预期签名并比较
    let expected = compute_signature(conversation_id, is_user_turn, timestamp);

    // 使用常量时间比较防止时序攻击
    if constant_time_compare(&expected, signature) {
        Ok(())
    } else {
        Err("Invalid signature".to_string())
    }
}

/// 常量时间字符串比较（防止时序攻击）
fn constant_time_compare(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }

    let mut result = 0u8;
    for (x, y) in a.bytes().zip(b.bytes()) {
        result |= x ^ y;
    }
    result == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sign_and_verify() {
        let conversation_id = "test-conversation-123";

        // 测试用户主动发送
        let sig = TurnSignature::sign(conversation_id, true);
        assert_eq!(sig.turn_value(), "user");
        assert!(verify_signature(conversation_id, true, sig.timestamp, &sig.signature).is_ok());

        // 测试系统自动发送
        let sig = TurnSignature::sign(conversation_id, false);
        assert_eq!(sig.turn_value(), "system");
        assert!(verify_signature(conversation_id, false, sig.timestamp, &sig.signature).is_ok());
    }

    #[test]
    fn test_invalid_signature() {
        let conversation_id = "test-conversation-123";
        let sig = TurnSignature::sign(conversation_id, true);

        // 篡改 is_user_turn 值
        assert!(verify_signature(conversation_id, false, sig.timestamp, &sig.signature).is_err());

        // 篡改 conversation_id
        assert!(verify_signature("other-id", true, sig.timestamp, &sig.signature).is_err());

        // 篡改签名
        assert!(verify_signature(conversation_id, true, sig.timestamp, "invalid").is_err());
    }

    #[test]
    fn test_expired_signature() {
        let conversation_id = "test-conversation-123";

        // 模拟过期的时间戳
        let expired_timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            - SIGNATURE_VALIDITY_SECONDS
            - 100;

        let signature = compute_signature(conversation_id, true, expired_timestamp);

        assert!(verify_signature(conversation_id, true, expired_timestamp, &signature).is_err());
    }
}
