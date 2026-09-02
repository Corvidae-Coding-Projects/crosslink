use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::OptionalExtension;

use crate::db::{Database, HydratedIssue, HydratedMilestone};
use crate::issue_file::{
    read_all_issue_files, read_all_milestone_files, read_comment_files, read_layout_version,
    read_milestones_file, IssueFile,
};

fn dedup_issue_files(issues: &[IssueFile]) -> (Vec<&IssueFile>, Vec<&IssueFile>) {
    let mut by_display_id: HashMap<i64, Vec<&IssueFile>> = HashMap::new();
    let mut no_display_id = Vec::new();

    for issue in issues {
        match issue.display_id {
            Some(id) => by_display_id.entry(id).or_default().push(issue),
            None => no_display_id.push(issue),
        }
    }

    let mut keep = Vec::new();
    let mut dupes = Vec::new();

    for (_id, mut group) in by_display_id {
        group.sort_by_key(|b| std::cmp::Reverse(b.updated_at));
        keep.push(group[0]);
        dupes.extend(group.into_iter().skip(1));
    }

    keep.extend(no_display_id);
    (keep, dupes)
}

#[derive(Debug, Default)]
pub struct HydrationStats {
    pub issues: usize,
    pub comments: usize,
    pub dependencies: usize,
    pub relations: usize,
    pub milestones: usize,
}

struct SavedIssue {
    id: i64,
    uuid: String,
    title: String,
    description: Option<String>,
    status: String,
    priority: String,
    parent_uuid: Option<String>,
    created_by: Option<String>,
    created_at: String,
    updated_at: String,
    closed_at: Option<String>,
    scheduled_at: Option<String>,
    due_at: Option<String>,
}

struct SavedMilestone {
    id: i64,
    uuid: String,
    name: String,
    description: Option<String>,
    status: String,
    created_at: String,
    closed_at: Option<String>,
}

type SavedComment = (
    i64,
    i64,
    Option<String>,
    Option<String>,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
);

type SavedLocalTimeEntry = (i64, String, String, Option<String>, Option<i64>);
type SavedMilestoneLinks = (Vec<SavedMilestone>, Vec<(String, String)>);
type ExistingComment = (
    i64,
    Option<String>,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
);

struct SavedChildren {
    labels: Vec<(i64, String)>,
    comments: Vec<SavedComment>,
    deps: Vec<(String, String)>,
    relations: Vec<(String, String)>,
    milestone_issues: Vec<(String, String)>,
}

