use anyhow::{Context, Result};
use chrono::Utc;
use std::collections::{BTreeSet, HashSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use uuid::Uuid;

use crate::checkpoint::{
    read_watermark, write_checkpoint, CheckpointState, CompactComment, CompactIssue,
    CompactMilestone, CompactTimeEntry, LockEntry, SkewWarning, UnsignedEventWarning,
};
use crate::events::{Event, EventEnvelope, OrderingKey};
use crate::hub_source::{HubSource, WorktreeSource};
use crate::issue_file::{IssueFile, LockFileV2};

const LEASE_DURATION_SECS: i64 = 30;

const COMPACTION_LOCK_FILE: &str = "compaction.lock";

struct CompactionLockGuard {
    path: PathBuf,
}

struct StaleLockInfo {
    agent_id: String,

    acquired_at: chrono::DateTime<Utc>,
}

impl CompactionLockGuard {
    fn try_acquire(cache_dir: &Path, agent_id: &str, force: bool) -> Result<Option<Self>> {
        let lock_dir = cache_dir.join("checkpoint");
        fs::create_dir_all(&lock_dir)
            .with_context(|| format!("Failed to create checkpoint dir: {}", lock_dir.display()))?;
        let path = lock_dir.join(COMPACTION_LOCK_FILE);

        match Self::try_create(&path, agent_id) {
            Ok(guard) => return Ok(Some(guard)),
            Err(e) => {
                if !path.exists() {
                    return Err(e);
                }
            }
        }

        if let Some(info) = Self::read_lock_info(&path) {
            let age = Utc::now() - info.acquired_at;
            let is_stale = age.num_seconds() > LEASE_DURATION_SECS * 2;
            let is_self = info.agent_id == agent_id;

            if is_stale || (force && is_self) {
                let _ = fs::remove_file(&path);
                return Self::try_create(&path, agent_id).map(Some).or(Ok(None));
            }
        } else if force {
            let _ = fs::remove_file(&path);
            return Self::try_create(&path, agent_id).map(Some).or(Ok(None));
        }

        Ok(None)
    }

    fn try_create(path: &Path, agent_id: &str) -> Result<Self> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .with_context(|| "Compaction lock file already exists")?;

        let content = serde_json::json!({
            "agent_id": agent_id,
            "acquired_at": Utc::now().to_rfc3339(),
            "pid": std::process::id(),
        });
        file.write_all(content.to_string().as_bytes())
            .with_context(|| "Failed to write compaction lock file")?;

        Ok(Self {
            path: path.to_path_buf(),
        })
    }

    fn read_lock_info(path: &Path) -> Option<StaleLockInfo> {
        let content = fs::read_to_string(path).ok()?;
        let value: serde_json::Value = serde_json::from_str(&content).ok()?;
        let agent_id = value.get("agent_id")?.as_str()?.to_string();
        let acquired_str = value.get("acquired_at")?.as_str()?;
        let acquired_at = chrono::DateTime::parse_from_rfc3339(acquired_str)
            .ok()?
            .with_timezone(&Utc);
        Some(StaleLockInfo {
            agent_id,
            acquired_at,
        })
    }
}

impl Drop for CompactionLockGuard {
    fn drop(&mut self) {
        if let Err(e) = fs::remove_file(&self.path) {
            tracing::warn!(
                "failed to remove compaction lock file {}: {}",
                self.path.display(),
                e
            );
        }
    }
}

const SKEW_THRESHOLD_SECS: i64 = 60;

#[derive(Debug, Clone)]
pub struct CompactionResult {
    pub events_processed: usize,
    pub issues_materialized: usize,
    pub locks_materialized: usize,
    pub skew_warnings: usize,
    pub unsigned_warnings: usize,
    pub git_skew_violations: usize,
}

#[derive(Debug)]
pub struct ReductionOutcome {
    pub state: CheckpointState,

    pub changed_issues: HashSet<Uuid>,

    pub changed_locks: HashSet<i64>,

    pub events_processed: usize,
}

pub fn reduce(source: &dyn HubSource) -> Result<ReductionOutcome> {
    let mut state = source.read_checkpoint()?;

    let watermark = match state.watermark.clone() {
        Some(wm) => Some(wm),
        None => source.read_legacy_watermark()?,
    };

    let mut all_events = Vec::new();
    for agent_id in source.agent_ids()? {
        let events = source.read_events(&agent_id, watermark.as_ref())?;
        all_events.extend(events);
    }

    if state.watermark.is_none() {
        if let Some(ref wm) = watermark {
            state.watermark = Some(wm.clone());
        }
    }

    if all_events.is_empty() && watermark.is_some() {
        return Ok(ReductionOutcome {
            state,
            changed_issues: HashSet::new(),
            changed_locks: HashSet::new(),
            events_processed: 0,
        });
    }

    if watermark.is_none() {
        state = CheckpointState::default();
    }

    all_events.sort_by_cached_key(OrderingKey::from_envelope);

    let events_processed = all_events.len();
    let mut changed_issues: HashSet<Uuid> = HashSet::new();
    let mut changed_locks: HashSet<i64> = HashSet::new();

    if watermark.is_none() {
        state.skew_warnings.clear();
        state.unsigned_event_warnings.clear();
    }

    let allowed_signers_path: PathBuf = source
        .allowed_signers_file()?
        .unwrap_or_else(|| PathBuf::from("/dev/null/no-allowed-signers"));

    for envelope in &all_events {
        detect_clock_skew(&mut state, envelope);
        check_unsigned(&mut state, envelope, &allowed_signers_path);
        apply(
            &mut state,
            envelope,
            &mut changed_issues,
            &mut changed_locks,
        );
    }

    if let Some(last) = all_events.last() {
        state.watermark = Some(OrderingKey::from_envelope(last));
    }

    Ok(ReductionOutcome {
        state,
        changed_issues,
        changed_locks,
        events_processed,
    })
}

pub fn compact(
    cache_dir: &Path,
    agent_id: &str,
    force: bool,
    _hub_lock: &crate::sync::HubWriteLock,
) -> Result<Option<CompactionResult>> {
    let Some(_lock_guard) = CompactionLockGuard::try_acquire(cache_dir, agent_id, force)? else {
        return Ok(None);
    };

    let source = WorktreeSource::new(cache_dir);
    let outcome = reduce(&source)?;

    if outcome.events_processed == 0 && outcome.state.watermark.is_some() {
        let git_violations = crate::clock_skew::detect_git_skew_violations(cache_dir)
            .unwrap_or_else(|e| {
                tracing::warn!("git skew detection failed, defaulting to no violations: {e}");
                Vec::new()
            });
        let git_skew_violations = git_violations.len();
        crate::clock_skew::write_skew_violations(cache_dir, &git_violations)?;

        let mut state = outcome.state;
        state.compaction_lease = None;
        write_checkpoint(cache_dir, &state)?;

        return Ok(Some(CompactionResult {
            events_processed: 0,
            issues_materialized: 0,
            locks_materialized: 0,
            skew_warnings: state.skew_warnings.len(),
            unsigned_warnings: state.unsigned_event_warnings.len(),
            git_skew_violations,
        }));
    }

    let ReductionOutcome {
        mut state,
        changed_issues,
        changed_locks,
        events_processed,
    } = outcome;

    materialize(cache_dir, &state, &changed_issues, &changed_locks)?;

    let git_violations =
        crate::clock_skew::detect_git_skew_violations(cache_dir).unwrap_or_else(|e| {
            tracing::warn!("git skew detection failed, defaulting to no violations: {e}");
            Vec::new()
        });
    let git_skew_violations = git_violations.len();
    crate::clock_skew::write_skew_violations(cache_dir, &git_violations)?;

    let issues_materialized = changed_issues.len();
    let locks_materialized = changed_locks.len();
    let skew_warnings = state.skew_warnings.len();
    let unsigned_warnings = state.unsigned_event_warnings.len();

    state.compaction_lease = None;
    write_checkpoint(cache_dir, &state)?;

    Ok(Some(CompactionResult {
        events_processed,
        issues_materialized,
        locks_materialized,
        skew_warnings,
        unsigned_warnings,
        git_skew_violations,
    }))
}

pub fn prune_events(cache_dir: &Path, agent_id: &str) -> Result<usize> {
    let Some(watermark) = read_watermark(cache_dir)? else {
        return Ok(0);
    };

    let log_path = cache_dir.join("agents").join(agent_id).join("events.log");
    if !log_path.exists() {
        return Ok(0);
    }

    let all_events = crate::events::read_events(&log_path)?;
    let before_count = all_events.len();
    let remaining: Vec<_> = all_events
        .into_iter()
        .filter(|e| OrderingKey::from_envelope(e) > watermark)
        .collect();

    let pruned = before_count - remaining.len();
    if pruned > 0 {
        let codec = crate::events::NdjsonCodec;
        let bytes = <crate::events::NdjsonCodec as crate::events::EventCodec>::encode_batch(
            &codec, &remaining,
        )?;
        crate::utils::atomic_write(&log_path, &bytes)
            .with_context(|| format!("Failed to write pruned log: {}", log_path.display()))?;
    }

    Ok(pruned)
}

fn apply(
    state: &mut CheckpointState,
    envelope: &EventEnvelope,
    changed_issues: &mut HashSet<Uuid>,
    changed_locks: &mut HashSet<i64>,
) {
    match &envelope.event {
        Event::LockClaimed {
            issue_display_id,
            branch,
        } => {
            apply_lock_event(
                state,
                envelope,
                changed_locks,
                *issue_display_id,
                Some(branch),
            );
        }
        Event::LockReleased { issue_display_id } => {
            apply_lock_event(state, envelope, changed_locks, *issue_display_id, None);
        }

        Event::MilestoneCreated {
            uuid,
            display_id,
            name,
            description,
            created_at,
        } => {
            apply_milestone_created(
                state,
                *uuid,
                *display_id,
                name,
                description.as_ref(),
                *created_at,
            );
        }
        Event::MilestoneClosed { uuid, closed_at } => {
            if let Some(m) = state.milestones.get_mut(uuid) {
                m.status = crate::models::IssueStatus::Closed;
                m.closed_at = Some(*closed_at);
            }
        }
        Event::MilestoneDeleted { uuid } => {
            apply_milestone_deleted(state, *uuid, changed_issues);
        }
        _ => apply_issue_event(state, envelope, changed_issues),
    }
}

const fn event_issue_target(event: &Event) -> Option<Uuid> {
    match event {
        Event::IssueCreated { uuid, .. }
        | Event::IssueUpdated { uuid, .. }
        | Event::StatusChanged { uuid, .. }
        | Event::IssueDeleted { uuid } => Some(*uuid),
        Event::DependencyAdded { blocked_uuid, .. }
        | Event::DependencyRemoved { blocked_uuid, .. } => Some(*blocked_uuid),
        Event::MilestoneAssigned { issue_uuid, .. }
        | Event::LabelAdded { issue_uuid, .. }
        | Event::LabelRemoved { issue_uuid, .. }
        | Event::ParentChanged { issue_uuid, .. }
        | Event::CommentAdded { issue_uuid, .. }
        | Event::TimeEntryAdded { issue_uuid, .. }
        | Event::ScheduleChanged { issue_uuid, .. } => Some(*issue_uuid),

        Event::RelationAdded { .. }
        | Event::RelationRemoved { .. }
        | Event::LockClaimed { .. }
        | Event::LockReleased { .. }
        | Event::MilestoneCreated { .. }
        | Event::MilestoneClosed { .. }
        | Event::MilestoneDeleted { .. } => None,
    }
}

fn apply_issue_event(
    state: &mut CheckpointState,
    envelope: &EventEnvelope,
    changed_issues: &mut HashSet<Uuid>,
) {
    if let Some(target) = event_issue_target(&envelope.event) {
        if state.deleted_issues.contains(&target) {
            return;
        }
    }

    match &envelope.event {
        Event::IssueCreated {
            uuid,
            title,
            description,
            priority,
            labels,
            parent_uuid,
            created_by,
            display_id,
            scheduled_at,
            due_at,
        } => {
            apply_issue_created(
                state,
                envelope,
                changed_issues,
                &IssueCreatedArgs {
                    uuid: *uuid,
                    title,
                    description: description.as_ref(),
                    priority,
                    labels,
                    parent_uuid: *parent_uuid,
                    created_by,
                    carried_display_id: *display_id,
                    scheduled_at: *scheduled_at,
                    due_at: *due_at,
                },
            );
        }
        Event::IssueUpdated {
            uuid,
            title,
            description,
            priority,
        } => {
            apply_issue_field(state, envelope, changed_issues, *uuid, |issue| {
                if let Some(t) = title {
                    issue.title.clone_from(t);
                }
                if let Some(d) = description {
                    issue.description = Some(d.clone());
                }
                if let Some(p) = priority {
                    if let Ok(v) = p.parse() {
                        issue.priority = v;
                    }
                }
            });
        }
        Event::StatusChanged {
            uuid,
            new_status,
            closed_at,
        } => {
            apply_issue_field(state, envelope, changed_issues, *uuid, |issue| {
                issue.status = new_status.parse().unwrap_or(issue.status);
                issue.closed_at = *closed_at;
            });
        }
        Event::ScheduleChanged {
            issue_uuid,
            scheduled_at,
            due_at,
        } => {
            apply_issue_field(state, envelope, changed_issues, *issue_uuid, |issue| {
                issue.scheduled_at = *scheduled_at;
                issue.due_at = *due_at;
            });
        }
        Event::IssueDeleted { uuid } => {
            state.issues.remove(uuid);
            state.deleted_issues.insert(*uuid);
            changed_issues.insert(*uuid);
        }
        Event::CommentAdded {
            issue_uuid,
            comment_uuid,
            display_id,
            author,
            content,
            created_at,
            kind,
            trigger_type,
            intervention_context,
            driver_key_fingerprint,
            signed_by,
            signature,
        } => {
            let claimed = adopt_comment_id(state, *display_id);
            apply_issue_field(state, envelope, changed_issues, *issue_uuid, |issue| {
                issue
                    .comments
                    .entry(*comment_uuid)
                    .or_insert_with(|| CompactComment {
                        display_id: Some(claimed),
                        author: author.clone(),
                        content: content.clone(),
                        created_at: *created_at,
                        kind: kind.clone(),
                        trigger_type: trigger_type.clone(),
                        intervention_context: intervention_context.clone(),
                        driver_key_fingerprint: driver_key_fingerprint.clone(),
                        signed_by: signed_by.clone(),
                        signature: signature.clone(),
                    });
            });
        }
        Event::TimeEntryAdded {
            issue_uuid,
            entry_uuid,
            display_id,
            started_at,
            ended_at,
            duration_seconds,
        } => {
            apply_issue_field(state, envelope, changed_issues, *issue_uuid, |issue| {
                issue
                    .time_entries
                    .entry(*entry_uuid)
                    .or_insert_with(|| CompactTimeEntry {
                        display_id: *display_id,
                        started_at: *started_at,
                        ended_at: *ended_at,
                        duration_seconds: *duration_seconds,
                    });
            });
        }
        _ => apply_graph_event(state, envelope, changed_issues),
    }
}

const fn adopt_comment_id(state: &mut CheckpointState, carried: Option<i64>) -> i64 {
    if let Some(id) = carried {
        if id >= state.next_comment_id {
            state.next_comment_id = id + 1;
        }
        id
    } else {
        let id = state.next_comment_id;
        state.next_comment_id += 1;
        id
    }
}

