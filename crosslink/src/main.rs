#![warn(clippy::pedantic, clippy::nursery)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::missing_panics_doc)]
#![allow(clippy::use_self)]
#![allow(clippy::module_name_repetitions)]
#![allow(clippy::too_many_lines)]
#![allow(clippy::similar_names)]
#![allow(clippy::items_after_statements)]
#![allow(clippy::wildcard_imports)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::significant_drop_tightening)]
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::cast_possible_wrap,
    clippy::cast_lossless
)]
#![allow(clippy::redundant_pub_crate)]
#![allow(clippy::struct_field_names)]
#![allow(clippy::struct_excessive_bools, clippy::fn_params_excessive_bools)]
#![allow(clippy::ref_option)]
#![allow(clippy::large_stack_arrays)]

mod agent_flags;
mod agent_requests;
mod agents;
mod checkpoint;
mod clock_skew;
mod commands;
mod compaction;
mod daemon;
mod dashboard;
mod db;
mod events;
mod external;
mod findings;
mod git_compat;
mod hub_source;
mod hub_v3;
mod hydration;
mod identity;
mod issue_file;
mod issue_filing;
mod knowledge;
mod lock_check;
mod locks;
mod models;
mod orchestrator;
mod pipeline;
mod seam;
mod server;
mod shared_writer;
mod signing;
mod sync;
mod token_usage;
mod trust_model;
mod tui;
mod utils;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use crosslink::reconcile;
use std::env;
use std::path::PathBuf;

use db::Database;

#[derive(Parser)]
#[command(name = "crosslink")]
#[command(about = "A simple, lean issue tracker CLI")]
#[command(version = option_env!("CROSSLINK_VERSION").unwrap_or(env!("CARGO_PKG_VERSION")))]
struct Cli {
    #[arg(short, long, global = true)]
    quiet: bool,

    #[arg(long, global = true)]
    json: bool,

    #[arg(long, global = true, default_value = "warn", env = "CROSSLINK_LOG")]
    log_level: String,

    #[arg(
        long,
        global = true,
        default_value = "text",
        env = "CROSSLINK_LOG_FORMAT"
    )]
    log_format: String,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Init {
        #[arg(short, long, conflicts_with = "update")]
        force: bool,

        #[arg(long, conflicts_with_all = ["force", "reconfigure"])]
        update: bool,

        #[arg(long, requires = "update")]
        dry_run: bool,

        #[arg(long, requires = "update")]
        no_prompt: bool,

        #[arg(long)]
        python_prefix: Option<String>,

        #[arg(long)]
        skip_cpitd: bool,

        #[arg(long)]
        skip_signing: bool,

        #[arg(long)]
        signing_key: Option<String>,

        #[arg(long, conflicts_with = "update")]
        reconfigure: bool,

        #[arg(long)]
        defaults: bool,

        #[arg(long = "agent-integration", alias = "integration", default_value = "both", value_parser = ["claude", "codex", "both"])]
        agent_integration: String,
    },

    Doctor,

    Issue {
        #[command(subcommand)]
        action: IssueCommands,
    },

    Timer {
        #[command(subcommand)]
        action: TimerCommands,
    },

    Export {
        #[arg(short, long)]
        output: Option<String>,

        #[arg(short, long, default_value = "json")]
        format: String,
    },

    Import {
        input: String,
    },

    Archive {
        #[command(subcommand)]
        action: ArchiveCommands,
    },

    Milestone {
        #[command(subcommand)]
        action: MilestoneCommands,
    },

    Session {
        #[command(subcommand)]
        action: SessionCommands,
    },

    Daemon {
        #[command(subcommand)]
        action: DaemonCommands,
    },

    Cpitd {
        #[command(subcommand)]
        action: CpitdCommands,
    },

    Agent {
        #[command(subcommand)]
        action: AgentCommands,
    },

    Trust {
        #[command(subcommand)]
        action: TrustCommands,
    },

    Locks {
        #[command(subcommand)]
        action: LocksCommands,
    },

    #[command(hide = true)]
    Heartbeat,

    Sync,

    Migrate {
        #[command(subcommand)]
        action: MigrateCommands,
    },

    Reconcile {
        #[arg(long, required = true)]
        check: bool,
    },

    Config {
        #[command(subcommand)]
        command: Option<ConfigCommands>,

        #[arg(long)]
        preset: Option<String>,
    },

    #[command(about = "Inspect prompt-context footprint and deployment health")]
    Context {
        #[command(subcommand)]
        command: ContextCommands,
    },

    Workflow {
        #[command(subcommand)]
        command: WorkflowCommands,
    },

    Style {
        #[command(subcommand)]
        command: StyleCommands,
    },

    Knowledge {
        #[command(subcommand)]
        command: KnowledgeCommands,
    },

    Integrity {
        #[command(subcommand)]
        action: Option<IntegrityCommands>,
    },

    Compact {
        #[arg(long)]
        force: bool,
    },

    Prune {
        #[arg(long = "dry-run")]
        dry_run: bool,

        #[arg(long)]
        force: bool,

        #[arg(long = "keep-commits", default_value = "1")]
        keep_commits: usize,

        #[arg(long = "hub-only")]
        hub_only: bool,

        #[arg(long = "knowledge-only")]
        knowledge_only: bool,
    },

    Kickoff {
        #[command(subcommand)]
        action: Option<KickoffCommands>,
    },

    Design {
        description: Option<String>,

        #[arg(long)]
        issue: Option<i64>,

        #[arg(long = "gh-issue")]
        gh_issue: Option<i64>,

        #[arg(long = "continue", value_name = "SLUG")]
        continue_slug: Option<String>,
    },

    Swarm {
        #[command(subcommand)]
        action: SwarmCommands,
    },

    Sentinel {
        #[command(subcommand)]
        action: SentinelCommands,
    },

    #[command(about = "Open the terminal workspace overview")]
    Tui,

    #[command(alias = "mission-control")]
    Mc {
        #[arg(long, default_value = "tiled")]
        layout: String,
    },

    Dashboard {
        #[command(subcommand)]
        action: DashboardCommands,
    },

    #[command(hide = true)]
    Serve {
        #[arg(long, default_value = "3100")]
        port: u16,

        #[arg(long)]
        dashboard_dir: Option<PathBuf>,
    },

    Container {
        #[command(subcommand)]
        action: ContainerCommands,
    },

    #[command(hide = true)]
    Create {
        title: String,

        #[arg(short, long)]
        description: Option<String>,

        #[arg(short, long, default_value = "medium")]
        priority: String,

        #[arg(short, long)]
        template: Option<String>,

        #[arg(short, long)]
        label: Vec<String>,

        #[arg(short, long)]
        work: bool,

        #[arg(long)]
        defer_id: bool,

        #[arg(long)]
        force: bool,

        #[arg(long, value_parser = parse_issue_id_clap)]
        parent: Option<i64>,

        #[arg(long, value_parser = crate::utils::parse_scheduled_date)]
        scheduled: Option<chrono::DateTime<chrono::Utc>>,

        #[arg(long, value_parser = crate::utils::parse_due_date)]
        due: Option<chrono::DateTime<chrono::Utc>>,
    },

    #[command(hide = true)]
    Quick {
        title: String,

        #[arg(short, long)]
        description: Option<String>,

        #[arg(short, long, default_value = "medium")]
        priority: String,

        #[arg(short, long)]
        template: Option<String>,

        #[arg(short, long)]
        label: Vec<String>,

        #[arg(long)]
        force: bool,

        #[arg(long, value_parser = parse_issue_id_clap)]
        parent: Option<i64>,

        #[arg(long, value_parser = crate::utils::parse_scheduled_date)]
        scheduled: Option<chrono::DateTime<chrono::Utc>>,

        #[arg(long, value_parser = crate::utils::parse_due_date)]
        due: Option<chrono::DateTime<chrono::Utc>>,
    },

    #[command(hide = true)]
    List {
        #[arg(short, long, default_value = "open")]
        status: String,

        #[arg(short, long)]
        label: Option<String>,

        #[arg(short, long)]
        priority: Option<String>,
    },

    #[command(hide = true)]
    Show {
        #[arg(value_parser = parse_issue_id_clap)]
        id: i64,
    },

    #[command(hide = true)]
    Close {
        #[arg(value_parser = parse_issue_id_clap)]
        id: i64,

        #[arg(long)]
        no_changelog: bool,
    },

    #[command(hide = true)]
    New {
        title: String,

        #[arg(short, long)]
        description: Option<String>,

        #[arg(short, long, default_value = "medium")]
        priority: String,

        #[arg(short, long)]
        template: Option<String>,

        #[arg(short, long)]
        label: Vec<String>,

        #[arg(short, long)]
        work: bool,

        #[arg(long, value_parser = parse_issue_id_clap)]
        parent: Option<i64>,
    },

    #[command(hide = true)]
    Issues {
        #[command(subcommand)]
        action: Option<IssuesAliasCommands>,
    },

    #[command(hide = true)]
    Subissue {
        #[arg(value_parser = parse_issue_id_clap)]
        parent: i64,

        title: String,

        #[arg(short, long)]
        description: Option<String>,

        #[arg(short, long, default_value = "medium")]
        priority: String,

        #[arg(short, long)]
        template: Option<String>,

        #[arg(short, long)]
        label: Vec<String>,

        #[arg(short, long)]
        work: bool,

        #[arg(long)]
        force: bool,
    },

    #[command(hide = true, name = "start")]
    TimerStart {
        #[arg(value_parser = parse_issue_id_clap)]
        id: i64,
    },

    #[command(hide = true, name = "stop")]
    TimerStop,

    #[command(hide = true, name = "migrate-to-shared")]
    MigrateToShared,

    #[command(hide = true, name = "migrate-from-shared")]
    MigrateFromShared,

    #[command(hide = true, name = "migrate-rename-branch")]
    MigrateRenameBranch,
}

