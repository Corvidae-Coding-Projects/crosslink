use anyhow::Result;
use std::path::Path;

use crate::application::{CommandService, QueryService, RepositoryService};
use crate::db::Database;
use crate::hydration::hydrate_to_sqlite;
use crate::identity::AgentConfig;
use crate::sync::SyncManager;
use crate::utils::{format_issue_id, truncate};
use crate::LocksCommands;

pub fn run(command: LocksCommands, crosslink_dir: &Path, db: &Database, json: bool) -> Result<()> {
    match command {
        LocksCommands::List => {
            let queries = RepositoryService::projection(db);
            list(crosslink_dir, &queries, json)
        }
        LocksCommands::Check { id } => check(crosslink_dir, id),
        LocksCommands::Claim { id, branch } => {
            let service = RepositoryService::new(db, crosslink_dir)?;
            claim(&service, crosslink_dir, id, branch.as_deref())
        }
        LocksCommands::Release { id } => {
            let service = RepositoryService::new(db, crosslink_dir)?;
            release(&service, crosslink_dir, id)
        }
        LocksCommands::Steal { id } => {
            let service = RepositoryService::new(db, crosslink_dir)?;
            steal(&service, crosslink_dir, id)
        }
    }
}

pub fn list(crosslink_dir: &Path, db: &impl QueryService, json_output: bool) -> Result<()> {
    let sync = SyncManager::new(crosslink_dir)?;
    anyhow::ensure!(
        sync.is_initialized(),
        "Hub cache is unavailable. Retry after repository readiness is restored."
    );

    let locks_file = sync.read_locks_auto()?;

    if json_output {
        let json = serde_json::to_string_pretty(&locks_file)?;
        println!("{json}");
        return Ok(());
    }

    if locks_file.locks.is_empty() {
        println!("No active locks.");
        return Ok(());
    }

    let stale = sync.find_stale_locks()?;
    let stale_ids: Vec<i64> = stale.iter().map(|(id, _)| *id).collect();

    println!("Active locks:");
    for (&issue_id, lock) in &locks_file.locks {
        let title = db
            .get_issue(issue_id)?
            .map_or_else(|| "(unknown issue)".to_string(), |i| truncate(&i.title, 40));

        let stale_marker = if stale_ids.contains(&issue_id) {
            " [STALE]"
        } else {
            ""
        };

        println!(
            "  {:<5} {} -- claimed by {} on {}{}",
            format_issue_id(issue_id),
            title,
            lock.agent_id,
            lock.claimed_at.format("%Y-%m-%d %H:%M"),
            stale_marker
        );
        if let Some(branch) = &lock.branch {
            println!("         branch: {branch}");
        }
    }
    Ok(())
}

pub fn check(crosslink_dir: &Path, issue_id: i64) -> Result<()> {
    let sync = SyncManager::new(crosslink_dir)?;
    anyhow::ensure!(
        sync.is_initialized(),
        "Hub cache is unavailable. Retry after repository readiness is restored."
    );

    let locks_file = sync.read_locks_auto()?;

    match locks_file.get_lock(issue_id) {
        Some(lock) => {
            println!(
                "Issue {} is locked by '{}' (claimed {})",
                format_issue_id(issue_id),
                lock.agent_id,
                lock.claimed_at.format("%Y-%m-%d %H:%M")
            );
            if let Some(branch) = &lock.branch {
                println!("  Branch: {branch}");
            }

            let stale = sync.find_stale_locks()?;
            if stale.iter().any(|(id, _)| *id == issue_id) {
                println!("  Warning: this lock appears STALE (no recent heartbeat)");
            }
        }
        None => {
            println!(
                "Issue {} is not locked. Available for claiming.",
                format_issue_id(issue_id)
            );
        }
    }
    Ok(())
}

pub fn claim(
    commands: &impl CommandService,
    crosslink_dir: &Path,
    issue_id: i64,
    branch: Option<&str>,
) -> Result<()> {
    let agent = AgentConfig::load(crosslink_dir)?.ok_or_else(|| {
        anyhow::anyhow!("No agent configured. Run 'crosslink agent init <id>' first.")
    })?;

    let sync = SyncManager::new(crosslink_dir)?;
    sync.init_cache()?;
    sync.fetch()?;
    let _ = &agent;

    use crate::shared_writer::LockClaimResult;
    match commands.claim_lock(issue_id, branch)? {
        LockClaimResult::Claimed => {
            println!("Claimed lock on issue {}", format_issue_id(issue_id));
            if let Some(b) = branch {
                println!("  Branch: {b}");
            }
        }
        LockClaimResult::AlreadyHeld => {
            println!(
                "You already hold the lock on issue {}",
                format_issue_id(issue_id)
            );
        }
        LockClaimResult::Contended { winner_agent_id } => {
            anyhow::bail!(
                "Lock on issue {} was won by agent '{}'",
                format_issue_id(issue_id),
                winner_agent_id
            );
        }
    }
    Ok(())
}

pub fn release(commands: &impl CommandService, crosslink_dir: &Path, issue_id: i64) -> Result<()> {
    let agent = AgentConfig::load(crosslink_dir)?.ok_or_else(|| {
        anyhow::anyhow!("No agent configured. Run 'crosslink agent init <id>' first.")
    })?;

    let sync = SyncManager::new(crosslink_dir)?;
    sync.init_cache()?;
    sync.fetch()?;
    let _ = &agent;

    if commands.release_lock(issue_id)? {
        println!("Released lock on issue {}", format_issue_id(issue_id));
    } else {
        println!("Issue {} was not locked.", format_issue_id(issue_id));
    }
    Ok(())
}

