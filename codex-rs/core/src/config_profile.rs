use serde::Deserialize;
use std::path::PathBuf;

use crate::protocol::AskForApproval;
use codex_protocol::config_types::ReasoningEffort;
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::config_types::Verbosity;

/// Collection of common configuration options that a user can define as a unit
/// in `config.toml`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct ConfigProfile {
    pub model: Option<String>,
    /// The key in the `model_providers` map identifying the
    /// [`ModelProviderInfo`] to use.
    pub model_provider: Option<String>,
    pub approval_policy: Option<AskForApproval>,
    pub model_reasoning_effort: Option<ReasoningEffort>,
    pub model_reasoning_summary: Option<ReasoningSummary>,
    pub model_verbosity: Option<Verbosity>,
    pub chatgpt_base_url: Option<String>,
    pub experimental_instructions_file: Option<PathBuf>,
    pub include_apply_patch_tool: Option<bool>,
    pub include_view_image_tool: Option<bool>,
    pub experimental_use_unified_exec_tool: Option<bool>,
    pub experimental_use_exec_command_tool: Option<bool>,
    pub experimental_use_rmcp_client: Option<bool>,
    pub experimental_use_freeform_apply_patch: Option<bool>,
    pub tools_web_search: Option<bool>,
    pub tools_view_image: Option<bool>,
    /// Optional feature toggles scoped to this profile.
    #[serde(default)]
    pub features: Option<crate::features::FeaturesToml>,
    /// Temperature parameter for controlling randomness in generation (0.0-2.0).
    #[serde(default)]
    pub model_temperature: Option<f64>,
    /// Top-k sampling parameter.
    #[serde(default)]
    pub model_top_k: Option<u32>,
    /// Top-p (nucleus) sampling parameter (0.0-1.0).
    #[serde(default)]
    pub model_top_p: Option<f64>,
    /// Repetition penalty parameter (typically 1.0-1.2).
    #[serde(default)]
    pub model_repetition_penalty: Option<f64>,
}

impl From<ConfigProfile> for codex_app_server_protocol::Profile {
    fn from(config_profile: ConfigProfile) -> Self {
        Self {
            model: config_profile.model,
            model_provider: config_profile.model_provider,
            approval_policy: config_profile.approval_policy,
            model_reasoning_effort: config_profile.model_reasoning_effort,
            model_reasoning_summary: config_profile.model_reasoning_summary,
            model_verbosity: config_profile.model_verbosity,
            chatgpt_base_url: config_profile.chatgpt_base_url,
            model_temperature: config_profile.model_temperature,
            model_top_k: config_profile.model_top_k,
            model_top_p: config_profile.model_top_p,
            model_repetition_penalty: config_profile.model_repetition_penalty,
        }
    }
}
