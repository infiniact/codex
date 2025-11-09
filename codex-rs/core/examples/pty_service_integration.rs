//! 示例：如何集成外部 PtyService 到 codex-rs
//!
//! 此示例展示了如何实现 PtyServiceBridge trait 并将其集成到 ConversationManager 中，
//! 使 AI 对话在执行命令时使用外部 PTY 服务而不是默认的 portable-pty 后端。

use std::sync::Arc;
use codex_core::ConversationManager;
use codex_core::unified_exec::{PtyServiceBridge, PtyServiceResult};
use codex_core::AuthManager;
use codex_protocol::protocol::SessionSource;

/// 示例 PtyService 桥接器实现
/// 
/// 在实际应用中，这里应该连接到真实的 PtyService 实例
pub struct ExamplePtyServiceBridge {
    service_url: String,
}

impl ExamplePtyServiceBridge {
    pub fn new(service_url: String) -> Self {
        Self { service_url }
    }
}

#[async_trait::async_trait]
impl PtyServiceBridge for ExamplePtyServiceBridge {
    async fn execute(
        &self,
        command: &str,
        shell: &str,
        login: bool,
        display_in_panel: bool,
        connection_id: Option<&str>,
        stdin: Option<&str>,
    ) -> Result<PtyServiceResult, String> {
        // 在实际实现中，这里应该调用真实的 PtyService API
        println!("执行命令通过 PtyService: {command}");
        println!("使用 shell: {shell}");
        println!("登录模式: {login}");
        println!("在面板显示: {display_in_panel}");
        println!("连接 ID: {connection_id:?}");
        println!("Stdin: {stdin:?}");
        
        // 模拟执行结果
        Ok(PtyServiceResult {
            session_id: format!("session_{}", rand::random::<u32>()),
            output: format!("模拟输出: 执行命令 '{command}'"),
            exit_code: Some(0),
            panel_id: if display_in_panel {
                Some(format!("panel_{}", rand::random::<u32>()))
            } else {
                None
            },
            connection_id: connection_id.unwrap_or("default").to_string(),
        })
    }

    async fn write_stdin(&self, session_id: &str, input: &[u8]) -> Result<(), String> {
        // 在实际实现中，这里应该向指定会话写入数据
        println!("向会话 {session_id} 写入数据: {:?}", String::from_utf8_lossy(input));
        Ok(())
    }

    fn is_available(&self) -> bool {
        // 在实际实现中，这里应该检查 PtyService 是否可用
        println!("检查 PtyService 可用性: {}", self.service_url);
        true // 示例中总是返回可用
    }
}

/// 示例：如何创建带有 PtyService 桥接器的 ConversationManager
pub async fn create_conversation_manager_with_pty_service() -> ConversationManager {
    // 1. 创建认证管理器
    let auth_manager = Arc::new(AuthManager::new(
        std::path::PathBuf::from("."), 
        true, 
        codex_core::auth::AuthCredentialsStoreMode::File
    ));
    
    // 2. 创建 PtyService 桥接器
    let pty_bridge = Arc::new(ExamplePtyServiceBridge::new(
        "http://localhost:8080".to_string()
    ));
    
    // 3. 创建带有 PtyService 桥接器的对话管理器
    let conversation_manager = ConversationManager::new_with_pty_bridge(
        auth_manager,
        SessionSource::Cli,
        pty_bridge,
    );
    
    println!("✅ 成功创建带有 PtyService 桥接器的 ConversationManager");
    
    conversation_manager
}

/// 示例：如何为现有的 ConversationManager 设置 PtyService 桥接器
pub async fn set_pty_service_for_existing_manager() {
    // 1. 创建普通的 ConversationManager
    let auth_manager = Arc::new(AuthManager::new(
        std::path::PathBuf::from("."), 
        true, 
        codex_core::auth::AuthCredentialsStoreMode::File
    ));
    let conversation_manager = ConversationManager::new(
        auth_manager,
        SessionSource::Cli,
    );

    // 2. 创建 PtyService 桥接器
    let pty_bridge = Arc::new(ExamplePtyServiceBridge::new(
        "http://localhost:9090".to_string()
    ));

    // 3. 为现有管理器设置 PtyService 桥接器（异步方法）
    conversation_manager.set_pty_bridge(pty_bridge).await;

    // 4. 验证桥接器已设置（异步方法）
    if let Some(bridge) = conversation_manager.get_pty_bridge().await {
        println!("✅ 成功为现有 ConversationManager 设置 PtyService 桥接器");
        println!("桥接器可用性: {}", bridge.is_available());
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("🚀 PtyService 集成示例");
    println!("===================");
    
    // 示例 1: 创建带有 PtyService 的 ConversationManager
    println!("\n📝 示例 1: 创建带有 PtyService 的 ConversationManager");
    let _manager1 = create_conversation_manager_with_pty_service().await;
    
    // 示例 2: 为现有 ConversationManager 设置 PtyService
    println!("\n📝 示例 2: 为现有 ConversationManager 设置 PtyService");
    set_pty_service_for_existing_manager().await;
    
    println!("\n✨ 所有示例执行完成！");
    println!("\n💡 使用说明:");
    println!("   - 实现 PtyServiceBridge trait 来连接你的 PtyService");
    println!("   - 使用 ConversationManager::new_with_pty_bridge() 创建带桥接器的管理器");
    println!("   - 或使用 set_pty_bridge() 为现有管理器设置桥接器");
    println!("   - AI 对话执行命令时将自动使用 PtyService");
    
    Ok(())
}