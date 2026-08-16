use anyhow::Result;
use chrono::Utc;
use rusqlite::params;
use std::collections::HashMap;

use super::alerts::DerivedAlert;
use super::db::DashboardDb;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SyncStats {
    pub opened: usize,
    pub resolved: usize,
    pub unchanged: usize,
    pub opened_alerts: Vec<DerivedAlert>,
}

pub fn sync_alerts_for_project(
    db: &DashboardDb,
    project_id: i64,
    derived: &[DerivedAlert],
) -> Result<SyncStats> {
    let now = Utc::now().to_rfc3339();

    let tx = db.conn.unchecked_transaction()?;

    let mut open_rows: HashMap<(String, String), i64> = HashMap::new();
    {
        let mut stmt = tx.prepare(
            "SELECT id, kind, COALESCE(subject_ref, '')
             FROM alerts
             WHERE project_id = ?1 AND resolved_at IS NULL",
        )?;
        let rows = stmt.query_map([project_id], |row| {
            let id: i64 = row.get(0)?;
            let kind: String = row.get(1)?;
            let subject: String = row.get(2)?;
            Ok((id, kind, subject))
        })?;
        for row in rows {
            let (id, kind, subject) = row?;
            open_rows.insert((kind, subject), id);
        }
    }

    let derived_keys: HashMap<(String, String), &DerivedAlert> = derived
        .iter()
        .map(|a| ((a.kind.to_string(), a.subject_ref.clone()), a))
        .collect();

    let mut stats = SyncStats::default();

    for (key, alert) in &derived_keys {
        if open_rows.contains_key(key) {
            stats.unchanged += 1;
            continue;
        }
        tx.execute(
            "INSERT INTO alerts
               (project_id, kind, severity, subject_ref, detail, opened_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                project_id,
                alert.kind,
                alert.severity.as_str(),
                alert.subject_ref,
                alert.detail,
                now,
            ],
        )?;
        stats.opened += 1;
        stats.opened_alerts.push((*alert).clone());
    }

    for (key, row_id) in &open_rows {
        if derived_keys.contains_key(key) {
            continue;
        }
        tx.execute(
            "UPDATE alerts SET resolved_at = ?1 WHERE id = ?2",
            params![now, row_id],
        )?;
        stats.resolved += 1;
    }

    tx.commit()?;
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dashboard::alerts::Severity;
    use tempfile::tempdir;

    fn open_temp_db() -> (tempfile::TempDir, DashboardDb, i64) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("dashboard.db");
        let db = DashboardDb::open(&path).unwrap();
        db.conn
            .execute(
                "INSERT INTO projects (slug, clone_path, default_branch, status, added_at)
                 VALUES ('owner/repo', '/tmp/x', 'main', 'active', '2026-04-20T00:00:00Z')",
                [],
            )
            .unwrap();
        let project_id = db.conn.last_insert_rowid();
        (dir, db, project_id)
    }

    fn mk(kind: &'static str, subject: &str, severity: Severity) -> DerivedAlert {
        DerivedAlert {
            kind,
            severity,
            subject_ref: subject.to_string(),
            detail: "test".into(),
        }
    }

    fn count_open(db: &DashboardDb, project_id: i64) -> i64 {
        db.conn
            .query_row(
                "SELECT COUNT(*) FROM alerts
                 WHERE project_id = ?1 AND resolved_at IS NULL",
                [project_id],
                |r| r.get(0),
            )
            .unwrap()
    }

    fn count_resolved(db: &DashboardDb, project_id: i64) -> i64 {
        db.conn
            .query_row(
                "SELECT COUNT(*) FROM alerts
                 WHERE project_id = ?1 AND resolved_at IS NOT NULL",
                [project_id],
                |r| r.get(0),
            )
            .unwrap()
    }

    #[test]
    fn test_sync_opens_new_alerts() {
        let (_dir, db, pid) = open_temp_db();
        let derived = vec![
            mk("stale_lock", "lock:1", Severity::Warning),
            mk("overdue_issue", "issue:5", Severity::Warning),
        ];
        let stats = sync_alerts_for_project(&db, pid, &derived).unwrap();
        assert_eq!(stats.opened, 2);
        assert_eq!(stats.resolved, 0);
        assert_eq!(count_open(&db, pid), 2);

        assert_eq!(stats.opened_alerts.len(), 2);
        let kinds: Vec<_> = stats.opened_alerts.iter().map(|a| a.kind).collect();
        assert!(kinds.contains(&"stale_lock"));
        assert!(kinds.contains(&"overdue_issue"));
    }

    #[test]
    fn test_sync_opened_alerts_only_contains_fires() {
        let (_dir, db, pid) = open_temp_db();

        sync_alerts_for_project(&db, pid, &[mk("stale_lock", "lock:1", Severity::Warning)])
            .unwrap();

        let stats = sync_alerts_for_project(
            &db,
            pid,
            &[
                mk("stale_lock", "lock:1", Severity::Warning),
                mk("overdue_issue", "issue:5", Severity::Warning),
            ],
        )
        .unwrap();
        assert_eq!(stats.opened_alerts.len(), 1);
        assert_eq!(stats.opened_alerts[0].kind, "overdue_issue");
    }

    #[test]
    fn test_sync_is_idempotent() {
        let (_dir, db, pid) = open_temp_db();
        let derived = vec![mk("stale_lock", "lock:1", Severity::Warning)];

        let stats1 = sync_alerts_for_project(&db, pid, &derived).unwrap();
        assert_eq!(stats1.opened, 1);

        let stats2 = sync_alerts_for_project(&db, pid, &derived).unwrap();
        assert_eq!(stats2.opened, 0);
        assert_eq!(stats2.unchanged, 1);

        assert_eq!(count_open(&db, pid), 1);
        assert_eq!(count_resolved(&db, pid), 0);
    }

    #[test]
    fn test_sync_resolves_alerts_no_longer_derived() {
        let (_dir, db, pid) = open_temp_db();
        sync_alerts_for_project(
            &db,
            pid,
            &[
                mk("stale_lock", "lock:1", Severity::Warning),
                mk("stale_lock", "lock:2", Severity::Warning),
            ],
        )
        .unwrap();
        assert_eq!(count_open(&db, pid), 2);

        let stats =
            sync_alerts_for_project(&db, pid, &[mk("stale_lock", "lock:2", Severity::Warning)])
                .unwrap();
        assert_eq!(stats.resolved, 1);
        assert_eq!(count_open(&db, pid), 1);
        assert_eq!(count_resolved(&db, pid), 1);
    }

    #[test]
    fn test_sync_resolves_and_opens_in_same_tick() {
        let (_dir, db, pid) = open_temp_db();

        sync_alerts_for_project(
            &db,
            pid,
            &[
                mk("stale_lock", "lock:1", Severity::Warning),
                mk("overdue_issue", "issue:5", Severity::Warning),
            ],
        )
        .unwrap();

        let stats = sync_alerts_for_project(
            &db,
            pid,
            &[
                mk("overdue_issue", "issue:5", Severity::Warning),
                mk("overdue_issue", "issue:7", Severity::Warning),
            ],
        )
        .unwrap();
        assert_eq!(stats.opened, 1);
        assert_eq!(stats.resolved, 1);
        assert_eq!(stats.unchanged, 1);
    }

    #[test]
    fn test_different_projects_isolated() {
        let (_dir, db, pid_a) = open_temp_db();
        db.conn
            .execute(
                "INSERT INTO projects (slug, clone_path, default_branch, status, added_at)
                 VALUES ('owner/other', '/tmp/y', 'main', 'active', '2026-04-20T00:00:00Z')",
                [],
            )
            .unwrap();
        let pid_b = db.conn.last_insert_rowid();

        sync_alerts_for_project(&db, pid_a, &[mk("stale_lock", "lock:1", Severity::Warning)])
            .unwrap();
        sync_alerts_for_project(&db, pid_b, &[]).unwrap();

        assert_eq!(count_open(&db, pid_a), 1);
        assert_eq!(count_open(&db, pid_b), 0);
    }
}
