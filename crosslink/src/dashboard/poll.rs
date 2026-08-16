use anyhow::Result;
use chrono::Utc;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use super::alerts;
use super::alerts_db;
use super::db::DashboardDb;
use super::projects::Project;
use super::reader;
use super::webhook;
use crate::server::types::{WsDashboardAlertsEvent, WsDashboardProjectEvent, WsEventType};
use crate::server::ws::WsEvent;

pub const DEFAULT_TICK: Duration = Duration::from_secs(5);

const DEFAULT_AGENT_ACTIVE_MINUTES: i64 = 10;

const DEFAULT_STALE_LOCK_MINUTES: i64 = 60;

pub async fn run(
    db_path: PathBuf,
    cancel: CancellationToken,
    ws_tx: Option<tokio::sync::broadcast::Sender<WsEvent>>,
) {
    run_with_tick(db_path, DEFAULT_TICK, cancel, ws_tx).await;
}

pub async fn run_with_tick(
    db_path: PathBuf,
    tick: Duration,
    cancel: CancellationToken,
    ws_tx: Option<tokio::sync::broadcast::Sender<WsEvent>>,
) {
    tracing::info!(
        "dashboard poll loop starting (tick = {:?}, db = {})",
        tick,
        db_path.display()
    );

    let mut interval = tokio::time::interval(tick);

    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            () = cancel.cancelled() => {
                tracing::info!("dashboard poll loop cancelled");
                return;
            }
            _ = interval.tick() => {
                if let Err(e) = poll_all_projects(&db_path, ws_tx.as_ref()).await {
                    tracing::warn!("dashboard poll tick failed: {e}");
                }
            }
        }
    }
}