#[derive(Subcommand)]
enum IssueCommands {
    Create {
        title: String,

        #[arg(short, long)]
        description: Option<String>,

        #[arg(short, long, default_value = "medium")]
        priority: String,

        #[arg(short, long)]
        template: Option<String>,

        #[arg(short, long)]
        label: Vec<String>,

        #[arg(short, long)]
        work: bool,

        #[arg(long)]
        defer_id: bool,

        #[arg(long)]
        force: bool,

        #[arg(long, value_parser = parse_issue_id_clap)]
        parent: Option<i64>,

        #[arg(long, value_parser = crate::utils::parse_scheduled_date)]
        scheduled: Option<chrono::DateTime<chrono::Utc>>,

        #[arg(long, value_parser = crate::utils::parse_due_date)]
        due: Option<chrono::DateTime<chrono::Utc>>,
    },

    Quick {
        title: String,

        #[arg(short, long)]
        description: Option<String>,

        #[arg(short, long, default_value = "medium")]
        priority: String,

        #[arg(short, long)]
        template: Option<String>,

        #[arg(short, long)]
        label: Vec<String>,

        #[arg(long)]
        force: bool,

        #[arg(long, value_parser = parse_issue_id_clap)]
        parent: Option<i64>,

        #[arg(long, value_parser = crate::utils::parse_scheduled_date)]
        scheduled: Option<chrono::DateTime<chrono::Utc>>,

        #[arg(long, value_parser = crate::utils::parse_due_date)]
        due: Option<chrono::DateTime<chrono::Utc>>,
    },

    List {
        #[arg(short, long, default_value = "open")]
        status: String,

        #[arg(short, long)]
        label: Option<String>,

        #[arg(short, long)]
        priority: Option<String>,

        #[arg(long)]
        repo: Option<String>,

        #[arg(long)]
        refresh: bool,
    },

    Search {
        query: String,

        #[arg(long)]
        repo: Option<String>,

        #[arg(long)]
        refresh: bool,
    },

    Show {
        #[arg(value_parser = parse_issue_id_clap)]
        id: i64,

        #[arg(long)]
        repo: Option<String>,

        #[arg(long)]
        refresh: bool,
    },

    Update {
        #[arg(value_parser = parse_issue_id_clap)]
        id: i64,

        #[arg(short, long)]
        title: Option<String>,

        #[arg(short, long)]
        description: Option<String>,

        #[arg(short, long)]
        priority: Option<String>,

        #[arg(long, value_parser = crate::utils::parse_scheduled_date)]
        scheduled: Option<chrono::DateTime<chrono::Utc>>,

        #[arg(long, conflicts_with = "scheduled")]
        no_scheduled: bool,

        #[arg(long, value_parser = crate::utils::parse_due_date)]
        due: Option<chrono::DateTime<chrono::Utc>>,

        #[arg(long, conflicts_with = "due")]
        no_due: bool,
    },

    Close {
        #[arg(value_parser = parse_issue_id_clap)]
        id: i64,

        #[arg(long)]
        no_changelog: bool,
    },

    CloseAll {
        #[arg(short, long)]
        label: Option<String>,

        #[arg(short, long)]
        priority: Option<String>,

        #[arg(long)]
        no_changelog: bool,
    },

    Reopen {
        #[arg(value_parser = parse_issue_id_clap)]
        id: i64,
    },

    Delete {
        #[arg(value_parser = parse_issue_id_clap)]
        id: i64,

        #[arg(short, long)]
        force: bool,
    },

    Comment {
        #[arg(value_parser = parse_issue_id_clap)]
        id: i64,

        text: String,

        #[arg(long, default_value = "note")]
        kind: String,
    },

    Intervene {
        #[arg(value_parser = parse_issue_id_clap)]
        id: i64,

        description: String,

        #[arg(long)]
        trigger: String,

        #[arg(long)]
        context: Option<String>,
    },

    Label {
        #[arg(value_parser = parse_issue_id_clap)]
        id: i64,

        label: String,
    },

    Unlabel {
        #[arg(value_parser = parse_issue_id_clap)]
        id: i64,

        label: String,
    },

    Block {
        #[arg(value_parser = parse_issue_id_clap)]
        id: i64,

        #[arg(value_parser = parse_issue_id_clap)]
        blocker: i64,
    },

    Unblock {
        #[arg(value_parser = parse_issue_id_clap)]
        id: i64,

        #[arg(value_parser = parse_issue_id_clap)]
        blocker: i64,
    },

    Blocked,

    Ready,

    Relate {
        #[arg(value_parser = parse_issue_id_clap)]
        id: i64,

        #[arg(value_parser = parse_issue_id_clap)]
        related: i64,
    },

    Unrelate {
        #[arg(value_parser = parse_issue_id_clap)]
        id: i64,

        #[arg(value_parser = parse_issue_id_clap)]
        related: i64,
    },

    Related {
        #[arg(value_parser = parse_issue_id_clap)]
        id: i64,
    },

    Next,

    Tree {
        #[arg(short, long, default_value = "all")]
        status: String,
    },

    Tested,
}

#[derive(Subcommand)]
enum TimerCommands {
    Start {
        #[arg(value_parser = parse_issue_id_clap)]
        id: i64,
    },

    Stop,

    Show,
}

#[derive(Subcommand)]
enum MigrateCommands {
    ToShared,

    FromShared,

    RenameBranch,

    HubV3 {
        #[arg(long)]
        finalize: bool,

        #[arg(long)]
        yes_delete_v2: bool,

        #[arg(long)]
        adopt_stale: bool,

        #[arg(long)]
        remigrate_from_v2: bool,
    },

    HubBranches,
}

#[derive(Subcommand)]
enum DashboardCommands {
    Serve {
        #[arg(long, default_value = "3100")]
        port: u16,

        #[arg(long)]
        dashboard_dir: Option<PathBuf>,

        #[arg(long)]
        rotate_token: bool,
    },

    Track {
        path: PathBuf,

        #[arg(long)]
        slug: Option<String>,

        #[arg(long)]
        init: bool,

        #[arg(long)]
        agent_id: Option<String>,
    },

    Untrack {
        slug: String,
    },

    List,

    Discover {
        #[arg(long)]
        root: Vec<PathBuf>,

        #[arg(long, default_value_t = crate::dashboard::discover::DEFAULT_DEPTH)]
        depth: usize,

        #[arg(long)]
        track: bool,
    },
}

#[derive(Subcommand)]
enum IssuesAliasCommands {
    List {
        #[arg(short, long, default_value = "open")]
        status: String,

        #[arg(short, long)]
        label: Option<String>,

        #[arg(short, long)]
        priority: Option<String>,
    },
}

#[derive(Subcommand)]
enum ContainerCommands {
    Build {
        #[arg(long)]
        force: bool,

        #[arg(long)]
        tag: Option<String>,

        #[arg(long)]
        dockerfile: Option<String>,
    },

    Start {
        worktree: String,

        #[arg(long)]
        name: Option<String>,

        #[arg(long)]
        prompt: Option<String>,

        #[arg(long)]
        issue: Option<i64>,

        #[arg(long)]
        memory: Option<String>,
    },

    Ps,

    Logs {
        name: String,

        #[arg(short, long)]
        follow: bool,

        #[arg(long)]
        tail: Option<u32>,
    },

    Stop {
        name: String,
    },

    Rm {
        name: String,
    },

    Kill {
        name: String,
    },

    Shell {
        name: String,
    },

    Snapshot {
        name: String,

        #[arg(long)]
        tag: Option<String>,
    },

    Auth {
        #[command(subcommand)]
        action: ContainerAuthCommands,
    },
}

#[derive(Subcommand)]
enum ContainerAuthCommands {
    Login {
        #[arg(long, default_value = "codex", value_parser = ["claude", "codex"])]
        provider: String,
    },

    Status {
        #[arg(long, default_value = "codex", value_parser = ["claude", "codex"])]
        provider: String,
    },

    Refresh {
        #[arg(long, default_value = "codex", value_parser = ["claude", "codex"])]
        provider: String,
    },

    Logout {
        #[arg(long, default_value = "codex", value_parser = ["claude", "codex"])]
        provider: String,

        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand, Clone, Copy)]
enum ArchiveCommands {
    Add {
        #[arg(value_parser = parse_issue_id_clap)]
        id: i64,
    },

    Remove {
        #[arg(value_parser = parse_issue_id_clap)]
        id: i64,
    },

    List,

    Older {
        days: i64,
    },
}

#[derive(Subcommand)]
enum MilestoneCommands {
    Create {
        name: String,

        #[arg(short, long)]
        description: Option<String>,
    },

