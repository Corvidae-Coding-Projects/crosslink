use std::cell::RefCell;

use anyhow::Result;
use chrono::{DateTime, Utc};

use crate::application::{Command, CommandResult, CommandService, LocalStateService, QueryService};
use crate::db::sentinel::{
    DispatchMetric, NewDispatch, RunCounters, SentinelDispatch, SentinelRun,
};
use crate::db::{CommentAuthorRow, Database, TimeEntryRow, UsageSummaryRow};
use crate::models::{Comment, Issue, Milestone, Session, TokenUsage};

struct RecordingService<'a> {
    database: &'a Database,
    commands: RefCell<Vec<Command>>,
}

impl<'a> RecordingService<'a> {
    fn new(database: &'a Database) -> Self {
        Self {
            database,
            commands: RefCell::new(Vec::new()),
        }
    }

    fn recorded(&self) -> Vec<Command> {
        self.commands.borrow().clone()
    }
}

impl CommandService for RecordingService<'_> {
    fn execute(&self, command: Command) -> Result<CommandResult> {
        self.commands.borrow_mut().push(command.clone());
        self.database.execute(command)
    }
}

impl QueryService for RecordingService<'_> {
    fn list_issue_records(&self) -> Result<Vec<crate::issue_file::IssueFile>> {
        QueryService::list_issue_records(self.database)
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

    fn get_comments_with_author(&self, issue_id: i64) -> Result<Vec<CommentAuthorRow>> {
        self.database.get_comments_with_author(issue_id)
    }

    fn get_time_entries_for_issue(&self, issue_id: i64) -> Result<Vec<TimeEntryRow>> {
        self.database.get_time_entries_for_issue(issue_id)
    }

    fn authority_mode(&self) -> crate::application::AuthorityMode {
        crate::application::AuthorityMode::Local
    }
}

