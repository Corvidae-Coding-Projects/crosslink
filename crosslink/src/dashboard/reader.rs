#![allow(dead_code)]

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone)]
pub struct HubSnapshot {
    pub hub_sha: Option<String>,

    pub layout_version: u32,

    pub issues: Vec<crate::issue_file::IssueFile>,

    pub agents: Vec<crate::locks::Heartbeat>,

    pub locks: Vec<LockRecord>,

    pub agent_requests: Vec<AgentRequestsForAgent>,

    pub ci_status: Option<CiStatus>,

    pub signature_state: SignatureState,

    pub last_commit_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
pub struct AgentRequestsForAgent {
    pub agent_id: String,
    pub requests: Vec<crate::agent_requests::RequestWithAck>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct CiStatus {
    pub sha: String,
    pub state: String,
    #[serde(default)]
    pub url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureState {
    Valid,
    Unsigned,
    Invalid,
    Unknown,
}

impl SignatureState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Valid => "valid",
            Self::Unsigned => "unsigned",
            Self::Invalid => "invalid",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone)]
pub struct LockRecord {
    pub issue_id: i64,
    pub lock: crate::locks::Lock,
}

pub fn read_snapshot(clone_path: &Path) -> Result<HubSnapshot> {
    anyhow::ensure!(
        clone_path.is_dir(),
        "clone path does not exist or is not a directory: {}",
        clone_path.display()
    );

    if crate::hub_v3::HubMode::resolve(clone_path).is_v3() {
        let hub_sha = git_rev_parse(clone_path, crate::hub_v3::CHECKPOINT_REF);
        let last_commit_at = git_last_commit_at(clone_path, crate::hub_v3::CHECKPOINT_REF);
        return read_snapshot_v3(clone_path, hub_sha, last_commit_at);
    }

    let hub_sha = git_rev_parse(clone_path, "crosslink/hub");
    let last_commit_at = git_last_commit_at(clone_path, "crosslink/hub");

    let hub_root = resolve_hub_root(clone_path);

    let meta_dir = hub_root.join("meta");
    let layout_version = if meta_dir.is_dir() {
        crate::issue_file::read_layout_version(&meta_dir).unwrap_or(1)
    } else {
        1
    };

    let issues_dir = hub_root.join("issues");
    let issues = if issues_dir.is_dir() {
        crate::issue_file::read_all_issue_files(&issues_dir).unwrap_or_default()
    } else {
        Vec::new()
    };

    let agents = read_agent_heartbeats(&hub_root);
    let locks = read_locks(&hub_root);
    let agent_requests = read_agent_requests(&hub_root, &agents);
    let ci_status = read_ci_status(&hub_root, hub_sha.as_deref());
    let signature_state = read_signature_state(clone_path);

    Ok(HubSnapshot {
        hub_sha,
        layout_version,
        issues,
        agents,
        locks,
        agent_requests,
        ci_status,
        signature_state,
        last_commit_at,
    })
}

fn read_snapshot_v3(
    clone_path: &Path,
    hub_sha: Option<String>,
    last_commit_at: Option<DateTime<Utc>>,
) -> Result<HubSnapshot> {
    let state = read_checkpoint_state(clone_path)?;

    let issues: Vec<crate::issue_file::IssueFile> =
        state.issues.values().map(compact_issue_to_file).collect();

    let locks: Vec<LockRecord> = state
        .locks
        .into_iter()
        .map(|(issue_id, entry)| LockRecord {
            issue_id,
            lock: crate::locks::Lock {
                agent_id: entry.agent_id,
                branch: entry.branch,
                claimed_at: entry.claimed_at,
                signed_by: String::new(),
            },
        })
        .collect();

    let mut agents: Vec<crate::locks::Heartbeat> =
        crate::hub_v3::read_heartbeats_from_refs(clone_path)
            .unwrap_or_default()
            .into_iter()
            .map(|(_, hb)| hb)
            .collect();
    agents.sort_by(|a, b| a.agent_id.cmp(&b.agent_id));

    let agent_requests: Vec<AgentRequestsForAgent> =
        crate::hub_v3::read_all_agent_requests(clone_path)
            .unwrap_or_default()
            .into_iter()
            .map(|(agent_id, requests)| AgentRequestsForAgent { agent_id, requests })
            .collect();

    Ok(HubSnapshot {
        hub_sha,

        layout_version: 3,
        issues,
        agents,
        locks,
        agent_requests,
        ci_status: None,
        signature_state: read_signature_state(clone_path),
        last_commit_at,
    })
}

fn read_checkpoint_state(repo_dir: &Path) -> Result<crate::checkpoint::CheckpointState> {
    let Some(tip) = crate::hub_v3::git_rev_parse_optional(repo_dir, crate::hub_v3::CHECKPOINT_REF)?
    else {
        return Ok(crate::checkpoint::CheckpointState::default());
    };
    let spec = format!("{tip}:state.json");
    let Some(bytes) = crate::hub_v3::git_cat_file_blob_optional(repo_dir, &spec)? else {
        return Ok(crate::checkpoint::CheckpointState::default());
    };
    crate::checkpoint::CheckpointState::from_slice(&bytes)
        .context("failed to parse v3 checkpoint state.json for dashboard snapshot")
}

fn compact_issue_to_file(issue: &crate::checkpoint::CompactIssue) -> crate::issue_file::IssueFile {
    let comments = issue
        .comments
        .values()
        .map(|c| crate::issue_file::CommentEntry {
            id: c.display_id.unwrap_or(0),
            author: c.author.clone(),
            content: c.content.clone(),
            created_at: c.created_at,
            kind: c.kind.clone(),
            trigger_type: c.trigger_type.clone(),
            intervention_context: c.intervention_context.clone(),
            driver_key_fingerprint: c.driver_key_fingerprint.clone(),
            signed_by: c.signed_by.clone(),
            signature: c.signature.clone(),
        })
        .collect();
    let time_entries = issue
        .time_entries
        .values()
        .map(|t| crate::issue_file::TimeEntry {
            id: t.display_id.unwrap_or(0),
            started_at: t.started_at,
            ended_at: t.ended_at,
            duration_seconds: t.duration_seconds,
        })
        .collect();
    crate::issue_file::IssueFile {
        uuid: issue.uuid,
        display_id: issue.display_id,
        title: issue.title.clone(),
        description: issue.description.clone(),
        status: issue.status,
        priority: issue.priority,
        parent_uuid: issue.parent_uuid,
        created_by: issue.created_by.clone(),
        created_at: issue.created_at,
        updated_at: issue.updated_at,
        closed_at: issue.closed_at,
        scheduled_at: issue.scheduled_at,
        due_at: issue.due_at,
        labels: issue.labels.iter().cloned().collect(),
        comments,
        blockers: issue.blockers.iter().copied().collect(),
        related: issue.related.iter().copied().collect(),
        milestone_uuid: issue.milestone_uuid,
        time_entries,
    }
}

fn read_ci_status(hub_root: &Path, hub_sha: Option<&str>) -> Option<CiStatus> {
    let path = hub_root.join("meta").join("ci-status.json");
    if !path.is_file() {
        return None;
    }
    let raw = std::fs::read_to_string(&path).ok()?;
    let status: CiStatus = serde_json::from_str(&raw).ok()?;
    match hub_sha {
        Some(sha) if status.sha == sha => Some(status),

        None => Some(status),

        Some(_) => None,
    }
}

fn read_signature_state(crosslink_workspace: &Path) -> SignatureState {
    let candidates = [
        crosslink_workspace.join("crosslink").join(".crosslink"),
        crosslink_workspace.join(".crosslink"),
    ];
    let Some(crosslink_dir) = candidates.iter().find(|c| c.is_dir()) else {
        return SignatureState::Unknown;
    };
    let Ok(sync) = crate::sync::SyncManager::new(crosslink_dir) else {
        return SignatureState::Unknown;
    };
    if !sync.is_initialized() {
        return SignatureState::Unknown;
    }
    match sync.verify_hub_signature_auto() {
        Ok(crate::signing::SignatureVerification::Valid) => SignatureState::Valid,
        Ok(crate::signing::SignatureVerification::Unsigned) => SignatureState::Unsigned,
        Ok(crate::signing::SignatureVerification::Invalid) => SignatureState::Invalid,
        Ok(crate::signing::SignatureVerification::NoCommits) | Err(_) => SignatureState::Unknown,
    }
}

fn resolve_hub_root(clone_path: &Path) -> std::path::PathBuf {
    let candidates = [
        clone_path.join(".crosslink").join(".hub-cache"),
        clone_path
            .join("crosslink")
            .join(".crosslink")
            .join(".hub-cache"),
    ];
    for candidate in candidates {
        if candidate.is_dir()
            && (candidate.join("issues").is_dir() || candidate.join("agents").is_dir())
        {
            return candidate;
        }
    }
    clone_path.to_path_buf()
}

fn read_agent_requests(
    clone_path: &Path,
    agents: &[crate::locks::Heartbeat],
) -> Vec<AgentRequestsForAgent> {
    let agents_dir = clone_path.join("agents");
    if !agents_dir.is_dir() {
        return Vec::new();
    }

    let mut ids: std::collections::BTreeSet<String> =
        agents.iter().map(|a| a.agent_id.clone()).collect();
    if let Ok(entries) = std::fs::read_dir(&agents_dir) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                if let Some(name) = entry.file_name().to_str() {
                    ids.insert(name.to_string());
                }
            }
        }
    }

    let mut out = Vec::new();
    for agent_id in ids {
        let Ok(requests) = crate::agent_requests::scan(clone_path, &agent_id) else {
            continue;
        };
        if !requests.is_empty() {
            out.push(AgentRequestsForAgent { agent_id, requests });
        }
    }
    out
}