    List {
        #[arg(short, long, default_value = "open")]
        status: String,
    },

    Show {
        id: i64,
    },

    Add {
        id: i64,

        issues: Vec<i64>,
    },

    Remove {
        id: i64,

        issue: i64,
    },

    Close {
        id: i64,
    },

    Delete {
        id: i64,
    },
}

#[derive(Subcommand)]
enum SessionCommands {
    Start,

    End {
        #[arg(short, long)]
        notes: Option<String>,
    },

    Status,

    Work {
        #[arg(value_parser = parse_issue_id_clap)]
        id: i64,
    },

    LastHandoff,

    Action {
        text: String,
    },
}

#[derive(Subcommand)]
enum CpitdCommands {
    Scan {
        paths: Vec<String>,

        #[arg(long, default_value = "50")]
        min_tokens: u32,

        #[arg(long)]
        ignore: Vec<String>,

        #[arg(long = "dry-run")]
        dry_run: bool,
    },

    Status,

    Clear,
}

#[derive(Subcommand)]
enum DaemonCommands {
    Start,

    Stop,

    Status,

    #[command(hide = true)]
    Run {
        #[arg(long)]
        dir: PathBuf,
    },
}

#[derive(Subcommand)]
enum AgentCommands {
    Init {
        agent_id: String,

        #[arg(short, long)]
        description: Option<String>,

        #[arg(long)]
        no_key: bool,

        #[arg(long)]
        force: bool,

        #[arg(long, value_parser = ["driver", "agent"])]
        role: Option<String>,
    },

    Status,

    Prompt {
        session: String,

        message: String,

        #[arg(long)]
        no_submit: bool,
    },

    Bootstrap {
        #[arg(long)]
        repo: String,

        #[arg(long)]
        identity: String,

        #[arg(long)]
        branch: Option<String>,

        #[arg(short, long)]
        description: Option<String>,

        #[arg(long)]
        no_key: bool,

        #[arg(long, default_value = ".")]
        target: String,
    },

    Request {
        target: String,

        kind: String,

        #[arg(long)]
        subject_issue: Option<i64>,

        #[arg(long)]
        reason: Option<String>,
    },

    Requests {
        #[arg(long)]
        target: Option<String>,

        #[arg(long)]
        pending: bool,
    },

    PollRequests,

    Flags {
        #[arg(long)]
        strict: bool,
    },
}

#[derive(Subcommand)]
enum TrustCommands {
    Approve { agent_id: String },

    Revoke { agent_id: String },

    List,

    Pending,

    Check { agent_id: String },
}

#[derive(Subcommand)]
enum LocksCommands {
    List,

    Check {
        #[arg(value_parser = parse_issue_id_clap)]
        id: i64,
    },

    Claim {
        #[arg(value_parser = parse_issue_id_clap)]
        id: i64,

        #[arg(short, long)]
        branch: Option<String>,
    },

    Release {
        #[arg(value_parser = parse_issue_id_clap)]
        id: i64,
    },

    Steal {
        #[arg(value_parser = parse_issue_id_clap)]
        id: i64,
    },
}

#[derive(Subcommand)]
enum WorkflowCommands {
    Diff {
        #[arg(short, long)]
        section: Option<String>,

        #[arg(long)]
        check: bool,
    },

    Trail {
        #[arg(value_parser = parse_issue_id_clap)]
        id: i64,

        #[arg(long)]
        kind: Option<String>,

        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum StyleCommands {
    Set {
        url: String,

        #[arg(long, name = "ref")]
        ref_name: Option<String>,
    },

    Sync {
        #[arg(long = "dry-run")]
        dry_run: bool,
    },

    Diff,

    Show,

    Unset,
}

#[derive(Subcommand)]
enum KnowledgeCommands {
    Add {
        slug: String,

        #[arg(short, long)]
        title: Option<String>,

        #[arg(long)]
        tag: Vec<String>,

        #[arg(long)]
        source: Vec<String>,

        #[arg(long)]
        content: Option<String>,

        #[arg(long, value_name = "PATH")]
        from_doc: Option<PathBuf>,

        #[arg(long = "contributor", value_name = "AGENT_ID")]
        contributor: Vec<String>,

        #[arg(long, hide = true)]
        repo: Option<String>,
    },

    Show {
        slug: String,

        #[arg(long)]
        repo: Option<String>,

        #[arg(long)]
        refresh: bool,
    },

    List {
        #[arg(long)]
        tag: Option<String>,

        #[arg(long)]
        contributor: Option<String>,

        #[arg(long)]
        since: Option<String>,

        #[arg(long)]
        json: bool,

        #[arg(long)]
        repo: Option<String>,

        #[arg(long)]
        refresh: bool,
    },

    Edit {
        slug: String,

        #[arg(long, group = "content_mode")]
        append: Option<String>,

        #[arg(long)]
        content: Option<String>,

        #[arg(long, value_name = "HEADING", group = "content_mode")]
        replace_section: Option<String>,

        #[arg(long, value_name = "HEADING", group = "content_mode")]
        append_to_section: Option<String>,

        #[arg(long)]
        tag: Vec<String>,

        #[arg(long)]
        source: Vec<String>,

        #[arg(long, value_name = "PATH")]
        from_doc: Option<PathBuf>,

        #[arg(long = "contributor", value_name = "AGENT_ID")]
        contributor: Vec<String>,

        #[arg(long, hide = true)]
        repo: Option<String>,
    },

    Remove {
        slug: String,

        #[arg(long, hide = true)]
        repo: Option<String>,
    },

    Sync {
        #[arg(long, hide = true)]
        repo: Option<String>,
    },

    Import {
        directory: PathBuf,

        #[arg(long)]
        tag: Vec<String>,

        #[arg(long)]
        overwrite: bool,

        #[arg(long = "dry-run")]
        dry_run: bool,

        #[arg(long = "contributor", value_name = "AGENT_ID")]
        contributor: Vec<String>,

        #[arg(long, hide = true)]
        repo: Option<String>,
    },

    Search {
        query: Option<String>,

        #[arg(short = 'C', long, default_value = "1")]
        context: usize,

        #[arg(long)]
        source: Option<String>,

        #[arg(long)]
        tag: Option<String>,

        #[arg(long)]
        since: Option<String>,

        #[arg(long)]
        contributor: Option<String>,

        #[arg(long)]
        repo: Option<String>,

        #[arg(long)]
        refresh: bool,
    },
}

#[derive(Subcommand)]
enum IntegrityCommands {
    Counters {
        #[arg(long)]
        repair: bool,
    },

    Hydration {
        #[arg(long)]
        repair: bool,

        #[arg(long)]
        accept_data_loss: bool,
    },

    Locks {
        #[arg(long)]
        repair: bool,
    },

    Schema {
        #[arg(long)]
        repair: bool,
    },

    Layout {
        #[arg(long)]
        repair: bool,
    },

    SignBackfill {
        #[arg(long)]
        confirm: bool,

        #[arg(long)]
        key: Option<std::path::PathBuf>,
    },
}

#[derive(Subcommand)]
enum KickoffCommands {
    Run {
        description: String,

        #[arg(long)]
        issue: Option<i64>,

        #[arg(long, default_value = "none")]
        container: String,

        #[arg(long, default_value = "local")]
        verify: String,

        #[arg(long, default_value = "standard")]
        model: String,

        #[arg(
            long,
            default_value = "ghcr.io/corvidae-coding-projects/crosslink-agent:latest"
        )]
        image: String,

        #[arg(long, default_value = "1h")]
        timeout: String,

        #[arg(long = "dry-run")]
        dry_run: bool,

        #[arg(long)]
        branch: Option<String>,

        #[arg(long, value_name = "PATH")]
        doc: Option<PathBuf>,

        #[arg(long)]
        skip_permissions: bool,

        #[arg(
            long,
            value_parser = clap::builder::PossibleValuesParser::new([
                "acceptEdits", "auto", "bypassPermissions",
                "default", "dontAsk", "plan",
            ]),
            conflicts_with = "skip_permissions"
        )]
        permission_mode: Option<String>,

        #[arg(
            long,
            value_name = "LEVEL",
            value_parser = clap::builder::PossibleValuesParser::new(
                commands::kickoff::EFFORT_LEVELS,
            )
        )]
        effort: Option<String>,

        #[arg(long, value_name = "AMOUNT")]
        budget_usd: Option<String>,

        #[arg(long, value_name = "PATH")]
        template: Option<PathBuf>,
    },

    Status {
        agent: Option<String>,
    },

    Logs {
        agent: String,

        #[arg(short, long, default_value = "20")]
        lines: usize,
    },

    Stop {
        agent: String,

        #[arg(long)]
        force: bool,
    },

    Plan {
        doc: PathBuf,

        #[arg(long)]
        issue: Option<i64>,

        #[arg(long, default_value = "standard")]
        model: String,

        #[arg(long, default_value = "30m")]
        timeout: String,

        #[arg(long = "dry-run")]
        dry_run: bool,

        #[arg(long)]
        skip_permissions: bool,

        #[arg(
            long,
            value_parser = clap::builder::PossibleValuesParser::new([
                "acceptEdits", "auto", "bypassPermissions",
                "default", "dontAsk", "plan",
            ]),
            conflicts_with = "skip_permissions"
        )]
        permission_mode: Option<String>,

        #[arg(
            long,
            value_name = "LEVEL",
            value_parser = clap::builder::PossibleValuesParser::new(
                commands::kickoff::EFFORT_LEVELS,
            )
        )]
        effort: Option<String>,

        #[arg(long, value_name = "AMOUNT")]
        budget_usd: Option<String>,

        #[arg(long, value_name = "PATH")]
        template: Option<PathBuf>,
    },

    ShowPlan {
        agent: String,
    },

    Report {
        agent: Option<String>,

        #[arg(long)]
        json: bool,

        #[arg(long)]
        markdown: bool,

        #[arg(long)]
        all: bool,
    },

    List {
        #[arg(long, default_value = "all")]
        status: String,
    },

    Cleanup {
        #[arg(long = "dry-run")]
        dry_run: bool,

        #[arg(long)]
        force: bool,

        #[arg(long, default_value = "0")]
        keep: usize,

        #[arg(long)]
        json: bool,
    },

    Graph {
        #[arg(long)]
        all: bool,

        #[arg(long)]
        no_pager: bool,
    },

    #[command(alias = "go")]
    Launch {
        doc: Option<PathBuf>,

        #[arg(long)]
        plan: bool,

        #[arg(long)]
        run: bool,

        #[arg(long, default_value = "local")]
        verify: String,

        #[arg(long, default_value = "standard")]
        model: String,

        #[arg(long, default_value = "1h")]
        timeout: String,

        #[arg(long, default_value = "none")]
        container: String,

        #[arg(long)]
        issue: Option<i64>,

        #[arg(long = "dry-run")]
        dry_run: bool,

        #[arg(long)]
        skip_permissions: bool,

        #[arg(
            long,
            value_parser = clap::builder::PossibleValuesParser::new([
                "acceptEdits", "auto", "bypassPermissions",
                "default", "dontAsk", "plan",
            ]),
            conflicts_with = "skip_permissions"
        )]
        permission_mode: Option<String>,
    },
}

