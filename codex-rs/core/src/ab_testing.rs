//! A/B Testing Framework for Prompts
//!
//! Provides experiment management and variant assignment for testing
//! different prompt configurations.
//!
//! # Example
//!
//! ```rust
//! use ab_testing::{ABTestManager, Experiment, Variant};
//!
//! let mut manager = ABTestManager::new();
//!
//! // Create experiment
//! let experiment = Experiment::new("prompt_compression")
//!     .with_variant(Variant::new("control", 50).with_config("use_compressed", false))
//!     .with_variant(Variant::new("compressed", 50).with_config("use_compressed", true));
//!
//! manager.add_experiment(experiment);
//!
//! // Assign user to variant
//! let variant = manager.assign_variant("prompt_compression", "user_123");
//!
//! // Track metrics
//! manager.record_metric("prompt_compression", "user_123", "response_quality", 0.85);
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use serde::{Deserialize, Serialize};

/// Type alias for metrics storage to reduce type complexity
type MetricsMap = HashMap<(String, String, String), Vec<MetricPoint>>;
/// Type alias for metric references
type MetricRef<'a> = (&'a (String, String, String), &'a Vec<MetricPoint>);
use chrono::{DateTime, Utc};

/// Experiment status
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExperimentStatus {
    /// Experiment is in draft mode
    Draft,
    /// Experiment is actively running
    Running,
    /// Experiment is paused
    Paused,
    /// Experiment has concluded
    Completed,
}

/// A single variant in an experiment
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Variant {
    /// Unique variant identifier
    pub id: String,
    /// Human-readable name
    pub name: String,
    /// Traffic weight (0-100)
    pub weight: u32,
    /// Configuration overrides for this variant
    pub config: HashMap<String, serde_json::Value>,
    /// Description of what this variant tests
    pub description: Option<String>,
}

impl Variant {
    /// Create a new variant
    pub fn new(id: impl Into<String>, weight: u32) -> Self {
        let id = id.into();
        Self {
            id: id.clone(),
            name: id,
            weight,
            config: HashMap::new(),
            description: None,
        }
    }

    /// Set a configuration value
    pub fn with_config(mut self, key: impl Into<String>, value: impl Serialize) -> Self {
        self.config.insert(
            key.into(),
            serde_json::to_value(value).unwrap_or(serde_json::Value::Null),
        );
        self
    }

    /// Set description
    pub fn with_description(mut self, desc: impl Into<String>) -> Self {
        self.description = Some(desc.into());
        self
    }
}

/// An A/B test experiment
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Experiment {
    /// Unique experiment identifier
    pub id: String,
    /// Human-readable name
    pub name: String,
    /// Experiment description
    pub description: Option<String>,
    /// Current status
    pub status: ExperimentStatus,
    /// Variants in this experiment
    pub variants: Vec<Variant>,
    /// Metrics being tracked
    pub metrics: Vec<String>,
    /// Start time
    pub started_at: Option<DateTime<Utc>>,
    /// End time
    pub ended_at: Option<DateTime<Utc>>,
    /// Minimum sample size per variant
    pub min_sample_size: Option<u32>,
}

impl Experiment {
    /// Create a new experiment
    pub fn new(id: impl Into<String>) -> Self {
        let id = id.into();
        Self {
            id: id.clone(),
            name: id,
            description: None,
            status: ExperimentStatus::Draft,
            variants: Vec::new(),
            metrics: Vec::new(),
            started_at: None,
            ended_at: None,
            min_sample_size: None,
        }
    }

    /// Add a variant
    pub fn with_variant(mut self, variant: Variant) -> Self {
        self.variants.push(variant);
        self
    }

    /// Add a metric to track
    pub fn with_metric(mut self, metric: impl Into<String>) -> Self {
        self.metrics.push(metric.into());
        self
    }

    /// Set description
    pub fn with_description(mut self, desc: impl Into<String>) -> Self {
        self.description = Some(desc.into());
        self
    }