fn read_agent_heartbeats(clone_path: &Path) -> Vec<crate::locks::Heartbeat> {
    let agents_dir = clone_path.join("agents");
    if !agents_dir.is_dir() {
        return Vec::new();
    }

    let Ok(entries) = std::fs::read_dir(&agents_dir) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for entry in entries.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let hb_path = entry.path().join("heartbeat.json");
        if !hb_path.is_file() {
            continue;
        }
        if let Ok(content) = std::fs::read_to_string(&hb_path) {
            if let Ok(hb) = serde_json::from_str::<crate::locks::Heartbeat>(&content) {
                out.push(hb);
            }
        }
    }
    out.sort_by(|a, b| a.agent_id.cmp(&b.agent_id));
    out
}

fn read_locks(clone_path: &Path) -> Vec<LockRecord> {
    let v2_dir = clone_path.join("locks");
    if v2_dir.is_dir() {
        return read_locks_v2(&v2_dir);
    }

    let v1_path = clone_path.join("locks.json");
    if v1_path.is_file() {
        return read_locks_v1(&v1_path);
    }

    Vec::new()
}

fn read_locks_v1(path: &Path) -> Vec<LockRecord> {
    match crate::locks::LocksFile::load(path) {
        Ok(file) => file
            .locks
            .into_iter()
            .map(|(issue_id, lock)| LockRecord { issue_id, lock })
            .collect(),
        Err(_) => Vec::new(),
    }
}

