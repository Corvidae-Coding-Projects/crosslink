use crate::issue_file::{
    read_counters, read_issue_file, write_counters, write_issue_file, IssueFile,
};
use crate::models::{IssueStatus, Priority};
use crate::shared_writer::core::{PushOutcome, SharedWriter, LOCK_CONFIRM_TIMEOUT_SECS};
use crate::shared_writer::locks::LockClaimResult;
use anyhow::{bail, Result};
use chrono::Utc;
use std::path::Path;
use tempfile::tempdir;
use uuid::Uuid;

fn hub_lock_for_test(cache_dir: &Path) -> crate::sync::HubWriteLock {
    let lock_path = cache_dir.join(".hub-write-lock");
    crate::sync::acquire_hub_lock(&lock_path).expect("failed to acquire hub write lock for test")
}

fn make_issue(display_id: i64, title: &str) -> IssueFile {
    IssueFile {
        uuid: Uuid::new_v4(),
        display_id: Some(display_id),
        title: title.to_string(),
        description: None,
        status: IssueStatus::Open,
        priority: Priority::Medium,
        parent_uuid: None,
        created_by: "test-agent".to_string(),
        created_at: Utc::now(),
        updated_at: Utc::now(),
        closed_at: None,
        scheduled_at: None,
        due_at: None,
        labels: vec![],
        comments: vec![],
        blockers: vec![],
        related: vec![],
        milestone_uuid: None,
        time_entries: vec![],
    }
}

#[test]
fn test_new_returns_none_without_agent_config() {
    let dir = tempdir().unwrap();
    let crosslink_dir = dir.path().join(".crosslink");
    std::fs::create_dir_all(&crosslink_dir).unwrap();

    let writer = SharedWriter::new(&crosslink_dir).unwrap();
    assert!(writer.is_none());
}

#[test]
fn test_claim_display_id() {
    let dir = tempdir().unwrap();
    let meta_dir = dir.path().join("meta");
    std::fs::create_dir_all(&meta_dir).unwrap();

    let counters_path = meta_dir.join("counters.json");

    let counters = read_counters(&counters_path).unwrap();
    assert_eq!(counters.next_display_id, 1);

    let first = counters.next_display_id;
    let mut updated = counters;
    updated.next_display_id += 1;
    write_counters(&counters_path, &updated).unwrap();

    assert_eq!(first, 1);

    let counters = read_counters(&counters_path).unwrap();
    assert_eq!(counters.next_display_id, 2);
}

#[test]
fn test_load_issue_by_display_id() {
    let dir = tempdir().unwrap();
    let issues_dir = dir.path().join("issues");
    std::fs::create_dir_all(&issues_dir).unwrap();

    let issue1 = make_issue(1, "First");
    let issue2 = make_issue(2, "Second");
    write_issue_file(&issues_dir.join(format!("{}.json", issue1.uuid)), &issue1).unwrap();
    write_issue_file(&issues_dir.join(format!("{}.json", issue2.uuid)), &issue2).unwrap();

    let found = scan_for_display_id(&issues_dir, 2).unwrap();
    assert_eq!(found.title, "Second");
    assert_eq!(found.uuid, issue2.uuid);
}

#[test]
fn test_load_issue_by_display_id_not_found() {
    let dir = tempdir().unwrap();
    let issues_dir = dir.path().join("issues");
    std::fs::create_dir_all(&issues_dir).unwrap();

    let result = scan_for_display_id(&issues_dir, 99);
    assert!(result.is_err());
}

#[test]
fn test_resolve_uuid_from_files() {
    let dir = tempdir().unwrap();
    let issues_dir = dir.path().join("issues");
    std::fs::create_dir_all(&issues_dir).unwrap();

    let issue = make_issue(42, "Target");
    write_issue_file(&issues_dir.join(format!("{}.json", issue.uuid)), &issue).unwrap();

    let found = scan_for_display_id(&issues_dir, 42).unwrap();
    assert_eq!(found.uuid, issue.uuid);
}

#[test]
fn test_counters_sequential_claim() {
    let dir = tempdir().unwrap();
    let meta_dir = dir.path().join("meta");
    std::fs::create_dir_all(&meta_dir).unwrap();
    let path = meta_dir.join("counters.json");

    let mut counters = read_counters(&path).unwrap();
    let ids: Vec<i64> = (0..3)
        .map(|_| {
            let id = counters.next_display_id;
            counters.next_display_id += 1;
            id
        })
        .collect();

    write_counters(&path, &counters).unwrap();

    assert_eq!(ids, vec![1, 2, 3]);
    let reloaded = read_counters(&path).unwrap();
    assert_eq!(reloaded.next_display_id, 4);
}

fn scan_for_display_id(issues_dir: &Path, display_id: i64) -> Result<IssueFile> {
    for entry in std::fs::read_dir(issues_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if let Ok(issue) = read_issue_file(&path) {
            if issue.display_id == Some(display_id) {
                return Ok(issue);
            }
        }
    }
    bail!("Issue #{display_id} not found")
}

#[test]
fn test_v1_issue_path_format() {
    let uuid = Uuid::parse_str("a1b2c3d4-e5f6-7890-abcd-ef1234567890").unwrap();
    let path = format!("issues/{uuid}.json");
    assert_eq!(path, "issues/a1b2c3d4-e5f6-7890-abcd-ef1234567890.json");
}

#[test]
fn test_v2_issue_path_format() {
    let uuid = Uuid::parse_str("a1b2c3d4-e5f6-7890-abcd-ef1234567890").unwrap();
    let path = format!("issues/{uuid}/issue.json");
    assert_eq!(
        path,
        "issues/a1b2c3d4-e5f6-7890-abcd-ef1234567890/issue.json"
    );
}

