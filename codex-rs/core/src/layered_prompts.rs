//! Layered Prompt System
//!
//! Provides a hierarchical prompt loading system with A/B testing support.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │                     Layered Prompts                          │
//! ├─────────────────────────────────────────────────────────────┤
//! │  Core Layer (~800 tokens)                                    │
//! │  ├── base.md          - Core capabilities & personality      │
//! │  ├── sandbox.md       - Sandbox & approval modes             │
//! │  └── formatting.md    - Output formatting rules              │
//! ├─────────────────────────────────────────────────────────────┤
//! │  Scenario Layer (on-demand, ~300-800 tokens each)            │
//! │  ├── planning.md      - Task planning guidelines             │
//! │  ├── code_review.md   - Code review rubric                   │
//! │  ├── theme_generation.md - Theme design rules                │
//! │  └── shell.md         - Shell command guidelines             │
//! ├─────────────────────────────────────────────────────────────┤
//! │  Extension Layer (optional, ~100-200 tokens each)            │
//! │  ├── preambles.md     - Preamble message examples            │
//! │  └── progress.md      - Progress update guidelines           │
//! └─────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Token Savings
//!
//! | Configuration | Original | Optimized | Savings |
//! |---------------|----------|-----------|---------|
//! | Full prompt   | ~3500    | ~1800     | 48%     |
//! | Core only     | ~3500    | ~800      | 77%     |
//! | Core + Review | ~4500    | ~1600     | 64%     |

use std::collections::HashMap;
use serde::{Deserialize, Serialize};

/// Prompt layer types
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PromptLayer {
    /// Core prompts - always loaded (~800 tokens)
    Core,
    /// Scenario-specific prompts - loaded on demand
    Scenario,
    /// Optional extensions - loaded when enabled
    Extension,
}

/// Scenario types for conditional loading
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Scenario {
    /// General coding tasks
    General,
    /// Task planning
    Planning,
    /// Code review
    CodeReview,
    /// Theme generation
    ThemeGeneration,
    /// Shell command execution
    Shell,
}

/// Extension types
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Extension {
    /// Preamble messages before tool calls
    Preambles,
    /// Progress update guidelines
    Progress,
}

/// A single prompt component
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptComponent {
    /// Unique identifier
    pub id: String,
    /// Layer this component belongs to
    pub layer: PromptLayer,
    /// The prompt content
    pub content: String,
    /// Estimated token count
    pub estimated_tokens: usize,
    /// Priority for ordering (lower = first)
    pub priority: u32,
}

impl PromptComponent {
    /// Estimate tokens using simple heuristic (4 chars ≈ 1 token for English)
    pub fn estimate_tokens(content: &str) -> usize {
        let chars = content.len();
        let cjk_chars = content.chars().filter(|c| is_cjk(*c)).count();
        let english_chars = chars - cjk_chars * 3; // CJK chars are ~3 bytes

        // English: ~4 chars/token, CJK: ~1.5 chars/token
        (english_chars / 4) + (cjk_chars * 2 / 3)
    }
}

fn is_cjk(c: char) -> bool {
    matches!(c, '\u{4E00}'..='\u{9FFF}' | '\u{3400}'..='\u{4DBF}')
}

/// Configuration for layered prompt loading
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LayeredPromptConfig {
    /// Whether to use compressed prompts
    pub use_compressed: bool,
    /// Active scenarios
    pub scenarios: Vec<Scenario>,
    /// Active extensions
    pub extensions: Vec<Extension>,
    /// A/B test variant (if any)
    pub ab_variant: Option<String>,
    /// Maximum total tokens
    pub max_tokens: Option<usize>,
}

impl Default for LayeredPromptConfig {
    fn default() -> Self {
        Self {
            use_compressed: true,
            scenarios: vec![Scenario::General],
            extensions: vec![Extension::Preambles],
            ab_variant: None,
            max_tokens: None,
        }
    }
}

/// Layered prompt manager
pub struct LayeredPromptManager {
    /// Core prompts (always loaded)
    core_prompts: Vec<PromptComponent>,
    /// Scenario prompts (loaded on demand)
    scenario_prompts: HashMap<Scenario, PromptComponent>,
    /// Extension prompts (optional)
    extension_prompts: HashMap<Extension, PromptComponent>,
    /// Original full prompt for comparison
    original_prompt: String,
}

