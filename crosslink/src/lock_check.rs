use anyhow::{bail, Result};
use std::path::Path;

use crate::db::Database;
use crate::identity::AgentConfig;
use crate::sync::SyncManager;

#[derive(Debug, PartialEq, Eq)]
pub enum LockStatus {
    NotConfigured,

    Available,

    LockedBySelf,

    LockedByOther { agent_id: String, stale: bool },
}

pub fn check_lock(crosslink_dir: &Path, issue_id: i64) -> Result<LockStatus> {
    let Some(agent) = AgentConfig::load(crosslink_dir)? else {
        return Ok(LockStatus::NotConfigured);
    };

    let Ok(sync) = SyncManager::new(crosslink_dir) else {
        return Ok(LockStatus::NotConfigured);
    };

    if !sync.is_initialized() {
        return Ok(LockStatus::NotConfigured);
    }

    let Ok(locks) = sync.read_locks_auto() else {
        return Ok(LockStatus::NotConfigured);
    };

    if locks.is_locked_by(issue_id, &agent.agent_id) {
        return Ok(LockStatus::LockedBySelf);
    }

    locks
        .get_lock(issue_id)
        .map_or(Ok(LockStatus::Available), |lock| {
            let stale = sync
                .find_stale_locks()
                .unwrap_or_default()
                .iter()
                .any(|(id, _)| *id == issue_id);
            Ok(LockStatus::LockedByOther {
                agent_id: lock.agent_id.clone(),
                stale,
            })
        })
}

fn read_auto_steal_config(crosslink_dir: &Path) -> Option<u64> {
    let config_path = crosslink_dir.join("hook-config.json");
    let content = std::fs::read_to_string(&config_path).ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&content).ok()?;
    match parsed.get("auto_steal_stale_locks")? {
        serde_json::Value::Bool(true) => Some(1),
        serde_json::Value::Number(n) => n.as_u64().filter(|&v| v > 0),
        serde_json::Value::String(s) if s == "true" => Some(1),
        serde_json::Value::String(s) if s != "false" => s.parse::<u64>().ok().filter(|&v| v > 0),
        _ => None,
    }
}

fn auto_steal_if_configured(
    crosslink_dir: &Path,
    issue_id: i64,
    stale_agent_id: &str,
    db: &Database,
) -> Result<bool> {
    let Some(multiplier) = read_auto_steal_config(crosslink_dir) else {
        return Ok(false);
    };

    let Ok(sync) = SyncManager::new(crosslink_dir) else {
        return Ok(false);
    };

    if !sync.is_initialized() {
        return Ok(false);
    }

    let stale_locks = sync.find_stale_locks_with_age()?;
    let stale_minutes = match stale_locks.iter().find(|(id, _, _)| *id == issue_id) {
        Some((_, _, mins)) => *mins,
        None => return Ok(false),
    };

    let stale_timeout = if sync.is_v2_layout() {
        30u64
    } else {
        sync.read_locks_auto()
            .map_or(60, |l| l.settings.stale_lock_timeout_minutes)
    };
    let auto_steal_threshold = multiplier.saturating_mul(stale_timeout);

    if stale_minutes < auto_steal_threshold {
        return Ok(false);
    }

    if sync.hub_mode().is_v3() {
        if let Ok(Some(writer)) = crate::shared_writer::SharedWriter::new(crosslink_dir) {
            writer.steal_lock_v2(issue_id, stale_agent_id, None)?;
            let comment = format!(
                "[auto-steal] Lock auto-stolen from agent '{stale_agent_id}' (stale for {stale_minutes} min, threshold: {auto_steal_threshold} min)"
            );
            if let Err(e) = writer.add_comment(db, issue_id, &comment, "system") {
                tracing::warn!("could not add audit comment for lock steal: {e}");
            }
        } else {
            return Ok(false);
        }
    } else {
        return Ok(false);
    }

    Ok(true)
}

