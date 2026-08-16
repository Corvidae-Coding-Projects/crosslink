use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SentinelConfig {
    pub enabled: bool,
    pub interval_minutes: u64,
    pub max_concurrent_agents: u32,
    pub sources: SourcesConfig,
    pub default_agent: DefaultAgentConfig,
    pub escalation: EscalationConfig,
    pub webhook: WebhookServerConfig,
    pub notifications: NotificationConfig,
}

impl Default for SentinelConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_minutes: 10,
            max_concurrent_agents: 3,
            sources: SourcesConfig::default(),
            default_agent: DefaultAgentConfig::default(),
            escalation: EscalationConfig::default(),
            webhook: WebhookServerConfig::default(),
            notifications: NotificationConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SourcesConfig {
    pub github_labels: GitHubLabelsConfig,
    pub internal_hygiene: InternalHygieneConfig,
    pub github_ci: GitHubCIConfig,
    pub maintenance_sweep: MaintenanceSweepSourceConfig,
    pub cpitd: CpitdSourceConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct GitHubLabelsConfig {
    pub enabled: bool,
    pub labels: Vec<String>,
}

impl Default for GitHubLabelsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            labels: vec![
                "agent-todo: replicate".to_string(),
                "agent-todo: fix".to_string(),
            ],
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct InternalHygieneConfig {
    pub enabled: bool,
    pub stale_threshold_days: i64,
}

impl Default for InternalHygieneConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            stale_threshold_days: 30,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct MaintenanceSweepSourceConfig {
    pub enabled: bool,
    pub lint_enabled: bool,
    pub test_coverage_enabled: bool,
    pub lint_warning_threshold: u64,
}

impl Default for MaintenanceSweepSourceConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            lint_enabled: true,
            test_coverage_enabled: false,
            lint_warning_threshold: 10,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct GitHubCIConfig {
    pub enabled: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CpitdSourceConfig {
    pub enabled: bool,

    pub interval_hours: u64,

    pub min_tokens: u32,
}

impl Default for CpitdSourceConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_hours: 168,
            min_tokens: 50,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DefaultAgentConfig {
    #[serde(alias = "model_tier")]
    pub model: String,
    pub timeout_minutes: u64,

    pub verify: String,
}

impl DefaultAgentConfig {
    pub fn verify_level(&self) -> crate::commands::kickoff::VerifyLevel {
        crate::commands::kickoff::parse_verify_level(&self.verify)
            .unwrap_or(crate::commands::kickoff::VerifyLevel::Local)
    }
}

impl Default for DefaultAgentConfig {
    fn default() -> Self {
        Self {
            model: "standard".to_string(),
            timeout_minutes: 30,
            verify: "local".to_string(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct WebhookServerConfig {
    pub enabled: bool,
    pub port: u16,

    pub secret: Option<String>,
}

impl Default for WebhookServerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            port: 9876,
            secret: None,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct NotificationConfig {
    pub enabled: bool,

    pub webhook_urls: Vec<String>,
}

impl NotificationConfig {
    pub fn is_slack_url(url: &str) -> bool {
        url.contains("hooks.slack.com")
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct EscalationConfig {
    pub enabled: bool,
    #[serde(alias = "model_tier")]
    pub model: String,
    pub cooldown_minutes: u64,
    pub max_attempts: u32,

    pub timeout_multiplier_pct: u32,
}

impl Default for EscalationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            model: "advanced".to_string(),
            cooldown_minutes: 30,
            max_attempts: 2,
            timeout_multiplier_pct: 150,
        }
    }
}

impl SentinelConfig {
    pub fn load(crosslink_dir: &Path) -> Result<Self> {
        let config_path = crosslink_dir.join("hook-config.json");
        if !config_path.exists() {
            return Ok(Self::default());
        }
        let content = std::fs::read_to_string(&config_path)
            .with_context(|| format!("Failed to read {}", config_path.display()))?;
        let root: serde_json::Value = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse {}", config_path.display()))?;
        match root.get("sentinel") {
            Some(sentinel_val) => {
                let config: SentinelConfig = serde_json::from_value(sentinel_val.clone())
                    .context("Failed to parse sentinel config")?;
                Ok(config)
            }
            None => Ok(Self::default()),
        }
    }
}
