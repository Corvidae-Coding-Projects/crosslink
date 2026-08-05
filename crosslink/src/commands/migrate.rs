//! Migration commands for converting between local `SQLite` and shared JSON.
//!
//! - `migrate-to-shared`: Export all `SQLite` issues to JSON on the coordination branch.
//! - `migrate-from-shared`: Import JSON issues from the coordination branch into `SQLite`.

use anyhow::{bail, Result};
use chrono::Utc;
use std::collections::HashMap;
use std::path::Path;
use uuid::Uuid;

use crate::db::Database;
use crate::hydration::hydrate_to_sqlite;
use crate::identity::AgentConfig;
use crate::issue_file::{
    write_counters, write_issue_file, write_milestone_file, CommentEntry, Counters, IssueFile,
    MilestoneEntry,
};
use crate::sync::SyncManager;

/// `crosslink migrate-to-shared` — export local `SQLite` issues to shared JSON.
///
/// Reads all issues, comments, labels, dependencies, relations, milestones
/// from the local database and writes them as JSON files on the coordination branch.
pub fn to_shared(crosslink_dir: &Path, db: &Database) -> Result<()> {
    let agent = AgentConfig::load(crosslink_dir)?.ok_or_else(|| {
        anyhow::anyhow!("No agent configured. Run 'crosslink agent init <id>' first.")
    })?;

    let sync = SyncManager::new(crosslink_dir)?;
    sync.init_cache()?;
    sync.fetch()?;

    // GH#4 (second half): on a v3 hub the v2 body below writes worktree JSON
    // and counters the reduction never reads, then pushes the nonexistent
    // legacy `crosslink/hub` ref ("src refspec does not match any"). Route
    // the migration through the event log instead.
    if sync.hub_mode().is_v3() {
        return to_shared_v3(crosslink_dir, db, &sync);
    }

    let cache_dir = sync.cache_path().to_path_buf();
    let issues_dir = cache_dir.join("issues");
    let meta_dir = cache_dir.join("meta");
    std::fs::create_dir_all(&issues_dir)?;
    std::fs::create_dir_all(&meta_dir)?;

    // Check if there are already issue files on the coordination branch
    let existing_count = std::fs::read_dir(&issues_dir)?
        .filter_map(std::result::Result::ok)
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
        .count();
    if existing_count > 0 {
        bail!(
            "Coordination branch already has {existing_count} issue file(s). \
             Migration would overwrite them. Aborting.\n\
             Use 'crosslink migrate-from-shared' to import instead."
        );
    }

    // Load all issues from SQLite
    let issues = db.list_issues(Some("all"), None, None)?;
    if issues.is_empty() {
        println!("No issues to migrate.");
        return Ok(());
    }

    // Assign UUIDs: display_id → UUID mapping
    let mut id_to_uuid: HashMap<i64, Uuid> = HashMap::new();
    for issue in &issues {
        id_to_uuid.insert(issue.id, Uuid::new_v4());
    }

    // Load milestones and assign UUIDs
    let milestones = db.list_milestones(Some("all"))?;
    let mut milestone_id_to_uuid: HashMap<i64, Uuid> = HashMap::new();
    for ms in &milestones {
        milestone_id_to_uuid.insert(ms.id, Uuid::new_v4());
    }

    let mut max_comment_id: i64 = 0;
    let mut files_written = 0;

    // Convert each issue to an IssueFile and write JSON
    for issue in &issues {
        let uuid = id_to_uuid[&issue.id];

        // Resolve parent UUID
        let parent_uuid = issue
            .parent_id
            .and_then(|pid| id_to_uuid.get(&pid).copied());

        // Load associated data
        let labels = db.get_labels(issue.id)?;
        let comments = db.get_comments(issue.id)?;
        let blockers = db.get_blockers(issue.id)?;
        let related_issues = db.get_related_issues(issue.id)?;
        let milestone = db.get_issue_milestone(issue.id)?;

        // Convert comments
        let comment_entries: Vec<CommentEntry> = comments
            .iter()
            .map(|c| {
                if c.id > max_comment_id {
                    max_comment_id = c.id;
                }
                CommentEntry {
                    id: c.id,
                    author: agent.agent_id.clone(),
                    content: c.content.clone(),
                    created_at: c.created_at,
                    kind: "note".to_string(),
                    trigger_type: None,
                    intervention_context: None,
                    driver_key_fingerprint: None,
                    signed_by: None,
                    signature: None,
                }
            })
            .collect();

        // Convert blockers to UUIDs (skip if blocker not in our set)
        let blocker_uuids: Vec<Uuid> = blockers
            .iter()
            .filter_map(|bid| id_to_uuid.get(bid).copied())
            .collect();

        // Convert relations to UUIDs (single direction — only store if related_id > issue_id
        // to avoid duplicates; hydration handles bidirectional insertion)
        let related_uuids: Vec<Uuid> = related_issues
            .iter()
            .filter(|r| r.id > issue.id) // only store one direction
            .filter_map(|r| id_to_uuid.get(&r.id).copied())
            .collect();

        // Resolve milestone UUID
        let milestone_uuid = milestone
            .as_ref()
            .and_then(|m| milestone_id_to_uuid.get(&m.id).copied());

        let issue_file = IssueFile {
            uuid,
            display_id: Some(issue.id),
            title: issue.title.clone(),
            description: issue.description.clone(),
            status: issue.status,
            priority: issue.priority,
            parent_uuid,
            created_by: agent.agent_id.clone(),
            created_at: issue.created_at,
            updated_at: issue.updated_at,
            closed_at: issue.closed_at,
            scheduled_at: issue.scheduled_at,
            due_at: issue.due_at,
            labels,
            comments: comment_entries,
            blockers: blocker_uuids,
            related: related_uuids,
            milestone_uuid,
            time_entries: vec![],
        };

        let path = issues_dir.join(format!("{uuid}.json"));
        write_issue_file(&path, &issue_file)?;
        files_written += 1;
    }

    // Write counters.json
    let max_display_id = issues.iter().map(|i| i.id).max().unwrap_or(0);
    let max_milestone_id = milestones.iter().map(|m| m.id).max().unwrap_or(0);
    let counters = Counters {
        next_display_id: max_display_id + 1,
        next_comment_id: max_comment_id + 1,
        next_milestone_id: max_milestone_id + 1,
    };
    write_counters(&meta_dir.join("counters.json"), &counters)?;

    // Write per-file milestones to meta/milestones/{uuid}.json
    if !milestones.is_empty() {
        let milestones_dir = meta_dir.join("milestones");
        std::fs::create_dir_all(&milestones_dir)?;
        for ms in &milestones {
            let uuid = milestone_id_to_uuid[&ms.id];
            let entry = MilestoneEntry {
                uuid,
                display_id: ms.id,
                name: ms.name.clone(),
                description: ms.description.clone(),
                status: ms.status,
                created_at: ms.created_at,
                closed_at: ms.closed_at,
            };
            write_milestone_file(&milestones_dir.join(format!("{uuid}.json")), &entry)?;
        }
    }

    // Commit and push
    git_in_dir(&cache_dir, &["add", "issues/", "meta/"])?;
    let commit_msg = format!(
        "{}: migrate {} issues to shared at {}",
        agent.agent_id,
        files_written,
        Utc::now().format("%Y-%m-%dT%H:%M:%SZ")
    );
    git_in_dir(&cache_dir, &["commit", "-m", &commit_msg])?;

    // Best-effort push
    let remote = crate::sync::read_tracker_remote(crosslink_dir);
    match git_in_dir(&cache_dir, &["push", &remote, crate::sync::HUB_BRANCH]) {
        Ok(_) => println!("Pushed to remote."),
        Err(e) => {
            let err = e.to_string();
            if err.contains("Could not resolve host") || err.contains("Could not read from remote")
            {
                println!("Offline — committed locally, will push on next sync.");
            } else {
                tracing::warn!("push failed: {}. Committed locally.", err);
            }
        }
    }

    println!(
        "Migrated {} issue(s), {} milestone(s) to shared coordination branch.",
        files_written,
        milestones.len()
    );
    println!(
        "Next display ID: {}, next comment ID: {}",
        counters.next_display_id, counters.next_comment_id
    );

    Ok(())
}