fn read_locks_v2(dir: &Path) -> Vec<LockRecord> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };

        let Ok(issue_id) = stem.parse::<i64>() else {
            continue;
        };
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(lock) = serde_json::from_str::<crate::locks::Lock>(&content) {
                out.push(LockRecord { issue_id, lock });
            }
        }
    }
    out.sort_by_key(|r| r.issue_id);
    out
}

fn git_rev_parse(clone_path: &Path, revision: &str) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(clone_path)
        .args(["rev-parse", revision])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if stdout.is_empty() {
        None
    } else {
        Some(stdout)
    }
}

fn git_last_commit_at(clone_path: &Path, revision: &str) -> Option<DateTime<Utc>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(clone_path)
        .args(["log", "-1", "--format=%cI", revision])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
    DateTime::parse_from_rfc3339(&raw)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProjectCounters {
    pub open_issues: i64,
    pub overdue_issues: i64,
    pub due_soon_issues: i64,
    pub blocked_issues: i64,
    pub active_agents: i64,
    pub stale_locks: i64,
}

impl HubSnapshot {
    #[must_use]
    pub fn derive_counters(
        &self,
        now: DateTime<Utc>,
        agent_active_window_minutes: i64,
        stale_lock_minutes: i64,
    ) -> ProjectCounters {
        use std::collections::HashSet;

        let open: Vec<&crate::issue_file::IssueFile> = self
            .issues
            .iter()
            .filter(|i| matches!(i.status, crate::models::IssueStatus::Open))
            .collect();

        let open_uuids: HashSet<uuid::Uuid> = open.iter().map(|i| i.uuid).collect();

        let due_soon_window = chrono::Duration::hours(24);
        let mut overdue = 0i64;
        let mut due_soon = 0i64;
        let mut blocked = 0i64;
        for issue in &open {
            if let Some(due) = issue.due_at {
                if due < now {
                    overdue += 1;
                } else if due - now <= due_soon_window {
                    due_soon += 1;
                }
            }
            if issue.blockers.iter().any(|b| open_uuids.contains(b)) {
                blocked += 1;
            }
        }

        let agent_window = chrono::Duration::minutes(agent_active_window_minutes);
        let active_agents = self
            .agents
            .iter()
            .filter(|a| now - a.last_heartbeat <= agent_window)
            .count() as i64;

        let stale_window = chrono::Duration::minutes(stale_lock_minutes);
        let stale_locks = self
            .locks
            .iter()
            .filter(|r| now - r.lock.claimed_at > stale_window)
            .count() as i64;

        ProjectCounters {
            open_issues: open.len() as i64,
            overdue_issues: overdue,
            due_soon_issues: due_soon,
            blocked_issues: blocked,
            active_agents,
            stale_locks,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use serde_json::json;
    use std::fs;
    use tempfile::tempdir;
    use uuid::Uuid;

    fn now_fixed() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 4, 20, 12, 0, 0).unwrap()
    }

