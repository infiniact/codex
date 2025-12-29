//! Sandbox command assessment module (stub implementation).
//!
//! This module provides functionality to assess commands before execution.
//! Currently implemented as a stub that returns None.

#![allow(dead_code)]

use std::path::Path;
use std::sync::Arc;

use crate::AuthManager;
use crate::ModelProviderInfo;
use crate::config::Config;
use crate::models_manager::manager::ModelsManager;
use crate::protocol::SandboxPolicy;
use codex_otel::otel_manager::OtelEventManager;
use codex_protocol::ConversationId;
use codex_protocol::protocol::SandboxCommandAssessment;
use codex_protocol::protocol::SessionSource;

/// Assess a command before execution.
///
/// This is a stub implementation that always returns None.
/// The full implementation requires the askama templating crate.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn assess_command(
    _config: Arc<Config>,
    _provider: ModelProviderInfo,
    _auth_manager: Arc<AuthManager>,
    _parent_otel: &OtelEventManager,
    _conversation_id: ConversationId,
    _models_manager: Arc<ModelsManager>,
    _session_source: SessionSource,
    _call_id: &str,
    _command: &[String],
    _sandbox_policy: &SandboxPolicy,
    _cwd: &Path,
    _failure_message: Option<&str>,
) -> Option<SandboxCommandAssessment> {
    // Stub implementation - returns None indicating no assessment available.
    // The full implementation requires askama templating and is currently disabled.
    tracing::debug!("assess_command stub called - returning None");
    None
}
