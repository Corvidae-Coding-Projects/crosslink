use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::db::Database;
use crate::hydration::hydrate_to_sqlite;
use crate::reconcile::migration::{activate_repository, RepositoryActivation};
pub use crate::reconcile::readiness::DaemonResponse;
use crate::reconcile::readiness::{
    self, DaemonIdentity, ReadinessDraft, ReadinessRecord, ReadinessState, READINESS_SCHEMA_VERSION,
};

const FLUSH_INTERVAL_SECS: u64 = 30;
const ENSURE_DEADLINE_SECS: u64 = 120;
const START_LOCK_DEADLINE_SECS: u64 = 5;
const START_POLL_MILLIS: u64 = 25;
const READINESS_POLL_MILLIS: u64 = 50;
const MAX_RETRY_SECS: u64 = 15;
const LEASE_LIVENESS_SWEEP_POLLS: u8 = 40;

pub const WAITING_EXIT_CODE: i32 = 20;
pub const BLOCKED_EXIT_CODE: i32 = 21;

struct FileLease {
    path: PathBuf,
    contents: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProcessLeaseOwner {
    repository_id: String,
    pid: u32,
    process_start: String,
    token: String,
}

impl Drop for FileLease {
    fn drop(&mut self) {
        if fs::read(&self.path).ok().as_deref() == Some(self.contents.as_slice()) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn hydrate_v3_tick(cache_dir: &Path, db: &Database) -> Result<crate::hydration::HydrationStats> {
    let source = crate::hub_source::RefHubSource::new(cache_dir)?;
    let outcome = crate::compaction::reduce(&source)?;
    crate::hydration::hydrate_from_state(&outcome.state, db)
}

pub fn start(crosslink_dir: &Path) -> Result<ReadinessRecord> {
    ensure(crosslink_dir, true)
}

pub fn ensure(crosslink_dir: &Path, wait_ready: bool) -> Result<ReadinessRecord> {
    ensure_with_deadline(
        crosslink_dir,
        wait_ready,
        Duration::from_secs(ENSURE_DEADLINE_SECS),
    )
}

fn ensure_with_deadline(
    crosslink_dir: &Path,
    wait_ready: bool,
    timeout: Duration,
) -> Result<ReadinessRecord> {
    validate_crosslink_dir(crosslink_dir)?;
    let identity = ensure_process(crosslink_dir)?;
    let deadline = Instant::now() + timeout;
    let mut last_liveness_probe = Instant::now()
        .checked_sub(Duration::from_secs(1))
        .unwrap_or_else(Instant::now);
    loop {
        if last_liveness_probe.elapsed() >= Duration::from_secs(1)
            && !readiness::daemon_identity_is_live(&identity)
        {
            bail!(
                "daemon process {} exited before reporting readiness",
                identity.pid
            );
        }
        if last_liveness_probe.elapsed() >= Duration::from_secs(1) {
            last_liveness_probe = Instant::now();
        }
        if let Some(record) = readiness::read_record(crosslink_dir)? {
            if (
                record.repository_id.as_str(),
                record.daemon_epoch.as_str(),
                record.daemon_pid,
            ) == (
                identity.repository_id.as_str(),
                identity.daemon_epoch.as_str(),
                identity.pid,
            ) && (!wait_ready || record.state.is_terminal())
            {
                readiness::validate_record(crosslink_dir, &record)?;
                return Ok(record);
            }
        }
        if Instant::now() >= deadline {
            bail!(
                "timed out waiting for daemon readiness after {} seconds",
                timeout.as_secs()
            );
        }
        thread::sleep(Duration::from_millis(READINESS_POLL_MILLIS));
    }
}

pub fn status(crosslink_dir: &Path) -> Result<Option<ReadinessRecord>> {
    let Some(identity) = active_identity(crosslink_dir)? else {
        return Ok(None);
    };
    let Some(record) = readiness::read_record(crosslink_dir)? else {
        return Ok(None);
    };
    anyhow::ensure!(
        (
            record.repository_id.as_str(),
            record.daemon_epoch.as_str(),
            record.daemon_pid,
        ) == (
            identity.repository_id.as_str(),
            identity.daemon_epoch.as_str(),
            identity.pid,
        ),
        "readiness belongs to a stale daemon epoch"
    );
    readiness::validate_record(crosslink_dir, &record)?;
    Ok(Some(record))
}

pub fn stop(crosslink_dir: &Path) -> Result<()> {
    let _start_lease = acquire_start_lease(crosslink_dir)?;
    let Some(identity) = readiness::read_daemon_identity(crosslink_dir)? else {
        println!("Daemon not running (no identity file)");
        return Ok(());
    };
    let run_path = crosslink_dir.join("daemon.run.lock");
    let run_contents = fs::read(&run_path).ok();
    let run_identity = run_contents
        .as_deref()
        .and_then(|bytes| serde_json::from_slice::<DaemonIdentity>(bytes).ok());
    let repository_id = readiness::repository_id(crosslink_dir)?;
    if identity.repository_id != repository_id {
        if let Some(contents) = run_contents {
            anyhow::ensure!(
                run_identity.as_ref() == Some(&identity),
                "refusing to remove a foreign daemon identity with a mismatched or malformed run lease"
            );
            remove_owned_lease(&run_path, &contents)?;
        }
        readiness::remove_daemon_identity_if(crosslink_dir, &identity)?;
        println!("Daemon already stopped (foreign copied identity removed)");
        return Ok(());
    }
    anyhow::ensure!(
        identity.schema_version == READINESS_SCHEMA_VERSION,
        "refusing to stop an unsupported daemon identity schema"
    );
    if !readiness::daemon_identity_is_live(&identity) {
        anyhow::ensure!(
            !readiness::is_process_running(identity.pid),
            "refusing to remove daemon identity because PID {} was reused",
            identity.pid
        );
        if let Some(contents) = run_contents {
            anyhow::ensure!(
                run_identity.as_ref() == Some(&identity),
                "refusing to remove a stale daemon identity with a mismatched or malformed run lease"
            );
            remove_owned_lease(&run_path, &contents)?;
        }
        readiness::remove_daemon_identity_if(crosslink_dir, &identity)?;
        println!("Daemon already stopped (stale identity removed)");
        return Ok(());
    }
    anyhow::ensure!(
        run_identity.as_ref() == Some(&identity),
        "refusing to stop a process without a matching live daemon lease"
    );
    kill_process(identity.pid)?;
    if !wait_for_identity_exit(&identity, Duration::from_secs(5)) {
        anyhow::ensure!(
            readiness::daemon_identity_is_live(&identity),
            "daemon PID was reused before forced shutdown"
        );
        kill_process_force(identity.pid)?;
        anyhow::ensure!(
            wait_for_identity_exit(&identity, Duration::from_secs(2)),
            "daemon process {} did not exit after forced shutdown",
            identity.pid
        );
    }
    readiness::remove_daemon_identity_if(crosslink_dir, &identity)?;
    println!("Daemon stopped (PID {})", identity.pid);
    Ok(())
}

pub fn run_daemon(crosslink_dir: &Path, requested_epoch: Option<&str>) -> Result<()> {
    validate_crosslink_dir(crosslink_dir)?;
    let epoch = requested_epoch.map_or_else(|| Uuid::new_v4().to_string(), str::to_string);
    let identity = DaemonIdentity {
        schema_version: READINESS_SCHEMA_VERSION,
        repository_id: readiness::repository_id(crosslink_dir)?,
        daemon_epoch: epoch,
        pid: std::process::id(),
        process_start: readiness::current_process_start_token()?,
    };
    let run_lease = acquire_run_lease(crosslink_dir, &identity)?;
    readiness::write_daemon_identity(crosslink_dir, &identity)?;
    let should_exit = install_shutdown_handlers();
    let attempt_id = Uuid::new_v4().to_string();
    readiness::write_record(
        crosslink_dir,
        ReadinessDraft {
            daemon_epoch: &identity.daemon_epoch,
            daemon_pid: identity.pid,
            attempt_id: &attempt_id,
            state: ReadinessState::Starting,
            generation_id: None,
            reason: None,
        },
    )?;
    let ready = reconcile_until_ready(crosslink_dir, &identity, &should_exit)?;
    if ready {
        run_normal_loop(crosslink_dir, &should_exit)?;
    }
    drop(run_lease);
    Ok(())
}

fn ensure_process(crosslink_dir: &Path) -> Result<DaemonIdentity> {
    if let Some(identity) = active_identity(crosslink_dir)? {
        return Ok(identity);
    }
    let _lease = acquire_start_lease(crosslink_dir)?;
    if let Some(identity) = active_identity(crosslink_dir)? {
        return Ok(identity);
    }
    if let Some(identity) = readiness::read_daemon_identity(crosslink_dir)? {
        if identity.daemon_epoch == "legacy"
            && identity.process_start.is_empty()
            && readiness::is_process_running(identity.pid)
        {
            retire_legacy_daemon(crosslink_dir, identity.pid)?;
        }
        readiness::remove_daemon_identity_if(crosslink_dir, &identity)?;
    }
    let epoch = Uuid::new_v4().to_string();
    let executable = std::env::current_exe().context("resolving crosslink executable")?;
    let log_path = crosslink_dir.join("daemon.log");
    let stdout = fs::File::create(&log_path)
        .with_context(|| format!("creating daemon log {}", log_path.display()))?;
    let stderr = stdout.try_clone().context("cloning daemon log handle")?;
    let mut command = Command::new(executable);
    command
        .args(["daemon", "run", "--dir"])
        .arg(crosslink_dir)
        .args(["--epoch", &epoch])
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr);
    detach_daemon_command(&mut command);
    let child = command.spawn().context("spawning reconciliation daemon")?;
    let identity = DaemonIdentity {
        schema_version: READINESS_SCHEMA_VERSION,
        repository_id: readiness::repository_id(crosslink_dir)?,
        daemon_epoch: epoch,
        pid: child.id(),
        process_start: readiness::process_start_token_for(child.id())?,
    };
    readiness::write_daemon_identity(crosslink_dir, &identity)?;
    Ok(identity)
}

#[cfg(unix)]
fn detach_daemon_command(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    command.process_group(0);
}

#[cfg(windows)]
fn detach_daemon_command(command: &mut Command) {
    use std::os::windows::process::CommandExt;

    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
}

fn active_identity(crosslink_dir: &Path) -> Result<Option<DaemonIdentity>> {
    let Some(identity) = readiness::read_daemon_identity(crosslink_dir)? else {
        return Ok(None);
    };
    if identity.schema_version != READINESS_SCHEMA_VERSION
        || identity.repository_id != readiness::repository_id(crosslink_dir)?
        || !readiness::daemon_identity_is_live(&identity)
    {
        return Ok(None);
    }
    Ok(Some(identity))
}

fn acquire_start_lease(crosslink_dir: &Path) -> Result<FileLease> {
    let path = crosslink_dir.join("daemon.start.lock");
    let deadline = Instant::now() + Duration::from_secs(START_LOCK_DEADLINE_SECS);
    let owner = ProcessLeaseOwner {
        repository_id: readiness::repository_id(crosslink_dir)?,
        pid: std::process::id(),
        process_start: readiness::current_process_start_token()?,
        token: Uuid::new_v4().to_string(),
    };
    let contents = serde_json::to_vec(&owner).context("serializing daemon startup lease")?;
    let mut liveness_sweep = 0_u8;
    loop {
        match publish_lease(&path, &contents) {
            Ok(true) => return Ok(FileLease { path, contents }),
            Ok(false) => {
                if lease_liveness_probe_due(&mut liveness_sweep)
                    && remove_stale_start_lease(&path, &owner.repository_id)?
                {
                    continue;
                }
                if Instant::now() >= deadline {
                    bail!("timed out joining concurrent daemon startup");
                }
                thread::sleep(Duration::from_millis(START_POLL_MILLIS));
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error).context("acquiring daemon startup lease"),
        }
    }
}

const fn lease_liveness_probe_due(sweep: &mut u8) -> bool {
    let due = *sweep == 0;
    *sweep = (*sweep + 1) % LEASE_LIVENESS_SWEEP_POLLS;
    due
}

fn acquire_run_lease(crosslink_dir: &Path, identity: &DaemonIdentity) -> Result<FileLease> {
    let path = crosslink_dir.join("daemon.run.lock");
    let contents = serde_json::to_vec(identity).context("serializing daemon run lease")?;
    loop {
        match publish_lease(&path, &contents) {
            Ok(true) => return Ok(FileLease { path, contents }),
            Ok(false) => {
                let existing = fs::read(&path)
                    .with_context(|| format!("reading daemon run lease {}", path.display()))?;
                if let Ok(holder) = serde_json::from_slice::<DaemonIdentity>(&existing) {
                    if holder.repository_id != identity.repository_id {
                        remove_owned_lease(&path, &existing)?;
                        continue;
                    }
                    if readiness::daemon_identity_is_live(&holder) {
                        bail!("another daemon owns the repository run lease");
                    }
                    remove_owned_lease(&path, &existing)?;
                    continue;
                }
                if lease_is_fresh(&path, Duration::from_secs(1))? {
                    bail!("daemon run lease is incomplete and still fresh");
                }
                remove_owned_lease(&path, &existing)?;
            }
            Err(error) => return Err(error).context("acquiring daemon run lease"),
        }
    }
}

fn publish_lease(path: &Path, contents: &[u8]) -> std::io::Result<bool> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let temporary = parent.join(format!(".daemon-lease-{}.tmp", Uuid::new_v4()));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)?;
    file.write_all(contents)?;
    file.sync_all()?;
    let published = match fs::hard_link(&temporary, path) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(error) => {
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
    };
    if let Err(error) = fs::remove_file(&temporary) {
        if published && fs::read(path).ok().as_deref() == Some(contents) {
            let _ = fs::remove_file(path);
        }
        return Err(error);
    }
    if published {
        if let Err(error) = sync_lease_directory(parent) {
            if fs::read(path).ok().as_deref() == Some(contents) {
                let _ = fs::remove_file(path);
            }
            return Err(error);
        }
    }
    Ok(published)
}

