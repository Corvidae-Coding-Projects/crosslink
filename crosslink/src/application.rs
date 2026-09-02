use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};

use crate::agent_requests::{AgentRequest, AgentRequestAck};
use crate::db::sentinel::{
    DispatchMetric, NewDispatch, RunCounters, SentinelDispatch, SentinelRun,
};
use crate::db::Database;
use crate::db::UsageSummaryRow;
use crate::models::{Comment, Issue, Milestone, Session, TokenUsage};
use crate::shared_writer::{
    DescriptionUpdate, FieldUpdate, ImportedIssueSpec, IssueUpdate, LockClaimResult, PushOutcome,
    SharedWriter,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthorityMode {
    Local,
    Shared,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    CreateIssue {
        title: String,
        description: Option<String>,
        priority: String,
        labels: Vec<String>,
        scheduled_at: Option<DateTime<Utc>>,
        due_at: Option<DateTime<Utc>>,
    },
    CreateSubissue {
        parent_id: i64,
        title: String,
        description: Option<String>,
        priority: String,
        labels: Vec<String>,
    },
    ImportIssues {
        specs: Vec<ImportedIssueSpec>,
    },
    UpdateIssue {
        id: i64,
        title: Option<String>,
        description: DescriptionChange,
        status: Option<String>,
        priority: Option<String>,
        scheduled_at: DateTimeChange,
        due_at: DateTimeChange,
    },
    DeleteIssue {
        id: i64,
    },
    ArchiveIssue {
        id: i64,
    },
    UnarchiveIssue {
        id: i64,
    },
    AddComment {
        issue_id: i64,
        content: String,
        kind: String,
    },
    AddIntervention {
        issue_id: i64,
        content: String,
        trigger_type: String,
        context: Option<String>,
        driver_key_fingerprint: Option<String>,
    },
    AddLabel {
        issue_id: i64,
        label: String,
    },
    RemoveLabel {
        issue_id: i64,
        label: String,
    },
    AddDependency {
        issue_id: i64,
        blocker_id: i64,
    },
    RemoveDependency {
        issue_id: i64,
        blocker_id: i64,
    },
    AddRelation {
        issue_id: i64,
        related_id: i64,
    },
    RemoveRelation {
        issue_id: i64,
        related_id: i64,
    },
    CreateMilestone {
        name: String,
        description: Option<String>,
    },
    AssignMilestone {
        milestone_id: i64,
        issue_ids: Vec<i64>,
    },
    ClearMilestone {
        milestone_id: i64,
        issue_id: i64,
    },
    CloseMilestone {
        id: i64,
    },
    DeleteMilestone {
        id: i64,
    },
    ClaimLock {
        issue_id: i64,
        branch: Option<String>,
    },
    ReleaseLock {
        issue_id: i64,
    },
    StealLock {
        issue_id: i64,
        stale_agent_id: String,
        branch: Option<String>,
    },
    ForceReleaseLock {
        issue_id: i64,
        stale_agent_id: String,
    },
    SetSessionIssue {
        session_id: i64,
        issue_id: i64,
    },
    ClearSessionIssue {
        session_id: i64,
    },
    WriteAgentRequest {
        target_agent_id: String,
        request: AgentRequest,
    },
    WriteAgentAck {
        target_agent_id: String,
        ack: AgentRequestAck,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DescriptionChange {
    Unchanged,
    Clear,
    Set(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DateTimeChange {
    Unchanged,
    Clear,
    Set(DateTime<Utc>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct LocalTokenUsage {
    pub agent_id: String,
    pub session_id: Option<i64>,
    pub provider: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cached_input_tokens: Option<i64>,
    pub reasoning_output_tokens: Option<i64>,
    pub cache_read_tokens: Option<i64>,
    pub cache_creation_tokens: Option<i64>,
    pub model: String,
    pub cost_estimate: Option<f64>,
    pub provider_metadata_json: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandResult {
    None,
    Id(i64),
    Changed(bool),
    Lock(LockClaimResult),
    Imported(Vec<(uuid::Uuid, i64)>),
    Push(PushOutcome),
}

pub trait CommandService {
    fn execute(&self, command: Command) -> Result<CommandResult>;

    fn create_issue(
        &self,
        title: &str,
        description: Option<&str>,
        priority: &str,
        scheduled_at: Option<DateTime<Utc>>,
        due_at: Option<DateTime<Utc>>,
    ) -> Result<i64> {
        self.create_issue_with_labels(title, description, priority, &[], scheduled_at, due_at)
    }

    fn create_issue_with_labels(
        &self,
        title: &str,
        description: Option<&str>,
        priority: &str,
        labels: &[String],
        scheduled_at: Option<DateTime<Utc>>,
        due_at: Option<DateTime<Utc>>,
    ) -> Result<i64> {
        expect_id(self.execute(Command::CreateIssue {
            title: title.to_string(),
            description: description.map(str::to_string),
            priority: priority.to_string(),
            labels: labels.to_vec(),
            scheduled_at,
            due_at,
        })?)
    }

    fn create_subissue(
        &self,
        parent_id: i64,
        title: &str,
        description: Option<&str>,
        priority: &str,
    ) -> Result<i64> {
        self.create_subissue_with_labels(parent_id, title, description, priority, &[])
    }

    fn create_subissue_with_labels(
        &self,
        parent_id: i64,
        title: &str,
        description: Option<&str>,
        priority: &str,
        labels: &[String],
    ) -> Result<i64> {
        expect_id(self.execute(Command::CreateSubissue {
            parent_id,
            title: title.to_string(),
            description: description.map(str::to_string),
            priority: priority.to_string(),
            labels: labels.to_vec(),
        })?)
    }

    fn update_issue(&self, id: i64, update: OwnedIssueUpdate) -> Result<()> {
        expect_none(self.execute(Command::UpdateIssue {
            id,
            title: update.title,
            description: update.description,
            status: update.status,
            priority: update.priority,
            scheduled_at: update.scheduled_at,
            due_at: update.due_at,
        })?)
    }

    fn import_issues(&self, specs: &[ImportedIssueSpec]) -> Result<Vec<(uuid::Uuid, i64)>> {
        match self.execute(Command::ImportIssues {
            specs: specs.to_vec(),
        })? {
            CommandResult::Imported(assigned) => Ok(assigned),
            other => bail!("command returned unexpected result: {other:?}"),
        }
    }

    fn close_issue(&self, id: i64) -> Result<()> {
        self.update_issue(id, OwnedIssueUpdate::status("closed"))
    }

    fn reopen_issue(&self, id: i64) -> Result<()> {
        self.update_issue(id, OwnedIssueUpdate::status("open"))
    }

    fn archive_issue(&self, id: i64) -> Result<()> {
        expect_none(self.execute(Command::ArchiveIssue { id })?)
    }

    fn unarchive_issue(&self, id: i64) -> Result<()> {
        expect_none(self.execute(Command::UnarchiveIssue { id })?)
    }

    fn delete_issue(&self, id: i64) -> Result<()> {
        expect_none(self.execute(Command::DeleteIssue { id })?)
    }

    fn add_comment(&self, issue_id: i64, content: &str, kind: &str) -> Result<i64> {
        expect_id(self.execute(Command::AddComment {
            issue_id,
            content: content.to_string(),
            kind: kind.to_string(),
        })?)
    }

    fn add_intervention(
        &self,
        issue_id: i64,
        content: &str,
        trigger_type: &str,
        context: Option<&str>,
        driver_key_fingerprint: Option<&str>,
    ) -> Result<i64> {
        expect_id(self.execute(Command::AddIntervention {
            issue_id,
            content: content.to_string(),
            trigger_type: trigger_type.to_string(),
            context: context.map(str::to_string),
            driver_key_fingerprint: driver_key_fingerprint.map(str::to_string),
        })?)
    }

    fn add_label(&self, issue_id: i64, label: &str) -> Result<bool> {
        expect_changed(self.execute(Command::AddLabel {
            issue_id,
            label: label.to_string(),
        })?)
    }

    fn remove_label(&self, issue_id: i64, label: &str) -> Result<bool> {
        expect_changed(self.execute(Command::RemoveLabel {
            issue_id,
            label: label.to_string(),
        })?)
    }

    fn add_dependency(&self, issue_id: i64, blocker_id: i64) -> Result<bool> {
        expect_changed(self.execute(Command::AddDependency {
            issue_id,
            blocker_id,
        })?)
    }

    fn remove_dependency(&self, issue_id: i64, blocker_id: i64) -> Result<bool> {
        expect_changed(self.execute(Command::RemoveDependency {
            issue_id,
            blocker_id,
        })?)
    }

    fn add_relation(&self, issue_id: i64, related_id: i64) -> Result<bool> {
        expect_changed(self.execute(Command::AddRelation {
            issue_id,
            related_id,
        })?)
    }

    fn remove_relation(&self, issue_id: i64, related_id: i64) -> Result<bool> {
        expect_changed(self.execute(Command::RemoveRelation {
            issue_id,
            related_id,
        })?)
    }

    fn create_milestone(&self, name: &str, description: Option<&str>) -> Result<i64> {
        expect_id(self.execute(Command::CreateMilestone {
            name: name.to_string(),
            description: description.map(str::to_string),
        })?)
    }

    fn assign_milestone(&self, milestone_id: i64, issue_ids: &[i64]) -> Result<()> {
        expect_none(self.execute(Command::AssignMilestone {
            milestone_id,
            issue_ids: issue_ids.to_vec(),
        })?)
    }

    fn clear_milestone(&self, milestone_id: i64, issue_id: i64) -> Result<bool> {
        expect_changed(self.execute(Command::ClearMilestone {
            milestone_id,
            issue_id,
        })?)
    }

    fn close_milestone(&self, id: i64) -> Result<()> {
        expect_none(self.execute(Command::CloseMilestone { id })?)
    }

    fn delete_milestone(&self, id: i64) -> Result<()> {
        expect_none(self.execute(Command::DeleteMilestone { id })?)
    }

    fn claim_lock(&self, issue_id: i64, branch: Option<&str>) -> Result<LockClaimResult> {
        expect_lock(self.execute(Command::ClaimLock {
            issue_id,
            branch: branch.map(str::to_string),
        })?)
    }

    fn release_lock(&self, issue_id: i64) -> Result<bool> {
        expect_changed(self.execute(Command::ReleaseLock { issue_id })?)
    }

    fn steal_lock(
        &self,
        issue_id: i64,
        stale_agent_id: &str,
        branch: Option<&str>,
    ) -> Result<LockClaimResult> {
        expect_lock(self.execute(Command::StealLock {
            issue_id,
            stale_agent_id: stale_agent_id.to_string(),
            branch: branch.map(str::to_string),
        })?)
    }

    fn force_release_lock(&self, issue_id: i64, stale_agent_id: &str) -> Result<bool> {
        expect_changed(self.execute(Command::ForceReleaseLock {
            issue_id,
            stale_agent_id: stale_agent_id.to_string(),
        })?)
    }

    fn set_session_issue(&self, session_id: i64, issue_id: i64) -> Result<bool> {
        expect_changed(self.execute(Command::SetSessionIssue {
            session_id,
            issue_id,
        })?)
    }

    fn clear_session_issue(&self, session_id: i64) -> Result<bool> {
        expect_changed(self.execute(Command::ClearSessionIssue { session_id })?)
    }

    fn write_agent_request(
        &self,
        target_agent_id: &str,
        request: &AgentRequest,
    ) -> Result<PushOutcome> {
        expect_push(self.execute(Command::WriteAgentRequest {
            target_agent_id: target_agent_id.to_string(),
            request: request.clone(),
        })?)
    }

    fn write_agent_ack(&self, target_agent_id: &str, ack: &AgentRequestAck) -> Result<PushOutcome> {
        expect_push(self.execute(Command::WriteAgentAck {
            target_agent_id: target_agent_id.to_string(),
            ack: ack.clone(),
        })?)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedIssueUpdate {
    pub title: Option<String>,
    pub description: DescriptionChange,
    pub status: Option<String>,
    pub priority: Option<String>,
    pub scheduled_at: DateTimeChange,
    pub due_at: DateTimeChange,
}

impl OwnedIssueUpdate {
    #[must_use]
    pub fn status(status: &str) -> Self {
        Self {
            title: None,
            description: DescriptionChange::Unchanged,
            status: Some(status.to_string()),
            priority: None,
            scheduled_at: DateTimeChange::Unchanged,
            due_at: DateTimeChange::Unchanged,
        }
    }
}

pub trait QueryService {
    fn list_issue_records(&self) -> Result<Vec<crate::issue_file::IssueFile>>;
    fn get_issue(&self, id: i64) -> Result<Option<Issue>>;
    fn require_issue(&self, id: i64) -> Result<Issue>;
    fn list_issues(
        &self,
        status: Option<&str>,
        label: Option<&str>,
        priority: Option<&str>,
    ) -> Result<Vec<Issue>>;
    fn search_issues(&self, query: &str) -> Result<Vec<Issue>>;
    fn get_subissues(&self, parent_id: i64) -> Result<Vec<Issue>>;
    fn get_labels(&self, issue_id: i64) -> Result<Vec<String>>;
    fn get_labels_batch(
        &self,
        issue_ids: &[i64],
    ) -> Result<std::collections::HashMap<i64, Vec<String>>>;
    fn get_comments(&self, issue_id: i64) -> Result<Vec<Comment>>;
    fn search_comments(&self, query: &str) -> Result<Vec<(Comment, i64, String)>>;
    fn get_blockers(&self, issue_id: i64) -> Result<Vec<i64>>;
    fn get_blocking(&self, issue_id: i64) -> Result<Vec<i64>>;
    fn get_blocker_counts_batch(
        &self,
        issue_ids: &[i64],
    ) -> Result<std::collections::HashMap<i64, usize>>;
    fn list_blocked_issues(&self) -> Result<Vec<Issue>>;
    fn list_ready_issues(&self) -> Result<Vec<Issue>>;
    fn list_archived_issues(&self) -> Result<Vec<Issue>>;
    fn get_related_issues(&self, issue_id: i64) -> Result<Vec<Issue>>;
    fn get_related_issue_ids(&self, issue_id: i64) -> Result<Vec<i64>>;
    fn get_milestone(&self, id: i64) -> Result<Option<Milestone>>;
    fn list_milestones(&self, status: Option<&str>) -> Result<Vec<Milestone>>;
    fn get_milestone_issues(&self, milestone_id: i64) -> Result<Vec<Issue>>;
    fn get_issue_milestone(&self, issue_id: i64) -> Result<Option<Milestone>>;
    fn get_milestone_uuid_for_issue(&self, issue_id: i64) -> Result<Option<String>>;
    fn count_issues_since(&self, since: &str) -> Result<i64>;
    fn count_comments_since(&self, since: &str) -> Result<i64>;
    fn get_issue_count(&self) -> Result<i64>;
    fn get_milestone_count(&self) -> Result<i64>;
    fn get_issue_uuid_by_id(&self, id: i64) -> Result<String>;
    fn get_issue_export_metadata(&self, id: i64) -> Result<(Option<String>, Option<String>)>;
    fn get_comments_with_author(&self, issue_id: i64) -> Result<Vec<crate::db::CommentAuthorRow>>;
    fn get_time_entries_for_issue(&self, issue_id: i64) -> Result<Vec<crate::db::TimeEntryRow>>;
    fn authority_mode(&self) -> AuthorityMode;
}

pub trait LocalStateService {
    fn get_schema_version(&self) -> Result<i32>;
    fn start_session(&self, agent_id: Option<&str>) -> Result<i64>;
    fn end_session(&self, id: i64, notes: Option<&str>) -> Result<bool>;
    fn set_session_action(&self, id: i64, action: &str) -> Result<bool>;
    fn start_timer(&self, issue_id: i64) -> Result<i64>;
    fn stop_timer(&self, issue_id: i64) -> Result<bool>;
    fn get_active_timer(&self) -> Result<Option<(i64, DateTime<Utc>)>>;
    fn get_total_time(&self, issue_id: i64) -> Result<i64>;
    fn get_current_session_for_agent(&self, agent_id: Option<&str>) -> Result<Option<Session>>;
    fn get_last_session_for_agent(&self, agent_id: Option<&str>) -> Result<Option<Session>>;
    fn record_token_usage(&self, usage: &LocalTokenUsage) -> Result<i64>;
    fn get_token_usage(&self, id: i64) -> Result<Option<TokenUsage>>;
    fn list_token_usage(
        &self,
        agent_id: Option<&str>,
        session_id: Option<i64>,
        model: Option<&str>,
        from: Option<&str>,
        to: Option<&str>,
        limit: Option<i64>,
    ) -> Result<Vec<TokenUsage>>;
    fn get_usage_summary(
        &self,
        agent_id: Option<&str>,
        from: Option<&str>,
        to: Option<&str>,
    ) -> Result<Vec<UsageSummaryRow>>;
    fn insert_sentinel_run(&self, run_id: &str, mode: &str) -> Result<i64>;
    fn complete_sentinel_run(&self, run_id: &str, counters: &RunCounters) -> Result<()>;
    fn list_sentinel_runs(&self, limit: usize) -> Result<Vec<SentinelRun>>;
    fn insert_sentinel_dispatch(&self, dispatch: &NewDispatch<'_>) -> Result<i64>;
    fn update_dispatch_outcome(
        &self,
        dispatch_id: i64,
        outcome: &str,
        outcome_detail: &str,
    ) -> Result<()>;
    fn get_pending_dispatches(&self) -> Result<Vec<SentinelDispatch>>;
    fn count_pending_dispatches(&self) -> Result<i64>;
    fn get_latest_dispatch_for_signal(
        &self,
        issue_number: i64,
        label: &str,
    ) -> Result<Option<SentinelDispatch>>;
    fn load_dispatch_seen_set(&self) -> Result<Vec<SentinelDispatch>>;
    fn list_dispatches_for_run(&self, run_id: &str) -> Result<Vec<SentinelDispatch>>;
    fn get_dispatch_metrics(&self) -> Result<Vec<DispatchMetric>>;
    fn get_repeat_failure_counts(&self) -> Result<Vec<(String, i64)>>;
    fn get_escalation_heavy_counts(&self) -> Result<Vec<(String, i64, i64, i64)>>;
}

pub struct RepositoryService<'a> {
    database: &'a Database,
    writer: Option<SharedWriter>,
    allow_domain_commands: bool,
    operation_dir: Option<PathBuf>,
}

impl<'a> RepositoryService<'a> {
    pub fn new(database: &'a Database, crosslink_dir: &Path) -> Result<Self> {
        let writer = SharedWriter::new(crosslink_dir)?;
        Ok(Self {
            database,
            writer,
            allow_domain_commands: true,
            operation_dir: Some(crosslink_dir.to_path_buf()),
        })
    }

    pub const fn projection(database: &'a Database) -> Self {
        Self {
            database,
            writer: None,
            allow_domain_commands: false,
            operation_dir: None,
        }
    }

    pub fn local_state(database: &'a Database, crosslink_dir: &Path) -> Self {
        Self {
            database,
            writer: None,
            allow_domain_commands: false,
            operation_dir: Some(crosslink_dir.to_path_buf()),
        }
    }

    pub fn for_kickoff(database: &'a Database, crosslink_dir: &Path) -> Result<Self> {
        if crate::identity::AgentConfig::load(crosslink_dir)?.is_some() {
            let sync = crate::sync::SyncManager::new(crosslink_dir)?;
            if !sync.remote_exists() && !sync.is_initialized() {
                sync.init_cache()
                    .context("failed to initialize local Git authority for kickoff")?;
                match crate::reconcile::migration::activate_repository(crosslink_dir)
                    .context("failed to reconcile local Git authority for kickoff")?
                {
                    crate::reconcile::migration::RepositoryActivation::ReadyCurrent { .. }
                    | crate::reconcile::migration::RepositoryActivation::ReadyMigrated { .. }
                    | crate::reconcile::migration::RepositoryActivation::ReadyAdopted { .. } => {}
                    crate::reconcile::migration::RepositoryActivation::WaitingForRemote {
                        reason,
                    } => bail!("local kickoff reconciliation is waiting for remote: {reason}"),
                    crate::reconcile::migration::RepositoryActivation::BlockedCorrupt {
                        reason,
                    } => bail!("local kickoff reconciliation is blocked: {reason}"),
                }
            }
        }
        Self::new(database, crosslink_dir)
    }

    fn issue_update<'b>(
        title: &'b Option<String>,
        description: &'b DescriptionChange,
        status: &'b Option<String>,
        priority: &'b Option<String>,
        scheduled_at: DateTimeChange,
        due_at: DateTimeChange,
    ) -> IssueUpdate<'b> {
        let description = match description {
            DescriptionChange::Unchanged => DescriptionUpdate::Unchanged,
            DescriptionChange::Clear => DescriptionUpdate::Clear,
            DescriptionChange::Set(value) => DescriptionUpdate::Set(value),
        };
        IssueUpdate {
            title: title.as_deref(),
            description,
            status: status.as_deref(),
            priority: priority.as_deref(),
            scheduled_at: map_datetime_change(scheduled_at),
            due_at: map_datetime_change(due_at),
        }
    }

    fn acquire_operation(
        &self,
    ) -> Result<Option<crate::reconcile::readiness::MutationOperationPermit>> {
        self.operation_dir
            .as_deref()
            .map(crate::reconcile::readiness::acquire_mutation_operation_permit)
            .transpose()
    }
}

impl CommandService for RepositoryService<'_> {
    fn execute(&self, command: Command) -> Result<CommandResult> {
        if !self.allow_domain_commands
            && !matches!(
                &command,
                Command::SetSessionIssue { .. } | Command::ClearSessionIssue { .. }
            )
        {
            bail!("domain mutation is unavailable through a projection-only service");
        }
        let _operation = self.acquire_operation()?;
        match command {
            Command::CreateIssue {
                title,
                description,
                priority,
                labels,
                scheduled_at,
                due_at,
            } => {
                let id = if let Some(writer) = &self.writer {
                    let id = writer.create_issue(
                        self.database,
                        &title,
                        description.as_deref(),
                        &priority,
                        scheduled_at,
                        due_at,
                    )?;
                    for label in labels {
                        writer.add_label(self.database, id, &label)?;
                    }
                    id
                } else {
                    if scheduled_at.is_some() || due_at.is_some() {
                        bail!("scheduling dates require shared Git authority");
                    }
                    self.database.transaction(|| {
                        let id = self.database.create_issue(
                            &title,
                            description.as_deref(),
                            &priority,
                        )?;
                        for label in labels {
                            self.database.add_label(id, &label)?;
                        }
                        Ok(id)
                    })?
                };
                Ok(CommandResult::Id(id))
            }
            Command::CreateSubissue {
                parent_id,
                title,
                description,
                priority,
                labels,
            } => {
                let id = if let Some(writer) = &self.writer {
                    let id = writer.create_subissue(
                        self.database,
                        parent_id,
                        &title,
                        description.as_deref(),
                        &priority,
                    )?;
                    for label in labels {
                        writer.add_label(self.database, id, &label)?;
                    }
                    id
                } else {
                    self.database.transaction(|| {
                        let id = self.database.create_subissue(
                            parent_id,
                            &title,
                            description.as_deref(),
                            &priority,
                        )?;
                        for label in labels {
                            self.database.add_label(id, &label)?;
                        }
                        Ok(id)
                    })?
                };
                Ok(CommandResult::Id(id))
            }
            Command::ImportIssues { specs } => {
                let assigned = if let Some(writer) = &self.writer {
                    writer.import_issues(self.database, &specs)?
                } else {
                    self.database.transaction(|| {
                        let mut assigned = Vec::with_capacity(specs.len());
                        let mut uuid_to_id = std::collections::HashMap::new();
                        for spec in &specs {
                            let id = self.database.create_issue(
                                &spec.title,
                                spec.description.as_deref(),
                                &spec.priority,
                            )?;
                            for label in &spec.labels {
                                self.database.add_label(id, label)?;
                            }
                            for comment in &spec.comments {
                                self.database
                                    .add_comment(id, &comment.content, &comment.kind)?;
                            }
                            if spec.closed {
                                self.database.close_issue(id)?;
                            }
                            uuid_to_id.insert(spec.uuid, id);
                            assigned.push((spec.uuid, id));
                        }
                        for spec in &specs {
                            let issue_id = uuid_to_id[&spec.uuid];
                            if let Some(parent_uuid) = spec.parent_uuid {
                                if let Some(parent_id) = uuid_to_id.get(&parent_uuid) {
                                    self.database.update_parent(issue_id, Some(*parent_id))?;
                                }
                            }
                            for blocker_uuid in &spec.blockers {
                                if let Some(blocker_id) = uuid_to_id.get(blocker_uuid) {
                                    self.database.add_dependency(issue_id, *blocker_id)?;
                                }
                            }
                        }
                        Ok(assigned)
                    })?
                };
                Ok(CommandResult::Imported(assigned))
            }
            Command::UpdateIssue {
                id,
                title,
                description,
                status,
                priority,
                scheduled_at,
                due_at,
            } => {
                let update = Self::issue_update(
                    &title,
                    &description,
                    &status,
                    &priority,
                    scheduled_at,
                    due_at,
                );
                if let Some(writer) = &self.writer {
                    let metadata_changed = update.title.is_some()
                        || !matches!(update.description, DescriptionUpdate::Unchanged)
                        || update.priority.is_some()
                        || !matches!(update.scheduled_at, FieldUpdate::Unchanged)
                        || !matches!(update.due_at, FieldUpdate::Unchanged);
                    let current_status = self.database.get_issue(id)?.map(|issue| issue.status);
                    match (update.status, metadata_changed, current_status) {
                        (Some("archived"), false, Some(crate::models::IssueStatus::Closed))
                        | (Some("closed"), false, Some(crate::models::IssueStatus::Archived)) => {
                            writer.update_issue(self.database, id, update)?;
                        }
                        (Some("archived"), false, Some(status)) => {
                            bail!("can only archive a closed issue, found '{status}'");
                        }
                        (Some("archived"), false, None) => bail!("issue #{id} not found"),
                        (Some("closed"), false, _) => writer.close_issue(self.database, id)?,
                        (Some("open"), false, _) => writer.reopen_issue(self.database, id)?,
                        _ => writer.update_issue(self.database, id, update)?,
                    }
                } else {
                    if !matches!(scheduled_at, DateTimeChange::Unchanged)
                        || !matches!(due_at, DateTimeChange::Unchanged)
                    {
                        bail!("scheduling dates require shared Git authority");
                    }
                    let description = match update.description {
                        DescriptionUpdate::Unchanged => None,
                        DescriptionUpdate::Clear => Some(""),
                        DescriptionUpdate::Set(value) => Some(value),
                    };
                    if !self.database.update_issue(
                        id,
                        update.title,
                        description,
                        update.priority,
                    )? {
                        bail!("issue #{id} not found");
                    }
                    if matches!(update.description, DescriptionUpdate::Clear) {
                        self.database
                            .conn
                            .execute("UPDATE issues SET description = NULL WHERE id = ?1", [id])?;
                    }
                    if let Some(status) = update.status {
                        match status {
                            "open" => {
                                self.database.reopen_issue(id)?;
                            }
                            "closed" => {
                                let current = self.database.require_issue(id)?;
                                if current.status == crate::models::IssueStatus::Archived {
                                    self.database.unarchive_issue(id)?;
                                } else {
                                    self.database.close_issue(id)?;
                                }
                            }
                            "archived" => {
                                if !self.database.archive_issue(id)? {
                                    bail!("can only archive a closed issue");
                                }
                            }
                            other => bail!("unsupported issue status '{other}'"),
                        }
                    }
                }
                Ok(CommandResult::None)
            }
            Command::DeleteIssue { id } => {
                if let Some(writer) = &self.writer {
                    writer.delete_issue(self.database, id)?;
                } else if !self.database.delete_issue(id)? {
                    bail!("issue #{id} not found");
                }
                Ok(CommandResult::None)
            }
            Command::ArchiveIssue { id } => {
                let issue = self.database.require_issue(id)?;
                if issue.status != crate::models::IssueStatus::Closed {
                    bail!("can only archive a closed issue, found '{}'", issue.status);
                }
                if let Some(writer) = &self.writer {
                    writer.update_issue(
                        self.database,
                        id,
                        IssueUpdate {
                            status: Some("archived"),
                            ..IssueUpdate::default()
                        },
                    )?;
                } else if !self.database.archive_issue(id)? {
                    bail!("issue #{id} not found or not closed");
                }
                Ok(CommandResult::None)
            }
            Command::UnarchiveIssue { id } => {
                let issue = self.database.require_issue(id)?;
                if issue.status != crate::models::IssueStatus::Archived {
                    bail!("issue #{id} not found or not archived");
                }
                if let Some(writer) = &self.writer {
                    writer.update_issue(
                        self.database,
                        id,
                        IssueUpdate {
                            status: Some("closed"),
                            ..IssueUpdate::default()
                        },
                    )?;
                } else if !self.database.unarchive_issue(id)? {
                    bail!("issue #{id} not found or not archived");
                }
                Ok(CommandResult::None)
            }
            Command::AddComment {
                issue_id,
                content,
                kind,
            } => {
                let id = if let Some(writer) = &self.writer {
                    writer.add_comment(self.database, issue_id, &content, &kind)?
                } else {
                    self.database.add_comment(issue_id, &content, &kind)?
                };
                Ok(CommandResult::Id(id))
            }
            Command::AddIntervention {
                issue_id,
                content,
                trigger_type,
                context,
                driver_key_fingerprint,
            } => {
                let id = if let Some(writer) = &self.writer {
                    writer.add_intervention_comment(
                        self.database,
                        issue_id,
                        &content,
                        &trigger_type,
                        context.as_deref(),
                        driver_key_fingerprint.as_deref(),
                    )?
                } else {
                    self.database.add_intervention_comment(
                        issue_id,
                        &content,
                        &trigger_type,
                        context.as_deref(),
                        driver_key_fingerprint.as_deref(),
                    )?
                };
                Ok(CommandResult::Id(id))
            }
            Command::AddLabel { issue_id, label } => {
                Ok(CommandResult::Changed(if let Some(writer) = &self.writer {
                    writer.add_label(self.database, issue_id, &label)?
                } else {
                    self.database.add_label(issue_id, &label)?
                }))
            }
            Command::RemoveLabel { issue_id, label } => {
                Ok(CommandResult::Changed(if let Some(writer) = &self.writer {
                    writer.remove_label(self.database, issue_id, &label)?
                } else {
                    self.database.remove_label(issue_id, &label)?
                }))
            }
            Command::AddDependency {
                issue_id,
                blocker_id,
            } => Ok(CommandResult::Changed(if let Some(writer) = &self.writer {
                writer.add_blocker(self.database, issue_id, blocker_id)?
            } else {
                self.database.add_dependency(issue_id, blocker_id)?
            })),
            Command::RemoveDependency {
                issue_id,
                blocker_id,
            } => Ok(CommandResult::Changed(if let Some(writer) = &self.writer {
                writer.remove_blocker(self.database, issue_id, blocker_id)?
            } else {
                self.database.remove_dependency(issue_id, blocker_id)?
            })),
            Command::AddRelation {
                issue_id,
                related_id,
            } => Ok(CommandResult::Changed(if let Some(writer) = &self.writer {
                writer.add_relation(self.database, issue_id, related_id)?
            } else {
                self.database.add_relation(issue_id, related_id)?
            })),
            Command::RemoveRelation {
                issue_id,
                related_id,
            } => Ok(CommandResult::Changed(if let Some(writer) = &self.writer {
                writer.remove_relation(self.database, issue_id, related_id)?
            } else {
                self.database.remove_relation(issue_id, related_id)?
            })),
            Command::CreateMilestone { name, description } => {
                let id = if let Some(writer) = &self.writer {
                    writer.create_milestone(self.database, &name, description.as_deref())?
                } else {
                    self.database
                        .create_milestone(&name, description.as_deref())?
                };
                Ok(CommandResult::Id(id))
            }
            Command::AssignMilestone {
                milestone_id,
                issue_ids,
            } => {
                if let Some(writer) = &self.writer {
                    writer.set_milestone_on_issues(self.database, milestone_id, &issue_ids)?;
                } else {
                    for issue_id in issue_ids {
                        self.database
                            .add_issue_to_milestone(milestone_id, issue_id)?;
                    }
                }
                Ok(CommandResult::None)
            }
            Command::ClearMilestone {
                milestone_id,
                issue_id,
            } => {
                let changed = if let Some(writer) = &self.writer {
                    writer.clear_milestone_on_issue(self.database, issue_id)?;
                    true
                } else {
                    self.database
                        .remove_issue_from_milestone(milestone_id, issue_id)?
                };
                Ok(CommandResult::Changed(changed))
            }
            Command::CloseMilestone { id } => {
                if let Some(writer) = &self.writer {
                    writer.close_milestone(self.database, id)?;
                } else if !self.database.close_milestone(id)? {
                    bail!("milestone #{id} not found");
                }
                Ok(CommandResult::None)
            }
            Command::DeleteMilestone { id } => {
                if let Some(writer) = &self.writer {
                    writer.delete_milestone(self.database, id)?;
                } else if !self.database.delete_milestone(id)? {
                    bail!("milestone #{id} not found");
                }
                Ok(CommandResult::None)
            }
            Command::ClaimLock { issue_id, branch } => {
                let writer = self.writer.as_ref().ok_or_else(|| {
                    anyhow::anyhow!("locks require configured shared Git authority")
                })?;
                Ok(CommandResult::Lock(
                    writer.claim_lock_v2(issue_id, branch.as_deref())?,
                ))
            }
            Command::ReleaseLock { issue_id } => {
                Ok(CommandResult::Changed(if let Some(writer) = &self.writer {
                    writer.release_lock_v2(issue_id)?
                } else {
                    false
                }))
            }
            Command::StealLock {
                issue_id,
                stale_agent_id,
                branch,
            } => {
                let writer = self.writer.as_ref().ok_or_else(|| {
                    anyhow::anyhow!("locks require configured shared Git authority")
                })?;
                Ok(CommandResult::Lock(writer.steal_lock_v2(
                    issue_id,
                    &stale_agent_id,
                    branch.as_deref(),
                )?))
            }
            Command::ForceReleaseLock {
                issue_id,
                stale_agent_id,
            } => {
                let writer = self.writer.as_ref().ok_or_else(|| {
                    anyhow::anyhow!("locks require configured shared Git authority")
                })?;
                Ok(CommandResult::Changed(
                    writer.force_release_lock_v2(issue_id, &stale_agent_id)?,
                ))
            }
            Command::SetSessionIssue {
                session_id,
                issue_id,
            } => Ok(CommandResult::Changed(
                self.database.set_session_issue(session_id, issue_id)?,
            )),
            Command::ClearSessionIssue { session_id } => Ok(CommandResult::Changed(
                self.database.clear_session_issue(session_id)?,
            )),
            Command::WriteAgentRequest {
                target_agent_id,
                request,
            } => {
                let writer = self.writer.as_ref().ok_or_else(|| {
                    anyhow::anyhow!("agent requests require configured shared Git authority")
                })?;
                Ok(CommandResult::Push(
                    writer.write_agent_request(&target_agent_id, &request)?,
                ))
            }
            Command::WriteAgentAck {
                target_agent_id,
                ack,
            } => {
                let writer = self.writer.as_ref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "agent acknowledgements require configured shared Git authority"
                    )
                })?;
                Ok(CommandResult::Push(
                    writer.write_agent_ack(&target_agent_id, &ack)?,
                ))
            }
        }
    }
}

impl QueryService for RepositoryService<'_> {
    fn list_issue_records(&self) -> Result<Vec<crate::issue_file::IssueFile>> {
        build_issue_records(self)
    }

    fn get_issue(&self, id: i64) -> Result<Option<Issue>> {
        self.database.get_issue(id)
    }

    fn require_issue(&self, id: i64) -> Result<Issue> {
        self.database.require_issue(id)
    }

    fn list_issues(
        &self,
        status: Option<&str>,
        label: Option<&str>,
        priority: Option<&str>,
    ) -> Result<Vec<Issue>> {
        self.database.list_issues(status, label, priority)
    }

    fn search_issues(&self, query: &str) -> Result<Vec<Issue>> {
        self.database.search_issues(query)
    }

    fn get_subissues(&self, parent_id: i64) -> Result<Vec<Issue>> {
        self.database.get_subissues(parent_id)
    }

    fn get_labels(&self, issue_id: i64) -> Result<Vec<String>> {
        self.database.get_labels(issue_id)
    }

    fn get_labels_batch(
        &self,
        issue_ids: &[i64],
    ) -> Result<std::collections::HashMap<i64, Vec<String>>> {
        self.database.get_labels_batch(issue_ids)
    }

    fn get_comments(&self, issue_id: i64) -> Result<Vec<Comment>> {
        self.database.get_comments(issue_id)
    }

    fn search_comments(&self, query: &str) -> Result<Vec<(Comment, i64, String)>> {
        self.database.search_comments(query)
    }

    fn get_blockers(&self, issue_id: i64) -> Result<Vec<i64>> {
        self.database.get_blockers(issue_id)
    }

    fn get_blocking(&self, issue_id: i64) -> Result<Vec<i64>> {
        self.database.get_blocking(issue_id)
    }

    fn get_blocker_counts_batch(
        &self,
        issue_ids: &[i64],
    ) -> Result<std::collections::HashMap<i64, usize>> {
        self.database.get_blocker_counts_batch(issue_ids)
    }

    fn list_blocked_issues(&self) -> Result<Vec<Issue>> {
        self.database.list_blocked_issues()
    }

    fn list_ready_issues(&self) -> Result<Vec<Issue>> {
        self.database.list_ready_issues()
    }

    fn list_archived_issues(&self) -> Result<Vec<Issue>> {
        self.database.list_archived_issues()
    }

    fn get_related_issues(&self, issue_id: i64) -> Result<Vec<Issue>> {
        self.database.get_related_issues(issue_id)
    }

    fn get_related_issue_ids(&self, issue_id: i64) -> Result<Vec<i64>> {
        self.database.get_related_issue_ids(issue_id)
    }

    fn get_milestone(&self, id: i64) -> Result<Option<Milestone>> {
        self.database.get_milestone(id)
    }

    fn list_milestones(&self, status: Option<&str>) -> Result<Vec<Milestone>> {
        self.database.list_milestones(status)
    }

    fn get_milestone_issues(&self, milestone_id: i64) -> Result<Vec<Issue>> {
        self.database.get_milestone_issues(milestone_id)
    }

    fn get_issue_milestone(&self, issue_id: i64) -> Result<Option<Milestone>> {
        self.database.get_issue_milestone(issue_id)
    }

    fn get_milestone_uuid_for_issue(&self, issue_id: i64) -> Result<Option<String>> {
        self.database.get_milestone_uuid_for_issue(issue_id)
    }

    fn count_issues_since(&self, since: &str) -> Result<i64> {
        self.database.count_issues_since(since)
    }

    fn count_comments_since(&self, since: &str) -> Result<i64> {
        self.database.count_comments_since(since)
    }

    fn get_issue_count(&self) -> Result<i64> {
        self.database.get_issue_count()
    }

    fn get_milestone_count(&self) -> Result<i64> {
        self.database.get_milestone_count()
    }

    fn get_issue_uuid_by_id(&self, id: i64) -> Result<String> {
        self.database.get_issue_uuid_by_id(id)
    }

    fn get_issue_export_metadata(&self, id: i64) -> Result<(Option<String>, Option<String>)> {
        self.database.get_issue_export_metadata(id)
    }

    fn get_comments_with_author(&self, issue_id: i64) -> Result<Vec<crate::db::CommentAuthorRow>> {
        self.database.get_comments_with_author(issue_id)
    }

    fn get_time_entries_for_issue(&self, issue_id: i64) -> Result<Vec<crate::db::TimeEntryRow>> {
        self.database.get_time_entries_for_issue(issue_id)
    }

    fn authority_mode(&self) -> AuthorityMode {
        if self.writer.is_some() {
            AuthorityMode::Shared
        } else {
            AuthorityMode::Local
        }
    }
}

impl LocalStateService for RepositoryService<'_> {
    fn get_schema_version(&self) -> Result<i32> {
        self.database.get_schema_version()
    }

    fn start_session(&self, agent_id: Option<&str>) -> Result<i64> {
        let _operation = self.acquire_operation()?;
        self.database.start_session_with_agent(agent_id)
    }

    fn end_session(&self, id: i64, notes: Option<&str>) -> Result<bool> {
        let _operation = self.acquire_operation()?;
        self.database.end_session(id, notes)
    }

    fn set_session_action(&self, id: i64, action: &str) -> Result<bool> {
        let _operation = self.acquire_operation()?;
        self.database.set_session_action(id, action)
    }

    fn start_timer(&self, issue_id: i64) -> Result<i64> {
        let _operation = self.acquire_operation()?;
        self.database.start_timer(issue_id)
    }

    fn stop_timer(&self, issue_id: i64) -> Result<bool> {
        let _operation = self.acquire_operation()?;
        self.database.stop_timer(issue_id)
    }

    fn get_active_timer(&self) -> Result<Option<(i64, DateTime<Utc>)>> {
        self.database.get_active_timer()
    }

    fn get_total_time(&self, issue_id: i64) -> Result<i64> {
        self.database.get_total_time(issue_id)
    }

    fn get_current_session_for_agent(&self, agent_id: Option<&str>) -> Result<Option<Session>> {
        self.database.get_current_session_for_agent(agent_id)
    }

    fn get_last_session_for_agent(&self, agent_id: Option<&str>) -> Result<Option<Session>> {
        self.database.get_last_session_for_agent(agent_id)
    }

    fn record_token_usage(&self, usage: &LocalTokenUsage) -> Result<i64> {
        let _operation = self.acquire_operation()?;
        self.database.create_token_usage_for_provider(
            &usage.agent_id,
            usage.session_id,
            &usage.provider,
            usage.input_tokens,
            usage.output_tokens,
            usage.cached_input_tokens,
            usage.reasoning_output_tokens,
            usage.cache_read_tokens,
            usage.cache_creation_tokens,
            &usage.model,
            usage.cost_estimate,
            usage.provider_metadata_json.as_deref(),
        )
    }

    fn get_token_usage(&self, id: i64) -> Result<Option<TokenUsage>> {
        self.database.get_token_usage(id)
    }

    fn list_token_usage(
        &self,
        agent_id: Option<&str>,
        session_id: Option<i64>,
        model: Option<&str>,
        from: Option<&str>,
        to: Option<&str>,
        limit: Option<i64>,
    ) -> Result<Vec<TokenUsage>> {
        self.database
            .list_token_usage(agent_id, session_id, model, from, to, limit)
    }

    fn get_usage_summary(
        &self,
        agent_id: Option<&str>,
        from: Option<&str>,
        to: Option<&str>,
    ) -> Result<Vec<UsageSummaryRow>> {
        self.database.get_usage_summary(agent_id, from, to)
    }

    fn insert_sentinel_run(&self, run_id: &str, mode: &str) -> Result<i64> {
        let _operation = self.acquire_operation()?;
        self.database.insert_sentinel_run(run_id, mode)
    }

    fn complete_sentinel_run(&self, run_id: &str, counters: &RunCounters) -> Result<()> {
        let _operation = self.acquire_operation()?;
        self.database.complete_sentinel_run(run_id, counters)
    }

    fn list_sentinel_runs(&self, limit: usize) -> Result<Vec<SentinelRun>> {
        self.database.list_sentinel_runs(limit)
    }

    fn insert_sentinel_dispatch(&self, dispatch: &NewDispatch<'_>) -> Result<i64> {
        let _operation = self.acquire_operation()?;
        self.database.insert_sentinel_dispatch(dispatch)
    }

    fn update_dispatch_outcome(
        &self,
        dispatch_id: i64,
        outcome: &str,
        outcome_detail: &str,
    ) -> Result<()> {
        let _operation = self.acquire_operation()?;
        self.database
            .update_dispatch_outcome(dispatch_id, outcome, outcome_detail)
    }

    fn get_pending_dispatches(&self) -> Result<Vec<SentinelDispatch>> {
        self.database.get_pending_dispatches()
    }

    fn count_pending_dispatches(&self) -> Result<i64> {
        self.database.count_pending_dispatches()
    }

    fn get_latest_dispatch_for_signal(
        &self,
        issue_number: i64,
        label: &str,
    ) -> Result<Option<SentinelDispatch>> {
        self.database
            .get_latest_dispatch_for_signal(issue_number, label)
    }

    fn load_dispatch_seen_set(&self) -> Result<Vec<SentinelDispatch>> {
        self.database.load_dispatch_seen_set()
    }

    fn list_dispatches_for_run(&self, run_id: &str) -> Result<Vec<SentinelDispatch>> {
        self.database.list_dispatches_for_run(run_id)
    }

    fn get_dispatch_metrics(&self) -> Result<Vec<DispatchMetric>> {
        self.database.get_dispatch_metrics()
    }

    fn get_repeat_failure_counts(&self) -> Result<Vec<(String, i64)>> {
        self.database.get_repeat_failure_counts()
    }

    fn get_escalation_heavy_counts(&self) -> Result<Vec<(String, i64, i64, i64)>> {
        self.database.get_escalation_heavy_counts()
    }
}

fn build_issue_records(
    queries: &(impl QueryService + ?Sized),
) -> Result<Vec<crate::issue_file::IssueFile>> {
    let issues = queries.list_issues(Some("all"), None, None)?;
    let mut uuid_map = std::collections::HashMap::new();
    for issue in &issues {
        let (uuid, _) = queries.get_issue_export_metadata(issue.id)?;
        let uuid = uuid
            .and_then(|value| value.parse().ok())
            .unwrap_or_else(uuid::Uuid::new_v4);
        uuid_map.insert(issue.id, uuid);
    }
    let mut records = Vec::with_capacity(issues.len());
    for issue in issues {
        let (uuid, created_by) = queries.get_issue_export_metadata(issue.id)?;
        let uuid = uuid
            .and_then(|value| value.parse().ok())
            .or_else(|| uuid_map.get(&issue.id).copied())
            .unwrap_or_else(uuid::Uuid::new_v4);
        let parent_uuid = issue
            .parent_id
            .map(|parent_id| resolve_issue_record_uuid(queries, &uuid_map, parent_id));
        let blockers = queries
            .get_blockers(issue.id)?
            .into_iter()
            .map(|id| resolve_issue_record_uuid(queries, &uuid_map, id))
            .collect();
        let related = queries
            .get_related_issue_ids(issue.id)?
            .into_iter()
            .map(|id| resolve_issue_record_uuid(queries, &uuid_map, id))
            .collect();
        let milestone_uuid = queries
            .get_milestone_uuid_for_issue(issue.id)?
            .and_then(|uuid| uuid.parse().ok());
        let comments = queries
            .get_comments_with_author(issue.id)?
            .into_iter()
            .map(
                |(
                    id,
                    author,
                    content,
                    created_at,
                    kind,
                    trigger_type,
                    intervention_context,
                    driver_key_fingerprint,
                )| {
                    crate::issue_file::CommentEntry {
                        id,
                        author: author.unwrap_or_else(|| "unknown".to_string()),
                        content,
                        created_at,
                        kind,
                        trigger_type,
                        intervention_context,
                        driver_key_fingerprint,
                        signed_by: None,
                        signature: None,
                    }
                },
            )
            .collect();
        let time_entries = queries
            .get_time_entries_for_issue(issue.id)?
            .into_iter()
            .map(
                |(id, started_at, ended_at, duration_seconds)| crate::issue_file::TimeEntry {
                    id,
                    started_at,
                    ended_at,
                    duration_seconds,
                },
            )
            .collect();
        records.push(crate::issue_file::IssueFile {
            uuid,
            display_id: Some(issue.id),
            title: issue.title,
            description: issue.description,
            status: issue.status,
            priority: issue.priority,
            parent_uuid,
            created_by: created_by.unwrap_or_else(|| "unknown".to_string()),
            created_at: issue.created_at,
            updated_at: issue.updated_at,
            closed_at: issue.closed_at,
            scheduled_at: issue.scheduled_at,
            due_at: issue.due_at,
            labels: queries.get_labels(issue.id)?,
            comments,
            blockers,
            related,
            milestone_uuid,
            time_entries,
        });
    }
    Ok(records)
}

fn resolve_issue_record_uuid(
    queries: &(impl QueryService + ?Sized),
    uuid_map: &std::collections::HashMap<i64, uuid::Uuid>,
    id: i64,
) -> uuid::Uuid {
    uuid_map.get(&id).copied().unwrap_or_else(|| {
        queries
            .get_issue_uuid_by_id(id)
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or_else(uuid::Uuid::new_v4)
    })
}

const fn map_datetime_change(change: DateTimeChange) -> FieldUpdate<DateTime<Utc>> {
    match change {
        DateTimeChange::Unchanged => FieldUpdate::Unchanged,
        DateTimeChange::Clear => FieldUpdate::Clear,
        DateTimeChange::Set(value) => FieldUpdate::Set(value),
    }
}

fn expect_none(result: CommandResult) -> Result<()> {
    match result {
        CommandResult::None => Ok(()),
        other => bail!("command returned unexpected result: {other:?}"),
    }
}

fn expect_id(result: CommandResult) -> Result<i64> {
    match result {
        CommandResult::Id(id) => Ok(id),
        other => bail!("command returned unexpected result: {other:?}"),
    }
}

fn expect_changed(result: CommandResult) -> Result<bool> {
    match result {
        CommandResult::Changed(changed) => Ok(changed),
        other => bail!("command returned unexpected result: {other:?}"),
    }
}

fn expect_lock(result: CommandResult) -> Result<LockClaimResult> {
    match result {
        CommandResult::Lock(result) => Ok(result),
        other => bail!("command returned unexpected result: {other:?}"),
    }
}

fn expect_push(result: CommandResult) -> Result<PushOutcome> {
    match result {
        CommandResult::Push(result) => Ok(result),
        other => bail!("command returned unexpected result: {other:?}"),
    }
}

#[cfg(test)]
impl CommandService for Database {
    fn execute(&self, command: Command) -> Result<CommandResult> {
        RepositoryService {
            database: self,
            writer: None,
            allow_domain_commands: true,
            operation_dir: None,
        }
        .execute(command)
    }
}

impl QueryService for Database {
    fn list_issue_records(&self) -> Result<Vec<crate::issue_file::IssueFile>> {
        build_issue_records(self)
    }

    fn get_issue(&self, id: i64) -> Result<Option<Issue>> {
        Database::get_issue(self, id)
    }

    fn require_issue(&self, id: i64) -> Result<Issue> {
        Database::require_issue(self, id)
    }

    fn list_issues(
        &self,
        status: Option<&str>,
        label: Option<&str>,
        priority: Option<&str>,
    ) -> Result<Vec<Issue>> {
        Database::list_issues(self, status, label, priority)
    }

    fn search_issues(&self, query: &str) -> Result<Vec<Issue>> {
        Database::search_issues(self, query)
    }

    fn get_subissues(&self, parent_id: i64) -> Result<Vec<Issue>> {
        Database::get_subissues(self, parent_id)
    }

    fn get_labels(&self, issue_id: i64) -> Result<Vec<String>> {
        Database::get_labels(self, issue_id)
    }

    fn get_labels_batch(
        &self,
        issue_ids: &[i64],
    ) -> Result<std::collections::HashMap<i64, Vec<String>>> {
        Database::get_labels_batch(self, issue_ids)
    }

    fn get_comments(&self, issue_id: i64) -> Result<Vec<Comment>> {
        Database::get_comments(self, issue_id)
    }

    fn search_comments(&self, query: &str) -> Result<Vec<(Comment, i64, String)>> {
        Database::search_comments(self, query)
    }

    fn get_blockers(&self, issue_id: i64) -> Result<Vec<i64>> {
        Database::get_blockers(self, issue_id)
    }

    fn get_blocking(&self, issue_id: i64) -> Result<Vec<i64>> {
        Database::get_blocking(self, issue_id)
    }

    fn get_blocker_counts_batch(
        &self,
        issue_ids: &[i64],
    ) -> Result<std::collections::HashMap<i64, usize>> {
        Database::get_blocker_counts_batch(self, issue_ids)
    }

    fn list_blocked_issues(&self) -> Result<Vec<Issue>> {
        Database::list_blocked_issues(self)
    }

    fn list_ready_issues(&self) -> Result<Vec<Issue>> {
        Database::list_ready_issues(self)
    }

    fn list_archived_issues(&self) -> Result<Vec<Issue>> {
        Database::list_archived_issues(self)
    }

    fn get_related_issues(&self, issue_id: i64) -> Result<Vec<Issue>> {
        Database::get_related_issues(self, issue_id)
    }

    fn get_related_issue_ids(&self, issue_id: i64) -> Result<Vec<i64>> {
        Database::get_related_issue_ids(self, issue_id)
    }

    fn get_milestone(&self, id: i64) -> Result<Option<Milestone>> {
        Database::get_milestone(self, id)
    }

    fn list_milestones(&self, status: Option<&str>) -> Result<Vec<Milestone>> {
        Database::list_milestones(self, status)
    }

    fn get_milestone_issues(&self, milestone_id: i64) -> Result<Vec<Issue>> {
        Database::get_milestone_issues(self, milestone_id)
    }

    fn get_issue_milestone(&self, issue_id: i64) -> Result<Option<Milestone>> {
        Database::get_issue_milestone(self, issue_id)
    }

    fn get_milestone_uuid_for_issue(&self, issue_id: i64) -> Result<Option<String>> {
        Database::get_milestone_uuid_for_issue(self, issue_id)
    }

    fn count_issues_since(&self, since: &str) -> Result<i64> {
        Database::count_issues_since(self, since)
    }

    fn count_comments_since(&self, since: &str) -> Result<i64> {
        Database::count_comments_since(self, since)
    }

    fn get_issue_count(&self) -> Result<i64> {
        Database::get_issue_count(self)
    }

    fn get_milestone_count(&self) -> Result<i64> {
        Database::get_milestone_count(self)
    }

    fn get_issue_uuid_by_id(&self, id: i64) -> Result<String> {
        Database::get_issue_uuid_by_id(self, id)
    }

    fn get_issue_export_metadata(&self, id: i64) -> Result<(Option<String>, Option<String>)> {
        Database::get_issue_export_metadata(self, id)
    }

    fn get_comments_with_author(&self, issue_id: i64) -> Result<Vec<crate::db::CommentAuthorRow>> {
        Database::get_comments_with_author(self, issue_id)
    }

    fn get_time_entries_for_issue(&self, issue_id: i64) -> Result<Vec<crate::db::TimeEntryRow>> {
        Database::get_time_entries_for_issue(self, issue_id)
    }

    fn authority_mode(&self) -> AuthorityMode {
        AuthorityMode::Local
    }
}

#[cfg(test)]
impl LocalStateService for Database {
    fn get_schema_version(&self) -> Result<i32> {
        Database::get_schema_version(self)
    }

    fn start_session(&self, agent_id: Option<&str>) -> Result<i64> {
        Database::start_session_with_agent(self, agent_id)
    }

    fn end_session(&self, id: i64, notes: Option<&str>) -> Result<bool> {
        Database::end_session(self, id, notes)
    }

    fn set_session_action(&self, id: i64, action: &str) -> Result<bool> {
        Database::set_session_action(self, id, action)
    }

    fn start_timer(&self, issue_id: i64) -> Result<i64> {
        Database::start_timer(self, issue_id)
    }

    fn stop_timer(&self, issue_id: i64) -> Result<bool> {
        Database::stop_timer(self, issue_id)
    }

    fn get_active_timer(&self) -> Result<Option<(i64, DateTime<Utc>)>> {
        Database::get_active_timer(self)
    }

    fn get_total_time(&self, issue_id: i64) -> Result<i64> {
        Database::get_total_time(self, issue_id)
    }

    fn get_current_session_for_agent(&self, agent_id: Option<&str>) -> Result<Option<Session>> {
        Database::get_current_session_for_agent(self, agent_id)
    }

    fn get_last_session_for_agent(&self, agent_id: Option<&str>) -> Result<Option<Session>> {
        Database::get_last_session_for_agent(self, agent_id)
    }

    fn record_token_usage(&self, usage: &LocalTokenUsage) -> Result<i64> {
        Database::create_token_usage_for_provider(
            self,
            &usage.agent_id,
            usage.session_id,
            &usage.provider,
            usage.input_tokens,
            usage.output_tokens,
            usage.cached_input_tokens,
            usage.reasoning_output_tokens,
            usage.cache_read_tokens,
            usage.cache_creation_tokens,
            &usage.model,
            usage.cost_estimate,
            usage.provider_metadata_json.as_deref(),
        )
    }

    fn get_token_usage(&self, id: i64) -> Result<Option<TokenUsage>> {
        Database::get_token_usage(self, id)
    }

    fn list_token_usage(
        &self,
        agent_id: Option<&str>,
        session_id: Option<i64>,
        model: Option<&str>,
        from: Option<&str>,
        to: Option<&str>,
        limit: Option<i64>,
    ) -> Result<Vec<TokenUsage>> {
        Database::list_token_usage(self, agent_id, session_id, model, from, to, limit)
    }

    fn get_usage_summary(
        &self,
        agent_id: Option<&str>,
        from: Option<&str>,
        to: Option<&str>,
    ) -> Result<Vec<UsageSummaryRow>> {
        Database::get_usage_summary(self, agent_id, from, to)
    }

    fn insert_sentinel_run(&self, run_id: &str, mode: &str) -> Result<i64> {
        Database::insert_sentinel_run(self, run_id, mode)
    }

    fn complete_sentinel_run(&self, run_id: &str, counters: &RunCounters) -> Result<()> {
        Database::complete_sentinel_run(self, run_id, counters)
    }

    fn list_sentinel_runs(&self, limit: usize) -> Result<Vec<SentinelRun>> {
        Database::list_sentinel_runs(self, limit)
    }

    fn insert_sentinel_dispatch(&self, dispatch: &NewDispatch<'_>) -> Result<i64> {
        Database::insert_sentinel_dispatch(self, dispatch)
    }

    fn update_dispatch_outcome(
        &self,
        dispatch_id: i64,
        outcome: &str,
        outcome_detail: &str,
    ) -> Result<()> {
        Database::update_dispatch_outcome(self, dispatch_id, outcome, outcome_detail)
    }

    fn get_pending_dispatches(&self) -> Result<Vec<SentinelDispatch>> {
        Database::get_pending_dispatches(self)
    }

    fn count_pending_dispatches(&self) -> Result<i64> {
        Database::count_pending_dispatches(self)
    }

    fn get_latest_dispatch_for_signal(
        &self,
        issue_number: i64,
        label: &str,
    ) -> Result<Option<SentinelDispatch>> {
        Database::get_latest_dispatch_for_signal(self, issue_number, label)
    }

    fn load_dispatch_seen_set(&self) -> Result<Vec<SentinelDispatch>> {
        Database::load_dispatch_seen_set(self)
    }

    fn list_dispatches_for_run(&self, run_id: &str) -> Result<Vec<SentinelDispatch>> {
        Database::list_dispatches_for_run(self, run_id)
    }

    fn get_dispatch_metrics(&self) -> Result<Vec<DispatchMetric>> {
        Database::get_dispatch_metrics(self)
    }

    fn get_repeat_failure_counts(&self) -> Result<Vec<(String, i64)>> {
        Database::get_repeat_failure_counts(self)
    }

    fn get_escalation_heavy_counts(&self) -> Result<Vec<(String, i64, i64, i64)>> {
        Database::get_escalation_heavy_counts(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn populated_database() -> (Database, tempfile::TempDir, i64, i64, i64) {
        let directory = tempfile::tempdir().unwrap();
        let database = Database::open(&directory.path().join("issues.db")).unwrap();
        let parent = database
            .create_issue("parent boundary", Some("query marker"), "high")
            .unwrap();
        let child = database
            .create_subissue(parent, "child boundary", None, "medium")
            .unwrap();
        let archived = database
            .create_issue("archived boundary", None, "low")
            .unwrap();
        database.add_label(parent, "boundary").unwrap();
        database
            .add_comment(parent, "boundary comment", "note")
            .unwrap();
        database.add_dependency(child, parent).unwrap();
        database.add_relation(parent, archived).unwrap();
        let milestone = database
            .create_milestone("boundary milestone", Some("parity"))
            .unwrap();
        database.add_issue_to_milestone(milestone, parent).unwrap();
        database.close_issue(archived).unwrap();
        database.archive_issue(archived).unwrap();
        database.start_timer(parent).unwrap();
        database.stop_timer(parent).unwrap();
        (database, directory, parent, child, milestone)
    }

    #[test]
    fn query_service_matches_database_projection() {
        let (database, _directory, parent, child, milestone) = populated_database();
        let service = RepositoryService::projection(&database);
        let ids = [parent, child];
        let since = "1970-01-01T00:00:00Z";

        assert_eq!(
            service.list_issue_records().unwrap(),
            QueryService::list_issue_records(&database).unwrap()
        );
        assert_eq!(
            service.get_issue(parent).unwrap(),
            database.get_issue(parent).unwrap()
        );
        assert_eq!(
            service.require_issue(parent).unwrap(),
            database.require_issue(parent).unwrap()
        );
        assert_eq!(
            service.list_issues(Some("all"), None, None).unwrap(),
            database.list_issues(Some("all"), None, None).unwrap()
        );
        assert_eq!(
            service.search_issues("marker").unwrap(),
            database.search_issues("marker").unwrap()
        );
        assert_eq!(
            service.get_subissues(parent).unwrap(),
            database.get_subissues(parent).unwrap()
        );
        assert_eq!(
            service.get_labels(parent).unwrap(),
            database.get_labels(parent).unwrap()
        );
        assert_eq!(
            service.get_labels_batch(&ids).unwrap(),
            database.get_labels_batch(&ids).unwrap()
        );
        assert_eq!(
            service.get_comments(parent).unwrap(),
            database.get_comments(parent).unwrap()
        );
        assert_eq!(
            service.search_comments("boundary").unwrap(),
            database.search_comments("boundary").unwrap()
        );
        assert_eq!(
            service.get_blockers(child).unwrap(),
            database.get_blockers(child).unwrap()
        );
        assert_eq!(
            service.get_blocking(parent).unwrap(),
            database.get_blocking(parent).unwrap()
        );
        assert_eq!(
            service.get_blocker_counts_batch(&ids).unwrap(),
            database.get_blocker_counts_batch(&ids).unwrap()
        );
        assert_eq!(
            service.list_blocked_issues().unwrap(),
            database.list_blocked_issues().unwrap()
        );
        assert_eq!(
            service.list_ready_issues().unwrap(),
            database.list_ready_issues().unwrap()
        );
        assert_eq!(
            service.list_archived_issues().unwrap(),
            database.list_archived_issues().unwrap()
        );
        assert_eq!(
            service.get_related_issues(parent).unwrap(),
            database.get_related_issues(parent).unwrap()
        );
        assert_eq!(
            service.get_related_issue_ids(parent).unwrap(),
            database.get_related_issue_ids(parent).unwrap()
        );
        assert_eq!(
            service.get_milestone(milestone).unwrap(),
            database.get_milestone(milestone).unwrap()
        );
        assert_eq!(
            service.list_milestones(None).unwrap(),
            database.list_milestones(None).unwrap()
        );
        assert_eq!(
            service.get_milestone_issues(milestone).unwrap(),
            database.get_milestone_issues(milestone).unwrap()
        );
        assert_eq!(
            service.get_issue_milestone(parent).unwrap(),
            database.get_issue_milestone(parent).unwrap()
        );
        assert_eq!(
            service.get_milestone_uuid_for_issue(parent).unwrap(),
            database.get_milestone_uuid_for_issue(parent).unwrap()
        );
        assert_eq!(
            service.count_issues_since(since).unwrap(),
            database.count_issues_since(since).unwrap()
        );
        assert_eq!(
            service.count_comments_since(since).unwrap(),
            database.count_comments_since(since).unwrap()
        );
        assert_eq!(
            service.get_issue_count().unwrap(),
            database.get_issue_count().unwrap()
        );
        assert_eq!(
            service.get_milestone_count().unwrap(),
            database.get_milestone_count().unwrap()
        );
        assert_eq!(
            service.get_issue_uuid_by_id(parent).unwrap(),
            database.get_issue_uuid_by_id(parent).unwrap()
        );
        assert_eq!(
            service.get_issue_export_metadata(parent).unwrap(),
            database.get_issue_export_metadata(parent).unwrap()
        );
        assert_eq!(
            service.get_comments_with_author(parent).unwrap(),
            database.get_comments_with_author(parent).unwrap()
        );
        assert_eq!(
            service.get_time_entries_for_issue(parent).unwrap(),
            database.get_time_entries_for_issue(parent).unwrap()
        );
    }

    #[test]
    fn projection_only_service_rejects_domain_mutation_but_allows_local_state() {
        let (database, _directory, parent, _child, _milestone) = populated_database();
        let before = database.get_issue_count().unwrap();
        let service = RepositoryService::projection(&database);

        let error = service
            .create_issue("must fail", None, "medium", None, None)
            .unwrap_err();
        assert!(error.to_string().contains("projection-only"));
        assert_eq!(database.get_issue_count().unwrap(), before);

        let session = service.start_session(Some("boundary-agent")).unwrap();
        assert!(service.set_session_issue(session, parent).unwrap());
        assert_eq!(
            service
                .get_current_session_for_agent(Some("boundary-agent"))
                .unwrap()
                .unwrap()
                .active_issue_id,
            Some(parent)
        );
        service.start_timer(parent).unwrap();
        assert_eq!(service.get_active_timer().unwrap().unwrap().0, parent);
        service.stop_timer(parent).unwrap();
        let usage_id = service
            .record_token_usage(&LocalTokenUsage {
                agent_id: "boundary-agent".to_string(),
                session_id: Some(session),
                provider: "codex".to_string(),
                input_tokens: 10,
                output_tokens: 4,
                cached_input_tokens: Some(2),
                reasoning_output_tokens: Some(1),
                cache_read_tokens: None,
                cache_creation_tokens: None,
                model: "test".to_string(),
                cost_estimate: None,
                provider_metadata_json: None,
            })
            .unwrap();
        assert!(service.get_token_usage(usage_id).unwrap().is_some());
        service.insert_sentinel_run("boundary-run", "test").unwrap();
        assert_eq!(
            service.list_sentinel_runs(1).unwrap()[0].run_id,
            "boundary-run"
        );
    }

    #[test]
    fn configured_shared_mode_cannot_fall_back_when_writer_is_missing() {
        let directory = tempfile::tempdir().unwrap();
        let crosslink_dir = directory.path().join(".crosslink");
        std::fs::create_dir(&crosslink_dir).unwrap();
        let database = Database::open(&crosslink_dir.join("issues.db")).unwrap();
        let agent = crate::identity::AgentConfig {
            agent_id: "boundary-agent".to_string(),
            machine_id: "boundary-machine".to_string(),
            description: None,
            role: crate::identity::AgentRole::Driver,
            ssh_key_path: None,
            ssh_fingerprint: None,
            ssh_public_key: None,
        };
        std::fs::write(
            crosslink_dir.join("agent.json"),
            serde_json::to_vec(&agent).unwrap(),
        )
        .unwrap();
        let before = database.get_issue_count().unwrap();

        let Err(error) = RepositoryService::new(&database, &crosslink_dir) else {
            panic!("configured shared mode unexpectedly resolved as local")
        };

        assert!(error.to_string().contains("shared Git authority"));
        assert_eq!(database.get_issue_count().unwrap(), before);
    }

    fn source_files(root: &Path, output: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(root).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                source_files(&path, output);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                output.push(path);
            }
        }
    }

    #[test]
    fn production_source_cannot_bypass_application_mutation_boundary() {
        let source_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        source_files(&source_root, &mut files);
        let database_methods = [
            "create_issue",
            "create_subissue",
            "update_issue",
            "delete_issue",
            "close_issue",
            "reopen_issue",
            "archive_issue",
            "unarchive_issue",
            "add_comment",
            "add_intervention_comment",
            "add_label",
            "remove_label",
            "add_dependency",
            "remove_dependency",
            "add_relation",
            "remove_relation",
            "update_parent",
            "create_milestone",
            "add_issue_to_milestone",
            "remove_issue_from_milestone",
            "close_milestone",
            "delete_milestone",
            "set_session_issue",
            "clear_session_issue",
            "start_session_with_agent",
            "end_session",
            "set_session_action",
            "start_timer",
            "stop_timer",
            "create_token_usage_for_provider",
            "insert_sentinel_run",
            "complete_sentinel_run",
            "insert_sentinel_dispatch",
            "update_dispatch_outcome",
            "clear_shared_data",
            "add_blocker",
            "remove_blocker",
            "set_milestone_on_issues",
            "clear_milestone_on_issue",
            "claim_lock_v2",
            "release_lock_v2",
            "steal_lock_v2",
            "force_release_lock_v2",
            "write_agent_request",
            "write_agent_ack",
        ];
        let method_calls = regex::Regex::new(&format!(
            r"(?m)\b([A-Za-z_][A-Za-z0-9_]*)\s*\.\s*({})\s*\(",
            database_methods.join("|")
        ))
        .unwrap();
        let test_module =
            regex::Regex::new(r"(?m)^\s*#\[cfg\(test\)\]\s*\n\s*mod\s+[A-Za-z0-9_]+\s*\{").unwrap();
        let raw_shared_sql = regex::Regex::new(
            r"(?i)\b(?:INSERT\s+INTO|REPLACE\s+INTO|UPDATE|DELETE\s+FROM)\s+(?:issues|comments|labels|dependencies|relations|milestones|milestone_issues)\b",
        )
        .unwrap();
        let mut violations = Vec::new();

        for path in files {
            let relative = path.strip_prefix(&source_root).unwrap();
            let relative_text = relative.to_string_lossy();
            if relative_text == "application.rs"
                || relative_text.starts_with("db/")
                || relative_text.starts_with("shared_writer/")
                || relative_text.starts_with("reconcile/")
                || relative_text == "hydration.rs"
                || relative_text == "compaction.rs"
                || relative_text.contains("tests.rs")
            {
                continue;
            }
            let source = std::fs::read_to_string(&path).unwrap();
            let production = test_module
                .find(&source)
                .map_or(source.as_str(), |found| &source[..found.start()]);
            for capture in method_calls.captures_iter(production) {
                let receiver = &capture[1];
                if !matches!(receiver, "service" | "commands") {
                    violations.push(format!(
                        "{}: {}.{}(",
                        relative.display(),
                        receiver,
                        &capture[2]
                    ));
                }
            }
            if raw_shared_sql.is_match(production) {
                violations.push(format!("{}: raw shared-domain SQL", relative.display()));
            }
            if production.contains("SharedWriter::new(") {
                violations.push(format!("{}: SharedWriter::new(", relative.display()));
            }
        }

        assert!(
            violations.is_empty(),
            "direct production mutation bypasses:\n{}",
            violations.join("\n")
        );
    }
}