#[test]
fn test_v2_comment_path_format() {
    let issue_uuid = Uuid::parse_str("a1b2c3d4-e5f6-7890-abcd-ef1234567890").unwrap();
    let comment_uuid = Uuid::parse_str("11111111-2222-3333-4444-555555555555").unwrap();
    let path = format!("issues/{issue_uuid}/comments/{comment_uuid}.json");
    assert_eq!(
        path,
        "issues/a1b2c3d4-e5f6-7890-abcd-ef1234567890/comments/11111111-2222-3333-4444-555555555555.json"
    );
}

#[test]
fn test_v2_scan_finds_issue_in_subdirectory() {
    let dir = tempdir().unwrap();
    let issues_dir = dir.path().join("issues");

    let issue = make_issue(7, "V2 Issue");
    let issue_subdir = issues_dir.join(issue.uuid.to_string());
    std::fs::create_dir_all(issue_subdir.join("comments")).unwrap();
    write_issue_file(&issue_subdir.join("issue.json"), &issue).unwrap();

    let mut found = false;
    for entry in std::fs::read_dir(&issues_dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.is_dir() {
            let issue_file = path.join("issue.json");
            if issue_file.exists() {
                if let Ok(loaded) = read_issue_file(&issue_file) {
                    if loaded.display_id == Some(7) {
                        assert_eq!(loaded.title, "V2 Issue");
                        found = true;
                    }
                }
            }
        }
    }
    assert!(found, "v2 issue not found in subdirectory scan");
}

#[test]
fn test_v2_comment_file_construction() {
    use crate::issue_file::CommentFile;

    let issue_uuid = Uuid::parse_str("a1b2c3d4-e5f6-7890-abcd-ef1234567890").unwrap();
    let comment_uuid = Uuid::new_v4();
    let comment = CommentFile {
        uuid: comment_uuid,
        issue_uuid,
        author: "test-agent".to_string(),
        content: "A standalone comment".to_string(),
        created_at: Utc::now(),
        kind: "note".to_string(),
        trigger_type: None,
        intervention_context: None,
        driver_key_fingerprint: None,
        signed_by: None,
        signature: None,
    };

    let json = serde_json::to_string_pretty(&comment).unwrap();
    let parsed: CommentFile = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.uuid, comment_uuid);
    assert_eq!(parsed.issue_uuid, issue_uuid);
    assert_eq!(parsed.content, "A standalone comment");
    assert_eq!(parsed.kind, "note");
}

#[test]
fn test_v2_intervention_comment_file_construction() {
    use crate::issue_file::CommentFile;

    let issue_uuid = Uuid::parse_str("a1b2c3d4-e5f6-7890-abcd-ef1234567890").unwrap();
    let comment_uuid = Uuid::new_v4();
    let comment = CommentFile {
        uuid: comment_uuid,
        issue_uuid,
        author: "test-agent".to_string(),
        content: "Driver intervention".to_string(),
        created_at: Utc::now(),
        kind: "intervention".to_string(),
        trigger_type: Some("redirect".to_string()),
        intervention_context: Some("User redirected task".to_string()),
        driver_key_fingerprint: Some("SHA256:abc123".to_string()),
        signed_by: None,
        signature: None,
    };

    let json = serde_json::to_string_pretty(&comment).unwrap();
    let parsed: CommentFile = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.kind, "intervention");
    assert_eq!(parsed.trigger_type, Some("redirect".to_string()));
    assert_eq!(
        parsed.intervention_context,
        Some("User redirected task".to_string())
    );
    assert_eq!(
        parsed.driver_key_fingerprint,
        Some("SHA256:abc123".to_string())
    );
}

#[test]
fn test_lock_confirm_timeout_constant() {
    assert_eq!(LOCK_CONFIRM_TIMEOUT_SECS, 30);
}

mod lock_v2_tests {
    use super::*;
    use crate::issue_file::LockFileV2;
    use tempfile::tempdir;

    #[test]
    fn test_lock_claim_result_variants() {
        let claimed = LockClaimResult::Claimed;
        let already = LockClaimResult::AlreadyHeld;
        let contended = LockClaimResult::Contended {
            winner_agent_id: "agent-2".to_string(),
        };
        assert_eq!(claimed, LockClaimResult::Claimed);
        assert_eq!(already, LockClaimResult::AlreadyHeld);
        assert_ne!(claimed, already);
        assert_ne!(claimed, contended);
        assert_eq!(
            contended,
            LockClaimResult::Contended {
                winner_agent_id: "agent-2".to_string(),
            }
        );

        let _ = format!("{claimed:?}");
        let _ = format!("{contended:?}");
    }