    /// Set minimum sample size
    pub fn with_min_sample_size(mut self, size: u32) -> Self {
        self.min_sample_size = Some(size);
        self
    }

    /// Start the experiment
    pub fn start(&mut self) {
        self.status = ExperimentStatus::Running;
        self.started_at = Some(Utc::now());
    }

    /// Pause the experiment
    pub fn pause(&mut self) {
        self.status = ExperimentStatus::Paused;
    }

    /// Complete the experiment
    pub fn complete(&mut self) {
        self.status = ExperimentStatus::Completed;
        self.ended_at = Some(Utc::now());
    }

    /// Get total weight of all variants
    fn total_weight(&self) -> u32 {
        self.variants.iter().map(|v| v.weight).sum()
    }
}

/// Metric data point
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricPoint {
    /// Metric name
    pub metric: String,
    /// Metric value
    pub value: f64,
    /// Timestamp
    pub timestamp: DateTime<Utc>,
    /// Additional metadata
    pub metadata: Option<HashMap<String, serde_json::Value>>,
}

/// User assignment record
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Assignment {
    /// User/session identifier
    pub user_id: String,
    /// Assigned variant ID
    pub variant_id: String,
    /// Assignment timestamp
    pub assigned_at: DateTime<Utc>,
}

/// Variant statistics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VariantStats {
    /// Variant ID
    pub variant_id: String,
    /// Number of assignments
    pub sample_size: usize,
    /// Metrics statistics
    pub metrics: HashMap<String, MetricStats>,
}

/// Statistics for a single metric
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricStats {
    /// Number of data points
    pub count: usize,
    /// Sum of values
    pub sum: f64,
    /// Mean value
    pub mean: f64,
    /// Minimum value
    pub min: f64,
    /// Maximum value
    pub max: f64,
    /// Standard deviation
    pub std_dev: f64,
}

impl MetricStats {
    /// Create empty stats
    pub fn new() -> Self {
        Self {
            count: 0,
            sum: 0.0,
            mean: 0.0,
            min: f64::MAX,
            max: f64::MIN,
            std_dev: 0.0,
        }
    }

    /// Add a value
    pub fn add(&mut self, value: f64) {
        self.count += 1;
        self.sum += value;
        self.mean = self.sum / self.count as f64;
        self.min = self.min.min(value);
        self.max = self.max.max(value);
        // Note: Proper std_dev would require storing all values or using Welford's algorithm
    }
}

impl Default for MetricStats {
    fn default() -> Self {
        Self::new()
    }
}

/// Experiment results
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExperimentResults {
    /// Experiment ID
    pub experiment_id: String,
    /// Per-variant statistics
    pub variants: Vec<VariantStats>,
    /// Recommended variant (if conclusive)
    pub recommended_variant: Option<String>,
    /// Confidence level (0-1)
    pub confidence: f64,
    /// Whether results are statistically significant
    pub is_significant: bool,
}

/// A/B Test Manager
pub struct ABTestManager {
    /// Active experiments
    experiments: Arc<RwLock<HashMap<String, Experiment>>>,
    /// User assignments
    assignments: Arc<RwLock<HashMap<(String, String), Assignment>>>,
    /// Recorded metrics
    metrics: Arc<RwLock<MetricsMap>>,
}

