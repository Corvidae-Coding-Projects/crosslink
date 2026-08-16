use anyhow::Result;
use chrono::Utc;

use super::core::SyncManager;
use crate::identity::AgentConfig;
use crate::locks::Heartbeat;

impl SyncManager {
    pub fn push_heartbeat(&self, agent: &AgentConfig, active_issue_id: Option<i64>) -> Result<()> {
        let _lock_guard = self.acquire_lock()?;

        if !self.hub_mode.get().is_v3() {
            return Ok(());
        }

        let heartbeat = Heartbeat {
            agent_id: agent.agent_id.clone(),
            last_heartbeat: Utc::now(),
            active_issue_id,
            machine_id: agent.machine_id.clone(),
        };

        crate::hub_v3::write_heartbeat_to_ref(&self.cache_dir, &agent.agent_id, &heartbeat)?;
        if self.remote_exists() {
            match crate::hub_v3::push_agent_ref(&self.cache_dir, &self.remote, &agent.agent_id)? {
                crate::hub_v3::PushOutcome::Pushed | crate::hub_v3::PushOutcome::NoRemote => {}
                other => {
                    tracing::warn!(
                        "v3 heartbeat push for '{}' did not complete: {other:?}",
                        agent.agent_id
                    );
                }
            }
        }
        Ok(())
    }

    pub fn read_heartbeats_auto(&self) -> Result<Vec<Heartbeat>> {
        if self.hub_mode.get().is_v3() {
            return Ok(crate::hub_v3::read_heartbeats_from_refs(&self.cache_dir)?
                .into_iter()
                .map(|(_, hb)| hb)
                .collect());
        }
        self.read_heartbeats_v2()
    }

    pub fn read_heartbeats_v2(&self) -> Result<Vec<Heartbeat>> {
        let agents_dir = self.cache_dir.join("agents");
        if !agents_dir.exists() {
            return Ok(Vec::new());
        }
        let mut heartbeats = Vec::new();
        for entry in std::fs::read_dir(&agents_dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let agent_id = entry.file_name().to_string_lossy().to_string();
            let hb_path = entry.path().join("heartbeat.json");
            if !hb_path.exists() {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(&hb_path) else {
                continue;
            };

            if let Ok(hb) = serde_json::from_str::<Heartbeat>(&content) {
                heartbeats.push(hb);
            } else if let Ok(val) = serde_json::from_str::<serde_json::Value>(&content) {
                let Some(timestamp) = val
                    .get("timestamp")
                    .and_then(|t| t.as_str())
                    .and_then(|ts| chrono::DateTime::parse_from_rfc3339(ts).ok())
                    .map(|dt| dt.with_timezone(&Utc))
                else {
                    tracing::warn!(
                        "corrupt or missing timestamp in heartbeat for agent '{}', skipping",
                        agent_id
                    );
                    continue;
                };
                let active_issue_id = val
                    .get("active_issue_id")
                    .and_then(serde_json::Value::as_i64);
                let machine_id = val
                    .get("machine_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                heartbeats.push(Heartbeat {
                    agent_id,
                    last_heartbeat: timestamp,
                    active_issue_id,
                    machine_id,
                });
            }
        }
        Ok(heartbeats)
    }
}
