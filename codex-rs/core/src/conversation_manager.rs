use crate::AuthManager;
#[cfg(any(test, feature = "test-support"))]
use crate::CodexAuth;
use crate::codex::Codex;
use crate::codex::CodexSpawnOk;
use crate::codex::INITIAL_SUBMIT_ID;
use crate::codex_conversation::CodexConversation;
use crate::config::Config;
use crate::error::CodexErr;
use crate::error::Result as CodexResult;
use crate::openai_models::models_manager::ModelsManager;
use crate::protocol::Event;
use crate::protocol::EventMsg;
use crate::protocol::SessionConfiguredEvent;
use crate::rollout::RolloutRecorder;
use crate::unified_exec::PtyServiceBridge;
use codex_protocol::ConversationId;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ModelPreset;
use codex_protocol::protocol::InitialHistory;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::SessionSource;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Represents a newly created Codex conversation, including the first event
/// (which is [`EventMsg::SessionConfigured`]).
pub struct NewConversation {
    pub conversation_id: ConversationId,
    pub conversation: Arc<CodexConversation>,
    pub session_configured: SessionConfiguredEvent,
}

/// [`ConversationManager`] is responsible for creating conversations and
/// maintaining them in memory.
pub struct ConversationManager {
    conversations: Arc<RwLock<HashMap<ConversationId, Arc<CodexConversation>>>>,
    auth_manager: Arc<AuthManager>,
    models_manager: Arc<ModelsManager>,
    session_source: SessionSource,
    /// 可选的 PtyService 桥接器，用于统一的命令执行（支持运行时修改）
    pty_bridge: Arc<RwLock<Option<Arc<dyn PtyServiceBridge>>>>,
}

impl ConversationManager {
    /// 创建新的对话管理器
    ///
    /// 使用默认配置创建对话管理器，不包含 PtyService 桥接器
    ///
    /// # 参数
    ///
    /// - `auth_manager`: 认证管理器
    /// - `session_source`: 会话来源
    pub fn new(auth_manager: Arc<AuthManager>, session_source: SessionSource) -> Self {
        Self {
            conversations: Arc::new(RwLock::new(HashMap::new())),
            auth_manager: auth_manager.clone(),
            models_manager: Arc::new(ModelsManager::new(auth_manager)),
            session_source,
            pty_bridge: Arc::new(RwLock::new(None)),
        }
    }