#[derive(Subcommand)]
enum ConfigCommands {
    Show,

    Get {
        key: String,
    },

    Set {
        key: String,

        value: Option<String>,

        #[arg(long)]
        add: Option<String>,

        #[arg(long)]
        remove: Option<String>,

        #[arg(long)]
        local: bool,
    },

    List,

    Reset {
        key: Option<String>,
    },

    Diff,
}

#[derive(Subcommand)]
enum SwarmCommands {
    Init {
        #[arg(long, value_name = "PATH")]
        doc: PathBuf,
    },

    Status,

    Resume,

    SyncStatus,

    Adopt {
        agent: String,

        #[arg(long, value_name = "SLUG")]
        slot: String,
    },

    Archive,

    Reset {
        #[arg(long)]
        no_archive: bool,
    },

    #[command(name = "list")]
    ListSwarms,

    Launch {
        phase: String,

        #[arg(long)]
        retry_failed: bool,

        #[arg(long)]
        budget_aware: bool,
    },

    Gate {
        phase: String,
    },

    Checkpoint {
        phase: String,

        #[arg(long)]
        notes: Option<String>,

        #[arg(long)]
        force: bool,
    },

    Config {
        #[arg(long, value_name = "DURATION")]
        budget_window: String,

        #[arg(long, default_value = "standard")]
        model: String,

        #[arg(
            long,
            value_name = "LEVEL",
            value_parser = clap::builder::PossibleValuesParser::new(
                commands::kickoff::EFFORT_LEVELS,
            )
        )]
        effort: Option<String>,

        #[arg(long, value_name = "AMOUNT")]
        budget_usd: Option<String>,
    },

    Estimate {
        phase: String,
    },

    Harvest,

    Plan {
        #[arg(long, value_name = "DURATION")]
        budget_window: Option<String>,
    },

    PlanShow,

    Review {
        #[arg(long, default_value = "4")]
        agents: usize,

        #[arg(long, default_value = "adversarial")]
        mandate: String,

        #[arg(long, value_name = "PATH")]
        doc: Option<PathBuf>,

        #[arg(long)]
        file_issues: bool,

        #[arg(long)]
        fix: bool,
    },

    Fix {
        #[arg(long, value_name = "IDS")]
        issues: Option<String>,

        #[arg(long, value_name = "LABEL")]
        from_label: Option<String>,

        #[arg(long, default_value = "6")]
        max_agents: usize,

        #[arg(long)]
        budget_aware: bool,
    },

    Merge {
        #[arg(long, default_value = "swarm-combined")]
        branch: String,

        #[arg(long)]
        base: Option<String>,

        #[arg(long)]
        dry_run: bool,

        #[arg(long, value_name = "SLUGS")]
        agents: Option<String>,
    },

    #[command(name = "move")]
    MoveAgent {
        agent: String,

        #[arg(long, value_name = "PHASE")]
        to_phase: String,
    },

    MergePhases {
        phase_a: String,

        phase_b: String,
    },

    SplitPhase {
        phase: String,

        #[arg(long, value_name = "SLUG")]
        after: String,
    },

    RemoveAgent {
        agent: String,
    },

    Reorder {
        phase: String,

        #[arg(long)]
        position: usize,
    },

    RenamePhase {
        old: String,

        new: String,
    },

    ReviewContinue,

    ReviewStatus,

    Pipeline {
        #[arg(long, default_value = "4")]
        agents: usize,

        #[arg(long, default_value = "adversarial")]
        mandate: String,

        #[arg(long, default_value = "main")]
        target_branch: String,

        #[arg(long)]
        auto_fix: bool,

        #[arg(long)]
        auto_file_issues: bool,
    },

    TrustInit {
        #[arg(long, default_value = "local-only")]
        model: String,
    },
}

#[derive(Subcommand)]
pub enum SentinelCommands {
    Run {
        #[arg(long)]
        dry_run: bool,

        #[arg(long)]
        label: Option<String>,

        #[arg(long)]
        model: Option<String>,
    },

    Watch {
        #[arg(long, default_value = "10")]
        interval: u64,

        #[arg(long)]
        model: Option<String>,
    },

    Status,

    History {
        #[arg(long, default_value = "10")]
        limit: usize,

        #[arg(long)]
        detail: bool,

        #[arg(long)]
        json: bool,
    },

    Stop,

    Metrics {
        #[arg(long)]
        json: bool,
    },

    Patterns {
        #[arg(long)]
        json: bool,
    },

    #[command(hide = true)]
    RunDaemon {
        #[arg(long)]
        dir: std::path::PathBuf,
        #[arg(long, default_value = "10")]
        interval: u64,

        #[arg(long)]
        model: Option<String>,
    },
}

#[derive(Subcommand, Clone, Copy)]
enum ContextCommands {
    Measure {
        #[arg(short, long)]
        verbose: bool,
    },

    Check,
}

fn init_tracing(log_level: &str, log_format: &str) {
    use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};
    let filter = EnvFilter::try_new(log_level).unwrap_or_else(|_| EnvFilter::new("warn"));
    if log_format == "json" {
        tracing_subscriber::registry()
            .with(filter)
            .with(fmt::layer().json().with_writer(std::io::stderr))
            .init();
    } else {
        tracing_subscriber::registry()
            .with(filter)
            .with(fmt::layer().with_target(false).with_writer(std::io::stderr))
            .init();
    }
}

fn find_crosslink_dir() -> Result<PathBuf> {
    find_crosslink_dir_from(&env::current_dir()?)
}

fn is_initialized_crosslink_dir(candidate: &std::path::Path) -> bool {
    candidate.join("hook-config.json").is_file()
}

fn find_crosslink_dir_from(start: &std::path::Path) -> Result<PathBuf> {
    let mut current = start.to_path_buf();
    let mut skipped_strays: Vec<PathBuf> = Vec::new();

    loop {
        let candidate = current.join(".crosslink");
        if candidate.is_dir() {
            if is_initialized_crosslink_dir(&candidate) {
                for stray in &skipped_strays {
                    tracing::warn!(
                        "ignoring stray .crosslink at {} (no hook-config.json — likely \
                         created by cwd drift, GH#625); using {} — consider deleting \
                         the stray",
                        stray.display(),
                        candidate.display()
                    );
                }
                return Ok(candidate);
            }
            skipped_strays.push(candidate);
        }

        if !current.pop() {
            break;
        }
    }

    if let Some(main_root) = utils::resolve_main_repo_root(start) {
        let candidate = main_root.join(".crosslink");
        if candidate.is_dir() && is_initialized_crosslink_dir(&candidate) {
            return Ok(candidate);
        }
    }

    if skipped_strays.is_empty() {
        bail!("Not a crosslink repository (or any parent). Run 'crosslink init' first.");
    }
    bail!(
        "Not a crosslink repository (or any parent). Found stray uninitialized \
         .crosslink director{} (no hook-config.json) at: {} — likely created by \
         cwd drift (GH#625). Delete {} or run 'crosslink init' at the project root.",
        if skipped_strays.len() == 1 {
            "y"
        } else {
            "ies"
        },
        skipped_strays
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", "),
        if skipped_strays.len() == 1 {
            "it"
        } else {
            "them"
        },
    );
}