pub fn steal(commands: &impl CommandService, crosslink_dir: &Path, issue_id: i64) -> Result<()> {
    let agent = AgentConfig::load(crosslink_dir)?.ok_or_else(|| {
        anyhow::anyhow!("No agent configured. Run 'crosslink agent init <id>' first.")
    })?;

    let sync = SyncManager::new(crosslink_dir)?;
    sync.init_cache()?;
    sync.fetch()?;

    let locks = sync.read_locks_auto()?;
    if let Some(existing) = locks.get_lock(issue_id) {
        if existing.agent_id == agent.agent_id {
            println!(
                "You already hold the lock on issue {}",
                format_issue_id(issue_id)
            );
            return Ok(());
        }

        let stale_locks = sync.find_stale_locks()?;
        let is_stale = stale_locks.iter().any(|(id, _)| *id == issue_id);

        if !is_stale {
            tracing::warn!(
                "Lock on {} held by '{}' is NOT stale. Stealing anyway.",
                format_issue_id(issue_id),
                existing.agent_id
            );
        }

        commands.steal_lock(issue_id, &existing.agent_id, None)?;
        println!(
            "Stole lock on issue {} from '{}'",
            format_issue_id(issue_id),
            existing.agent_id
        );
    } else {
        use crate::shared_writer::LockClaimResult;
        match commands.claim_lock(issue_id, None)? {
            LockClaimResult::Claimed | LockClaimResult::AlreadyHeld => {}
            LockClaimResult::Contended { winner_agent_id } => {
                anyhow::bail!("Lock contended — won by '{winner_agent_id}'");
            }
        }
        println!(
            "Claimed lock on issue {} (was not locked)",
            format_issue_id(issue_id)
        );
    }

    Ok(())
}

pub fn sync_cmd(crosslink_dir: &Path, db: &Database) -> Result<()> {
    let sync = SyncManager::new(crosslink_dir)?;
    sync.init_cache()?;
    sync.fetch()?;

    if !sync.hub_mode().is_v3() {
        return sync_v2_readonly(crosslink_dir, db, &sync);
    }

    match sync.ensure_agent_key_published(crosslink_dir) {
        Ok(true) => println!("Published agent key to hub (deferred from agent init)."),
        Ok(false) => {}
        Err(e) => tracing::warn!("could not publish agent key: {}", e),
    }
    if let Err(e) = sync.configure_signing(crosslink_dir) {
        tracing::warn!("could not configure commit signing: {e} — commits will be unsigned");
    }

    let source = crate::hub_source::RefHubSource::new(sync.cache_path())?;
    let outcome = crate::compaction::reduce(&source)?;
    let stats = crate::hydration::hydrate_from_state(&outcome.state, db)?;
    crate::hydration::record_hydrated_ref_durable(crosslink_dir)?;
    if stats.issues > 0 {
        println!(
            "Hydrated {} issue(s), {} comment(s), {} dep(s), {} relation(s), {} milestone(s).",
            stats.issues, stats.comments, stats.dependencies, stats.relations, stats.milestones
        );
    }

    if let (Ok(service), Ok(Some(cfg))) = (
        RepositoryService::new(db, crosslink_dir),
        crate::identity::AgentConfig::load(crosslink_dir),
    ) {
        match crate::agent_requests::poll::process_pending(&service, crosslink_dir, &cfg.agent_id) {
            Ok(result) if !result.acted.is_empty() => {
                println!(
                    "Processed {} agent request(s) for {}.",
                    result.acted.len(),
                    cfg.agent_id
                );
            }
            Ok(_) => {}
            Err(e) => tracing::warn!("agent request poll failed: {e}"),
        }
    }

    println!("Cache: {}", sync.cache_path().display());

    report_locks(&sync)?;
    Ok(())
}

fn sync_v2_readonly(crosslink_dir: &Path, db: &Database, sync: &SyncManager) -> Result<()> {
    let stats = hydrate_to_sqlite(sync.cache_path(), db)?;
    crate::hydration::record_hydrated_ref_durable(crosslink_dir)?;
    if stats.issues > 0 {
        println!(
            "Hydrated {} issue(s), {} comment(s), {} dep(s), {} relation(s), {} milestone(s) \
             (read-only v2 inspection).",
            stats.issues, stats.comments, stats.dependencies, stats.relations, stats.milestones
        );
    }
    println!("Cache: {}", sync.cache_path().display());
    println!(
        "This hub uses the legacy v2 layout (read-only). Run `crosslink migrate hub-v3` to \
         migrate to the per-agent-ref layout; mutations are refused until you do."
    );
    report_locks(sync)?;
    Ok(())
}

fn report_locks(sync: &SyncManager) -> Result<()> {
    let locks_file = sync.read_locks_auto()?;
    println!("{} active lock(s).", locks_file.locks.len());

    let stale = sync.find_stale_locks()?;
    if !stale.is_empty() {
        println!("{} stale lock(s) detected:", stale.len());
        for (id, agent) in &stale {
            println!("  {} (held by {})", format_issue_id(*id), agent);
        }
    }
    Ok(())
}
