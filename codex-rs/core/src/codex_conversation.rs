use crate::codex::Codex;
use crate::error::Result as CodexResult;
use crate::protocol::Event;
use crate::protocol::Op;
use crate::protocol::Submission;
use std::path::PathBuf;

pub struct CodexConversation {
    codex: Codex,
    rollout_path: PathBuf,
}

/// Conduit for the bidirectional stream of messages that compose a conversation
/// in Codex.
impl CodexConversation {
    pub(crate) fn new(codex: Codex, rollout_path: PathBuf) -> Self {
        Self {
            codex,
            rollout_path,
        }
    }

    pub async fn submit(&self, op: Op) -> CodexResult<String> {
        self.codex.submit(op).await
    }

    /// Use sparingly: this is intended to be removed soon.
    pub async fn submit_with_id(&self, sub: Submission) -> CodexResult<()> {
        self.codex.submit_with_id(sub).await
    }

    pub async fn next_event(&self) -> CodexResult<Event> {
        self.codex.next_event().await
    }

    pub fn rollout_path(&self) -> PathBuf {
        self.rollout_path.clone()
    }

    /// 检查会话是否仍然活跃（agent loop 是否仍在运行）
    ///
    /// 返回 `true` 表示会话仍然活跃，可以继续发送消息
    /// 返回 `false` 表示会话已关闭，需要重新创建会话
    ///
    /// # 使用场景
    ///
    /// 在调用 `submit()` 之前检查会话状态：
    /// ```rust,no_run
    /// if !conversation.is_alive() {
    ///     // 会话已关闭，需要重新创建会话或从 rollout 恢复
    /// }
    /// ```
    pub fn is_alive(&self) -> bool {
        self.codex.is_alive()
    }

    /// 检查是否有待处理的事件
    pub fn has_pending_events(&self) -> bool {
        self.codex.has_pending_events()
    }
}
