use anyhow::{Context, Result};
use std::path::Path;

use crate::application::CommandService;
use crate::db::Database;
use crate::hydration::hydrate_to_sqlite;

#[derive(Debug, Default, Clone)]
pub struct HydrationDriftReport {
    pub sqlite_only_issues: Vec<i64>,

    pub sqlite_only_labels: Vec<(i64, String)>,

    pub sqlite_only_dependencies: Vec<(i64, i64)>,

    pub sqlite_only_relations: Vec<(i64, i64)>,

    pub sqlite_only_milestone_issues: Vec<(i64, i64)>,

    pub sqlite_only_comments: Vec<i64>,

    pub sqlite_only_time_entries: Vec<i64>,
}

impl HydrationDriftReport {
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.sqlite_only_issues.is_empty()
            && self.sqlite_only_labels.is_empty()
            && self.sqlite_only_dependencies.is_empty()
            && self.sqlite_only_relations.is_empty()
            && self.sqlite_only_milestone_issues.is_empty()
            && self.sqlite_only_comments.is_empty()
            && self.sqlite_only_time_entries.is_empty()
    }

    #[must_use]
    pub const fn has_unrecoverable_loss(&self) -> bool {
        !self.sqlite_only_comments.is_empty() || !self.sqlite_only_time_entries.is_empty()
    }

    #[allow(dead_code)]
    #[must_use]
    pub const fn is_fully_re_emittable(&self) -> bool {
        !self.is_empty()
            && self.sqlite_only_comments.is_empty()
            && self.sqlite_only_time_entries.is_empty()
            && self.sqlite_only_issues.is_empty()
    }

    #[must_use]
    pub fn summary(&self) -> String {
        if self.is_empty() {
            return String::new();
        }
        let mut parts: Vec<String> = Vec::new();
        let push = |parts: &mut Vec<String>, label: &str, n: usize| {
            if n > 0 {
                parts.push(format!("{n} sqlite-only {label}"));
            }
        };
        push(&mut parts, "issue(s)", self.sqlite_only_issues.len());
        push(&mut parts, "label(s)", self.sqlite_only_labels.len());
        push(
            &mut parts,
            "dependency(ies)",
            self.sqlite_only_dependencies.len(),
        );
        push(&mut parts, "relation(s)", self.sqlite_only_relations.len());
        push(
            &mut parts,
            "milestone assignment(s)",
            self.sqlite_only_milestone_issues.len(),
        );
        push(&mut parts, "comment(s)", self.sqlite_only_comments.len());
        push(
            &mut parts,
            "time entry(ies)",
            self.sqlite_only_time_entries.len(),
        );
        parts.join(", ")
    }
}

pub fn detect(
    cache_dir: &Path,
    main_db: &Database,
    v3_state: Option<&crate::checkpoint::CheckpointState>,
) -> Result<HydrationDriftReport> {
    let temp_dir = tempfile::tempdir().context("create temp dir for drift detection")?;
    let temp_db_path = temp_dir.path().join("hydrated-view.sqlite");
    {
        let temp_db =
            Database::open(&temp_db_path).context("open temp drift-detection database")?;
        if let Some(state) = v3_state {
            crate::hydration::hydrate_from_state(state, &temp_db)
                .context("hydrate reduced state into temp database for drift detection")?;
        } else {
            hydrate_to_sqlite(cache_dir, &temp_db)
                .context("hydrate JSON into temp database for drift detection")?;
        }
    }

    let escaped = temp_db_path.to_string_lossy().replace('\'', "''");
    main_db
        .conn
        .execute(&format!("ATTACH DATABASE '{escaped}' AS json_view"), [])
        .context("attach JSON-view database")?;

    let result = run_diff_queries(main_db);

    if let Err(e) = main_db.conn.execute("DETACH DATABASE json_view", []) {
        tracing::warn!("detach json_view database failed: {e}");
    }

    result
}