/// `crosslink migrate-from-shared` — import shared JSON issues into local `SQLite`.
///
/// Fetches the coordination branch and hydrates all issues into the local database.
pub fn from_shared(crosslink_dir: &Path, db: &Database) -> Result<()> {
    let sync = SyncManager::new(crosslink_dir)?;
    sync.init_cache()?;
    sync.fetch()?;

    let cache_dir = sync.cache_path().to_path_buf();
    let issues_dir = cache_dir.join("issues");

    // Count issue files
    let issue_count = if issues_dir.exists() {
        std::fs::read_dir(&issues_dir)?
            .filter_map(std::result::Result::ok)
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
            .count()
    } else {
        0
    };

    if issue_count == 0 {
        println!("No issue files found on coordination branch.");
        return Ok(());
    }

    // Hydrate into SQLite
    let stats = hydrate_to_sqlite(&cache_dir, db)?;

    println!(
        "Imported from shared: {} issue(s), {} comment(s), {} dep(s), {} relation(s), {} milestone(s).",
        stats.issues, stats.comments, stats.dependencies, stats.relations, stats.milestones
    );

    Ok(())
}

/// `crosslink migrate-rename-branch` — rename crosslink/locks to crosslink/hub.
///
/// Runs the auto-migration and updates the `.crosslink/.gitignore` if needed.
pub fn rename_branch(crosslink_dir: &Path) -> Result<()> {
    let sync = SyncManager::new(crosslink_dir)?;
    let migrated = sync.migrate_from_locks_branch()?;
    if migrated {
        println!("Migrated crosslink/locks -> crosslink/hub");

        // Update .gitignore
        let gitignore_path = crosslink_dir.join(".gitignore");
        if gitignore_path.exists() {
            let content = std::fs::read_to_string(&gitignore_path)?;
            let updated = content.replace(".locks-cache/", ".hub-cache/");
            if content != updated {
                std::fs::write(&gitignore_path, updated)?;
                println!("Updated .crosslink/.gitignore");
            }
        }

        // Initialize the new cache worktree
        sync.init_cache()?;
        println!("Cache initialized at .crosslink/.hub-cache/");
    } else {
        println!("No migration needed (already using crosslink/hub).");
    }
    Ok(())
}