fn apply_milestone_created(
    state: &mut CheckpointState,
    uuid: Uuid,
    carried_display_id: Option<i64>,
    name: &str,
    description: Option<&String>,
    created_at: chrono::DateTime<Utc>,
) {
    if state.milestones.contains_key(&uuid) {
        return;
    }
    let display_id = adopt_milestone_id(state, carried_display_id);
    state.milestones.insert(
        uuid,
        CompactMilestone {
            uuid,
            display_id: Some(display_id),
            name: name.to_string(),
            description: description.cloned(),
            status: crate::models::IssueStatus::Open,
            created_at,
            closed_at: None,
        },
    );
}

fn apply_milestone_deleted(
    state: &mut CheckpointState,
    uuid: Uuid,
    changed_issues: &mut HashSet<Uuid>,
) {
    state.milestones.remove(&uuid);
    for (issue_uuid, issue) in &mut state.issues {
        if issue.milestone_uuid == Some(uuid) {
            issue.milestone_uuid = None;
            changed_issues.insert(*issue_uuid);
        }
    }
}

const fn adopt_milestone_id(state: &mut CheckpointState, carried: Option<i64>) -> i64 {
    if let Some(id) = carried {
        if id >= state.next_milestone_id {
            state.next_milestone_id = id + 1;
        }
        id
    } else {
        let id = state.next_milestone_id;
        state.next_milestone_id += 1;
        id
    }
}

fn apply_graph_event(
    state: &mut CheckpointState,
    envelope: &EventEnvelope,
    changed_issues: &mut HashSet<Uuid>,
) {
    match &envelope.event {
        Event::DependencyAdded {
            blocked_uuid,
            blocker_uuid,
        } => {
            apply_issue_field(state, envelope, changed_issues, *blocked_uuid, |i| {
                i.blockers.insert(*blocker_uuid);
            });
        }
        Event::DependencyRemoved {
            blocked_uuid,
            blocker_uuid,
        } => {
            apply_issue_field(state, envelope, changed_issues, *blocked_uuid, |i| {
                i.blockers.remove(blocker_uuid);
            });
        }
        Event::RelationAdded { uuid_a, uuid_b } => {
            apply_issue_field(state, envelope, changed_issues, *uuid_a, |i| {
                i.related.insert(*uuid_b);
            });
            apply_issue_field(state, envelope, changed_issues, *uuid_b, |i| {
                i.related.insert(*uuid_a);
            });
        }
        Event::RelationRemoved { uuid_a, uuid_b } => {
            apply_issue_field(state, envelope, changed_issues, *uuid_a, |i| {
                i.related.remove(uuid_b);
            });
            apply_issue_field(state, envelope, changed_issues, *uuid_b, |i| {
                i.related.remove(uuid_a);
            });
        }
        Event::MilestoneAssigned {
            issue_uuid,
            milestone_uuid,
        } => {
            apply_issue_field(state, envelope, changed_issues, *issue_uuid, |i| {
                i.milestone_uuid = *milestone_uuid;
            });
        }
        Event::LabelAdded { issue_uuid, label } => {
            apply_issue_field(state, envelope, changed_issues, *issue_uuid, |i| {
                i.labels.insert(label.clone());
            });
        }
        Event::LabelRemoved { issue_uuid, label } => {
            apply_issue_field(state, envelope, changed_issues, *issue_uuid, |i| {
                i.labels.remove(label);
            });
        }
        Event::ParentChanged {
            issue_uuid,
            new_parent_uuid,
        } => {
            apply_issue_field(state, envelope, changed_issues, *issue_uuid, |i| {
                i.parent_uuid = *new_parent_uuid;
            });
        }
        _ => {}
    }
}

struct IssueCreatedArgs<'a> {
    uuid: Uuid,
    title: &'a str,
    description: Option<&'a String>,
    priority: &'a str,
    labels: &'a [String],
    parent_uuid: Option<Uuid>,
    created_by: &'a str,

    carried_display_id: Option<i64>,
    scheduled_at: Option<chrono::DateTime<Utc>>,
    due_at: Option<chrono::DateTime<Utc>>,
}

fn apply_issue_created(
    state: &mut CheckpointState,
    envelope: &EventEnvelope,
    changed_issues: &mut HashSet<Uuid>,
    args: &IssueCreatedArgs<'_>,
) {
    if state.issues.contains_key(&args.uuid) {
        return;
    }
    let display_id = assign_issue_display_id(state, args.uuid, args.carried_display_id);
    state.display_id_map.insert(args.uuid, display_id);
    state.issues.insert(
        args.uuid,
        CompactIssue {
            uuid: args.uuid,
            display_id: Some(display_id),
            title: args.title.to_string(),
            description: args.description.cloned(),
            status: crate::models::IssueStatus::Open,
            priority: args
                .priority
                .parse()
                .unwrap_or(crate::models::Priority::Medium),
            parent_uuid: args.parent_uuid,
            created_by: args.created_by.to_string(),
            created_at: envelope.timestamp,
            updated_at: envelope.timestamp,
            closed_at: None,
            scheduled_at: args.scheduled_at,
            due_at: args.due_at,
            labels: args.labels.iter().cloned().collect(),
            blockers: BTreeSet::new(),
            related: BTreeSet::new(),
            milestone_uuid: None,
            comments: std::collections::BTreeMap::new(),
            time_entries: std::collections::BTreeMap::new(),
        },
    );
    changed_issues.insert(args.uuid);
}

fn assign_issue_display_id(state: &mut CheckpointState, uuid: Uuid, carried: Option<i64>) -> i64 {
    if let Some(&existing) = state.display_id_map.get(&uuid) {
        return existing;
    }
    match carried {
        Some(id) => {
            let already_taken = state.display_id_map.values().any(|&v| v == id);
            if already_taken {
                allocate_next_free_display_id(state)
            } else {
                if id >= state.next_display_id {
                    state.next_display_id = id + 1;
                }
                id
            }
        }
        None => allocate_next_free_display_id(state),
    }
}

fn allocate_next_free_display_id(state: &mut CheckpointState) -> i64 {
    let mut id = state.next_display_id;
    while state.display_id_map.values().any(|&v| v == id) {
        id += 1;
    }
    state.next_display_id = id + 1;
    id
}

fn apply_issue_field(
    state: &mut CheckpointState,
    envelope: &EventEnvelope,
    changed_issues: &mut HashSet<Uuid>,
    uuid: Uuid,
    mutate: impl FnOnce(&mut CompactIssue),
) {
    if let Some(issue) = state.issues.get_mut(&uuid) {
        mutate(issue);
        issue.updated_at = envelope.timestamp;
        changed_issues.insert(uuid);
    }
}

fn apply_lock_event(
    state: &mut CheckpointState,
    envelope: &EventEnvelope,
    changed_locks: &mut HashSet<i64>,
    issue_display_id: i64,
    branch_opt: Option<&Option<String>>,
) {
    if let Some(branch) = branch_opt {
        if let Some(existing) = state.locks.get(&issue_display_id) {
            if existing.agent_id != envelope.agent_id {
                return;
            }
        }
        state.locks.insert(
            issue_display_id,
            LockEntry {
                agent_id: envelope.agent_id.clone(),
                branch: branch.clone(),
                claimed_at: envelope.timestamp,
            },
        );
        changed_locks.insert(issue_display_id);
    } else if let Some(existing) = state.locks.get(&issue_display_id) {
        if existing.agent_id == envelope.agent_id {
            state.locks.remove(&issue_display_id);
            changed_locks.insert(issue_display_id);
        }
    }
}

fn materialize(
    cache_dir: &Path,
    state: &CheckpointState,
    changed_issues: &HashSet<Uuid>,
    changed_locks: &HashSet<i64>,
) -> Result<()> {
    let issues_dir = cache_dir.join("issues");
    let locks_dir = cache_dir.join("locks");
    let meta_dir = cache_dir.join("meta");

    let layout_version = crate::issue_file::read_layout_version(&meta_dir)
        .unwrap_or(crate::issue_file::CURRENT_LAYOUT_VERSION);

    for uuid in changed_issues {
        if state.deleted_issues.contains(uuid) {
            continue;
        }
        if let Some(compact) = state.issues.get(uuid) {
            let issue_file = compact_to_issue_file(compact);
            let content = serde_json::to_string_pretty(&issue_file)?;

            if layout_version >= 2 {
                let single_issue_dir = issues_dir.join(uuid.to_string());
                std::fs::create_dir_all(&single_issue_dir).with_context(|| {
                    format!("Failed to create issue dir: {}", single_issue_dir.display())
                })?;
                let path = single_issue_dir.join("issue.json");
                crate::utils::atomic_write(&path, content.as_bytes())?;

                let v1_path = issues_dir.join(format!("{uuid}.json"));
                if v1_path.exists() {
                    let _ = std::fs::remove_file(&v1_path);
                }
            } else {
                let path = issues_dir.join(format!("{uuid}.json"));
                crate::utils::atomic_write(&path, content.as_bytes())?;
            }
        }
    }

    if !meta_dir.join("version.json").exists() {
        let _ = crate::issue_file::write_layout_version(&meta_dir, layout_version);
    }

    std::fs::create_dir_all(&locks_dir)?;
    for display_id in changed_locks {
        let lock_path = locks_dir.join(format!("{display_id}.json"));
        if let Some(lock_entry) = state.locks.get(display_id) {
            let lock_file = LockFileV2 {
                issue_id: *display_id,
                agent_id: lock_entry.agent_id.clone(),
                branch: lock_entry.branch.clone(),
                claimed_at: lock_entry.claimed_at,
                signed_by: None,
            };
            let content = serde_json::to_string_pretty(&lock_file)?;
            crate::utils::atomic_write(&lock_path, content.as_bytes())?;
        } else if lock_path.exists() {
            std::fs::remove_file(&lock_path)
                .with_context(|| format!("Failed to remove lock file: {}", lock_path.display()))?;
        }
    }

    Ok(())
}

fn compact_to_issue_file(compact: &CompactIssue) -> IssueFile {
    IssueFile::from(compact)
}

fn detect_clock_skew(state: &mut CheckpointState, envelope: &EventEnvelope) {
    let now = Utc::now();
    let future_skew = (envelope.timestamp - now).num_seconds();
    if future_skew > SKEW_THRESHOLD_SECS {
        state.skew_warnings.push(SkewWarning {
            agent_id: envelope.agent_id.clone(),
            skew_seconds: future_skew,
            event_timestamp: envelope.timestamp,
        });
    }
}