pub fn enforce_lock(crosslink_dir: &Path, issue_id: i64, db: &Database) -> Result<()> {
    let _operation = crate::reconcile::readiness::acquire_mutation_operation_permit(crosslink_dir)?;
    match check_lock(crosslink_dir, issue_id)? {
        LockStatus::NotConfigured | LockStatus::Available | LockStatus::LockedBySelf => Ok(()),
        LockStatus::LockedByOther { agent_id, stale } => {
            if stale {
                match auto_steal_if_configured(crosslink_dir, issue_id, &agent_id, db) {
                    Ok(true) => {
                        tracing::info!(
                            "Auto-stole stale lock on issue #{} from '{}'.",
                            issue_id,
                            agent_id
                        );
                        return Ok(());
                    }
                    Ok(false) => {}
                    Err(e) => {
                        tracing::warn!(
                            "Auto-steal of stale lock on #{} failed: {}. Proceeding.",
                            issue_id,
                            e
                        );
                    }
                }

                tracing::warn!(
                    "Issue {} is locked by '{}' but the lock appears STALE. Proceeding.",
                    crate::utils::format_issue_id(issue_id),
                    agent_id
                );
                Ok(())
            } else {
                bail!(
                    "Issue {} is locked by agent '{}'. \
                     Use 'crosslink locks check {}' for details. \
                     Ask the human to release it or wait for the lock to expire.",
                    crate::utils::format_issue_id(issue_id),
                    agent_id,
                    issue_id
                )
            }
        }
    }
}