fn remove_stale_start_lease(path: &Path, repository_id: &str) -> Result<bool> {
    let contents = fs::read(path)
        .with_context(|| format!("reading daemon startup lease {}", path.display()))?;
    if let Ok(owner) = serde_json::from_slice::<ProcessLeaseOwner>(&contents) {
        if owner.repository_id != repository_id {
            remove_owned_lease(path, &contents)?;
            return Ok(true);
        }
        if readiness::process_identity_is_live(owner.pid, &owner.process_start) {
            return Ok(false);
        }
        remove_owned_lease(path, &contents)?;
        return Ok(true);
    }
    if let Ok(pid) = String::from_utf8_lossy(&contents).trim().parse::<u32>() {
        if readiness::is_process_running(pid) {
            return Ok(false);
        }
        remove_owned_lease(path, &contents)?;
        return Ok(true);
    }
    if lease_is_fresh(path, Duration::from_secs(1))? {
        return Ok(false);
    }
    remove_owned_lease(path, &contents)?;
    Ok(true)
}

fn remove_owned_lease(path: &Path, contents: &[u8]) -> Result<()> {
    if fs::read(path).ok().as_deref() == Some(contents) {
        fs::remove_file(path)
            .with_context(|| format!("removing stale lease {}", path.display()))?;
    }
    Ok(())
}