/// Run a git command in the specified directory.
fn git_in_dir(dir: &Path, args: &[&str]) -> Result<std::process::Output> {
    let output = std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .with_context(|| format!("Failed to run git {args:?}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git {args:?} failed: {stderr}");
    }
    Ok(output)
}

use anyhow::Context;

/// V3 analogue of `to_shared` (GH#4): promote SQLite-only issues — rows whose
/// uuid the reduced state does not know — through the event log in a single
/// commit, via the same batch path `crosslink import` uses. Idempotent:
/// already-promoted issues are skipped, so it can be re-run to sweep up rows
/// created before the hub was established.
fn to_shared_v3(crosslink_dir: &Path, db: &Database, sync: &SyncManager) -> Result<()> {
    let source = crate::hub_source::RefHubSource::new(sync.cache_path())
        .map_err(|e| anyhow::anyhow!("v3: construct RefHubSource for to-shared: {e}"))?;
    let outcome = crate::compaction::reduce(&source)
        .map_err(|e| anyhow::anyhow!("v3: reduce for to-shared: {e}"))?;
    let state = outcome.state;

    let hub_uuids: std::collections::HashSet<String> =
        state.issues.keys().map(Uuid::to_string).collect();
    let used_ids: std::collections::HashSet<i64> =
        state.issues.values().filter_map(|i| i.display_id).collect();

    let specs = specs_from_db(db, &hub_uuids, &used_ids)?;
    if specs.is_empty() {
        println!("No SQLite-only issues to migrate - the hub already covers the local database.");
        return Ok(());
    }

    let writer = crate::shared_writer::SharedWriter::new(crosslink_dir)?.ok_or_else(|| {
        anyhow::anyhow!(
            "v3 migration needs an initialized shared writer (agent identity and hub cache)"
        )
    })?;
    let assigned = writer.import_issues(db, &specs)?;
    println!(
        "Migrated {} issue(s) to the v3 hub event log.",
        assigned.len()
    );
    Ok(())
}