pub fn release_lock_best_effort(crosslink_dir: &Path, issue_id: i64) {
    let Ok(Some(_agent)) = AgentConfig::load(crosslink_dir) else {
        return;
    };
    let Ok(sync) = SyncManager::new(crosslink_dir) else {
        return;
    };
    if !sync.is_initialized() || !sync.hub_mode().is_v3() {
        return;
    }
    if let Ok(Some(writer)) = crate::shared_writer::SharedWriter::new(crosslink_dir) {
        if let Err(e) = writer.release_lock_v2(issue_id) {
            tracing::warn!(
                "Could not release lock on {}: {}",
                crate::utils::format_issue_id(issue_id),
                e
            );
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum ClaimResult {
    Claimed,

    AlreadyHeld,

    Contended { winner_agent_id: String },

    NotConfigured,
}

pub fn try_claim_lock(
    crosslink_dir: &Path,
    issue_id: i64,
    branch: Option<&str>,
) -> Result<ClaimResult> {
    let Some(_agent) = AgentConfig::load(crosslink_dir)? else {
        return Ok(ClaimResult::NotConfigured);
    };
    let sync = match SyncManager::new(crosslink_dir) {
        Ok(s) if s.is_initialized() => s,
        _ => return Ok(ClaimResult::NotConfigured),
    };

    if sync.hub_mode().is_v3() {
        let Some(writer) = crate::shared_writer::SharedWriter::new(crosslink_dir)? else {
            return Ok(ClaimResult::NotConfigured);
        };
        match writer.claim_lock_v2(issue_id, branch)? {
            crate::shared_writer::LockClaimResult::Claimed => Ok(ClaimResult::Claimed),
            crate::shared_writer::LockClaimResult::AlreadyHeld => Ok(ClaimResult::AlreadyHeld),
            crate::shared_writer::LockClaimResult::Contended { winner_agent_id } => {
                Ok(ClaimResult::Contended { winner_agent_id })
            }
        }
    } else {
        Ok(ClaimResult::NotConfigured)
    }
}

pub fn try_release_lock(crosslink_dir: &Path, issue_id: i64) -> Result<bool> {
    let Some(_agent) = AgentConfig::load(crosslink_dir)? else {
        return Ok(false);
    };
    let sync = match SyncManager::new(crosslink_dir) {
        Ok(s) if s.is_initialized() => s,
        _ => return Ok(false),
    };

    if sync.hub_mode().is_v3() {
        let Some(writer) = crate::shared_writer::SharedWriter::new(crosslink_dir)? else {
            return Ok(false);
        };
        writer.release_lock_v2(issue_id)
    } else {
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn temp_db() -> Database {
        Database::open(std::path::Path::new(":memory:")).unwrap()
    }

    fn write_agent_config(crosslink_dir: &Path, agent_id: &str) {
        let agent_json = serde_json::json!({
            "agent_id": agent_id,
            "machine_id": "test-machine"
        });
        std::fs::write(
            crosslink_dir.join("agent.json"),
            serde_json::to_string(&agent_json).unwrap(),
        )
        .unwrap();
    }

    #[allow(clippy::too_many_arguments)]
    fn seed_v3_hub(
        self_agent: &str,
        issue_id: i64,
        lock_holder: Option<&str>,
        holder_fresh: bool,
    ) -> Option<(tempfile::TempDir, std::path::PathBuf)> {
        let dir = tempdir().unwrap();
        let repo_root = dir.path();
        let ok = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(repo_root)
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            return None;
        }
        for cfg in [["user.email", "t@t.local"], ["user.name", "t"]] {
            let _ = std::process::Command::new("git")
                .args(["config", cfg[0], cfg[1]])
                .current_dir(repo_root)
                .status();
        }

        let crosslink_dir = repo_root.join(".crosslink");
        std::fs::create_dir_all(&crosslink_dir).unwrap();
        write_agent_config(&crosslink_dir, self_agent);
        std::fs::write(crosslink_dir.join("hook-config.json"), r"{}").unwrap();

        let sync = SyncManager::new(&crosslink_dir).unwrap();
        sync.init_cache().unwrap();
        assert!(sync.hub_mode().is_v3(), "seed helper must produce a v3 hub");
        let cache_dir = sync.cache_path();

        if let Some(holder) = lock_holder {
            let claimed_at = if holder_fresh {
                chrono::Utc::now()
            } else {
                chrono::Utc::now() - chrono::Duration::minutes(200)
            };
            let mut state = crate::checkpoint::CheckpointState {
                watermark: Some(crate::hub_v3::genesis_sentinel_watermark()),
                ..Default::default()
            };
            state.locks.insert(
                issue_id,
                crate::checkpoint::LockEntry {
                    agent_id: holder.to_string(),
                    branch: None,
                    claimed_at,
                },
            );
            let bytes = serde_json::to_vec_pretty(&state).unwrap();
            crate::hub_v3::commit_blob_to_ref(
                cache_dir,
                crate::hub_v3::CHECKPOINT_REF,
                "state.json",
                &bytes,
                "test: seed v3 lock",
            )
            .unwrap();

            if holder_fresh {
                let hb = crate::locks::Heartbeat {
                    agent_id: holder.to_string(),
                    last_heartbeat: claimed_at,
                    active_issue_id: Some(issue_id),
                    machine_id: "holder-machine".to_string(),
                };
                crate::hub_v3::write_heartbeat_to_ref(cache_dir, holder, &hb).unwrap();
            }
        }

        crate::reconcile::migration::establish_verified_readiness_for_test(
            &crosslink_dir,
            "lock-check-test",
        )
        .unwrap();

        Some((dir, crosslink_dir))
    }

    #[test]
    fn test_no_agent_config_returns_not_configured() {
        let dir = tempdir().unwrap();
        let crosslink_dir = dir.path().join(".crosslink");
        std::fs::create_dir_all(&crosslink_dir).unwrap();

        let status = check_lock(&crosslink_dir, 1).unwrap();
        assert_eq!(status, LockStatus::NotConfigured);
    }

    #[test]
    fn test_enforce_not_configured_allows() {
        let dir = tempdir().unwrap();
        let crosslink_dir = dir.path().join(".crosslink");
        std::fs::create_dir_all(&crosslink_dir).unwrap();

        let db = temp_db();

        assert!(enforce_lock(&crosslink_dir, 1, &db).is_ok());
    }

    #[test]
    fn test_enforce_available_allows() {}

    #[test]
    fn test_lock_status_debug() {
        let statuses = vec![
            LockStatus::NotConfigured,
            LockStatus::Available,
            LockStatus::LockedBySelf,
            LockStatus::LockedByOther {
                agent_id: "worker-1".to_string(),
                stale: false,
            },
            LockStatus::LockedByOther {
                agent_id: "worker-2".to_string(),
                stale: true,
            },
        ];
        for s in statuses {
            let _ = format!("{s:?}");
        }
    }

    #[test]
    fn test_lock_status_equality() {
        assert_eq!(LockStatus::NotConfigured, LockStatus::NotConfigured);
        assert_eq!(LockStatus::Available, LockStatus::Available);
        assert_eq!(LockStatus::LockedBySelf, LockStatus::LockedBySelf);
        assert_ne!(LockStatus::Available, LockStatus::NotConfigured);
        assert_eq!(
            LockStatus::LockedByOther {
                agent_id: "a".to_string(),
                stale: false
            },
            LockStatus::LockedByOther {
                agent_id: "a".to_string(),
                stale: false
            }
        );
        assert_ne!(
            LockStatus::LockedByOther {
                agent_id: "a".to_string(),
                stale: false
            },
            LockStatus::LockedByOther {
                agent_id: "b".to_string(),
                stale: false
            }
        );

        assert_ne!(
            LockStatus::LockedByOther {
                agent_id: "a".to_string(),
                stale: false
            },
            LockStatus::LockedByOther {
                agent_id: "a".to_string(),
                stale: true
            }
        );
    }

    #[test]
    fn test_check_lock_agent_config_no_cache_returns_not_configured() {
        let dir = tempdir().unwrap();
        let crosslink_dir = dir.path().join(".crosslink");
        std::fs::create_dir_all(&crosslink_dir).unwrap();
        write_agent_config(&crosslink_dir, "worker-1");

        let status = check_lock(&crosslink_dir, 42).unwrap();
        assert_eq!(status, LockStatus::NotConfigured);
    }

    #[test]
    fn test_enforce_lock_agent_config_no_cache_allows() {
        let dir = tempdir().unwrap();
        let crosslink_dir = dir.path().join(".crosslink");
        std::fs::create_dir_all(&crosslink_dir).unwrap();
        write_agent_config(&crosslink_dir, "worker-1");

        let db = temp_db();
        assert!(enforce_lock(&crosslink_dir, 42, &db).is_ok());
    }

    #[test]
    fn test_auto_steal_config_disabled() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("hook-config.json"),
            r#"{"auto_steal_stale_locks": false}"#,
        )
        .unwrap();
        assert_eq!(read_auto_steal_config(dir.path()), None);
    }

    #[test]
    fn test_auto_steal_config_enabled_int() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("hook-config.json"),
            r#"{"auto_steal_stale_locks": 2}"#,
        )
        .unwrap();
        assert_eq!(read_auto_steal_config(dir.path()), Some(2));
    }

    #[test]
    fn test_auto_steal_config_enabled_string() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("hook-config.json"),
            r#"{"auto_steal_stale_locks": "3"}"#,
        )
        .unwrap();
        assert_eq!(read_auto_steal_config(dir.path()), Some(3));
    }

    #[test]
    fn test_auto_steal_config_string_false() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("hook-config.json"),
            r#"{"auto_steal_stale_locks": "false"}"#,
        )
        .unwrap();
        assert_eq!(read_auto_steal_config(dir.path()), None);
    }

    #[test]
    fn test_auto_steal_config_missing_key() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("hook-config.json"), r"{}").unwrap();
        assert_eq!(read_auto_steal_config(dir.path()), None);
    }

    #[test]
    fn test_auto_steal_config_no_file() {
        let dir = tempdir().unwrap();
        assert_eq!(read_auto_steal_config(dir.path()), None);
    }

    #[test]
    fn test_auto_steal_config_zero_disabled() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("hook-config.json"),
            r#"{"auto_steal_stale_locks": 0}"#,
        )
        .unwrap();
        assert_eq!(read_auto_steal_config(dir.path()), None);
    }

    #[test]
    fn test_auto_steal_config_bool_true_returns_default() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("hook-config.json"),
            r#"{"auto_steal_stale_locks": true}"#,
        )
        .unwrap();
        assert_eq!(read_auto_steal_config(dir.path()), Some(1));
    }

    #[test]
    fn test_auto_steal_config_null_returns_none() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("hook-config.json"),
            r#"{"auto_steal_stale_locks": null}"#,
        )
        .unwrap();
        assert_eq!(read_auto_steal_config(dir.path()), None);
    }

    #[test]
    fn test_auto_steal_config_array_returns_none() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("hook-config.json"),
            r#"{"auto_steal_stale_locks": [1, 2, 3]}"#,
        )
        .unwrap();
        assert_eq!(read_auto_steal_config(dir.path()), None);
    }

    #[test]
    fn test_auto_steal_config_object_returns_none() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("hook-config.json"),
            r#"{"auto_steal_stale_locks": {"enabled": true}}"#,
        )
        .unwrap();
        assert_eq!(read_auto_steal_config(dir.path()), None);
    }

    #[test]
    fn test_auto_steal_config_string_non_numeric_returns_none() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("hook-config.json"),
            r#"{"auto_steal_stale_locks": "enabled"}"#,
        )
        .unwrap();
        assert_eq!(read_auto_steal_config(dir.path()), None);
    }

    #[test]
    fn test_auto_steal_config_string_zero_returns_none() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("hook-config.json"),
            r#"{"auto_steal_stale_locks": "0"}"#,
        )
        .unwrap();
        assert_eq!(read_auto_steal_config(dir.path()), None);
    }

    #[test]
    fn test_auto_steal_config_one_returns_some_one() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("hook-config.json"),
            r#"{"auto_steal_stale_locks": 1}"#,
        )
        .unwrap();
        assert_eq!(read_auto_steal_config(dir.path()), Some(1));
    }

    #[test]
    fn test_auto_steal_config_invalid_json_returns_none() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("hook-config.json"),
            b"not valid json { at all !!!",
        )
        .unwrap();
        assert_eq!(read_auto_steal_config(dir.path()), None);
    }

    #[test]
    fn test_auto_steal_no_config_returns_false() {
        let dir = tempdir().unwrap();
        let crosslink_dir = dir.path().join(".crosslink");
        std::fs::create_dir_all(&crosslink_dir).unwrap();

        let db = temp_db();
        let result = auto_steal_if_configured(&crosslink_dir, 1, "other-agent", &db);
        assert!(!result.unwrap());
    }

    #[test]
    fn test_auto_steal_disabled_config_returns_false() {
        let dir = tempdir().unwrap();
        let crosslink_dir = dir.path().join(".crosslink");
        std::fs::create_dir_all(&crosslink_dir).unwrap();
        std::fs::write(
            crosslink_dir.join("hook-config.json"),
            r#"{"auto_steal_stale_locks": false}"#,
        )
        .unwrap();

        let db = temp_db();
        let result = auto_steal_if_configured(&crosslink_dir, 1, "other-agent", &db);
        assert!(!result.unwrap());
    }

    #[test]
    fn test_auto_steal_config_enabled_but_no_cache_returns_false() {
        let dir = tempdir().unwrap();
        let crosslink_dir = dir.path().join(".crosslink");
        std::fs::create_dir_all(&crosslink_dir).unwrap();

        std::fs::write(
            crosslink_dir.join("hook-config.json"),
            r#"{"auto_steal_stale_locks": 2}"#,
        )
        .unwrap();

        let db = temp_db();

        let result = auto_steal_if_configured(&crosslink_dir, 1, "other-agent", &db);
        assert!(!result.unwrap());
    }

    #[test]
    fn test_enforce_lock_locked_by_other_non_stale_returns_error() {
        let Some((_dir, crosslink_dir)) = seed_v3_hub("agent-self", 7, Some("other-agent"), true)
        else {
            return;
        };
        let db = temp_db();
        let result = enforce_lock(&crosslink_dir, 7, &db);

        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("other-agent"),
            "error should name the holder: {msg}"
        );
        assert!(msg.contains('7'), "error should name the issue id: {msg}");
    }

    #[test]
    fn test_enforce_lock_locked_by_other_stale_no_auto_steal_proceeds() {
        let Some((_dir, crosslink_dir)) = seed_v3_hub("agent-self", 8, Some("other-agent"), false)
        else {
            return;
        };

        let db = temp_db();

        let result = enforce_lock(&crosslink_dir, 8, &db);
        assert!(
            result.is_ok(),
            "stale lock without auto-steal should proceed: {result:?}"
        );
    }

    #[test]
    fn test_enforce_lock_locked_by_self_succeeds() {
        let dir = tempdir().unwrap();
        let repo_root = dir.path();

        let init_status = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(repo_root)
            .status();
        if init_status.map_or(true, |s| !s.success()) {
            return;
        }

        let crosslink_dir = repo_root.join(".crosslink");
        std::fs::create_dir_all(&crosslink_dir).unwrap();
        write_agent_config(&crosslink_dir, "agent-self");

        let hub_cache = crosslink_dir.join(".hub-cache");
        std::fs::create_dir_all(hub_cache.join("locks")).unwrap();
        std::fs::create_dir_all(hub_cache.join("meta")).unwrap();

        let claimed_at = chrono::Utc::now();
        let locks_json = serde_json::json!({
            "version": 1,
            "locks": {
                "9": {
                    "agent_id": "agent-self",
                    "branch": null,
                    "claimed_at": claimed_at.to_rfc3339(),
                    "signed_by": ""
                }
            },
            "settings": {
                "stale_lock_timeout_minutes": 60
            }
        });
        std::fs::write(
            hub_cache.join("locks.json"),
            serde_json::to_string_pretty(&locks_json).unwrap(),
        )
        .unwrap();

        let db = temp_db();
        assert!(enforce_lock(&crosslink_dir, 9, &db).is_ok());
    }

    #[test]
    fn test_enforce_lock_available_succeeds() {
        let dir = tempdir().unwrap();
        let repo_root = dir.path();

        let init_status = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(repo_root)
            .status();
        if init_status.map_or(true, |s| !s.success()) {
            return;
        }

        let crosslink_dir = repo_root.join(".crosslink");
        std::fs::create_dir_all(&crosslink_dir).unwrap();
        write_agent_config(&crosslink_dir, "agent-self");

        let hub_cache = crosslink_dir.join(".hub-cache");
        std::fs::create_dir_all(hub_cache.join("locks")).unwrap();
        std::fs::create_dir_all(hub_cache.join("meta")).unwrap();

        let locks_json = serde_json::json!({
            "version": 1,
            "locks": {},
            "settings": {"stale_lock_timeout_minutes": 60}
        });
        std::fs::write(
            hub_cache.join("locks.json"),
            serde_json::to_string_pretty(&locks_json).unwrap(),
        )
        .unwrap();

        let db = temp_db();

        assert!(enforce_lock(&crosslink_dir, 10, &db).is_ok());
    }

    #[test]
    fn test_check_lock_locked_by_self() {
        let Some((_dir, crosslink_dir)) = seed_v3_hub("agent-self", 5, Some("agent-self"), true)
        else {
            return;
        };
        let status = check_lock(&crosslink_dir, 5).unwrap();
        assert_eq!(status, LockStatus::LockedBySelf);
    }

    #[test]
    fn test_check_lock_available() {
        let Some((_dir, crosslink_dir)) = seed_v3_hub("agent-self", 5, None, false) else {
            return;
        };
        let status = check_lock(&crosslink_dir, 99).unwrap();
        assert_eq!(status, LockStatus::Available);
    }

    #[test]
    fn test_check_lock_locked_by_other_non_stale() {
        let Some((_dir, crosslink_dir)) = seed_v3_hub("agent-self", 3, Some("other-agent"), true)
        else {
            return;
        };
        let status = check_lock(&crosslink_dir, 3).unwrap();
        match status {
            LockStatus::LockedByOther { agent_id, stale } => {
                assert_eq!(agent_id, "other-agent");
                assert!(!stale);
            }
            other => panic!("Expected LockedByOther, got {other:?}"),
        }
    }

    #[test]
    fn test_check_lock_locked_by_other_stale() {
        let Some((_dir, crosslink_dir)) = seed_v3_hub("agent-self", 4, Some("other-agent"), false)
        else {
            return;
        };
        let status = check_lock(&crosslink_dir, 4).unwrap();
        match status {
            LockStatus::LockedByOther { agent_id, stale } => {
                assert_eq!(agent_id, "other-agent");
                assert!(stale, "lock with no fresh heartbeat must be stale");
            }
            other => panic!("Expected LockedByOther, got {other:?}"),
        }
    }

    fn write_v1_locks_json(
        hub_cache: &Path,
        issue_id: i64,
        agent_id: &str,
        age_minutes: i64,
        timeout_minutes: u64,
    ) {
        let claimed_at = chrono::Utc::now() - chrono::Duration::minutes(age_minutes);
        let locks_json = serde_json::json!({
            "version": 1,
            "locks": {
                issue_id.to_string(): {
                    "agent_id": agent_id,
                    "branch": null,
                    "claimed_at": claimed_at.to_rfc3339(),
                    "signed_by": ""
                }
            },
            "settings": {
                "stale_lock_timeout_minutes": timeout_minutes
            }
        });
        std::fs::write(
            hub_cache.join("locks.json"),
            serde_json::to_string_pretty(&locks_json).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn test_auto_steal_issue_not_in_stale_list_returns_false() {
        let dir = tempdir().unwrap();
        let crosslink_dir = dir.path().join(".crosslink");
        let hub_cache = crosslink_dir.join(".hub-cache");
        std::fs::create_dir_all(&hub_cache).unwrap();

        std::fs::write(
            crosslink_dir.join("hook-config.json"),
            r#"{"auto_steal_stale_locks": 2}"#,
        )
        .unwrap();

        write_v1_locks_json(&hub_cache, 99, "other-agent", 120, 60);

        let db = temp_db();
        let result = auto_steal_if_configured(&crosslink_dir, 50, "other-agent", &db);
        assert!(!result.unwrap());
    }

    #[test]
    fn test_auto_steal_stale_but_below_threshold_returns_false() {
        let dir = tempdir().unwrap();
        let crosslink_dir = dir.path().join(".crosslink");
        let hub_cache = crosslink_dir.join(".hub-cache");
        std::fs::create_dir_all(&hub_cache).unwrap();

        std::fs::write(
            crosslink_dir.join("hook-config.json"),
            r#"{"auto_steal_stale_locks": 2}"#,
        )
        .unwrap();

        write_v1_locks_json(&hub_cache, 30, "other-agent", 90, 60);

        let db = temp_db();
        let result = auto_steal_if_configured(&crosslink_dir, 30, "other-agent", &db);
        assert!(!result.unwrap());
    }

    #[test]
    fn test_auto_steal_no_agent_config_returns_false() {
        let dir = tempdir().unwrap();
        let crosslink_dir = dir.path().join(".crosslink");
        let hub_cache = crosslink_dir.join(".hub-cache");
        std::fs::create_dir_all(&hub_cache).unwrap();

        std::fs::write(
            crosslink_dir.join("hook-config.json"),
            r#"{"auto_steal_stale_locks": 1}"#,
        )
        .unwrap();

        write_v1_locks_json(&hub_cache, 40, "other-agent", 200, 60);

        let db = temp_db();
        let result = auto_steal_if_configured(&crosslink_dir, 40, "other-agent", &db);
        assert!(!result.unwrap());
    }

    #[test]
    fn test_auto_steal_stale_lock_is_stolen_via_v3() {
        let Some((_dir, crosslink_dir)) = seed_v3_hub("agent-self", 55, Some("other-agent"), false)
        else {
            return;
        };
        std::fs::write(
            crosslink_dir.join("hook-config.json"),
            r#"{"auto_steal_stale_locks": 1}"#,
        )
        .unwrap();

        let db = temp_db();
        let result = auto_steal_if_configured(&crosslink_dir, 55, "other-agent", &db);

        assert!(matches!(result, Ok(true)), "expected steal, got {result:?}");
    }

    #[test]
    fn test_enforce_lock_auto_steal_err_proceeds() {
        let Some((_dir, crosslink_dir)) = seed_v3_hub("agent-self", 60, Some("other-agent"), false)
        else {
            return;
        };

        std::fs::write(
            crosslink_dir.join("hook-config.json"),
            r#"{"auto_steal_stale_locks": 1}"#,
        )
        .unwrap();
        std::fs::create_dir_all(crosslink_dir.join(".hub-cache/locks/60.json")).unwrap();

        let db = temp_db();
        let result = enforce_lock(&crosslink_dir, 60, &db);
        assert!(
            result.is_ok(),
            "enforce_lock should proceed even when auto-steal errors: {result:?}"
        );
    }

    #[test]
    fn test_check_lock_no_crosslink_dir() {
        let result = check_lock(std::path::Path::new("/nonexistent-crosslink-dir-xyz"), 1);

        match result {
            Ok(LockStatus::NotConfigured) | Err(_) => {}
            Ok(other) => panic!("unexpected status: {other:?}"),
        }
    }

    #[test]
    fn test_read_auto_steal_config_bool_true() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("hook-config.json"),
            r#"{"auto_steal_stale_locks": true}"#,
        )
        .unwrap();
        assert_eq!(read_auto_steal_config(dir.path()), Some(1));
    }

    #[test]
    fn test_read_auto_steal_config_number() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("hook-config.json"),
            r#"{"auto_steal_stale_locks": 600}"#,
        )
        .unwrap();
        assert_eq!(read_auto_steal_config(dir.path()), Some(600));
    }

    #[test]
    fn test_read_auto_steal_config_string() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("hook-config.json"),
            r#"{"auto_steal_stale_locks": "900"}"#,
        )
        .unwrap();
        assert_eq!(read_auto_steal_config(dir.path()), Some(900));
    }

    #[test]
    fn test_read_auto_steal_config_missing() {
        let dir = tempdir().unwrap();

        assert_eq!(read_auto_steal_config(dir.path()), None);
    }

    #[test]
    fn test_read_auto_steal_config_false() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("hook-config.json"),
            r#"{"auto_steal_stale_locks": false}"#,
        )
        .unwrap();
        assert_eq!(read_auto_steal_config(dir.path()), None);
    }

    #[test]
    fn test_check_lock_on_frozen_v2_hub_is_available() {
        let dir = tempdir().unwrap();
        let repo_root = dir.path();

        let init_status = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(repo_root)
            .status();
        if init_status.map_or(true, |s| !s.success()) {
            return;
        }

        let crosslink_dir = repo_root.join(".crosslink");
        std::fs::create_dir_all(&crosslink_dir).unwrap();
        write_agent_config(&crosslink_dir, "agent-self");

        let hub_cache = crosslink_dir.join(".hub-cache");
        std::fs::create_dir_all(&hub_cache).unwrap();
        std::fs::write(hub_cache.join("locks.json"), b"not valid json!!!").unwrap();

        fn snapshot(path: &Path) -> Vec<(String, Vec<u8>)> {
            let mut entries = std::fs::read_dir(path)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect::<Vec<_>>();
            entries.sort();
            entries
                .into_iter()
                .filter(|path| path.is_file())
                .map(|path| {
                    (
                        path.file_name().unwrap().to_string_lossy().to_string(),
                        std::fs::read(path).unwrap(),
                    )
                })
                .collect()
        }

        let before = snapshot(&hub_cache);
        let status = check_lock(&crosslink_dir, 1).unwrap();
        assert_eq!(status, LockStatus::Available);
        assert_eq!(snapshot(&hub_cache), before);
    }
}