fn lease_is_fresh(path: &Path, duration: Duration) -> Result<bool> {
    let modified = fs::metadata(path)?.modified()?;
    Ok(modified.elapsed().unwrap_or_default() < duration)
}

#[cfg(unix)]
fn sync_lease_directory(directory: &Path) -> std::io::Result<()> {
    fs::File::open(directory)?.sync_all()
}

#[cfg(windows)]
fn sync_lease_directory(directory: &Path) -> std::io::Result<()> {
    let _ = directory;
    Ok(())
}

fn reconcile_until_ready(
    crosslink_dir: &Path,
    identity: &DaemonIdentity,
    should_exit: &AtomicBool,
) -> Result<bool> {
    reconcile_until_ready_loop(crosslink_dir, identity, should_exit)
}

fn reconcile_until_ready_loop(
    crosslink_dir: &Path,
    identity: &DaemonIdentity,
    should_exit: &AtomicBool,
) -> Result<bool> {
    let mut retry_seconds = 1;
    loop {
        if should_exit.load(Ordering::SeqCst) {
            return Ok(false);
        }
        let attempt_id = Uuid::new_v4().to_string();
        let transition = readiness::acquire_transition_permit_observed(
            crosslink_dir,
            Some(should_exit),
            None,
            || {
                readiness::write_record(
                    crosslink_dir,
                    ReadinessDraft {
                        daemon_epoch: &identity.daemon_epoch,
                        daemon_pid: identity.pid,
                        attempt_id: &attempt_id,
                        state: ReadinessState::Reconciling,
                        generation_id: None,
                        reason: Some("waiting for active repository mutations to finish"),
                    },
                )
                .map(|_| ())
            },
        )?;
        readiness::write_record(
            crosslink_dir,
            ReadinessDraft {
                daemon_epoch: &identity.daemon_epoch,
                daemon_pid: identity.pid,
                attempt_id: &attempt_id,
                state: ReadinessState::Reconciling,
                generation_id: None,
                reason: None,
            },
        )?;
        let activation = match activate_repository(crosslink_dir) {
            Ok(activation) => activation,
            Err(error) => RepositoryActivation::BlockedCorrupt {
                reason: format!("{error:#}"),
            },
        };
        match activation {
            RepositoryActivation::ReadyCurrent { generation_id } => {
                record_ready(
                    crosslink_dir,
                    identity,
                    &attempt_id,
                    ReadinessState::ReadyCurrent,
                    &generation_id,
                )?;
                drop(transition);
                return Ok(true);
            }
            RepositoryActivation::ReadyMigrated { generation_id } => {
                record_ready(
                    crosslink_dir,
                    identity,
                    &attempt_id,
                    ReadinessState::ReadyMigrated,
                    &generation_id,
                )?;
                drop(transition);
                return Ok(true);
            }
            RepositoryActivation::ReadyAdopted { generation_id } => {
                record_ready(
                    crosslink_dir,
                    identity,
                    &attempt_id,
                    ReadinessState::ReadyAdopted,
                    &generation_id,
                )?;
                drop(transition);
                return Ok(true);
            }
            RepositoryActivation::WaitingForRemote { reason } => {
                readiness::write_record(
                    crosslink_dir,
                    ReadinessDraft {
                        daemon_epoch: &identity.daemon_epoch,
                        daemon_pid: identity.pid,
                        attempt_id: &attempt_id,
                        state: ReadinessState::WaitingForRemote,
                        generation_id: None,
                        reason: Some(&reason),
                    },
                )?;
                drop(transition);
                if wait_interruptible(should_exit, Duration::from_secs(retry_seconds)) {
                    return Ok(false);
                }
                retry_seconds = (retry_seconds * 2).min(MAX_RETRY_SECS);
            }
            RepositoryActivation::BlockedCorrupt { reason } => {
                readiness::write_record(
                    crosslink_dir,
                    ReadinessDraft {
                        daemon_epoch: &identity.daemon_epoch,
                        daemon_pid: identity.pid,
                        attempt_id: &attempt_id,
                        state: ReadinessState::BlockedCorrupt,
                        generation_id: None,
                        reason: Some(&reason),
                    },
                )?;
                drop(transition);
                while !wait_interruptible(should_exit, Duration::from_secs(1)) {}
                return Ok(false);
            }
        }
    }
}