impl LayeredPromptManager {
    /// Create a new layered prompt manager
    pub fn new() -> Self {
        Self {
            core_prompts: Vec::new(),
            scenario_prompts: HashMap::new(),
            extension_prompts: HashMap::new(),
            original_prompt: String::new(),
        }
    }

    /// Load prompts from embedded strings
    pub fn load_embedded() -> Self {
        let mut manager = Self::new();

        // Load core prompts
        manager.core_prompts = vec![
            PromptComponent {
                id: "core.base".to_string(),
                layer: PromptLayer::Core,
                content: include_str!("../prompts/core/base.md").to_string(),
                estimated_tokens: PromptComponent::estimate_tokens(
                    include_str!("../prompts/core/base.md")
                ),
                priority: 0,
            },
            PromptComponent {
                id: "core.sandbox".to_string(),
                layer: PromptLayer::Core,
                content: include_str!("../prompts/core/sandbox.md").to_string(),
                estimated_tokens: PromptComponent::estimate_tokens(
                    include_str!("../prompts/core/sandbox.md")
                ),
                priority: 1,
            },
            PromptComponent {
                id: "core.formatting".to_string(),
                layer: PromptLayer::Core,
                content: include_str!("../prompts/core/formatting.md").to_string(),
                estimated_tokens: PromptComponent::estimate_tokens(
                    include_str!("../prompts/core/formatting.md")
                ),
                priority: 2,
            },
        ];

        // Load scenario prompts
        manager.scenario_prompts.insert(
            Scenario::Planning,
            PromptComponent {
                id: "scenario.planning".to_string(),
                layer: PromptLayer::Scenario,
                content: include_str!("../prompts/scenarios/planning.md").to_string(),
                estimated_tokens: PromptComponent::estimate_tokens(
                    include_str!("../prompts/scenarios/planning.md")
                ),
                priority: 10,
            },
        );

        manager.scenario_prompts.insert(
            Scenario::CodeReview,
            PromptComponent {
                id: "scenario.code_review".to_string(),
                layer: PromptLayer::Scenario,
                content: include_str!("../prompts/scenarios/code_review.md").to_string(),
                estimated_tokens: PromptComponent::estimate_tokens(
                    include_str!("../prompts/scenarios/code_review.md")
                ),
                priority: 10,
            },
        );

        manager.scenario_prompts.insert(
            Scenario::ThemeGeneration,
            PromptComponent {
                id: "scenario.theme".to_string(),
                layer: PromptLayer::Scenario,
                content: include_str!("../prompts/scenarios/theme_generation.md").to_string(),
                estimated_tokens: PromptComponent::estimate_tokens(
                    include_str!("../prompts/scenarios/theme_generation.md")
                ),
                priority: 10,
            },
        );

        manager.scenario_prompts.insert(
            Scenario::Shell,
            PromptComponent {
                id: "scenario.shell".to_string(),
                layer: PromptLayer::Scenario,
                content: include_str!("../prompts/scenarios/shell.md").to_string(),
                estimated_tokens: PromptComponent::estimate_tokens(
                    include_str!("../prompts/scenarios/shell.md")
                ),
                priority: 10,
            },
        );

        // Load extension prompts
        manager.extension_prompts.insert(
            Extension::Preambles,
            PromptComponent {
                id: "ext.preambles".to_string(),
                layer: PromptLayer::Extension,
                content: include_str!("../prompts/extensions/preambles.md").to_string(),
                estimated_tokens: PromptComponent::estimate_tokens(
                    include_str!("../prompts/extensions/preambles.md")
                ),
                priority: 20,
            },
        );

        manager.extension_prompts.insert(
            Extension::Progress,
            PromptComponent {
                id: "ext.progress".to_string(),
                layer: PromptLayer::Extension,
                content: include_str!("../prompts/extensions/progress.md").to_string(),
                estimated_tokens: PromptComponent::estimate_tokens(
                    include_str!("../prompts/extensions/progress.md")
                ),
                priority: 21,
            },
        );

        // Store original prompt for comparison
        manager.original_prompt = include_str!("../prompt.md").to_string();

        manager
    }