    /// 创建带有 PtyService 桥接器的对话管理器
    ///
    /// 创建对话管理器并配置 PtyService 桥接器，用于执行命令时
    /// 使用外部 PTY 服务而不是默认的 portable-pty 后端
    ///
    /// # 参数
    ///
    /// - `auth_manager`: 认证管理器
    /// - `session_source`: 会话来源
    /// - `pty_bridge`: PtyService 桥接器实现
    ///
    /// # 示例
    ///
    /// ```rust,no_run
    /// use std::sync::Arc;
    /// use codex_rs::conversation_manager::ConversationManager;
    /// use codex_rs::AuthManager;
    /// use codex_protocol::protocol::SessionSource;
    ///
    /// let auth_manager = Arc::new(AuthManager::new());
    /// let pty_bridge = Arc::new(MyPtyServiceBridge::new());
    /// let manager = ConversationManager::new_with_pty_bridge(
    ///     auth_manager,
    ///     SessionSource::Cli,
    ///     pty_bridge,
    /// );
    /// ```
    pub fn new_with_pty_bridge(
        auth_manager: Arc<AuthManager>,
        session_source: SessionSource,
        pty_bridge: Arc<dyn PtyServiceBridge>,
    ) -> Self {
        let models_manager = Arc::new(ModelsManager::new(auth_manager.clone()));
        Self {
            conversations: Arc::new(RwLock::new(HashMap::new())),
            auth_manager,
            models_manager,
            session_source,
            pty_bridge: Arc::new(RwLock::new(Some(pty_bridge))),
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    /// Construct with a dummy AuthManager containing the provided CodexAuth.
    /// Used for integration tests: should not be used by ordinary business logic.
    pub fn with_auth(auth: CodexAuth) -> Self {
        Self::new(
            crate::AuthManager::from_auth_for_testing(auth),
            SessionSource::Exec,
        )
    }

    /// 设置 PtyService 桥接器（异步版本）
    ///
    /// 允许在创建 ConversationManager 后设置 PtyServiceBridge。
    /// 设置后，所有新创建的会话都将使用此桥接器进行命令执行。
    ///
    /// # 参数
    ///
    /// - `pty_bridge`: PtyService 桥接器实现
    pub async fn set_pty_bridge(&self, pty_bridge: Arc<dyn PtyServiceBridge>) {
        let mut bridge = self.pty_bridge.write().await;
        *bridge = Some(pty_bridge);
    }

    /// 设置 PtyService 桥接器（同步版本）
    ///
    /// 这是 `set_pty_bridge` 的同步版本，用于在非异步上下文中设置桥接器。
    /// 注意：此方法会阻塞当前线程直到获取写锁。
    ///
    /// # 参数
    ///
    /// - `pty_bridge`: PtyService 桥接器实现
    pub fn set_pty_bridge_blocking(&self, pty_bridge: Arc<dyn PtyServiceBridge>) {
        let mut bridge = self.pty_bridge.blocking_write();
        *bridge = Some(pty_bridge);
    }

    /// 获取当前的 PtyService 桥接器
    ///
    /// 返回当前设置的 PtyService 桥接器
    ///
    /// # 返回值
    ///
    /// - `Some(bridge)`: 如果已设置桥接器
    /// - `None`: 如果未设置桥接器
    pub async fn get_pty_bridge(&self) -> Option<Arc<dyn PtyServiceBridge>> {
        self.pty_bridge.read().await.clone()
    }

    /// 设置会话的连接 ID
    ///
    /// 为特定会话设置关联的连接 ID，用于在命令执行时确定使用哪个 PTY 连接
    ///
    /// # 参数
    ///
    /// - `conversation_id`: 会话 ID
    /// - `connection_id`: 连接 ID
    pub async fn set_conversation_connection(
        &self,
        conversation_id: ConversationId,
        connection_id: String,
    ) {
        crate::unified_exec::set_global_conversation_connection(
            &conversation_id.to_string(),
            connection_id,
        )
        .await;
    }

    /// 获取会话的连接 ID
    ///
    /// 返回与特定会话关联的连接 ID
    ///
    /// # 参数
    ///
    /// - `conversation_id`: 会话 ID
    ///
    /// # 返回值
    ///
    /// - `Some(connection_id)`: 如果该会话有关联的连接
    /// - `None`: 如果该会话没有关联的连接
    pub async fn get_conversation_connection(
        &self,
        conversation_id: &ConversationId,
    ) -> Option<String> {
        crate::unified_exec::get_global_conversation_connection(&conversation_id.to_string()).await
    }
    pub fn session_source(&self) -> SessionSource {
        self.session_source.clone()
    }

    pub async fn new_conversation(&self, config: Config) -> CodexResult<NewConversation> {
        self.spawn_conversation(
            config,
            self.auth_manager.clone(),
            self.models_manager.clone(),
        )
        .await
    }

    async fn spawn_conversation(
        &self,
        config: Config,
        auth_manager: Arc<AuthManager>,
        models_manager: Arc<ModelsManager>,
    ) -> CodexResult<NewConversation> {
        // 获取当前的 pty_bridge
        let pty_bridge = self.pty_bridge.read().await.clone();

        let CodexSpawnOk {
            codex,
            conversation_id,
        } = Codex::spawn_with_pty_bridge(
            config,
            auth_manager,
            models_manager,
            InitialHistory::New,
            self.session_source.clone(),
            pty_bridge,
        )
        .await?;
        self.finalize_spawn(codex, conversation_id).await
    }

    async fn finalize_spawn(
        &self,
        codex: Codex,
        conversation_id: ConversationId,
    ) -> CodexResult<NewConversation> {
        // The first event must be `SessionInitialized`. Validate and forward it
        // to the caller so that they can display it in the conversation
        // history.
        let event = codex.next_event().await?;
        let session_configured = match event {
            Event {
                id,
                msg: EventMsg::SessionConfigured(session_configured),
            } if id == INITIAL_SUBMIT_ID => session_configured,
            _ => {
                return Err(CodexErr::SessionConfiguredNotFirstEvent);
            }
        };

        let conversation = Arc::new(CodexConversation::new(
            codex,
            session_configured.rollout_path.clone(),
        ));
        self.conversations
            .write()
            .await
            .insert(conversation_id, conversation.clone());

        Ok(NewConversation {
            conversation_id,
            conversation,
            session_configured,
        })
    }

    pub async fn get_conversation(
        &self,
        conversation_id: ConversationId,
    ) -> CodexResult<Arc<CodexConversation>> {
        let conversations = self.conversations.read().await;
        conversations
            .get(&conversation_id)
            .cloned()
            .ok_or_else(|| CodexErr::ConversationNotFound(conversation_id))
    }

    pub async fn resume_conversation_from_rollout(
        &self,
        config: Config,
        rollout_path: PathBuf,
        auth_manager: Arc<AuthManager>,
    ) -> CodexResult<NewConversation> {
        let initial_history = RolloutRecorder::get_rollout_history(&rollout_path).await?;
        // 获取当前的 pty_bridge
        let pty_bridge = self.pty_bridge.read().await.clone();

        let CodexSpawnOk {
            codex,
            conversation_id,
        } = Codex::spawn_with_pty_bridge(
            config,
            auth_manager,
            self.models_manager.clone(),
            initial_history,
            self.session_source.clone(),
            pty_bridge,
        )
        .await?;
        self.finalize_spawn(codex, conversation_id).await
    }

    pub async fn resume_conversation_with_history(
        &self,
        config: Config,
        initial_history: InitialHistory,
        auth_manager: Arc<AuthManager>,
    ) -> CodexResult<NewConversation> {
        let CodexSpawnOk {
            codex,
            conversation_id,
        } = Codex::spawn(
            config,
            auth_manager,
            self.models_manager.clone(),
            initial_history,
            self.session_source.clone(),
        )
        .await?;
        self.finalize_spawn(codex, conversation_id).await
    }

    /// Removes the conversation from the manager's internal map, though the
    /// conversation is stored as `Arc<CodexConversation>`, it is possible that
    /// other references to it exist elsewhere. Returns the conversation if the
    /// conversation was found and removed.
    pub async fn remove_conversation(
        &self,
        conversation_id: &ConversationId,
    ) -> Option<Arc<CodexConversation>> {
        self.conversations.write().await.remove(conversation_id)
    }

    /// Fork an existing conversation by taking messages up to the given position
    /// (not including the message at the given position) and starting a new
    /// conversation with identical configuration (unless overridden by the
    /// caller's `config`). The new conversation will have a fresh id.
    pub async fn fork_conversation(
        &self,
        nth_user_message: usize,
        config: Config,
        path: PathBuf,
    ) -> CodexResult<NewConversation> {
        // Compute the prefix up to the cut point.
        let history = RolloutRecorder::get_rollout_history(&path).await?;
        let history = truncate_before_nth_user_message(history, nth_user_message);

        // Spawn a new conversation with the computed initial history.
        let auth_manager = self.auth_manager.clone();
        // 获取当前的 pty_bridge
        let pty_bridge = self.pty_bridge.read().await.clone();

        let CodexSpawnOk {
            codex,
            conversation_id,
        } = Codex::spawn_with_pty_bridge(
            config,
            auth_manager,
            self.models_manager.clone(),
            history,
            self.session_source.clone(),
            pty_bridge,
        )
        .await?;

        self.finalize_spawn(codex, conversation_id).await
    }

    pub async fn list_models(&self) -> Vec<ModelPreset> {
        self.models_manager.list_models().await
    }

    pub fn get_models_manager(&self) -> Arc<ModelsManager> {
        self.models_manager.clone()
    }
}

/// Return a prefix of `items` obtained by cutting strictly before the nth user message
/// (0-based) and all items that follow it.
fn truncate_before_nth_user_message(history: InitialHistory, n: usize) -> InitialHistory {
    // Work directly on rollout items, and cut the vector at the nth user message input.
    let items: Vec<RolloutItem> = history.get_rollout_items();

    // Find indices of user message inputs in rollout order.
    let mut user_positions: Vec<usize> = Vec::new();
    for (idx, item) in items.iter().enumerate() {
        if let RolloutItem::ResponseItem(item @ ResponseItem::Message { .. }) = item
            && matches!(
                crate::event_mapping::parse_turn_item(item),
                Some(TurnItem::UserMessage(_))
            )
        {
            user_positions.push(idx);
        }
    }

    // If fewer than or equal to n user messages exist, treat as empty (out of range).
    if user_positions.len() <= n {
        return InitialHistory::New;
    }

    // Cut strictly before the nth user message (do not keep the nth itself).
    let cut_idx = user_positions[n];
    let rolled: Vec<RolloutItem> = items.into_iter().take(cut_idx).collect();

    if rolled.is_empty() {
        InitialHistory::New
    } else {
        InitialHistory::Forked(rolled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex::make_session_and_context;
    use assert_matches::assert_matches;
    use codex_protocol::models::ContentItem;
    use codex_protocol::models::ReasoningItemReasoningSummary;
    use codex_protocol::models::ResponseItem;
    use pretty_assertions::assert_eq;

    fn user_msg(text: &str) -> ResponseItem {
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::OutputText {
                text: text.to_string(),
            }],
        }
    }
    fn assistant_msg(text: &str) -> ResponseItem {
        ResponseItem::Message {
            id: None,
            role: "assistant".to_string(),
            content: vec![ContentItem::OutputText {
                text: text.to_string(),
            }],
        }
    }

    #[test]
    fn drops_from_last_user_only() {
        let items = [
            user_msg("u1"),
            assistant_msg("a1"),
            assistant_msg("a2"),
            user_msg("u2"),
            assistant_msg("a3"),
            ResponseItem::Reasoning {
                id: "r1".to_string(),
                summary: vec![ReasoningItemReasoningSummary::SummaryText {
                    text: "s".to_string(),
                }],
                content: None,
                encrypted_content: None,
            },
            ResponseItem::FunctionCall {
                id: None,
                name: "tool".to_string(),
                arguments: "{}".to_string(),
                call_id: "c1".to_string(),
                thought_signature: None,
            },
            assistant_msg("a4"),
        ];

        // Wrap as InitialHistory::Forked with response items only.
        let initial: Vec<RolloutItem> = items
            .iter()
            .cloned()
            .map(RolloutItem::ResponseItem)
            .collect();
        let truncated = truncate_before_nth_user_message(InitialHistory::Forked(initial), 1);
        let got_items = truncated.get_rollout_items();
        let expected_items = vec![
            RolloutItem::ResponseItem(items[0].clone()),
            RolloutItem::ResponseItem(items[1].clone()),
            RolloutItem::ResponseItem(items[2].clone()),
        ];
        assert_eq!(
            serde_json::to_value(&got_items).unwrap(),
            serde_json::to_value(&expected_items).unwrap()
        );

        let initial2: Vec<RolloutItem> = items
            .iter()
            .cloned()
            .map(RolloutItem::ResponseItem)
            .collect();
        let truncated2 = truncate_before_nth_user_message(InitialHistory::Forked(initial2), 2);
        assert_matches!(truncated2, InitialHistory::New);
    }

    #[test]
    fn ignores_session_prefix_messages_when_truncating() {
        let (session, turn_context) = make_session_and_context();
        let mut items = session.build_initial_context(&turn_context);
        items.push(user_msg("feature request"));
        items.push(assistant_msg("ack"));
        items.push(user_msg("second question"));
        items.push(assistant_msg("answer"));

        let rollout_items: Vec<RolloutItem> = items
            .iter()
            .cloned()
            .map(RolloutItem::ResponseItem)
            .collect();

        let truncated = truncate_before_nth_user_message(InitialHistory::Forked(rollout_items), 1);
        let got_items = truncated.get_rollout_items();

        let expected: Vec<RolloutItem> = vec![
            RolloutItem::ResponseItem(items[0].clone()),
            RolloutItem::ResponseItem(items[1].clone()),
            RolloutItem::ResponseItem(items[2].clone()),
        ];

        assert_eq!(
            serde_json::to_value(&got_items).unwrap(),
            serde_json::to_value(&expected).unwrap()
        );
    }
}