fn record_ready(
    crosslink_dir: &Path,
    identity: &DaemonIdentity,
    attempt_id: &str,
    state: ReadinessState,
    generation_id: &str,
) -> Result<()> {
    crate::reconcile::migration::write_ready_activation(
        crosslink_dir,
        identity,
        attempt_id,
        state,
        generation_id,
    )
}

fn run_normal_loop(crosslink_dir: &Path, should_exit: &AtomicBool) -> Result<()> {
    let db_path = crosslink_dir.join("issues.db");
    let session_file = crosslink_dir.join("session.json");
    let mut heartbeat_counter = 0_u64;
    while !wait_interruptible(should_exit, Duration::from_secs(FLUSH_INTERVAL_SECS)) {
        let identity = readiness::read_daemon_identity(crosslink_dir)?
            .ok_or_else(|| anyhow::anyhow!("daemon identity disappeared"))?;
        let deferred = defer_reconciliation_for_active_mutations(crosslink_dir, &identity)?;
        if !deferred && !reconcile_until_ready(crosslink_dir, &identity, should_exit)? {
            return Ok(());
        }
        let mutation_permit = readiness::acquire_mutation_permit(crosslink_dir)?;
        let mut active_issue_id = None;
        match Database::open_read_only(&db_path) {
            Ok(db) => {
                let agent_id = crate::identity::AgentConfig::load(crosslink_dir)
                    .ok()
                    .flatten()
                    .map(|agent| agent.agent_id);
                if let Ok(Some(session)) = db.get_current_session_for_agent(agent_id.as_deref()) {
                    active_issue_id = session.active_issue_id;
                    let data = serde_json::json!({
                        "session_id": session.id,
                        "started_at": session.started_at.to_rfc3339(),
                        "active_issue_id": session.active_issue_id,
                    });
                    if let Ok(bytes) = serde_json::to_vec_pretty(&data) {
                        if let Err(error) = fs::write(&session_file, bytes) {
                            tracing::warn!("failed to write session file: {error}");
                        }
                    }
                }
            }
            Err(error) => tracing::warn!("failed to open database: {error}"),
        }
        drop(mutation_permit);
        heartbeat_counter += 1;
        if heartbeat_counter.is_multiple_of(5) {
            if let Err(error) = run_sync_tick(crosslink_dir, &db_path, active_issue_id) {
                tracing::warn!("daemon sync tick failed: {error}");
            }
        }
    }
    Ok(())
}

fn defer_reconciliation_for_active_mutations(
    crosslink_dir: &Path,
    identity: &DaemonIdentity,
) -> Result<bool> {
    defer_reconciliation_for_active_mutations_with(crosslink_dir, identity, || Ok(()))
}

fn defer_reconciliation_for_active_mutations_with<F>(
    crosslink_dir: &Path,
    identity: &DaemonIdentity,
    before_refresh_write: F,
) -> Result<bool>
where
    F: FnOnce() -> Result<()>,
{
    if !readiness::has_active_mutation_permits(crosslink_dir)? {
        return Ok(false);
    }
    if let Err(error) = refresh_ready_record_with(crosslink_dir, identity, before_refresh_write) {
        tracing::debug!("readiness refresh deferred behind active mutation: {error}");
    }
    Ok(true)
}

fn refresh_ready_record_with<F>(
    crosslink_dir: &Path,
    identity: &DaemonIdentity,
    before_write: F,
) -> Result<bool>
where
    F: FnOnce() -> Result<()>,
{
    let Some(record) = readiness::read_record(crosslink_dir)? else {
        return Ok(false);
    };
    if !record.state.grants_mutations()
        || record.daemon_epoch != identity.daemon_epoch
        || record.daemon_pid != identity.pid
        || readiness::validate_record(crosslink_dir, &record).is_err()
    {
        return Ok(false);
    }
    before_write()?;
    readiness::write_record(
        crosslink_dir,
        ReadinessDraft {
            daemon_epoch: &identity.daemon_epoch,
            daemon_pid: identity.pid,
            attempt_id: &record.attempt_id,
            state: record.state,
            generation_id: record.generation_id.as_deref(),
            reason: None,
        },
    )?;
    Ok(true)
}

fn run_sync_tick(crosslink_dir: &Path, db_path: &Path, active_issue_id: Option<i64>) -> Result<()> {
    let _permit = readiness::acquire_mutation_operation_permit(crosslink_dir)?;
    let Some(agent) = crate::identity::AgentConfig::load(crosslink_dir)? else {
        return Ok(());
    };
    let sync = crate::sync::SyncManager::new(crosslink_dir)?;
    let db = Database::open(db_path)?;
    sync.push_heartbeat(&agent, active_issue_id)?;
    if sync.hub_mode().is_v3() {
        hydrate_v3_tick(sync.cache_path(), &db)
    } else {
        hydrate_to_sqlite(sync.cache_path(), &db)
    }
    .context("hydrating after heartbeat publication")?;
    crate::hydration::record_hydrated_ref_durable(crosslink_dir)?;
    Ok(())
}

fn install_shutdown_handlers() -> Arc<AtomicBool> {
    let should_exit = Arc::new(AtomicBool::new(false));
    #[cfg(unix)]
    {
        if let Err(error) =
            signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&should_exit))
        {
            tracing::warn!("could not register SIGTERM handler: {error}");
        }
        if let Err(error) =
            signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&should_exit))
        {
            tracing::warn!("could not register SIGINT handler: {error}");
        }
    }
    should_exit
}

fn wait_interruptible(flag: &AtomicBool, duration: Duration) -> bool {
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        if flag.load(Ordering::SeqCst) {
            return true;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        thread::sleep(remaining.min(Duration::from_millis(100)));
    }
    flag.load(Ordering::SeqCst)
}