pub fn hydrate_to_sqlite(cache_dir: &Path, db: &Database) -> Result<HydrationStats> {
    let issues_dir = cache_dir.join("issues");
    let issue_files = read_all_issue_files(&issues_dir)?;

    if issue_files.is_empty() {
        return Ok(HydrationStats::default());
    }

    let json_uuids: std::collections::HashSet<String> =
        issue_files.iter().map(|f| f.uuid.to_string()).collect();

    let all_rows: Vec<SavedIssue> = db
        .conn
        .prepare(
            "SELECT i.id, i.uuid, i.title, i.description, i.status, i.priority, p.uuid, \
             i.created_by, i.created_at, i.updated_at, i.closed_at, i.scheduled_at, i.due_at \
             FROM issues i LEFT JOIN issues p ON p.id = i.parent_id WHERE i.uuid IS NOT NULL",
        )?
        .query_map([], |row| {
            Ok(SavedIssue {
                id: row.get(0)?,
                uuid: row.get(1)?,
                title: row.get(2)?,
                description: row.get(3)?,
                status: row.get(4)?,
                priority: row.get(5)?,
                parent_uuid: row.get(6)?,
                created_by: row.get(7)?,
                created_at: row.get(8)?,
                updated_at: row.get(9)?,
                closed_at: row.get(10)?,
                scheduled_at: row.get(11)?,
                due_at: row.get(12)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let sqlite_only_rows: Vec<SavedIssue> = all_rows
        .into_iter()
        .filter(|row| {
            if json_uuids.contains(&row.uuid) {
                return false;
            }

            row.created_by.is_none()
        })
        .collect();
    if !sqlite_only_rows.is_empty() {
        tracing::info!(
            "hydration: preserving {} SQLite-only issue(s) not found in JSON",
            sqlite_only_rows.len()
        );
    }

    let preserved_ids: Vec<i64> = sqlite_only_rows.iter().map(|r| r.id).collect();
    let saved_children = snapshot_children(db, &preserved_ids)?;

    let v2_comment_id_start = saved_children
        .comments
        .iter()
        .map(|c| c.0)
        .min()
        .map_or(-1, |min| min - 1);

    let (deduped, milestone_entries) = dedup_and_load_milestones(&issue_files, cache_dir)?;

    let mut uuid_to_id: HashMap<String, i64> = deduped
        .iter()
        .filter_map(|f| f.display_id.map(|id| (f.uuid.to_string(), id)))
        .collect();

    let milestone_uuid_to_id: HashMap<String, i64> = milestone_entries
        .iter()
        .map(|m| (m.uuid.to_string(), m.display_id))
        .collect();

    let mut stats = HydrationStats::default();
    let layout_version = read_layout_version(&cache_dir.join("meta")).unwrap_or(1);

    db.set_foreign_keys(false)?;

    let result = db.transaction(|| {
        db.clear_shared_data()?;

        for entry in &milestone_entries {
            let created_at = entry.created_at.to_rfc3339();
            let closed_at = entry.closed_at.map(|dt| dt.to_rfc3339());
            db.insert_hydrated_milestone(&HydratedMilestone {
                id: entry.display_id,
                uuid: &entry.uuid.to_string(),
                name: &entry.name,
                description: entry.description.as_deref(),
                status: entry.status.as_str(),
                created_at: &created_at,
                closed_at: closed_at.as_deref(),
            })?;
            stats.milestones += 1;
        }

        let sorted_issues = topo_sort_issues(&deduped);

        hydrate_issues(
            db,
            &sorted_issues,
            &mut uuid_to_id,
            &milestone_uuid_to_id,
            &issues_dir,
            layout_version,
            v2_comment_id_start,
            &mut stats,
        )?;

        hydrate_dependencies(db, &deduped, &uuid_to_id, &mut stats)?;

        hydrate_relations(db, &deduped, &uuid_to_id, &mut stats)?;

        restore_sqlite_only_issues(db, &sqlite_only_rows, &saved_children, &mut stats)?;

        Ok(stats)
    });

    if let Err(e) = db.set_foreign_keys(true) {
        tracing::warn!("failed to re-enable foreign key constraints: {}", e);
    }

    result
}

pub fn clear_shared_projection(db: &Database) -> Result<()> {
    db.clear_shared_data()
}

pub fn hydrate_from_state(
    state: &crate::checkpoint::CheckpointState,
    db: &Database,
) -> Result<HydrationStats> {
    hydrate_from_state_verified(state, db, |_| Ok(()))
}

pub fn hydrate_from_state_verified<F>(
    state: &crate::checkpoint::CheckpointState,
    db: &Database,
    verify: F,
) -> Result<HydrationStats>
where
    F: FnOnce(&Database) -> Result<()>,
{
    db.set_foreign_keys(false)?;
    let result = db.transaction(|| hydrate_from_state_verified_in_transaction(state, db, verify));
    if let Err(error) = db.set_foreign_keys(true) {
        tracing::warn!("failed to re-enable foreign key constraints: {error}");
    }
    result
}

pub(crate) fn hydrate_from_state_verified_in_transaction<F>(
    state: &crate::checkpoint::CheckpointState,
    db: &Database,
    verify: F,
) -> Result<HydrationStats>
where
    F: FnOnce(&Database) -> Result<()>,
{
    let state_uuids: std::collections::HashSet<String> =
        state.issues.keys().map(uuid::Uuid::to_string).collect();
    let all_rows: Vec<SavedIssue> = db
        .conn
        .prepare(
            "SELECT i.id, i.uuid, i.title, i.description, i.status, i.priority, p.uuid, \
             i.created_by, i.created_at, i.updated_at, i.closed_at, i.scheduled_at, i.due_at \
             FROM issues i LEFT JOIN issues p ON p.id = i.parent_id WHERE i.uuid IS NOT NULL",
        )?
        .query_map([], |row| {
            Ok(SavedIssue {
                id: row.get(0)?,
                uuid: row.get(1)?,
                title: row.get(2)?,
                description: row.get(3)?,
                status: row.get(4)?,
                priority: row.get(5)?,
                parent_uuid: row.get(6)?,
                created_by: row.get(7)?,
                created_at: row.get(8)?,
                updated_at: row.get(9)?,
                closed_at: row.get(10)?,
                scheduled_at: row.get(11)?,
                due_at: row.get(12)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut sqlite_only_rows = Vec::new();
    for row in all_rows {
        let uuid = row.uuid.parse().map_err(|error| {
            anyhow::anyhow!("local issue UUID {} is invalid: {error}", row.uuid)
        })?;
        if !state_uuids.contains(&row.uuid) && !state.deleted_issues.contains(&uuid) {
            sqlite_only_rows.push(row);
        }
    }
    if !sqlite_only_rows.is_empty() {
        tracing::info!(
            "hydrate_from_state: preserving {} SQLite-only issue(s) not in reduced state",
            sqlite_only_rows.len()
        );
    }
    let preserved_ids: Vec<i64> = sqlite_only_rows.iter().map(|r| r.id).collect();
    let saved_children = snapshot_children(db, &preserved_ids)?;
    let saved_local_time_entries = snapshot_all_time_entries(db)?;
    let saved_local_milestones = snapshot_local_milestones(db, state)?;
    let saved_session_links = snapshot_session_issue_links(db)?;
    let saved_sentinel_links = snapshot_sentinel_issue_links(db)?;

    let comment_id_start = saved_children
        .comments
        .iter()
        .map(|c| c.0)
        .min()
        .map_or(-1, |min| min.min(0) - 1);

    let mut uuid_to_id: HashMap<String, i64> = state
        .issues
        .values()
        .filter_map(|i| i.display_id.map(|id| (i.uuid.to_string(), id)))
        .collect();
    let milestone_uuid_to_id: HashMap<String, i64> = state
        .milestones
        .values()
        .filter_map(|m| m.display_id.map(|id| (m.uuid.to_string(), id)))
        .collect();

    let mut stats = HydrationStats::default();

    db.clear_shared_data()?;

    for m in state.milestones.values() {
        let Some(ms_id) = m.display_id else {
            continue;
        };
        let created_at = m.created_at.to_rfc3339();
        let closed_at = m.closed_at.map(|dt| dt.to_rfc3339());
        db.insert_hydrated_milestone(&HydratedMilestone {
            id: ms_id,
            uuid: &m.uuid.to_string(),
            name: &m.name,
            description: m.description.as_deref(),
            status: m.status.as_str(),
            created_at: &created_at,
            closed_at: closed_at.as_deref(),
        })?;
        stats.milestones += 1;
    }

    hydrate_state_issues(
        db,
        state,
        &mut uuid_to_id,
        &milestone_uuid_to_id,
        comment_id_start,
        &mut stats,
    )?;
    hydrate_state_dependencies(db, state, &uuid_to_id, &mut stats)?;
    hydrate_state_relations(db, state, &uuid_to_id, &mut stats)?;

    restore_sqlite_only_issues(db, &sqlite_only_rows, &saved_children, &mut stats)?;
    restore_local_milestones(db, &saved_local_milestones)?;
    restore_missing_time_entries(db, &saved_local_time_entries)?;
    restore_session_issue_links(db, &saved_session_links)?;
    restore_sentinel_issue_links(db, &saved_sentinel_links)?;
    verify(db)?;
    Ok(stats)
}

fn snapshot_local_milestones(
    db: &Database,
    state: &crate::checkpoint::CheckpointState,
) -> Result<SavedMilestoneLinks> {
    let milestones = db
        .conn
        .prepare(
            "SELECT id, uuid, name, description, status, created_at, closed_at \
             FROM milestones WHERE uuid IS NOT NULL",
        )?
        .query_map([], |row| {
            Ok(SavedMilestone {
                id: row.get(0)?,
                uuid: row.get(1)?,
                name: row.get(2)?,
                description: row.get(3)?,
                status: row.get(4)?,
                created_at: row.get(5)?,
                closed_at: row.get(6)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut local_milestones = Vec::new();
    for milestone in milestones {
        let uuid = milestone.uuid.parse().map_err(|error| {
            anyhow::anyhow!(
                "local milestone UUID {} is invalid: {error}",
                milestone.uuid
            )
        })?;
        if !state.milestones.contains_key(&uuid) && !state.deleted_milestones.contains(&uuid) {
            local_milestones.push(milestone);
        }
    }
    let local_uuids = local_milestones
        .iter()
        .map(|milestone| milestone.uuid.as_str())
        .collect::<std::collections::HashSet<_>>();
    let memberships = db
        .conn
        .prepare(
            "SELECT m.uuid, i.uuid FROM milestone_issues mi \
             JOIN milestones m ON m.id = mi.milestone_id \
             JOIN issues i ON i.id = mi.issue_id \
             WHERE m.uuid IS NOT NULL AND i.uuid IS NOT NULL",
        )?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<std::result::Result<Vec<(String, String)>, _>>()?
        .into_iter()
        .filter(|(milestone_uuid, _)| local_uuids.contains(milestone_uuid.as_str()))
        .collect();
    Ok((local_milestones, memberships))
}

fn restore_local_milestones(
    db: &Database,
    saved: &(Vec<SavedMilestone>, Vec<(String, String)>),
) -> Result<()> {
    let mut occupied = db
        .conn
        .prepare("SELECT id FROM milestones")?
        .query_map([], |row| row.get(0))?
        .collect::<std::result::Result<std::collections::HashSet<i64>, _>>()?;
    let mut next_local = occupied
        .iter()
        .copied()
        .chain(saved.0.iter().map(|milestone| milestone.id))
        .min()
        .unwrap_or(0)
        .min(0)
        - 1;
    for milestone in &saved.0 {
        let id = if occupied.insert(milestone.id) {
            milestone.id
        } else {
            while !occupied.insert(next_local) {
                next_local -= 1;
            }
            let value = next_local;
            next_local -= 1;
            value
        };
        db.insert_hydrated_milestone(&HydratedMilestone {
            id,
            uuid: &milestone.uuid,
            name: &milestone.name,
            description: milestone.description.as_deref(),
            status: &milestone.status,
            created_at: &milestone.created_at,
            closed_at: milestone.closed_at.as_deref(),
        })?;
    }
    for (milestone_uuid, issue_uuid) in &saved.1 {
        let ids: Option<(i64, i64)> = db
            .conn
            .query_row(
                "SELECT m.id, i.id FROM milestones m CROSS JOIN issues i \
                 WHERE m.uuid = ?1 AND i.uuid = ?2",
                rusqlite::params![milestone_uuid, issue_uuid],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((milestone_id, issue_id)) = ids {
            db.insert_hydrated_milestone_issue(milestone_id, issue_id)?;
        }
    }
    Ok(())
}

fn snapshot_session_issue_links(db: &Database) -> Result<Vec<(i64, String)>> {
    let links = db
        .conn
        .prepare(
            "SELECT s.id, i.uuid FROM sessions s JOIN issues i ON i.id = s.active_issue_id \
             WHERE i.uuid IS NOT NULL",
        )?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(links)
}

fn restore_session_issue_links(db: &Database, links: &[(i64, String)]) -> Result<()> {
    db.conn.execute(
        "UPDATE sessions SET active_issue_id = NULL \
         WHERE active_issue_id IS NOT NULL AND active_issue_id NOT IN (SELECT id FROM issues)",
        [],
    )?;
    for (session_id, issue_uuid) in links {
        db.conn.execute(
            "UPDATE sessions SET active_issue_id = \
             (SELECT id FROM issues WHERE uuid = ?1) WHERE id = ?2",
            rusqlite::params![issue_uuid, session_id],
        )?;
    }
    Ok(())
}

fn snapshot_sentinel_issue_links(db: &Database) -> Result<Vec<(i64, String)>> {
    let links = db
        .conn
        .prepare(
            "SELECT d.id, i.uuid FROM sentinel_dispatches d \
             JOIN issues i ON i.id = d.crosslink_issue_id WHERE i.uuid IS NOT NULL",
        )?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(links)
}

fn restore_sentinel_issue_links(db: &Database, links: &[(i64, String)]) -> Result<()> {
    db.conn.execute(
        "UPDATE sentinel_dispatches SET crosslink_issue_id = NULL \
         WHERE crosslink_issue_id IS NOT NULL \
         AND crosslink_issue_id NOT IN (SELECT id FROM issues)",
        [],
    )?;
    for (dispatch_id, issue_uuid) in links {
        db.conn.execute(
            "UPDATE sentinel_dispatches SET crosslink_issue_id = \
             (SELECT id FROM issues WHERE uuid = ?1) WHERE id = ?2",
            rusqlite::params![issue_uuid, dispatch_id],
        )?;
    }
    Ok(())
}

fn snapshot_all_time_entries(db: &Database) -> Result<Vec<SavedLocalTimeEntry>> {
    let entries = db
        .conn
        .prepare(
            "SELECT t.id, i.uuid, t.started_at, t.ended_at, t.duration_seconds \
             FROM time_entries t JOIN issues i ON i.id = t.issue_id \
             WHERE i.uuid IS NOT NULL",
        )?
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(entries)
}

fn restore_missing_time_entries(db: &Database, saved: &[SavedLocalTimeEntry]) -> Result<()> {
    let mut occupied = db
        .conn
        .prepare("SELECT id FROM time_entries")?
        .query_map([], |row| row.get(0))?
        .collect::<std::result::Result<std::collections::HashSet<i64>, _>>()?;
    let mut next_local = occupied.iter().copied().min().unwrap_or(0).min(0) - 1;
    for (id, issue_uuid, started_at, ended_at, duration_seconds) in saved {
        let issue_id: Option<i64> = db
            .conn
            .query_row(
                "SELECT id FROM issues WHERE uuid = ?1",
                [issue_uuid],
                |row| row.get(0),
            )
            .optional()?;
        let Some(issue_id) = issue_id else {
            continue;
        };
        let existing: Option<(i64, String, Option<String>, Option<i64>)> = db
            .conn
            .query_row(
                "SELECT issue_id, started_at, ended_at, duration_seconds \
                 FROM time_entries WHERE id = ?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        if existing.as_ref()
            == Some(&(
                issue_id,
                started_at.clone(),
                ended_at.clone(),
                *duration_seconds,
            ))
        {
            continue;
        }
        let restored_id = if existing.is_none() && occupied.insert(*id) {
            *id
        } else {
            while !occupied.insert(next_local) {
                next_local -= 1;
            }
            let value = next_local;
            next_local -= 1;
            value
        };
        db.insert_hydrated_time_entry(
            restored_id,
            issue_id,
            started_at,
            ended_at.as_deref(),
            *duration_seconds,
        )?;
    }
    Ok(())
}

fn topo_sort_state_issues(
    state: &crate::checkpoint::CheckpointState,
) -> Vec<&crate::checkpoint::CompactIssue> {
    let present: std::collections::HashSet<uuid::Uuid> = state.issues.keys().copied().collect();
    let mut roots: Vec<&crate::checkpoint::CompactIssue> = Vec::new();
    let mut children: Vec<&crate::checkpoint::CompactIssue> = Vec::new();
    for issue in state.issues.values() {
        match issue.parent_uuid {
            Some(p) if present.contains(&p) => children.push(issue),
            _ => roots.push(issue),
        }
    }
    roots.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.uuid.cmp(&b.uuid)));
    let mut sorted = roots;
    let mut remaining = children;
    for _ in 0..10 {
        if remaining.is_empty() {
            break;
        }
        let sorted_uuids: std::collections::HashSet<uuid::Uuid> =
            sorted.iter().map(|i| i.uuid).collect();
        let (mut ready, still): (Vec<_>, Vec<_>) = remaining
            .into_iter()
            .partition(|i| i.parent_uuid.is_none_or(|p| sorted_uuids.contains(&p)));
        ready.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.uuid.cmp(&b.uuid)));
        sorted.extend(ready);
        remaining = still;
    }
    sorted.extend(remaining);
    sorted
}

fn hydrate_state_issues(
    db: &Database,
    state: &crate::checkpoint::CheckpointState,
    uuid_to_id: &mut HashMap<String, i64>,
    milestone_uuid_to_id: &HashMap<String, i64>,
    comment_id_start: i64,
    stats: &mut HydrationStats,
) -> Result<()> {
    let mut next_local_id: i64 = -1;
    let mut next_v2_comment_id: i64 = comment_id_start;

    for issue in topo_sort_state_issues(state) {
        let display_id = issue.display_id.unwrap_or_else(|| {
            let local = next_local_id;
            next_local_id -= 1;
            uuid_to_id.insert(issue.uuid.to_string(), local);
            local
        });

        let parent_id = issue
            .parent_uuid
            .and_then(|u| uuid_to_id.get(&u.to_string()).copied());
        let created_at = issue.created_at.to_rfc3339();
        let updated_at = issue.updated_at.to_rfc3339();
        let closed_at = issue.closed_at.map(|dt| dt.to_rfc3339());
        let scheduled_at = issue.scheduled_at.map(|dt| dt.to_rfc3339());
        let due_at = issue.due_at.map(|dt| dt.to_rfc3339());

        db.insert_hydrated_issue(&HydratedIssue {
            id: display_id,
            uuid: &issue.uuid.to_string(),
            title: &issue.title,
            description: issue.description.as_deref(),
            status: issue.status.as_str(),
            priority: issue.priority.as_str(),
            parent_id,
            created_by: Some(&issue.created_by),
            created_at: &created_at,
            updated_at: &updated_at,
            closed_at: closed_at.as_deref(),
            scheduled_at: scheduled_at.as_deref(),
            due_at: due_at.as_deref(),
        })?;
        stats.issues += 1;

        for label in &issue.labels {
            db.insert_hydrated_label(display_id, label)?;
        }

        for (comment_uuid, c) in &issue.comments {
            let comment_created = c.created_at.to_rfc3339();

            let cid = c.display_id.unwrap_or_else(|| {
                let id = next_v2_comment_id;
                next_v2_comment_id -= 1;
                id
            });
            db.insert_hydrated_comment(
                cid,
                display_id,
                Some(&comment_uuid.to_string()),
                Some(&c.author),
                &c.content,
                &comment_created,
                &c.kind,
                c.trigger_type.as_deref(),
                c.intervention_context.as_deref(),
                c.driver_key_fingerprint.as_deref(),
            )?;
            stats.comments += 1;
        }

        for (entry_uuid, te) in &issue.time_entries {
            let started = te.started_at.to_rfc3339();
            let ended = te.ended_at.map(|dt| dt.to_rfc3339());

            let te_id = te
                .display_id
                .unwrap_or_else(|| negative_id_from_uuid(entry_uuid));
            db.insert_hydrated_time_entry(
                te_id,
                display_id,
                &started,
                ended.as_deref(),
                te.duration_seconds,
            )?;
        }

        if let Some(ms_uuid) = &issue.milestone_uuid {
            if let Some(&ms_id) = milestone_uuid_to_id.get(&ms_uuid.to_string()) {
                db.insert_hydrated_milestone_issue(ms_id, display_id)?;
            }
        }
    }
    Ok(())
}

fn negative_id_from_uuid(u: &uuid::Uuid) -> i64 {
    let b = u.as_bytes();
    let mut acc: i64 = 0;
    for &byte in &b[..7] {
        acc = (acc << 8) | i64::from(byte);
    }
    -(acc + 1)
}

fn hydrate_state_dependencies(
    db: &Database,
    state: &crate::checkpoint::CheckpointState,
    uuid_to_id: &HashMap<String, i64>,
    stats: &mut HydrationStats,
) -> Result<()> {
    for issue in state.issues.values() {
        let Some(&blocked_id) = uuid_to_id.get(&issue.uuid.to_string()) else {
            continue;
        };
        for blocker_uuid in &issue.blockers {
            if let Some(&blocker_id) = uuid_to_id.get(&blocker_uuid.to_string()) {
                db.insert_dependency_raw(blocker_id, blocked_id)?;
                stats.dependencies += 1;
            }
        }
    }
    Ok(())
}

fn hydrate_state_relations(
    db: &Database,
    state: &crate::checkpoint::CheckpointState,
    uuid_to_id: &HashMap<String, i64>,
    stats: &mut HydrationStats,
) -> Result<()> {
    for issue in state.issues.values() {
        let Some(&issue_id) = uuid_to_id.get(&issue.uuid.to_string()) else {
            continue;
        };
        for related_uuid in &issue.related {
            if let Some(&related_id) = uuid_to_id.get(&related_uuid.to_string()) {
                db.insert_relation_raw(issue_id, related_id)?;
                stats.relations += 1;
            }
        }
    }
    Ok(())
}

fn dedup_and_load_milestones<'a>(
    issue_files: &'a [IssueFile],
    cache_dir: &Path,
) -> Result<(Vec<&'a IssueFile>, Vec<crate::issue_file::MilestoneEntry>)> {
    let (deduped, dupes) = dedup_issue_files(issue_files);
    if !dupes.is_empty() {
        tracing::warn!(
            "{} duplicate issue file(s) skipped during hydration (same display_id)",
            dupes.len()
        );
        for d in &dupes {
            tracing::warn!(
                "  skipped: {} (display_id {:?}, uuid {})",
                d.title,
                d.display_id,
                d.uuid
            );
        }
    }

    let milestones_dir = cache_dir.join("meta").join("milestones");
    let mut milestone_entries = read_all_milestone_files(&milestones_dir)?;
    if milestone_entries.is_empty() {
        let legacy_path = cache_dir.join("meta").join("milestones.json");
        let legacy = read_milestones_file(&legacy_path)?;
        milestone_entries = legacy.milestones.into_values().collect();
    }

    Ok((deduped, milestone_entries))
}

fn topo_sort_issues<'a>(issues: &[&'a IssueFile]) -> Vec<&'a IssueFile> {
    let uuid_set: std::collections::HashSet<_> = issues.iter().map(|i| i.uuid).collect();
    let mut roots: Vec<&'a IssueFile> = Vec::new();
    let mut children: Vec<&'a IssueFile> = Vec::new();

    for &issue in issues {
        match issue.parent_uuid {
            Some(parent) if uuid_set.contains(&parent) => children.push(issue),
            _ => roots.push(issue),
        }
    }

    roots.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.uuid.cmp(&b.uuid)));
    let mut sorted = roots;

    let mut remaining = children;
    for _ in 0..10 {
        if remaining.is_empty() {
            break;
        }
        let sorted_uuids: std::collections::HashSet<_> = sorted.iter().map(|i| i.uuid).collect();
        let (mut ready, still_remaining): (Vec<&'a IssueFile>, Vec<&'a IssueFile>) = remaining
            .into_iter()
            .partition(|i| i.parent_uuid.is_none_or(|p| sorted_uuids.contains(&p)));
        ready.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.uuid.cmp(&b.uuid)));
        sorted.extend(ready);
        remaining = still_remaining;
    }

    sorted.extend(remaining);
    sorted
}

#[allow(clippy::too_many_arguments)]
fn hydrate_issues(
    db: &Database,
    sorted_issues: &[&IssueFile],
    uuid_to_id: &mut HashMap<String, i64>,
    milestone_uuid_to_id: &HashMap<String, i64>,
    issues_dir: &Path,
    layout_version: u32,
    v2_comment_id_start: i64,
    stats: &mut HydrationStats,
) -> Result<()> {
    let mut next_local_id: i64 = -1;

    let mut next_v2_comment_id: i64 = v2_comment_id_start;

    for issue in sorted_issues {
        let display_id = issue.display_id.unwrap_or_else(|| {
            let local_id = next_local_id;
            next_local_id -= 1;

            uuid_to_id.insert(issue.uuid.to_string(), local_id);
            local_id
        });

        let parent_id = issue
            .parent_uuid
            .and_then(|u| uuid_to_id.get(&u.to_string()).copied());

        let created_at = issue.created_at.to_rfc3339();
        let updated_at = issue.updated_at.to_rfc3339();
        let closed_at = issue.closed_at.map(|dt| dt.to_rfc3339());
        let scheduled_at = issue.scheduled_at.map(|dt| dt.to_rfc3339());
        let due_at = issue.due_at.map(|dt| dt.to_rfc3339());

        db.insert_hydrated_issue(&HydratedIssue {
            id: display_id,
            uuid: &issue.uuid.to_string(),
            title: &issue.title,
            description: issue.description.as_deref(),
            status: issue.status.as_str(),
            priority: issue.priority.as_str(),
            parent_id,
            created_by: Some(&issue.created_by),
            created_at: &created_at,
            updated_at: &updated_at,
            closed_at: closed_at.as_deref(),
            scheduled_at: scheduled_at.as_deref(),
            due_at: due_at.as_deref(),
        })?;
        stats.issues += 1;

        for label in &issue.labels {
            db.insert_hydrated_label(display_id, label)?;
        }

        for comment in &issue.comments {
            let comment_created = comment.created_at.to_rfc3339();
            db.insert_hydrated_comment(
                comment.id,
                display_id,
                None,
                Some(&comment.author),
                &comment.content,
                &comment_created,
                &comment.kind,
                comment.trigger_type.as_deref(),
                comment.intervention_context.as_deref(),
                comment.driver_key_fingerprint.as_deref(),
            )?;
            stats.comments += 1;
        }

        if layout_version >= 2 {
            let comments_dir = issues_dir.join(issue.uuid.to_string()).join("comments");
            if let Ok(v2_comments) = read_comment_files(&comments_dir) {
                for cf in &v2_comments {
                    let cf_uuid = cf.uuid.to_string();

                    if comment_uuid_exists(db, &cf_uuid)? {
                        continue;
                    }
                    let comment_created = cf.created_at.to_rfc3339();
                    let v2_id = next_v2_comment_id;
                    next_v2_comment_id -= 1;
                    db.insert_hydrated_comment(
                        v2_id,
                        display_id,
                        Some(&cf_uuid),
                        Some(&cf.author),
                        &cf.content,
                        &comment_created,
                        &cf.kind,
                        cf.trigger_type.as_deref(),
                        cf.intervention_context.as_deref(),
                        cf.driver_key_fingerprint.as_deref(),
                    )?;
                    stats.comments += 1;
                }
            }
        }

        for te in &issue.time_entries {
            let started = te.started_at.to_rfc3339();
            let ended = te.ended_at.map(|dt| dt.to_rfc3339());
            db.insert_hydrated_time_entry(
                te.id,
                display_id,
                &started,
                ended.as_deref(),
                te.duration_seconds,
            )?;
        }

        if let Some(ms_uuid) = &issue.milestone_uuid {
            if let Some(&ms_id) = milestone_uuid_to_id.get(&ms_uuid.to_string()) {
                db.insert_hydrated_milestone_issue(ms_id, display_id)?;
            }
        }
    }

    Ok(())
}

fn snapshot_children(db: &Database, preserved_ids: &[i64]) -> Result<SavedChildren> {
    if preserved_ids.is_empty() {
        return Ok(SavedChildren {
            labels: vec![],
            comments: vec![],
            deps: vec![],
            relations: vec![],
            milestone_issues: vec![],
        });
    }

    let id_placeholders: String = preserved_ids
        .iter()
        .map(std::string::ToString::to_string)
        .collect::<Vec<_>>()
        .join(",");

    let labels = db
        .conn
        .prepare(&format!(
            "SELECT issue_id, label FROM labels WHERE issue_id IN ({id_placeholders})"
        ))?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let comments = db.conn
        .prepare(&format!(
            "SELECT id, issue_id, uuid, author, content, created_at, kind, trigger_type, intervention_context, driver_key_fingerprint \
             FROM comments WHERE issue_id IN ({id_placeholders})"
        ))?
        .query_map([], |row| Ok((
            row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?,
            row.get(5)?, row.get(6)?, row.get(7)?, row.get(8)?, row.get(9)?,
        )))?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let deps = db
        .conn
        .prepare(&format!(
            "SELECT blocker.uuid, blocked.uuid FROM dependencies d \
             JOIN issues blocker ON blocker.id = d.blocker_id \
             JOIN issues blocked ON blocked.id = d.blocked_id \
             WHERE d.blocker_id IN ({id_placeholders}) OR d.blocked_id IN ({id_placeholders})"
        ))?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let relations = db
        .conn
        .prepare(&format!(
            "SELECT first.uuid, second.uuid FROM relations r \
             JOIN issues first ON first.id = r.issue_id_1 \
             JOIN issues second ON second.id = r.issue_id_2 \
             WHERE r.issue_id_1 IN ({id_placeholders}) OR r.issue_id_2 IN ({id_placeholders})"
        ))?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let milestone_issues = db
        .conn
        .prepare(&format!(
            "SELECT m.uuid, i.uuid FROM milestone_issues mi \
             JOIN milestones m ON m.id = mi.milestone_id \
             JOIN issues i ON i.id = mi.issue_id \
             WHERE mi.issue_id IN ({id_placeholders})"
        ))?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    Ok(SavedChildren {
        labels,
        comments,
        deps,
        relations,
        milestone_issues,
    })
}

fn comment_uuid_exists(db: &Database, uuid: &str) -> Result<bool> {
    let count: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM comments WHERE uuid = ?1",
        [uuid],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

fn restore_sqlite_only_issues(
    db: &Database,
    sqlite_only_rows: &[SavedIssue],
    saved_children: &SavedChildren,
    stats: &mut HydrationStats,
) -> Result<()> {
    let occupied = occupied_issue_ids(db)?;
    let mut assigned = occupied.clone();
    let mut next_local = occupied
        .iter()
        .copied()
        .chain(sqlite_only_rows.iter().map(|row| row.id))
        .min()
        .unwrap_or(0)
        .min(0)
        - 1;
    let mut remap: HashMap<i64, i64> = HashMap::new();
    for row in sqlite_only_rows {
        if !assigned.insert(row.id) {
            while !assigned.insert(next_local) {
                next_local -= 1;
            }
            remap.insert(row.id, next_local);
            next_local -= 1;
        }
    }
    if !remap.is_empty() {
        let mut collided: Vec<i64> = remap.keys().copied().collect();
        collided.sort_unstable();
        tracing::warn!(
            "hydration: {} local-only issue(s) collide with hub-assigned display id(s) \
             {:?}; remapped to negative local ids so the hub issues are not overwritten",
            remap.len(),
            collided
        );
    }
    let mapped = |id: i64| remap.get(&id).copied().unwrap_or(id);

    for row in sqlite_only_rows {
        db.insert_hydrated_issue(&HydratedIssue {
            id: mapped(row.id),
            uuid: &row.uuid,
            title: &row.title,
            description: row.description.as_deref(),
            status: &row.status,
            priority: &row.priority,
            parent_id: None,
            created_by: row.created_by.as_deref(),
            created_at: &row.created_at,
            updated_at: &row.updated_at,
            closed_at: row.closed_at.as_deref(),
            scheduled_at: row.scheduled_at.as_deref(),
            due_at: row.due_at.as_deref(),
        })?;
        stats.issues += 1;
    }
    for row in sqlite_only_rows {
        db.conn.execute(
            "UPDATE issues SET parent_id = (SELECT id FROM issues WHERE uuid = ?1) \
             WHERE uuid = ?2",
            rusqlite::params![row.parent_uuid, row.uuid],
        )?;
    }

    for (issue_id, label) in &saved_children.labels {
        db.insert_hydrated_label(mapped(*issue_id), label)?;
    }

    let occupied_comments = occupied_comment_ids(db)?;
    let mut next_comment_local = occupied_comments
        .iter()
        .copied()
        .chain(saved_children.comments.iter().map(|c| c.0))
        .min()
        .unwrap_or(0)
        .min(0)
        - 1;
    let mut comment_collisions: Vec<i64> = Vec::new();
    for (
        id,
        issue_id,
        uuid,
        author,
        content,
        created_at,
        kind,
        trigger_type,
        intervention_context,
        driver_key_fingerprint,
    ) in &saved_children.comments
    {
        if let Some(u) = uuid.as_deref() {
            let existing: Option<ExistingComment> = db
                .conn
                .query_row(
                    "SELECT issue_id, author, content, created_at, kind, trigger_type, \
                     intervention_context, driver_key_fingerprint FROM comments WHERE uuid = ?1",
                    [u],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                            row.get(5)?,
                            row.get(6)?,
                            row.get(7)?,
                        ))
                    },
                )
                .optional()?;
            if let Some(existing) = existing {
                anyhow::ensure!(
                    existing
                        == (
                            mapped(*issue_id),
                            author.clone(),
                            content.clone(),
                            created_at.clone(),
                            kind.clone(),
                            trigger_type.clone(),
                            intervention_context.clone(),
                            driver_key_fingerprint.clone(),
                        ),
                    "local comment UUID {u} conflicts with the authority projection"
                );
                continue;
            }
        }
        let comment_id = if occupied_comments.contains(id) {
            let re_keyed = next_comment_local;
            next_comment_local -= 1;
            comment_collisions.push(*id);
            re_keyed
        } else {
            *id
        };
        db.insert_hydrated_comment(
            comment_id,
            mapped(*issue_id),
            uuid.as_deref(),
            author.as_deref(),
            content,
            created_at,
            kind,
            trigger_type.as_deref(),
            intervention_context.as_deref(),
            driver_key_fingerprint.as_deref(),
        )?;
        stats.comments += 1;
    }
    if !comment_collisions.is_empty() {
        comment_collisions.sort_unstable();
        tracing::warn!(
            "hydration: {} preserved comment(s) collide with hub-assigned comment id(s) \
             {:?}; re-keyed to negative local ids so hydration does not abort (GH#11)",
            comment_collisions.len(),
            comment_collisions
        );
    }
    for (blocker_uuid, blocked_uuid) in &saved_children.deps {
        if let Some((blocker_id, blocked_id)) = issue_ids_by_uuid(db, blocker_uuid, blocked_uuid)? {
            db.insert_dependency_raw(blocker_id, blocked_id)?;
            stats.dependencies += 1;
        }
    }
    for (first_uuid, second_uuid) in &saved_children.relations {
        if let Some((first_id, second_id)) = issue_ids_by_uuid(db, first_uuid, second_uuid)? {
            db.insert_relation_raw(first_id, second_id)?;
            stats.relations += 1;
        }
    }
    for (milestone_uuid, issue_uuid) in &saved_children.milestone_issues {
        let ids: Option<(i64, i64)> = db
            .conn
            .query_row(
                "SELECT m.id, i.id FROM milestones m CROSS JOIN issues i \
                 WHERE m.uuid = ?1 AND i.uuid = ?2",
                rusqlite::params![milestone_uuid, issue_uuid],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((milestone_id, issue_id)) = ids {
            db.insert_hydrated_milestone_issue(milestone_id, issue_id)?;
        }
    }

    Ok(())
}

fn issue_ids_by_uuid(
    db: &Database,
    first_uuid: &str,
    second_uuid: &str,
) -> Result<Option<(i64, i64)>> {
    db.conn
        .query_row(
            "SELECT first.id, second.id FROM issues first CROSS JOIN issues second \
             WHERE first.uuid = ?1 AND second.uuid = ?2",
            rusqlite::params![first_uuid, second_uuid],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(Into::into)
}

fn occupied_comment_ids(db: &Database) -> Result<std::collections::HashSet<i64>> {
    let ids = db
        .conn
        .prepare("SELECT id FROM comments")?
        .query_map([], |row| row.get(0))?
        .collect::<std::result::Result<std::collections::HashSet<i64>, _>>()?;
    Ok(ids)
}

fn occupied_issue_ids(db: &Database) -> Result<std::collections::HashSet<i64>> {
    let ids = db
        .conn
        .prepare("SELECT id FROM issues")?
        .query_map([], |row| row.get(0))?
        .collect::<std::result::Result<std::collections::HashSet<i64>, _>>()?;
    Ok(ids)
}

fn hydrate_dependencies(
    db: &Database,
    issue_files: &[&IssueFile],
    uuid_to_id: &HashMap<String, i64>,
    stats: &mut HydrationStats,
) -> Result<()> {
    for issue in issue_files {
        let Some(blocked_id) = issue.display_id else {
            continue;
        };
        for blocker_uuid in &issue.blockers {
            if let Some(&blocker_id) = uuid_to_id.get(&blocker_uuid.to_string()) {
                db.insert_dependency_raw(blocker_id, blocked_id)?;
                stats.dependencies += 1;
            }
        }
    }
    Ok(())
}

fn hydrate_relations(
    db: &Database,
    issue_files: &[&IssueFile],
    uuid_to_id: &HashMap<String, i64>,
    stats: &mut HydrationStats,
) -> Result<()> {
    for issue in issue_files {
        let Some(issue_id) = issue.display_id else {
            continue;
        };
        for related_uuid in &issue.related {
            if let Some(&related_id) = uuid_to_id.get(&related_uuid.to_string()) {
                db.insert_relation_raw(issue_id, related_id)?;
                stats.relations += 1;
            }
        }
    }
    Ok(())
}

const LAST_HYDRATED_REF_FILE: &str = ".last-hydrated-ref";
const HYDRATED_FRONTIER_DIR: &str = "hydrated-frontiers";
const MAX_HYDRATED_FRONTIERS: usize = 64;

pub fn hydrate_current_authority_under_operation(
    crosslink_dir: &Path,
    db: &Database,
) -> Result<HydrationStats> {
    let sync = crate::sync::SyncManager::new(crosslink_dir)?;
    let stats = if sync.hub_mode().is_v3() {
        let source = crate::hub_source::RefHubSource::new(sync.cache_path())?;
        let outcome = crate::compaction::reduce(&source)?;
        hydrate_from_state(&outcome.state, db)?
    } else {
        hydrate_to_sqlite(sync.cache_path(), db)?
    };
    record_hydrated_ref_durable(crosslink_dir)?;
    Ok(stats)
}

pub fn maybe_auto_hydrate_under_operation(crosslink_dir: &Path, db: &Database) -> Result<bool> {
    if !projection_needs_hydration(crosslink_dir)? {
        return Ok(false);
    }
    hydrate_current_authority_under_operation(crosslink_dir, db)?;
    Ok(true)
}

pub fn projection_needs_hydration(crosslink_dir: &Path) -> Result<bool> {
    let sync = crate::sync::SyncManager::new(crosslink_dir)?;
    anyhow::ensure!(
        sync.is_initialized(),
        "repository authority cache is missing; run `crosslink daemon ensure --wait-ready --json`"
    );
    let current_ref = projection_authority_ref(crosslink_dir)?.ok_or_else(|| {
        anyhow::anyhow!(
            "repository authority frontier is missing; run `crosslink daemon ensure --wait-ready --json`"
        )
    })?;
    let last_ref = hydrated_frontier(crosslink_dir)?;
    Ok(last_ref.as_deref() != Some(&current_ref))
}

#[deprecated(note = "use maybe_auto_hydrate_under_operation when an operation permit is held")]
pub fn maybe_auto_hydrate(crosslink_dir: &Path, db: &Database) -> Result<bool> {
    let _permit = crate::reconcile::readiness::acquire_mutation_operation_permit(crosslink_dir)?;
    maybe_auto_hydrate_under_operation(crosslink_dir, db)
}

pub fn record_hydrated_ref(crosslink_dir: &Path) {
    if let Err(error) = record_hydrated_ref_durable(crosslink_dir) {
        tracing::warn!("failed to record hydrated projection frontier: {error}");
    }
}

pub fn record_hydrated_ref_durable(crosslink_dir: &Path) -> Result<()> {
    let frontier = projection_authority_ref(crosslink_dir)?
        .ok_or_else(|| anyhow::anyhow!("projection authority frontier is unavailable"))?;
    let directory = crosslink_dir.join(HYDRATED_FRONTIER_DIR);
    std::fs::create_dir_all(&directory)?;
    let mut records = std::fs::read_dir(&directory)?
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("ref"))
        .collect::<Vec<_>>();
    records.sort();
    if records
        .last()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .is_some_and(|value| value.trim() == frontier)
    {
        crate::reconcile::readiness::refresh_ready_record_after_projection(crosslink_dir)?;
        return Ok(());
    }
    let sequence = records
        .last()
        .and_then(|path| path.file_stem())
        .and_then(|stem| stem.to_str())
        .and_then(|stem| stem.split_once('-'))
        .and_then(|(value, _)| value.parse::<u64>().ok())
        .unwrap_or(0)
        .saturating_add(1);
    let token = uuid::Uuid::new_v4();
    let temporary = directory.join(format!(".{sequence:020}-{token}.tmp"));
    let destination = directory.join(format!("{sequence:020}-{token}.ref"));
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)?;
    use std::io::Write as _;
    file.write_all(frontier.as_bytes())?;
    file.sync_all()?;
    crate::utils::durable_rename(&temporary, &destination, false)?;
    sync_frontier_directory(&directory)?;
    records.push(destination);
    records.sort();
    let remove_count = records.len().saturating_sub(MAX_HYDRATED_FRONTIERS);
    for path in records.into_iter().take(remove_count) {
        std::fs::remove_file(path)?;
    }
    if remove_count > 0 {
        sync_frontier_directory(&directory)?;
    }
    crate::reconcile::readiness::refresh_ready_record_after_projection(crosslink_dir)?;
    Ok(())
}

