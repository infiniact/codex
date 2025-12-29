//! Session-wide mutable state.

use codex_protocol::models::ResponseItem;
use codex_protocol::plan_tool::StepStatus;
use codex_protocol::plan_tool::UpdatePlanArgs;

use crate::codex::SessionConfiguration;
use crate::context_manager::ContextManager;
use crate::protocol::RateLimitSnapshot;
use crate::protocol::TokenUsage;
use crate::protocol::TokenUsageInfo;
use crate::truncate::TruncationPolicy;

/// Persistent, session-scoped state previously stored directly on `Session`.
pub(crate) struct SessionState {
    pub(crate) session_configuration: SessionConfiguration,
    pub(crate) history: ContextManager,
    pub(crate) latest_rate_limits: Option<RateLimitSnapshot>,
    /// 当前活跃的任务计划，用于在 compact 后恢复
    pub(crate) current_plan: Option<UpdatePlanArgs>,
}

impl SessionState {
    /// Create a new session state mirroring previous `State::default()` semantics.
    pub(crate) fn new(session_configuration: SessionConfiguration) -> Self {
        let history = ContextManager::new();
        Self {
            session_configuration,
            history,
            latest_rate_limits: None,
            current_plan: None,
        }
    }

    // Plan management helpers
    pub(crate) fn set_current_plan(&mut self, plan: UpdatePlanArgs) {
        self.current_plan = Some(plan);
    }

    pub(crate) fn get_current_plan(&self) -> Option<&UpdatePlanArgs> {
        self.current_plan.as_ref()
    }

    pub(crate) fn clear_current_plan(&mut self) {
        self.current_plan = None;
    }

    // History helpers
    pub(crate) fn record_items<I>(&mut self, items: I, policy: TruncationPolicy)
    where
        I: IntoIterator,
        I::Item: std::ops::Deref<Target = ResponseItem>,
    {
        self.history.record_items(items, policy);
    }

    pub(crate) fn clone_history(&self) -> ContextManager {
        self.history.clone()
    }

    pub(crate) fn replace_history(&mut self, items: Vec<ResponseItem>) {
        self.history.replace(items);
    }

    pub(crate) fn set_token_info(&mut self, info: Option<TokenUsageInfo>) {
        self.history.set_token_info(info);
    }

    // Token/rate limit helpers
    pub(crate) fn update_token_info_from_usage(
        &mut self,
        usage: &TokenUsage,
        model_context_window: Option<i64>,
    ) {
        self.history.update_token_info(usage, model_context_window);
    }

    pub(crate) fn token_info(&self) -> Option<TokenUsageInfo> {
        self.history.token_info()
    }

    pub(crate) fn set_rate_limits(&mut self, snapshot: RateLimitSnapshot) {
        self.latest_rate_limits = Some(merge_rate_limit_fields(
            self.latest_rate_limits.as_ref(),
            snapshot,
        ));
    }

    pub(crate) fn token_info_and_rate_limits(
        &self,
    ) -> (Option<TokenUsageInfo>, Option<RateLimitSnapshot>) {
        (self.token_info(), self.latest_rate_limits.clone())
    }

    pub(crate) fn set_token_usage_full(&mut self, context_window: i64) {
        self.history.set_token_usage_full(context_window);
    }

    pub(crate) fn get_total_token_usage(&self) -> i64 {
        self.history.get_total_token_usage()
    }

    
    /// 获取缓存的 token 数量
    pub(crate) fn get_cached_token_usage(&self) -> i64 {
        self.history.get_cached_token_usage()
    }

    /// 获取最近一次请求的 token 使用量
    pub(crate) fn get_last_token_usage(&self) -> i64 {
        self.history.get_last_token_usage()
    }

    /// 判断当前是否处于逻辑单元边界，适合进行 compact
    ///
    /// 逻辑单元边界的定义：
    /// 1. 如果有活跃的 Plan，检查是否有 in_progress 的步骤
    ///    - 有 in_progress 步骤 = 不在边界（正在执行任务）
    ///    - 没有 in_progress 步骤 = 在边界（步骤间隙）
    /// 2. 如果没有 Plan，默认认为在边界
    ///
    /// 返回值：
    /// - `true`: 当前处于逻辑单元边界，可以安全 compact
    /// - `false`: 当前在逻辑单元内部，不建议 compact
    pub(crate) fn is_at_logical_unit_boundary(&self) -> bool {
        match &self.current_plan {
            Some(plan) => {
                // 检查是否有 in_progress 的步骤
                let has_in_progress = plan
                    .plan
                    .iter()
                    .any(|step| matches!(step.status, StepStatus::InProgress));

                // 如果没有 in_progress 步骤，则处于边界
                !has_in_progress
            }
            None => {
                // 没有 Plan 时，默认认为在边界
                true
            }
        }
    }

    /// 获取当前 Plan 的进度信息，用于日志记录
    pub(crate) fn get_plan_progress(&self) -> Option<PlanProgress> {
        self.current_plan.as_ref().map(|plan| {
            let total = plan.plan.len();
            let completed = plan
                .plan
                .iter()
                .filter(|s| matches!(s.status, StepStatus::Completed))
                .count();
            let in_progress = plan
                .plan
                .iter()
                .filter(|s| matches!(s.status, StepStatus::InProgress))
                .count();
            let pending = plan
                .plan
                .iter()
                .filter(|s| matches!(s.status, StepStatus::Pending))
                .count();

            PlanProgress {
                total,
                completed,
                in_progress,
                pending,
            }
        })
    }
}

/// Plan 进度信息
#[derive(Debug, Clone)]
pub(crate) struct PlanProgress {
    pub total: usize,
    pub completed: usize,
    pub in_progress: usize,
    pub pending: usize,
}

impl PlanProgress {
    /// 检查是否有未完成的步骤
    pub fn has_incomplete_steps(&self) -> bool {
        self.in_progress > 0 || self.pending > 0
    }
}

// Sometimes new snapshots don't include credits or plan information.
fn merge_rate_limit_fields(
    previous: Option<&RateLimitSnapshot>,
    mut snapshot: RateLimitSnapshot,
) -> RateLimitSnapshot {
    if snapshot.credits.is_none() {
        snapshot.credits = previous.and_then(|prior| prior.credits.clone());
    }
    if snapshot.plan_type.is_none() {
        snapshot.plan_type = previous.and_then(|prior| prior.plan_type);
    }
    snapshot
}