fn wait_for_identity_exit(identity: &DaemonIdentity, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !readiness::daemon_identity_is_live(identity) {
            return true;
        }
        thread::sleep(Duration::from_millis(250));
    }
    !readiness::daemon_identity_is_live(identity)
}

fn retire_legacy_daemon(crosslink_dir: &Path, pid: u32) -> Result<()> {
    let command = process_command_line(pid)
        .ok_or_else(|| anyhow::anyhow!("cannot verify live legacy daemon PID {pid}"))?;
    let process_start = readiness::process_start_token_for(pid)?;
    let canonical = crosslink_dir
        .canonicalize()
        .with_context(|| format!("canonicalizing {}", crosslink_dir.display()))?;
    anyhow::ensure!(
        command.contains("crosslink")
            && command.contains("daemon")
            && command.contains("run")
            && command.contains(&canonical.to_string_lossy().to_string()),
        "refusing to replace live legacy PID {pid} because it is not the repository daemon"
    );
    anyhow::ensure!(
        readiness::process_identity_is_live(pid, &process_start),
        "refusing to stop legacy PID {pid} after its process identity changed"
    );
    kill_process(pid)?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if !readiness::process_identity_is_live(pid, &process_start) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(250));
    }
    let current = process_command_line(pid);
    anyhow::ensure!(
        readiness::process_identity_is_live(pid, &process_start)
            && current.as_deref() == Some(command.as_str()),
        "refusing to force-stop legacy PID {pid} after its process identity changed"
    );
    kill_process_force(pid)?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if !readiness::process_identity_is_live(pid, &process_start) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(250));
    }
    bail!("legacy daemon PID {pid} remained live after forced termination")
}

#[cfg(target_os = "linux")]
fn process_command_line(pid: u32) -> Option<String> {
    let bytes = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    Some(
        String::from_utf8_lossy(&bytes)
            .replace('\0', " ")
            .trim()
            .to_string(),
    )
}