pub fn hydrated_frontier(crosslink_dir: &Path) -> Result<Option<String>> {
    let directory = crosslink_dir.join(HYDRATED_FRONTIER_DIR);
    let mut records = if directory.is_dir() {
        std::fs::read_dir(&directory)?
            .collect::<std::io::Result<Vec<_>>>()?
            .into_iter()
            .map(|entry| entry.path())
            .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("ref"))
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    records.sort();
    if let Some(path) = records.last() {
        let value = std::fs::read_to_string(path)?;
        let value = value.trim().to_string();
        anyhow::ensure!(!value.is_empty(), "hydrated projection frontier is empty");
        return Ok(Some(value));
    }
    let legacy = crosslink_dir.join(LAST_HYDRATED_REF_FILE);
    if !legacy.exists() {
        return Ok(None);
    }
    let value = std::fs::read_to_string(legacy)?;
    let value = value.trim().to_string();
    anyhow::ensure!(
        !value.is_empty(),
        "legacy hydrated projection frontier is empty"
    );
    Ok(Some(value))
}

#[cfg(unix)]
fn sync_frontier_directory(directory: &Path) -> Result<()> {
    std::fs::File::open(directory)?.sync_all()?;
    Ok(())
}

#[cfg(windows)]
fn sync_frontier_directory(directory: &Path) -> Result<()> {
    let _ = directory;
    Ok(())
}