fn get_db() -> Result<Database> {
    let crosslink_dir = find_crosslink_dir()?;
    let db_path = crosslink_dir.join("issues.db");
    let db = Database::open(&db_path).context("Failed to open database")?;

    if let Err(e) = hydration::maybe_auto_hydrate(&crosslink_dir, &db) {
        tracing::debug!("auto-hydration skipped: {}", e);
    }

    Ok(db)
}

fn get_writer(crosslink_dir: &std::path::Path) -> Option<shared_writer::SharedWriter> {
    match shared_writer::SharedWriter::new(crosslink_dir) {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!("SharedWriter unavailable: {}", e);
            None
        }
    }
}

fn parse_issue_id_clap(s: &str) -> std::result::Result<i64, String> {
    parse_issue_id(s).map_err(|e| e.to_string())
}

fn parse_issue_id(s: &str) -> Result<i64> {
    if let Some(n) = s.strip_prefix('L').or_else(|| s.strip_prefix('l')) {
        let num: i64 = n
            .parse()
            .with_context(|| format!("Invalid local issue ID: {s}"))?;
        if num <= 0 {
            bail!("Local issue ID must be positive: {s}");
        }
        Ok(-num)
    } else {
        s.parse().with_context(|| format!("Invalid issue ID: {s}"))
    }
}

fn hint(quiet: bool, msg: &str) {
    if !quiet {
        tracing::info!("hint: {}", msg);
    }
}

const fn scheduled_update(
    set: Option<chrono::DateTime<chrono::Utc>>,
    clear: bool,
) -> shared_writer::FieldUpdate<chrono::DateTime<chrono::Utc>> {
    use shared_writer::FieldUpdate;
    if clear {
        FieldUpdate::Clear
    } else if let Some(dt) = set {
        FieldUpdate::Set(dt)
    } else {
        FieldUpdate::Unchanged
    }
}

fn dispatch_issue(action: IssueCommands, quiet: bool, json: bool) -> Result<()> {
    match action {
        IssueCommands::Create {
            title,
            description,
            priority,
            template,
            label,
            work,
            defer_id,
            force,
            parent,
            scheduled,
            due,
        } => {
            let db = get_db()?;
            let crosslink_dir = find_crosslink_dir()?;
            let writer = get_writer(&crosslink_dir);
            let opts = commands::create::CreateOpts {
                labels: &label,
                work,
                quiet,
                crosslink_dir: Some(&crosslink_dir),
                defer_id: parent.is_none() && defer_id,
                force,
            };

            if let Some(parent_id) = parent {
                if scheduled.is_some() || due.is_some() {
                    anyhow::bail!("Scheduling dates apply to parent issues, not subissues.");
                }
                commands::create::run_subissue(
                    &db,
                    writer.as_ref(),
                    parent_id,
                    &title,
                    description.as_deref(),
                    &priority,
                    template.as_deref(),
                    &opts,
                )
            } else {
                commands::create::run(
                    &db,
                    writer.as_ref(),
                    &title,
                    description.as_deref(),
                    &priority,
                    template.as_deref(),
                    scheduled,
                    due,
                    &opts,
                )
            }
        }

        IssueCommands::Quick {
            title,
            description,
            priority,
            template,
            label,
            force,
            parent,
            scheduled,
            due,
        } => {
            let db = get_db()?;
            let crosslink_dir = find_crosslink_dir()?;
            let writer = get_writer(&crosslink_dir);
            let opts = commands::create::CreateOpts {
                labels: &label,
                work: true,
                quiet,
                crosslink_dir: Some(&crosslink_dir),
                defer_id: false,
                force,
            };
            if let Some(parent_id) = parent {
                if scheduled.is_some() || due.is_some() {
                    anyhow::bail!("Scheduling dates apply to parent issues, not subissues.");
                }
                commands::create::run_subissue(
                    &db,
                    writer.as_ref(),
                    parent_id,
                    &title,
                    description.as_deref(),
                    &priority,
                    template.as_deref(),
                    &opts,
                )
            } else {
                commands::create::run(
                    &db,
                    writer.as_ref(),
                    &title,
                    description.as_deref(),
                    &priority,
                    template.as_deref(),
                    scheduled,
                    due,
                    &opts,
                )
            }
        }

        IssueCommands::List {
            status,
            label,
            priority,
            repo,
            refresh,
        } => {
            if let Some(repo_value) = repo {
                let crosslink_dir = find_crosslink_dir()?;
                commands::external_issues::list(
                    &crosslink_dir,
                    &repo_value,
                    Some(&status),
                    label.as_deref(),
                    priority.as_deref(),
                    refresh,
                    json,
                    quiet,
                )
            } else {
                let db = get_db()?;
                if json {
                    commands::list::run_json(
                        &db,
                        Some(&status),
                        label.as_deref(),
                        priority.as_deref(),
                    )
                } else {
                    commands::list::run(&db, Some(&status), label.as_deref(), priority.as_deref())
                }
            }
        }

        IssueCommands::Search {
            query,
            repo,
            refresh,
        } => {
            if let Some(repo_value) = repo {
                let crosslink_dir = find_crosslink_dir()?;
                commands::external_issues::search(
                    &crosslink_dir,
                    &repo_value,
                    &query,
                    refresh,
                    json,
                    quiet,
                )
            } else {
                let db = get_db()?;
                if json {
                    commands::search::run_json(&db, &query)
                } else {
                    commands::search::run(&db, &query)
                }
            }
        }

        IssueCommands::Show { id, repo, refresh } => {
            if let Some(repo_value) = repo {
                let crosslink_dir = find_crosslink_dir()?;
                commands::external_issues::show(
                    &crosslink_dir,
                    &repo_value,
                    id,
                    refresh,
                    json,
                    quiet,
                )
            } else {
                let db = get_db()?;
                if json {
                    commands::show::run_json(&db, id)
                } else {
                    commands::show::run(&db, id)
                }
            }
        }

        IssueCommands::Update {
            id,
            title,
            description,
            priority,
            scheduled,
            no_scheduled,
            due,
            no_due,
        } => {
            let db = get_db()?;
            let crosslink_dir = find_crosslink_dir()?;
            let writer = get_writer(&crosslink_dir);
            let update = shared_writer::IssueUpdate {
                title: title.as_deref(),
                description: description
                    .as_deref()
                    .map_or(shared_writer::DescriptionUpdate::Unchanged, |s| {
                        shared_writer::DescriptionUpdate::Set(s)
                    }),
                status: None,
                priority: priority.as_deref(),
                scheduled_at: scheduled_update(scheduled, no_scheduled),
                due_at: scheduled_update(due, no_due),
            };
            commands::update::run(&db, writer.as_ref(), id, update)
        }

        IssueCommands::Close { id, no_changelog } => {
            let db = get_db()?;
            let crosslink_dir = find_crosslink_dir()?;
            let writer = get_writer(&crosslink_dir);
            if quiet {
                commands::lifecycle::close_quiet(
                    &db,
                    writer.as_ref(),
                    id,
                    !no_changelog,
                    &crosslink_dir,
                )
            } else {
                commands::lifecycle::close(&db, writer.as_ref(), id, !no_changelog, &crosslink_dir)
            }
        }

        IssueCommands::CloseAll {
            label,
            priority,
            no_changelog,
        } => {
            let db = get_db()?;
            let crosslink_dir = find_crosslink_dir()?;
            let writer = get_writer(&crosslink_dir);
            commands::lifecycle::close_all(
                &db,
                writer.as_ref(),
                label.as_deref(),
                priority.as_deref(),
                !no_changelog,
                &crosslink_dir,
            )
        }

        IssueCommands::Reopen { id } => {
            let db = get_db()?;
            let crosslink_dir = find_crosslink_dir()?;
            let writer = get_writer(&crosslink_dir);
            commands::lifecycle::reopen(&db, writer.as_ref(), id)
        }

        IssueCommands::Delete { id, force } => {
            let db = get_db()?;
            let crosslink_dir = find_crosslink_dir()?;
            let writer = get_writer(&crosslink_dir);
            commands::delete::run(&db, writer.as_ref(), id, force)
        }

        IssueCommands::Comment { id, text, kind } => {
            let db = get_db()?;
            let crosslink_dir = find_crosslink_dir()?;
            let writer = get_writer(&crosslink_dir);
            commands::comment::run(&db, writer.as_ref(), id, &text, &kind)
        }

        IssueCommands::Intervene {
            id,
            description,
            trigger,
            context,
        } => {
            let db = get_db()?;
            let crosslink_dir = find_crosslink_dir()?;
            let writer = get_writer(&crosslink_dir);
            commands::intervene::run(
                &db,
                writer.as_ref(),
                id,
                &description,
                &trigger,
                context.as_deref(),
                &crosslink_dir,
            )
        }

        IssueCommands::Label { id, label } => {
            let db = get_db()?;
            let crosslink_dir = find_crosslink_dir()?;
            let writer = get_writer(&crosslink_dir);
            commands::label::add(&db, writer.as_ref(), id, &label)
        }

        IssueCommands::Unlabel { id, label } => {
            let db = get_db()?;
            let crosslink_dir = find_crosslink_dir()?;
            let writer = get_writer(&crosslink_dir);
            commands::label::remove(&db, writer.as_ref(), id, &label)
        }

        IssueCommands::Block { id, blocker } => {
            let db = get_db()?;
            let crosslink_dir = find_crosslink_dir()?;
            let writer = get_writer(&crosslink_dir);
            commands::deps::block(&db, writer.as_ref(), id, blocker)
        }

        IssueCommands::Unblock { id, blocker } => {
            let db = get_db()?;
            let crosslink_dir = find_crosslink_dir()?;
            let writer = get_writer(&crosslink_dir);
            commands::deps::unblock(&db, writer.as_ref(), id, blocker)
        }

        IssueCommands::Blocked => {
            let db = get_db()?;
            commands::deps::list_blocked(&db, json)
        }

        IssueCommands::Ready => {
            let db = get_db()?;
            commands::deps::list_ready(&db, json)
        }

        IssueCommands::Relate { id, related } => {
            let db = get_db()?;
            let crosslink_dir = find_crosslink_dir()?;
            let writer = get_writer(&crosslink_dir);
            commands::relate::add(&db, writer.as_ref(), id, related)
        }

        IssueCommands::Unrelate { id, related } => {
            let db = get_db()?;
            let crosslink_dir = find_crosslink_dir()?;
            let writer = get_writer(&crosslink_dir);
            commands::relate::remove(&db, writer.as_ref(), id, related)
        }

        IssueCommands::Related { id } => {
            let db = get_db()?;
            commands::relate::list(&db, id)
        }

        IssueCommands::Next => {
            let db = get_db()?;
            let crosslink_dir = find_crosslink_dir()?;
            commands::next::run(&db, &crosslink_dir)
        }

        IssueCommands::Tree { status } => {
            let db = get_db()?;
            commands::tree::run(&db, Some(&status), json)
        }

        IssueCommands::Tested => {
            let crosslink_dir = find_crosslink_dir()?;
            commands::tested::run(&crosslink_dir)
        }
    }
}