    /// Assemble prompt based on configuration
    pub fn assemble(&self, config: &LayeredPromptConfig) -> AssembledPrompt {
        let mut components: Vec<&PromptComponent> = Vec::new();
        let mut total_tokens = 0;

        // Always include core prompts
        for prompt in &self.core_prompts {
            components.push(prompt);
            total_tokens += prompt.estimated_tokens;
        }

        // Add requested scenarios
        for scenario in &config.scenarios {
            if let Some(prompt) = self.scenario_prompts.get(scenario)
                && config.max_tokens.is_none_or(|max| total_tokens + prompt.estimated_tokens <= max)
            {
                components.push(prompt);
                total_tokens += prompt.estimated_tokens;
            }
        }

        // Add requested extensions
        for extension in &config.extensions {
            if let Some(prompt) = self.extension_prompts.get(extension)
                && config.max_tokens.is_none_or(|max| total_tokens + prompt.estimated_tokens <= max)
            {
                components.push(prompt);
                total_tokens += prompt.estimated_tokens;
            }
        }

        // Sort by priority
        components.sort_by_key(|c| c.priority);

        // Combine content
        let content = components
            .iter()
            .map(|c| c.content.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");

        let original_tokens = PromptComponent::estimate_tokens(&self.original_prompt);
        let savings_percent = if original_tokens > 0 {
            ((original_tokens - total_tokens) as f64 / original_tokens as f64 * 100.0) as u32
        } else {
            0
        };

        AssembledPrompt {
            content,
            total_tokens,
            original_tokens,
            savings_percent,
            components_used: components.iter().map(|c| c.id.clone()).collect(),
        }
    }

    /// Get token statistics
    pub fn get_stats(&self) -> PromptStats {
        let core_tokens: usize = self.core_prompts.iter().map(|p| p.estimated_tokens).sum();
        let scenario_tokens: HashMap<String, usize> = self.scenario_prompts
            .iter()
            .map(|(k, v)| (format!("{k:?}"), v.estimated_tokens))
            .collect();
        let extension_tokens: HashMap<String, usize> = self.extension_prompts
            .iter()
            .map(|(k, v)| (format!("{k:?}"), v.estimated_tokens))
            .collect();

        PromptStats {
            original_tokens: PromptComponent::estimate_tokens(&self.original_prompt),
            core_tokens,
            scenario_tokens,
            extension_tokens,
        }
    }
}

impl Default for LayeredPromptManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Assembled prompt result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssembledPrompt {
    /// The combined prompt content
    pub content: String,
    /// Total estimated tokens
    pub total_tokens: usize,
    /// Original prompt tokens (for comparison)
    pub original_tokens: usize,
    /// Percentage savings
    pub savings_percent: u32,
    /// Components used
    pub components_used: Vec<String>,
}

/// Prompt statistics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptStats {
    /// Original prompt token count
    pub original_tokens: usize,
    /// Core layer token count
    pub core_tokens: usize,
    /// Per-scenario token counts
    pub scenario_tokens: HashMap<String, usize>,
    /// Per-extension token counts
    pub extension_tokens: HashMap<String, usize>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_token_estimation() {
        let english = "Hello world";
        let tokens = PromptComponent::estimate_tokens(english);
        assert!(tokens > 0 && tokens < 10);

        let chinese = "你好世界";
        let tokens_zh = PromptComponent::estimate_tokens(chinese);
        assert!(tokens_zh > 0);
    }

    #[test]
    fn test_assemble_core_only() {
        let manager = LayeredPromptManager::load_embedded();
        let config = LayeredPromptConfig {
            scenarios: vec![],
            extensions: vec![],
            ..Default::default()
        };

        let assembled = manager.assemble(&config);
        assert!(assembled.total_tokens > 0);
        assert!(assembled.savings_percent > 50); // Should save >50%
    }

    #[test]
    fn test_assemble_with_scenarios() {
        let manager = LayeredPromptManager::load_embedded();
        let config = LayeredPromptConfig {
            scenarios: vec![Scenario::CodeReview],
            extensions: vec![],
            ..Default::default()
        };

        let assembled = manager.assemble(&config);
        assert!(assembled.components_used.contains(&"scenario.code_review".to_string()));
    }

    #[test]
    fn test_max_tokens_limit() {
        let manager = LayeredPromptManager::load_embedded();
        let config = LayeredPromptConfig {
            scenarios: vec![Scenario::CodeReview, Scenario::Planning],
            extensions: vec![Extension::Preambles, Extension::Progress],
            max_tokens: Some(1000),
            ..Default::default()
        };

        let assembled = manager.assemble(&config);
        assert!(assembled.total_tokens <= 1000);
    }
}
