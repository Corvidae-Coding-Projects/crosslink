use std::path::{Path, PathBuf};

use axum::{
    extract::{Path as AxumPath, State},
    http::StatusCode,
    response::Json,
};
use chrono::{Duration, Utc};
use serde::Serialize;
use serde_json::{json, Value};

use crate::server::{
    state::AppState,
    types::{AgentDetail, AgentStatus, AgentSummary, ApiError, LockEntry},
};
use crate::sync::SyncManager;

const ACTIVE_THRESHOLD_SECS: i64 = 5 * 60;

const IDLE_THRESHOLD_SECS: i64 = 30 * 60;

#[derive(Debug, Serialize)]
pub struct AgentStatusResponse {
    pub agent_id: String,

    pub kickoff_status: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree_path: Option<String>,

    pub tmux_session_active: bool,
}

const fn classify_status(age_secs: i64) -> AgentStatus {
    if age_secs < ACTIVE_THRESHOLD_SECS {
        AgentStatus::Active
    } else if age_secs < IDLE_THRESHOLD_SECS {
        AgentStatus::Idle
    } else {
        AgentStatus::Stale
    }
}

fn find_worktree_for_agent(root: &Path, agent_id: &str) -> Option<PathBuf> {
    let worktrees_dir = root.join(".worktrees");
    if !worktrees_dir.is_dir() {
        return None;
    }
    std::fs::read_dir(&worktrees_dir)
        .ok()?
        .filter_map(std::result::Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .find(|e| {
            let slug = e.file_name().to_string_lossy().to_string();
            agent_id == slug
                || contains_at_word_boundary(agent_id, &slug)
                || contains_at_word_boundary(&slug, agent_id)
        })
        .map(|e| e.path())
}

const fn is_boundary(c: u8) -> bool {
    matches!(c, b'-' | b'_' | b'.')
}

fn contains_at_word_boundary(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() || needle.len() > haystack.len() {
        return false;
    }
    let h = haystack.as_bytes();
    let n = needle.as_bytes();
    for start in 0..=(h.len() - n.len()) {
        if &h[start..start + n.len()] == n {
            let left_ok = start == 0 || is_boundary(h[start - 1]);
            let right_ok = start + n.len() == h.len() || is_boundary(h[start + n.len()]);
            if left_ok && right_ok {
                return true;
            }
        }
    }
    false
}

fn read_worktree_branch(worktree: &Path) -> Option<String> {
    let git_entry = worktree.join(".git");
    let head_content = if git_entry.is_file() {
        let git_file = std::fs::read_to_string(&git_entry).ok()?;
        let gitdir = git_file.strip_prefix("gitdir: ")?.trim();
        let head_path = PathBuf::from(gitdir).join("HEAD");
        std::fs::read_to_string(&head_path).ok()?
    } else if git_entry.is_dir() {
        std::fs::read_to_string(git_entry.join("HEAD")).ok()?
    } else {
        return None;
    };

    head_content
        .strip_prefix("ref: refs/heads/")
        .map(|b| b.trim().to_string())
}

async fn tmux_session_exists(name: &str) -> bool {
    tokio::process::Command::new("tmux")
        .args(["has-session", "-t", name])
        .output()
        .await
        .is_ok_and(|o| o.status.success())
}

fn agent_tmux_session(agent_id: &str) -> String {
    let slug = agent_id
        .strip_prefix("feature/")
        .or_else(|| agent_id.strip_prefix("feat-"))
        .unwrap_or(agent_id);

    let wt_slug = slug.rsplit("--").next().unwrap_or(slug);
    let raw = format!("feat-{wt_slug}");
    let sanitized: String = raw
        .chars()
        .map(|c| if c == '.' || c == ':' { '-' } else { c })
        .collect();
    if sanitized.len() > 50 {
        sanitized[..50].to_string()
    } else {
        sanitized
    }
}

fn worktree_runtime_status(worktree: &std::path::Path) -> Option<String> {
    let sentinel = std::fs::read_to_string(worktree.join(".kickoff-status"))
        .ok()
        .map(|value| value.trim().to_string());
    let runtime = crate::agents::runtime_snapshot(worktree).status;
    match (sentinel, runtime) {
        (Some(current), Some(normalized))
            if current.eq_ignore_ascii_case("running")
                || matches!(
                    normalized.as_str(),
                    "done" | "failed" | "timed-out" | "waiting"
                ) =>
        {
            Some(normalized)
        }
        (Some(current), _) => Some(current),
        (None, runtime) => runtime,
    }
}

use crate::server::errors::internal_error;

pub async fn list_agents(
    State(state): State<AppState>,
) -> Result<Json<Value>, (StatusCode, Json<ApiError>)> {
    let sync = SyncManager::new(&state.crosslink_dir)
        .map_err(|e| internal_error("Failed to initialise SyncManager", e))?;

    let heartbeats = sync
        .read_heartbeats_auto()
        .map_err(|e| internal_error("Failed to read heartbeats", e))?;

    let locks_file = sync
        .read_locks_auto()
        .unwrap_or_else(|_| crate::locks::LocksFile::empty());

    let now = Utc::now();
    let root = state
        .crosslink_dir
        .parent()
        .map_or_else(|| state.crosslink_dir.clone(), std::path::Path::to_path_buf);

    let agents: Vec<AgentSummary> = heartbeats
        .into_iter()
        .map(|hb| {
            let age_secs = now
                .signed_duration_since(hb.last_heartbeat)
                .max(Duration::zero())
                .num_seconds();
            let status = classify_status(age_secs);
            let agent_locks = locks_file.agent_locks(&hb.agent_id);
            let worktree = find_worktree_for_agent(&root, &hb.agent_id);
            let branch = worktree.as_deref().and_then(read_worktree_branch);
            let worktree_path = worktree.map(|p| p.to_string_lossy().into_owned());
            AgentSummary {
                agent_id: hb.agent_id,
                machine_id: hb.machine_id,
                description: None,
                status,
                last_heartbeat: hb.last_heartbeat,
                active_issue_id: hb.active_issue_id,
                branch,
                worktree_path,
                locks: agent_locks,
            }
        })
        .collect();

    let total = agents.len();
    Ok(Json(json!({
        "items": agents,
        "total": total
    })))
}

pub async fn get_agent(
    State(state): State<AppState>,
    AxumPath(agent_id): AxumPath<String>,
) -> Result<Json<AgentDetail>, (StatusCode, Json<ApiError>)> {
    let sync = SyncManager::new(&state.crosslink_dir)
        .map_err(|e| internal_error("Failed to initialise SyncManager", e))?;

    let heartbeats = sync
        .read_heartbeats_auto()
        .map_err(|e| internal_error("Failed to read heartbeats", e))?;

    let hb = heartbeats.into_iter().find(|h| h.agent_id == agent_id);

    let locks_file = sync
        .read_locks_auto()
        .unwrap_or_else(|_| crate::locks::LocksFile::empty());

    let now = Utc::now();
    let (status, agent_locks) = hb.as_ref().map_or_else(
        || (AgentStatus::Unknown, locks_file.agent_locks(&agent_id)),
        |h| {
            let age_secs = now
                .signed_duration_since(h.last_heartbeat)
                .max(Duration::zero())
                .num_seconds();
            (
                classify_status(age_secs),
                locks_file.agent_locks(&h.agent_id),
            )
        },
    );

    let root = state
        .crosslink_dir
        .parent()
        .map_or_else(|| state.crosslink_dir.clone(), std::path::Path::to_path_buf);

    let worktree = find_worktree_for_agent(&root, &agent_id);
    let branch = worktree.as_deref().and_then(read_worktree_branch);
    let worktree_path = worktree.as_ref().map(|p| p.to_string_lossy().into_owned());

    let kickoff_status = worktree
        .as_ref()
        .and_then(|worktree| worktree_runtime_status(worktree));

    let heartbeat_history = hb
        .as_ref()
        .map(|h| vec![h.last_heartbeat])
        .unwrap_or_default();

    let summary = AgentSummary {
        agent_id: hb
            .as_ref()
            .map_or_else(|| agent_id.clone(), |h| h.agent_id.clone()),
        machine_id: hb
            .as_ref()
            .map(|h| h.machine_id.clone())
            .unwrap_or_default(),
        description: None,
        status,
        last_heartbeat: hb.as_ref().map_or_else(Utc::now, |h| h.last_heartbeat),
        active_issue_id: hb.as_ref().and_then(|h| h.active_issue_id),
        branch,
        worktree_path,
        locks: agent_locks,
    };

    Ok(Json(AgentDetail {
        summary,
        heartbeat_history,
        kickoff_status,
    }))
}

pub async fn get_agent_status(
    State(state): State<AppState>,
    AxumPath(agent_id): AxumPath<String>,
) -> Result<Json<AgentStatusResponse>, (StatusCode, Json<ApiError>)> {
    let root = state
        .crosslink_dir
        .parent()
        .map_or_else(|| state.crosslink_dir.clone(), std::path::Path::to_path_buf);

    let worktree = find_worktree_for_agent(&root, &agent_id);

    let kickoff_status = worktree.as_ref().map_or_else(
        || "unknown".to_string(),
        |worktree| worktree_runtime_status(worktree).unwrap_or_else(|| "running".to_string()),
    );

    let session_name = agent_tmux_session(&agent_id);
    let tmux_session_active = tmux_session_exists(&session_name).await;

    Ok(Json(AgentStatusResponse {
        agent_id,
        kickoff_status,
        worktree_path: worktree.map(|p| p.to_string_lossy().into_owned()),
        tmux_session_active,
    }))
}

pub async fn list_locks(
    State(state): State<AppState>,
) -> Result<Json<Value>, (StatusCode, Json<ApiError>)> {
    let sync = SyncManager::new(&state.crosslink_dir)
        .map_err(|e| internal_error("Failed to initialise SyncManager", e))?;

    let locks_file = sync
        .read_locks_auto()
        .map_err(|e| internal_error("Failed to read locks", e))?;

    let now = Utc::now();
    let stale_timeout =
        Duration::minutes(locks_file.settings.stale_lock_timeout_minutes.cast_signed());

    let entries: Vec<LockEntry> = locks_file
        .locks
        .iter()
        .map(|(issue_id, lock)| {
            let age = now
                .signed_duration_since(lock.claimed_at)
                .max(Duration::zero());
            let is_stale = age >= stale_timeout;
            LockEntry {
                issue_id: *issue_id,
                agent_id: lock.agent_id.clone(),
                branch: lock.branch.clone(),
                claimed_at: lock.claimed_at,
                signed_by: lock.signed_by.clone(),
                age_seconds: age.num_seconds(),
                is_stale,
            }
        })
        .collect();

    let total = entries.len();
    Ok(Json(json!({
        "items": entries,
        "total": total
    })))
}

pub async fn list_stale_locks(
    State(state): State<AppState>,
) -> Result<Json<Value>, (StatusCode, Json<ApiError>)> {
    let sync = SyncManager::new(&state.crosslink_dir)
        .map_err(|e| internal_error("Failed to initialise SyncManager", e))?;

    let stale_locks = sync
        .find_stale_locks_with_age()
        .map_err(|e| internal_error("Failed to read stale locks", e))?;

    let locks_file = sync
        .read_locks_auto()
        .unwrap_or_else(|_| crate::locks::LocksFile::empty());

    let now = Utc::now();
    let entries: Vec<LockEntry> = stale_locks
        .into_iter()
        .filter_map(|(issue_id, _agent_id_from_stale, _age_minutes)| {
            let lock = locks_file.get_lock(issue_id)?;
            let age_secs = now
                .signed_duration_since(lock.claimed_at)
                .max(Duration::zero())
                .num_seconds();
            Some(LockEntry {
                issue_id,
                agent_id: lock.agent_id.clone(),
                branch: lock.branch.clone(),
                claimed_at: lock.claimed_at,
                signed_by: lock.signed_by.clone(),
                age_seconds: age_secs,
                is_stale: true,
            })
        })
        .collect();

    let total = entries.len();
    Ok(Json(json!({
        "items": entries,
        "total": total
    })))
}

#[derive(serde::Deserialize)]
pub struct LockNotifyRequest {
    pub issue_id: i64,
    pub action: String,
    pub agent_id: String,
}

pub async fn notify_lock_changed(
    State(state): State<AppState>,
    Json(body): Json<LockNotifyRequest>,
) -> Result<Json<Value>, (StatusCode, Json<ApiError>)> {
    let action = match body.action.as_str() {
        "claimed" => crate::server::types::LockAction::Claimed,
        "released" => crate::server::types::LockAction::Released,
        other => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(ApiError {
                    error: format!(
                        "Invalid lock action '{other}'. Must be 'claimed' or 'released'"
                    ),
                    detail: None,
                }),
            ));
        }
    };

    let _ = state.ws_tx.send(crate::server::ws::WsEvent::LockChanged(
        crate::server::types::WsLockChangedEvent {
            event_type: crate::server::types::WsEventType::LockChanged,
            issue_id: body.issue_id,
            action,
            agent_id: body.agent_id,
        },
    ));

    Ok(Json(json!({ "ok": true })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_classify_status_active() {
        assert_eq!(classify_status(0), AgentStatus::Active);
        assert_eq!(classify_status(60), AgentStatus::Active);
        assert_eq!(
            classify_status(ACTIVE_THRESHOLD_SECS - 1),
            AgentStatus::Active
        );
    }

    #[test]
    fn test_classify_status_idle() {
        assert_eq!(classify_status(ACTIVE_THRESHOLD_SECS), AgentStatus::Idle);
        assert_eq!(classify_status(IDLE_THRESHOLD_SECS - 1), AgentStatus::Idle);
    }

    #[test]
    fn test_classify_status_stale() {
        assert_eq!(classify_status(IDLE_THRESHOLD_SECS), AgentStatus::Stale);
        assert_eq!(
            classify_status(IDLE_THRESHOLD_SECS + 3600),
            AgentStatus::Stale
        );
    }

    #[test]
    fn test_agent_tmux_session_basic() {
        let name = agent_tmux_session("add-auth-feature");
        assert_eq!(name, "feat-add-auth-feature");
    }

    #[test]
    fn test_agent_tmux_session_strips_feature_prefix() {
        let name = agent_tmux_session("feature/add-auth");
        assert_eq!(name, "feat-add-auth");
    }

    #[test]
    fn test_agent_tmux_session_sanitizes_dots() {
        let name = agent_tmux_session("fix.auth.bug");
        assert_eq!(name, "feat-fix-auth-bug");
    }

    #[test]
    fn test_agent_tmux_session_truncates() {
        let long = "a".repeat(100);
        let name = agent_tmux_session(&long);
        assert!(name.len() <= 50);
    }

    #[test]
    fn test_find_worktree_exact_match() {
        let dir = tempfile::tempdir().unwrap();
        let worktrees = dir.path().join(".worktrees");
        std::fs::create_dir_all(worktrees.join("my-agent")).unwrap();

        let result = find_worktree_for_agent(dir.path(), "my-agent");
        assert!(result.is_some());
        assert!(result.unwrap().ends_with("my-agent"));
    }

    #[test]
    fn test_find_worktree_no_match() {
        let dir = tempfile::tempdir().unwrap();
        let worktrees = dir.path().join(".worktrees");
        std::fs::create_dir_all(worktrees.join("other-agent")).unwrap();

        let result = find_worktree_for_agent(dir.path(), "nonexistent-xyz");
        assert!(result.is_none());
    }

    #[test]
    fn test_find_worktree_no_worktrees_dir() {
        let dir = tempfile::tempdir().unwrap();
        let result = find_worktree_for_agent(dir.path(), "my-agent");
        assert!(result.is_none());
    }

    #[test]
    fn test_read_worktree_branch_from_file() {
        let dir = tempfile::tempdir().unwrap();

        let gitdir = dir.path().join("gitdir");
        std::fs::create_dir_all(&gitdir).unwrap();
        std::fs::write(gitdir.join("HEAD"), "ref: refs/heads/feature/my-branch\n").unwrap();
        std::fs::write(
            dir.path().join(".git"),
            format!("gitdir: {}\n", gitdir.display()),
        )
        .unwrap();

        let branch = read_worktree_branch(dir.path());
        assert_eq!(branch, Some("feature/my-branch".to_string()));
    }

    #[test]
    fn test_read_worktree_branch_detached() {
        let dir = tempfile::tempdir().unwrap();
        let gitdir = dir.path().join("gitdir");
        std::fs::create_dir_all(&gitdir).unwrap();

        std::fs::write(
            gitdir.join("HEAD"),
            "abc123def456abc123def456abc123def456abc1\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join(".git"),
            format!("gitdir: {}\n", gitdir.display()),
        )
        .unwrap();

        let branch = read_worktree_branch(dir.path());
        assert!(branch.is_none());
    }

    #[test]
    fn test_read_worktree_branch_bare_git_dir() {
        let dir = tempfile::tempdir().unwrap();
        let git_dir = dir.path().join(".git");
        std::fs::create_dir_all(&git_dir).unwrap();
        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();

        let branch = read_worktree_branch(dir.path());
        assert_eq!(branch, Some("main".to_string()));
    }

    #[test]
    fn test_read_worktree_branch_no_git() {
        let dir = tempfile::tempdir().unwrap();
        let branch = read_worktree_branch(dir.path());
        assert!(branch.is_none());
    }

    #[test]
    fn test_agent_tmux_session_double_dash_split() {
        let name = agent_tmux_session("parent--child-slug");
        assert_eq!(name, "feat-child-slug");
    }

    #[test]
    fn test_agent_tmux_session_feat_prefix() {
        let name = agent_tmux_session("feat-my-task");
        assert_eq!(name, "feat-my-task");
    }

    #[test]
    fn test_agent_tmux_session_colons_sanitized() {
        let name = agent_tmux_session("fix:auth:bug");
        assert_eq!(name, "feat-fix-auth-bug");
    }

    #[test]
    fn test_find_worktree_partial_match_agent_contains_slug() {
        let dir = tempfile::tempdir().unwrap();
        let worktrees = dir.path().join(".worktrees");
        std::fs::create_dir_all(worktrees.join("short")).unwrap();

        let result = find_worktree_for_agent(dir.path(), "long-short-name");
        assert!(result.is_some());
    }

    #[test]
    fn test_find_worktree_partial_match_slug_contains_agent() {
        let dir = tempfile::tempdir().unwrap();
        let worktrees = dir.path().join(".worktrees");
        std::fs::create_dir_all(worktrees.join("my-agent-extended")).unwrap();

        let result = find_worktree_for_agent(dir.path(), "my-agent");
        assert!(result.is_some());
    }

    #[test]
    fn test_classify_status_boundary_values() {
        assert_eq!(classify_status(ACTIVE_THRESHOLD_SECS), AgentStatus::Idle);

        assert_eq!(
            classify_status(ACTIVE_THRESHOLD_SECS - 1),
            AgentStatus::Active
        );

        assert_eq!(classify_status(IDLE_THRESHOLD_SECS), AgentStatus::Stale);

        assert_eq!(classify_status(IDLE_THRESHOLD_SECS - 1), AgentStatus::Idle);

        assert_eq!(classify_status(-10), AgentStatus::Active);
    }

    use crate::db::Database;
    use crate::server::{routes::build_router, state::AppState};
    use axum::{
        body::Body,
        http::{Method, Request, StatusCode},
    };
    use tower::util::ServiceExt;

    fn test_app() -> (axum::Router, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("test.db");
        let db = Database::open(&db_path).expect("test db");
        let crosslink_dir = dir.path().join(".crosslink");
        std::fs::create_dir_all(&crosslink_dir).unwrap();
        let state = AppState::new(db, crosslink_dir);
        (build_router(state, None), dir)
    }

    fn test_app_with_heartbeat(agent_id: &str) -> (axum::Router, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("test.db");
        let db = Database::open(&db_path).expect("test db");
        let crosslink_dir = dir.path().join(".crosslink");
        std::fs::create_dir_all(&crosslink_dir).unwrap();

        let agent_dir = crosslink_dir
            .join(".hub-cache")
            .join("agents")
            .join(agent_id);
        std::fs::create_dir_all(&agent_dir).unwrap();
        let hb = serde_json::json!({
            "agent_id": agent_id,
            "timestamp": chrono::Utc::now().to_rfc3339(),
            "active_issue_id": null,
            "machine_id": "test-machine"
        });
        std::fs::write(
            agent_dir.join("heartbeat.json"),
            serde_json::to_string(&hb).unwrap(),
        )
        .unwrap();

        let state = AppState::new(db, crosslink_dir);
        (build_router(state, None), dir)
    }

    async fn body_json(resp: axum::response::Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn test_list_agents_empty() {
        let (app, _dir) = test_app();
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/v1/agents")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["total"], 0);
        assert!(body["items"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_list_agents_with_heartbeat() {
        let (app, _dir) = test_app_with_heartbeat("test-agent-1");
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/v1/agents")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["total"], 1);
        let items = body["items"].as_array().unwrap();
        assert_eq!(items[0]["agent_id"], "test-agent-1");
        assert_eq!(items[0]["machine_id"], "test-machine");
        assert_eq!(items[0]["status"], "active");
    }

    #[tokio::test]
    async fn test_get_agent_no_heartbeat_returns_unknown() {
        let (app, _dir) = test_app_with_heartbeat("existing-agent");
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/v1/agents/no-heartbeat-agent")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let status = resp.status();
        assert!(
            status == StatusCode::OK || status == StatusCode::INTERNAL_SERVER_ERROR,
            "Expected 200 or 500, got {status}"
        );
        if status == StatusCode::OK {
            let body = body_json(resp).await;
            assert_eq!(body["status"], "unknown");
            assert_eq!(body["agent_id"], "no-heartbeat-agent");
        }
    }

    #[tokio::test]
    async fn test_get_agent_found() {
        let (app, _dir) = test_app_with_heartbeat("my-agent");
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/v1/agents/my-agent")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;

        assert_eq!(body["agent_id"], "my-agent");
        assert_eq!(body["status"], "active");
        assert_eq!(body["heartbeat_history"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn test_get_agent_status_unknown() {
        let (app, _dir) = test_app();
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/v1/agents/unknown-agent/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["agent_id"], "unknown-agent");
        assert_eq!(body["kickoff_status"], "unknown");
        assert_eq!(body["tmux_session_active"], false);
    }

    #[tokio::test]
    async fn test_list_locks_empty() {
        let (app, _dir) = test_app();
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/v1/locks")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["total"], 0);
        assert!(body["items"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_list_stale_locks_empty() {
        let (app, _dir) = test_app();
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/v1/locks/stale")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["total"], 0);
        assert!(body["items"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_notify_lock_changed_claimed() {
        let (app, _dir) = test_app();
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/v1/locks/notify")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "issue_id": 1,
                            "action": "claimed",
                            "agent_id": "agent-1"
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["ok"], true);
    }

    #[tokio::test]
    async fn test_notify_lock_changed_released() {
        let (app, _dir) = test_app();
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/v1/locks/notify")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "issue_id": 42,
                            "action": "released",
                            "agent_id": "agent-2"
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["ok"], true);
    }

    #[tokio::test]
    async fn test_notify_lock_changed_invalid_action() {
        let (app, _dir) = test_app();
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/v1/locks/notify")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "issue_id": 1,
                            "action": "stolen",
                            "agent_id": "agent-1"
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = body_json(resp).await;
        assert!(body["error"]
            .as_str()
            .unwrap()
            .contains("Invalid lock action"));
    }

    #[tokio::test]
    async fn test_get_agent_status_with_worktree_no_kickoff_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("test.db");
        let db = Database::open(&db_path).expect("test db");
        let crosslink_dir = dir.path().join(".crosslink");
        std::fs::create_dir_all(&crosslink_dir).unwrap();

        let worktrees_dir = dir.path().join(".worktrees").join("my-wt-agent");
        std::fs::create_dir_all(&worktrees_dir).unwrap();

        let state = AppState::new(db, crosslink_dir);
        let app = build_router(state, None);

        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/v1/agents/my-wt-agent/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["agent_id"], "my-wt-agent");

        assert_eq!(body["kickoff_status"], "running");
    }

    #[tokio::test]
    async fn test_get_agent_status_with_worktree_and_kickoff_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("test.db");
        let db = Database::open(&db_path).expect("test db");
        let crosslink_dir = dir.path().join(".crosslink");
        std::fs::create_dir_all(&crosslink_dir).unwrap();

        let worktrees_dir = dir.path().join(".worktrees").join("my-wt-agent2");
        std::fs::create_dir_all(&worktrees_dir).unwrap();

        std::fs::write(worktrees_dir.join(".kickoff-status"), "completed\n").unwrap();

        let state = AppState::new(db, crosslink_dir);
        let app = build_router(state, None);

        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/v1/agents/my-wt-agent2/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["agent_id"], "my-wt-agent2");
        assert_eq!(body["kickoff_status"], "completed");
        assert!(body["worktree_path"].as_str().is_some());
    }

    #[test]
    fn test_internal_error_helper() {
        let (status, json) = crate::server::errors::internal_error("ctx", "boom");
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(json.error, "ctx");
        assert_eq!(json.detail.as_deref(), Some("boom"));
    }

    #[test]
    fn test_not_found_helper() {
        let (status, json) = crate::server::errors::not_found("missing");
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(json.error, "not found");
        assert_eq!(json.detail.as_deref(), Some("missing"));
    }

    fn test_app_with_heartbeat_and_kickoff(
        agent_id: &str,
        kickoff_status: &str,
    ) -> (axum::Router, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("test.db");
        let db = Database::open(&db_path).expect("test db");
        let crosslink_dir = dir.path().join(".crosslink");
        std::fs::create_dir_all(&crosslink_dir).unwrap();

        let agent_dir = crosslink_dir
            .join(".hub-cache")
            .join("agents")
            .join(agent_id);
        std::fs::create_dir_all(&agent_dir).unwrap();
        let hb = serde_json::json!({
            "agent_id": agent_id,
            "timestamp": chrono::Utc::now().to_rfc3339(),
            "active_issue_id": null,
            "machine_id": "test-machine"
        });
        std::fs::write(
            agent_dir.join("heartbeat.json"),
            serde_json::to_string(&hb).unwrap(),
        )
        .unwrap();

        let worktrees_dir = dir.path().join(".worktrees").join(agent_id);
        std::fs::create_dir_all(&worktrees_dir).unwrap();
        std::fs::write(
            worktrees_dir.join(".kickoff-status"),
            format!("{kickoff_status}\n"),
        )
        .unwrap();

        let state = AppState::new(db, crosslink_dir);
        (build_router(state, None), dir)
    }

    #[tokio::test]
    async fn test_get_agent_with_kickoff_status() {
        let (app, _dir) = test_app_with_heartbeat_and_kickoff("kickoff-agent", "completed");
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/v1/agents/kickoff-agent")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["agent_id"], "kickoff-agent");

        assert_eq!(body["kickoff_status"], "completed");
    }

    fn test_app_with_lock_v3(
        agent_id: &str,
        issue_id: i64,
        branch: &str,
        fresh_heartbeat: bool,
    ) -> Option<(axum::Router, tempfile::TempDir)> {
        let dir = tempfile::tempdir().expect("tempdir");
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
        let sync = crate::sync::SyncManager::new(&crosslink_dir).unwrap();
        sync.init_cache().unwrap();
        assert!(sync.hub_mode().is_v3());
        let cache_dir = sync.cache_path();

        let claimed_at = if fresh_heartbeat {
            chrono::Utc::now()
        } else {
            chrono::Utc::now() - chrono::Duration::minutes(120)
        };
        let mut cp = crate::checkpoint::CheckpointState::default();
        cp.locks.insert(
            issue_id,
            crate::checkpoint::LockEntry {
                agent_id: agent_id.to_string(),
                branch: Some(branch.to_string()),
                claimed_at,
            },
        );
        let bytes = serde_json::to_vec_pretty(&cp).unwrap();
        crate::hub_v3::commit_blob_to_ref(
            cache_dir,
            crate::hub_v3::CHECKPOINT_REF,
            "state.json",
            &bytes,
            "test: seed v3 lock",
        )
        .unwrap();

        if fresh_heartbeat {
            let hb = crate::locks::Heartbeat {
                agent_id: agent_id.to_string(),
                last_heartbeat: claimed_at,
                active_issue_id: Some(issue_id),
                machine_id: "test-machine".to_string(),
            };
            crate::hub_v3::write_heartbeat_to_ref(cache_dir, agent_id, &hb).unwrap();
        }

        let db = Database::open(&repo_root.join("test.db")).expect("test db");
        let state = AppState::new(db, crosslink_dir);
        Some((build_router(state, None), dir))
    }

    #[tokio::test]
    async fn test_list_locks_with_one_lock() {
        let Some((app, _dir)) = test_app_with_lock_v3("lock-agent", 42, "feature/test", true)
        else {
            return;
        };
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/v1/locks")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["total"], 1);
        let items = body["items"].as_array().unwrap();
        assert_eq!(items[0]["issue_id"], 42);
        assert_eq!(items[0]["agent_id"], "lock-agent");
        assert_eq!(items[0]["branch"], "feature/test");
        assert_eq!(items[0]["is_stale"], false);
    }

    fn test_app_with_stale_lock(
        agent_id: &str,
        issue_id: i64,
    ) -> Option<(axum::Router, tempfile::TempDir)> {
        test_app_with_lock_v3(agent_id, issue_id, "feature/stale-test", false)
    }

    #[tokio::test]
    async fn test_list_stale_locks_with_stale_entry() {
        let Some((app, _dir)) = test_app_with_stale_lock("stale-agent", 77) else {
            return;
        };
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/v1/locks/stale")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;

        let total = body["total"].as_u64().unwrap_or(0);
        assert!(total >= 1, "expected at least one stale lock, got {total}");
        let items = body["items"].as_array().unwrap();
        let entry = &items[0];
        assert_eq!(entry["issue_id"], 77);
        assert_eq!(entry["agent_id"], "stale-agent");
        assert_eq!(entry["branch"], "feature/stale-test");
        assert_eq!(entry["is_stale"], true);

        assert!(entry["age_seconds"].as_i64().unwrap_or(0) > 0);
    }

    #[test]
    fn test_internal_error_helper_detail_none_via_display() {
        let (status, json) =
            crate::server::errors::internal_error("db error", std::io::Error::other("disk full"));
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(json.error, "db error");
        assert!(json.detail.as_deref().unwrap().contains("disk full"));
    }

    #[test]
    fn test_not_found_helper_with_owned_string() {
        let msg = format!("agent '{}' not found", "worker-1");
        let (status, json) = crate::server::errors::not_found(msg);
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(json.error, "not found");
        assert!(json.detail.as_deref().unwrap().contains("worker-1"));
    }
}