    #[test]
    fn test_read_lock_v2_file() {
        let dir = tempdir().unwrap();
        let locks_dir = dir.path().join("locks");
        std::fs::create_dir_all(&locks_dir).unwrap();

        let lock = LockFileV2 {
            issue_id: 42,
            agent_id: "agent-1".to_string(),
            branch: Some("feature/x".to_string()),
            claimed_at: chrono::Utc::now(),
            signed_by: Some("SHA256:abc".to_string()),
        };
        let json = serde_json::to_string_pretty(&lock).unwrap();
        std::fs::write(locks_dir.join("42.json"), &json).unwrap();

        let content = std::fs::read_to_string(locks_dir.join("42.json")).unwrap();
        let parsed: LockFileV2 = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed.issue_id, 42);
        assert_eq!(parsed.agent_id, "agent-1");
        assert_eq!(parsed.branch, Some("feature/x".to_string()));
    }

    #[test]
    fn test_read_lock_v2_missing() {
        let dir = tempdir().unwrap();
        let lock_path = dir.path().join("locks").join("99.json");
        assert!(!lock_path.exists());
    }

    #[test]
    fn test_lock_v2_file_roundtrip() {
        let dir = tempdir().unwrap();
        let locks_dir = dir.path().join("locks");
        std::fs::create_dir_all(&locks_dir).unwrap();

        let lock = LockFileV2 {
            issue_id: 5,
            agent_id: "worker-1".to_string(),
            branch: None,
            claimed_at: chrono::Utc::now(),
            signed_by: None,
        };
        let json = serde_json::to_string_pretty(&lock).unwrap();
        let path = locks_dir.join("5.json");
        std::fs::write(&path, &json).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let parsed: LockFileV2 = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed.issue_id, lock.issue_id);
        assert_eq!(parsed.agent_id, lock.agent_id);
        assert!(parsed.branch.is_none());
        assert!(parsed.signed_by.is_none());
    }

    #[test]
    fn test_lock_contention_deterministic_winner() {
        use crate::checkpoint::{read_checkpoint, write_checkpoint, CheckpointState};
        use crate::events::{append_event, Event, EventEnvelope};
        use chrono::Utc;

        let dir = tempdir().unwrap();
        let cache = dir.path();

        std::fs::create_dir_all(cache.join("checkpoint")).unwrap();
        std::fs::create_dir_all(cache.join("agents/agent-a")).unwrap();
        std::fs::create_dir_all(cache.join("agents/agent-b")).unwrap();
        std::fs::create_dir_all(cache.join("locks")).unwrap();
        std::fs::create_dir_all(cache.join("issues")).unwrap();

        let state = CheckpointState::default();
        write_checkpoint(cache, &state).unwrap();

        let now = Utc::now();

        let e1 = EventEnvelope {
            agent_id: "agent-a".to_string(),
            agent_seq: 1,
            timestamp: now - chrono::Duration::seconds(1),
            event: Event::LockClaimed {
                issue_display_id: 1,
                branch: None,
            },
            signed_by: None,
            signature: None,
        };
        append_event(&cache.join("agents/agent-a/events.log"), &e1).unwrap();

        let e2 = EventEnvelope {
            agent_id: "agent-b".to_string(),
            agent_seq: 1,
            timestamp: now,
            event: Event::LockClaimed {
                issue_display_id: 1,
                branch: None,
            },
            signed_by: None,
            signature: None,
        };
        append_event(&cache.join("agents/agent-b/events.log"), &e2).unwrap();

        let lock = hub_lock_for_test(cache);
        let result = crate::compaction::compact(cache, "agent-a", true, &lock)
            .unwrap()
            .unwrap();
        assert_eq!(result.locks_materialized, 1);

        let state = read_checkpoint(cache).unwrap();
        let lock_entry = state.locks.get(&1).unwrap();
        assert_eq!(lock_entry.agent_id, "agent-a");
    }

    #[test]
    fn test_prune_then_checkpoint_clear() {
        use crate::checkpoint::{write_checkpoint, CheckpointState, LockEntry};
        use crate::events::{append_event, Event, EventEnvelope, OrderingKey};
        use chrono::Utc;

        let dir = tempdir().unwrap();
        let cache = dir.path();

        std::fs::create_dir_all(cache.join("checkpoint")).unwrap();
        std::fs::create_dir_all(cache.join("agents/stale-agent")).unwrap();
        std::fs::create_dir_all(cache.join("locks")).unwrap();
        std::fs::create_dir_all(cache.join("issues")).unwrap();

        let now = Utc::now();

        let e = EventEnvelope {
            agent_id: "stale-agent".to_string(),
            agent_seq: 1,
            timestamp: now,
            event: Event::LockClaimed {
                issue_display_id: 5,
                branch: None,
            },
            signed_by: None,
            signature: None,
        };
        append_event(&cache.join("agents/stale-agent/events.log"), &e).unwrap();

        let watermark = OrderingKey {
            timestamp: now + chrono::Duration::seconds(1),
            agent_id: "stale-agent".to_string(),
            agent_seq: 1,
        };

        let mut state = CheckpointState::default();
        state.activate_legacy(Some(watermark));
        state.locks.insert(
            5,
            LockEntry {
                agent_id: "stale-agent".to_string(),
                branch: None,
                claimed_at: now,
            },
        );
        write_checkpoint(cache, &state).unwrap();

        let lock = crate::issue_file::LockFileV2 {
            issue_id: 5,
            agent_id: "stale-agent".to_string(),
            branch: None,
            claimed_at: now,
            signed_by: None,
        };
        std::fs::write(
            cache.join("locks/5.json"),
            serde_json::to_string_pretty(&lock).unwrap(),
        )
        .unwrap();

        let pruned = crate::compaction::prune_events(cache, "stale-agent").unwrap();
        assert!(pruned > 0);

        let mut state = crate::checkpoint::read_checkpoint(cache).unwrap();
        state.locks.remove(&5);
        write_checkpoint(cache, &state).unwrap();

        let lock_path = cache.join("locks/5.json");
        if lock_path.exists() {
            std::fs::remove_file(&lock_path).unwrap();
        }

        let state = crate::checkpoint::read_checkpoint(cache).unwrap();
        assert!(state.locks.is_empty());
        assert!(!cache.join("locks/5.json").exists());
    }

    #[test]
    fn test_lock_file_v2_with_all_fields() {
        let dir = tempdir().unwrap();
        let locks_dir = dir.path().join("locks");
        std::fs::create_dir_all(&locks_dir).unwrap();

        let now = chrono::Utc::now();
        let lock = LockFileV2 {
            issue_id: 100,
            agent_id: "agent-special".to_string(),
            branch: Some("feature/special-branch".to_string()),
            claimed_at: now,
            signed_by: Some("SHA256:xyz789".to_string()),
        };
        let json = serde_json::to_string_pretty(&lock).unwrap();
        let path = locks_dir.join("100.json");
        std::fs::write(&path, &json).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let parsed: LockFileV2 = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed.issue_id, 100);
        assert_eq!(parsed.agent_id, "agent-special");
        assert_eq!(parsed.branch, Some("feature/special-branch".to_string()));
        assert_eq!(parsed.claimed_at, now);
        assert_eq!(parsed.signed_by, Some("SHA256:xyz789".to_string()));
    }

    #[test]
    fn test_lock_claim_result_display_and_equality() {
        let c1 = LockClaimResult::Contended {
            winner_agent_id: "agent-1".to_string(),
        };
        let c2 = LockClaimResult::Contended {
            winner_agent_id: "agent-2".to_string(),
        };
        assert_ne!(c1, c2);

        let c3 = LockClaimResult::Contended {
            winner_agent_id: "agent-1".to_string(),
        };
        assert_eq!(c1, c3);

        let cloned = c1.clone();
        assert_eq!(c1, cloned);
    }

    #[test]
    fn test_lock_contention_with_three_agents() {
        use crate::checkpoint::{read_checkpoint, write_checkpoint, CheckpointState};
        use crate::events::{append_event, Event, EventEnvelope};
        use chrono::Utc;

        let dir = tempdir().unwrap();
        let cache = dir.path();

        std::fs::create_dir_all(cache.join("checkpoint")).unwrap();
        std::fs::create_dir_all(cache.join("agents/agent-a")).unwrap();
        std::fs::create_dir_all(cache.join("agents/agent-b")).unwrap();
        std::fs::create_dir_all(cache.join("agents/agent-c")).unwrap();
        std::fs::create_dir_all(cache.join("locks")).unwrap();
        std::fs::create_dir_all(cache.join("issues")).unwrap();

        let state = CheckpointState::default();
        write_checkpoint(cache, &state).unwrap();

        let now = Utc::now();

        let e1 = EventEnvelope {
            agent_id: "agent-c".to_string(),
            agent_seq: 1,
            timestamp: now - chrono::Duration::seconds(3),
            event: Event::LockClaimed {
                issue_display_id: 1,
                branch: Some("feature/c".to_string()),
            },
            signed_by: None,
            signature: None,
        };
        append_event(&cache.join("agents/agent-c/events.log"), &e1).unwrap();

        let e2 = EventEnvelope {
            agent_id: "agent-a".to_string(),
            agent_seq: 1,
            timestamp: now - chrono::Duration::seconds(2),
            event: Event::LockClaimed {
                issue_display_id: 1,
                branch: Some("feature/a".to_string()),
            },
            signed_by: None,
            signature: None,
        };
        append_event(&cache.join("agents/agent-a/events.log"), &e2).unwrap();

        let e3 = EventEnvelope {
            agent_id: "agent-b".to_string(),
            agent_seq: 1,
            timestamp: now - chrono::Duration::seconds(1),
            event: Event::LockClaimed {
                issue_display_id: 1,
                branch: Some("feature/b".to_string()),
            },
            signed_by: None,
            signature: None,
        };
        append_event(&cache.join("agents/agent-b/events.log"), &e3).unwrap();

        let hub_lock = hub_lock_for_test(cache);
        let result = crate::compaction::compact(cache, "agent-a", true, &hub_lock)
            .unwrap()
            .unwrap();
        assert_eq!(result.locks_materialized, 1);

        let state = read_checkpoint(cache).unwrap();
        let lock = state.locks.get(&1).unwrap();
        assert_eq!(lock.agent_id, "agent-c");
        assert_eq!(lock.branch, Some("feature/c".to_string()));
    }

    #[test]
    fn test_lock_contention_then_winner_releases() {
        use crate::checkpoint::{read_checkpoint, write_checkpoint, CheckpointState};
        use crate::events::{append_event, Event, EventEnvelope};
        use chrono::Utc;

        let dir = tempdir().unwrap();
        let cache = dir.path();

        std::fs::create_dir_all(cache.join("checkpoint")).unwrap();
        std::fs::create_dir_all(cache.join("agents/agent-a")).unwrap();
        std::fs::create_dir_all(cache.join("agents/agent-b")).unwrap();
        std::fs::create_dir_all(cache.join("locks")).unwrap();
        std::fs::create_dir_all(cache.join("issues")).unwrap();

        let state = CheckpointState::default();
        write_checkpoint(cache, &state).unwrap();

        let now = Utc::now();

        let e1 = EventEnvelope {
            agent_id: "agent-a".to_string(),
            agent_seq: 1,
            timestamp: now - chrono::Duration::seconds(3),
            event: Event::LockClaimed {
                issue_display_id: 1,
                branch: None,
            },
            signed_by: None,
            signature: None,
        };
        append_event(&cache.join("agents/agent-a/events.log"), &e1).unwrap();

        let e2 = EventEnvelope {
            agent_id: "agent-b".to_string(),
            agent_seq: 1,
            timestamp: now - chrono::Duration::seconds(2),
            event: Event::LockClaimed {
                issue_display_id: 1,
                branch: None,
            },
            signed_by: None,
            signature: None,
        };
        append_event(&cache.join("agents/agent-b/events.log"), &e2).unwrap();

        let e3 = EventEnvelope {
            agent_id: "agent-a".to_string(),
            agent_seq: 2,
            timestamp: now - chrono::Duration::seconds(1),
            event: Event::LockReleased {
                issue_display_id: 1,
            },
            signed_by: None,
            signature: None,
        };
        append_event(&cache.join("agents/agent-a/events.log"), &e3).unwrap();

        let hub_lock = hub_lock_for_test(cache);
        crate::compaction::compact(cache, "agent-a", true, &hub_lock).unwrap();

        let state = read_checkpoint(cache).unwrap();
        assert!(state.locks.is_empty());
        assert!(!cache.join("locks/1.json").exists());
    }

    #[test]
    fn test_lock_file_v2_missing_optional_fields() {
        let json = r#"{
            "issue_id": 7,
            "agent_id": "agent-minimal",
            "branch": null,
            "claimed_at": "2026-01-01T00:00:00Z",
            "signed_by": null
        }"#;
        let parsed: LockFileV2 = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.issue_id, 7);
        assert_eq!(parsed.agent_id, "agent-minimal");
        assert!(parsed.branch.is_none());
        assert!(parsed.signed_by.is_none());
    }

    #[test]
    fn test_lock_contention_deterministic_across_compaction_agents() {
        use crate::checkpoint::{read_checkpoint, write_checkpoint, CheckpointState};
        use crate::events::{append_event, Event, EventEnvelope};
        use chrono::Utc;

        let now = Utc::now();

        for compactor in &["agent-a", "agent-b"] {
            let dir = tempdir().unwrap();
            let cache = dir.path();

            std::fs::create_dir_all(cache.join("checkpoint")).unwrap();
            std::fs::create_dir_all(cache.join("agents/agent-a")).unwrap();
            std::fs::create_dir_all(cache.join("agents/agent-b")).unwrap();
            std::fs::create_dir_all(cache.join("locks")).unwrap();
            std::fs::create_dir_all(cache.join("issues")).unwrap();

            let state = CheckpointState::default();
            write_checkpoint(cache, &state).unwrap();

            let e1 = EventEnvelope {
                agent_id: "agent-a".to_string(),
                agent_seq: 1,
                timestamp: now - chrono::Duration::seconds(2),
                event: Event::LockClaimed {
                    issue_display_id: 1,
                    branch: None,
                },
                signed_by: None,
                signature: None,
            };
            append_event(&cache.join("agents/agent-a/events.log"), &e1).unwrap();

            let e2 = EventEnvelope {
                agent_id: "agent-b".to_string(),
                agent_seq: 1,
                timestamp: now - chrono::Duration::seconds(1),
                event: Event::LockClaimed {
                    issue_display_id: 1,
                    branch: None,
                },
                signed_by: None,
                signature: None,
            };
            append_event(&cache.join("agents/agent-b/events.log"), &e2).unwrap();

            let hub_lock = hub_lock_for_test(cache);
            crate::compaction::compact(cache, compactor, true, &hub_lock).unwrap();

            let state = read_checkpoint(cache).unwrap();
            assert_eq!(
                state.locks[&1].agent_id, "agent-a",
                "Winner should be agent-a regardless of who runs compaction (compactor={compactor})"
            );
        }
    }
}