const CLI_STACK_SIZE: usize = 8 * 1024 * 1024;

fn main() -> Result<()> {
    let result = std::thread::Builder::new()
        .name("crosslink-main".to_string())
        .stack_size(CLI_STACK_SIZE)
        .spawn(run_cli)
        .context("failed to start Crosslink CLI thread")?
        .join();
    match result {
        Ok(result) => result,
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

fn run_cli() -> Result<()> {
    let cli = Cli::parse();

    let log_format = match &cli.command {
        Commands::Dashboard {
            action: DashboardCommands::Serve { .. },
        }
        | Commands::Serve { .. }
            if cli.log_format == "text" =>
        {
            "json"
        }
        _ => cli.log_format.as_str(),
    };
    init_tracing(&cli.log_level, log_format);

    match cli.command {
        Commands::Init {
            force,
            update,
            dry_run,
            no_prompt,
            python_prefix,
            skip_cpitd,
            skip_signing,
            signing_key,
            reconfigure,
            defaults,
            agent_integration,
        } => {
            let cwd = env::current_dir()?;
            let opts = commands::init::InitOpts {
                force,
                update,
                dry_run,
                no_prompt,
                python_prefix: python_prefix.as_deref(),
                skip_cpitd,
                skip_signing,
                signing_key: signing_key.as_deref(),
                reconfigure,
                defaults,
                integrations: agent_integration.parse()?,
            };
            commands::init::run(&cwd, &opts)
        }

        Commands::Issue { action } => dispatch_issue(action, cli.quiet, cli.json),

        Commands::Timer { action } => {
            let db = get_db()?;
            match action {
                TimerCommands::Start { id } => commands::timer::start(&db, id),
                TimerCommands::Stop => commands::timer::stop(&db),
                TimerCommands::Show => commands::timer::status(&db),
            }
        }

        Commands::Migrate { action } => match action {
            MigrateCommands::ToShared => {
                let crosslink_dir = find_crosslink_dir()?;
                let db = get_db()?;
                commands::migrate::to_shared(&crosslink_dir, &db)
            }
            MigrateCommands::FromShared => {
                let crosslink_dir = find_crosslink_dir()?;
                let db = get_db()?;
                commands::migrate::from_shared(&crosslink_dir, &db)
            }
            MigrateCommands::RenameBranch => {
                let crosslink_dir = find_crosslink_dir()?;
                commands::migrate::rename_branch(&crosslink_dir)
            }
            MigrateCommands::HubV3 {
                finalize,
                yes_delete_v2,
                adopt_stale,
                remigrate_from_v2,
            } => {
                let crosslink_dir = find_crosslink_dir()?;
                commands::migrate_hub_v3::hub_v3(
                    &crosslink_dir,
                    finalize,
                    yes_delete_v2,
                    adopt_stale,
                    remigrate_from_v2,
                )
            }
            MigrateCommands::HubBranches => {
                let crosslink_dir = find_crosslink_dir()?;
                commands::migrate_hub_v3::hub_branches(&crosslink_dir)
            }
        },

        Commands::Reconcile { check } => {
            anyhow::ensure!(check, "only read-only reconciliation checks are available");
            let crosslink_dir = find_crosslink_dir()?;
            let report = reconcile::check_repository(&crosslink_dir);
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!("{}", report.text());
            }
            Ok(())
        }

        Commands::Create {
            title,
            description,
            priority,
            template,
            label,
            work,
            defer_id,
            force,
            parent,
            scheduled,
            due,
        } => dispatch_issue(
            IssueCommands::Create {
                title,
                description,
                priority,
                template,
                label,
                work,
                defer_id,
                force,
                parent,
                scheduled,
                due,
            },
            cli.quiet,
            cli.json,
        ),

        Commands::Quick {
            title,
            description,
            priority,
            template,
            label,
            force,
            parent,
            scheduled,
            due,
        } => dispatch_issue(
            IssueCommands::Quick {
                title,
                description,
                priority,
                template,
                label,
                force,
                parent,
                scheduled,
                due,
            },
            cli.quiet,
            cli.json,
        ),

        Commands::List {
            status,
            label,
            priority,
        } => dispatch_issue(
            IssueCommands::List {
                status,
                label,
                priority,
                repo: None,
                refresh: false,
            },
            cli.quiet,
            cli.json,
        ),

        Commands::Show { id } => dispatch_issue(
            IssueCommands::Show {
                id,
                repo: None,
                refresh: false,
            },
            cli.quiet,
            cli.json,
        ),

        Commands::Close { id, no_changelog } => dispatch_issue(
            IssueCommands::Close { id, no_changelog },
            cli.quiet,
            cli.json,
        ),

        Commands::New {
            title,
            description,
            priority,
            template,
            label,
            work,
            parent,
        } => {
            hint(
                cli.quiet,
                "did you mean 'crosslink issue create'? Using that.",
            );
            dispatch_issue(
                IssueCommands::Create {
                    title,
                    description,
                    priority,
                    template,
                    label,
                    work,
                    defer_id: false,
                    force: false,
                    parent,
                    scheduled: None,
                    due: None,
                },
                cli.quiet,
                cli.json,
            )
        }

        Commands::Issues { action } => {
            hint(
                cli.quiet,
                "did you mean 'crosslink issue list'? Using that.",
            );
            if let Some(IssuesAliasCommands::List {
                status,
                label,
                priority,
            }) = action
            {
                dispatch_issue(
                    IssueCommands::List {
                        status,
                        label,
                        priority,
                        repo: None,
                        refresh: false,
                    },
                    cli.quiet,
                    cli.json,
                )
            } else {
                dispatch_issue(
                    IssueCommands::List {
                        status: "open".to_string(),
                        label: None,
                        priority: None,
                        repo: None,
                        refresh: false,
                    },
                    cli.quiet,
                    cli.json,
                )
            }
        }

        Commands::Subissue {
            parent,
            title,
            description,
            priority,
            template,
            label,
            work,
            force,
        } => {
            hint(
                cli.quiet,
                "did you mean 'crosslink issue create --parent'? Using that.",
            );
            dispatch_issue(
                IssueCommands::Create {
                    title,
                    description,
                    priority,
                    template,
                    label,
                    work,
                    defer_id: false,
                    force,
                    parent: Some(parent),
                    scheduled: None,
                    due: None,
                },
                cli.quiet,
                cli.json,
            )
        }

        Commands::TimerStart { id } => {
            hint(
                cli.quiet,
                "did you mean 'crosslink timer start'? Using that.",
            );
            let db = get_db()?;
            commands::timer::start(&db, id)
        }

        Commands::TimerStop => {
            hint(
                cli.quiet,
                "did you mean 'crosslink timer stop'? Using that.",
            );
            let db = get_db()?;
            commands::timer::stop(&db)
        }

        Commands::MigrateToShared => {
            hint(
                cli.quiet,
                "did you mean 'crosslink migrate to-shared'? Using that.",
            );
            let crosslink_dir = find_crosslink_dir()?;
            let db = get_db()?;
            commands::migrate::to_shared(&crosslink_dir, &db)
        }

        Commands::MigrateFromShared => {
            hint(
                cli.quiet,
                "did you mean 'crosslink migrate from-shared'? Using that.",
            );
            let crosslink_dir = find_crosslink_dir()?;
            let db = get_db()?;
            commands::migrate::from_shared(&crosslink_dir, &db)
        }

        Commands::MigrateRenameBranch => {
            hint(
                cli.quiet,
                "did you mean 'crosslink migrate rename-branch'? Using that.",
            );
            let crosslink_dir = find_crosslink_dir()?;
            commands::migrate::rename_branch(&crosslink_dir)
        }

        Commands::Export { output, format } => {
            let db = get_db()?;
            match format.as_str() {
                "json" => commands::export::run_json(&db, output.as_deref()),
                "markdown" | "md" => commands::export::run_markdown(&db, output.as_deref()),
                _ => {
                    bail!("Unknown format '{format}'. Use 'json' or 'markdown'");
                }
            }
        }

        Commands::Import { input } => {
            let db = get_db()?;
            let crosslink_dir = find_crosslink_dir()?;
            let writer = get_writer(&crosslink_dir);
            let path = std::path::Path::new(&input);
            commands::import::run_json(&db, writer.as_ref(), path)
        }

        Commands::Archive { action } => {
            let db = get_db()?;
            commands::archive::run(action, &db)
        }

        Commands::Milestone { action } => {
            let db = get_db()?;
            let crosslink_dir = find_crosslink_dir()?;
            commands::milestone::run(action, &db, &crosslink_dir)
        }

        Commands::Session { action } => {
            let db = get_db()?;
            let crosslink_dir = find_crosslink_dir()?;
            commands::session::run(action, &db, &crosslink_dir, cli.json)
        }

        Commands::Daemon { action } => match action {
            DaemonCommands::Start => {
                let crosslink_dir = find_crosslink_dir()?;
                daemon::start(&crosslink_dir)
            }
            DaemonCommands::Stop => {
                let crosslink_dir = find_crosslink_dir()?;
                daemon::stop(&crosslink_dir)
            }
            DaemonCommands::Status => {
                let crosslink_dir = find_crosslink_dir()?;
                daemon::status(&crosslink_dir);
                Ok(())
            }
            DaemonCommands::Run { dir } => daemon::run_daemon(&dir),
        },

        Commands::Cpitd { action } => {
            let db = get_db()?;
            commands::cpitd::run(action, &db, cli.quiet)
        }

        Commands::Agent { action } => {
            let crosslink_dir = find_crosslink_dir()?;
            commands::agent::run(action, &crosslink_dir)
        }

        Commands::Trust { action } => {
            let crosslink_dir = find_crosslink_dir()?;
            commands::trust::run(action, &crosslink_dir)
        }

        Commands::Locks { action } => {
            let crosslink_dir = find_crosslink_dir()?;
            let db = get_db()?;
            commands::locks_cmd::run(action, &crosslink_dir, &db, cli.json)
        }

        Commands::Heartbeat => {
            let crosslink_dir = find_crosslink_dir()?;
            let agent = crate::identity::AgentConfig::load(&crosslink_dir)?;
            match agent {
                Some(agent) => {
                    let sync = crate::sync::SyncManager::new(&crosslink_dir)?;
                    let _ = sync.init_cache();
                    let db = get_db()?;
                    let active_issue = db
                        .get_current_session_for_agent(None)?
                        .and_then(|s| s.active_issue_id);
                    sync.push_heartbeat(&agent, active_issue)?;
                    Ok(())
                }
                None => Ok(()),
            }
        }

        Commands::Sync => {
            let crosslink_dir = find_crosslink_dir()?;
            let db = get_db()?;
            commands::locks_cmd::sync_cmd(&crosslink_dir, &db)
        }

        Commands::Integrity { action } => {
            let crosslink_dir = find_crosslink_dir()?;
            let db = get_db()?;
            commands::integrity_cmd::run(action.as_ref(), &crosslink_dir, &db)
        }

        Commands::Prune {
            dry_run,
            force,
            keep_commits,
            hub_only,
            knowledge_only,
        } => {
            let crosslink_dir = find_crosslink_dir()?;
            let opts = commands::prune::PruneOpts {
                dry_run,
                force,
                keep_commits,
                hub_only,
                knowledge_only,
            };
            commands::prune::run(&crosslink_dir, &opts, cli.json)
        }

        Commands::Compact { force } => {
            let crosslink_dir = find_crosslink_dir()?;
            let db = get_db()?;
            commands::compact::run(&crosslink_dir, &db, force)
        }

        Commands::Container { action } => commands::container::run(action),

        Commands::Style { command } => {
            let crosslink_dir = find_crosslink_dir()?;
            commands::style::run(command, &crosslink_dir)
        }

        Commands::Knowledge { command } => {
            let crosslink_dir = find_crosslink_dir()?;
            commands::knowledge::dispatch(command, &crosslink_dir, cli.json)
        }

        Commands::Config { command, preset } => {
            let crosslink_dir = find_crosslink_dir()?;
            command.map_or_else(
                || commands::config::run_bare(&crosslink_dir, preset.as_deref()),
                |cmd| commands::config::run(cmd, &crosslink_dir),
            )
        }
        Commands::Context { command } => {
            let crosslink_dir = find_crosslink_dir()?;
            commands::context::run(command, &crosslink_dir)
        }
        Commands::Workflow { command } => {
            let crosslink_dir = find_crosslink_dir()?;
            commands::workflow::run(command, &crosslink_dir, get_db)
        }

        Commands::Kickoff { action } => {
            let crosslink_dir = find_crosslink_dir()?;
            let db = get_db()?;
            let writer = get_writer(&crosslink_dir);

            let action = action.unwrap_or_else(|| KickoffCommands::Launch {
                doc: None,
                plan: false,
                run: false,
                verify: "local".to_string(),
                model: "standard".to_string(),
                timeout: "1h".to_string(),
                container: "none".to_string(),
                issue: None,
                dry_run: false,
                skip_permissions: false,
                permission_mode: None,
            });
            commands::kickoff::dispatch(
                action,
                &crosslink_dir,
                &db,
                writer.as_ref(),
                cli.quiet,
                cli.json,
            )
        }
        Commands::Doctor => {
            let crosslink_dir = find_crosslink_dir()?;
            commands::doctor::run(&crosslink_dir, cli.json)
        }
        Commands::Design {
            description,
            issue,
            gh_issue,
            continue_slug,
        } => {
            let crosslink_dir = find_crosslink_dir()?;
            commands::design_cmd::run(
                &crosslink_dir,
                description.as_deref(),
                issue,
                gh_issue,
                continue_slug.as_deref(),
            )
        }
        Commands::Swarm { action } => {
            let crosslink_dir = find_crosslink_dir()?;
            match action {
                SwarmCommands::Init { doc } => commands::swarm::init(&crosslink_dir, &doc),
                SwarmCommands::Status => commands::swarm::status(&crosslink_dir, cli.json),
                SwarmCommands::Resume => commands::swarm::resume(&crosslink_dir),
                SwarmCommands::SyncStatus => commands::swarm::sync_status(&crosslink_dir),
                SwarmCommands::Adopt { agent, slot } => {
                    commands::swarm::adopt(&crosslink_dir, &agent, &slot)
                }
                SwarmCommands::Archive => commands::swarm::archive(&crosslink_dir),
                SwarmCommands::Reset { no_archive } => {
                    commands::swarm::reset(&crosslink_dir, no_archive)
                }
                SwarmCommands::ListSwarms => commands::swarm::list_swarms(&crosslink_dir),
                SwarmCommands::Launch {
                    phase,
                    budget_aware,
                    retry_failed,
                } => {
                    let db = get_db()?;
                    let writer = get_writer(&crosslink_dir);
                    if retry_failed {
                        commands::swarm::launch_retry_failed(
                            &crosslink_dir,
                            &db,
                            writer.as_ref(),
                            &phase,
                            cli.quiet,
                        )
                    } else if budget_aware {
                        commands::swarm::launch_budget_aware(
                            &crosslink_dir,
                            &db,
                            writer.as_ref(),
                            &phase,
                            cli.quiet,
                        )
                    } else {
                        commands::swarm::launch(
                            &crosslink_dir,
                            &db,
                            writer.as_ref(),
                            &phase,
                            cli.quiet,
                        )
                    }
                }
                SwarmCommands::Gate { phase } => commands::swarm::gate(&crosslink_dir, &phase),
                SwarmCommands::Checkpoint {
                    phase,
                    notes,
                    force,
                } => commands::swarm::checkpoint(&crosslink_dir, &phase, notes.as_deref(), force),
                SwarmCommands::Config {
                    budget_window,
                    model,
                    effort,
                    budget_usd,
                } => commands::swarm::config_budget(
                    &crosslink_dir,
                    &budget_window,
                    &model,
                    effort.as_deref(),
                    budget_usd.as_deref(),
                ),
                SwarmCommands::Estimate { phase } => {
                    commands::swarm::estimate(&crosslink_dir, &phase)
                }
                SwarmCommands::Harvest => commands::swarm::harvest_costs(&crosslink_dir),
                SwarmCommands::Plan { budget_window } => {
                    commands::swarm::plan(&crosslink_dir, budget_window.as_deref())
                }
                SwarmCommands::PlanShow => commands::swarm::plan_show(&crosslink_dir),
                SwarmCommands::Review {
                    agents,
                    mandate,
                    doc,
                    file_issues,
                    fix,
                } => commands::swarm::review(
                    &crosslink_dir,
                    agents,
                    &mandate,
                    doc.as_deref(),
                    file_issues,
                    fix,
                ),
                SwarmCommands::Fix {
                    issues,
                    from_label,
                    max_agents,
                    budget_aware,
                } => commands::swarm::fix(
                    &crosslink_dir,
                    issues.as_deref(),
                    from_label.as_deref(),
                    max_agents,
                    budget_aware,
                ),
                SwarmCommands::Merge {
                    branch,
                    base,
                    dry_run,
                    agents,
                } => commands::swarm::merge(
                    &crosslink_dir,
                    &branch,
                    base.as_deref(),
                    dry_run,
                    agents.as_deref(),
                ),
                SwarmCommands::MoveAgent { agent, to_phase } => {
                    commands::swarm::move_agent(&crosslink_dir, &agent, &to_phase)
                }
                SwarmCommands::MergePhases { phase_a, phase_b } => {
                    commands::swarm::merge_phases(&crosslink_dir, &phase_a, &phase_b)
                }
                SwarmCommands::SplitPhase { phase, after } => {
                    commands::swarm::split_phase(&crosslink_dir, &phase, &after)
                }
                SwarmCommands::RemoveAgent { agent } => {
                    commands::swarm::remove_agent(&crosslink_dir, &agent)
                }
                SwarmCommands::Reorder { phase, position } => {
                    commands::swarm::reorder_phase(&crosslink_dir, &phase, position)
                }
                SwarmCommands::RenamePhase { old, new } => {
                    commands::swarm::rename_phase(&crosslink_dir, &old, &new)
                }
                SwarmCommands::ReviewContinue => commands::swarm::review_continue(&crosslink_dir),
                SwarmCommands::ReviewStatus => commands::swarm::review_status(&crosslink_dir),
                SwarmCommands::Pipeline {
                    agents,
                    mandate,
                    target_branch,
                    auto_fix,
                    auto_file_issues,
                } => commands::swarm::run_pipeline_cmd(
                    &crosslink_dir,
                    agents,
                    &mandate,
                    &target_branch,
                    auto_fix,
                    auto_file_issues,
                ),
                SwarmCommands::TrustInit { model } => {
                    commands::swarm::trust_init(&crosslink_dir, &model)
                }
            }
        }
        Commands::Sentinel { action } => {
            if let SentinelCommands::RunDaemon {
                ref dir,
                interval,
                ref model,
            } = action
            {
                return commands::sentinel::watch::run_watch_loop(dir, interval, model.clone());
            }
            let crosslink_dir = find_crosslink_dir()?;
            let db = get_db()?;
            let writer = get_writer(&crosslink_dir);
            let sentinel_model = match &action {
                SentinelCommands::Run { model, .. }
                | SentinelCommands::Watch { model, .. }
                | SentinelCommands::RunDaemon { model, .. } => model.clone(),
                _ => None,
            };
            commands::sentinel::dispatch_cmd(
                action,
                &crosslink_dir,
                &db,
                writer.as_ref(),
                cli.quiet,
                cli.json,
                sentinel_model,
            )
        }
        Commands::Tui => {
            let db = get_db()?;
            let crosslink_dir = find_crosslink_dir()?;
            commands::tui::run(&db, &crosslink_dir)
        }
        Commands::Mc { layout } => {
            let crosslink_dir = find_crosslink_dir()?;
            commands::mission_control::run(&crosslink_dir, &layout)
        }
        Commands::Dashboard { action } => match action {
            DashboardCommands::Serve {
                port,
                dashboard_dir,
                rotate_token,
            } => {
                let crosslink_dir = find_crosslink_dir()?;
                let db = get_db()?;

                let dashboard_db_path = dashboard::db::DashboardDb::default_path()?;
                dashboard::db::DashboardDb::open(&dashboard_db_path)?;
                if rotate_token {
                    let _ = crate::server::state::rotate_auth_token();
                    println!("auth token rotated at ~/.crosslink/.dashboard-token");
                }

                tokio::runtime::Runtime::new()?.block_on(server::run_with_dashboard_db(
                    port,
                    dashboard_dir,
                    db,
                    crosslink_dir,
                    Some(dashboard_db_path),
                ))
            }
            DashboardCommands::Track {
                path,
                slug,
                init,
                agent_id,
            } => {
                if init {
                    let id = agent_id.ok_or_else(|| {
                        anyhow::anyhow!(
                            "--init requires --agent-id <ID> \
                             (alphanumeric, hyphens, underscores)"
                        )
                    })?;
                    dashboard::projects::track_with_init(&path, slug.as_deref(), &id)
                } else {
                    dashboard::projects::track(&path, slug.as_deref())
                }
            }
            DashboardCommands::Untrack { slug } => dashboard::projects::untrack(&slug),
            DashboardCommands::List => dashboard::projects::list(),
            DashboardCommands::Discover { root, depth, track } => {
                let opts = if root.is_empty() {
                    let mut o = dashboard::discover::DiscoverOptions::defaults();
                    o.depth = depth;
                    o
                } else {
                    dashboard::discover::DiscoverOptions { roots: root, depth }
                };
                let db_path = dashboard::db::DashboardDb::default_path()?;
                let db = dashboard::db::DashboardDb::open(&db_path)?;
                let hits = dashboard::discover::discover(&db, &opts)?;
                if hits.is_empty() {
                    println!(
                        "No crosslink-enabled repos found under: {}",
                        opts.roots
                            .iter()
                            .map(|p| p.display().to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                    return Ok(());
                }
                println!("{:<40} {:<10} PATH", "SLUG", "TRACKED?");
                for h in &hits {
                    println!(
                        "{:<40} {:<10} {}",
                        h.slug,
                        if h.already_tracked { "yes" } else { "no" },
                        h.path.display()
                    );
                }
                if track {
                    let to_track: Vec<_> = hits.iter().filter(|h| !h.already_tracked).collect();
                    if to_track.is_empty() {
                        println!("\nAll {} hits already tracked.", hits.len());
                    } else {
                        println!("\nTracking {} new repo(s)…", to_track.len());
                        for h in to_track {
                            if let Err(e) = dashboard::projects::track(&h.path, None) {
                                eprintln!("  {}: {e}", h.slug);
                            }
                        }
                    }
                }
                Ok(())
            }
        },
        Commands::Serve {
            port,
            dashboard_dir,
        } => {
            eprintln!(
                "warning: `crosslink serve` is deprecated. \
                 Use `crosslink dashboard` instead. \
                 See https://github.com/Corvidae-Coding-Projects/crosslink/issues/429. \
                 This alias will be removed in a future release."
            );
            let crosslink_dir = find_crosslink_dir()?;
            let db = get_db()?;
            tokio::runtime::Runtime::new()?.block_on(server::run(
                port,
                dashboard_dir,
                db,
                crosslink_dir,
            ))
        }
    }
}

#[cfg(test)]
mod find_crosslink_dir_tests {
    use super::find_crosslink_dir_from;

    fn mk_initialized(dir: &std::path::Path) {
        let cl = dir.join(".crosslink");
        std::fs::create_dir_all(&cl).unwrap();
        std::fs::write(cl.join("hook-config.json"), "{}").unwrap();
    }

    fn mk_stray(dir: &std::path::Path) {
        let cl = dir.join(".crosslink");
        std::fs::create_dir_all(cl.join(".cache")).unwrap();
        std::fs::write(cl.join("issues.db"), b"sqlite").unwrap();
    }

    #[test]
    fn binds_initialized_dir_directly() {
        let root = tempfile::tempdir().unwrap();
        mk_initialized(root.path());
        let found = find_crosslink_dir_from(root.path()).unwrap();
        assert_eq!(found, root.path().join(".crosslink"));
    }

    #[test]
    fn skips_stray_in_subdir_and_binds_root() {
        let root = tempfile::tempdir().unwrap();
        mk_initialized(root.path());
        let web = root.path().join("web");
        std::fs::create_dir_all(&web).unwrap();
        mk_stray(&web);

        let found = find_crosslink_dir_from(&web).unwrap();
        assert_eq!(
            found,
            root.path().join(".crosslink"),
            "must bind the initialized root, not the stray"
        );
    }

    #[test]
    fn stray_only_errors_naming_the_stray() {
        let root = tempfile::tempdir().unwrap();
        let web = root.path().join("web");
        std::fs::create_dir_all(&web).unwrap();
        mk_stray(&web);

        let err = find_crosslink_dir_from(&web).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("stray") && msg.contains("GH#625"),
            "error must explain the stray, got: {msg}"
        );
        assert!(
            msg.contains(&web.join(".crosslink").display().to_string()),
            "error must name the stray path, got: {msg}"
        );
    }

    #[test]
    fn nested_initialized_dir_still_wins_over_parent() {
        let root = tempfile::tempdir().unwrap();
        mk_initialized(root.path());
        let sub = root.path().join("subproject");
        std::fs::create_dir_all(&sub).unwrap();
        mk_initialized(&sub);

        let found = find_crosslink_dir_from(&sub).unwrap();
        assert_eq!(found, sub.join(".crosslink"));
    }
}