/// Build [`crate::shared_writer::ImportedIssueSpec`]s for every `SQLite` issue
/// the hub does not know. Existing uuids are preserved (so hydration replaces
/// the local row in place instead of duplicating it); rows without a uuid get
/// a fresh one. Positive local ids are carried into the reduction when free,
/// preserving local numbering.
fn specs_from_db(
    db: &Database,
    hub_uuids: &std::collections::HashSet<String>,
    used_ids: &std::collections::HashSet<i64>,
) -> Result<Vec<crate::shared_writer::ImportedIssueSpec>> {
    struct Row {
        id: i64,
        uuid: Option<String>,
        title: String,
        description: Option<String>,
        priority: String,
        parent_id: Option<i64>,
        status: String,
    }

    let rows: Vec<Row> = db
        .conn
        .prepare(
            "SELECT id, uuid, title, description, priority, parent_id, status \
             FROM issues ORDER BY id",
        )?
        .query_map([], |row| {
            Ok(Row {
                id: row.get(0)?,
                uuid: row.get(1)?,
                title: row.get(2)?,
                description: row.get(3)?,
                priority: row.get(4)?,
                parent_id: row.get(5)?,
                status: row.get(6)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    // id -> uuid over ALL rows (hub-backed parents/blockers resolve too).
    let mut id_to_uuid: HashMap<i64, Uuid> = HashMap::new();
    for row in &rows {
        let uuid = row
            .uuid
            .as_deref()
            .and_then(|u| u.parse::<Uuid>().ok())
            .unwrap_or_else(Uuid::new_v4);
        id_to_uuid.insert(row.id, uuid);
    }

    let mut specs = Vec::new();
    for row in &rows {
        let uuid = id_to_uuid[&row.id];
        if hub_uuids.contains(&uuid.to_string()) {
            continue; // already promoted
        }

        let comments = db
            .conn
            .prepare(
                "SELECT author, content, created_at, kind FROM comments \
                 WHERE issue_id = ?1 ORDER BY id",
            )?
            .query_map([row.id], |r| {
                Ok((
                    r.get::<_, Option<String>>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?
            .into_iter()
            .map(
                |(author, content, created_at, kind)| crate::shared_writer::ImportedCommentSpec {
                    author: author.unwrap_or_else(|| "migrate".to_string()),
                    content,
                    created_at: created_at
                        .parse::<chrono::DateTime<Utc>>()
                        .unwrap_or_else(|_| Utc::now()),
                    kind,
                },
            )
            .collect();

        let blockers: Vec<Uuid> = db
            .conn
            .prepare("SELECT blocker_id FROM dependencies WHERE blocked_id = ?1")?
            .query_map([row.id], |r| r.get::<_, i64>(0))?
            .collect::<std::result::Result<Vec<i64>, _>>()?
            .into_iter()
            .filter_map(|bid| id_to_uuid.get(&bid).copied())
            .collect();

        specs.push(crate::shared_writer::ImportedIssueSpec {
            uuid,
            title: row.title.clone(),
            description: row.description.clone(),
            priority: row.priority.clone(),
            parent_uuid: row.parent_id.and_then(|pid| id_to_uuid.get(&pid).copied()),
            closed: row.status == "closed",
            labels: db.get_labels(row.id)?,
            comments,
            blockers,
            display_id: (row.id > 0 && !used_ids.contains(&row.id)).then_some(row.id),
        });
    }
    Ok(specs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn setup_test_db() -> (Database, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = Database::open(&db_path).unwrap();
        (db, dir)
    }

    #[test]
    fn test_to_shared_no_agent() {
        let (db, dir) = setup_test_db();
        let crosslink_dir = dir.path().join(".crosslink");
        std::fs::create_dir_all(&crosslink_dir).unwrap();

        let result = to_shared(&crosslink_dir, &db);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("No agent configured"));
    }

    #[test]
    fn test_from_shared_no_sync() {
        let (db, dir) = setup_test_db();
        let crosslink_dir = dir.path().join(".crosslink");
        std::fs::create_dir_all(&crosslink_dir).unwrap();

        // Without sync manager setup, from_shared should fail gracefully
        let result = from_shared(&crosslink_dir, &db);
        assert!(result.is_err());
    }

    #[test]
    fn test_issue_to_issuefile_conversion() {
        // Test the core conversion logic without git
        let (db, _dir) = setup_test_db();

        let id1 = db
            .create_issue("Bug fix", Some("Fix the bug"), "high")
            .unwrap();
        let id2 = db.create_issue("Feature", None, "medium").unwrap();
        db.add_comment(id1, "First comment", "note").unwrap();
        db.add_label(id1, "bug").unwrap();
        db.add_dependency(id1, id2).unwrap(); // id1 blocked by id2

        let issues = db.list_issues(Some("all"), None, None).unwrap();
        assert_eq!(issues.len(), 2);

        // Simulate UUID assignment
        let mut id_to_uuid: HashMap<i64, Uuid> = HashMap::new();
        for issue in &issues {
            id_to_uuid.insert(issue.id, Uuid::new_v4());
        }

        // Convert issue 1
        let issue = issues.iter().find(|i| i.id == id1).unwrap();
        let labels = db.get_labels(issue.id).unwrap();
        let comments = db.get_comments(issue.id).unwrap();
        let blockers = db.get_blockers(issue.id).unwrap();

        assert_eq!(labels, vec!["bug"]);
        assert_eq!(comments.len(), 1);
        assert_eq!(blockers, vec![id2]);

        let blocker_uuids: Vec<Uuid> = blockers
            .iter()
            .filter_map(|bid| id_to_uuid.get(bid).copied())
            .collect();
        assert_eq!(blocker_uuids.len(), 1);
        assert_eq!(blocker_uuids[0], id_to_uuid[&id2]);

        let issue_file = IssueFile {
            uuid: id_to_uuid[&id1],
            display_id: Some(id1),
            title: issue.title.clone(),
            description: issue.description.clone(),
            status: issue.status,
            priority: issue.priority,
            parent_uuid: None,
            created_by: "test-agent".to_string(),
            created_at: issue.created_at,
            updated_at: issue.updated_at,
            closed_at: issue.closed_at,
            scheduled_at: issue.scheduled_at,
            due_at: issue.due_at,
            labels,
            comments: comments
                .iter()
                .map(|c| CommentEntry {
                    id: c.id,
                    author: "test-agent".to_string(),
                    content: c.content.clone(),
                    created_at: c.created_at,
                    kind: "note".to_string(),
                    trigger_type: None,
                    intervention_context: None,
                    driver_key_fingerprint: None,
                    signed_by: None,
                    signature: None,
                })
                .collect(),
            blockers: blocker_uuids,
            related: vec![],
            milestone_uuid: None,
            time_entries: vec![],
        };

        // Verify the conversion
        assert_eq!(issue_file.title, "Bug fix");
        assert_eq!(issue_file.description, Some("Fix the bug".to_string()));
        assert_eq!(issue_file.priority, "high");
        assert_eq!(issue_file.labels, vec!["bug"]);
        assert_eq!(issue_file.comments.len(), 1);
        assert_eq!(issue_file.blockers.len(), 1);

        // JSON roundtrip
        let json = serde_json::to_string_pretty(&issue_file).unwrap();
        let parsed: IssueFile = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.uuid, issue_file.uuid);
        assert_eq!(parsed.display_id, issue_file.display_id);
        assert_eq!(parsed.blockers, issue_file.blockers);
    }

    #[test]
    fn test_write_issue_files_to_dir() {
        let dir = tempdir().unwrap();
        let issues_dir = dir.path().join("issues");
        std::fs::create_dir_all(&issues_dir).unwrap();

        let uuid = Uuid::new_v4();
        let now = Utc::now();
        let issue = IssueFile {
            uuid,
            display_id: Some(1),
            title: "Test issue".to_string(),
            description: None,
            status: crate::models::IssueStatus::Open,
            priority: crate::models::Priority::Medium,
            parent_uuid: None,
            created_by: "agent".to_string(),
            created_at: now,
            updated_at: now,
            closed_at: None,
            scheduled_at: None,
            due_at: None,
            labels: vec!["test".to_string()],
            comments: vec![],
            blockers: vec![],
            related: vec![],
            milestone_uuid: None,
            time_entries: vec![],
        };

        let path = issues_dir.join(format!("{uuid}.json"));
        write_issue_file(&path, &issue).unwrap();

        // Verify file exists and is valid
        let loaded = crate::issue_file::read_issue_file(&path).unwrap();
        assert_eq!(loaded.uuid, uuid);
        assert_eq!(loaded.title, "Test issue");
        assert_eq!(loaded.labels, vec!["test"]);
    }

    #[test]
    fn test_counters_from_issues() {
        let (db, _dir) = setup_test_db();

        db.create_issue("Issue 1", None, "medium").unwrap();
        db.create_issue("Issue 2", None, "high").unwrap();
        let id3 = db.create_issue("Issue 3", None, "low").unwrap();
        db.add_comment(id3, "comment A", "note").unwrap();
        let cid = db.add_comment(id3, "comment B", "note").unwrap();

        let issues = db.list_issues(Some("all"), None, None).unwrap();
        let max_display_id = issues.iter().map(|i| i.id).max().unwrap_or(0);

        let mut max_comment_id: i64 = 0;
        for issue in &issues {
            let comments = db.get_comments(issue.id).unwrap();
            for c in &comments {
                if c.id > max_comment_id {
                    max_comment_id = c.id;
                }
            }
        }

        assert_eq!(max_display_id, id3);
        assert_eq!(max_comment_id, cid);

        let counters = Counters {
            next_display_id: max_display_id + 1,
            next_comment_id: max_comment_id + 1,
            next_milestone_id: 1,
        };
        assert_eq!(counters.next_display_id, id3 + 1);
        assert_eq!(counters.next_comment_id, cid + 1);
    }

    #[test]
    fn test_milestone_migration() {
        let (db, _dir) = setup_test_db();

        let ms_id = db.create_milestone("v1.0", Some("First release")).unwrap();
        let issue_id = db.create_issue("Feature A", None, "high").unwrap();
        db.add_issue_to_milestone(ms_id, issue_id).unwrap();

        let milestones = db.list_milestones(Some("all")).unwrap();
        assert_eq!(milestones.len(), 1);
        assert_eq!(milestones[0].name, "v1.0");

        let ms_issues = db.get_milestone_issues(ms_id).unwrap();
        assert_eq!(ms_issues.len(), 1);
        assert_eq!(ms_issues[0].id, issue_id);

        // Convert to MilestoneEntry
        let uuid = Uuid::new_v4();
        let ms = &milestones[0];
        let entry = MilestoneEntry {
            uuid,
            display_id: ms.id,
            name: ms.name.clone(),
            description: ms.description.clone(),
            status: ms.status,
            created_at: ms.created_at,
            closed_at: ms.closed_at,
        };
        assert_eq!(entry.name, "v1.0");
        assert_eq!(entry.description, Some("First release".to_string()));
    }

    #[test]
    fn test_relation_single_direction() {
        let (db, _dir) = setup_test_db();

        let id1 = db.create_issue("Issue 1", None, "medium").unwrap();
        let id2 = db.create_issue("Issue 2", None, "medium").unwrap();
        let id3 = db.create_issue("Issue 3", None, "medium").unwrap();

        db.add_relation(id1, id2).unwrap();
        db.add_relation(id1, id3).unwrap();

        let mut id_to_uuid: HashMap<i64, Uuid> = HashMap::new();
        id_to_uuid.insert(id1, Uuid::new_v4());
        id_to_uuid.insert(id2, Uuid::new_v4());
        id_to_uuid.insert(id3, Uuid::new_v4());

        // For issue 1, related issues are 2 and 3
        let related = db.get_related_issues(id1).unwrap();
        assert_eq!(related.len(), 2);

        // Only store relations where related_id > issue_id
        let related_uuid_count = related
            .iter()
            .filter(|r| r.id > id1)
            .filter_map(|r| id_to_uuid.get(&r.id).copied())
            .count();
        assert_eq!(related_uuid_count, 2);

        // For issue 2, related issue is 1 (but 1 < 2 so we skip it)
        let related2 = db.get_related_issues(id2).unwrap();
        let related2_uuid_count = related2
            .iter()
            .filter(|r| r.id > id2)
            .filter_map(|r| id_to_uuid.get(&r.id).copied())
            .count();
        // id1 < id2, so no stored relations from id2's perspective
        // id3 > id2, but id2 isn't directly related to id3
        assert_eq!(related2_uuid_count, 0);
    }

    #[test]
    fn test_subissue_parent_uuid() {
        let (db, _dir) = setup_test_db();

        let parent = db.create_issue("Parent", None, "medium").unwrap();
        let child = db.create_subissue(parent, "Child", None, "medium").unwrap();

        let mut id_to_uuid: HashMap<i64, Uuid> = HashMap::new();
        id_to_uuid.insert(parent, Uuid::new_v4());
        id_to_uuid.insert(child, Uuid::new_v4());

        let child_issue = db.get_issue(child).unwrap().unwrap();
        assert_eq!(child_issue.parent_id, Some(parent));

        let parent_uuid = child_issue
            .parent_id
            .and_then(|pid| id_to_uuid.get(&pid).copied());
        assert!(parent_uuid.is_some());
        assert_eq!(parent_uuid.unwrap(), id_to_uuid[&parent]);
    }
}
