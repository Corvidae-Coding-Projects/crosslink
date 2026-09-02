use anyhow::Result;
use std::collections::HashMap;

use crate::application::LocalStateService;

use super::config::SentinelConfig;

#[derive(Debug, Clone)]
pub struct TuningOverride {
    overrides: HashMap<String, String>,
}

impl TuningOverride {
    pub fn from_history(db: &impl LocalStateService, config: &SentinelConfig) -> Result<Self> {
        let metrics = db.get_dispatch_metrics()?;
        let mut overrides = HashMap::new();

        let default_model = &config.default_agent.model;
        let escalation_model = &config.escalation.model;
        let threshold = 40.0;

        for m in &metrics {
            if m.model != *default_model {
                continue;
            }
            let completed = m.total - m.pending;
            if completed < 5 {
                continue;
            }
            if m.success_rate < threshold {
                tracing::info!(
                    "self-tuning: promoting '{}' from {} to {} (success rate {:.0}% < {:.0}%)",
                    m.label,
                    default_model,
                    escalation_model,
                    m.success_rate,
                    threshold
                );
                overrides.insert(m.label.clone(), escalation_model.clone());
            }
        }

        Ok(Self { overrides })
    }

    pub fn model_for_label(&self, label: &str) -> Option<&str> {
        self.overrides.get(label).map(String::as_str)
    }

    pub fn has_overrides(&self) -> bool {
        !self.overrides.is_empty()
    }

    pub fn none() -> Self {
        Self {
            overrides: HashMap::new(),
        }
    }
}