mod integration {
    use super::*;
    use crate::db::Database;
    use crate::identity::{AgentConfig, AgentRole};
    use std::process::Command;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};
    use tempfile::TempDir;

    fn setup_shared_writer_env() -> (TempDir, TempDir, std::path::PathBuf) {
        let remote_dir = tempfile::tempdir().unwrap();
        let work_dir = tempfile::tempdir().unwrap();

        Command::new("git")
            .current_dir(remote_dir.path())
            .args(["init", "--bare", "-b", "main"])
            .output()
            .unwrap();

        Command::new("git")
            .current_dir(work_dir.path())
            .args(["init", "-b", "main"])
            .output()
            .unwrap();

        for args in [
            vec!["config", "user.email", "test@test.local"],
            vec!["config", "user.name", "Test"],
            vec![
                "remote",
                "add",
                "origin",
                remote_dir.path().to_str().unwrap(),
            ],
        ] {
            Command::new("git")
                .current_dir(work_dir.path())
                .args(&args)
                .output()
                .unwrap();
        }

        std::fs::write(work_dir.path().join("README.md"), "# test\n").unwrap();
        Command::new("git")
            .current_dir(work_dir.path())
            .args(["add", "."])
            .output()
            .unwrap();
        Command::new("git")
            .current_dir(work_dir.path())
            .args(["commit", "-m", "init", "--no-gpg-sign"])
            .output()
            .unwrap();
        Command::new("git")
            .current_dir(work_dir.path())
            .args(["push", "-u", "origin", "main"])
            .output()
            .unwrap();

        let crosslink_dir = work_dir.path().join(".crosslink");
        std::fs::create_dir_all(&crosslink_dir).unwrap();
        std::fs::write(
            crosslink_dir.join("hook-config.json"),
            r#"{"remote":"origin","layout":"v2"}"#,
        )
        .unwrap();

        let agent_config = AgentConfig {
            agent_id: "test-agent".to_string(),
            machine_id: "test-machine".to_string(),
            description: Some("Integration test agent".to_string()),
            role: AgentRole::Driver,
            ssh_key_path: None,
            ssh_fingerprint: None,
            ssh_public_key: None,
        };
        let agent_json = serde_json::to_string_pretty(&agent_config).unwrap();
        std::fs::write(crosslink_dir.join("agent.json"), agent_json).unwrap();

        let sync = crate::sync::SyncManager::new(&crosslink_dir).unwrap();
        sync.init_cache().unwrap();
        drop(sync);
        let projection = Database::open(&crosslink_dir.join("issues.db")).unwrap();
        drop(projection);
        let activation = crate::reconcile::migration::activate_repository(&crosslink_dir).unwrap();
        let (state, generation_id) = match activation {
            crate::reconcile::migration::RepositoryActivation::ReadyCurrent { generation_id } => (
                crate::reconcile::readiness::ReadinessState::ReadyCurrent,
                generation_id,
            ),
            crate::reconcile::migration::RepositoryActivation::ReadyMigrated { generation_id } => (
                crate::reconcile::readiness::ReadinessState::ReadyMigrated,
                generation_id,
            ),
            crate::reconcile::migration::RepositoryActivation::ReadyAdopted { generation_id } => (
                crate::reconcile::readiness::ReadinessState::ReadyAdopted,
                generation_id,
            ),
            other => panic!("unexpected activation: {other:?}"),
        };
        let identity = crate::reconcile::readiness::DaemonIdentity {
            schema_version: crate::reconcile::readiness::READINESS_SCHEMA_VERSION,
            repository_id: crate::reconcile::readiness::repository_id(&crosslink_dir).unwrap(),
            daemon_epoch: Uuid::new_v4().to_string(),
            pid: std::process::id(),
            process_start: crate::reconcile::readiness::current_process_start_token().unwrap(),
        };
        crate::reconcile::readiness::write_daemon_identity(&crosslink_dir, &identity).unwrap();
        crate::reconcile::readiness::write_record(
            &crosslink_dir,
            crate::reconcile::readiness::ReadinessDraft {
                daemon_epoch: &identity.daemon_epoch,
                daemon_pid: identity.pid,
                attempt_id: "shared-writer-test",
                state,
                generation_id: Some(&generation_id),
                reason: None,
            },
        )
        .unwrap();

        (work_dir, remote_dir, crosslink_dir)
    }

    fn setup_shared_writer_env_v2() -> (TempDir, TempDir, std::path::PathBuf) {
        let (work_dir, remote_dir, crosslink_dir) = setup_shared_writer_env();

        let cache_dir = crosslink_dir.join(".hub-cache");
        let _ = Command::new("git")
            .current_dir(work_dir.path())
            .args(["worktree", "remove", "--force", cache_dir.to_str().unwrap()])
            .output();

        for r in [
            "refs/heads/crosslink/meta",
            "refs/heads/crosslink/checkpoint",
            "refs/heads/crosslink/agents/test-agent",
        ] {
            let _ = Command::new("git")
                .current_dir(work_dir.path())
                .args(["update-ref", "-d", r])
                .output();
        }

        let _ = Command::new("git")
            .current_dir(work_dir.path())
            .args(["branch", "-D", "crosslink/hub-v3-host"])
            .output();

        Command::new("git")
            .current_dir(work_dir.path())
            .args([
                "worktree",
                "add",
                "--orphan",
                "-b",
                "crosslink/hub",
                cache_dir.to_str().unwrap(),
            ])
            .output()
            .unwrap();

        let meta_dir = cache_dir.join("meta");
        std::fs::create_dir_all(meta_dir.join("milestones")).unwrap();
        std::fs::create_dir_all(cache_dir.join("issues")).unwrap();
        std::fs::create_dir_all(cache_dir.join("locks")).unwrap();
        crate::issue_file::write_layout_version(
            &meta_dir,
            crate::issue_file::CURRENT_LAYOUT_VERSION,
        )
        .unwrap();
        std::fs::write(
            cache_dir.join("locks.json"),
            serde_json::to_string(&serde_json::json!({"version":1,"locks":{},"settings":{"stale_lock_timeout_minutes":60}})).unwrap(),
        )
        .unwrap();
        for args in [
            vec!["config", "user.email", "test@test.local"],
            vec!["config", "user.name", "Test"],
        ] {
            Command::new("git")
                .current_dir(&cache_dir)
                .args(&args)
                .output()
                .unwrap();
        }
        Command::new("git")
            .current_dir(&cache_dir)
            .args(["add", "-A"])
            .output()
            .unwrap();
        Command::new("git")
            .current_dir(&cache_dir)
            .args(["commit", "-m", "v2 hub", "--no-gpg-sign"])
            .output()
            .unwrap();

        (work_dir, remote_dir, crosslink_dir)
    }

    fn make_db(dir: &std::path::Path) -> Database {
        Database::open(&dir.join("issues.db")).unwrap()
    }

    #[test]
    fn test_new_returns_some_with_agent_and_hub() {
        let (work_dir, _remote, crosslink_dir) = setup_shared_writer_env();
        let writer = SharedWriter::new(&crosslink_dir).unwrap();
        assert!(
            writer.is_some(),
            "SharedWriter::new() should return Some when agent.json and hub branch exist"
        );
        drop(work_dir);
    }

    #[test]
    fn test_new_agent_id_matches_config() {
        let (work_dir, _remote, crosslink_dir) = setup_shared_writer_env();
        let writer = SharedWriter::new(&crosslink_dir).unwrap().unwrap();
        assert_eq!(writer.agent_id(), "test-agent");
        drop(work_dir);
    }

    #[test]
    fn test_new_rejects_waiting_repository_before_cache_mutation() {
        let (work_dir, _remote, crosslink_dir) = setup_shared_writer_env();
        let identity = crate::reconcile::readiness::read_daemon_identity(&crosslink_dir)
            .unwrap()
            .unwrap();
        crate::reconcile::readiness::write_record(
            &crosslink_dir,
            crate::reconcile::readiness::ReadinessDraft {
                daemon_epoch: &identity.daemon_epoch,
                daemon_pid: identity.pid,
                attempt_id: "offline",
                state: crate::reconcile::readiness::ReadinessState::WaitingForRemote,
                generation_id: None,
                reason: Some("offline"),
            },
        )
        .unwrap();
        let cache = crosslink_dir.join(".hub-cache");
        let refs_before = Command::new("git")
            .current_dir(&cache)
            .args(["for-each-ref", "--format=%(refname) %(objectname)"])
            .output()
            .unwrap()
            .stdout;
        let error = SharedWriter::new(&crosslink_dir).err().unwrap();
        assert!(error.to_string().contains("waiting_for_remote"));
        let refs_after = Command::new("git")
            .current_dir(&cache)
            .args(["for-each-ref", "--format=%(refname) %(objectname)"])
            .output()
            .unwrap()
            .stdout;
        assert_eq!(refs_after, refs_before);
        drop(work_dir);
    }

    #[test]
    fn test_new_holds_operation_authority_until_construction_finishes() {
        let (work_dir, _remote, crosslink_dir) = setup_shared_writer_env();
        let outer =
            crate::reconcile::readiness::acquire_mutation_operation_permit(&crosslink_dir).unwrap();
        let transition_dir = crosslink_dir.clone();
        let (acquired_tx, acquired_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let transition = std::thread::spawn(move || {
            let permit =
                crate::reconcile::readiness::acquire_transition_permit(&transition_dir).unwrap();
            acquired_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            drop(permit);
        });
        let transition_path = crosslink_dir.join("readiness/transition.lock");
        let deadline = Instant::now() + Duration::from_secs(2);
        while !transition_path.exists() {
            assert!(
                Instant::now() < deadline,
                "transition did not publish its barrier"
            );
            std::thread::yield_now();
        }
        assert!(SharedWriter::new(&crosslink_dir).unwrap().is_some());
        assert!(acquired_rx.try_recv().is_err());
        drop(outer);
        acquired_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        release_tx.send(()).unwrap();
        transition.join().unwrap();
        drop(work_dir);
    }

    #[test]
    fn test_new_creates_issues_and_meta_dirs() {
        let (work_dir, _remote, crosslink_dir) = setup_shared_writer_env();
        SharedWriter::new(&crosslink_dir).unwrap().unwrap();
        let cache_dir = crosslink_dir.join(".hub-cache");
        assert!(
            cache_dir.join("issues").exists(),
            "issues/ dir should exist"
        );
        assert!(
            cache_dir.join("meta").join("milestones").exists(),
            "meta/milestones/ dir should exist"
        );
        drop(work_dir);
    }

    #[test]
    fn test_read_lock_v2_returns_none_when_no_lock() {
        let (work_dir, _remote, crosslink_dir) = setup_shared_writer_env();
        let writer = SharedWriter::new(&crosslink_dir).unwrap().unwrap();

        let result = writer.read_lock_v2(999).unwrap();
        assert!(
            result.is_none(),
            "No lock should exist for non-existent issue"
        );
        drop(work_dir);
    }

    #[test]
    fn test_read_lock_v2_reads_existing_lock_file() {
        let (work_dir, _remote, crosslink_dir) = setup_shared_writer_env_v2();
        let error = SharedWriter::new(&crosslink_dir).err().unwrap();
        assert!(error.to_string().contains("readiness"));
        drop(work_dir);
    }

    #[test]
    fn test_crosslink_dir_accessor() {
        let (work_dir, _remote, crosslink_dir) = setup_shared_writer_env();
        let writer = SharedWriter::new(&crosslink_dir).unwrap().unwrap();

        let dir = writer.crosslink_dir();

        assert!(
            dir.exists(),
            "crosslink_dir() should point to an existing dir"
        );
        drop(work_dir);
    }

    #[test]
    fn test_resolve_ssh_key_path_returns_none_without_key() {
        let (work_dir, _remote, crosslink_dir) = setup_shared_writer_env();
        let writer = SharedWriter::new(&crosslink_dir).unwrap().unwrap();

        let key_path = writer.resolve_ssh_key_path();
        assert!(
            key_path.is_none(),
            "resolve_ssh_key_path should return None when no key is configured"
        );
        drop(work_dir);
    }

    #[test]
    fn test_load_issue_by_display_id_not_found() {
        let (work_dir, _remote, crosslink_dir) = setup_shared_writer_env();
        let writer = SharedWriter::new(&crosslink_dir).unwrap().unwrap();

        let result = writer.load_issue_by_display_id(9999);
        assert!(result.is_err(), "Non-existent issue should return error");
        drop(work_dir);
    }

    #[test]
    fn test_sign_comment_without_key_returns_none() {
        let (work_dir, _remote, crosslink_dir) = setup_shared_writer_env();
        let writer = SharedWriter::new(&crosslink_dir).unwrap().unwrap();

        let (signed_by, signature) = writer.sign_comment("content", "author", 1);
        assert!(signed_by.is_none());
        assert!(signature.is_none());
        drop(work_dir);
    }

    #[test]
    fn test_create_envelope_without_signing() {
        let (work_dir, _remote, crosslink_dir) = setup_shared_writer_env();
        let writer = SharedWriter::new(&crosslink_dir).unwrap().unwrap();

        let event = crate::events::Event::IssueCreated {
            uuid: Uuid::new_v4(),
            title: "test".to_string(),
            description: None,
            priority: "low".to_string(),
            labels: vec![],
            parent_uuid: None,
            created_by: "test-agent".to_string(),
            display_id: None,
            scheduled_at: None,
            due_at: None,
        };
        let envelope = writer.create_envelope(event);
        assert_eq!(envelope.agent_id, "test-agent");
        assert!(envelope.signature.is_none(), "No signature without key");
        assert!(envelope.signed_by.is_none(), "No signed_by without key");
        assert_eq!(envelope.agent_seq, 1, "First event should have seq 1");
        drop(work_dir);
    }

    #[test]
    fn test_next_event_seq_increments() {
        let (work_dir, _remote, crosslink_dir) = setup_shared_writer_env();
        let writer = SharedWriter::new(&crosslink_dir).unwrap().unwrap();

        let s1 = writer.next_event_seq();
        let s2 = writer.next_event_seq();
        let s3 = writer.next_event_seq();

        assert_eq!(s1 + 1, s2);
        assert_eq!(s2 + 1, s3);
        drop(work_dir);
    }

    #[test]
    fn test_read_max_event_seq_returns_zero_when_no_log() {
        let dir = tempfile::tempdir().unwrap();
        let seq = SharedWriter::read_max_event_seq(
            dir.path(),
            "nonexistent-agent",
            crate::hub_v3::HubMode::V2,
        );
        assert_eq!(seq, 0, "Max event seq should be 0 when no log exists");
    }

    #[test]
    fn test_layout_version_one_for_v1_hub() {
        let dir = tempfile::tempdir().unwrap();
        let meta_dir = dir.path().join("meta");
        std::fs::create_dir_all(&meta_dir).unwrap();

        let version = crate::issue_file::read_layout_version(&meta_dir).unwrap_or(1);
        assert_eq!(version, 1);
    }

    #[test]
    fn test_push_outcome_eq() {
        assert_eq!(PushOutcome::Pushed, PushOutcome::Pushed);
        assert_eq!(PushOutcome::LocalOnly, PushOutcome::LocalOnly);
        assert_ne!(PushOutcome::Pushed, PushOutcome::LocalOnly);
    }

    #[test]
    fn test_push_outcome_copy() {
        let o = PushOutcome::Pushed;
        let o2 = o;
        assert_eq!(o, o2);
    }

    #[test]
    fn test_new_without_agent_config_but_hub_already_initialized() {
        let (work_dir, _remote, crosslink_dir) = setup_shared_writer_env();

        std::fs::remove_file(crosslink_dir.join("agent.json")).unwrap();

        let writer = SharedWriter::new(&crosslink_dir).unwrap();
        assert!(
            writer.is_some(),
            "SharedWriter::new() should return Some when hub cache already exists (anonymous mode)"
        );

        let writer = writer.unwrap();

        assert!(
            writer.agent_id().starts_with("anon-"),
            "Anonymous writer should have agent_id starting with 'anon-', got: {}",
            writer.agent_id()
        );

        drop(work_dir);
    }

    #[test]
    fn initialized_cache_without_agent_or_daemon_stays_fail_closed() {
        let (work_dir, _remote, crosslink_dir) = setup_shared_writer_env();
        std::fs::remove_file(crosslink_dir.join("agent.json")).unwrap();
        std::fs::remove_file(crosslink_dir.join("daemon.pid")).unwrap();
        std::fs::remove_dir_all(crosslink_dir.join("readiness")).unwrap();
        let error = match SharedWriter::new(&crosslink_dir) {
            Err(error) => error.to_string(),
            Ok(_) => panic!("initialized repository unexpectedly bypassed readiness"),
        };
        assert!(error.contains("readiness is missing"));
        drop(work_dir);
    }

    #[test]
    fn initialized_hook_only_repository_stays_fail_closed_before_cache_bootstrap() {
        let work_dir = tempfile::tempdir().unwrap();

        Command::new("git")
            .current_dir(work_dir.path())
            .args(["init", "-b", "main"])
            .output()
            .unwrap();

        for args in [
            vec!["config", "user.email", "test@test.local"],
            vec!["config", "user.name", "Test"],
            vec!["remote", "add", "origin", "/nonexistent/path/to/remote"],
        ] {
            Command::new("git")
                .current_dir(work_dir.path())
                .args(&args)
                .output()
                .unwrap();
        }

        let crosslink_dir = work_dir.path().join(".crosslink");
        std::fs::create_dir_all(&crosslink_dir).unwrap();
        std::fs::write(
            crosslink_dir.join("hook-config.json"),
            r#"{"remote":"origin","layout":"v2"}"#,
        )
        .unwrap();

        let result = SharedWriter::new(&crosslink_dir);
        let error = result
            .err()
            .expect("initialized repository must require readiness");
        assert!(error.to_string().contains("readiness is missing"));
        assert!(!crosslink_dir.join(".hub-cache").exists());
        assert!(!crosslink_dir.join("issues.db").exists());

        drop(work_dir);
    }

    #[test]
    fn test_resolve_ssh_key_path_nonexistent_file() {
        let (work_dir, _remote, crosslink_dir) = setup_shared_writer_env();

        let agent_config = AgentConfig {
            agent_id: "test-agent".to_string(),
            machine_id: "test-machine".to_string(),
            description: None,
            role: AgentRole::Driver,
            ssh_key_path: Some("nonexistent_key_file.pem".to_string()),
            ssh_fingerprint: Some("SHA256:fakefingerprint".to_string()),
            ssh_public_key: None,
        };
        let agent_json = serde_json::to_string_pretty(&agent_config).unwrap();
        std::fs::write(crosslink_dir.join("agent.json"), agent_json).unwrap();

        let writer = SharedWriter::new(&crosslink_dir).unwrap().unwrap();

        let resolved = writer.resolve_ssh_key_path();
        assert!(
            resolved.is_none(),
            "resolve_ssh_key_path should return None when file doesn't exist"
        );

        drop(work_dir);
    }

    #[test]
    fn test_resolve_ssh_key_path_existing_file() {
        let (work_dir, _remote, crosslink_dir) = setup_shared_writer_env();

        let fake_key_name = "test_agent_key.pem";
        let fake_key_path = crosslink_dir.join(fake_key_name);
        std::fs::write(&fake_key_path, "fake key content").unwrap();

        let agent_config = AgentConfig {
            agent_id: "test-agent".to_string(),
            machine_id: "test-machine".to_string(),
            description: None,
            role: AgentRole::Driver,
            ssh_key_path: Some(fake_key_name.to_string()),
            ssh_fingerprint: Some("SHA256:fakefingerprint".to_string()),
            ssh_public_key: None,
        };
        let agent_json = serde_json::to_string_pretty(&agent_config).unwrap();
        std::fs::write(crosslink_dir.join("agent.json"), agent_json).unwrap();

        let writer = SharedWriter::new(&crosslink_dir).unwrap().unwrap();

        let resolved = writer.resolve_ssh_key_path();
        assert!(
            resolved.is_some(),
            "resolve_ssh_key_path should return Some when key file exists"
        );
        assert!(
            resolved.unwrap().ends_with(fake_key_name),
            "Resolved path should end with the key filename"
        );

        drop(work_dir);
    }

    #[test]
    fn test_new_without_agent_json_and_no_hub() {
        let dir = tempfile::tempdir().unwrap();
        let crosslink_dir = dir.path().join(".crosslink");
        std::fs::create_dir_all(&crosslink_dir).unwrap();
        std::fs::write(
            crosslink_dir.join("hook-config.json"),
            r#"{"remote":"origin"}"#,
        )
        .unwrap();

        let result = SharedWriter::new(&crosslink_dir).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_v2_create_issue_refuses_with_migrate_message() {
        let (work_dir, _remote, crosslink_dir) = setup_shared_writer_env_v2();
        let error = SharedWriter::new(&crosslink_dir).err().unwrap();
        assert!(error.to_string().contains("readiness"));
        drop(work_dir);
    }

    #[test]
    fn test_v2_add_label_refuses() {
        let (work_dir, _remote, crosslink_dir) = setup_shared_writer_env();
        let writer = SharedWriter::new(&crosslink_dir).unwrap().unwrap();
        let db = make_db(work_dir.path());

        let result = writer.add_label(&db, 1, "bug");
        assert!(
            result.is_err(),
            "add_label must not succeed on a v2 hub (it can never reach a write)"
        );
        drop(work_dir);
    }

    #[test]
    fn test_v2_lock_claim_refuses() {
        let (work_dir, _remote, crosslink_dir) = setup_shared_writer_env_v2();
        let error = SharedWriter::new(&crosslink_dir).err().unwrap();
        assert!(error.to_string().contains("readiness"));
        drop(work_dir);
    }
}