impl ABTestManager {
    /// Create a new A/B test manager
    pub fn new() -> Self {
        Self {
            experiments: Arc::new(RwLock::new(HashMap::new())),
            assignments: Arc::new(RwLock::new(HashMap::new())),
            metrics: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Add an experiment
    pub async fn add_experiment(&self, experiment: Experiment) {
        let mut experiments = self.experiments.write().await;
        experiments.insert(experiment.id.clone(), experiment);
    }

    /// Get an experiment
    pub async fn get_experiment(&self, experiment_id: &str) -> Option<Experiment> {
        let experiments = self.experiments.read().await;
        experiments.get(experiment_id).cloned()
    }

    /// Start an experiment
    pub async fn start_experiment(&self, experiment_id: &str) -> Result<(), String> {
        let mut experiments = self.experiments.write().await;
        if let Some(exp) = experiments.get_mut(experiment_id) {
            if exp.variants.is_empty() {
                return Err("Experiment must have at least one variant".to_string());
            }
            exp.start();
            Ok(())
        } else {
            Err(format!("Experiment '{experiment_id}' not found"))
        }
    }

    /// Assign a user to a variant
    pub async fn assign_variant(&self, experiment_id: &str, user_id: &str) -> Option<Variant> {
        // Check for existing assignment
        {
            let assignments = self.assignments.read().await;
            if let Some(assignment) = assignments.get(&(experiment_id.to_string(), user_id.to_string())) {
                let experiments = self.experiments.read().await;
                if let Some(exp) = experiments.get(experiment_id) {
                    return exp.variants.iter().find(|v| v.id == assignment.variant_id).cloned();
                }
            }
        }

        // Find variant to assign
        let variant_to_assign: Option<Variant> = {
            let experiments = self.experiments.read().await;
            let experiment = experiments.get(experiment_id)?;

            if experiment.status != ExperimentStatus::Running {
                return None;
            }

            // Deterministic assignment based on user_id hash
            let hash = Self::hash_user_id(user_id);
            let total_weight = experiment.total_weight();
            if total_weight == 0 {
                return None;
            }

            let bucket = hash % total_weight;
            let mut cumulative = 0u32;

            let mut found_variant = None;
            for variant in &experiment.variants {
                cumulative += variant.weight;
                if bucket < cumulative {
                    found_variant = Some(variant.clone());
                    break;
                }
            }
            found_variant
        };

        // Record assignment outside the read lock
        if let Some(ref variant) = variant_to_assign {
            let mut assignments = self.assignments.write().await;
            assignments.insert(
                (experiment_id.to_string(), user_id.to_string()),
                Assignment {
                    user_id: user_id.to_string(),
                    variant_id: variant.id.clone(),
                    assigned_at: Utc::now(),
                },
            );
        }

        variant_to_assign
    }

    /// Record a metric value
    pub async fn record_metric(
        &self,
        experiment_id: &str,
        user_id: &str,
        metric: &str,
        value: f64,
    ) {
        let mut metrics = self.metrics.write().await;
        let key = (experiment_id.to_string(), user_id.to_string(), metric.to_string());

        let points = metrics.entry(key).or_insert_with(Vec::new);
        points.push(MetricPoint {
            metric: metric.to_string(),
            value,
            timestamp: Utc::now(),
            metadata: None,
        });
    }

    /// Record a metric with metadata
    pub async fn record_metric_with_metadata(
        &self,
        experiment_id: &str,
        user_id: &str,
        metric: &str,
        value: f64,
        metadata: HashMap<String, serde_json::Value>,
    ) {
        let mut metrics = self.metrics.write().await;
        let key = (experiment_id.to_string(), user_id.to_string(), metric.to_string());

        let points = metrics.entry(key).or_insert_with(Vec::new);
        points.push(MetricPoint {
            metric: metric.to_string(),
            value,
            timestamp: Utc::now(),
            metadata: Some(metadata),
        });
    }

    /// Get experiment results
    pub async fn get_results(&self, experiment_id: &str) -> Option<ExperimentResults> {
        let experiments = self.experiments.read().await;
        let experiment = experiments.get(experiment_id)?;

        let assignments = self.assignments.read().await;
        let metrics = self.metrics.read().await;

        let mut variant_stats: Vec<VariantStats> = Vec::new();

        for variant in &experiment.variants {
            // Count assignments for this variant
            let sample_size = assignments
                .iter()
                .filter(|((exp, _), a)| exp == experiment_id && a.variant_id == variant.id)
                .count();

            // Calculate metric stats
            let mut metric_stats: HashMap<String, MetricStats> = HashMap::new();

            for metric_name in &experiment.metrics {
                let mut stats = MetricStats::new();

                for ((exp, user, metric), points) in metrics.iter() {
                    if exp != experiment_id || metric != metric_name {
                        continue;
                    }

                    // Check if this user is in this variant
                    if let Some(assignment) = assignments.get(&(exp.clone(), user.clone()))
                        && assignment.variant_id == variant.id
                    {
                        for point in points {
                            stats.add(point.value);
                        }
                    }
                }

                if stats.count > 0 {
                    metric_stats.insert(metric_name.clone(), stats);
                }
            }

            variant_stats.push(VariantStats {
                variant_id: variant.id.clone(),
                sample_size,
                metrics: metric_stats,
            });
        }

        // Determine recommended variant (simple: highest mean for first metric)
        let (recommended_variant, confidence, is_significant) =
            Self::analyze_results(&experiment.metrics, &variant_stats);

        Some(ExperimentResults {
            experiment_id: experiment_id.to_string(),
            variants: variant_stats,
            recommended_variant,
            confidence,
            is_significant,
        })
    }

    /// Simple hash function for user ID
    fn hash_user_id(user_id: &str) -> u32 {
        use std::hash::{Hash, Hasher};
        use std::collections::hash_map::DefaultHasher;

        let mut hasher = DefaultHasher::new();
        user_id.hash(&mut hasher);
        (hasher.finish() % u32::MAX as u64) as u32
    }

    /// Analyze results to determine winner
    fn analyze_results(
        metrics: &[String],
        variant_stats: &[VariantStats],
    ) -> (Option<String>, f64, bool) {
        if metrics.is_empty() || variant_stats.is_empty() {
            return (None, 0.0, false);
        }

        let primary_metric = &metrics[0];
        let mut best_variant: Option<&VariantStats> = None;
        let mut best_mean = f64::MIN;

        for stats in variant_stats {
            if let Some(metric) = stats.metrics.get(primary_metric)
                && metric.mean > best_mean
            {
                best_mean = metric.mean;
                best_variant = Some(stats);
            }
        }

        // Simple significance check: need at least 100 samples per variant
        let all_have_samples = variant_stats.iter().all(|s| s.sample_size >= 100);

        if let Some(winner) = best_variant {
            let confidence = if all_have_samples { 0.95 } else { 0.5 };
            (Some(winner.variant_id.clone()), confidence, all_have_samples)
        } else {
            (None, 0.0, false)
        }
    }

    /// Export experiment data
    pub async fn export_experiment(&self, experiment_id: &str) -> Option<serde_json::Value> {
        let experiments = self.experiments.read().await;
        let experiment = experiments.get(experiment_id)?;

        let assignments = self.assignments.read().await;
        let metrics = self.metrics.read().await;

        let experiment_assignments: Vec<&Assignment> = assignments
            .iter()
            .filter(|((exp, _), _)| exp == experiment_id)
            .map(|(_, a)| a)
            .collect();

        let experiment_metrics: Vec<MetricRef<'_>> = metrics
            .iter()
            .filter(|((exp, _, _), _)| exp == experiment_id)
            .collect();

        Some(serde_json::json!({
            "experiment": experiment,
            "assignments": experiment_assignments,
            "metrics": experiment_metrics,
        }))
    }
}

impl Default for ABTestManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Create pre-configured experiments for prompt optimization
pub fn create_prompt_experiments() -> Vec<Experiment> {
    vec![
        // Experiment 1: Compressed vs Original prompts
        Experiment::new("prompt_compression")
            .with_description("Test compressed vs original system prompts")
            .with_variant(
                Variant::new("control", 50)
                    .with_description("Original full prompts")
                    .with_config("use_compressed", false)
            )
            .with_variant(
                Variant::new("compressed", 50)
                    .with_description("Compressed layered prompts")
                    .with_config("use_compressed", true)
            )
            .with_metric("response_quality")
            .with_metric("task_completion_rate")
            .with_metric("tokens_used")
            .with_metric("response_time_ms")
            .with_min_sample_size(100),

        // Experiment 2: Different compression levels
        Experiment::new("compression_levels")
            .with_description("Test different prompt compression levels")
            .with_variant(
                Variant::new("core_only", 33)
                    .with_description("Core prompts only (~800 tokens)")
                    .with_config("compression_level", "core_only")
            )
            .with_variant(
                Variant::new("core_plus_scenario", 34)
                    .with_description("Core + scenario prompts (~1200 tokens)")
                    .with_config("compression_level", "core_scenario")
            )
            .with_variant(
                Variant::new("full_layered", 33)
                    .with_description("All layers enabled (~1800 tokens)")
                    .with_config("compression_level", "full")
            )
            .with_metric("response_quality")
            .with_metric("task_completion_rate")
            .with_metric("tokens_used")
            .with_min_sample_size(50),

        // Experiment 3: Scenario detection
        Experiment::new("scenario_detection")
            .with_description("Test auto scenario detection vs fixed scenarios")
            .with_variant(
                Variant::new("fixed", 50)
                    .with_description("Always load general scenario")
                    .with_config("scenario_detection", "fixed")
            )
            .with_variant(
                Variant::new("auto", 50)
                    .with_description("Auto-detect scenario from input")
                    .with_config("scenario_detection", "auto")
            )
            .with_metric("scenario_relevance")
            .with_metric("task_completion_rate")
            .with_min_sample_size(100),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_experiment_creation() {
        let experiment = Experiment::new("test")
            .with_variant(Variant::new("a", 50))
            .with_variant(Variant::new("b", 50))
            .with_metric("quality");

        assert_eq!(experiment.variants.len(), 2);
        assert_eq!(experiment.metrics.len(), 1);
    }

    #[tokio::test]
    async fn test_variant_assignment() {
        let manager = ABTestManager::new();

        let mut experiment = Experiment::new("test")
            .with_variant(Variant::new("a", 50))
            .with_variant(Variant::new("b", 50));
        experiment.start();

        manager.add_experiment(experiment).await;

        let variant = manager.assign_variant("test", "user_1").await;
        assert!(variant.is_some());

        // Same user should get same variant
        let variant2 = manager.assign_variant("test", "user_1").await;
        assert_eq!(variant.unwrap().id, variant2.unwrap().id);
    }

    #[tokio::test]
    async fn test_metric_recording() {
        let manager = ABTestManager::new();

        let mut experiment = Experiment::new("test")
            .with_variant(Variant::new("a", 100))
            .with_metric("quality");
        experiment.start();

        manager.add_experiment(experiment).await;
        manager.assign_variant("test", "user_1").await;

        manager.record_metric("test", "user_1", "quality", 0.85).await;
        manager.record_metric("test", "user_1", "quality", 0.90).await;

        let results = manager.get_results("test").await.unwrap();
        assert_eq!(results.variants.len(), 1);
    }

    #[tokio::test]
    async fn test_deterministic_assignment() {
        let manager = ABTestManager::new();

        let mut experiment = Experiment::new("test")
            .with_variant(Variant::new("a", 50))
            .with_variant(Variant::new("b", 50));
        experiment.start();

        manager.add_experiment(experiment).await;

        // Multiple calls should return same variant
        let v1 = manager.assign_variant("test", "consistent_user").await;
        let v2 = manager.assign_variant("test", "consistent_user").await;
        let v3 = manager.assign_variant("test", "consistent_user").await;

        let v1_id = v1.as_ref().unwrap().id.clone();
        let v2_id = v2.as_ref().unwrap().id.clone();
        let v3_id = v3.as_ref().unwrap().id.clone();
        assert_eq!(v1_id, v2_id);
        assert_eq!(v2_id, v3_id);
    }
}