    fn write_issue(dir: &Path, file: &crate::issue_file::IssueFile) {
        let issue_dir = dir.join("issues").join(file.uuid.to_string());
        fs::create_dir_all(&issue_dir).unwrap();
        let path = issue_dir.join("issue.json");
        fs::write(&path, serde_json::to_string_pretty(file).unwrap()).unwrap();
    }

    fn make_issue(
        uuid: Uuid,
        display_id: i64,
        status: crate::models::IssueStatus,
        due_at: Option<DateTime<Utc>>,
        blockers: Vec<Uuid>,
    ) -> crate::issue_file::IssueFile {
        crate::issue_file::IssueFile {
            uuid,
            display_id: Some(display_id),
            title: format!("Issue {display_id}"),
            description: None,
            status,
            priority: crate::models::Priority::Medium,
            parent_uuid: None,
            created_by: "test".into(),
            created_at: now_fixed(),
            updated_at: now_fixed(),
            closed_at: None,
            scheduled_at: None,
            due_at,
            labels: vec![],
            comments: vec![],
            blockers,
            related: vec![],
            milestone_uuid: None,
            time_entries: vec![],
        }
    }

    #[test]
    fn test_read_snapshot_rejects_missing_dir() {
        let bogus = std::path::PathBuf::from("/tmp/definitely-not-a-clone-path-xyz123");
        let err = read_snapshot(&bogus).unwrap_err();
        assert!(err.to_string().contains("does not exist"));
    }

    #[test]
    fn test_read_snapshot_empty_clone() {
        let dir = tempdir().unwrap();
        let snap = read_snapshot(dir.path()).unwrap();
        assert!(snap.issues.is_empty());
        assert!(snap.agents.is_empty());
        assert!(snap.locks.is_empty());
        assert_eq!(snap.layout_version, 1);
        assert!(snap.hub_sha.is_none());
    }