fn run_diff_queries(main_db: &Database) -> Result<HydrationDriftReport> {
    let mut report = HydrationDriftReport::default();

    {
        let mut stmt = main_db.conn.prepare(
            "SELECT id FROM main.issues \
             WHERE uuid IS NOT NULL \
               AND uuid NOT IN (SELECT uuid FROM json_view.issues WHERE uuid IS NOT NULL) \
             ORDER BY id",
        )?;
        report.sqlite_only_issues = stmt
            .query_map([], |row| row.get::<_, i64>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
    }

    {
        let mut stmt = main_db.conn.prepare(
            "SELECT issue_id, label FROM main.labels \
             WHERE issue_id IN (SELECT id FROM json_view.issues) \
               AND (issue_id, label) NOT IN \
                   (SELECT issue_id, label FROM json_view.labels) \
             ORDER BY issue_id, label",
        )?;
        report.sqlite_only_labels = stmt
            .query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
    }

    {
        let mut stmt = main_db.conn.prepare(
            "SELECT blocker_id, blocked_id FROM main.dependencies \
             WHERE blocker_id IN (SELECT id FROM json_view.issues) \
               AND blocked_id IN (SELECT id FROM json_view.issues) \
               AND (blocker_id, blocked_id) NOT IN \
                   (SELECT blocker_id, blocked_id FROM json_view.dependencies) \
             ORDER BY blocker_id, blocked_id",
        )?;
        report.sqlite_only_dependencies = stmt
            .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
    }

    {
        let mut stmt = main_db.conn.prepare(
            "SELECT DISTINCT \
                 MIN(issue_id_1, issue_id_2) AS lo, \
                 MAX(issue_id_1, issue_id_2) AS hi \
             FROM main.relations \
             WHERE issue_id_1 IN (SELECT id FROM json_view.issues) \
               AND issue_id_2 IN (SELECT id FROM json_view.issues) \
               AND (MIN(issue_id_1, issue_id_2), MAX(issue_id_1, issue_id_2)) NOT IN \
                   (SELECT MIN(issue_id_1, issue_id_2), MAX(issue_id_1, issue_id_2) \
                    FROM json_view.relations) \
             ORDER BY lo, hi",
        )?;
        report.sqlite_only_relations = stmt
            .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
    }

    {
        let mut stmt = main_db.conn.prepare(
            "SELECT milestone_id, issue_id FROM main.milestone_issues \
             WHERE issue_id IN (SELECT id FROM json_view.issues) \
               AND (milestone_id, issue_id) NOT IN \
                   (SELECT milestone_id, issue_id FROM json_view.milestone_issues) \
             ORDER BY milestone_id, issue_id",
        )?;
        report.sqlite_only_milestone_issues = stmt
            .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
    }

    {
        let mut stmt = main_db.conn.prepare(
            "SELECT id FROM main.comments \
             WHERE issue_id IN (SELECT id FROM json_view.issues) \
               AND uuid IS NOT NULL \
               AND uuid NOT IN \
                   (SELECT uuid FROM json_view.comments WHERE uuid IS NOT NULL) \
             ORDER BY id",
        )?;
        report.sqlite_only_comments = stmt
            .query_map([], |row| row.get::<_, i64>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
    }

    {
        let mut stmt = main_db.conn.prepare(
            "SELECT id FROM main.time_entries \
             WHERE issue_id IN (SELECT id FROM json_view.issues) \
             ORDER BY id",
        )?;
        report.sqlite_only_time_entries = stmt
            .query_map([], |row| row.get::<_, i64>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
    }

    Ok(report)
}

#[derive(Debug, Default, Clone)]
pub struct ReEmitStats {
    pub labels: usize,
    pub dependencies: usize,
    pub relations: usize,
    pub milestone_issues: usize,
}

impl ReEmitStats {
    #[must_use]
    pub const fn total(&self) -> usize {
        self.labels + self.dependencies + self.relations + self.milestone_issues
    }
}

pub fn re_emit(
    drift: &HydrationDriftReport,
    commands: &impl CommandService,
) -> Result<ReEmitStats> {
    let mut stats = ReEmitStats::default();

    for (issue_id, label) in &drift.sqlite_only_labels {
        if commands.add_label(*issue_id, label)? {
            stats.labels += 1;
        }
    }

    for (blocker_id, blocked_id) in &drift.sqlite_only_dependencies {
        if commands.add_dependency(*blocked_id, *blocker_id)? {
            stats.dependencies += 1;
        }
    }

    for (a, b) in &drift.sqlite_only_relations {
        if commands.add_relation(*a, *b)? {
            stats.relations += 1;
        }
    }

    if !drift.sqlite_only_milestone_issues.is_empty() {
        use std::collections::BTreeMap;
        let mut by_milestone: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
        for (m_id, i_id) in &drift.sqlite_only_milestone_issues {
            by_milestone.entry(*m_id).or_default().push(*i_id);
        }
        for (m_id, issue_ids) in by_milestone {
            commands.assign_milestone(m_id, &issue_ids)?;
            stats.milestone_issues += issue_ids.len();
        }
    }

    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::HUB_CACHE_DIR;

    fn setup_empty_cache(crosslink_dir: &std::path::Path) {
        let cache_dir = crosslink_dir.join(HUB_CACHE_DIR);
        std::fs::create_dir_all(cache_dir.join("issues")).unwrap();
        std::fs::create_dir_all(cache_dir.join("meta").join("milestones")).unwrap();
    }

    #[test]
    fn test_drift_report_summary_empty() {
        let report = HydrationDriftReport::default();
        assert!(report.is_empty());
        assert!(!report.has_unrecoverable_loss());
        assert!(!report.is_fully_re_emittable());
        assert_eq!(report.summary(), "");
    }

    #[test]
    fn test_drift_report_summary_with_labels() {
        let report = HydrationDriftReport {
            sqlite_only_labels: vec![(1, "bug".to_string())],
            ..Default::default()
        };
        assert!(!report.is_empty());
        assert!(!report.has_unrecoverable_loss());
        assert!(report.is_fully_re_emittable());
        assert_eq!(report.summary(), "1 sqlite-only label(s)");
    }

    #[test]
    fn test_drift_report_unrecoverable_when_comments_present() {
        let report = HydrationDriftReport {
            sqlite_only_comments: vec![42],
            ..Default::default()
        };
        assert!(!report.is_empty());
        assert!(report.has_unrecoverable_loss());
        assert!(!report.is_fully_re_emittable());
    }

    #[test]
    fn test_detect_no_drift_on_empty_state() {
        let dir = tempfile::tempdir().unwrap();
        let crosslink_dir = dir.path();
        setup_empty_cache(crosslink_dir);
        let db = Database::open(&dir.path().join("test.db")).unwrap();

        let report = detect(&crosslink_dir.join(HUB_CACHE_DIR), &db, None).unwrap();
        assert!(
            report.is_empty(),
            "empty SQLite + empty JSON should report no drift, got: {report:?}"
        );
    }

    fn make_issue(display_id: i64, title: &str) -> crate::issue_file::IssueFile {
        crate::issue_file::IssueFile {
            uuid: uuid::Uuid::new_v4(),
            display_id: Some(display_id),
            title: title.to_string(),
            description: None,
            status: crate::models::IssueStatus::Open,
            priority: crate::models::Priority::Medium,
            parent_uuid: None,
            created_by: "test-agent".to_string(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            closed_at: None,
            scheduled_at: None,
            due_at: None,
            labels: Vec::new(),
            comments: Vec::new(),
            blockers: Vec::new(),
            related: Vec::new(),
            milestone_uuid: None,
            time_entries: Vec::new(),
        }
    }

    #[test]
    fn test_detect_sqlite_only_dependency_on_json_known_issues() {
        use crate::issue_file::write_issue_file;

        let dir = tempfile::tempdir().unwrap();
        let crosslink_dir = dir.path();
        setup_empty_cache(crosslink_dir);
        let cache_dir = crosslink_dir.join(HUB_CACHE_DIR);

        let issue_a = make_issue(1, "first");
        let issue_b = make_issue(2, "second");
        write_issue_file(
            &cache_dir
                .join("issues")
                .join(format!("{}.json", issue_a.uuid)),
            &issue_a,
        )
        .unwrap();
        write_issue_file(
            &cache_dir
                .join("issues")
                .join(format!("{}.json", issue_b.uuid)),
            &issue_b,
        )
        .unwrap();

        let db = Database::open(&dir.path().join("test.db")).unwrap();
        hydrate_to_sqlite(&cache_dir, &db).unwrap();
        db.add_dependency(2, 1).unwrap();

        let report = detect(&cache_dir, &db, None).unwrap();

        assert_eq!(
            report.sqlite_only_dependencies,
            vec![(1, 2)],
            "the SQLite-only dependency must surface as drift"
        );
        assert!(
            report.is_fully_re_emittable(),
            "dependency-only drift must be re-emittable"
        );
        assert!(!report.has_unrecoverable_loss());
    }
}
