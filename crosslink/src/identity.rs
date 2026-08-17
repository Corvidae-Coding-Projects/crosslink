use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::signing;
use crate::utils::is_windows_reserved_name;

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AgentRole {
    #[default]
    Driver,
    Agent,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentConfig {
    pub agent_id: String,
    pub machine_id: String,
    #[serde(default)]
    pub description: Option<String>,

    #[serde(default)]
    pub role: AgentRole,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_key_path: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_fingerprint: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_public_key: Option<String>,
}

impl AgentConfig {
    pub fn load(crosslink_dir: &Path) -> Result<Option<Self>> {
        let path = crosslink_dir.join("agent.json");
        if !path.exists() {
            return Ok(None);
        }
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        let config: Self = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse {}", path.display()))?;
        config.validate()?;
        Ok(Some(config))
    }

    pub fn init(crosslink_dir: &Path, agent_id: &str, description: Option<&str>) -> Result<Self> {
        Self::init_with_role(crosslink_dir, agent_id, description, AgentRole::Driver)
    }

    pub fn init_with_role(
        crosslink_dir: &Path,
        agent_id: &str,
        description: Option<&str>,
        role: AgentRole,
    ) -> Result<Self> {
        let machine_id = detect_hostname();
        let config = Self {
            agent_id: agent_id.to_string(),
            machine_id,
            description: description.map(std::string::ToString::to_string),
            role,
            ssh_key_path: None,
            ssh_fingerprint: None,
            ssh_public_key: None,
        };
        config.validate()?;
        let path = crosslink_dir.join("agent.json");
        let json = serde_json::to_string_pretty(&config)?;
        std::fs::write(&path, json)
            .with_context(|| format!("Failed to write {}", path.display()))?;
        Ok(config)
    }

    #[must_use]
    pub fn anonymous(crosslink_dir: &Path) -> Self {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        crosslink_dir.hash(&mut hasher);
        let hash = hasher.finish();
        let truncated: u32 = (hash & 0xFFFF_FFFF) as u32;
        let short = format!("{truncated:08x}");
        Self {
            agent_id: format!("anon-{short}"),
            machine_id: detect_hostname(),
            description: Some("Anonymous agent (pre-init)".to_string()),
            role: AgentRole::Driver,
            ssh_key_path: None,
            ssh_fingerprint: None,
            ssh_public_key: None,
        }
    }

    fn validate(&self) -> Result<()> {
        anyhow::ensure!(!self.agent_id.is_empty(), "agent_id cannot be empty");
        anyhow::ensure!(
            self.agent_id.len() >= 3,
            "agent_id must be at least 3 characters"
        );
        anyhow::ensure!(
            self.agent_id
                .chars()
                .all(|c| c.is_alphanumeric() || c == '-' || c == '_'),
            "agent_id must be alphanumeric with hyphens/underscores only"
        );
        anyhow::ensure!(
            self.agent_id.len() <= 64,
            "agent_id must be <= 64 characters"
        );
        anyhow::ensure!(
            !is_windows_reserved_name(&self.agent_id),
            "agent_id '{}' is a Windows reserved filename and cannot be used",
            self.agent_id
        );
        Ok(())
    }
}

fn detect_hostname() -> String {
    if let Ok(name) = std::env::var("COMPUTERNAME") {
        return name;
    }

    if let Ok(name) = std::env::var("HOSTNAME") {
        return name;
    }

    if let Ok(output) = std::process::Command::new("hostname").output() {
        if output.status.success() {
            let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !name.is_empty() {
                return name;
            }
        }
    }
    "unknown".to_string()
}

#[must_use]
pub fn resolve_driver_fingerprint(crosslink_dir: &Path) -> Option<String> {
    let driver_pub = crosslink_dir.join("driver-key.pub");
    if !driver_pub.exists() {
        return None;
    }
    signing::get_key_fingerprint(&driver_pub).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use tempfile::tempdir;

    #[test]
    fn test_load_missing_file() {
        let dir = tempdir().unwrap();
        let result = AgentConfig::load(dir.path()).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_init_and_load_roundtrip() {
        let dir = tempdir().unwrap();
        let config = AgentConfig::init(dir.path(), "worker-1", Some("Test agent")).unwrap();
        assert_eq!(config.agent_id, "worker-1");
        assert_eq!(config.description, Some("Test agent".to_string()));
        assert!(!config.machine_id.is_empty());

        let loaded = AgentConfig::load(dir.path()).unwrap().unwrap();
        assert_eq!(loaded.agent_id, config.agent_id);
        assert_eq!(loaded.machine_id, config.machine_id);
        assert_eq!(loaded.description, config.description);
    }

    #[test]
    fn test_init_no_description() {
        let dir = tempdir().unwrap();
        let config = AgentConfig::init(dir.path(), "worker-2", None).unwrap();
        assert_eq!(config.agent_id, "worker-2");
        assert!(config.description.is_none());
    }

    fn test_config(agent_id: &str) -> AgentConfig {
        AgentConfig {
            agent_id: agent_id.to_string(),
            machine_id: "test".to_string(),
            description: None,
            role: AgentRole::Driver,
            ssh_key_path: None,
            ssh_fingerprint: None,
            ssh_public_key: None,
        }
    }

    #[test]
    fn test_validate_empty_id() {
        let config = test_config("");
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_invalid_chars() {
        assert!(test_config("worker 1").validate().is_err());
        assert!(test_config("worker@1").validate().is_err());
    }

    #[test]
    fn test_validate_too_long() {
        let config = AgentConfig {
            agent_id: "a".repeat(65),
            ..test_config("xxx")
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_too_short() {
        assert!(test_config("a").validate().is_err());
        assert!(test_config("ab").validate().is_err());
        assert!(test_config("abc").validate().is_ok());
    }

    #[test]
    fn test_validate_valid_ids() {
        for id in &["worker-1", "agent_2", "MyAgent", "abc", "test-agent-42"] {
            assert!(test_config(id).validate().is_ok(), "Failed for id: {id}");
        }
    }

    #[test]
    fn test_validate_rejects_windows_reserved_names() {
        for id in &["CON", "con", "PRN", "AUX", "NUL", "COM1", "LPT1"] {
            let err = test_config(id).validate();
            assert!(err.is_err(), "Should reject Windows reserved name: {id}");
            assert!(
                err.unwrap_err()
                    .to_string()
                    .contains("Windows reserved filename"),
                "Error message should mention Windows reserved filename for: {id}"
            );
        }
    }

    #[test]
    fn test_json_roundtrip() {
        let config = AgentConfig {
            description: Some("Test agent".to_string()),
            machine_id: "my-host".to_string(),
            ..test_config("worker-1")
        };
        let json = serde_json::to_string(&config).unwrap();
        let parsed: AgentConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config, parsed);
    }

    #[test]
    fn test_json_missing_description_defaults_none() {
        let json = r#"{"agent_id": "worker-1", "machine_id": "host"}"#;
        let config: AgentConfig = serde_json::from_str(json).unwrap();
        assert!(config.description.is_none());
        assert!(config.ssh_key_path.is_none());
        assert!(config.ssh_fingerprint.is_none());
        assert!(config.ssh_public_key.is_none());
    }

    #[test]
    fn test_json_backward_compat_no_ssh_fields() {
        let json = r#"{"agent_id": "worker-1", "machine_id": "host", "description": "old agent"}"#;
        let config: AgentConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.agent_id, "worker-1");
        assert!(config.ssh_key_path.is_none());
    }

    #[test]
    fn test_json_backward_compat_no_role_field_defaults_driver() {
        let json = r#"{"agent_id": "worker-1", "machine_id": "host"}"#;
        let config: AgentConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.role, AgentRole::Driver);
    }

    #[test]
    fn test_role_serde_roundtrip_driver() {
        let config = AgentConfig {
            role: AgentRole::Driver,
            ..test_config("worker-1")
        };
        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains("\"role\":\"driver\""));
        let parsed: AgentConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.role, AgentRole::Driver);
    }

    #[test]
    fn test_role_serde_roundtrip_agent() {
        let config = AgentConfig {
            role: AgentRole::Agent,
            ..test_config("worker-1")
        };
        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains("\"role\":\"agent\""));
        let parsed: AgentConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.role, AgentRole::Agent);
    }

    #[test]
    fn test_init_defaults_to_driver_role() {
        let dir = tempdir().unwrap();
        let config = AgentConfig::init(dir.path(), "worker-1", None).unwrap();
        assert_eq!(config.role, AgentRole::Driver);
        let loaded = AgentConfig::load(dir.path()).unwrap().unwrap();
        assert_eq!(loaded.role, AgentRole::Driver);
    }

    #[test]
    fn test_init_with_role_agent_persists() {
        let dir = tempdir().unwrap();
        let config =
            AgentConfig::init_with_role(dir.path(), "worker-1", None, AgentRole::Agent).unwrap();
        assert_eq!(config.role, AgentRole::Agent);
        let loaded = AgentConfig::load(dir.path()).unwrap().unwrap();
        assert_eq!(loaded.role, AgentRole::Agent);
    }

    #[test]
    fn test_json_with_ssh_fields() {
        let config = AgentConfig {
            ssh_key_path: Some("keys/test_ed25519".to_string()),
            ssh_fingerprint: Some("SHA256:abc123".to_string()),
            ssh_public_key: Some("ssh-ed25519 AAAA test".to_string()),
            ..test_config("worker-1")
        };
        let json = serde_json::to_string(&config).unwrap();
        let parsed: AgentConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.ssh_key_path, Some("keys/test_ed25519".to_string()));
        assert_eq!(parsed.ssh_fingerprint, Some("SHA256:abc123".to_string()));
    }

    #[test]
    fn test_detect_hostname_returns_something() {
        let hostname = detect_hostname();
        assert!(!hostname.is_empty());
    }

    #[test]
    fn test_resolve_driver_fingerprint_missing_file() {
        let dir = tempdir().unwrap();
        assert!(resolve_driver_fingerprint(dir.path()).is_none());
    }

    #[test]
    fn test_resolve_driver_fingerprint_invalid_content() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("driver-key.pub"), "not a key").unwrap();

        assert!(resolve_driver_fingerprint(dir.path()).is_none());
    }

    #[test]
    fn test_anonymous_produces_valid_config() {
        let dir = tempdir().unwrap();
        let config = AgentConfig::anonymous(dir.path());
        assert!(config.agent_id.starts_with("anon-"));
        assert_eq!(config.agent_id.len(), "anon-".len() + 8);
        assert_eq!(
            config.description,
            Some("Anonymous agent (pre-init)".to_string())
        );
        assert!(!config.machine_id.is_empty());
        assert!(config.ssh_key_path.is_none());
        assert!(config.ssh_fingerprint.is_none());
        assert!(config.ssh_public_key.is_none());
    }

    #[test]
    fn test_anonymous_is_stable_for_same_path() {
        let dir = tempdir().unwrap();
        let config1 = AgentConfig::anonymous(dir.path());
        let config2 = AgentConfig::anonymous(dir.path());
        assert_eq!(config1.agent_id, config2.agent_id);
    }

    #[test]
    fn test_anonymous_differs_for_different_paths() {
        let dir1 = tempdir().unwrap();
        let dir2 = tempdir().unwrap();
        let config1 = AgentConfig::anonymous(dir1.path());
        let config2 = AgentConfig::anonymous(dir2.path());

        assert_ne!(config1.agent_id, config2.agent_id);
    }

    #[test]
    fn test_detect_hostname_with_computername_env() {
        std::env::set_var("COMPUTERNAME", "test-host-win");
        let hostname = detect_hostname();
        assert_eq!(hostname, "test-host-win");
        std::env::remove_var("COMPUTERNAME");
    }

    #[test]
    fn test_detect_hostname_from_hostname_env() {
        let hostname = detect_hostname();

        assert!(
            !hostname.is_empty(),
            "detect_hostname should never return empty"
        );

        if let Ok(env_val) = std::env::var("HOSTNAME") {
            if std::env::var("COMPUTERNAME").is_err() {
                assert_eq!(hostname, env_val);
            }
        }
    }

    #[test]
    fn test_detect_hostname_returns_non_empty() {
        std::env::remove_var("COMPUTERNAME");
        std::env::remove_var("HOSTNAME");
        let hostname = detect_hostname();
        assert!(!hostname.is_empty());
    }

    proptest! {
        #[test]
        fn prop_valid_ids_roundtrip(id in "[a-zA-Z0-9_-]{3,64}") {



            prop_assume!(!is_windows_reserved_name(&id));
            let config = test_config(&id);
            prop_assert!(config.validate().is_ok());
            let json = serde_json::to_string(&config).unwrap();
            let parsed: AgentConfig = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(parsed.agent_id, id);
        }
    }
}