    #[test]
    fn test_read_snapshot_parses_issues() {
        let dir = tempdir().unwrap();
        let issue = make_issue(
            Uuid::new_v4(),
            42,
            crate::models::IssueStatus::Open,
            None,
            vec![],
        );
        write_issue(dir.path(), &issue);
        let snap = read_snapshot(dir.path()).unwrap();
        assert_eq!(snap.issues.len(), 1);
        assert_eq!(snap.issues[0].display_id, Some(42));
    }

    #[test]
    fn test_read_snapshot_parses_heartbeats() {
        let dir = tempdir().unwrap();
        let agents_dir = dir.path().join("agents").join("jus4");
        fs::create_dir_all(&agents_dir).unwrap();
        let hb = json!({
            "agent_id": "jus4",
            "last_heartbeat": "2026-04-20T11:55:00+00:00",
            "active_issue_id": 42,
            "machine_id": "host-a"
        });
        fs::write(agents_dir.join("heartbeat.json"), hb.to_string()).unwrap();

        let snap = read_snapshot(dir.path()).unwrap();
        assert_eq!(snap.agents.len(), 1);
        assert_eq!(snap.agents[0].agent_id, "jus4");
        assert_eq!(snap.agents[0].active_issue_id, Some(42));
    }

    #[test]
    fn test_read_snapshot_parses_v1_locks() {
        let dir = tempdir().unwrap();
        let locks_json = json!({
            "version": 1,
            "locks": {
                "42": {
                    "agent_id": "jus4",
                    "branch": "feature/xyz",
                    "claimed_at": "2026-04-20T10:00:00+00:00",
                    "signed_by": "SHA256:abc"
                }
            },
            "settings": { "stale_lock_timeout_minutes": 60 }
        });
        fs::write(dir.path().join("locks.json"), locks_json.to_string()).unwrap();

        let snap = read_snapshot(dir.path()).unwrap();
        assert_eq!(snap.locks.len(), 1);
        assert_eq!(snap.locks[0].issue_id, 42);
        assert_eq!(snap.locks[0].lock.agent_id, "jus4");
    }

    #[test]
    fn test_read_snapshot_parses_v2_locks() {
        let dir = tempdir().unwrap();
        let locks_dir = dir.path().join("locks");
        fs::create_dir_all(&locks_dir).unwrap();
        let lock_json = json!({
            "agent_id": "jus4",
            "branch": null,
            "claimed_at": "2026-04-20T10:00:00+00:00",
            "signed_by": "SHA256:abc"
        });
        fs::write(locks_dir.join("42.json"), lock_json.to_string()).unwrap();

        let snap = read_snapshot(dir.path()).unwrap();
        assert_eq!(snap.locks.len(), 1);
        assert_eq!(snap.locks[0].issue_id, 42);
    }

    #[test]
    fn test_counters_open_and_closed() {
        let snap = HubSnapshot {
            hub_sha: None,
            layout_version: 2,
            issues: vec![
                make_issue(
                    Uuid::new_v4(),
                    1,
                    crate::models::IssueStatus::Open,
                    None,
                    vec![],
                ),
                make_issue(
                    Uuid::new_v4(),
                    2,
                    crate::models::IssueStatus::Open,
                    None,
                    vec![],
                ),
                make_issue(
                    Uuid::new_v4(),
                    3,
                    crate::models::IssueStatus::Closed,
                    None,
                    vec![],
                ),
            ],
            agents: vec![],
            locks: vec![],
            agent_requests: vec![],
            ci_status: None,
            signature_state: SignatureState::Unknown,
            last_commit_at: None,
        };
        let counters = snap.derive_counters(now_fixed(), 10, 60);
        assert_eq!(counters.open_issues, 2);
        assert_eq!(counters.blocked_issues, 0);
    }

