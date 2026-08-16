use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use uuid::Uuid;

use crate::events::OrderingKey;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointState {
    pub next_display_id: i64,
    pub next_comment_id: i64,
    pub display_id_map: BTreeMap<Uuid, i64>,
    pub locks: BTreeMap<i64, LockEntry>,
    pub issues: BTreeMap<Uuid, CompactIssue>,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub milestones: BTreeMap<Uuid, CompactMilestone>,

    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub deleted_issues: BTreeSet<Uuid>,

    #[serde(default = "default_next_id")]
    pub next_milestone_id: i64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skew_warnings: Vec<SkewWarning>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compaction_lease: Option<CompactionLease>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unsigned_event_warnings: Vec<UnsignedEventWarning>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watermark: Option<OrderingKey>,
}

const fn default_next_id() -> i64 {
    1
}

impl Default for CheckpointState {
    fn default() -> Self {
        Self {
            next_display_id: 1,
            next_comment_id: 1,
            display_id_map: BTreeMap::new(),
            locks: BTreeMap::new(),
            issues: BTreeMap::new(),
            milestones: BTreeMap::new(),
            deleted_issues: BTreeSet::new(),
            next_milestone_id: 1,
            skew_warnings: Vec::new(),
            compaction_lease: None,
            unsigned_event_warnings: Vec::new(),
            watermark: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockEntry {
    pub agent_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    pub claimed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactIssue {
    pub uuid: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_id: Option<i64>,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub status: crate::models::IssueStatus,
    pub priority: crate::models::Priority,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_uuid: Option<Uuid>,
    pub created_by: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub closed_at: Option<DateTime<Utc>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheduled_at: Option<DateTime<Utc>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub due_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub labels: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub blockers: BTreeSet<Uuid>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub related: BTreeSet<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub milestone_uuid: Option<Uuid>,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub comments: BTreeMap<Uuid, CompactComment>,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub time_entries: BTreeMap<Uuid, CompactTimeEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompactComment {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_id: Option<i64>,
    pub author: String,
    pub content: String,
    pub created_at: DateTime<Utc>,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intervention_context: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver_key_fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signed_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompactTimeEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_id: Option<i64>,
    pub started_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_seconds: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompactMilestone {
    pub uuid: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_id: Option<i64>,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub status: crate::models::IssueStatus,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionLease {
    pub agent_id: String,
    pub acquired_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkewWarning {
    pub agent_id: String,
    pub skew_seconds: i64,
    pub event_timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnsignedEventWarning {
    pub agent_id: String,
    pub agent_seq: u64,
    pub timestamp: DateTime<Utc>,
}

impl CheckpointState {
    #[allow(dead_code)]
    pub fn from_slice(bytes: &[u8]) -> Result<Self> {
        serde_json::from_slice(bytes).context("Failed to parse checkpoint state from bytes")
    }
}

const CHECKPOINT_FILE: &str = "state.json";
const WATERMARK_FILE: &str = "watermark.json";

fn checkpoint_dir(cache_dir: &Path) -> std::path::PathBuf {
    cache_dir.join("checkpoint")
}

pub fn read_checkpoint(cache_dir: &Path) -> Result<CheckpointState> {
    let path = checkpoint_dir(cache_dir).join(CHECKPOINT_FILE);
    if !path.exists() {
        return Ok(CheckpointState::default());
    }
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("Failed to read checkpoint: {}", path.display()))?;
    serde_json::from_str(&content)
        .with_context(|| format!("Failed to parse checkpoint: {}", path.display()))
}

pub fn write_checkpoint(cache_dir: &Path, state: &CheckpointState) -> Result<()> {
    let dir = checkpoint_dir(cache_dir);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("Failed to create checkpoint dir: {}", dir.display()))?;
    let path = dir.join(CHECKPOINT_FILE);
    let content = serde_json::to_string_pretty(state)?;
    crate::utils::atomic_write(&path, content.as_bytes())
}

pub fn read_watermark(cache_dir: &Path) -> Result<Option<OrderingKey>> {
    let state = read_checkpoint(cache_dir)?;
    if state.watermark.is_some() {
        return Ok(state.watermark);
    }

    let path = checkpoint_dir(cache_dir).join(WATERMARK_FILE);
    if !path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("Failed to read watermark: {}", path.display()))?;
    let key: OrderingKey = serde_json::from_str(&content)
        .with_context(|| format!("Failed to parse watermark: {}", path.display()))?;
    Ok(Some(key))
}

#[cfg(test)]
pub(crate) fn write_watermark(cache_dir: &Path, key: &OrderingKey) -> Result<()> {
    let mut state = read_checkpoint(cache_dir)?;
    state.watermark = Some(key.clone());
    write_checkpoint(cache_dir, &state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_checkpoint_state() {
        let state = CheckpointState::default();
        assert_eq!(state.next_display_id, 1);
        assert_eq!(state.next_comment_id, 1);
        assert!(state.display_id_map.is_empty());
        assert!(state.locks.is_empty());
        assert!(state.issues.is_empty());
        assert!(state.skew_warnings.is_empty());
        assert!(state.compaction_lease.is_none());
        assert!(state.unsigned_event_warnings.is_empty());
    }

    #[test]
    fn test_checkpoint_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path();

        let mut state = CheckpointState {
            next_display_id: 42,
            next_comment_id: 10,
            ..Default::default()
        };

        let uuid = Uuid::new_v4();
        state.display_id_map.insert(uuid, 1);
        state.issues.insert(
            uuid,
            CompactIssue {
                uuid,
                display_id: Some(1),
                title: "Test".to_string(),
                description: None,
                status: crate::models::IssueStatus::Open,
                priority: crate::models::Priority::Medium,
                parent_uuid: None,
                created_by: "agent-1".to_string(),
                created_at: Utc::now(),
                updated_at: Utc::now(),
                closed_at: None,
                scheduled_at: None,
                due_at: None,
                labels: BTreeSet::from(["bug".to_string()]),
                blockers: BTreeSet::new(),
                related: BTreeSet::new(),
                milestone_uuid: None,
                comments: BTreeMap::new(),
                time_entries: BTreeMap::new(),
            },
        );
        state.locks.insert(
            1,
            LockEntry {
                agent_id: "agent-1".to_string(),
                branch: Some("feature/x".to_string()),
                claimed_at: Utc::now(),
            },
        );

        write_checkpoint(cache_dir, &state).unwrap();
        let loaded = read_checkpoint(cache_dir).unwrap();

        assert_eq!(loaded.next_display_id, 42);
        assert_eq!(loaded.next_comment_id, 10);
        assert_eq!(loaded.display_id_map.len(), 1);
        assert_eq!(loaded.issues.len(), 1);
        assert_eq!(loaded.locks.len(), 1);
        assert_eq!(loaded.issues[&uuid].title, "Test");
        assert!(loaded.issues[&uuid].labels.contains("bug"));
    }

    #[test]
    fn test_read_checkpoint_missing() {
        let dir = tempfile::tempdir().unwrap();
        let state = read_checkpoint(dir.path()).unwrap();
        assert_eq!(state.next_display_id, 1);
    }

    #[test]
    fn test_watermark_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path();

        let key = OrderingKey {
            timestamp: Utc::now(),
            agent_id: "agent-1".to_string(),
            agent_seq: 5,
        };

        write_watermark(cache_dir, &key).unwrap();
        let loaded = read_watermark(cache_dir).unwrap().unwrap();

        assert_eq!(loaded.agent_id, "agent-1");
        assert_eq!(loaded.agent_seq, 5);
    }

    #[test]
    fn test_read_watermark_missing() {
        let dir = tempfile::tempdir().unwrap();
        let result = read_watermark(dir.path()).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_compaction_lease_serialization() {
        let lease = CompactionLease {
            agent_id: "agent-1".to_string(),
            acquired_at: Utc::now(),
            expires_at: Utc::now() + chrono::Duration::seconds(30),
            pid: Some(12345),
        };
        let json = serde_json::to_string(&lease).unwrap();
        let parsed: CompactionLease = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.agent_id, "agent-1");
        assert_eq!(parsed.pid, Some(12345));
    }

    #[test]
    fn test_compaction_lease_backward_compat() {
        let json = r#"{"agent_id":"agent-1","acquired_at":"2025-01-01T00:00:00Z","expires_at":"2025-01-01T00:00:30Z"}"#;
        let parsed: CompactionLease = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.agent_id, "agent-1");
        assert_eq!(parsed.pid, None);
    }

    #[test]
    fn test_compact_issue_with_sets() {
        let issue = CompactIssue {
            uuid: Uuid::new_v4(),
            display_id: Some(1),
            title: "Test".to_string(),
            description: None,
            status: crate::models::IssueStatus::Open,
            priority: crate::models::Priority::High,
            parent_uuid: None,
            created_by: "agent-1".to_string(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            closed_at: None,
            scheduled_at: None,
            due_at: None,
            labels: BTreeSet::from(["a".to_string(), "b".to_string()]),
            blockers: BTreeSet::from([Uuid::new_v4()]),
            related: BTreeSet::new(),
            milestone_uuid: None,
            comments: BTreeMap::new(),
            time_entries: BTreeMap::new(),
        };
        let json = serde_json::to_string(&issue).unwrap();
        let parsed: CompactIssue = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.labels.len(), 2);
        assert_eq!(parsed.blockers.len(), 1);
    }

    #[test]
    fn test_read_watermark_legacy_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path();

        let state = CheckpointState::default();
        assert!(state.watermark.is_none());
        write_checkpoint(cache_dir, &state).unwrap();

        let checkpoint_dir = cache_dir.join("checkpoint");
        let legacy_key = OrderingKey {
            timestamp: Utc::now(),
            agent_id: "legacy-agent".to_string(),
            agent_seq: 99,
        };
        let watermark_path = checkpoint_dir.join("watermark.json");
        let content = serde_json::to_string_pretty(&legacy_key).unwrap();
        std::fs::write(&watermark_path, content).unwrap();

        let loaded = read_watermark(cache_dir).unwrap().unwrap();
        assert_eq!(loaded.agent_id, "legacy-agent");
        assert_eq!(loaded.agent_seq, 99);
    }

    #[test]
    fn test_read_watermark_embedded_takes_precedence_over_legacy() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path();

        let embedded_key = OrderingKey {
            timestamp: Utc::now(),
            agent_id: "embedded-agent".to_string(),
            agent_seq: 50,
        };
        write_watermark(cache_dir, &embedded_key).unwrap();

        let checkpoint_dir = cache_dir.join("checkpoint");
        let legacy_key = OrderingKey {
            timestamp: Utc::now(),
            agent_id: "legacy-agent".to_string(),
            agent_seq: 99,
        };
        let watermark_path = checkpoint_dir.join("watermark.json");
        let content = serde_json::to_string_pretty(&legacy_key).unwrap();
        std::fs::write(&watermark_path, content).unwrap();

        let loaded = read_watermark(cache_dir).unwrap().unwrap();
        assert_eq!(loaded.agent_id, "embedded-agent");
        assert_eq!(loaded.agent_seq, 50);
    }

    #[test]
    fn test_checkpoint_state_with_warnings() {
        let mut state = CheckpointState::default();
        state.skew_warnings.push(SkewWarning {
            agent_id: "agent-1".to_string(),
            skew_seconds: 120,
            event_timestamp: Utc::now(),
        });
        state.unsigned_event_warnings.push(UnsignedEventWarning {
            agent_id: "agent-2".to_string(),
            agent_seq: 3,
            timestamp: Utc::now(),
        });

        let json = serde_json::to_string_pretty(&state).unwrap();
        let parsed: CheckpointState = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.skew_warnings.len(), 1);
        assert_eq!(parsed.unsigned_event_warnings.len(), 1);
    }
}