fn check_unsigned(
    state: &mut CheckpointState,
    envelope: &EventEnvelope,
    allowed_signers_path: &Path,
) {
    let unsigned = envelope.signed_by.is_none() || envelope.signature.is_none();
    let invalid = !unsigned
        && allowed_signers_path.exists()
        && matches!(
            crate::events::verify_event_signature(envelope, allowed_signers_path),
            Ok(false)
        );
    if unsigned || invalid {
        state.unsigned_event_warnings.push(UnsignedEventWarning {
            agent_id: envelope.agent_id.clone(),
            agent_seq: envelope.agent_seq,
            timestamp: envelope.timestamp,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checkpoint::{read_checkpoint, CompactionLease};
    use crate::events::{append_event, Event, EventEnvelope};
    use chrono::Duration;

    fn try_acquire_lease(state: &mut CheckpointState, agent_id: &str) -> bool {
        let now = Utc::now();
        if let Some(ref lease) = state.compaction_lease {
            if lease.agent_id == agent_id {
            } else if lease.expires_at > now {
                let holder_dead = lease.pid.is_some_and(|pid| !is_pid_alive(pid));
                if !holder_dead {
                    return false;
                }
            }
        }

        state.compaction_lease = Some(CompactionLease {
            agent_id: agent_id.to_string(),
            acquired_at: now,
            expires_at: now + Duration::seconds(LEASE_DURATION_SECS),
            pid: Some(std::process::id()),
        });
        true
    }

    #[cfg(windows)]
    fn is_pid_alive(pid: u32) -> bool {
        std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output()
            .is_ok_and(|output| {
                let stdout = String::from_utf8_lossy(&output.stdout);
                stdout.contains(&pid.to_string())
            })
    }

    #[cfg(not(windows))]
    fn is_pid_alive(pid: u32) -> bool {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    fn release_lease(state: &mut CheckpointState) {
        state.compaction_lease = None;
    }

    fn make_envelope(agent_id: &str, seq: u64, event: Event) -> EventEnvelope {
        EventEnvelope {
            agent_id: agent_id.to_string(),
            agent_seq: seq,
            timestamp: Utc::now(),
            event,
            signed_by: None,
            signature: None,
        }
    }

    fn setup_cache(dir: &Path) {
        std::fs::create_dir_all(dir.join("agents")).unwrap();
        std::fs::create_dir_all(dir.join("issues")).unwrap();
        std::fs::create_dir_all(dir.join("locks")).unwrap();
        std::fs::create_dir_all(dir.join("checkpoint")).unwrap();

        crate::issue_file::write_layout_version(
            &dir.join("meta"),
            crate::issue_file::CURRENT_LAYOUT_VERSION,
        )
        .unwrap();
    }

    fn test_hub_lock(cache_dir: &Path) -> crate::sync::HubWriteLock {
        let lock_path = cache_dir.join(".hub-write-lock");
        crate::sync::acquire_hub_lock(&lock_path).expect("failed to acquire hub write lock in test")
    }

    fn compact_t(
        cache_dir: &Path,
        agent_id: &str,
        force: bool,
    ) -> Result<Option<CompactionResult>> {
        let lock = test_hub_lock(cache_dir);
        compact(cache_dir, agent_id, force, &lock)
    }

    #[test]
    fn test_compact_empty() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path();
        setup_cache(cache_dir);

        let result = compact_t(cache_dir, "agent-1", false).unwrap().unwrap();
        assert_eq!(result.events_processed, 0);
        assert_eq!(result.issues_materialized, 0);
    }

    #[test]
    fn test_compact_issue_created() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path();
        setup_cache(cache_dir);

        let uuid = Uuid::new_v4();
        let log_path = cache_dir.join("agents/agent-1/events.log");
        let env = make_envelope(
            "agent-1",
            1,
            Event::IssueCreated {
                uuid,
                title: "Test issue".to_string(),
                description: Some("A description".to_string()),
                priority: "high".to_string(),
                labels: vec!["bug".to_string()],
                parent_uuid: None,
                created_by: "agent-1".to_string(),
                display_id: None,
                scheduled_at: None,
                due_at: None,
            },
        );
        append_event(&log_path, &env).unwrap();

        let result = compact_t(cache_dir, "agent-1", false).unwrap().unwrap();
        assert_eq!(result.events_processed, 1);
        assert_eq!(result.issues_materialized, 1);

        let issue_path = cache_dir
            .join("issues")
            .join(uuid.to_string())
            .join("issue.json");
        assert!(issue_path.exists());
        let issue: IssueFile =
            serde_json::from_str(&std::fs::read_to_string(&issue_path).unwrap()).unwrap();
        assert_eq!(issue.title, "Test issue");
        assert_eq!(issue.display_id, Some(1));
        assert_eq!(issue.priority, crate::models::Priority::High);
        assert_eq!(issue.labels, vec!["bug".to_string()]);
    }

    #[test]
    fn test_display_id_stability() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path();
        setup_cache(cache_dir);

        let uuid1 = Uuid::new_v4();
        let uuid2 = Uuid::new_v4();
        let log_path = cache_dir.join("agents/agent-1/events.log");

        let mut e1 = make_envelope(
            "agent-1",
            1,
            Event::IssueCreated {
                uuid: uuid1,
                title: "First".to_string(),
                description: None,
                priority: "medium".to_string(),
                labels: vec![],
                parent_uuid: None,
                created_by: "agent-1".to_string(),
                display_id: None,
                scheduled_at: None,
                due_at: None,
            },
        );
        e1.timestamp = Utc::now() - Duration::seconds(10);

        let e2 = make_envelope(
            "agent-1",
            2,
            Event::IssueCreated {
                uuid: uuid2,
                title: "Second".to_string(),
                description: None,
                priority: "medium".to_string(),
                labels: vec![],
                parent_uuid: None,
                created_by: "agent-1".to_string(),
                display_id: None,
                scheduled_at: None,
                due_at: None,
            },
        );

        append_event(&log_path, &e1).unwrap();
        append_event(&log_path, &e2).unwrap();

        compact_t(cache_dir, "agent-1", true).unwrap();

        let wm_path = cache_dir.join("checkpoint/watermark.json");
        if wm_path.exists() {
            std::fs::remove_file(&wm_path).unwrap();
        }

        compact_t(cache_dir, "agent-1", true).unwrap();

        let state = read_checkpoint(cache_dir).unwrap();
        assert_eq!(state.display_id_map[&uuid1], 1);
        assert_eq!(state.display_id_map[&uuid2], 2);
    }

    #[test]
    fn test_idempotent_issue_created() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path();
        setup_cache(cache_dir);

        let uuid = Uuid::new_v4();
        let log_path = cache_dir.join("agents/agent-1/events.log");

        let mut e1 = make_envelope(
            "agent-1",
            1,
            Event::IssueCreated {
                uuid,
                title: "Issue".to_string(),
                description: None,
                priority: "medium".to_string(),
                labels: vec![],
                parent_uuid: None,
                created_by: "agent-1".to_string(),
                display_id: None,
                scheduled_at: None,
                due_at: None,
            },
        );
        e1.timestamp = Utc::now() - Duration::seconds(1);
        let e2 = make_envelope(
            "agent-1",
            2,
            Event::IssueCreated {
                uuid,
                title: "Issue duplicate".to_string(),
                description: None,
                priority: "high".to_string(),
                labels: vec![],
                parent_uuid: None,
                created_by: "agent-1".to_string(),
                display_id: None,
                scheduled_at: None,
                due_at: None,
            },
        );

        append_event(&log_path, &e1).unwrap();
        append_event(&log_path, &e2).unwrap();

        compact_t(cache_dir, "agent-1", true).unwrap();

        let state = read_checkpoint(cache_dir).unwrap();
        assert_eq!(state.issues.len(), 1);
        assert_eq!(state.issues[&uuid].title, "Issue");
        assert_eq!(state.next_display_id, 2);
    }

    #[test]
    fn test_lock_contention_first_wins() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path();
        setup_cache(cache_dir);

        let log1 = cache_dir.join("agents/agent-1/events.log");
        let log2 = cache_dir.join("agents/agent-2/events.log");

        let mut e1 = make_envelope(
            "agent-1",
            1,
            Event::LockClaimed {
                issue_display_id: 1,
                branch: Some("feature/a".to_string()),
            },
        );
        e1.timestamp = Utc::now() - Duration::seconds(2);

        let mut e2 = make_envelope(
            "agent-2",
            1,
            Event::LockClaimed {
                issue_display_id: 1,
                branch: Some("feature/b".to_string()),
            },
        );
        e2.timestamp = Utc::now() - Duration::seconds(1);

        append_event(&log1, &e1).unwrap();
        append_event(&log2, &e2).unwrap();

        compact_t(cache_dir, "agent-1", true).unwrap();

        let state = read_checkpoint(cache_dir).unwrap();
        assert_eq!(state.locks[&1].agent_id, "agent-1");
    }

    #[test]
    fn test_lock_release() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path();
        setup_cache(cache_dir);

        let log = cache_dir.join("agents/agent-1/events.log");

        let mut e1 = make_envelope(
            "agent-1",
            1,
            Event::LockClaimed {
                issue_display_id: 1,
                branch: None,
            },
        );
        e1.timestamp = Utc::now() - Duration::seconds(2);

        let e2 = make_envelope(
            "agent-1",
            2,
            Event::LockReleased {
                issue_display_id: 1,
            },
        );

        append_event(&log, &e1).unwrap();
        append_event(&log, &e2).unwrap();

        compact_t(cache_dir, "agent-1", true).unwrap();

        let state = read_checkpoint(cache_dir).unwrap();
        assert!(state.locks.is_empty());

        assert!(!cache_dir.join("locks/1.json").exists());
    }

    #[test]
    fn test_issue_update_last_writer_wins() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path();
        setup_cache(cache_dir);

        let uuid = Uuid::new_v4();
        let log1 = cache_dir.join("agents/agent-1/events.log");
        let log2 = cache_dir.join("agents/agent-2/events.log");

        let mut e_create = make_envelope(
            "agent-1",
            1,
            Event::IssueCreated {
                uuid,
                title: "Original".to_string(),
                description: None,
                priority: "low".to_string(),
                labels: vec![],
                parent_uuid: None,
                created_by: "agent-1".to_string(),
                display_id: None,
                scheduled_at: None,
                due_at: None,
            },
        );
        e_create.timestamp = Utc::now() - Duration::seconds(10);

        let mut e_update1 = make_envelope(
            "agent-1",
            2,
            Event::IssueUpdated {
                uuid,
                title: Some("Agent 1 title".to_string()),
                description: None,
                priority: None,
            },
        );
        e_update1.timestamp = Utc::now() - Duration::seconds(5);

        let mut e_update2 = make_envelope(
            "agent-2",
            1,
            Event::IssueUpdated {
                uuid,
                title: Some("Agent 2 title".to_string()),
                description: Some("Agent 2 desc".to_string()),
                priority: None,
            },
        );
        e_update2.timestamp = Utc::now() - Duration::seconds(3);

        append_event(&log1, &e_create).unwrap();
        append_event(&log1, &e_update1).unwrap();
        append_event(&log2, &e_update2).unwrap();

        compact_t(cache_dir, "agent-1", true).unwrap();

        let state = read_checkpoint(cache_dir).unwrap();
        let issue = &state.issues[&uuid];
        assert_eq!(issue.title, "Agent 2 title");
        assert_eq!(issue.description.as_deref(), Some("Agent 2 desc"));
    }

    #[test]
    fn test_label_add_remove_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path();
        setup_cache(cache_dir);

        let uuid = Uuid::new_v4();
        let log = cache_dir.join("agents/agent-1/events.log");

        let mut e_create = make_envelope(
            "agent-1",
            1,
            Event::IssueCreated {
                uuid,
                title: "Test".to_string(),
                description: None,
                priority: "medium".to_string(),
                labels: vec![],
                parent_uuid: None,
                created_by: "agent-1".to_string(),
                display_id: None,
                scheduled_at: None,
                due_at: None,
            },
        );
        e_create.timestamp = Utc::now() - Duration::seconds(10);

        let mut e_add1 = make_envelope(
            "agent-1",
            2,
            Event::LabelAdded {
                issue_uuid: uuid,
                label: "bug".to_string(),
            },
        );
        e_add1.timestamp = Utc::now() - Duration::seconds(8);

        let mut e_add2 = make_envelope(
            "agent-1",
            3,
            Event::LabelAdded {
                issue_uuid: uuid,
                label: "bug".to_string(),
            },
        );
        e_add2.timestamp = Utc::now() - Duration::seconds(6);

        let e_remove = make_envelope(
            "agent-1",
            4,
            Event::LabelRemoved {
                issue_uuid: uuid,
                label: "bug".to_string(),
            },
        );

        append_event(&log, &e_create).unwrap();
        append_event(&log, &e_add1).unwrap();
        append_event(&log, &e_add2).unwrap();
        append_event(&log, &e_remove).unwrap();

        compact_t(cache_dir, "agent-1", true).unwrap();

        let state = read_checkpoint(cache_dir).unwrap();
        assert!(state.issues[&uuid].labels.is_empty());
    }

    #[test]
    fn test_dependency_add_remove() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path();
        setup_cache(cache_dir);

        let blocked = Uuid::new_v4();
        let blocker = Uuid::new_v4();
        let log = cache_dir.join("agents/agent-1/events.log");

        let mut e1 = make_envelope(
            "agent-1",
            1,
            Event::IssueCreated {
                uuid: blocked,
                title: "Blocked".to_string(),
                description: None,
                priority: "medium".to_string(),
                labels: vec![],
                parent_uuid: None,
                created_by: "agent-1".to_string(),
                display_id: None,
                scheduled_at: None,
                due_at: None,
            },
        );
        e1.timestamp = Utc::now() - Duration::seconds(10);

        let mut e2 = make_envelope(
            "agent-1",
            2,
            Event::DependencyAdded {
                blocked_uuid: blocked,
                blocker_uuid: blocker,
            },
        );
        e2.timestamp = Utc::now() - Duration::seconds(5);

        append_event(&log, &e1).unwrap();
        append_event(&log, &e2).unwrap();

        compact_t(cache_dir, "agent-1", true).unwrap();

        let state = read_checkpoint(cache_dir).unwrap();
        assert!(state.issues[&blocked].blockers.contains(&blocker));
    }

    #[test]
    fn test_relation_bidirectional() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path();
        setup_cache(cache_dir);

        let uuid_a = Uuid::new_v4();
        let uuid_b = Uuid::new_v4();
        let log = cache_dir.join("agents/agent-1/events.log");

        let mut e1 = make_envelope(
            "agent-1",
            1,
            Event::IssueCreated {
                uuid: uuid_a,
                title: "A".to_string(),
                description: None,
                priority: "medium".to_string(),
                labels: vec![],
                parent_uuid: None,
                created_by: "agent-1".to_string(),
                display_id: None,
                scheduled_at: None,
                due_at: None,
            },
        );
        e1.timestamp = Utc::now() - Duration::seconds(10);

        let mut e2 = make_envelope(
            "agent-1",
            2,
            Event::IssueCreated {
                uuid: uuid_b,
                title: "B".to_string(),
                description: None,
                priority: "medium".to_string(),
                labels: vec![],
                parent_uuid: None,
                created_by: "agent-1".to_string(),
                display_id: None,
                scheduled_at: None,
                due_at: None,
            },
        );
        e2.timestamp = Utc::now() - Duration::seconds(9);

        let e3 = make_envelope("agent-1", 3, Event::RelationAdded { uuid_a, uuid_b });

        append_event(&log, &e1).unwrap();
        append_event(&log, &e2).unwrap();
        append_event(&log, &e3).unwrap();

        compact_t(cache_dir, "agent-1", true).unwrap();

        let state = read_checkpoint(cache_dir).unwrap();
        assert!(state.issues[&uuid_a].related.contains(&uuid_b));
        assert!(state.issues[&uuid_b].related.contains(&uuid_a));
    }

    #[test]
    fn test_lease_acquire_release() {
        let mut state = CheckpointState::default();

        assert!(try_acquire_lease(&mut state, "agent-1"));
        assert!(state.compaction_lease.is_some());
        assert_eq!(state.compaction_lease.as_ref().unwrap().agent_id, "agent-1");

        assert!(try_acquire_lease(&mut state, "agent-1"));

        assert!(!try_acquire_lease(&mut state, "agent-2"));

        release_lease(&mut state);
        assert!(state.compaction_lease.is_none());

        assert!(try_acquire_lease(&mut state, "agent-2"));
    }

    #[test]
    fn test_lease_expiry() {
        let mut state = CheckpointState {
            compaction_lease: Some(CompactionLease {
                agent_id: "agent-1".to_string(),
                acquired_at: Utc::now() - Duration::seconds(60),
                expires_at: Utc::now() - Duration::seconds(30),
                pid: None,
            }),
            ..Default::default()
        };

        assert!(try_acquire_lease(&mut state, "agent-2"));
        assert_eq!(state.compaction_lease.as_ref().unwrap().agent_id, "agent-2");
    }

    #[test]
    fn test_lease_stale_by_dead_pid() {
        let dead_pid = u32::MAX - 1;

        let mut state = CheckpointState {
            compaction_lease: Some(CompactionLease {
                agent_id: "agent-1".to_string(),
                acquired_at: Utc::now(),
                expires_at: Utc::now() + Duration::seconds(300),
                pid: Some(dead_pid),
            }),
            ..Default::default()
        };

        assert!(try_acquire_lease(&mut state, "agent-2"));
        assert_eq!(state.compaction_lease.as_ref().unwrap().agent_id, "agent-2");
    }

    #[test]
    fn test_lease_not_stale_when_pid_alive() {
        let mut state = CheckpointState {
            compaction_lease: Some(CompactionLease {
                agent_id: "agent-1".to_string(),
                acquired_at: Utc::now(),
                expires_at: Utc::now() + Duration::seconds(300),
                pid: Some(std::process::id()),
            }),
            ..Default::default()
        };

        assert!(!try_acquire_lease(&mut state, "agent-2"));
    }

    #[test]
    fn test_lease_no_pid_falls_back_to_expiry() {
        let mut state = CheckpointState {
            compaction_lease: Some(CompactionLease {
                agent_id: "agent-1".to_string(),
                acquired_at: Utc::now(),
                expires_at: Utc::now() + Duration::seconds(300),
                pid: None,
            }),
            ..Default::default()
        };

        assert!(!try_acquire_lease(&mut state, "agent-2"));
    }

    #[test]
    fn test_clock_skew_detection() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path();
        setup_cache(cache_dir);

        let uuid = Uuid::new_v4();
        let log = cache_dir.join("agents/agent-1/events.log");

        let mut env = make_envelope(
            "agent-1",
            1,
            Event::IssueCreated {
                uuid,
                title: "Skewed".to_string(),
                description: None,
                priority: "medium".to_string(),
                labels: vec![],
                parent_uuid: None,
                created_by: "agent-1".to_string(),
                display_id: None,
                scheduled_at: None,
                due_at: None,
            },
        );

        env.timestamp = Utc::now() + Duration::seconds(120);

        append_event(&log, &env).unwrap();

        let result = compact_t(cache_dir, "agent-1", true).unwrap().unwrap();
        assert!(result.skew_warnings > 0);
    }

    #[test]
    fn test_unsigned_event_warning() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path();
        setup_cache(cache_dir);

        let uuid = Uuid::new_v4();
        let log = cache_dir.join("agents/agent-1/events.log");

        let env = make_envelope(
            "agent-1",
            1,
            Event::IssueCreated {
                uuid,
                title: "Unsigned".to_string(),
                description: None,
                priority: "medium".to_string(),
                labels: vec![],
                parent_uuid: None,
                created_by: "agent-1".to_string(),
                display_id: None,
                scheduled_at: None,
                due_at: None,
            },
        );

        append_event(&log, &env).unwrap();

        let result = compact_t(cache_dir, "agent-1", true).unwrap().unwrap();
        assert!(result.unsigned_warnings > 0);
    }

    #[test]
    fn test_prune_events() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path();
        setup_cache(cache_dir);

        let uuid = Uuid::new_v4();
        let log = cache_dir.join("agents/agent-1/events.log");

        let mut e1 = make_envelope(
            "agent-1",
            1,
            Event::IssueCreated {
                uuid,
                title: "Prunable".to_string(),
                description: None,
                priority: "medium".to_string(),
                labels: vec![],
                parent_uuid: None,
                created_by: "agent-1".to_string(),
                display_id: None,
                scheduled_at: None,
                due_at: None,
            },
        );
        e1.timestamp = Utc::now() - Duration::seconds(10);

        let e2 = make_envelope(
            "agent-1",
            2,
            Event::LabelAdded {
                issue_uuid: uuid,
                label: "bug".to_string(),
            },
        );

        append_event(&log, &e1).unwrap();
        append_event(&log, &e2).unwrap();

        compact_t(cache_dir, "agent-1", true).unwrap();

        let pruned = prune_events(cache_dir, "agent-1").unwrap();
        assert_eq!(pruned, 2);

        let remaining = crate::events::read_events(&log).unwrap();
        assert!(remaining.is_empty());
    }

    #[test]
    fn test_deterministic_reduction_order() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path();
        setup_cache(cache_dir);

        let uuid = Uuid::new_v4();
        let log1 = cache_dir.join("agents/agent-1/events.log");
        let log2 = cache_dir.join("agents/agent-2/events.log");

        let mut e_create = make_envelope(
            "agent-1",
            1,
            Event::IssueCreated {
                uuid,
                title: "Test".to_string(),
                description: None,
                priority: "medium".to_string(),
                labels: vec![],
                parent_uuid: None,
                created_by: "agent-1".to_string(),
                display_id: None,
                scheduled_at: None,
                due_at: None,
            },
        );
        e_create.timestamp = Utc::now() - Duration::seconds(20);

        let mut e_label1 = make_envelope(
            "agent-2",
            1,
            Event::LabelAdded {
                issue_uuid: uuid,
                label: "feature".to_string(),
            },
        );
        e_label1.timestamp = Utc::now() - Duration::seconds(10);

        let mut e_label2 = make_envelope(
            "agent-1",
            2,
            Event::LabelAdded {
                issue_uuid: uuid,
                label: "urgent".to_string(),
            },
        );
        e_label2.timestamp = Utc::now() - Duration::seconds(5);

        append_event(&log1, &e_create).unwrap();
        append_event(&log2, &e_label1).unwrap();
        append_event(&log1, &e_label2).unwrap();

        compact_t(cache_dir, "agent-1", true).unwrap();

        let state = read_checkpoint(cache_dir).unwrap();
        let issue = &state.issues[&uuid];
        assert!(issue.labels.contains("feature"));
        assert!(issue.labels.contains("urgent"));
        assert_eq!(issue.labels.len(), 2);
    }

    #[test]
    fn test_status_changed() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path();
        setup_cache(cache_dir);

        let uuid = Uuid::new_v4();
        let log = cache_dir.join("agents/agent-1/events.log");

        let mut e1 = make_envelope(
            "agent-1",
            1,
            Event::IssueCreated {
                uuid,
                title: "To close".to_string(),
                description: None,
                priority: "medium".to_string(),
                labels: vec![],
                parent_uuid: None,
                created_by: "agent-1".to_string(),
                display_id: None,
                scheduled_at: None,
                due_at: None,
            },
        );
        e1.timestamp = Utc::now() - Duration::seconds(10);

        let closed_at = Utc::now();
        let e2 = make_envelope(
            "agent-1",
            2,
            Event::StatusChanged {
                uuid,
                new_status: "closed".to_string(),
                closed_at: Some(closed_at),
            },
        );

        append_event(&log, &e1).unwrap();
        append_event(&log, &e2).unwrap();

        compact_t(cache_dir, "agent-1", true).unwrap();

        let state = read_checkpoint(cache_dir).unwrap();
        assert_eq!(
            state.issues[&uuid].status,
            crate::models::IssueStatus::Closed
        );
        assert!(state.issues[&uuid].closed_at.is_some());
    }

    #[test]
    fn test_compact_respects_file_lock() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path();
        setup_cache(cache_dir);

        let lock_path = cache_dir.join("checkpoint").join(COMPACTION_LOCK_FILE);
        let content = serde_json::json!({
            "agent_id": "agent-2",
            "acquired_at": Utc::now().to_rfc3339(),
            "pid": std::process::id(),
        });
        std::fs::write(&lock_path, content.to_string()).unwrap();

        let result = compact_t(cache_dir, "agent-1", false).unwrap();
        assert!(result.is_none());

        assert!(lock_path.exists());
    }

    #[test]
    fn test_compact_force_overrides_stale_lock() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path();
        setup_cache(cache_dir);

        let lock_path = cache_dir.join("checkpoint").join(COMPACTION_LOCK_FILE);
        let stale_time = Utc::now() - Duration::seconds(LEASE_DURATION_SECS * 3);
        let content = serde_json::json!({
            "agent_id": "agent-2",
            "acquired_at": stale_time.to_rfc3339(),
            "pid": 99999,
        });
        std::fs::write(&lock_path, content.to_string()).unwrap();

        let result = compact_t(cache_dir, "agent-1", true).unwrap();
        assert!(result.is_some());

        assert!(!lock_path.exists());
    }

    #[test]
    fn test_file_lock_guard_acquire_release() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path();
        setup_cache(cache_dir);

        let lock_path = cache_dir.join("checkpoint").join(COMPACTION_LOCK_FILE);

        {
            let guard = CompactionLockGuard::try_acquire(cache_dir, "agent-1", false)
                .unwrap()
                .unwrap();
            assert!(lock_path.exists());

            let result = CompactionLockGuard::try_acquire(cache_dir, "agent-2", false).unwrap();
            assert!(result.is_none());

            drop(guard);
        }

        assert!(!lock_path.exists());

        let guard = CompactionLockGuard::try_acquire(cache_dir, "agent-2", false)
            .unwrap()
            .unwrap();
        assert!(lock_path.exists());
        drop(guard);
    }

    #[test]
    fn test_file_lock_same_agent_force() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path();
        setup_cache(cache_dir);

        let lock_path = cache_dir.join("checkpoint").join(COMPACTION_LOCK_FILE);
        let content = serde_json::json!({
            "agent_id": "agent-1",
            "acquired_at": Utc::now().to_rfc3339(),
            "pid": 99999,
        });
        std::fs::write(&lock_path, content.to_string()).unwrap();

        let guard = CompactionLockGuard::try_acquire(cache_dir, "agent-1", true)
            .unwrap()
            .unwrap();
        assert!(lock_path.exists());
        drop(guard);
    }

    #[test]
    fn test_stale_lock_auto_cleared() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path();
        setup_cache(cache_dir);

        let lock_path = cache_dir.join("checkpoint").join(COMPACTION_LOCK_FILE);
        let stale_time = Utc::now() - Duration::seconds(LEASE_DURATION_SECS * 3);
        let content = serde_json::json!({
            "agent_id": "agent-old",
            "acquired_at": stale_time.to_rfc3339(),
            "pid": 99999,
        });
        std::fs::write(&lock_path, content.to_string()).unwrap();

        let guard = CompactionLockGuard::try_acquire(cache_dir, "agent-new", false)
            .unwrap()
            .unwrap();
        assert!(lock_path.exists());
        drop(guard);
    }

    #[test]
    fn test_materialized_issue_file_format() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path();
        setup_cache(cache_dir);

        let uuid = Uuid::new_v4();
        let log = cache_dir.join("agents/agent-1/events.log");

        let env = make_envelope(
            "agent-1",
            1,
            Event::IssueCreated {
                uuid,
                title: "Materialized".to_string(),
                description: Some("desc".to_string()),
                priority: "critical".to_string(),
                labels: vec!["bug".to_string(), "urgent".to_string()],
                parent_uuid: None,
                created_by: "agent-1".to_string(),
                display_id: None,
                scheduled_at: None,
                due_at: None,
            },
        );
        append_event(&log, &env).unwrap();
        compact_t(cache_dir, "agent-1", true).unwrap();

        let path = cache_dir
            .join("issues")
            .join(uuid.to_string())
            .join("issue.json");
        let content = std::fs::read_to_string(&path).unwrap();
        let issue: IssueFile = serde_json::from_str(&content).unwrap();

        assert_eq!(issue.uuid, uuid);
        assert_eq!(issue.display_id, Some(1));
        assert_eq!(issue.title, "Materialized");
        assert_eq!(issue.description.as_deref(), Some("desc"));
        assert_eq!(issue.status, crate::models::IssueStatus::Open);
        assert_eq!(issue.priority, crate::models::Priority::Critical);
        assert!(issue.comments.is_empty());
        assert!(issue.time_entries.is_empty());
    }

    #[test]
    fn test_parent_changed() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path();
        setup_cache(cache_dir);

        let parent = Uuid::new_v4();
        let child = Uuid::new_v4();
        let log = cache_dir.join("agents/agent-1/events.log");

        let mut e1 = make_envelope(
            "agent-1",
            1,
            Event::IssueCreated {
                uuid: child,
                title: "Child".to_string(),
                description: None,
                priority: "medium".to_string(),
                labels: vec![],
                parent_uuid: None,
                created_by: "agent-1".to_string(),
                display_id: None,
                scheduled_at: None,
                due_at: None,
            },
        );
        e1.timestamp = Utc::now() - Duration::seconds(10);

        let e2 = make_envelope(
            "agent-1",
            2,
            Event::ParentChanged {
                issue_uuid: child,
                new_parent_uuid: Some(parent),
            },
        );

        append_event(&log, &e1).unwrap();
        append_event(&log, &e2).unwrap();

        compact_t(cache_dir, "agent-1", true).unwrap();

        let state = read_checkpoint(cache_dir).unwrap();
        assert_eq!(state.issues[&child].parent_uuid, Some(parent));
    }

    #[test]
    fn test_milestone_assigned() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path();
        setup_cache(cache_dir);

        let issue_uuid = Uuid::new_v4();
        let milestone_uuid = Uuid::new_v4();
        let log = cache_dir.join("agents/agent-1/events.log");

        let mut e1 = make_envelope(
            "agent-1",
            1,
            Event::IssueCreated {
                uuid: issue_uuid,
                title: "Milestone test".to_string(),
                description: None,
                priority: "medium".to_string(),
                labels: vec![],
                parent_uuid: None,
                created_by: "agent-1".to_string(),
                display_id: None,
                scheduled_at: None,
                due_at: None,
            },
        );
        e1.timestamp = Utc::now() - Duration::seconds(10);

        let e2 = make_envelope(
            "agent-1",
            2,
            Event::MilestoneAssigned {
                issue_uuid,
                milestone_uuid: Some(milestone_uuid),
            },
        );

        append_event(&log, &e1).unwrap();
        append_event(&log, &e2).unwrap();

        compact_t(cache_dir, "agent-1", true).unwrap();

        let state = read_checkpoint(cache_dir).unwrap();
        assert_eq!(
            state.issues[&issue_uuid].milestone_uuid,
            Some(milestone_uuid)
        );
    }

    #[test]
    fn test_lock_release_by_non_holder_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path();
        setup_cache(cache_dir);

        let log_a = cache_dir.join("agents/agent-a/events.log");
        let log_b = cache_dir.join("agents/agent-b/events.log");

        let mut e1 = make_envelope(
            "agent-a",
            1,
            Event::LockClaimed {
                issue_display_id: 1,
                branch: None,
            },
        );
        e1.timestamp = Utc::now() - Duration::seconds(3);

        let mut e2 = make_envelope(
            "agent-b",
            1,
            Event::LockReleased {
                issue_display_id: 1,
            },
        );
        e2.timestamp = Utc::now() - Duration::seconds(1);

        append_event(&log_a, &e1).unwrap();
        append_event(&log_b, &e2).unwrap();

        compact_t(cache_dir, "agent-a", true).unwrap();

        let state = read_checkpoint(cache_dir).unwrap();

        assert_eq!(state.locks[&1].agent_id, "agent-a");
        assert!(cache_dir.join("locks/1.json").exists());
    }

    #[test]
    fn test_lock_claim_release_reclaim_cycle() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path();
        setup_cache(cache_dir);

        let log = cache_dir.join("agents/agent-1/events.log");
        let now = Utc::now();

        let mut e1 = make_envelope(
            "agent-1",
            1,
            Event::LockClaimed {
                issue_display_id: 1,
                branch: Some("feature/first".to_string()),
            },
        );
        e1.timestamp = now - Duration::seconds(3);

        let mut e2 = make_envelope(
            "agent-1",
            2,
            Event::LockReleased {
                issue_display_id: 1,
            },
        );
        e2.timestamp = now - Duration::seconds(2);

        let mut e3 = make_envelope(
            "agent-1",
            3,
            Event::LockClaimed {
                issue_display_id: 1,
                branch: Some("feature/second".to_string()),
            },
        );
        e3.timestamp = now - Duration::seconds(1);

        append_event(&log, &e1).unwrap();
        append_event(&log, &e2).unwrap();
        append_event(&log, &e3).unwrap();

        compact_t(cache_dir, "agent-1", true).unwrap();

        let state = read_checkpoint(cache_dir).unwrap();
        let lock = state.locks.get(&1).unwrap();
        assert_eq!(lock.agent_id, "agent-1");
        assert_eq!(lock.branch, Some("feature/second".to_string()));
    }

    #[test]
    fn test_three_way_lock_contention() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path();
        setup_cache(cache_dir);

        let now = Utc::now();

        for (agent, seq_offset) in &[("agent-a", 3), ("agent-b", 2), ("agent-c", 1)] {
            let log = cache_dir.join(format!("agents/{agent}/events.log"));
            let mut e = make_envelope(
                agent,
                1,
                Event::LockClaimed {
                    issue_display_id: 1,
                    branch: None,
                },
            );
            e.timestamp = now - Duration::seconds(*seq_offset);
            append_event(&log, &e).unwrap();
        }

        compact_t(cache_dir, "agent-a", true).unwrap();

        let state = read_checkpoint(cache_dir).unwrap();

        assert_eq!(state.locks[&1].agent_id, "agent-a");
    }

    #[test]
    fn test_lock_contention_timestamp_tiebreaker_uses_agent_id() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path();
        setup_cache(cache_dir);

        let same_time = Utc::now() - Duration::seconds(5);

        let log_a = cache_dir.join("agents/agent-alpha/events.log");
        let log_b = cache_dir.join("agents/agent-beta/events.log");

        let mut e1 = make_envelope(
            "agent-alpha",
            1,
            Event::LockClaimed {
                issue_display_id: 1,
                branch: None,
            },
        );
        e1.timestamp = same_time;

        let mut e2 = make_envelope(
            "agent-beta",
            1,
            Event::LockClaimed {
                issue_display_id: 1,
                branch: None,
            },
        );
        e2.timestamp = same_time;

        append_event(&log_a, &e1).unwrap();
        append_event(&log_b, &e2).unwrap();

        compact_t(cache_dir, "agent-alpha", true).unwrap();

        let state = read_checkpoint(cache_dir).unwrap();

        assert_eq!(state.locks[&1].agent_id, "agent-alpha");
    }

    #[test]
    fn test_concurrent_claims_on_different_issues() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path();
        setup_cache(cache_dir);

        let log_a = cache_dir.join("agents/agent-a/events.log");
        let log_b = cache_dir.join("agents/agent-b/events.log");

        let mut e1 = make_envelope(
            "agent-a",
            1,
            Event::LockClaimed {
                issue_display_id: 1,
                branch: Some("feature/a".to_string()),
            },
        );
        e1.timestamp = Utc::now() - Duration::seconds(2);

        let mut e2 = make_envelope(
            "agent-b",
            1,
            Event::LockClaimed {
                issue_display_id: 2,
                branch: Some("feature/b".to_string()),
            },
        );
        e2.timestamp = Utc::now() - Duration::seconds(1);

        append_event(&log_a, &e1).unwrap();
        append_event(&log_b, &e2).unwrap();

        let result = compact_t(cache_dir, "agent-a", true).unwrap().unwrap();
        assert_eq!(result.locks_materialized, 2);

        let state = read_checkpoint(cache_dir).unwrap();
        assert_eq!(state.locks.len(), 2);
        assert_eq!(state.locks[&1].agent_id, "agent-a");
        assert_eq!(state.locks[&2].agent_id, "agent-b");

        assert!(cache_dir.join("locks/1.json").exists());
        assert!(cache_dir.join("locks/2.json").exists());
    }

    #[test]
    fn test_lock_branch_metadata_preserved_through_contention() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path();
        setup_cache(cache_dir);

        let log_a = cache_dir.join("agents/agent-a/events.log");
        let log_b = cache_dir.join("agents/agent-b/events.log");

        let mut e1 = make_envelope(
            "agent-a",
            1,
            Event::LockClaimed {
                issue_display_id: 1,
                branch: Some("feature/winner-branch".to_string()),
            },
        );
        e1.timestamp = Utc::now() - Duration::seconds(2);

        let mut e2 = make_envelope(
            "agent-b",
            1,
            Event::LockClaimed {
                issue_display_id: 1,
                branch: Some("feature/loser-branch".to_string()),
            },
        );
        e2.timestamp = Utc::now() - Duration::seconds(1);

        append_event(&log_a, &e1).unwrap();
        append_event(&log_b, &e2).unwrap();

        compact_t(cache_dir, "agent-a", true).unwrap();

        let state = read_checkpoint(cache_dir).unwrap();
        let lock = state.locks.get(&1).unwrap();
        assert_eq!(lock.agent_id, "agent-a");
        assert_eq!(lock.branch, Some("feature/winner-branch".to_string()));

        let lock_content = std::fs::read_to_string(cache_dir.join("locks/1.json")).unwrap();
        let lock_file: LockFileV2 = serde_json::from_str(&lock_content).unwrap();
        assert_eq!(lock_file.branch, Some("feature/winner-branch".to_string()));
    }

    #[test]
    fn test_lock_release_removes_materialized_file() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path();
        setup_cache(cache_dir);

        let log = cache_dir.join("agents/agent-1/events.log");

        let mut e1 = make_envelope(
            "agent-1",
            1,
            Event::LockClaimed {
                issue_display_id: 5,
                branch: None,
            },
        );
        e1.timestamp = Utc::now() - Duration::seconds(2);

        let e2 = make_envelope(
            "agent-1",
            2,
            Event::LockReleased {
                issue_display_id: 5,
            },
        );

        append_event(&log, &e1).unwrap();
        append_event(&log, &e2).unwrap();

        compact_t(cache_dir, "agent-1", true).unwrap();

        let state = read_checkpoint(cache_dir).unwrap();
        assert!(state.locks.is_empty());
        assert!(!cache_dir.join("locks/5.json").exists());
    }

    #[test]
    fn test_lock_release_of_nonexistent_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path();
        setup_cache(cache_dir);

        let log = cache_dir.join("agents/agent-1/events.log");

        let e = make_envelope(
            "agent-1",
            1,
            Event::LockReleased {
                issue_display_id: 99,
            },
        );
        append_event(&log, &e).unwrap();

        let result = compact_t(cache_dir, "agent-1", true).unwrap().unwrap();
        assert_eq!(result.events_processed, 1);
        assert_eq!(result.locks_materialized, 0);

        let state = read_checkpoint(cache_dir).unwrap();
        assert!(state.locks.is_empty());
    }

    #[test]
    fn test_incremental_compaction_with_lock_changes() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path();
        setup_cache(cache_dir);

        let log = cache_dir.join("agents/agent-1/events.log");
        let now = Utc::now();

        let mut e1 = make_envelope(
            "agent-1",
            1,
            Event::LockClaimed {
                issue_display_id: 1,
                branch: Some("feature/x".to_string()),
            },
        );
        e1.timestamp = now - Duration::seconds(3);
        append_event(&log, &e1).unwrap();

        compact_t(cache_dir, "agent-1", true).unwrap();

        let state = read_checkpoint(cache_dir).unwrap();
        assert_eq!(state.locks[&1].agent_id, "agent-1");
        assert!(cache_dir.join("locks/1.json").exists());

        let mut e2 = make_envelope(
            "agent-1",
            2,
            Event::LockReleased {
                issue_display_id: 1,
            },
        );
        e2.timestamp = now - Duration::seconds(1);
        append_event(&log, &e2).unwrap();

        compact_t(cache_dir, "agent-1", true).unwrap();

        let state = read_checkpoint(cache_dir).unwrap();
        assert!(state.locks.is_empty());
        assert!(!cache_dir.join("locks/1.json").exists());
    }

    #[test]
    fn test_contention_loser_then_winner_releases() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path();
        setup_cache(cache_dir);

        let log_a = cache_dir.join("agents/agent-a/events.log");
        let log_b = cache_dir.join("agents/agent-b/events.log");
        let now = Utc::now();

        let mut e1 = make_envelope(
            "agent-a",
            1,
            Event::LockClaimed {
                issue_display_id: 1,
                branch: None,
            },
        );
        e1.timestamp = now - Duration::seconds(4);

        let mut e2 = make_envelope(
            "agent-b",
            1,
            Event::LockClaimed {
                issue_display_id: 1,
                branch: None,
            },
        );
        e2.timestamp = now - Duration::seconds(3);

        let mut e3 = make_envelope(
            "agent-a",
            2,
            Event::LockReleased {
                issue_display_id: 1,
            },
        );
        e3.timestamp = now - Duration::seconds(1);

        append_event(&log_a, &e1).unwrap();
        append_event(&log_b, &e2).unwrap();
        append_event(&log_a, &e3).unwrap();

        compact_t(cache_dir, "agent-a", true).unwrap();

        let state = read_checkpoint(cache_dir).unwrap();

        assert!(state.locks.is_empty());
        assert!(!cache_dir.join("locks/1.json").exists());
    }

    #[test]
    fn test_same_agent_reclaim_after_contention_loss() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path();
        setup_cache(cache_dir);

        let log_a = cache_dir.join("agents/agent-a/events.log");
        let log_b = cache_dir.join("agents/agent-b/events.log");
        let now = Utc::now();

        let mut e1 = make_envelope(
            "agent-a",
            1,
            Event::LockClaimed {
                issue_display_id: 1,
                branch: None,
            },
        );
        e1.timestamp = now - Duration::seconds(4);

        let mut e2 = make_envelope(
            "agent-b",
            1,
            Event::LockClaimed {
                issue_display_id: 1,
                branch: None,
            },
        );
        e2.timestamp = now - Duration::seconds(3);

        let mut e3 = make_envelope(
            "agent-a",
            2,
            Event::LockReleased {
                issue_display_id: 1,
            },
        );
        e3.timestamp = now - Duration::seconds(2);

        let mut e4 = make_envelope(
            "agent-b",
            2,
            Event::LockClaimed {
                issue_display_id: 1,
                branch: Some("feature/retry".to_string()),
            },
        );
        e4.timestamp = now - Duration::seconds(1);

        append_event(&log_a, &e1).unwrap();
        append_event(&log_b, &e2).unwrap();
        append_event(&log_a, &e3).unwrap();
        append_event(&log_b, &e4).unwrap();

        compact_t(cache_dir, "agent-a", true).unwrap();

        let state = read_checkpoint(cache_dir).unwrap();
        let lock = state.locks.get(&1).unwrap();
        assert_eq!(lock.agent_id, "agent-b");
        assert_eq!(lock.branch, Some("feature/retry".to_string()));
    }

    #[test]
    fn test_multiple_issues_independent_contention() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path();
        setup_cache(cache_dir);

        let log_a = cache_dir.join("agents/agent-a/events.log");
        let log_b = cache_dir.join("agents/agent-b/events.log");
        let now = Utc::now();

        let mut e1 = make_envelope(
            "agent-a",
            1,
            Event::LockClaimed {
                issue_display_id: 1,
                branch: None,
            },
        );
        e1.timestamp = now - Duration::seconds(4);

        let mut e2 = make_envelope(
            "agent-b",
            1,
            Event::LockClaimed {
                issue_display_id: 2,
                branch: None,
            },
        );
        e2.timestamp = now - Duration::seconds(3);

        let mut e3 = make_envelope(
            "agent-b",
            2,
            Event::LockClaimed {
                issue_display_id: 1,
                branch: None,
            },
        );
        e3.timestamp = now - Duration::seconds(2);

        let mut e4 = make_envelope(
            "agent-a",
            2,
            Event::LockClaimed {
                issue_display_id: 2,
                branch: None,
            },
        );
        e4.timestamp = now - Duration::seconds(1);

        append_event(&log_a, &e1).unwrap();
        append_event(&log_b, &e2).unwrap();
        append_event(&log_b, &e3).unwrap();
        append_event(&log_a, &e4).unwrap();

        compact_t(cache_dir, "agent-a", true).unwrap();

        let state = read_checkpoint(cache_dir).unwrap();
        assert_eq!(state.locks.len(), 2);
        assert_eq!(state.locks[&1].agent_id, "agent-a");
        assert_eq!(state.locks[&2].agent_id, "agent-b");
    }

    #[test]
    fn test_prune_preserves_unpruned_lock_events() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path();
        setup_cache(cache_dir);

        let log = cache_dir.join("agents/agent-1/events.log");
        let now = Utc::now();

        let mut e1 = make_envelope(
            "agent-1",
            1,
            Event::LockClaimed {
                issue_display_id: 1,
                branch: None,
            },
        );
        e1.timestamp = now - Duration::seconds(3);

        let mut e2 = make_envelope(
            "agent-1",
            2,
            Event::LockReleased {
                issue_display_id: 1,
            },
        );
        e2.timestamp = now - Duration::seconds(1);

        append_event(&log, &e1).unwrap();
        append_event(&log, &e2).unwrap();

        let watermark = OrderingKey {
            timestamp: now - Duration::seconds(2),
            agent_id: "agent-1".to_string(),
            agent_seq: 1,
        };
        crate::checkpoint::write_watermark(cache_dir, &watermark).unwrap();

        let pruned = prune_events(cache_dir, "agent-1").unwrap();
        assert_eq!(pruned, 1);

        let remaining = crate::events::read_events(&log).unwrap();
        assert_eq!(remaining.len(), 1);
        assert!(matches!(
            remaining[0].event,
            Event::LockReleased {
                issue_display_id: 1
            }
        ));
    }

    #[test]
    fn test_lock_claimed_at_timestamp_matches_event() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path();
        setup_cache(cache_dir);

        let log = cache_dir.join("agents/agent-1/events.log");
        let claim_time = Utc::now() - Duration::seconds(10);

        let mut e = make_envelope(
            "agent-1",
            1,
            Event::LockClaimed {
                issue_display_id: 1,
                branch: None,
            },
        );
        e.timestamp = claim_time;
        append_event(&log, &e).unwrap();

        compact_t(cache_dir, "agent-1", true).unwrap();

        let state = read_checkpoint(cache_dir).unwrap();
        let lock = state.locks.get(&1).unwrap();
        assert_eq!(lock.claimed_at, claim_time);

        let lock_content = std::fs::read_to_string(cache_dir.join("locks/1.json")).unwrap();
        let lock_file: LockFileV2 = serde_json::from_str(&lock_content).unwrap();
        assert_eq!(lock_file.claimed_at, claim_time);
    }

    #[test]
    fn test_no_clock_skew_within_threshold() {
        let mut state = CheckpointState::default();
        let env = make_envelope(
            "agent-1",
            1,
            Event::IssueCreated {
                uuid: Uuid::new_v4(),
                title: "Recent".to_string(),
                description: None,
                priority: "medium".to_string(),
                labels: vec![],
                parent_uuid: None,
                created_by: "agent-1".to_string(),
                display_id: None,
                scheduled_at: None,
                due_at: None,
            },
        );

        detect_clock_skew(&mut state, &env);
        assert!(state.skew_warnings.is_empty());
    }

    #[test]
    fn test_check_unsigned_with_signed_event_no_trust_file() {
        let mut state = CheckpointState::default();
        let mut env = make_envelope(
            "agent-1",
            1,
            Event::IssueCreated {
                uuid: Uuid::new_v4(),
                title: "Signed".to_string(),
                description: None,
                priority: "medium".to_string(),
                labels: vec![],
                parent_uuid: None,
                created_by: "agent-1".to_string(),
                display_id: None,
                scheduled_at: None,
                due_at: None,
            },
        );
        env.signed_by = Some("agent-1".to_string());
        env.signature = Some("fake-signature".to_string());

        let nonexistent = PathBuf::from("/tmp/nonexistent_trust_dir/allowed_signers");
        check_unsigned(&mut state, &env, &nonexistent);
        assert!(
            state.unsigned_event_warnings.is_empty(),
            "Signed event without trust file should not warn"
        );
    }

    #[test]
    fn test_apply_events_on_nonexistent_issue_are_noop() {
        let mut state = CheckpointState::default();
        let unknown = Uuid::new_v4();
        let mut changed_issues = HashSet::new();
        let mut changed_locks = HashSet::new();

        let env = make_envelope(
            "agent-1",
            1,
            Event::IssueUpdated {
                uuid: unknown,
                title: Some("Ghost".to_string()),
                description: None,
                priority: None,
            },
        );
        apply(&mut state, &env, &mut changed_issues, &mut changed_locks);
        assert!(state.issues.is_empty());
        assert!(changed_issues.is_empty());

        let env = make_envelope(
            "agent-1",
            2,
            Event::StatusChanged {
                uuid: unknown,
                new_status: "closed".to_string(),
                closed_at: Some(Utc::now()),
            },
        );
        apply(&mut state, &env, &mut changed_issues, &mut changed_locks);
        assert!(state.issues.is_empty());

        let env = make_envelope(
            "agent-1",
            3,
            Event::DependencyAdded {
                blocked_uuid: unknown,
                blocker_uuid: Uuid::new_v4(),
            },
        );
        apply(&mut state, &env, &mut changed_issues, &mut changed_locks);
        assert!(state.issues.is_empty());

        let env = make_envelope(
            "agent-1",
            4,
            Event::DependencyRemoved {
                blocked_uuid: unknown,
                blocker_uuid: Uuid::new_v4(),
            },
        );
        apply(&mut state, &env, &mut changed_issues, &mut changed_locks);
        assert!(state.issues.is_empty());

        let env = make_envelope(
            "agent-1",
            5,
            Event::RelationAdded {
                uuid_a: unknown,
                uuid_b: Uuid::new_v4(),
            },
        );
        apply(&mut state, &env, &mut changed_issues, &mut changed_locks);
        assert!(state.issues.is_empty());

        let env = make_envelope(
            "agent-1",
            6,
            Event::RelationRemoved {
                uuid_a: unknown,
                uuid_b: Uuid::new_v4(),
            },
        );
        apply(&mut state, &env, &mut changed_issues, &mut changed_locks);
        assert!(state.issues.is_empty());

        let env = make_envelope(
            "agent-1",
            7,
            Event::MilestoneAssigned {
                issue_uuid: unknown,
                milestone_uuid: Some(Uuid::new_v4()),
            },
        );
        apply(&mut state, &env, &mut changed_issues, &mut changed_locks);
        assert!(state.issues.is_empty());

        let env = make_envelope(
            "agent-1",
            8,
            Event::LabelAdded {
                issue_uuid: unknown,
                label: "ghost-label".to_string(),
            },
        );
        apply(&mut state, &env, &mut changed_issues, &mut changed_locks);
        assert!(state.issues.is_empty());

        let env = make_envelope(
            "agent-1",
            9,
            Event::LabelRemoved {
                issue_uuid: unknown,
                label: "ghost-label".to_string(),
            },
        );
        apply(&mut state, &env, &mut changed_issues, &mut changed_locks);
        assert!(state.issues.is_empty());

        let env = make_envelope(
            "agent-1",
            10,
            Event::ParentChanged {
                issue_uuid: unknown,
                new_parent_uuid: Some(Uuid::new_v4()),
            },
        );
        apply(&mut state, &env, &mut changed_issues, &mut changed_locks);
        assert!(state.issues.is_empty());

        assert!(changed_issues.is_empty());
        assert!(changed_locks.is_empty());
    }

    #[test]
    fn test_dependency_removed() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path();
        setup_cache(cache_dir);

        let blocked = Uuid::new_v4();
        let blocker = Uuid::new_v4();
        let log = cache_dir.join("agents/agent-1/events.log");
        let now = Utc::now();

        let mut e1 = make_envelope(
            "agent-1",
            1,
            Event::IssueCreated {
                uuid: blocked,
                title: "Blocked".to_string(),
                description: None,
                priority: "medium".to_string(),
                labels: vec![],
                parent_uuid: None,
                created_by: "agent-1".to_string(),
                display_id: None,
                scheduled_at: None,
                due_at: None,
            },
        );
        e1.timestamp = now - Duration::seconds(10);

        let mut e2 = make_envelope(
            "agent-1",
            2,
            Event::DependencyAdded {
                blocked_uuid: blocked,
                blocker_uuid: blocker,
            },
        );
        e2.timestamp = now - Duration::seconds(5);

        let e3 = make_envelope(
            "agent-1",
            3,
            Event::DependencyRemoved {
                blocked_uuid: blocked,
                blocker_uuid: blocker,
            },
        );

        append_event(&log, &e1).unwrap();
        append_event(&log, &e2).unwrap();
        append_event(&log, &e3).unwrap();

        compact_t(cache_dir, "agent-1", true).unwrap();

        let state = read_checkpoint(cache_dir).unwrap();
        assert!(
            state.issues[&blocked].blockers.is_empty(),
            "Dependency should be removed after DependencyRemoved event"
        );
    }

    #[test]
    fn test_relation_removed() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path();
        setup_cache(cache_dir);

        let uuid_a = Uuid::new_v4();
        let uuid_b = Uuid::new_v4();
        let log = cache_dir.join("agents/agent-1/events.log");
        let now = Utc::now();

        let mut e1 = make_envelope(
            "agent-1",
            1,
            Event::IssueCreated {
                uuid: uuid_a,
                title: "A".to_string(),
                description: None,
                priority: "medium".to_string(),
                labels: vec![],
                parent_uuid: None,
                created_by: "agent-1".to_string(),
                display_id: None,
                scheduled_at: None,
                due_at: None,
            },
        );
        e1.timestamp = now - Duration::seconds(10);

        let mut e2 = make_envelope(
            "agent-1",
            2,
            Event::IssueCreated {
                uuid: uuid_b,
                title: "B".to_string(),
                description: None,
                priority: "medium".to_string(),
                labels: vec![],
                parent_uuid: None,
                created_by: "agent-1".to_string(),
                display_id: None,
                scheduled_at: None,
                due_at: None,
            },
        );
        e2.timestamp = now - Duration::seconds(9);

        let mut e3 = make_envelope("agent-1", 3, Event::RelationAdded { uuid_a, uuid_b });
        e3.timestamp = now - Duration::seconds(5);

        let e4 = make_envelope("agent-1", 4, Event::RelationRemoved { uuid_a, uuid_b });

        append_event(&log, &e1).unwrap();
        append_event(&log, &e2).unwrap();
        append_event(&log, &e3).unwrap();
        append_event(&log, &e4).unwrap();

        compact_t(cache_dir, "agent-1", true).unwrap();

        let state = read_checkpoint(cache_dir).unwrap();
        assert!(
            state.issues[&uuid_a].related.is_empty(),
            "Relation should be removed from A"
        );
        assert!(
            state.issues[&uuid_b].related.is_empty(),
            "Relation should be removed from B"
        );
    }

    #[test]
    fn test_issue_update_description_and_priority() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path();
        setup_cache(cache_dir);

        let uuid = Uuid::new_v4();
        let log = cache_dir.join("agents/agent-1/events.log");

        let mut e_create = make_envelope(
            "agent-1",
            1,
            Event::IssueCreated {
                uuid,
                title: "Original".to_string(),
                description: None,
                priority: "low".to_string(),
                labels: vec![],
                parent_uuid: None,
                created_by: "agent-1".to_string(),
                display_id: None,
                scheduled_at: None,
                due_at: None,
            },
        );
        e_create.timestamp = Utc::now() - Duration::seconds(10);

        let e_update = make_envelope(
            "agent-1",
            2,
            Event::IssueUpdated {
                uuid,
                title: None,
                description: Some("New description".to_string()),
                priority: Some("critical".to_string()),
            },
        );

        append_event(&log, &e_create).unwrap();
        append_event(&log, &e_update).unwrap();

        compact_t(cache_dir, "agent-1", true).unwrap();

        let state = read_checkpoint(cache_dir).unwrap();
        let issue = &state.issues[&uuid];
        assert_eq!(issue.title, "Original", "Title should be unchanged");
        assert_eq!(
            issue.description.as_deref(),
            Some("New description"),
            "Description should be updated"
        );
        assert_eq!(
            issue.priority,
            crate::models::Priority::Critical,
            "Priority should be updated"
        );
    }

    #[test]
    fn test_prune_events_no_watermark() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path();
        setup_cache(cache_dir);

        let log = cache_dir.join("agents/agent-1/events.log");
        let env = make_envelope(
            "agent-1",
            1,
            Event::IssueCreated {
                uuid: Uuid::new_v4(),
                title: "Unprunable".to_string(),
                description: None,
                priority: "medium".to_string(),
                labels: vec![],
                parent_uuid: None,
                created_by: "agent-1".to_string(),
                display_id: None,
                scheduled_at: None,
                due_at: None,
            },
        );
        append_event(&log, &env).unwrap();

        let pruned = prune_events(cache_dir, "agent-1").unwrap();
        assert_eq!(pruned, 0);

        let remaining = crate::events::read_events(&log).unwrap();
        assert_eq!(remaining.len(), 1);
    }

    #[test]
    fn test_prune_events_no_log_file() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path();
        setup_cache(cache_dir);

        let watermark = OrderingKey {
            timestamp: Utc::now(),
            agent_id: "agent-1".to_string(),
            agent_seq: 1,
        };
        crate::checkpoint::write_watermark(cache_dir, &watermark).unwrap();

        std::fs::create_dir_all(cache_dir.join("agents/agent-1")).unwrap();

        let pruned = prune_events(cache_dir, "agent-1").unwrap();
        assert_eq!(pruned, 0);
    }

    #[test]
    fn test_prune_events_nothing_to_prune() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path();
        setup_cache(cache_dir);

        let log = cache_dir.join("agents/agent-1/events.log");
        let now = Utc::now();

        let env = make_envelope(
            "agent-1",
            5,
            Event::LabelAdded {
                issue_uuid: Uuid::new_v4(),
                label: "test".to_string(),
            },
        );
        append_event(&log, &env).unwrap();

        let watermark = OrderingKey {
            timestamp: now - Duration::seconds(100),
            agent_id: "agent-1".to_string(),
            agent_seq: 1,
        };
        crate::checkpoint::write_watermark(cache_dir, &watermark).unwrap();

        let pruned = prune_events(cache_dir, "agent-1").unwrap();
        assert_eq!(pruned, 0);

        let remaining = crate::events::read_events(&log).unwrap();
        assert_eq!(remaining.len(), 1);
    }

    #[test]
    fn test_compact_no_agents_dir() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path();

        std::fs::create_dir_all(cache_dir.join("checkpoint")).unwrap();
        std::fs::create_dir_all(cache_dir.join("issues")).unwrap();
        std::fs::create_dir_all(cache_dir.join("locks")).unwrap();

        let result = compact_t(cache_dir, "agent-1", false).unwrap().unwrap();
        assert_eq!(result.events_processed, 0);
        assert_eq!(result.issues_materialized, 0);
    }

    #[test]
    fn test_read_lock_info_malformed_json() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("compaction.lock");

        std::fs::write(&lock_path, "").unwrap();
        assert!(CompactionLockGuard::read_lock_info(&lock_path).is_none());

        std::fs::write(&lock_path, "not json at all").unwrap();
        assert!(CompactionLockGuard::read_lock_info(&lock_path).is_none());

        std::fs::write(&lock_path, r#"{"acquired_at": "2025-01-01T00:00:00Z"}"#).unwrap();
        assert!(CompactionLockGuard::read_lock_info(&lock_path).is_none());

        std::fs::write(&lock_path, r#"{"agent_id": "test"}"#).unwrap();
        assert!(CompactionLockGuard::read_lock_info(&lock_path).is_none());

        std::fs::write(
            &lock_path,
            r#"{"agent_id": "test", "acquired_at": "not-a-date"}"#,
        )
        .unwrap();
        assert!(CompactionLockGuard::read_lock_info(&lock_path).is_none());
    }

    #[test]
    fn test_read_lock_info_nonexistent_file() {
        let nonexistent = PathBuf::from("/tmp/does_not_exist_lock_file");
        assert!(CompactionLockGuard::read_lock_info(&nonexistent).is_none());
    }

    #[test]
    fn test_force_acquire_with_malformed_lock_file() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path();
        setup_cache(cache_dir);

        let lock_path = cache_dir.join("checkpoint").join(COMPACTION_LOCK_FILE);
        std::fs::write(&lock_path, "totally broken json").unwrap();

        let result = CompactionLockGuard::try_acquire(cache_dir, "agent-1", false).unwrap();
        assert!(
            result.is_none(),
            "Should not acquire when lock has unreadable info and force=false"
        );

        let guard = CompactionLockGuard::try_acquire(cache_dir, "agent-1", true)
            .unwrap()
            .unwrap();
        assert!(lock_path.exists());
        drop(guard);
    }

    #[test]
    fn test_compact_to_issue_file_with_blockers_and_related() {
        let uuid = Uuid::new_v4();
        let blocker = Uuid::new_v4();
        let related = Uuid::new_v4();

        let compact = CompactIssue {
            uuid,
            display_id: Some(42),
            title: "Full issue".to_string(),
            description: Some("With all fields".to_string()),
            status: crate::models::IssueStatus::Open,
            priority: crate::models::Priority::High,
            parent_uuid: Some(Uuid::new_v4()),
            created_by: "agent-1".to_string(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            closed_at: Some(Utc::now()),
            scheduled_at: None,
            due_at: None,
            labels: {
                let mut s = BTreeSet::new();
                s.insert("bug".to_string());
                s.insert("urgent".to_string());
                s
            },
            blockers: {
                let mut s = BTreeSet::new();
                s.insert(blocker);
                s
            },
            related: {
                let mut s = BTreeSet::new();
                s.insert(related);
                s
            },
            milestone_uuid: Some(Uuid::new_v4()),
            comments: std::collections::BTreeMap::new(),
            time_entries: std::collections::BTreeMap::new(),
        };

        let issue_file = compact_to_issue_file(&compact);
        assert_eq!(issue_file.uuid, uuid);
        assert_eq!(issue_file.display_id, Some(42));
        assert_eq!(issue_file.title, "Full issue");
        assert_eq!(issue_file.description.as_deref(), Some("With all fields"));
        assert_eq!(issue_file.priority, crate::models::Priority::High);
        assert!(issue_file.closed_at.is_some());
        assert_eq!(issue_file.blockers, vec![blocker]);
        assert_eq!(issue_file.related, vec![related]);
        assert_eq!(
            issue_file.labels,
            vec!["bug".to_string(), "urgent".to_string()]
        );
        assert!(issue_file.comments.is_empty());
        assert!(issue_file.time_entries.is_empty());
        assert_eq!(issue_file.milestone_uuid, compact.milestone_uuid);
        assert_eq!(issue_file.parent_uuid, compact.parent_uuid);
        assert_eq!(issue_file.created_by, "agent-1");
    }

    #[test]
    fn test_incremental_compaction_no_new_events() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path();
        setup_cache(cache_dir);

        let uuid = Uuid::new_v4();
        let log = cache_dir.join("agents/agent-1/events.log");

        let env = make_envelope(
            "agent-1",
            1,
            Event::IssueCreated {
                uuid,
                title: "Once".to_string(),
                description: None,
                priority: "medium".to_string(),
                labels: vec![],
                parent_uuid: None,
                created_by: "agent-1".to_string(),
                display_id: None,
                scheduled_at: None,
                due_at: None,
            },
        );
        append_event(&log, &env).unwrap();

        let r1 = compact_t(cache_dir, "agent-1", true).unwrap().unwrap();
        assert_eq!(r1.events_processed, 1);

        let r2 = compact_t(cache_dir, "agent-1", true).unwrap().unwrap();
        assert_eq!(r2.events_processed, 0);
        assert_eq!(r2.issues_materialized, 0);
    }

    #[test]
    fn test_lock_release_on_nonexistent_lock_entry() {
        let mut state = CheckpointState::default();
        let mut changed_issues = HashSet::new();
        let mut changed_locks: HashSet<i64> = HashSet::new();

        let env = make_envelope(
            "agent-1",
            1,
            Event::LockReleased {
                issue_display_id: 999,
            },
        );
        apply(&mut state, &env, &mut changed_issues, &mut changed_locks);
        assert!(state.locks.is_empty());
        assert!(changed_locks.is_empty());
    }

    #[test]
    fn test_compact_skips_non_directory_entries_in_agents() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path();
        setup_cache(cache_dir);

        std::fs::write(cache_dir.join("agents/stray-file.txt"), "junk").unwrap();

        let uuid = Uuid::new_v4();
        let log = cache_dir.join("agents/agent-1/events.log");
        let env = make_envelope(
            "agent-1",
            1,
            Event::IssueCreated {
                uuid,
                title: "Valid".to_string(),
                description: None,
                priority: "medium".to_string(),
                labels: vec![],
                parent_uuid: None,
                created_by: "agent-1".to_string(),
                display_id: None,
                scheduled_at: None,
                due_at: None,
            },
        );
        append_event(&log, &env).unwrap();

        let result = compact_t(cache_dir, "agent-1", true).unwrap().unwrap();
        assert_eq!(result.events_processed, 1);
        assert_eq!(result.issues_materialized, 1);
    }

    #[test]
    fn test_milestone_unassigned() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path();
        setup_cache(cache_dir);

        let issue_uuid = Uuid::new_v4();
        let milestone_uuid = Uuid::new_v4();
        let log = cache_dir.join("agents/agent-1/events.log");
        let now = Utc::now();

        let mut e1 = make_envelope(
            "agent-1",
            1,
            Event::IssueCreated {
                uuid: issue_uuid,
                title: "Ms test".to_string(),
                description: None,
                priority: "medium".to_string(),
                labels: vec![],
                parent_uuid: None,
                created_by: "agent-1".to_string(),
                display_id: None,
                scheduled_at: None,
                due_at: None,
            },
        );
        e1.timestamp = now - Duration::seconds(10);

        let mut e2 = make_envelope(
            "agent-1",
            2,
            Event::MilestoneAssigned {
                issue_uuid,
                milestone_uuid: Some(milestone_uuid),
            },
        );
        e2.timestamp = now - Duration::seconds(5);

        let e3 = make_envelope(
            "agent-1",
            3,
            Event::MilestoneAssigned {
                issue_uuid,
                milestone_uuid: None,
            },
        );

        append_event(&log, &e1).unwrap();
        append_event(&log, &e2).unwrap();
        append_event(&log, &e3).unwrap();

        compact_t(cache_dir, "agent-1", true).unwrap();

        let state = read_checkpoint(cache_dir).unwrap();
        assert_eq!(
            state.issues[&issue_uuid].milestone_uuid, None,
            "Milestone should be cleared"
        );
    }

    #[test]
    fn test_parent_changed_to_none() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path();
        setup_cache(cache_dir);

        let parent = Uuid::new_v4();
        let child = Uuid::new_v4();
        let log = cache_dir.join("agents/agent-1/events.log");
        let now = Utc::now();

        let mut e1 = make_envelope(
            "agent-1",
            1,
            Event::IssueCreated {
                uuid: child,
                title: "Child".to_string(),
                description: None,
                priority: "medium".to_string(),
                labels: vec![],
                parent_uuid: Some(parent),
                created_by: "agent-1".to_string(),
                display_id: None,
                scheduled_at: None,
                due_at: None,
            },
        );
        e1.timestamp = now - Duration::seconds(10);

        let e2 = make_envelope(
            "agent-1",
            2,
            Event::ParentChanged {
                issue_uuid: child,
                new_parent_uuid: None,
            },
        );

        append_event(&log, &e1).unwrap();
        append_event(&log, &e2).unwrap();

        compact_t(cache_dir, "agent-1", true).unwrap();

        let state = read_checkpoint(cache_dir).unwrap();
        assert_eq!(
            state.issues[&child].parent_uuid, None,
            "Parent should be cleared"
        );
    }

    #[test]
    fn test_clock_skew_past_timestamp_no_warning() {
        let mut state = CheckpointState::default();
        let mut env = make_envelope(
            "agent-1",
            1,
            Event::IssueCreated {
                uuid: Uuid::new_v4(),
                title: "Old".to_string(),
                description: None,
                priority: "medium".to_string(),
                labels: vec![],
                parent_uuid: None,
                created_by: "agent-1".to_string(),
                display_id: None,
                scheduled_at: None,
                due_at: None,
            },
        );

        env.timestamp = Utc::now() - Duration::seconds(300);

        detect_clock_skew(&mut state, &env);
        assert_eq!(state.skew_warnings.len(), 0);
    }

    #[test]
    fn test_clock_skew_future_timestamp() {
        let mut state = CheckpointState::default();
        let mut env = make_envelope(
            "agent-1",
            1,
            Event::IssueCreated {
                uuid: Uuid::new_v4(),
                title: "Future".to_string(),
                description: None,
                priority: "medium".to_string(),
                labels: vec![],
                parent_uuid: None,
                created_by: "agent-1".to_string(),
                display_id: None,
                scheduled_at: None,
                due_at: None,
            },
        );

        env.timestamp = Utc::now() + Duration::seconds(300);

        detect_clock_skew(&mut state, &env);
        assert_eq!(state.skew_warnings.len(), 1);
        assert_eq!(state.skew_warnings[0].agent_id, "agent-1");
    }

    #[test]
    fn test_check_unsigned_missing_signature_only() {
        let mut state = CheckpointState::default();
        let mut env = make_envelope(
            "agent-1",
            1,
            Event::IssueCreated {
                uuid: Uuid::new_v4(),
                title: "Half-signed".to_string(),
                description: None,
                priority: "medium".to_string(),
                labels: vec![],
                parent_uuid: None,
                created_by: "agent-1".to_string(),
                display_id: None,
                scheduled_at: None,
                due_at: None,
            },
        );
        env.signed_by = Some("agent-1".to_string());
        env.signature = None;

        let nonexistent = PathBuf::from("/tmp/nonexistent_trust");
        check_unsigned(&mut state, &env, &nonexistent);
        assert_eq!(
            state.unsigned_event_warnings.len(),
            1,
            "Should warn when signature is None"
        );
    }

    #[test]
    fn test_check_unsigned_missing_signed_by_only() {
        let mut state = CheckpointState::default();
        let mut env = make_envelope(
            "agent-1",
            1,
            Event::IssueCreated {
                uuid: Uuid::new_v4(),
                title: "Half-signed".to_string(),
                description: None,
                priority: "medium".to_string(),
                labels: vec![],
                parent_uuid: None,
                created_by: "agent-1".to_string(),
                display_id: None,
                scheduled_at: None,
                due_at: None,
            },
        );
        env.signed_by = None;
        env.signature = Some("fake-sig".to_string());

        let nonexistent = PathBuf::from("/tmp/nonexistent_trust");
        check_unsigned(&mut state, &env, &nonexistent);
        assert_eq!(
            state.unsigned_event_warnings.len(),
            1,
            "Should warn when signed_by is None"
        );
    }

    #[test]
    fn test_compact_migrates_legacy_watermark() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path();
        setup_cache(cache_dir);

        let uuid = Uuid::new_v4();
        let log = cache_dir.join("agents/agent-1/events.log");

        let mut e1 = make_envelope(
            "agent-1",
            1,
            Event::IssueCreated {
                uuid,
                title: "First".to_string(),
                description: None,
                priority: "medium".to_string(),
                labels: vec![],
                parent_uuid: None,
                created_by: "agent-1".to_string(),
                display_id: None,
                scheduled_at: None,
                due_at: None,
            },
        );
        e1.timestamp = Utc::now() - Duration::seconds(10);
        append_event(&log, &e1).unwrap();
        compact_t(cache_dir, "agent-1", true).unwrap();

        let state = read_checkpoint(cache_dir).unwrap();
        let embedded_watermark = state.watermark.clone().unwrap();

        let checkpoint_dir = cache_dir.join("checkpoint");
        let legacy_wm_path = checkpoint_dir.join("watermark.json");
        let wm_json = serde_json::to_string(&embedded_watermark).unwrap();
        std::fs::write(&legacy_wm_path, &wm_json).unwrap();

        let mut state_no_wm = state;
        state_no_wm.watermark = None;
        write_checkpoint(cache_dir, &state_no_wm).unwrap();

        let e2 = make_envelope(
            "agent-1",
            2,
            Event::LabelAdded {
                issue_uuid: uuid,
                label: "migrated".to_string(),
            },
        );
        append_event(&log, &e2).unwrap();

        let result = compact_t(cache_dir, "agent-1", true).unwrap().unwrap();
        assert_eq!(
            result.events_processed, 1,
            "Only the new event should be processed"
        );

        let final_state = read_checkpoint(cache_dir).unwrap();
        assert!(
            final_state.issues[&uuid].labels.contains("migrated"),
            "Label should be applied after migration"
        );

        assert!(
            final_state.watermark.is_some(),
            "Checkpoint should have embedded watermark after migration"
        );
    }

    #[test]
    fn test_materialize_removes_released_lock_file() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path();
        setup_cache(cache_dir);

        let lock_path = cache_dir.join("locks/7.json");
        let log = cache_dir.join("agents/agent-1/events.log");
        let now = Utc::now();

        let mut e_claim = make_envelope(
            "agent-1",
            1,
            Event::LockClaimed {
                issue_display_id: 7,
                branch: Some("feature/remove-test".to_string()),
            },
        );
        e_claim.timestamp = now - Duration::seconds(5);
        append_event(&log, &e_claim).unwrap();
        compact_t(cache_dir, "agent-1", true).unwrap();
        assert!(
            lock_path.exists(),
            "Lock file should exist after claim compaction"
        );

        let mut e_release = make_envelope(
            "agent-1",
            2,
            Event::LockReleased {
                issue_display_id: 7,
            },
        );
        e_release.timestamp = now - Duration::seconds(2);
        append_event(&log, &e_release).unwrap();
        compact_t(cache_dir, "agent-1", true).unwrap();

        assert!(
            !lock_path.exists(),
            "Lock file should be removed after release compaction"
        );
    }

    #[test]
    fn test_check_unsigned_with_invalid_signature_and_trust_file() {
        use std::process::Command;
        if Command::new("ssh-keygen").arg("--help").output().is_err() {
            eprintln!("Skipping: ssh-keygen not available");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path();
        setup_cache(cache_dir);

        let keys_dir = dir.path().join("keys");
        std::fs::create_dir_all(&keys_dir).unwrap();
        let private_key_path = keys_dir.join("agent_ed25519");
        let public_key_path = keys_dir.join("agent_ed25519.pub");
        let output = Command::new("ssh-keygen")
            .args([
                "-t",
                "ed25519",
                "-f",
                &private_key_path.to_string_lossy(),
                "-N",
                "",
                "-C",
                "check-test@host",
            ])
            .output()
            .unwrap();
        assert!(output.status.success());

        let public_key = std::fs::read_to_string(&public_key_path).unwrap();
        let public_key = public_key.trim();

        let signers_path = cache_dir.join("trust").join("allowed_signers");
        std::fs::create_dir_all(signers_path.parent().unwrap()).unwrap();

        std::fs::write(
            &signers_path,
            format!("check-agent@crosslink {public_key}\n"),
        )
        .unwrap();

        let mut env = make_envelope(
            "check-agent",
            1,
            Event::IssueCreated {
                uuid: Uuid::new_v4(),
                title: "Invalid sig".to_string(),
                description: None,
                priority: "medium".to_string(),
                labels: vec![],
                parent_uuid: None,
                created_by: "check-agent".to_string(),
                display_id: None,
                scheduled_at: None,
                due_at: None,
            },
        );
        env.signed_by = Some("SHA256:fake".to_string());
        env.signature = Some("aW52YWxpZHNpZw==".to_string());

        let mut state = CheckpointState::default();
        check_unsigned(&mut state, &env, &signers_path);
        assert_eq!(
            state.unsigned_event_warnings.len(),
            1,
            "Should warn when signature is present but invalid"
        );
        assert_eq!(state.unsigned_event_warnings[0].agent_id, "check-agent");
    }

    #[test]
    fn test_read_lock_info_valid() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("compaction.lock");

        let now = Utc::now();
        let content = serde_json::json!({
            "agent_id": "agent-test",
            "acquired_at": now.to_rfc3339(),
            "pid": 12345,
        });
        std::fs::write(&lock_path, content.to_string()).unwrap();

        let info = CompactionLockGuard::read_lock_info(&lock_path).unwrap();
        assert_eq!(info.agent_id, "agent-test");

        let diff = (info.acquired_at - now).num_milliseconds().abs();
        assert!(diff < 1000, "Parsed time should be close to written time");
    }

    fn issue_created(uuid: Uuid, title: &str) -> Event {
        Event::IssueCreated {
            uuid,
            title: title.to_string(),
            description: None,
            priority: "medium".to_string(),
            labels: vec![],
            parent_uuid: None,
            created_by: "agent-1".to_string(),
            display_id: None,
            scheduled_at: None,
            due_at: None,
        }
    }

    fn issue_created_with_id(uuid: Uuid, title: &str, display_id: i64) -> Event {
        Event::IssueCreated {
            uuid,
            title: title.to_string(),
            description: None,
            priority: "medium".to_string(),
            labels: vec![],
            parent_uuid: None,
            created_by: "agent-1".to_string(),
            display_id: Some(display_id),
            scheduled_at: None,
            due_at: None,
        }
    }

    fn comment_added(issue: Uuid, comment: Uuid, body: &str) -> Event {
        Event::CommentAdded {
            issue_uuid: issue,
            comment_uuid: comment,
            display_id: None,
            author: "agent-1".to_string(),
            content: body.to_string(),
            created_at: Utc::now(),
            kind: "note".to_string(),
            trigger_type: None,
            intervention_context: None,
            driver_key_fingerprint: None,
            signed_by: None,
            signature: None,
        }
    }

    #[test]
    fn test_new_variant_serde_roundtrip() {
        let variants = [
            comment_added(Uuid::new_v4(), Uuid::new_v4(), "hi"),
            Event::TimeEntryAdded {
                issue_uuid: Uuid::new_v4(),
                entry_uuid: Uuid::new_v4(),
                display_id: Some(2),
                started_at: Utc::now(),
                ended_at: None,
                duration_seconds: Some(60),
            },
            Event::IssueDeleted {
                uuid: Uuid::new_v4(),
            },
            Event::MilestoneCreated {
                uuid: Uuid::new_v4(),
                display_id: Some(1),
                name: "v1".to_string(),
                description: Some("d".to_string()),
                created_at: Utc::now(),
            },
            Event::MilestoneClosed {
                uuid: Uuid::new_v4(),
                closed_at: Utc::now(),
            },
            Event::MilestoneDeleted {
                uuid: Uuid::new_v4(),
            },
            Event::ScheduleChanged {
                issue_uuid: Uuid::new_v4(),
                scheduled_at: Some(Utc::now()),
                due_at: None,
            },
        ];
        for ev in variants {
            let json = serde_json::to_string(&ev).unwrap();
            let parsed: Event = serde_json::from_str(&json).unwrap();
            assert_eq!(
                serde_json::to_string(&parsed).unwrap(),
                json,
                "round-trip mismatch"
            );
        }
    }

    #[test]
    fn test_display_id_collision_deterministic_winner() {
        let mut state = CheckpointState::default();
        let mut ci = HashSet::new();
        let mut cl = HashSet::new();
        let uuid_a = Uuid::new_v4();
        let uuid_b = Uuid::new_v4();

        let mut e_a = make_envelope("agent-a", 1, issue_created_with_id(uuid_a, "A", 5));
        e_a.timestamp = Utc::now() - Duration::seconds(2);
        let mut e_b = make_envelope("agent-b", 1, issue_created_with_id(uuid_b, "B", 5));
        e_b.timestamp = Utc::now() - Duration::seconds(1);

        apply(&mut state, &e_a, &mut ci, &mut cl);
        apply(&mut state, &e_b, &mut ci, &mut cl);

        assert_eq!(state.display_id_map[&uuid_a], 5, "first claimant keeps 5");
        assert_ne!(state.display_id_map[&uuid_b], 5, "loser reassigned");
        assert_eq!(state.display_id_map[&uuid_b], 6, "loser gets next free id");

        apply(&mut state, &e_a, &mut ci, &mut cl);
        apply(&mut state, &e_b, &mut ci, &mut cl);
        assert_eq!(state.display_id_map[&uuid_a], 5);
        assert_eq!(state.display_id_map[&uuid_b], 6);
    }

    #[test]
    fn test_display_id_carried_bumps_next() {
        let mut state = CheckpointState {
            next_display_id: 3,
            ..Default::default()
        };
        let mut ci = HashSet::new();
        let mut cl = HashSet::new();
        let uuid = Uuid::new_v4();
        let env = make_envelope("agent-1", 1, issue_created_with_id(uuid, "X", 7));
        apply(&mut state, &env, &mut ci, &mut cl);
        assert_eq!(state.display_id_map[&uuid], 7);
        assert_eq!(state.next_display_id, 8);
    }

    #[test]
    fn test_display_id_none_allocates() {
        let mut state = CheckpointState::default();
        let mut ci = HashSet::new();
        let mut cl = HashSet::new();
        let uuid = Uuid::new_v4();
        let env = make_envelope("agent-1", 1, issue_created(uuid, "X"));
        apply(&mut state, &env, &mut ci, &mut cl);
        assert_eq!(state.display_id_map[&uuid], 1);
        assert_eq!(state.next_display_id, 2);
    }

    #[test]
    fn test_comment_added_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path();
        setup_cache(cache_dir);

        let issue = Uuid::new_v4();
        let comment = Uuid::new_v4();
        let log = cache_dir.join("agents/agent-1/events.log");

        let mut e1 = make_envelope("agent-1", 1, issue_created(issue, "I"));
        e1.timestamp = Utc::now() - Duration::seconds(10);
        let mut e2 = make_envelope("agent-1", 2, comment_added(issue, comment, "hello"));
        e2.timestamp = Utc::now() - Duration::seconds(5);

        let mut e3 = make_envelope("agent-1", 3, comment_added(issue, comment, "hello-dup"));
        e3.timestamp = Utc::now() - Duration::seconds(4);

        append_event(&log, &e1).unwrap();
        append_event(&log, &e2).unwrap();
        append_event(&log, &e3).unwrap();

        compact_t(cache_dir, "agent-1", true).unwrap();

        let state = read_checkpoint(cache_dir).unwrap();
        let issue_state = &state.issues[&issue];
        assert_eq!(issue_state.comments.len(), 1, "duplicate left one comment");

        assert_eq!(issue_state.comments[&comment].content, "hello");
        assert_eq!(issue_state.comments[&comment].display_id, Some(1));
    }

    #[test]
    fn test_comment_survives_incremental_compaction() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path();
        setup_cache(cache_dir);

        let issue = Uuid::new_v4();
        let comment1 = Uuid::new_v4();
        let comment2 = Uuid::new_v4();
        let log = cache_dir.join("agents/agent-1/events.log");

        let mut e1 = make_envelope("agent-1", 1, issue_created(issue, "I"));
        e1.timestamp = Utc::now() - Duration::seconds(10);
        let mut e2 = make_envelope("agent-1", 2, comment_added(issue, comment1, "first"));
        e2.timestamp = Utc::now() - Duration::seconds(5);
        append_event(&log, &e1).unwrap();
        append_event(&log, &e2).unwrap();

        compact_t(cache_dir, "agent-1", true).unwrap();

        let mut e3 = make_envelope("agent-1", 3, comment_added(issue, comment2, "second"));
        e3.timestamp = Utc::now() - Duration::seconds(1);
        append_event(&log, &e3).unwrap();
        compact_t(cache_dir, "agent-1", true).unwrap();

        let state = read_checkpoint(cache_dir).unwrap();
        let issue_state = &state.issues[&issue];
        assert_eq!(issue_state.comments.len(), 2, "both comments retained");
        assert!(issue_state.comments.contains_key(&comment1));
        assert!(issue_state.comments.contains_key(&comment2));
    }

    #[test]
    fn test_time_entry_added_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path();
        setup_cache(cache_dir);

        let issue = Uuid::new_v4();
        let entry = Uuid::new_v4();
        let log = cache_dir.join("agents/agent-1/events.log");

        let mut e1 = make_envelope("agent-1", 1, issue_created(issue, "I"));
        e1.timestamp = Utc::now() - Duration::seconds(10);
        let te = |seq: u64| {
            make_envelope(
                "agent-1",
                seq,
                Event::TimeEntryAdded {
                    issue_uuid: issue,
                    entry_uuid: entry,
                    display_id: Some(1),
                    started_at: Utc::now(),
                    ended_at: None,
                    duration_seconds: Some(120),
                },
            )
        };
        let mut e2 = te(2);
        e2.timestamp = Utc::now() - Duration::seconds(5);
        let mut e3 = te(3);
        e3.timestamp = Utc::now() - Duration::seconds(4);

        append_event(&log, &e1).unwrap();
        append_event(&log, &e2).unwrap();
        append_event(&log, &e3).unwrap();

        compact_t(cache_dir, "agent-1", true).unwrap();

        let state = read_checkpoint(cache_dir).unwrap();
        assert_eq!(state.issues[&issue].time_entries.len(), 1);
        assert_eq!(
            state.issues[&issue].time_entries[&entry].duration_seconds,
            Some(120)
        );
    }

    #[test]
    fn test_tombstone_blocks_later_events_and_recreate() {
        let mut state = CheckpointState::default();
        let mut ci = HashSet::new();
        let mut cl = HashSet::new();
        let uuid = Uuid::new_v4();

        let e_create = make_envelope("agent-1", 1, issue_created(uuid, "Doomed"));
        apply(&mut state, &e_create, &mut ci, &mut cl);
        let e_del = make_envelope("agent-1", 2, Event::IssueDeleted { uuid });
        apply(&mut state, &e_del, &mut ci, &mut cl);
        assert!(!state.issues.contains_key(&uuid));
        assert!(state.deleted_issues.contains(&uuid));

        let e_label = make_envelope(
            "agent-1",
            3,
            Event::LabelAdded {
                issue_uuid: uuid,
                label: "ghost".to_string(),
            },
        );
        apply(&mut state, &e_label, &mut ci, &mut cl);
        let e_update = make_envelope(
            "agent-1",
            4,
            Event::IssueUpdated {
                uuid,
                title: Some("Zombie".to_string()),
                description: None,
                priority: None,
            },
        );
        apply(&mut state, &e_update, &mut ci, &mut cl);

        let e_recreate = make_envelope("agent-1", 5, issue_created(uuid, "Resurrected"));
        apply(&mut state, &e_recreate, &mut ci, &mut cl);

        assert!(
            !state.issues.contains_key(&uuid),
            "deletion wins forever — no resurrection"
        );
    }

    #[test]
    fn test_tombstone_materialize_does_not_recreate_file() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path();
        setup_cache(cache_dir);

        let uuid = Uuid::new_v4();
        let log = cache_dir.join("agents/agent-1/events.log");

        let mut e1 = make_envelope("agent-1", 1, issue_created(uuid, "Doomed"));
        e1.timestamp = Utc::now() - Duration::seconds(10);
        append_event(&log, &e1).unwrap();
        compact_t(cache_dir, "agent-1", true).unwrap();

        let issue_path = cache_dir
            .join("issues")
            .join(uuid.to_string())
            .join("issue.json");
        assert!(issue_path.exists(), "issue file created");

        std::fs::remove_file(&issue_path).unwrap();

        let mut e2 = make_envelope("agent-1", 2, Event::IssueDeleted { uuid });
        e2.timestamp = Utc::now() - Duration::seconds(1);
        append_event(&log, &e2).unwrap();
        compact_t(cache_dir, "agent-1", true).unwrap();

        assert!(
            !issue_path.exists(),
            "materialize must not recreate a tombstoned issue"
        );
        let state = read_checkpoint(cache_dir).unwrap();
        assert!(state.deleted_issues.contains(&uuid));
        assert!(!state.issues.contains_key(&uuid));
    }

    #[test]
    fn test_schedule_changed_lww_and_clear() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path();
        setup_cache(cache_dir);

        let uuid = Uuid::new_v4();
        let log = cache_dir.join("agents/agent-1/events.log");
        let now = Utc::now();

        let mut e1 = make_envelope("agent-1", 1, issue_created(uuid, "Sched"));
        e1.timestamp = now - Duration::seconds(30);

        let first_sched = now - Duration::seconds(20);
        let mut e2 = make_envelope(
            "agent-1",
            2,
            Event::ScheduleChanged {
                issue_uuid: uuid,
                scheduled_at: Some(first_sched),
                due_at: Some(now + Duration::seconds(100)),
            },
        );
        e2.timestamp = now - Duration::seconds(15);

        let later_due = now + Duration::seconds(200);
        let mut e3 = make_envelope(
            "agent-1",
            3,
            Event::ScheduleChanged {
                issue_uuid: uuid,
                scheduled_at: None,
                due_at: Some(later_due),
            },
        );
        e3.timestamp = now - Duration::seconds(5);

        append_event(&log, &e1).unwrap();
        append_event(&log, &e2).unwrap();
        append_event(&log, &e3).unwrap();

        compact_t(cache_dir, "agent-1", true).unwrap();

        let state = read_checkpoint(cache_dir).unwrap();
        let issue = &state.issues[&uuid];
        assert!(
            issue.scheduled_at.is_none(),
            "later event cleared scheduled"
        );
        assert_eq!(issue.due_at, Some(later_due), "later due wins (LWW)");
    }

    #[test]
    fn test_issue_created_applies_schedule_fields() {
        let mut state = CheckpointState::default();
        let mut ci = HashSet::new();
        let mut cl = HashSet::new();
        let uuid = Uuid::new_v4();
        let sched = Utc::now() + Duration::seconds(10);
        let due = Utc::now() + Duration::seconds(20);
        let env = make_envelope(
            "agent-1",
            1,
            Event::IssueCreated {
                uuid,
                title: "S".to_string(),
                description: None,
                priority: "medium".to_string(),
                labels: vec![],
                parent_uuid: None,
                created_by: "agent-1".to_string(),
                display_id: None,
                scheduled_at: Some(sched),
                due_at: Some(due),
            },
        );
        apply(&mut state, &env, &mut ci, &mut cl);
        assert_eq!(state.issues[&uuid].scheduled_at, Some(sched));
        assert_eq!(state.issues[&uuid].due_at, Some(due));
    }

    #[test]
    fn test_milestone_lifecycle_with_id_adoption() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path();
        setup_cache(cache_dir);

        let ms = Uuid::new_v4();
        let issue = Uuid::new_v4();
        let log = cache_dir.join("agents/agent-1/events.log");
        let now = Utc::now();

        let mut e1 = make_envelope(
            "agent-1",
            1,
            Event::MilestoneCreated {
                uuid: ms,
                display_id: Some(4),
                name: "Release".to_string(),
                description: None,
                created_at: now - Duration::seconds(30),
            },
        );
        e1.timestamp = now - Duration::seconds(30);

        let mut e2 = make_envelope("agent-1", 2, issue_created(issue, "linked"));
        e2.timestamp = now - Duration::seconds(25);

        let mut e3 = make_envelope(
            "agent-1",
            3,
            Event::MilestoneAssigned {
                issue_uuid: issue,
                milestone_uuid: Some(ms),
            },
        );
        e3.timestamp = now - Duration::seconds(20);

        let closed_at = now - Duration::seconds(15);
        let mut e4 = make_envelope(
            "agent-1",
            4,
            Event::MilestoneClosed {
                uuid: ms,
                closed_at,
            },
        );
        e4.timestamp = now - Duration::seconds(15);

        append_event(&log, &e1).unwrap();
        append_event(&log, &e2).unwrap();
        append_event(&log, &e3).unwrap();
        append_event(&log, &e4).unwrap();

        compact_t(cache_dir, "agent-1", true).unwrap();

        let state = read_checkpoint(cache_dir).unwrap();
        let m = &state.milestones[&ms];
        assert_eq!(m.display_id, Some(4), "carried milestone id adopted");
        assert_eq!(state.next_milestone_id, 5, "next bumped past adopted");
        assert_eq!(m.status, crate::models::IssueStatus::Closed);
        assert_eq!(m.closed_at, Some(closed_at));
        assert_eq!(state.issues[&issue].milestone_uuid, Some(ms));

        let mut e5 = make_envelope("agent-1", 5, Event::MilestoneDeleted { uuid: ms });
        e5.timestamp = now - Duration::seconds(5);
        append_event(&log, &e5).unwrap();
        compact_t(cache_dir, "agent-1", true).unwrap();

        let state = read_checkpoint(cache_dir).unwrap();
        assert!(!state.milestones.contains_key(&ms), "milestone removed");
        assert_eq!(
            state.issues[&issue].milestone_uuid, None,
            "delete cleared issue linkage"
        );
    }

    #[test]
    fn test_deterministic_reduction_all_new_variants() {
        let base = Utc::now() - Duration::seconds(100);
        let issue1 = Uuid::new_v4();
        let issue2 = Uuid::new_v4();
        let comment1 = Uuid::new_v4();
        let entry1 = Uuid::new_v4();
        let ms = Uuid::new_v4();

        let mk = |agent: &str, seq: u64, secs: i64, ev: Event| {
            let mut e = make_envelope(agent, seq, ev);
            e.timestamp = base + Duration::seconds(secs);
            e
        };

        let envelopes = [
            mk("agent-a", 1, 0, issue_created_with_id(issue1, "One", 1)),
            mk("agent-b", 1, 1, issue_created_with_id(issue2, "Two", 1)),
            mk(
                "agent-a",
                2,
                2,
                Event::MilestoneCreated {
                    uuid: ms,
                    display_id: Some(1),
                    name: "M".to_string(),
                    description: None,
                    created_at: base + Duration::seconds(2),
                },
            ),
            mk(
                "agent-b",
                2,
                3,
                Event::MilestoneAssigned {
                    issue_uuid: issue1,
                    milestone_uuid: Some(ms),
                },
            ),
            mk("agent-a", 3, 4, comment_added(issue1, comment1, "c")),
            mk(
                "agent-b",
                3,
                5,
                Event::TimeEntryAdded {
                    issue_uuid: issue1,
                    entry_uuid: entry1,
                    display_id: Some(1),
                    started_at: base + Duration::seconds(5),
                    ended_at: None,
                    duration_seconds: Some(30),
                },
            ),
            mk(
                "agent-a",
                4,
                6,
                Event::ScheduleChanged {
                    issue_uuid: issue2,
                    scheduled_at: Some(base + Duration::seconds(50)),
                    due_at: None,
                },
            ),
            mk("agent-b", 4, 7, Event::IssueDeleted { uuid: issue2 }),
        ];

        let reduce_in_order = |order: &[usize]| -> CheckpointState {
            let dir = tempfile::tempdir().unwrap();
            let cache_dir = dir.path();
            setup_cache(cache_dir);
            for &i in order {
                let env = &envelopes[i];
                let log = cache_dir.join(format!("agents/{}/events.log", env.agent_id));
                append_event(&log, env).unwrap();
            }
            compact_t(cache_dir, "agent-a", true).unwrap();
            read_checkpoint(cache_dir).unwrap()
        };

        let forward: Vec<usize> = (0..envelopes.len()).collect();
        let mut shuffled = forward.clone();
        shuffled.reverse();

        let s1 = reduce_in_order(&forward);
        let s2 = reduce_in_order(&shuffled);

        let j1 = serde_json::to_string_pretty(&s1).unwrap();
        let j2 = serde_json::to_string_pretty(&s2).unwrap();
        assert_eq!(j1, j2, "reduction must be order-independent");

        assert!(s1.deleted_issues.contains(&issue2), "issue2 deleted");
        assert!(!s1.issues.contains_key(&issue2), "issue2 gone");
        assert_eq!(s1.issues[&issue1].comments.len(), 1);
        assert_eq!(s1.issues[&issue1].time_entries.len(), 1);
        assert_eq!(s1.issues[&issue1].milestone_uuid, Some(ms));

        assert_eq!(s1.display_id_map[&issue1], 1);
        assert_eq!(s1.milestones[&ms].display_id, Some(1));
    }
}