#[cfg(all(unix, not(target_os = "linux")))]
fn process_command_line(pid: u32) -> Option<String> {
    let output = Command::new("ps")
        .args(["-o", "command=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(windows)]
fn process_command_line(pid: u32) -> Option<String> {
    let expression =
        format!("(Get-CimInstance Win32_Process -Filter \"ProcessId={pid}\").CommandLine");
    let output = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &expression])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn validate_crosslink_dir(crosslink_dir: &Path) -> Result<()> {
    anyhow::ensure!(
        crosslink_dir.is_dir() && crosslink_dir.join("hook-config.json").is_file(),
        "invalid crosslink directory: {} is not an initialized repository",
        crosslink_dir.display()
    );
    Ok(())
}

#[cfg(windows)]
fn kill_process(pid: u32) -> Result<()> {
    let status = Command::new("taskkill")
        .args(["/PID", &pid.to_string()])
        .status()
        .context("stopping daemon process")?;
    anyhow::ensure!(status.success(), "taskkill failed for daemon PID {pid}");
    Ok(())
}

#[cfg(windows)]
fn kill_process_force(pid: u32) -> Result<()> {
    let status = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/F"])
        .status()
        .context("forcing daemon process shutdown")?;
    anyhow::ensure!(status.success(), "taskkill /F failed for daemon PID {pid}");
    Ok(())
}

#[cfg(not(windows))]
fn kill_process(pid: u32) -> Result<()> {
    let status = Command::new("kill")
        .arg(pid.to_string())
        .status()
        .context("stopping daemon process")?;
    anyhow::ensure!(status.success(), "kill failed for daemon PID {pid}");
    Ok(())
}

#[cfg(not(windows))]
fn kill_process_force(pid: u32) -> Result<()> {
    let status = Command::new("kill")
        .args(["-KILL", &pid.to_string()])
        .status()
        .context("forcing daemon process shutdown")?;
    anyhow::ensure!(status.success(), "SIGKILL failed for daemon PID {pid}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::SyncManager;
    use std::sync::{mpsc, Barrier};

    fn initialized() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        let crosslink = root.path().join(".crosslink");
        fs::create_dir(&crosslink).unwrap();
        fs::write(crosslink.join("hook-config.json"), "{}").unwrap();
        root
    }

    fn identity(repository_id: String, process_start: String) -> DaemonIdentity {
        DaemonIdentity {
            schema_version: READINESS_SCHEMA_VERSION,
            repository_id,
            daemon_epoch: Uuid::new_v4().to_string(),
            pid: std::process::id(),
            process_start,
        }
    }

    #[cfg(not(windows))]
    fn exited_pid() -> u32 {
        let mut child = Command::new("sh").args(["-c", "exit 0"]).spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        pid
    }

    fn run_git(directory: &Path, arguments: &[&str]) {
        let output = Command::new("git")
            .current_dir(directory)
            .args(arguments)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {arguments:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn connected() -> (tempfile::TempDir, tempfile::TempDir, PathBuf) {
        let remote = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        run_git(remote.path(), &["init", "--bare", "-b", "main"]);
        run_git(work.path(), &["init", "-b", "main"]);
        run_git(
            work.path(),
            &["config", "user.email", "test@example.invalid"],
        );
        run_git(work.path(), &["config", "user.name", "Test"]);
        fs::write(work.path().join("README.md"), "test").unwrap();
        run_git(work.path(), &["add", "README.md"]);
        run_git(work.path(), &["commit", "-m", "initial", "--no-gpg-sign"]);
        run_git(
            work.path(),
            &["remote", "add", "origin", remote.path().to_str().unwrap()],
        );
        run_git(work.path(), &["push", "-u", "origin", "main"]);
        let crosslink = work.path().join(".crosslink");
        fs::create_dir(&crosslink).unwrap();
        fs::write(crosslink.join("hook-config.json"), r#"{"remote":"origin"}"#).unwrap();
        crate::identity::AgentConfig::init(&crosslink, "test-agent", None).unwrap();
        (work, remote, crosslink)
    }

    fn blocked_attempt(crosslink: &Path) -> ReadinessRecord {
        let identity = identity(
            readiness::repository_id(crosslink).unwrap(),
            readiness::current_process_start_token().unwrap(),
        );
        readiness::write_daemon_identity(crosslink, &identity).unwrap();
        let should_exit = Arc::new(AtomicBool::new(false));
        let worker_dir = crosslink.to_path_buf();
        let worker_identity = identity;
        let worker_exit = Arc::clone(&should_exit);
        let worker = thread::spawn(move || {
            reconcile_until_ready_loop(&worker_dir, &worker_identity, &worker_exit)
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        let first = loop {
            if let Some(record) = readiness::read_record(crosslink).unwrap() {
                if record.state == ReadinessState::BlockedCorrupt {
                    break record;
                }
            }
            assert!(
                Instant::now() < deadline,
                "daemon did not publish blocked readiness"
            );
            thread::sleep(Duration::from_millis(10));
        };
        assert!(!worker.is_finished());
        let readiness_dir = crosslink.join("readiness");
        let record_count = readiness_record_count(&readiness_dir);
        let database = fs::read(crosslink.join("issues.db")).ok();
        let session = fs::read(crosslink.join("session.json")).ok();
        let refs = ref_snapshot(crosslink);
        thread::sleep(Duration::from_millis(75));
        let record = readiness::read_record(crosslink).unwrap().unwrap();
        assert_eq!(record, first);
        assert_eq!(readiness_record_count(&readiness_dir), record_count);
        assert_eq!(fs::read(crosslink.join("issues.db")).ok(), database);
        assert_eq!(fs::read(crosslink.join("session.json")).ok(), session);
        assert_eq!(ref_snapshot(crosslink), refs);
        assert!(!worker.is_finished());
        should_exit.store(true, Ordering::SeqCst);
        assert!(!worker.join().unwrap().unwrap());
        record
    }

    fn readiness_record_count(directory: &Path) -> usize {
        fs::read_dir(directory)
            .unwrap()
            .filter_map(std::result::Result::ok)
            .filter(|entry| {
                entry
                    .path()
                    .extension()
                    .is_some_and(|value| value == "json")
            })
            .count()
    }

    fn ref_snapshot(crosslink: &Path) -> Vec<u8> {
        let root = crosslink.parent().unwrap();
        let mut snapshot = Command::new("git")
            .current_dir(root)
            .args(["for-each-ref", "--format=%(refname):%(objectname)"])
            .output()
            .map(|output| output.stdout)
            .unwrap_or_default();
        if let Ok(sync) = SyncManager::new(crosslink) {
            if sync.cache_path().is_dir() {
                snapshot.extend(
                    Command::new("git")
                        .current_dir(sync.cache_path())
                        .args(["for-each-ref", "--format=%(refname):%(objectname)"])
                        .output()
                        .map(|output| output.stdout)
                        .unwrap_or_default(),
                );
            }
        }
        snapshot
    }

    fn ready_connected() -> (
        tempfile::TempDir,
        tempfile::TempDir,
        PathBuf,
        DaemonIdentity,
    ) {
        let (work, remote, crosslink) = connected();
        let db = Database::open(&crosslink.join("issues.db")).unwrap();
        db.create_issue("authority", None, "medium").unwrap();
        drop(db);
        crate::reconcile::migration::hub_v3(&crosslink, false, false, false, false).unwrap();
        let identity = identity(
            readiness::repository_id(&crosslink).unwrap(),
            readiness::current_process_start_token().unwrap(),
        );
        readiness::write_daemon_identity(&crosslink, &identity).unwrap();
        let activation = activate_repository(&crosslink).unwrap();
        let (state, generation) = match activation {
            RepositoryActivation::ReadyCurrent { generation_id } => {
                (ReadinessState::ReadyCurrent, generation_id)
            }
            RepositoryActivation::ReadyMigrated { generation_id } => {
                (ReadinessState::ReadyMigrated, generation_id)
            }
            RepositoryActivation::ReadyAdopted { generation_id } => {
                (ReadinessState::ReadyAdopted, generation_id)
            }
            other => panic!("unexpected activation: {other:?}"),
        };
        record_ready(&crosslink, &identity, "ready", state, &generation).unwrap();
        (work, remote, crosslink, identity)
    }

    #[cfg(windows)]
    fn exited_pid() -> u32 {
        let mut child = Command::new("cmd").args(["/C", "exit 0"]).spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        pid
    }

    #[test]
    fn bounded_wait_returns_immediately_for_shutdown() {
        let flag = AtomicBool::new(true);
        let started = Instant::now();
        assert!(wait_interruptible(&flag, Duration::from_secs(30)));
        assert!(started.elapsed() < Duration::from_millis(100));
    }

    #[test]
    fn readiness_exit_codes_are_distinct() {
        assert_ne!(WAITING_EXIT_CODE, BLOCKED_EXIT_CODE);
        assert_ne!(WAITING_EXIT_CODE, 0);
        assert_ne!(BLOCKED_EXIT_CODE, 0);
    }

    #[test]
    fn ready_record_requires_current_schema_frontier_and_descriptor_generation() {
        let (_work, _remote, crosslink, _identity) = ready_connected();
        let record = readiness::read_record(&crosslink).unwrap().unwrap();
        readiness::validate_record(&crosslink, &record).unwrap();

        let mut missing_frontier = record.clone();
        missing_frontier.projection_frontier = None;
        assert!(readiness::validate_record(&crosslink, &missing_frontier)
            .unwrap_err()
            .to_string()
            .contains("frontier"));

        let mut tampered_generation = record.clone();
        tampered_generation.generation_id = Some("0".repeat(32));
        assert!(readiness::validate_record(&crosslink, &tampered_generation)
            .unwrap_err()
            .to_string()
            .contains("generation identifier"));

        let old_schema = crate::db::SCHEMA_VERSION - 1;
        let connection = rusqlite::Connection::open(crosslink.join("issues.db")).unwrap();
        connection
            .pragma_update(None, "user_version", old_schema)
            .unwrap();
        drop(connection);
        let mut self_consistent_old_schema = record;
        self_consistent_old_schema.projection_schema_version = Some(old_schema);
        assert!(
            readiness::validate_record(&crosslink, &self_consistent_old_schema)
                .unwrap_err()
                .to_string()
                .contains("schema")
        );
    }

    #[test]
    fn copied_live_start_and_run_leases_are_replaced_without_signaling_owner() {
        let root = initialized();
        let crosslink = root.path().join(".crosslink");
        let process_start = readiness::current_process_start_token().unwrap();
        let foreign_start = ProcessLeaseOwner {
            repository_id: "foreign".to_string(),
            pid: std::process::id(),
            process_start: process_start.clone(),
            token: "foreign".to_string(),
        };
        fs::write(
            crosslink.join("daemon.start.lock"),
            serde_json::to_vec(&foreign_start).unwrap(),
        )
        .unwrap();
        let start = acquire_start_lease(&crosslink).unwrap();
        let start_owner: ProcessLeaseOwner =
            serde_json::from_slice(&fs::read(&start.path).unwrap()).unwrap();
        assert_eq!(
            start_owner.repository_id,
            readiness::repository_id(&crosslink).unwrap()
        );
        drop(start);

        let foreign = identity("foreign".to_string(), process_start.clone());
        fs::write(
            crosslink.join("daemon.run.lock"),
            serde_json::to_vec(&foreign).unwrap(),
        )
        .unwrap();
        let target = identity(readiness::repository_id(&crosslink).unwrap(), process_start);
        let run = acquire_run_lease(&crosslink, &target).unwrap();
        let owner: DaemonIdentity = serde_json::from_slice(&fs::read(&run.path).unwrap()).unwrap();
        assert_eq!(owner, target);
        drop(run);
    }

    #[test]
    fn fresh_malformed_run_lease_fails_closed() {
        let root = initialized();
        let crosslink = root.path().join(".crosslink");
        let path = crosslink.join("daemon.run.lock");
        fs::write(&path, b"{").unwrap();
        let target = identity(
            readiness::repository_id(&crosslink).unwrap(),
            readiness::current_process_start_token().unwrap(),
        );
        let error = acquire_run_lease(&crosslink, &target).err().unwrap();
        assert!(error.to_string().contains("incomplete"));
        assert_eq!(fs::read(path).unwrap(), b"{");
    }

    #[test]
    fn lease_liveness_checks_are_bounded_by_poll_cadence() {
        let mut sweep = 0;
        let probes = (0..400)
            .filter(|_| lease_liveness_probe_due(&mut sweep))
            .count();
        assert_eq!(probes, 10);
    }

    #[test]
    fn simultaneous_run_lease_claim_has_exactly_one_live_owner() {
        let root = initialized();
        let crosslink = root.path().join(".crosslink");
        let repository_id = readiness::repository_id(&crosslink).unwrap();
        let process_start = readiness::current_process_start_token().unwrap();
        let barrier = Arc::new(Barrier::new(3));
        let (result_tx, result_rx) = mpsc::sync_channel(2);
        let (release_tx, release_rx) = mpsc::channel();
        let release_rx = Arc::new(std::sync::Mutex::new(release_rx));
        let mut workers = Vec::new();
        for epoch in ["first", "second"] {
            let worker_dir = crosslink.clone();
            let worker_barrier = Arc::clone(&barrier);
            let worker_tx = result_tx.clone();
            let worker_release = Arc::clone(&release_rx);
            let worker_identity = DaemonIdentity {
                schema_version: READINESS_SCHEMA_VERSION,
                repository_id: repository_id.clone(),
                daemon_epoch: epoch.to_string(),
                pid: std::process::id(),
                process_start: process_start.clone(),
            };
            workers.push(thread::spawn(move || {
                worker_barrier.wait();
                match acquire_run_lease(&worker_dir, &worker_identity) {
                    Ok(lease) => {
                        worker_tx.send(true).unwrap();
                        worker_release.lock().unwrap().recv().unwrap();
                        drop(lease);
                    }
                    Err(_) => worker_tx.send(false).unwrap(),
                }
            }));
        }
        barrier.wait();
        let results = [
            result_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            result_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
        ];
        assert_eq!(results.into_iter().filter(|won| *won).count(), 1);
        release_tx.send(()).unwrap();
        for worker in workers {
            worker.join().unwrap();
        }
    }

    #[test]
    fn abandoned_unpublished_lease_temporary_does_not_block_restart() {
        let root = initialized();
        let crosslink = root.path().join(".crosslink");
        fs::write(crosslink.join(".daemon-lease-crashed.tmp"), b"{").unwrap();
        let start = acquire_start_lease(&crosslink).unwrap();
        drop(start);
        let target = identity(
            readiness::repository_id(&crosslink).unwrap(),
            readiness::current_process_start_token().unwrap(),
        );
        let run = acquire_run_lease(&crosslink, &target).unwrap();
        drop(run);
    }

    #[test]
    fn stop_removes_exact_crashed_identity_and_matching_lease() {
        let root = initialized();
        let crosslink = root.path().join(".crosslink");
        let stale = DaemonIdentity {
            schema_version: READINESS_SCHEMA_VERSION,
            repository_id: readiness::repository_id(&crosslink).unwrap(),
            daemon_epoch: "stale".to_string(),
            pid: exited_pid(),
            process_start: "gone".to_string(),
        };
        readiness::write_daemon_identity(&crosslink, &stale).unwrap();
        fs::write(
            crosslink.join("daemon.run.lock"),
            serde_json::to_vec(&stale).unwrap(),
        )
        .unwrap();
        stop(&crosslink).unwrap();
        assert!(!crosslink.join("daemon.pid").exists());
        assert!(!crosslink.join("daemon.run.lock").exists());
    }

    #[test]
    fn stop_refuses_reused_pid_and_preserves_identity() {
        let root = initialized();
        let crosslink = root.path().join(".crosslink");
        let reused = identity(
            readiness::repository_id(&crosslink).unwrap(),
            "different-start".to_string(),
        );
        readiness::write_daemon_identity(&crosslink, &reused).unwrap();
        fs::write(
            crosslink.join("daemon.run.lock"),
            serde_json::to_vec(&reused).unwrap(),
        )
        .unwrap();
        let error = stop(&crosslink).unwrap_err();
        assert!(error.to_string().contains("PID") && error.to_string().contains("reused"));
        assert!(crosslink.join("daemon.pid").is_file());
        assert!(crosslink.join("daemon.run.lock").is_file());
    }

    #[test]
    fn stop_preserves_current_repository_lease_when_identity_is_foreign() {
        let root = initialized();
        let crosslink = root.path().join(".crosslink");
        let process_start = readiness::current_process_start_token().unwrap();
        let foreign = identity("foreign".to_string(), process_start.clone());
        let current = identity(readiness::repository_id(&crosslink).unwrap(), process_start);
        readiness::write_daemon_identity(&crosslink, &foreign).unwrap();
        let run_path = crosslink.join("daemon.run.lock");
        let run_bytes = serde_json::to_vec(&current).unwrap();
        fs::write(&run_path, &run_bytes).unwrap();
        let error = stop(&crosslink).unwrap_err();
        assert!(error.to_string().contains("mismatched"));
        assert_eq!(fs::read(run_path).unwrap(), run_bytes);
        assert_eq!(
            readiness::read_daemon_identity(&crosslink).unwrap(),
            Some(foreign)
        );
    }

    #[test]
    fn truncated_projection_starts_into_parseable_blocked_state_without_source_loss() {
        let (_work, _remote, crosslink) = connected();
        let bytes = b"truncated sqlite evidence";
        fs::write(crosslink.join("issues.db"), bytes).unwrap();
        let record = blocked_attempt(&crosslink);
        assert_eq!(record.state, ReadinessState::BlockedCorrupt);
        assert!(record
            .reason
            .as_deref()
            .is_some_and(|reason| !reason.is_empty()));
        let evidence = record.evidence_path.as_deref().map(Path::new).unwrap();
        assert!(evidence.is_file());
        assert_eq!(fs::read(crosslink.join("issues.db")).unwrap(), bytes);
        assert_eq!(readiness::read_record(&crosslink).unwrap(), Some(record));
    }

    #[test]
    fn corrupt_cache_frontier_starts_into_parseable_blocked_state() {
        let (_work, _remote, crosslink) = connected();
        let db = Database::open(&crosslink.join("issues.db")).unwrap();
        drop(db);
        fs::write(crosslink.join(".hub-cache"), b"not a repository").unwrap();
        let record = blocked_attempt(&crosslink);
        assert_eq!(record.state, ReadinessState::BlockedCorrupt);
        assert!(record
            .reason
            .as_deref()
            .is_some_and(|reason| !reason.is_empty()));
        let evidence = record.evidence_path.as_deref().map(Path::new).unwrap();
        assert!(evidence.is_file());
        assert_eq!(
            fs::read(crosslink.join(".hub-cache")).unwrap(),
            b"not a repository"
        );
    }

    #[test]
    fn steady_state_defers_transition_while_long_mutation_is_live_then_resumes() {
        let (_work, _remote, crosslink, identity) = ready_connected();
        let long_mutation = readiness::acquire_mutation_permit(&crosslink).unwrap();
        let before = readiness::read_record(&crosslink).unwrap().unwrap();
        assert!(defer_reconciliation_for_active_mutations(&crosslink, &identity).unwrap());
        let refreshed = readiness::read_record(&crosslink).unwrap().unwrap();
        assert!(refreshed.sequence > before.sequence);
        let later_mutation = readiness::acquire_mutation_permit(&crosslink).unwrap();
        drop(later_mutation);
        drop(long_mutation);
        assert!(!defer_reconciliation_for_active_mutations(&crosslink, &identity).unwrap());
        let should_exit = AtomicBool::new(false);
        assert!(reconcile_until_ready(&crosslink, &identity, &should_exit).unwrap());
        assert!(readiness::require_mutation_ready(&crosslink).is_ok());
    }

    #[test]
    fn authority_advance_during_deferral_keeps_daemon_live_and_then_converges() {
        let (_work, _remote, crosslink, identity) = ready_connected();
        let long_mutation = readiness::acquire_mutation_permit(&crosslink).unwrap();
        let sync = SyncManager::new(&crosslink).unwrap();
        let cache = sync.cache_path().to_path_buf();
        assert!(
            defer_reconciliation_for_active_mutations_with(&crosslink, &identity, || {
                crate::hub_v3::write_heartbeat_to_ref(
                    &cache,
                    "interleaving-agent",
                    &crate::locks::Heartbeat {
                        agent_id: "interleaving-agent".to_string(),
                        last_heartbeat: chrono::Utc::now(),
                        active_issue_id: None,
                        machine_id: "test-machine".to_string(),
                    },
                )?;
                Ok(())
            },)
            .unwrap()
        );
        assert!(!readiness::projection_is_current(&crosslink).unwrap());
        drop(long_mutation);
        let should_exit = AtomicBool::new(false);
        assert!(reconcile_until_ready(&crosslink, &identity, &should_exit).unwrap());
        let record = readiness::read_record(&crosslink).unwrap().unwrap();
        readiness::validate_record(&crosslink, &record).unwrap();
        assert!(readiness::projection_is_current(&crosslink).unwrap());
    }

    #[test]
    fn offline_retry_blocks_mutation_without_projection_or_ref_movement_then_recovers() {
        let (_work, remote, crosslink, identity) = ready_connected();
        let sync = crate::sync::SyncManager::new(&crosslink).unwrap();
        let refs_before = Command::new("git")
            .current_dir(sync.cache_path())
            .args(["for-each-ref", "--format=%(refname) %(objectname)"])
            .output()
            .unwrap()
            .stdout;
        let db_path = crosslink.join("issues.db");
        let db_before = fs::read(&db_path).unwrap();
        let session_path = crosslink.join("session.json");
        let session_before = fs::read(&session_path).ok();
        let offline = remote
            .path()
            .parent()
            .unwrap()
            .join(format!("crosslink-offline-{}", Uuid::new_v4()));
        fs::rename(remote.path(), &offline).unwrap();
        let worker_dir = crosslink.clone();
        let worker_identity = identity;
        let should_exit = Arc::new(AtomicBool::new(false));
        let worker_exit = Arc::clone(&should_exit);
        let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            let result = reconcile_until_ready(&worker_dir, &worker_identity, &worker_exit);
            let _ = done_tx.send(result);
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if readiness::read_record(&crosslink)
                .unwrap()
                .is_some_and(|record| record.state == ReadinessState::WaitingForRemote)
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "daemon did not publish offline state"
            );
            thread::sleep(Duration::from_millis(10));
        }
        assert!(!worker.is_finished());
        assert!(readiness::acquire_mutation_permit(&crosslink).is_err());
        assert!(Database::open_read_only(&db_path).is_ok());
        assert_eq!(fs::read(&db_path).unwrap(), db_before);
        assert_eq!(fs::read(&session_path).ok(), session_before);
        let refs_offline = Command::new("git")
            .current_dir(sync.cache_path())
            .args(["for-each-ref", "--format=%(refname) %(objectname)"])
            .output()
            .unwrap()
            .stdout;
        assert_eq!(refs_offline, refs_before);
        fs::rename(&offline, remote.path()).unwrap();
        let result = match done_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(result) => result,
            Err(error) => {
                should_exit.store(true, Ordering::SeqCst);
                let _ = worker.join();
                panic!("offline recovery did not complete: {error}");
            }
        };
        worker.join().unwrap();
        assert!(result.unwrap());
        assert!(readiness::require_mutation_ready(&crosslink).is_ok());
    }
}