impl LocalStateService for RecordingService<'_> {
    fn get_schema_version(&self) -> Result<i32> {
        self.database.get_schema_version()
    }

    fn start_session(&self, agent_id: Option<&str>) -> Result<i64> {
        self.database.start_session_with_agent(agent_id)
    }

    fn end_session(&self, id: i64, notes: Option<&str>) -> Result<bool> {
        self.database.end_session(id, notes)
    }

    fn set_session_action(&self, id: i64, action: &str) -> Result<bool> {
        self.database.set_session_action(id, action)
    }

    fn start_timer(&self, issue_id: i64) -> Result<i64> {
        self.database.start_timer(issue_id)
    }

    fn stop_timer(&self, issue_id: i64) -> Result<bool> {
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

    fn record_token_usage(&self, usage: &crate::application::LocalTokenUsage) -> Result<i64> {
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
        self.database.insert_sentinel_run(run_id, mode)
    }

    fn complete_sentinel_run(&self, run_id: &str, counters: &RunCounters) -> Result<()> {
        self.database.complete_sentinel_run(run_id, counters)
    }

    fn list_sentinel_runs(&self, limit: usize) -> Result<Vec<SentinelRun>> {
        self.database.list_sentinel_runs(limit)
    }

    fn insert_sentinel_dispatch(&self, dispatch: &NewDispatch<'_>) -> Result<i64> {
        self.database.insert_sentinel_dispatch(dispatch)
    }

    fn update_dispatch_outcome(
        &self,
        dispatch_id: i64,
        outcome: &str,
        outcome_detail: &str,
    ) -> Result<()> {
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

#[test]
fn cli_and_sentinel_adapters_emit_typed_commands() {
    let directory = tempfile::tempdir().unwrap();
    let database = Database::open(&directory.path().join("issues.db")).unwrap();
    let first = database.create_issue("first", None, "medium").unwrap();
    let second = database.create_issue("second", None, "high").unwrap();
    let clone = database.create_issue("clone", None, "low").unwrap();
    database.add_label(clone, "cpitd").unwrap();
    let service = RecordingService::new(&database);

    crate::commands::create::run(
        &service,
        "created through adapter",
        None,
        "medium",
        None,
        None,
        None,
        &crate::commands::create::CreateOpts {
            labels: &["adapter".to_string()],
            work: false,
            quiet: true,
            crosslink_dir: None,
            defer_id: false,
            force: false,
        },
    )
    .unwrap();
    crate::commands::create::run_subissue(
        &service,
        first,
        "subissue through adapter",
        None,
        "medium",
        None,
        &crate::commands::create::CreateOpts {
            labels: &[],
            work: false,
            quiet: true,
            crosslink_dir: None,
            defer_id: false,
            force: false,
        },
    )
    .unwrap();
    let import_path = directory.path().join("import.json");
    std::fs::write(&import_path, "[]").unwrap();
    crate::commands::import::run_json(&service, &import_path).unwrap();

    crate::commands::update::run(
        &service,
        first,
        crate::shared_writer::IssueUpdate {
            title: Some("updated"),
            ..Default::default()
        },
    )
    .unwrap();
    crate::commands::comment::run(&service, first, "recorded", "note").unwrap();
    crate::commands::label::add(&service, first, "recorded").unwrap();
    crate::commands::label::remove(&service, first, "recorded").unwrap();
    crate::commands::deps::block(&service, first, second).unwrap();
    crate::commands::deps::unblock(&service, first, second).unwrap();
    crate::commands::relate::add(&service, first, second).unwrap();
    crate::commands::relate::remove(&service, first, second).unwrap();
    crate::commands::intervene::run(
        &service,
        first,
        "recorded intervention",
        "manual_action",
        None,
        directory.path(),
    )
    .unwrap();
    crate::commands::sentinel::engine::create_triage_issue(
        &service,
        "recording",
        "sentinel recording",
        "sentinel boundary",
        "medium",
        &["sentinel"],
    )
    .unwrap();
    crate::commands::cpitd::clear(&service).unwrap();
    crate::commands::lifecycle::close_quiet(&service, first, false, directory.path()).unwrap();
    crate::commands::archive::archive(&service, first).unwrap();
    crate::commands::archive::unarchive(&service, first).unwrap();
    crate::commands::milestone::create(&service, "recorded milestone", None).unwrap();
    let milestone = service
        .list_milestones(Some("all"))
        .unwrap()
        .into_iter()
        .find(|milestone| milestone.name == "recorded milestone")
        .unwrap();
    crate::commands::milestone::add(&service, milestone.id, &[first]).unwrap();
    crate::commands::milestone::remove(&service, milestone.id, first).unwrap();
    crate::commands::milestone::close(&service, milestone.id).unwrap();
    crate::commands::milestone::delete(&service, milestone.id).unwrap();
    crate::lock_check::try_claim_lock(&service, first, None).unwrap();
    crate::lock_check::try_release_lock(&service, first).unwrap();
    crate::commands::delete::run(&service, second, true).unwrap();
    let agent_request = crate::commands::agent::run(
        crate::AgentCommands::Request {
            target: "recorded-agent".to_string(),
            kind: "pause".to_string(),
            subject_issue: Some(first),
            reason: Some("recording".to_string()),
        },
        directory.path(),
        Some(&service),
    );
    assert!(agent_request.is_err());
    let session_id = service.start_session(None).unwrap();
    service.set_session_issue(session_id, first).unwrap();
    crate::commands::session::action(&service, "recorded action", directory.path()).unwrap();
    crate::commands::session::end(&service, Some("recorded handoff"), directory.path()).unwrap();

    let commands = service.recorded();
    assert!(commands
        .iter()
        .any(|command| matches!(command, Command::UpdateIssue { .. })));
    assert!(commands
        .iter()
        .any(|command| matches!(command, Command::AddComment { .. })));
    assert!(commands
        .iter()
        .any(|command| matches!(command, Command::AddLabel { .. })));
    assert!(commands
        .iter()
        .any(|command| matches!(command, Command::RemoveLabel { .. })));
    assert!(commands
        .iter()
        .any(|command| matches!(command, Command::AddDependency { .. })));
    assert!(commands
        .iter()
        .any(|command| matches!(command, Command::RemoveDependency { .. })));
    assert!(commands
        .iter()
        .any(|command| matches!(command, Command::AddRelation { .. })));
    assert!(commands
        .iter()
        .any(|command| matches!(command, Command::RemoveRelation { .. })));
    assert!(commands
        .iter()
        .any(|command| matches!(command, Command::AddIntervention { .. })));
    assert!(commands
        .iter()
        .any(|command| matches!(command, Command::CreateIssue { .. })));
    assert!(commands
        .iter()
        .any(|command| matches!(command, Command::CreateSubissue { .. })));
    assert!(commands
        .iter()
        .any(|command| matches!(command, Command::ImportIssues { .. })));
    assert!(commands
        .iter()
        .any(|command| matches!(command, Command::CreateMilestone { .. })));
    assert!(commands
        .iter()
        .any(|command| matches!(command, Command::AssignMilestone { .. })));
    assert!(commands
        .iter()
        .any(|command| matches!(command, Command::ClearMilestone { .. })));
    assert!(commands
        .iter()
        .any(|command| matches!(command, Command::CloseMilestone { .. })));
    assert!(commands
        .iter()
        .any(|command| matches!(command, Command::DeleteMilestone { .. })));
    assert!(commands
        .iter()
        .any(|command| matches!(command, Command::ClaimLock { .. })));
    assert!(commands
        .iter()
        .any(|command| matches!(command, Command::ReleaseLock { .. })));
    assert!(commands
        .iter()
        .any(|command| matches!(command, Command::DeleteIssue { .. })));
    assert!(commands
        .iter()
        .any(|command| matches!(command, Command::WriteAgentRequest { .. })));
    assert!(commands
        .iter()
        .any(|command| matches!(command, Command::SetSessionIssue { .. })));
    assert!(commands
        .iter()
        .any(|command| matches!(command, Command::ArchiveIssue { .. })));
    assert!(commands
        .iter()
        .any(|command| matches!(command, Command::UnarchiveIssue { .. })));
}

#[test]
fn command_service_entrypoints_emit_every_typed_mutation() {
    let directory = tempfile::tempdir().unwrap();
    let database = Database::open(&directory.path().join("issues.db")).unwrap();
    let service = RecordingService::new(&database);
    let first = service
        .create_issue("first", None, "medium", None, None)
        .unwrap();
    let second = service
        .create_subissue(first, "second", None, "high")
        .unwrap();
    service.import_issues(&[]).unwrap();
    service
        .update_issue(
            first,
            crate::application::OwnedIssueUpdate {
                title: Some("updated".to_string()),
                description: crate::application::DescriptionChange::Unchanged,
                status: None,
                priority: None,
                scheduled_at: crate::application::DateTimeChange::Unchanged,
                due_at: crate::application::DateTimeChange::Unchanged,
            },
        )
        .unwrap();
    service.add_comment(first, "note", "note").unwrap();
    service
        .add_intervention(first, "intervention", "manual", None, None)
        .unwrap();
    service.add_label(first, "boundary").unwrap();
    service.remove_label(first, "boundary").unwrap();
    service.add_dependency(first, second).unwrap();
    service.remove_dependency(first, second).unwrap();
    service.add_relation(first, second).unwrap();
    service.remove_relation(first, second).unwrap();
    service.close_issue(first).unwrap();
    service.archive_issue(first).unwrap();
    service.unarchive_issue(first).unwrap();
    let milestone = service.create_milestone("boundary", None).unwrap();
    service.assign_milestone(milestone, &[first]).unwrap();
    service.clear_milestone(milestone, first).unwrap();
    service.close_milestone(milestone).unwrap();
    service.delete_milestone(milestone).unwrap();
    let session = service.start_session(Some("recorder")).unwrap();
    service.set_session_issue(session, second).unwrap();
    service.clear_session_issue(session).unwrap();
    assert!(service.claim_lock(first, None).is_err());
    service.release_lock(first).unwrap();
    assert!(service.steal_lock(first, "stale", None).is_err());
    assert!(service.force_release_lock(first, "stale").is_err());
    assert!(service
        .write_agent_request(
            "target-agent",
            &crate::agent_requests::AgentRequest {
                request_id: "request".to_string(),
                kind: crate::agent_requests::RequestKind::Pause,
                subject: crate::agent_requests::RequestSubject::default(),
                requested_by: "recorder".to_string(),
                requested_at: Utc::now().to_rfc3339(),
                reason: None,
            },
        )
        .is_err());
    assert!(service
        .write_agent_ack(
            "target-agent",
            &crate::agent_requests::AgentRequestAck {
                request_id: "request".to_string(),
                ack_at: Utc::now().to_rfc3339(),
                acted: true,
                result: "recorded".to_string(),
                notes: None,
            },
        )
        .is_err());
    service.delete_issue(second).unwrap();

    let commands = service.recorded();
    let covered = commands
        .iter()
        .map(|command| match command {
            Command::CreateIssue { .. } => "create_issue",
            Command::CreateSubissue { .. } => "create_subissue",
            Command::ImportIssues { .. } => "import_issues",
            Command::UpdateIssue { .. } => "update_issue",
            Command::DeleteIssue { .. } => "delete_issue",
            Command::ArchiveIssue { .. } => "archive_issue",
            Command::UnarchiveIssue { .. } => "unarchive_issue",
            Command::AddComment { .. } => "add_comment",
            Command::AddIntervention { .. } => "add_intervention",
            Command::AddLabel { .. } => "add_label",
            Command::RemoveLabel { .. } => "remove_label",
            Command::AddDependency { .. } => "add_dependency",
            Command::RemoveDependency { .. } => "remove_dependency",
            Command::AddRelation { .. } => "add_relation",
            Command::RemoveRelation { .. } => "remove_relation",
            Command::CreateMilestone { .. } => "create_milestone",
            Command::AssignMilestone { .. } => "assign_milestone",
            Command::ClearMilestone { .. } => "clear_milestone",
            Command::CloseMilestone { .. } => "close_milestone",
            Command::DeleteMilestone { .. } => "delete_milestone",
            Command::ClaimLock { .. } => "claim_lock",
            Command::ReleaseLock { .. } => "release_lock",
            Command::StealLock { .. } => "steal_lock",
            Command::ForceReleaseLock { .. } => "force_release_lock",
            Command::SetSessionIssue { .. } => "set_session_issue",
            Command::ClearSessionIssue { .. } => "clear_session_issue",
            Command::WriteAgentRequest { .. } => "write_agent_request",
            Command::WriteAgentAck { .. } => "write_agent_ack",
        })
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(covered.len(), 28);
}

#[test]
fn legacy_http_mutation_adapters_emit_typed_commands() {
    let directory = tempfile::tempdir().unwrap();
    let database = Database::open(&directory.path().join("issues.db")).unwrap();
    let first = database.create_issue("first", None, "medium").unwrap();
    let second = database.create_issue("second", None, "high").unwrap();
    let service = RecordingService::new(&database);
    let created = crate::server::handlers::issues::execute_create_issue_command(
        &service,
        &crate::server::types::CreateIssueRequest {
            title: "http create".to_string(),
            description: Some("body".to_string()),
            priority: crate::server::types::ApiPriority::Medium,
            parent_id: None,
        },
    )
    .unwrap();
    crate::server::handlers::issues::execute_create_subissue_command(
        &service,
        first,
        &crate::server::types::CreateSubissueRequest {
            title: "http child".to_string(),
            description: None,
            priority: crate::server::types::ApiPriority::Low,
        },
    )
    .unwrap();
    crate::server::handlers::issues::execute_update_issue_command(
        &service,
        first,
        crate::server::types::UpdateIssueRequest {
            title: Some("http update".to_string()),
            description: None,
            priority: None,
        },
    )
    .unwrap();
    crate::server::handlers::issues::execute_add_comment_command(
        &service,
        first,
        &crate::server::types::CreateCommentRequest {
            content: "http note".to_string(),
            kind: crate::server::types::CommentKind::Note,
            trigger_type: None,
            intervention_context: None,
        },
    )
    .unwrap();
    crate::server::handlers::issues::execute_add_comment_command(
        &service,
        first,
        &crate::server::types::CreateCommentRequest {
            content: "http intervention".to_string(),
            kind: crate::server::types::CommentKind::Intervention,
            trigger_type: Some("manual".to_string()),
            intervention_context: None,
        },
    )
    .unwrap();
    crate::server::handlers::issues::execute_add_label_command(&service, first, "http").unwrap();
    crate::server::handlers::issues::execute_remove_label_command(&service, first, "http").unwrap();
    crate::server::handlers::issues::execute_add_blocker_command(&service, first, second).unwrap();
    crate::server::handlers::issues::execute_remove_blocker_command(&service, first, second)
        .unwrap();
    crate::server::handlers::issues::execute_close_issue_command(&service, first).unwrap();
    crate::server::handlers::issues::execute_reopen_issue_command(&service, first).unwrap();
    let milestone = crate::server::handlers::milestones::execute_create_milestone_command(
        &service,
        &crate::server::types::CreateMilestoneRequest {
            name: "http milestone".to_string(),
            description: None,
        },
    )
    .unwrap();
    crate::server::handlers::milestones::execute_assign_milestone_command(
        &service, milestone, first,
    )
    .unwrap();
    crate::server::handlers::milestones::execute_close_milestone_command(&service, milestone)
        .unwrap();
    let session = service.start_session(Some("http")).unwrap();
    crate::server::handlers::sessions::execute_set_session_issue_command(&service, session, first)
        .unwrap();
    crate::server::handlers::issues::execute_delete_issue_command(&service, created).unwrap();

    let commands = service.recorded();
    for expected in [
        "create",
        "subissue",
        "update",
        "comment",
        "intervention",
        "label",
        "unlabel",
        "block",
        "unblock",
        "close_reopen",
        "milestone_create",
        "milestone_assign",
        "milestone_close",
        "session_issue",
        "delete",
    ] {
        let found = match expected {
            "create" => commands
                .iter()
                .any(|command| matches!(command, Command::CreateIssue { .. })),
            "subissue" => commands
                .iter()
                .any(|command| matches!(command, Command::CreateSubissue { .. })),
            "update" | "close_reopen" => commands
                .iter()
                .any(|command| matches!(command, Command::UpdateIssue { .. })),
            "comment" => commands
                .iter()
                .any(|command| matches!(command, Command::AddComment { .. })),
            "intervention" => commands
                .iter()
                .any(|command| matches!(command, Command::AddIntervention { .. })),
            "label" => commands
                .iter()
                .any(|command| matches!(command, Command::AddLabel { .. })),
            "unlabel" => commands
                .iter()
                .any(|command| matches!(command, Command::RemoveLabel { .. })),
            "block" => commands
                .iter()
                .any(|command| matches!(command, Command::AddDependency { .. })),
            "unblock" => commands
                .iter()
                .any(|command| matches!(command, Command::RemoveDependency { .. })),
            "milestone_create" => commands
                .iter()
                .any(|command| matches!(command, Command::CreateMilestone { .. })),
            "milestone_assign" => commands
                .iter()
                .any(|command| matches!(command, Command::AssignMilestone { .. })),
            "milestone_close" => commands
                .iter()
                .any(|command| matches!(command, Command::CloseMilestone { .. })),
            "session_issue" => commands
                .iter()
                .any(|command| matches!(command, Command::SetSessionIssue { .. })),
            "delete" => commands
                .iter()
                .any(|command| matches!(command, Command::DeleteIssue { .. })),
            _ => false,
        };
        assert!(found, "HTTP adapter did not emit {expected}");
    }
}

#[test]
fn kickoff_and_orchestrator_adapters_emit_typed_commands() {
    let directory = tempfile::tempdir().unwrap();
    let crosslink_dir = directory.path().join(".crosslink");
    std::fs::create_dir(&crosslink_dir).unwrap();
    let database = Database::open(&crosslink_dir.join("issues.db")).unwrap();
    let service = RecordingService::new(&database);

    crate::commands::kickoff::resolve_kickoff_issue(&service, None, "kickoff recording").unwrap();
    let plan = crate::orchestrator::models::OrchestratorPlan {
        id: "recording-plan".to_string(),
        document_slug: "recording".to_string(),
        phases: vec![crate::orchestrator::models::OrchestratorPhase {
            id: "phase".to_string(),
            title: "Phase".to_string(),
            description: "Phase recording".to_string(),
            stages: vec![crate::orchestrator::models::OrchestratorStage {
                id: "stage".to_string(),
                title: "Stage".to_string(),
                description: "Stage recording".to_string(),
                tasks: Vec::new(),
                depends_on: Vec::new(),
                agent_count: 1,
                complexity_hours: 1.0,
            }],
            gate_criteria: Vec::new(),
        }],
        created_at: Utc::now(),
        total_stages: 1,
        estimated_hours: 1.0,
    };
    crate::orchestrator::executor::OrchestratorExecutor::init(&crosslink_dir, &service, &plan)
        .unwrap();

    let commands = service.recorded();
    assert!(commands
        .iter()
        .any(|command| matches!(command, Command::CreateMilestone { .. })));
    assert!(
        commands
            .iter()
            .filter(|command| matches!(command, Command::CreateIssue { .. }))
            .count()
            >= 2
    );
    assert!(commands
        .iter()
        .any(|command| matches!(command, Command::CreateSubissue { .. })));
    assert!(commands
        .iter()
        .any(|command| matches!(command, Command::AssignMilestone { .. })));
}

#[test]
fn mutation_adapter_registry_uses_the_application_boundary() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let adapters = [
        ("main.rs", "RepositoryService"),
        ("commands/create.rs", "CommandService"),
        ("commands/import.rs", "CommandService"),
        ("commands/locks_cmd.rs", "CommandService"),
        ("commands/session.rs", "CommandService"),
        ("commands/cpitd.rs", "CommandService"),
        ("commands/agent.rs", "CommandService"),
        ("agent_requests.rs", "CommandService"),
        ("commands/kickoff/run.rs", "CommandService"),
        ("commands/swarm/lifecycle.rs", "kickoff::run"),
        ("commands/sentinel/engine.rs", "CommandService"),
        ("orchestrator/executor.rs", "CommandService"),
        ("server/handlers/issues.rs", "RepositoryService::new"),
        ("server/handlers/milestones.rs", "RepositoryService::new"),
        ("server/handlers/sessions.rs", "CommandService"),
        ("server/handlers/usage.rs", "RepositoryService::local_state"),
        ("server/handlers/orchestrator.rs", "RepositoryService::new"),
        ("dashboard/api.rs", "actions::run_cli"),
        ("dashboard/actions.rs", "resolve_crosslink_bin"),
    ];

    for (path, boundary) in adapters {
        let source = std::fs::read_to_string(root.join(path)).unwrap();
        assert!(
            source.contains(boundary),
            "production adapter {path} is not wired through {boundary}"
        );
    }

    let tui = std::fs::read_to_string(root.join("tui/issues_tab.rs")).unwrap();
    assert!(tui.contains("QueryService"));
    for mutation in [
        "create_issue(",
        "update_issue(",
        "close_issue(",
        "add_comment(",
    ] {
        let production = tui
            .find("#[cfg(test)]\nmod tests")
            .map_or(tui.as_str(), |index| &tui[..index]);
        assert!(!production.contains(mutation));
    }
}