    #[test]
    fn test_counters_overdue_and_due_soon() {
        let now = now_fixed();
        let overdue = now - chrono::Duration::days(1);
        let soon = now + chrono::Duration::hours(6);
        let distant = now + chrono::Duration::days(30);

        let snap = HubSnapshot {
            hub_sha: None,
            layout_version: 2,
            issues: vec![
                make_issue(
                    Uuid::new_v4(),
                    1,
                    crate::models::IssueStatus::Open,
                    Some(overdue),
                    vec![],
                ),
                make_issue(
                    Uuid::new_v4(),
                    2,
                    crate::models::IssueStatus::Open,
                    Some(soon),
                    vec![],
                ),
                make_issue(
                    Uuid::new_v4(),
                    3,
                    crate::models::IssueStatus::Open,
                    Some(distant),
                    vec![],
                ),
            ],
            agents: vec![],
            locks: vec![],
            agent_requests: vec![],
            ci_status: None,
            signature_state: SignatureState::Unknown,
            last_commit_at: None,
        };
        let c = snap.derive_counters(now, 10, 60);
        assert_eq!(c.open_issues, 3);
        assert_eq!(c.overdue_issues, 1);
        assert_eq!(c.due_soon_issues, 1);
    }

    #[test]
    fn test_counters_blocked_only_counts_open_blockers() {
        let blocker_open = Uuid::new_v4();
        let blocker_closed = Uuid::new_v4();
        let snap = HubSnapshot {
            hub_sha: None,
            layout_version: 2,
            issues: vec![
                make_issue(
                    blocker_open,
                    10,
                    crate::models::IssueStatus::Open,
                    None,
                    vec![],
                ),
                make_issue(
                    blocker_closed,
                    11,
                    crate::models::IssueStatus::Closed,
                    None,
                    vec![],
                ),
                make_issue(
                    Uuid::new_v4(),
                    20,
                    crate::models::IssueStatus::Open,
                    None,
                    vec![blocker_open],
                ),
                make_issue(
                    Uuid::new_v4(),
                    21,
                    crate::models::IssueStatus::Open,
                    None,
                    vec![blocker_closed],
                ),
            ],
            agents: vec![],
            locks: vec![],
            agent_requests: vec![],
            ci_status: None,
            signature_state: SignatureState::Unknown,
            last_commit_at: None,
        };
        let c = snap.derive_counters(now_fixed(), 10, 60);
        assert_eq!(c.open_issues, 3);
        assert_eq!(c.blocked_issues, 1);
    }

    #[test]
    fn test_counters_active_agents() {
        let now = now_fixed();
        let fresh = crate::locks::Heartbeat {
            agent_id: "fresh".into(),
            last_heartbeat: now - chrono::Duration::minutes(2),
            active_issue_id: Some(1),
            machine_id: "host".into(),
        };
        let stale = crate::locks::Heartbeat {
            agent_id: "stale".into(),
            last_heartbeat: now - chrono::Duration::minutes(30),
            active_issue_id: None,
            machine_id: "host".into(),
        };
        let snap = HubSnapshot {
            hub_sha: None,
            layout_version: 2,
            issues: vec![],
            agents: vec![fresh, stale],
            locks: vec![],
            agent_requests: vec![],
            ci_status: None,
            signature_state: SignatureState::Unknown,
            last_commit_at: None,
        };
        let c = snap.derive_counters(now, 10, 60);
        assert_eq!(c.active_agents, 1);
    }

    #[test]
    fn test_counters_stale_locks() {
        let now = now_fixed();
        let fresh = LockRecord {
            issue_id: 1,
            lock: crate::locks::Lock {
                agent_id: "jus4".into(),
                branch: None,
                claimed_at: now - chrono::Duration::minutes(5),
                signed_by: "SHA256:a".into(),
            },
        };
        let stale = LockRecord {
            issue_id: 2,
            lock: crate::locks::Lock {
                agent_id: "jus4".into(),
                branch: None,
                claimed_at: now - chrono::Duration::minutes(90),
                signed_by: "SHA256:a".into(),
            },
        };
        let snap = HubSnapshot {
            hub_sha: None,
            layout_version: 2,
            issues: vec![],
            agents: vec![],
            locks: vec![fresh, stale],
            agent_requests: vec![],
            ci_status: None,
            signature_state: SignatureState::Unknown,
            last_commit_at: None,
        };
        let c = snap.derive_counters(now, 10, 60);
        assert_eq!(c.stale_locks, 1);
    }
}