pub fn projection_authority_ref(crosslink_dir: &Path) -> Result<Option<String>> {
    let sync = crate::sync::SyncManager::new(crosslink_dir)?;
    if !sync.is_initialized() {
        return Ok(None);
    }
    sync.validate_cache_repository()?;
    let output = std::process::Command::new("git")
        .current_dir(sync.cache_path())
        .args([
            "for-each-ref",
            "--format=%(refname)%00%(objectname)",
            "refs/heads/crosslink/checkpoint",
            "refs/heads/crosslink/meta",
            "refs/heads/crosslink/agents/",
        ])
        .output()
        .context("observing current v3 projection authority")?;
    anyhow::ensure!(
        output.status.success(),
        "observing current v3 projection authority failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let advertised = String::from_utf8(output.stdout.clone())
        .context("v3 projection authority observation was not UTF-8")?;
    let names = advertised
        .lines()
        .filter_map(|line| line.split_once('\0').map(|(name, _)| name))
        .collect::<std::collections::HashSet<_>>();
    if !names.is_empty() {
        anyhow::ensure!(
            names.contains(crate::hub_v3::CHECKPOINT_REF)
                && names.contains(crate::hub_v3::META_REF),
            "current v3 projection authority is incomplete"
        );
        use sha2::Digest as _;
        return Ok(Some(hex::encode(sha2::Sha256::digest(output.stdout))));
    }
    let output = std::process::Command::new("git")
        .current_dir(sync.cache_path())
        .args(["rev-parse", "--verify", "HEAD"])
        .output()
        .context("observing current v2 projection authority")?;
    if !output.status.success() {
        return Ok(None);
    }
    let value = String::from_utf8(output.stdout)
        .context("v2 projection authority observation was not UTF-8")?
        .trim()
        .to_string();
    anyhow::ensure!(
        !value.is_empty(),
        "current v2 projection authority is empty"
    );
    Ok(Some(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::issue_file::{
        write_comment_file, write_issue_file, write_layout_version, CommentEntry, CommentFile,
        IssueFile, TimeEntry,
    };
    use chrono::Utc;
    use std::process::Command;
    use tempfile::tempdir;
    use uuid::Uuid;

    fn setup_test_db() -> (Database, tempfile::TempDir) {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = Database::open(&db_path).unwrap();
        (db, dir)
    }

    fn make_issue(display_id: i64, title: &str) -> IssueFile {
        IssueFile {
            uuid: Uuid::new_v4(),
            display_id: Some(display_id),
            title: title.to_string(),
            description: None,
            status: crate::models::IssueStatus::Open,
            priority: crate::models::Priority::Medium,
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

    fn write_issues_to_cache(cache_dir: &Path, issues: &[IssueFile]) {
        let issues_dir = cache_dir.join("issues");
        std::fs::create_dir_all(&issues_dir).unwrap();
        for issue in issues {
            let path = issues_dir.join(format!("{}.json", issue.uuid));
            write_issue_file(&path, issue).unwrap();
        }
    }

    #[test]
    fn test_hydrate_empty_cache() {
        let (db, _dir) = setup_test_db();
        let cache = tempdir().unwrap();
        let stats = hydrate_to_sqlite(cache.path(), &db).unwrap();
        assert_eq!(stats.issues, 0);
    }

    #[test]
    fn test_hydrate_single_issue() {
        let (db, _dir) = setup_test_db();
        let cache = tempdir().unwrap();

        let issue = make_issue(1, "Test issue");
        write_issues_to_cache(cache.path(), &[issue]);

        let stats = hydrate_to_sqlite(cache.path(), &db).unwrap();
        assert_eq!(stats.issues, 1);

        let loaded = db.get_issue(1).unwrap().unwrap();
        assert_eq!(loaded.title, "Test issue");
    }

    #[test]
    fn test_hydrate_with_labels() {
        let (db, _dir) = setup_test_db();
        let cache = tempdir().unwrap();

        let mut issue = make_issue(1, "Labeled issue");
        issue.labels = vec!["bug".to_string(), "auth".to_string()];
        write_issues_to_cache(cache.path(), &[issue]);

        hydrate_to_sqlite(cache.path(), &db).unwrap();

        let labels = db.get_labels(1).unwrap();
        assert!(labels.contains(&"bug".to_string()));
        assert!(labels.contains(&"auth".to_string()));
    }

    #[test]
    fn test_hydrate_with_comments() {
        let (db, _dir) = setup_test_db();
        let cache = tempdir().unwrap();

        let mut issue = make_issue(1, "Commented issue");
        issue.comments = vec![CommentEntry {
            id: 1,
            author: "agent-1".to_string(),
            content: "First comment".to_string(),
            created_at: Utc::now(),
            kind: "note".to_string(),
            trigger_type: None,
            intervention_context: None,
            driver_key_fingerprint: None,
            signed_by: None,
            signature: None,
        }];
        write_issues_to_cache(cache.path(), &[issue]);

        let stats = hydrate_to_sqlite(cache.path(), &db).unwrap();
        assert_eq!(stats.comments, 1);

        let comments = db.get_comments(1).unwrap();
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].content, "First comment");
    }

    #[test]
    fn test_hydrate_dependencies() {
        let (db, _dir) = setup_test_db();
        let cache = tempdir().unwrap();

        let issue_a = make_issue(1, "Blocked issue");
        let issue_b = make_issue(2, "Blocker issue");

        let mut issue_a_with_dep = issue_a;
        issue_a_with_dep.blockers = vec![issue_b.uuid];

        write_issues_to_cache(cache.path(), &[issue_a_with_dep, issue_b]);

        let stats = hydrate_to_sqlite(cache.path(), &db).unwrap();
        assert_eq!(stats.dependencies, 1);

        let blockers = db.get_blockers(1).unwrap();
        assert_eq!(blockers, vec![2]);

        let blocking = db.get_blocking(2).unwrap();
        assert_eq!(blocking, vec![1]);
    }

    #[test]
    fn test_hydrate_dangling_blocker_uuid() {
        let (db, _dir) = setup_test_db();
        let cache = tempdir().unwrap();

        let mut issue = make_issue(1, "Issue with dangling dep");
        issue.blockers = vec![Uuid::new_v4()];
        write_issues_to_cache(cache.path(), &[issue]);

        let stats = hydrate_to_sqlite(cache.path(), &db).unwrap();
        assert_eq!(stats.issues, 1);
        assert_eq!(stats.dependencies, 0);
    }

    #[test]
    fn test_hydrate_relations() {
        let (db, _dir) = setup_test_db();
        let cache = tempdir().unwrap();

        let issue_a = make_issue(1, "Issue A");
        let issue_b = make_issue(2, "Issue B");

        let mut issue_a_related = issue_a;
        issue_a_related.related = vec![issue_b.uuid];

        write_issues_to_cache(cache.path(), &[issue_a_related, issue_b]);

        let stats = hydrate_to_sqlite(cache.path(), &db).unwrap();
        assert_eq!(stats.relations, 1);
    }

    #[test]
    fn test_hydrate_parent_child() {
        let (db, _dir) = setup_test_db();
        let cache = tempdir().unwrap();

        let parent = make_issue(1, "Parent");
        let mut child = make_issue(2, "Child");
        child.parent_uuid = Some(parent.uuid);

        write_issues_to_cache(cache.path(), &[parent, child]);

        hydrate_to_sqlite(cache.path(), &db).unwrap();

        let loaded = db.get_issue(2).unwrap().unwrap();
        assert_eq!(loaded.parent_id, Some(1));
    }

    #[test]
    fn test_hydrate_replaces_previous_data() {
        let (db, _dir) = setup_test_db();
        let cache = tempdir().unwrap();

        let issue = make_issue(1, "Original");
        write_issues_to_cache(cache.path(), std::slice::from_ref(&issue));
        hydrate_to_sqlite(cache.path(), &db).unwrap();

        let mut updated = issue;
        updated.title = "Updated".to_string();

        let issues_dir = cache.path().join("issues");
        std::fs::remove_dir_all(&issues_dir).unwrap();
        write_issues_to_cache(cache.path(), &[updated]);

        hydrate_to_sqlite(cache.path(), &db).unwrap();

        let loaded = db.get_issue(1).unwrap().unwrap();
        assert_eq!(loaded.title, "Updated");
    }

    #[test]
    fn test_hydrate_assigns_negative_id_for_null_display_id() {
        let (db, _dir) = setup_test_db();
        let cache = tempdir().unwrap();

        let mut offline = make_issue(0, "Offline");
        offline.display_id = None;

        let pushed = make_issue(1, "Pushed");
        write_issues_to_cache(cache.path(), &[offline, pushed]);

        let stats = hydrate_to_sqlite(cache.path(), &db).unwrap();
        assert_eq!(stats.issues, 2);

        assert!(db.get_issue(1).unwrap().is_some());

        let offline_issue = db.get_issue(-1).unwrap();
        assert!(offline_issue.is_some());
        assert_eq!(offline_issue.unwrap().title, "Offline");
    }

    #[test]
    fn test_hydrate_with_time_entries() {
        let (db, _dir) = setup_test_db();
        let cache = tempdir().unwrap();

        let mut issue = make_issue(1, "Timed issue");
        issue.time_entries = vec![TimeEntry {
            id: 1,
            started_at: Utc::now(),
            ended_at: Some(Utc::now()),
            duration_seconds: Some(3600),
        }];
        write_issues_to_cache(cache.path(), &[issue]);

        hydrate_to_sqlite(cache.path(), &db).unwrap();
    }

    #[test]
    fn test_hydrate_milestones_per_file() {
        let (db, _dir) = setup_test_db();
        let cache = tempdir().unwrap();

        let issue = make_issue(1, "Test");
        write_issues_to_cache(cache.path(), &[issue]);

        let ms_dir = cache.path().join("meta").join("milestones");
        std::fs::create_dir_all(&ms_dir).unwrap();
        let ms_uuid = Uuid::new_v4();
        let entry = crate::issue_file::MilestoneEntry {
            uuid: ms_uuid,
            display_id: 1,
            name: "v1.0".to_string(),
            description: None,
            status: crate::models::IssueStatus::Open,
            created_at: Utc::now(),
            closed_at: None,
        };
        crate::issue_file::write_milestone_file(&ms_dir.join(format!("{ms_uuid}.json")), &entry)
            .unwrap();

        let stats = hydrate_to_sqlite(cache.path(), &db).unwrap();
        assert_eq!(stats.milestones, 1);

        let ms = db.get_milestone(1).unwrap();
        assert!(ms.is_some());
        assert_eq!(ms.unwrap().name, "v1.0");
    }

    #[test]
    fn test_hydrate_milestones_legacy_fallback() {
        let (db, _dir) = setup_test_db();
        let cache = tempdir().unwrap();

        let issue = make_issue(1, "Test");
        write_issues_to_cache(cache.path(), &[issue]);

        let meta_dir = cache.path().join("meta");
        std::fs::create_dir_all(&meta_dir).unwrap();
        let ms_uuid = Uuid::new_v4();
        let mut milestones = std::collections::HashMap::new();
        milestones.insert(
            ms_uuid,
            crate::issue_file::MilestoneEntry {
                uuid: ms_uuid,
                display_id: 1,
                name: "legacy-ms".to_string(),
                description: None,
                status: crate::models::IssueStatus::Open,
                created_at: Utc::now(),
                closed_at: None,
            },
        );
        let mf = crate::issue_file::MilestonesFile { milestones };
        let json = serde_json::to_string_pretty(&mf).unwrap();
        std::fs::write(meta_dir.join("milestones.json"), json).unwrap();

        let stats = hydrate_to_sqlite(cache.path(), &db).unwrap();
        assert_eq!(stats.milestones, 1);

        let ms = db.get_milestone(1).unwrap();
        assert!(ms.is_some());
        assert_eq!(ms.unwrap().name, "legacy-ms");
    }

    #[test]
    fn test_dedup_no_duplicates() {
        let a = make_issue(1, "A");
        let b = make_issue(2, "B");
        let issues = [a, b];
        let (keep, dupes) = dedup_issue_files(&issues);
        assert_eq!(keep.len(), 2);
        assert_eq!(dupes.len(), 0);
    }

    #[test]
    fn test_dedup_keeps_most_recent() {
        use chrono::Duration;
        let mut old = make_issue(1, "Old");
        old.updated_at = Utc::now() - Duration::seconds(60);
        let mut new = make_issue(1, "New");
        new.updated_at = Utc::now();

        let issues = [old, new];
        let (keep, dupes) = dedup_issue_files(&issues);
        assert_eq!(keep.len(), 1);
        assert_eq!(dupes.len(), 1);
        assert_eq!(keep[0].title, "New");
        assert_eq!(dupes[0].title, "Old");
    }

    #[test]
    fn test_dedup_issue_with_no_display_id_passes_through() {
        let mut issue = make_issue(0, "Offline");
        issue.display_id = None;
        let issues = [issue];
        let (keep, dupes) = dedup_issue_files(&issues);
        assert_eq!(keep.len(), 1);
        assert_eq!(dupes.len(), 0);
    }

    #[test]
    fn test_dedup_three_copies_keeps_newest() {
        use chrono::Duration;
        let mut oldest = make_issue(5, "Oldest");
        oldest.updated_at = Utc::now() - Duration::seconds(120);
        let mut middle = make_issue(5, "Middle");
        middle.updated_at = Utc::now() - Duration::seconds(60);
        let mut newest = make_issue(5, "Newest");
        newest.updated_at = Utc::now();
        let issues = [oldest, middle, newest];
        let (keep, dupes) = dedup_issue_files(&issues);
        assert_eq!(keep.len(), 1);
        assert_eq!(dupes.len(), 2);
        assert_eq!(keep[0].title, "Newest");
    }

    #[test]
    fn test_hydrate_deduplicates_same_display_id() {
        use chrono::Duration;
        let (db, _dir) = setup_test_db();
        let cache = tempdir().unwrap();

        let mut old = make_issue(1, "Old title");
        old.updated_at = Utc::now() - Duration::seconds(60);
        let mut new = make_issue(1, "New title");
        new.updated_at = Utc::now();

        write_issues_to_cache(cache.path(), &[old, new]);

        let stats = hydrate_to_sqlite(cache.path(), &db).unwrap();

        assert_eq!(stats.issues, 1);
        let loaded = db.get_issue(1).unwrap().unwrap();
        assert_eq!(loaded.title, "New title");
    }

    #[test]
    fn test_topo_sort_roots_before_children() {
        let parent = make_issue(1, "Parent");
        let mut child = make_issue(2, "Child");
        child.parent_uuid = Some(parent.uuid);

        let sorted = topo_sort_issues(&[&child, &parent]);
        assert_eq!(sorted[0].title, "Parent");
        assert_eq!(sorted[1].title, "Child");
    }

    #[test]
    fn test_topo_sort_three_levels_deep() {
        let grandparent = make_issue(1, "Grandparent");
        let mut parent = make_issue(2, "Parent");
        parent.parent_uuid = Some(grandparent.uuid);
        let mut child = make_issue(3, "Child");
        child.parent_uuid = Some(parent.uuid);

        let sorted = topo_sort_issues(&[&child, &parent, &grandparent]);

        let pos = |title: &str| sorted.iter().position(|i| i.title == title).unwrap();
        assert!(pos("Grandparent") < pos("Parent"));
        assert!(pos("Parent") < pos("Child"));
    }

    #[test]
    fn test_topo_sort_orphaned_parent_uuid_treated_as_root() {
        let mut orphan_child = make_issue(2, "OrphanChild");
        orphan_child.parent_uuid = Some(Uuid::new_v4());

        let root = make_issue(1, "Root");

        let sorted = topo_sort_issues(&[&orphan_child, &root]);

        assert_eq!(sorted.len(), 2);
        let titles: Vec<&str> = sorted.iter().map(|i| i.title.as_str()).collect();
        assert!(titles.contains(&"Root"));
        assert!(titles.contains(&"OrphanChild"));
    }

    #[test]
    fn test_topo_sort_no_issues() {
        let sorted = topo_sort_issues(&[]);
        assert!(sorted.is_empty());
    }

    #[test]
    fn test_hydrate_dependency_skips_issue_with_no_display_id() {
        let (db, _dir) = setup_test_db();
        let cache = tempdir().unwrap();

        let blocker = make_issue(1, "Blocker");
        let mut offline = make_issue(0, "Offline blocked");
        offline.display_id = None;
        offline.blockers = vec![blocker.uuid];

        write_issues_to_cache(cache.path(), &[blocker, offline]);

        let stats = hydrate_to_sqlite(cache.path(), &db).unwrap();

        assert_eq!(stats.dependencies, 0);
    }

    #[test]
    fn test_hydrate_relation_skips_issue_with_no_display_id() {
        let (db, _dir) = setup_test_db();
        let cache = tempdir().unwrap();

        let related = make_issue(1, "Related");
        let mut offline = make_issue(0, "Offline related");
        offline.display_id = None;
        offline.related = vec![related.uuid];

        write_issues_to_cache(cache.path(), &[related, offline]);

        let stats = hydrate_to_sqlite(cache.path(), &db).unwrap();
        assert_eq!(stats.relations, 0);
    }

    #[test]
    fn test_hydrate_dangling_relation_uuid() {
        let (db, _dir) = setup_test_db();
        let cache = tempdir().unwrap();

        let mut issue = make_issue(1, "Issue with dangling relation");
        issue.related = vec![Uuid::new_v4()];
        write_issues_to_cache(cache.path(), &[issue]);

        let stats = hydrate_to_sqlite(cache.path(), &db).unwrap();
        assert_eq!(stats.relations, 0);
    }

    #[test]
    fn test_hydrate_issue_with_description_and_closed_at() {
        let (db, _dir) = setup_test_db();
        let cache = tempdir().unwrap();

        let mut issue = make_issue(1, "Closed issue");
        issue.description = Some("A detailed description".to_string());
        issue.status = crate::models::IssueStatus::Closed;
        issue.closed_at = Some(Utc::now());

        write_issues_to_cache(cache.path(), &[issue]);

        hydrate_to_sqlite(cache.path(), &db).unwrap();

        let loaded = db.get_issue(1).unwrap().unwrap();
        assert_eq!(
            loaded.description.as_deref(),
            Some("A detailed description")
        );
        assert_eq!(loaded.status, crate::models::IssueStatus::Closed);
        assert!(loaded.closed_at.is_some());
    }

    #[test]
    fn test_hydrate_issue_milestone_association() {
        let (db, _dir) = setup_test_db();
        let cache = tempdir().unwrap();

        let ms_uuid = Uuid::new_v4();

        let mut issue = make_issue(1, "Milestone issue");
        issue.milestone_uuid = Some(ms_uuid);
        write_issues_to_cache(cache.path(), &[issue]);

        let ms_dir = cache.path().join("meta").join("milestones");
        std::fs::create_dir_all(&ms_dir).unwrap();
        let entry = crate::issue_file::MilestoneEntry {
            uuid: ms_uuid,
            display_id: 10,
            name: "Sprint 1".to_string(),
            description: None,
            status: crate::models::IssueStatus::Open,
            created_at: Utc::now(),
            closed_at: None,
        };
        crate::issue_file::write_milestone_file(&ms_dir.join(format!("{ms_uuid}.json")), &entry)
            .unwrap();

        let stats = hydrate_to_sqlite(cache.path(), &db).unwrap();
        assert_eq!(stats.milestones, 1);

        let ms = db.get_issue_milestone(1).unwrap();
        assert!(ms.is_some());
        assert_eq!(ms.unwrap().name, "Sprint 1");
    }

    #[test]
    fn test_hydrate_issue_milestone_uuid_not_in_map() {
        let (db, _dir) = setup_test_db();
        let cache = tempdir().unwrap();

        let mut issue = make_issue(1, "Orphan milestone ref");
        issue.milestone_uuid = Some(Uuid::new_v4());
        write_issues_to_cache(cache.path(), &[issue]);

        let stats = hydrate_to_sqlite(cache.path(), &db).unwrap();
        assert_eq!(stats.issues, 1);
        assert_eq!(stats.milestones, 0);
    }

    #[test]
    fn test_hydrate_milestone_with_closed_at() {
        let (db, _dir) = setup_test_db();
        let cache = tempdir().unwrap();

        let issue = make_issue(1, "Test");
        write_issues_to_cache(cache.path(), &[issue]);

        let ms_dir = cache.path().join("meta").join("milestones");
        std::fs::create_dir_all(&ms_dir).unwrap();
        let ms_uuid = Uuid::new_v4();
        let entry = crate::issue_file::MilestoneEntry {
            uuid: ms_uuid,
            display_id: 1,
            name: "Closed sprint".to_string(),
            description: Some("A completed sprint".to_string()),
            status: crate::models::IssueStatus::Closed,
            created_at: Utc::now(),
            closed_at: Some(Utc::now()),
        };
        crate::issue_file::write_milestone_file(&ms_dir.join(format!("{ms_uuid}.json")), &entry)
            .unwrap();

        let stats = hydrate_to_sqlite(cache.path(), &db).unwrap();
        assert_eq!(stats.milestones, 1);
        let ms = db.get_milestone(1).unwrap().unwrap();
        assert_eq!(ms.status, crate::models::IssueStatus::Closed);
    }

    #[test]
    fn test_hydrate_v2_standalone_comment_files() {
        let (db, _dir) = setup_test_db();
        let cache = tempdir().unwrap();

        let issue = make_issue(1, "V2 issue");
        let issue_uuid = issue.uuid;

        let issue_dir = cache.path().join("issues").join(issue_uuid.to_string());
        std::fs::create_dir_all(&issue_dir).unwrap();
        write_issue_file(&issue_dir.join("issue.json"), &issue).unwrap();

        let comments_dir = issue_dir.join("comments");
        std::fs::create_dir_all(&comments_dir).unwrap();
        let comment_uuid = Uuid::new_v4();
        let cf = CommentFile {
            uuid: comment_uuid,
            issue_uuid,
            author: "agent-1".to_string(),
            content: "Standalone comment".to_string(),
            created_at: Utc::now(),
            kind: "note".to_string(),
            trigger_type: None,
            intervention_context: None,
            driver_key_fingerprint: None,
            signed_by: None,
            signature: None,
        };
        write_comment_file(&comments_dir.join(format!("{comment_uuid}.json")), &cf).unwrap();

        let meta_dir = cache.path().join("meta");
        write_layout_version(&meta_dir, 2).unwrap();

        let stats = hydrate_to_sqlite(cache.path(), &db).unwrap();
        assert_eq!(stats.issues, 1);
        assert_eq!(stats.comments, 1);

        let comments = db.get_comments(1).unwrap();
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].content, "Standalone comment");
    }

    #[test]
    fn test_hydrate_v2_comment_dedup_across_passes() {
        let (db, _dir) = setup_test_db();
        let cache = tempdir().unwrap();

        let local_id = db.create_issue("Local-only issue", None, "medium").unwrap();
        assert_eq!(local_id, 1);

        let issue = make_issue(1, "Hub issue");
        let issue_uuid = issue.uuid;
        let issue_dir = cache.path().join("issues").join(issue_uuid.to_string());
        std::fs::create_dir_all(&issue_dir).unwrap();
        write_issue_file(&issue_dir.join("issue.json"), &issue).unwrap();

        let comments_dir = issue_dir.join("comments");
        std::fs::create_dir_all(&comments_dir).unwrap();
        let comment_uuid = Uuid::new_v4();
        let cf = CommentFile {
            uuid: comment_uuid,
            issue_uuid,
            author: "agent-1".to_string(),
            content: "Hub comment".to_string(),
            created_at: Utc::now(),
            kind: "note".to_string(),
            trigger_type: None,
            intervention_context: None,
            driver_key_fingerprint: None,
            signed_by: None,
            signature: None,
        };
        write_comment_file(&comments_dir.join(format!("{comment_uuid}.json")), &cf).unwrap();
        write_layout_version(&cache.path().join("meta"), 2).unwrap();

        let uuid_count = |db: &Database| -> i64 {
            db.conn
                .query_row(
                    "SELECT COUNT(*) FROM comments WHERE uuid = ?1",
                    [comment_uuid.to_string()],
                    |row| row.get(0),
                )
                .unwrap()
        };

        hydrate_to_sqlite(cache.path(), &db).unwrap();
        assert_eq!(uuid_count(&db), 1, "one copy after the first pass");

        hydrate_to_sqlite(cache.path(), &db).unwrap();
        assert_eq!(
            uuid_count(&db),
            1,
            "hub comment must not multiply across hydration passes"
        );

        hydrate_to_sqlite(cache.path(), &db).unwrap();
        assert_eq!(uuid_count(&db), 1, "still one copy after a third pass");
    }

    #[test]
    fn test_hydrate_v2_comment_with_optional_fields() {
        let (db, _dir) = setup_test_db();
        let cache = tempdir().unwrap();

        let issue = make_issue(1, "V2 issue with rich comment");
        let issue_uuid = issue.uuid;

        let issue_dir = cache.path().join("issues").join(issue_uuid.to_string());
        std::fs::create_dir_all(&issue_dir).unwrap();
        write_issue_file(&issue_dir.join("issue.json"), &issue).unwrap();

        let comments_dir = issue_dir.join("comments");
        std::fs::create_dir_all(&comments_dir).unwrap();
        let comment_uuid = Uuid::new_v4();
        let cf = CommentFile {
            uuid: comment_uuid,
            issue_uuid,
            author: "agent-2".to_string(),
            content: "Intervention comment".to_string(),
            created_at: Utc::now(),
            kind: "intervention".to_string(),
            trigger_type: Some("tool_rejected".to_string()),
            intervention_context: Some("tried to write to protected file".to_string()),
            driver_key_fingerprint: Some("SHA256:abc123".to_string()),
            signed_by: Some("SHA256:abc123".to_string()),
            signature: Some("base64sig==".to_string()),
        };
        write_comment_file(&comments_dir.join(format!("{comment_uuid}.json")), &cf).unwrap();

        let meta_dir = cache.path().join("meta");
        write_layout_version(&meta_dir, 2).unwrap();

        let stats = hydrate_to_sqlite(cache.path(), &db).unwrap();
        assert_eq!(stats.comments, 1);
    }

    #[test]
    fn test_hydrate_v2_multiple_comments_get_unique_ids() {
        let (db, _dir) = setup_test_db();
        let cache = tempdir().unwrap();

        let issue = make_issue(1, "V2 multi-comment");
        let issue_uuid = issue.uuid;

        let issue_dir = cache.path().join("issues").join(issue_uuid.to_string());
        std::fs::create_dir_all(&issue_dir).unwrap();
        write_issue_file(&issue_dir.join("issue.json"), &issue).unwrap();

        let comments_dir = issue_dir.join("comments");
        std::fs::create_dir_all(&comments_dir).unwrap();

        for i in 0..3u32 {
            let cu = Uuid::new_v4();
            let cf = CommentFile {
                uuid: cu,
                issue_uuid,
                author: format!("agent-{i}"),
                content: format!("Comment {i}"),
                created_at: Utc::now(),
                kind: "note".to_string(),
                trigger_type: None,
                intervention_context: None,
                driver_key_fingerprint: None,
                signed_by: None,
                signature: None,
            };
            write_comment_file(&comments_dir.join(format!("{cu}.json")), &cf).unwrap();
        }

        let meta_dir = cache.path().join("meta");
        write_layout_version(&meta_dir, 2).unwrap();

        let stats = hydrate_to_sqlite(cache.path(), &db).unwrap();
        assert_eq!(stats.comments, 3);

        let comments = db.get_comments(1).unwrap();
        assert_eq!(comments.len(), 3);
    }

    #[test]
    fn test_hydrate_v1_layout_skips_v2_comment_files() {
        let (db, _dir) = setup_test_db();
        let cache = tempdir().unwrap();

        let issue = make_issue(1, "V1 issue");
        let issue_uuid = issue.uuid;
        write_issues_to_cache(cache.path(), &[issue]);

        let comments_dir = cache
            .path()
            .join("issues")
            .join(issue_uuid.to_string())
            .join("comments");
        std::fs::create_dir_all(&comments_dir).unwrap();
        let cu = Uuid::new_v4();
        let cf = CommentFile {
            uuid: cu,
            issue_uuid,
            author: "agent".to_string(),
            content: "Should be ignored".to_string(),
            created_at: Utc::now(),
            kind: "note".to_string(),
            trigger_type: None,
            intervention_context: None,
            driver_key_fingerprint: None,
            signed_by: None,
            signature: None,
        };
        write_comment_file(&comments_dir.join(format!("{cu}.json")), &cf).unwrap();

        let stats = hydrate_to_sqlite(cache.path(), &db).unwrap();
        assert_eq!(stats.comments, 0);
    }

    #[test]
    fn test_hydrate_time_entry_without_ended_at() {
        let (db, _dir) = setup_test_db();
        let cache = tempdir().unwrap();

        let mut issue = make_issue(1, "Active timer");
        issue.time_entries = vec![TimeEntry {
            id: 1,
            started_at: Utc::now(),
            ended_at: None,
            duration_seconds: None,
        }];
        write_issues_to_cache(cache.path(), &[issue]);

        hydrate_to_sqlite(cache.path(), &db).unwrap();
    }

    #[test]
    fn test_hydration_stats_default() {
        let stats = HydrationStats::default();
        assert_eq!(stats.issues, 0);
        assert_eq!(stats.comments, 0);
        assert_eq!(stats.dependencies, 0);
        assert_eq!(stats.relations, 0);
        assert_eq!(stats.milestones, 0);
    }

    #[test]
    fn test_hydrate_offline_child_resolves_offline_parent() {
        let (db, _dir) = setup_test_db();
        let cache = tempdir().unwrap();

        let mut parent = make_issue(0, "Offline parent");
        parent.display_id = None;
        let parent_uuid = parent.uuid;

        let mut child = make_issue(0, "Offline child");
        child.display_id = None;
        child.parent_uuid = Some(parent_uuid);

        write_issues_to_cache(cache.path(), &[parent, child]);

        let stats = hydrate_to_sqlite(cache.path(), &db).unwrap();
        assert_eq!(stats.issues, 2);

        let loaded_parent = db.get_issue(-1).unwrap();
        let loaded_child = db.get_issue(-2).unwrap();
        assert!(loaded_parent.is_some() || loaded_child.is_some());
    }

    fn initialized_repository() -> tempfile::TempDir {
        let root = tempdir().unwrap();
        assert!(Command::new("git")
            .current_dir(root.path())
            .args(["init", "-b", "main"])
            .status()
            .unwrap()
            .success());
        for (key, value) in [
            ("user.email", "test@example.invalid"),
            ("user.name", "Test"),
        ] {
            assert!(Command::new("git")
                .current_dir(root.path())
                .args(["config", key, value])
                .status()
                .unwrap()
                .success());
        }
        std::fs::write(root.path().join("README.md"), "test").unwrap();
        assert!(Command::new("git")
            .current_dir(root.path())
            .args(["add", "README.md"])
            .status()
            .unwrap()
            .success());
        assert!(Command::new("git")
            .current_dir(root.path())
            .args(["commit", "-m", "initial", "--no-gpg-sign"])
            .status()
            .unwrap()
            .success());
        let crosslink = root.path().join(".crosslink");
        std::fs::create_dir(&crosslink).unwrap();
        std::fs::write(crosslink.join("hook-config.json"), "{}").unwrap();
        root
    }

    #[test]
    fn projection_probe_fails_closed_before_cache_bootstrap() {
        let root = initialized_repository();
        let crosslink = root.path().join(".crosslink");
        let error = projection_needs_hydration(&crosslink).unwrap_err();
        assert!(error.to_string().contains("authority cache is missing"));
        assert!(!crosslink.join("issues.db").exists());
    }

    #[test]
    fn projection_probe_returns_false_only_for_an_observed_equal_frontier() {
        let root = initialized_repository();
        let crosslink = root.path().join(".crosslink");
        let sync = crate::sync::SyncManager::new(&crosslink).unwrap();
        assert_eq!(
            sync.init_cache_for_reconciliation(),
            crate::sync::ReconciliationCacheOutcome::Ready
        );
        assert!(projection_needs_hydration(&crosslink).unwrap());
        record_hydrated_ref_durable(&crosslink).unwrap();
        assert!(!projection_needs_hydration(&crosslink).unwrap());
        assert!(Command::new("git")
            .current_dir(sync.cache_path())
            .args(["update-ref", "-d", crate::hub_v3::META_REF])
            .status()
            .unwrap()
            .success());
        assert!(projection_needs_hydration(&crosslink).is_err());
    }

    #[test]
    fn projection_probe_rejects_corrupt_cache_without_creating_database() {
        let root = initialized_repository();
        let crosslink = root.path().join(".crosslink");
        let cache = crosslink.join(crate::sync::HUB_CACHE_DIR);
        std::fs::create_dir(&cache).unwrap();
        std::fs::write(cache.join("not-a-repository"), "invalid").unwrap();
        assert!(projection_needs_hydration(&crosslink).is_err());
        assert!(!crosslink.join("issues.db").exists());
    }
}