pub async fn poll_all_projects(
    db_path: &Path,
    ws_tx: Option<&tokio::sync::broadcast::Sender<WsEvent>>,
) -> Result<()> {
    let projects = load_active_projects(db_path)?;
    for project in projects {
        let slug = project.slug.clone();
        let outcome = match poll_project(db_path, &project).await {
            Ok(o) => o,
            Err(e) => {
                tracing::warn!("poll failed for {slug}: {e}");
                continue;
            }
        };

        if let Some(tx) = ws_tx {
            let _ = tx.send(WsEvent::DashboardProjectUpdated(WsDashboardProjectEvent {
                event_type: WsEventType::DashboardProjectUpdated,
                slug: slug.clone(),
            }));
            if outcome.alerts_opened > 0 || outcome.alerts_resolved > 0 {
                let _ = tx.send(WsEvent::DashboardAlertsChanged(WsDashboardAlertsEvent {
                    event_type: WsEventType::DashboardAlertsChanged,
                    slug,
                    opened: outcome.alerts_opened,
                    resolved: outcome.alerts_resolved,
                }));
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, Default)]
pub struct PollOutcome {
    pub alerts_opened: u32,
    pub alerts_resolved: u32,
}

pub async fn poll_project(db_path: &Path, project: &Project) -> Result<PollOutcome> {
    let fetch_ok = fetch_hub(&project.clone_path).await.is_ok();

    let clone_path = project.clone_path.clone();
    let snapshot = match tokio::task::spawn_blocking(move || reader::read_snapshot(&clone_path))
        .await
        .map_err(|e| anyhow::anyhow!("snapshot task panicked: {e}"))?
    {
        Ok(snap) => snap,
        Err(e) => {
            tracing::warn!("read_snapshot failed for {}: {e}", project.slug);
            let db_path_owned = db_path.to_path_buf();
            let project_id = project.id;
            tokio::task::spawn_blocking(move || {
                mark_project_status(&db_path_owned, project_id, "error")
            })
            .await
            .map_err(|e| anyhow::anyhow!("status update task panicked: {e}"))??;
            return Ok(PollOutcome::default());
        }
    };

    let status = if fetch_ok || snapshot.hub_sha.is_some() {
        "active"
    } else {
        "error"
    };

    let now = Utc::now();
    let counters = snapshot.derive_counters(
        now,
        DEFAULT_AGENT_ACTIVE_MINUTES,
        DEFAULT_STALE_LOCK_MINUTES,
    );
    let derived_alerts = alerts::derive_alerts(project, &snapshot, now);

    let project_id = project.id;
    let hub_sha = snapshot.hub_sha.clone();
    let last_commit_at = snapshot.last_commit_at.map(|dt| dt.to_rfc3339());
    let ci_state = snapshot.ci_status.as_ref().map(|c| c.state.clone());
    let db_path_owned = db_path.to_path_buf();

    let (sync_stats, webhook_urls) =
        tokio::task::spawn_blocking(move || -> Result<(alerts_db::SyncStats, Vec<String>)> {
            write_project_state(
                &db_path_owned,
                project_id,
                hub_sha.as_deref(),
                last_commit_at.as_deref(),
                counters,
                ci_state.as_deref(),
                status,
            )?;
            let db = DashboardDb::open(&db_path_owned)?;
            let stats = alerts_db::sync_alerts_for_project(&db, project_id, &derived_alerts)?;
            let urls = webhook::load_urls(&db).unwrap_or_default();
            Ok((stats, urls))
        })
        .await
        .map_err(|e| anyhow::anyhow!("DB update task panicked: {e}"))??;

    if !webhook_urls.is_empty() && !sync_stats.opened_alerts.is_empty() {
        let slug = project.slug.clone();
        let fired_at = now;
        for alert in &sync_stats.opened_alerts {
            let notif = webhook::AlertNotification::new(alert, slug.clone(), fired_at);
            let urls = webhook_urls.clone();
            tokio::spawn(async move {
                webhook::dispatch_all(&urls, &notif).await;
            });
        }
    }

    Ok(PollOutcome {
        alerts_opened: u32::try_from(sync_stats.opened).unwrap_or(u32::MAX),
        alerts_resolved: u32::try_from(sync_stats.resolved).unwrap_or(u32::MAX),
    })
}

fn load_active_projects(db_path: &Path) -> Result<Vec<Project>> {
    let db = DashboardDb::open(db_path)?;
    let mut stmt = db.conn.prepare(
        "SELECT id, slug, clone_path, default_branch, hub_sha, hub_fetched_at,
                status, added_at, last_activity_at, pinned
         FROM projects
         WHERE status = 'active'
         ORDER BY id",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok(Project {
                id: row.get(0)?,
                slug: row.get(1)?,
                clone_path: PathBuf::from(row.get::<_, String>(2)?),
                default_branch: row.get(3)?,
                hub_sha: row.get(4)?,
                hub_fetched_at: row.get(5)?,
                status: row.get(6)?,
                added_at: row.get(7)?,
                last_activity_at: row.get(8)?,
                pinned: row.get::<_, i64>(9)? != 0,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn mark_project_status(db_path: &Path, project_id: i64, status: &str) -> Result<()> {
    let db = DashboardDb::open(db_path)?;
    db.conn.execute(
        "UPDATE projects SET status = ?1 WHERE id = ?2",
        rusqlite::params![status, project_id],
    )?;
    Ok(())
}

fn write_project_state(
    db_path: &Path,
    project_id: i64,
    hub_sha: Option<&str>,
    last_commit_at: Option<&str>,
    counters: super::reader::ProjectCounters,
    ci_status: Option<&str>,
    status: &str,
) -> Result<()> {
    let db = DashboardDb::open(db_path)?;
    let now = Utc::now().to_rfc3339();

    db.conn.execute(
        "UPDATE projects
         SET hub_sha = ?1,
             hub_fetched_at = ?2,
             last_activity_at = COALESCE(?3, last_activity_at),
             status = ?4
         WHERE id = ?5",
        rusqlite::params![hub_sha, now, last_commit_at, status, project_id],
    )?;

    db.conn.execute(
        "INSERT INTO project_state
           (project_id, open_issues, overdue_issues, due_soon_issues, blocked_issues,
            active_agents, stale_locks, ci_status, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(project_id) DO UPDATE SET
           open_issues = excluded.open_issues,
           overdue_issues = excluded.overdue_issues,
           due_soon_issues = excluded.due_soon_issues,
           blocked_issues = excluded.blocked_issues,
           active_agents = excluded.active_agents,
           stale_locks = excluded.stale_locks,
           ci_status = excluded.ci_status,
           updated_at = excluded.updated_at",
        rusqlite::params![
            project_id,
            counters.open_issues,
            counters.overdue_issues,
            counters.due_soon_issues,
            counters.blocked_issues,
            counters.active_agents,
            counters.stale_locks,
            ci_status,
            now,
        ],
    )?;

    Ok(())
}

async fn fetch_hub(clone_path: &Path) -> Result<()> {
    let status = Command::new("git")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "")
        .arg("-C")
        .arg(clone_path)
        .args([
            "-c",
            "credential.helper=",
            "fetch",
            "--quiet",
            "origin",
            "+refs/heads/crosslink/*:refs/heads/crosslink/*",
        ])
        .status()
        .await?;
    if !status.success() {
        anyhow::bail!("git fetch exited with {status}");
    }

    if !crate::hub_v3::HubMode::resolve(clone_path).is_v3() {
        ensure_hub_cache_worktree(clone_path).await;
    }
    Ok(())
}

async fn ensure_hub_cache_worktree(clone_path: &Path) {
    let cache_path = clone_path.join(".crosslink").join(".hub-cache");

    if let Some(parent) = cache_path.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }

    if cache_path.is_dir() {
        let porcelain = Command::new("git")
            .arg("-C")
            .arg(&cache_path)
            .args(["status", "--porcelain"])
            .output()
            .await;
        let is_dirty = matches!(
            porcelain,
            Ok(out) if out.status.success() && !out.stdout.is_empty()
        );
        if is_dirty {
            return;
        }

        let status = Command::new("git")
            .arg("-C")
            .arg(&cache_path)
            .args(["reset", "--hard", "--quiet", "crosslink/hub"])
            .status()
            .await;
        if let Ok(s) = status {
            if !s.success() {
                tracing::warn!(
                    "hub-cache reset failed at {}: status {s}",
                    cache_path.display()
                );
            }
        }
        return;
    }

    let status = Command::new("git")
        .arg("-C")
        .arg(clone_path)
        .args([
            "worktree",
            "add",
            "--force",
            "--quiet",
            cache_path.to_string_lossy().as_ref(),
            "crosslink/hub",
        ])
        .status()
        .await;
    match status {
        Ok(s) if s.success() => {}
        Ok(s) => tracing::warn!(
            "`git worktree add` for hub-cache exited with {s} in {}",
            clone_path.display()
        ),
        Err(e) => tracing::warn!(
            "`git worktree add` for hub-cache failed in {}: {e}",
            clone_path.display()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command as StdCommand;
    use tempfile::tempdir;

    fn make_fake_clone(hub_files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempdir().unwrap();
        let path = dir.path();

        StdCommand::new("git")
            .arg("-C")
            .arg(path)
            .args(["init", "-q", "-b", "crosslink/hub"])
            .status()
            .unwrap();
        StdCommand::new("git")
            .arg("-C")
            .arg(path)
            .args(["config", "user.email", "test@test.local"])
            .status()
            .unwrap();
        StdCommand::new("git")
            .arg("-C")
            .arg(path)
            .args(["config", "user.name", "Test"])
            .status()
            .unwrap();

        for (rel, contents) in hub_files {
            let full = path.join(rel);
            if let Some(parent) = full.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&full, contents).unwrap();
        }

        StdCommand::new("git")
            .arg("-C")
            .arg(path)
            .args(["add", "-A"])
            .status()
            .unwrap();
        StdCommand::new("git")
            .arg("-C")
            .arg(path)
            .args(["commit", "-q", "-m", "test fixture"])
            .status()
            .unwrap();

        dir
    }

    fn seed_project(db_path: &Path, slug: &str, clone_path: &Path) -> i64 {
        let db = DashboardDb::open(db_path).unwrap();
        db.conn
            .execute(
                "INSERT INTO projects (slug, clone_path, default_branch, status, added_at)
                 VALUES (?1, ?2, 'main', 'active', ?3)",
                rusqlite::params![
                    slug,
                    clone_path.to_string_lossy().as_ref(),
                    Utc::now().to_rfc3339()
                ],
            )
            .unwrap();
        db.conn.last_insert_rowid()
    }

    #[tokio::test]
    async fn test_poll_project_populates_state_from_empty_hub() {
        let home = tempdir().unwrap();
        let db_path = home.path().join("dashboard.db");
        DashboardDb::open(&db_path).unwrap();
        let clone = make_fake_clone(&[("README.md", "hi")]);

        let project_id = seed_project(&db_path, "owner/repo", clone.path());
        let project = load_active_projects(&db_path).unwrap();
        let project = project.into_iter().find(|p| p.id == project_id).unwrap();

        poll_project(&db_path, &project).await.unwrap();

        let db = DashboardDb::open(&db_path).unwrap();
        let (open, overdue, blocked, agents, stale): (i64, i64, i64, i64, i64) = db
            .conn
            .query_row(
                "SELECT open_issues, overdue_issues, blocked_issues, active_agents, stale_locks
                 FROM project_state WHERE project_id = ?1",
                [project_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!(open, 0);
        assert_eq!(overdue, 0);
        assert_eq!(blocked, 0);
        assert_eq!(agents, 0);
        assert_eq!(stale, 0);

        let status: String = db
            .conn
            .query_row(
                "SELECT status FROM projects WHERE id = ?1",
                [project_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "active");
    }

    #[tokio::test]
    async fn test_poll_project_marks_unreachable_on_missing_clone() {
        let home = tempdir().unwrap();
        let db_path = home.path().join("dashboard.db");
        DashboardDb::open(&db_path).unwrap();

        let gone = home.path().join("no-such-clone");
        let project_id = seed_project(&db_path, "owner/gone", &gone);
        let project = load_active_projects(&db_path)
            .unwrap()
            .into_iter()
            .find(|p| p.id == project_id)
            .unwrap();
        assert_eq!(project.status, "active", "seeded active");

        poll_project(&db_path, &project).await.unwrap();

        let db = DashboardDb::open(&db_path).unwrap();
        let status: String = db
            .conn
            .query_row(
                "SELECT status FROM projects WHERE id = ?1",
                [project_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            status, "error",
            "missing clone must mark the project errored"
        );
    }

    #[tokio::test]
    async fn test_poll_project_counts_open_issue() {
        let home = tempdir().unwrap();
        let db_path = home.path().join("dashboard.db");
        DashboardDb::open(&db_path).unwrap();

        let issue_json = serde_json::json!({
            "uuid": "00000000-0000-0000-0000-000000000001",
            "display_id": 1,
            "title": "t",
            "status": "open",
            "priority": "medium",
            "created_by": "a",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z"
        });
        let clone = make_fake_clone(&[(
            "issues/00000000-0000-0000-0000-000000000001/issue.json",
            &issue_json.to_string(),
        )]);

        let project_id = seed_project(&db_path, "owner/repo", clone.path());
        let project = load_active_projects(&db_path)
            .unwrap()
            .into_iter()
            .find(|p| p.id == project_id)
            .unwrap();

        poll_project(&db_path, &project).await.unwrap();

        let db = DashboardDb::open(&db_path).unwrap();
        let open: i64 = db
            .conn
            .query_row(
                "SELECT open_issues FROM project_state WHERE project_id = ?1",
                [project_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(open, 1);
    }

    #[tokio::test]
    async fn test_poll_all_projects_tolerates_one_broken() {
        let home = tempdir().unwrap();
        let db_path = home.path().join("dashboard.db");
        DashboardDb::open(&db_path).unwrap();

        let clone = make_fake_clone(&[("README.md", "hi")]);
        seed_project(&db_path, "good/one", clone.path());

        let db = DashboardDb::open(&db_path).unwrap();
        db.conn
            .execute(
                "INSERT INTO projects (slug, clone_path, default_branch, status, added_at)
                 VALUES ('broken/one', '/nonexistent/path', 'main', 'active', ?1)",
                [Utc::now().to_rfc3339()],
            )
            .unwrap();
        drop(db);

        poll_all_projects(&db_path, None).await.unwrap();

        let db = DashboardDb::open(&db_path).unwrap();
        let count: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM project_state", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            count, 1,
            "only the healthy project should have project_state"
        );
    }

    #[tokio::test]
    async fn test_run_cancels_cleanly() {
        let home = tempdir().unwrap();
        let db_path = home.path().join("dashboard.db");
        DashboardDb::open(&db_path).unwrap();

        let cancel = CancellationToken::new();
        let handle = tokio::spawn({
            let cancel = cancel.clone();
            let path = db_path.clone();
            async move { run_with_tick(path, Duration::from_millis(50), cancel, None).await }
        });

        tokio::time::sleep(Duration::from_millis(120)).await;
        cancel.cancel();

        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("poll loop did not exit after cancel")
            .expect("poll loop task panicked");
    }

    #[tokio::test]
    async fn fetch_hub_fetches_v3_refs_and_skips_worktree() {
        let seed = tempdir().unwrap();
        let sp = seed.path();
        for args in [
            vec!["init", "-q", "-b", "crosslink/checkpoint"],
            vec!["config", "user.email", "test@test.local"],
            vec!["config", "user.name", "Test"],
            vec!["config", "commit.gpgsign", "false"],
        ] {
            StdCommand::new("git")
                .arg("-C")
                .arg(sp)
                .args(&args)
                .status()
                .unwrap();
        }
        let state =
            serde_json::to_vec_pretty(&crate::checkpoint::CheckpointState::default()).unwrap();
        fs::write(sp.join("state.json"), &state).unwrap();
        StdCommand::new("git")
            .arg("-C")
            .arg(sp)
            .args(["add", "-A"])
            .status()
            .unwrap();
        StdCommand::new("git")
            .arg("-C")
            .arg(sp)
            .args(["commit", "-q", "-m", "v3 checkpoint"])
            .status()
            .unwrap();
        StdCommand::new("git")
            .arg("-C")
            .arg(sp)
            .args(["branch", "crosslink/meta"])
            .status()
            .unwrap();

        let remote = tempdir().unwrap();
        StdCommand::new("git")
            .arg("-C")
            .arg(remote.path())
            .args(["init", "-q", "--bare", "-b", "main"])
            .status()
            .unwrap();
        StdCommand::new("git")
            .arg("-C")
            .arg(sp)
            .args(["remote", "add", "origin", remote.path().to_str().unwrap()])
            .status()
            .unwrap();
        StdCommand::new("git")
            .arg("-C")
            .arg(sp)
            .args([
                "push",
                "-q",
                "origin",
                "crosslink/checkpoint",
                "crosslink/meta",
            ])
            .status()
            .unwrap();

        let clone = tempdir().unwrap();
        let cp = clone.path().join("work");
        StdCommand::new("git")
            .args([
                "clone",
                "-q",
                remote.path().to_str().unwrap(),
                cp.to_str().unwrap(),
            ])
            .status()
            .unwrap();
        assert!(
            !crate::hub_v3::HubMode::resolve(&cp).is_v3(),
            "fresh clone has no local v3 refs yet"
        );

        fetch_hub(&cp).await.unwrap();

        assert!(
            crate::hub_v3::HubMode::resolve(&cp).is_v3(),
            "fetch must create local v3 refs"
        );

        assert!(
            !cp.join(".crosslink").join(".hub-cache").exists(),
            "v3 repos must not get a v2 hub-cache worktree"
        );
    }
}
