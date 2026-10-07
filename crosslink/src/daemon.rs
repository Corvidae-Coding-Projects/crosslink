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
/// Housekeeping re-hydrates and retries a readiness refresh this many times
/// within one tick when the hub moves under the write, before handing the
/// tick back to reconciliation.
const MAX_RECORD_REFRESH_ATTEMPTS: u32 = 3;
/// Consecutive housekeeping-triggered reconciliations, without a completed
/// tick in between, after which the daemon parks as `blocked_corrupt` with the
/// cause instead of flapping between ready and reconciling.
const MAX_CONSECUTIVE_RECONCILES: u32 = 5;
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
                let republishing = wait_ready && readiness::record_expired(&record)?;
                if !republishing {
                    readiness::validate_record(crosslink_dir, &record)?;
                    return Ok(record);
                }
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
    let mut budget = ReconcileBudget::default();
    let mut reconcile_reason: Option<String> = None;
    while reconcile_until_ready(
        crosslink_dir,
        &identity,
        &should_exit,
        reconcile_reason.as_deref(),
    )? {
        match run_normal_loop(crosslink_dir, &should_exit)? {
            NormalLoopExit::Shutdown => break,
            NormalLoopExit::Reconcile {
                reason,
                completed_ticks,
            } => {
                if budget.note_reconcile(completed_ticks) {
                    let parked = format!(
                        "housekeeping sent the daemon back to reconciliation {} times in a row without completing a tick; last cause: {reason}",
                        budget.consecutive
                    );
                    tracing::error!("{parked}");
                    println!("{parked}; parking as blocked_corrupt.");
                    park_blocked(crosslink_dir, &identity, &should_exit, &parked)?;
                    break;
                }
                tracing::warn!("{reason}; reconciling again");
                println!("{reason}; reconciling again.");
                reconcile_reason = Some(reason);
            }
        }
    }
    drop(run_lease);
    Ok(())
}

/// Consecutive housekeeping-triggered reconciliations. A completed
/// housekeeping tick in between means the previous reconciliation worked and
/// resets the count; reaching the limit without one means the fault is
/// persistent and the daemon should park with the cause.
#[derive(Default)]
struct ReconcileBudget {
    consecutive: u32,
}

impl ReconcileBudget {
    /// Record one reconciliation request. Returns true when the budget is spent.
    const fn note_reconcile(&mut self, completed_ticks_since_last: u64) -> bool {
        if completed_ticks_since_last > 0 {
            self.consecutive = 0;
        }
        self.consecutive += 1;
        self.consecutive >= MAX_CONSECUTIVE_RECONCILES
    }
}

/// Record `blocked_corrupt` with the cause and hold the daemon there until it
/// is told to exit, so `daemon status` shows why and `daemon stop` plus
/// `ensure` recovers, like the reconciliation path's own blocked state.
fn park_blocked(
    crosslink_dir: &Path,
    identity: &DaemonIdentity,
    should_exit: &AtomicBool,
    reason: &str,
) -> Result<()> {
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
    for state in [ReadinessState::Reconciling, ReadinessState::BlockedCorrupt] {
        readiness::write_record(
            crosslink_dir,
            ReadinessDraft {
                daemon_epoch: &identity.daemon_epoch,
                daemon_pid: identity.pid,
                attempt_id: &attempt_id,
                state,
                generation_id: None,
                reason: Some(reason),
            },
        )?;
    }
    drop(transition);
    while !wait_interruptible(should_exit, Duration::from_secs(1)) {}
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
    #[cfg(windows)]
    prevent_standard_handle_inheritance()?;
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

#[cfg(windows)]
fn prevent_standard_handle_inheritance() -> Result<()> {
    use std::ffi::c_void;

    #[link(name = "kernel32")]
    extern "system" {
        #[link_name = "GetStdHandle"]
        fn get_std_handle(stream: u32) -> *mut c_void;
        #[link_name = "SetHandleInformation"]
        fn set_handle_information(handle: *mut c_void, mask: u32, flags: u32) -> i32;
    }

    const STD_INPUT_HANDLE: u32 = -10_i32 as u32;
    const STD_OUTPUT_HANDLE: u32 = -11_i32 as u32;
    const STD_ERROR_HANDLE: u32 = -12_i32 as u32;
    const HANDLE_FLAG_INHERIT: u32 = 1;

    for stream in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        let handle = unsafe { get_std_handle(stream) };
        if handle.is_null() || handle.addr() == usize::MAX {
            continue;
        }
        if unsafe { set_handle_information(handle, HANDLE_FLAG_INHERIT, 0) } == 0 {
            return Err(std::io::Error::last_os_error())
                .context("disabling standard handle inheritance for daemon launch");
        }
    }
    Ok(())
}

#[cfg(unix)]
fn detach_daemon_command(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    command.process_group(0);
}

#[cfg(windows)]
fn detach_daemon_command(command: &mut Command) {
    use std::os::windows::process::CommandExt;

    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    command.creation_flags(CREATE_NEW_PROCESS_GROUP);
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
                let Some(existing) = read_lease_if_present(&path, "daemon run lease")? else {
                    continue;
                };
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

fn read_lease_if_present(path: &Path, description: &str) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(contents) => Ok(Some(contents)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => {
            Err(error).with_context(|| format!("reading {description} {}", path.display()))
        }
    }
}

fn remove_stale_start_lease(path: &Path, repository_id: &str) -> Result<bool> {
    let Some(contents) = read_lease_if_present(path, "daemon startup lease")? else {
        return Ok(true);
    };
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
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("removing stale lease {}", path.display()));
            }
        }
    }
    Ok(())
}

fn lease_is_fresh(path: &Path, duration: Duration) -> Result<bool> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    let modified = metadata.modified()?;
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
    reason: Option<&str>,
) -> Result<bool> {
    reconcile_until_ready_with(crosslink_dir, identity, should_exit, reason, || Ok(()))
}

/// Reconcile until the repository is ready, waiting or blocked. `reason` is
/// recorded on the reconciling record so an operator can see why the daemon
/// left the ready state. `before_record` runs between activation and the
/// ready-record write; production passes a no-op and tests use it to move the
/// hub at exactly that point.
fn reconcile_until_ready_with<F>(
    crosslink_dir: &Path,
    identity: &DaemonIdentity,
    should_exit: &AtomicBool,
    reason: Option<&str>,
    mut before_record: F,
) -> Result<bool>
where
    F: FnMut() -> Result<()>,
{
    let mut retry_seconds = 1;
    let mut reconcile_reason = reason.map(str::to_string);
    let mut stale_retries = 0_u32;
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
                reason: reconcile_reason.as_deref(),
            },
        )?;
        let activation = match activate_repository(crosslink_dir) {
            Ok(activation) => activation,
            Err(error) => RepositoryActivation::BlockedCorrupt {
                reason: format!("{error:#}"),
            },
        };
        let (state, generation_id) = match activation {
            RepositoryActivation::ReadyCurrent { generation_id } => {
                (ReadinessState::ReadyCurrent, generation_id)
            }
            RepositoryActivation::ReadyMigrated { generation_id } => {
                (ReadinessState::ReadyMigrated, generation_id)
            }
            RepositoryActivation::ReadyAdopted { generation_id } => {
                (ReadinessState::ReadyAdopted, generation_id)
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
                continue;
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
        };
        before_record()?;
        match record_ready(crosslink_dir, identity, &attempt_id, state, &generation_id) {
            Ok(()) => {
                drop(transition);
                return Ok(true);
            }
            // Another checkout published between activation and the record
            // write. The activation is still sound; only its currency is gone,
            // so derive it again instead of ending the daemon.
            Err(error) if error.downcast_ref::<readiness::StaleProjection>().is_some() => {
                drop(transition);
                stale_retries += 1;
                let moved = format!("{error:#}");
                if stale_retries >= MAX_CONSECUTIVE_RECONCILES {
                    let parked = format!(
                        "the hub moved under {stale_retries} consecutive reconciliations; last cause: {moved}"
                    );
                    tracing::error!("{parked}");
                    println!("{parked}; parking as blocked_corrupt.");
                    park_blocked(crosslink_dir, identity, should_exit, &parked)?;
                    return Ok(false);
                }
                tracing::warn!("{moved}; reconciling again");
                println!("{moved}; reconciling again.");
                reconcile_reason = Some(moved);
                if wait_interruptible(should_exit, Duration::from_secs(retry_seconds)) {
                    return Ok(false);
                }
                retry_seconds = (retry_seconds * 2).min(MAX_RETRY_SECS);
            }
            Err(error) => return Err(error),
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

enum NormalLoopExit {
    Shutdown,
    Reconcile {
        reason: String,
        completed_ticks: u64,
    },
}

enum Housekeeping {
    Done { active_issue_id: Option<i64> },
    Reconcile(String),
}

fn run_normal_loop(crosslink_dir: &Path, should_exit: &AtomicBool) -> Result<NormalLoopExit> {
    let db_path = crosslink_dir.join("issues.db");
    let session_file = crosslink_dir.join("session.json");
    let mut heartbeat_counter = 0_u64;
    let mut completed_ticks = 0_u64;
    while !wait_interruptible(should_exit, Duration::from_secs(FLUSH_INTERVAL_SECS)) {
        let identity = readiness::read_daemon_identity(crosslink_dir)?
            .ok_or_else(|| anyhow::anyhow!("daemon identity disappeared"))?;
        if defer_reconciliation_for_active_mutations(crosslink_dir, &identity)? {
            continue;
        }
        let active_issue_id = match housekeeping_tick(
            crosslink_dir,
            &identity,
            &db_path,
            &session_file,
            || Ok(()),
        )? {
            Housekeeping::Done { active_issue_id } => active_issue_id,
            Housekeeping::Reconcile(reason) => {
                return Ok(NormalLoopExit::Reconcile {
                    reason,
                    completed_ticks,
                })
            }
        };
        completed_ticks += 1;
        heartbeat_counter += 1;
        if heartbeat_counter.is_multiple_of(5) {
            if let Err(error) = run_sync_tick(crosslink_dir, &db_path, active_issue_id) {
                tracing::warn!("daemon sync tick failed: {error}");
            }
        }
    }
    Ok(NormalLoopExit::Shutdown)
}

/// One housekeeping pass under the operation permit. Failures that come from
/// the projection or the authority moving under the daemon (another checkout
/// publishing, an expired record, a hydration that cannot complete) are
/// reported as a reason to reconcile again rather than as errors, because the
/// reconciliation path already knows how to re-derive readiness from the hub
/// and to record waiting or blocked states with their cause. A hub that moves
/// under the record write is first absorbed in place: hydrate again and
/// refresh again, a bounded number of times, under the same permit. Identity
/// and lease failures, and a projection that cannot be opened, stay errors.
fn housekeeping_tick<F>(
    crosslink_dir: &Path,
    identity: &DaemonIdentity,
    db_path: &Path,
    session_file: &Path,
    mut before_refresh_write: F,
) -> Result<Housekeeping>
where
    F: FnMut() -> Result<()>,
{
    let Some(mutation_operation) = acquire_housekeeping_unless_expired(crosslink_dir, identity)?
    else {
        return Ok(Housekeeping::Reconcile(
            "readiness record expired".to_string(),
        ));
    };
    let db = Database::open(db_path).context("opening projection for daemon housekeeping")?;
    let mut attempts = 0_u32;
    loop {
        attempts += 1;
        if let Err(error) = crate::hydration::maybe_auto_hydrate_under_operation(crosslink_dir, &db)
        {
            drop(mutation_operation);
            return Ok(Housekeeping::Reconcile(format!(
                "hydrating current authority during daemon housekeeping failed: {error:#}"
            )));
        }
        match refresh_ready_record_with(crosslink_dir, identity, &mut before_refresh_write) {
            Ok(_) => break,
            Err(error)
                if attempts < MAX_RECORD_REFRESH_ATTEMPTS
                    && error.downcast_ref::<readiness::StaleProjection>().is_some() =>
            {
                tracing::debug!(
                    "the hub moved during the readiness refresh (attempt {attempts}); hydrating again: {error:#}"
                );
            }
            Err(error) => {
                drop(mutation_operation);
                return Ok(Housekeeping::Reconcile(format!(
                    "refreshing the readiness record failed after {attempts} attempt(s): {error:#}"
                )));
            }
        }
    }
    let service = crate::application::RepositoryService::projection(&db);
    let agent_id = crate::identity::AgentConfig::load(crosslink_dir)
        .ok()
        .flatten()
        .map(|agent| agent.agent_id);
    let mut active_issue_id = None;
    if let Ok(Some(session)) = crate::application::LocalStateService::get_current_session_for_agent(
        &service,
        agent_id.as_deref(),
    ) {
        active_issue_id = session.active_issue_id;
        let data = serde_json::json!({
            "session_id": session.id,
            "started_at": session.started_at.to_rfc3339(),
            "active_issue_id": session.active_issue_id,
        });
        if let Ok(bytes) = serde_json::to_vec_pretty(&data) {
            if let Err(error) = fs::write(session_file, bytes) {
                tracing::warn!("failed to write session file: {error}");
            }
        }
    }
    drop(mutation_operation);
    Ok(Housekeeping::Done { active_issue_id })
}

fn acquire_housekeeping_unless_expired(
    crosslink_dir: &Path,
    identity: &DaemonIdentity,
) -> Result<Option<readiness::MutationOperationPermit>> {
    match acquire_housekeeping_operation(crosslink_dir) {
        Ok(permit) => Ok(Some(permit)),
        Err(error) => match readiness::read_record(crosslink_dir)? {
            Some(record)
                if (record.daemon_epoch.as_str(), record.daemon_pid)
                    == (identity.daemon_epoch.as_str(), identity.pid)
                    && readiness::record_expired(&record)? =>
            {
                Ok(None)
            }
            _ => Err(error),
        },
    }
}

fn acquire_housekeeping_operation(
    crosslink_dir: &Path,
) -> Result<readiness::MutationOperationPermit> {
    readiness::acquire_projection_repair_operation_permit(crosslink_dir)
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
    F: FnMut() -> Result<()>,
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
    mut before_write: F,
) -> Result<bool>
where
    F: FnMut() -> Result<()>,
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
            reconcile_until_ready(&worker_dir, &worker_identity, &worker_exit, None)
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

    fn expire_latest_record(crosslink: &Path) {
        let mut records = fs::read_dir(crosslink.join("readiness"))
            .unwrap()
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|value| value == "json"))
            .collect::<Vec<_>>();
        records.sort();
        let path = records.last().unwrap();
        let mut record: ReadinessRecord = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        record.updated_at = (chrono::Utc::now()
            - chrono::Duration::seconds(readiness::MAX_RECORD_AGE_SECONDS + 1))
        .to_rfc3339();
        fs::write(path, serde_json::to_vec_pretty(&record).unwrap()).unwrap();
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
    fn vanished_lease_is_retryable() {
        let root = initialized();
        let path = root.path().join(".crosslink/daemon.start.lock");
        assert!(read_lease_if_present(&path, "daemon startup lease")
            .unwrap()
            .is_none());
        assert!(!lease_is_fresh(&path, Duration::from_secs(1)).unwrap());
        assert!(remove_stale_start_lease(&path, "repository").unwrap());
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
    fn steady_state_refreshes_readiness_without_starting_a_transition() {
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
        let housekeeping = acquire_housekeeping_operation(&crosslink).unwrap();
        assert!(refresh_ready_record_with(&crosslink, &identity, || Ok(())).unwrap());
        drop(housekeeping);
        let maintained = readiness::read_record(&crosslink).unwrap().unwrap();
        assert!(maintained.sequence > refreshed.sequence);
        assert_eq!(maintained.attempt_id, refreshed.attempt_id);
        assert!(!crosslink.join("readiness").join("transition.lock").exists());
        assert!(readiness::require_mutation_ready(&crosslink).is_ok());
    }

    #[test]
    fn expired_record_returns_housekeeping_to_reconciliation_and_recovers() {
        let (_work, _remote, crosslink, identity) = ready_connected();
        drop(
            acquire_housekeeping_unless_expired(&crosslink, &identity)
                .unwrap()
                .unwrap(),
        );
        expire_latest_record(&crosslink);
        assert!(readiness::require_mutation_ready(&crosslink).is_err());
        assert!(acquire_housekeeping_unless_expired(&crosslink, &identity)
            .unwrap()
            .is_none());
        assert!(
            reconcile_until_ready(&crosslink, &identity, &AtomicBool::new(false), None).unwrap()
        );
        let record = readiness::read_record(&crosslink).unwrap().unwrap();
        assert_eq!(record.daemon_epoch, identity.daemon_epoch);
        readiness::validate_record(&crosslink, &record).unwrap();
        assert!(readiness::require_mutation_ready(&crosslink).is_ok());
    }

    /// Publish one event to the hub as another checkout would, with its own
    /// sequence so repeated races stay valid.
    fn publish_as_racing_agent(cache: &Path, agent_seq: u64) -> Result<()> {
        crate::hub_v3::append_event_to_ref(
            cache,
            "racing-agent",
            &crate::events::EventEnvelope {
                agent_id: "racing-agent".to_string(),
                agent_seq,
                timestamp: chrono::Utc::now(),
                event: crate::events::Event::LockClaimed {
                    issue_display_id: 1,
                    branch: None,
                },
                signed_by: None,
                signature: None,
            },
        )?;
        Ok(())
    }

    #[test]
    fn authority_moving_during_every_record_write_reconciles_instead_of_failing() {
        let (_work, _remote, crosslink, identity) = ready_connected();
        let sync = SyncManager::new(&crosslink).unwrap();
        let cache = sync.cache_path().to_path_buf();
        let db_path = crosslink.join("issues.db");
        let session_file = crosslink.join("session.json");
        // Another checkout publishes between the daemon's validation and its
        // record write, every time the daemon tries: the race a busy hub of
        // kickoff worktrees produces.
        let mut races = 0;
        let outcome = housekeeping_tick(&crosslink, &identity, &db_path, &session_file, || {
            races += 1;
            publish_as_racing_agent(&cache, races)
        })
        .unwrap();
        assert_eq!(races, u64::from(MAX_RECORD_REFRESH_ATTEMPTS));
        let Housekeeping::Reconcile(reason) = outcome else {
            panic!("the moved authority must send housekeeping back to reconciliation");
        };
        assert!(
            reason.contains("refusing to publish ready state for a stale projection"),
            "{reason}"
        );
        assert!(reason.contains("after 3 attempt"), "{reason}");
        assert!(reconcile_until_ready(
            &crosslink,
            &identity,
            &AtomicBool::new(false),
            Some(&reason)
        )
        .unwrap());
        let record = readiness::read_record(&crosslink).unwrap().unwrap();
        assert_eq!(record.daemon_epoch, identity.daemon_epoch);
        readiness::validate_record(&crosslink, &record).unwrap();
        assert!(readiness::projection_is_current(&crosslink).unwrap());
    }

    #[test]
    fn a_single_race_during_the_record_write_is_absorbed_within_the_tick() {
        let (_work, _remote, crosslink, identity) = ready_connected();
        let sync = SyncManager::new(&crosslink).unwrap();
        let cache = sync.cache_path().to_path_buf();
        let before = readiness::read_record(&crosslink).unwrap().unwrap();
        let mut calls = 0;
        let outcome = housekeeping_tick(
            &crosslink,
            &identity,
            &crosslink.join("issues.db"),
            &crosslink.join("session.json"),
            || {
                calls += 1;
                if calls == 1 {
                    publish_as_racing_agent(&cache, 1)
                } else {
                    Ok(())
                }
            },
        )
        .unwrap();
        assert_eq!(calls, 2, "the tick hydrates again and refreshes once more");
        assert!(matches!(outcome, Housekeeping::Done { .. }));
        let after = readiness::read_record(&crosslink).unwrap().unwrap();
        assert!(after.sequence > before.sequence);
        assert_eq!(after.attempt_id, before.attempt_id);
        assert!(readiness::projection_is_current(&crosslink).unwrap());
    }

    #[test]
    fn authority_moving_during_the_reconciliation_record_write_reconciles_again() {
        let (_work, _remote, crosslink, identity) = ready_connected();
        let sync = SyncManager::new(&crosslink).unwrap();
        let cache = sync.cache_path().to_path_buf();
        // Another checkout publishes between activation and the ready-record
        // write of the reconciliation path itself.
        let mut calls = 0;
        let ready = reconcile_until_ready_with(
            &crosslink,
            &identity,
            &AtomicBool::new(false),
            None,
            || {
                calls += 1;
                if calls == 1 {
                    publish_as_racing_agent(&cache, 1)
                } else {
                    Ok(())
                }
            },
        )
        .unwrap();
        assert!(ready);
        assert_eq!(
            calls, 2,
            "the first write is refused and activation runs again"
        );
        let record = readiness::read_record(&crosslink).unwrap().unwrap();
        assert!(record.state.grants_mutations());
        assert_eq!(record.daemon_epoch, identity.daemon_epoch);
        readiness::validate_record(&crosslink, &record).unwrap();
        assert!(readiness::projection_is_current(&crosslink).unwrap());
    }

    #[test]
    fn a_hub_moving_under_every_reconciliation_write_parks_with_the_cause() {
        let (_work, _remote, crosslink, identity) = ready_connected();
        let sync = SyncManager::new(&crosslink).unwrap();
        let cache = sync.cache_path().to_path_buf();
        let should_exit = Arc::new(AtomicBool::new(false));
        let worker_dir = crosslink.clone();
        let worker_exit = Arc::clone(&should_exit);
        let worker = thread::spawn(move || {
            let mut races = 0;
            reconcile_until_ready_with(&worker_dir, &identity, &worker_exit, None, || {
                races += 1;
                publish_as_racing_agent(&cache, races)
            })
            .map(|ready| (ready, races))
        });
        // 1 + 2 + 4 + 8 s of backoff precede the fifth attempt; allow for the
        // activations around them on a slow machine.
        let deadline = Instant::now() + Duration::from_secs(120);
        let parked = loop {
            if let Some(record) = readiness::read_record(&crosslink).unwrap() {
                if record.state == ReadinessState::BlockedCorrupt {
                    break record;
                }
            }
            assert!(Instant::now() < deadline, "the daemon did not park");
            thread::sleep(Duration::from_millis(100));
        };
        should_exit.store(true, Ordering::SeqCst);
        let (ready, races) = worker.join().unwrap().unwrap();
        assert!(!ready);
        assert_eq!(races, MAX_CONSECUTIVE_RECONCILES as u64);
        let reason = parked.reason.unwrap_or_default();
        assert!(reason.contains("consecutive reconciliations"), "{reason}");
        assert!(
            reason.contains("refusing to publish ready state for a stale projection"),
            "{reason}"
        );
    }

    /// Unix only: the failure is injected through directory permissions.
    #[cfg(unix)]
    #[test]
    fn hydration_failure_during_housekeeping_reconciles_instead_of_failing() {
        use std::os::unix::fs::PermissionsExt;
        if Command::new("id")
            .arg("-u")
            .output()
            .is_ok_and(|output| String::from_utf8_lossy(&output.stdout).trim() == "0")
        {
            eprintln!(
                "skipped: root ignores directory permissions, so the failure cannot be injected"
            );
            return;
        }
        let (_work, _remote, crosslink, identity) = ready_connected();
        let sync = SyncManager::new(&crosslink).unwrap();
        // The hub moves, so the tick must hydrate, and the hydration cannot
        // record the new frontier because its directory is not writable.
        publish_as_racing_agent(sync.cache_path(), 1).unwrap();
        let frontier_dir = crosslink.join(crate::hydration::HYDRATED_FRONTIER_DIR);
        fs::set_permissions(&frontier_dir, fs::Permissions::from_mode(0o555)).unwrap();
        let outcome = housekeeping_tick(
            &crosslink,
            &identity,
            &crosslink.join("issues.db"),
            &crosslink.join("session.json"),
            || Ok(()),
        );
        fs::set_permissions(&frontier_dir, fs::Permissions::from_mode(0o755)).unwrap();
        let Housekeeping::Reconcile(reason) = outcome.unwrap() else {
            panic!(
                "a hydration that cannot complete must send housekeeping back to reconciliation"
            );
        };
        assert!(
            reason.contains("hydrating current authority during daemon housekeeping failed"),
            "{reason}"
        );
    }

    #[test]
    fn the_reconcile_budget_parks_persistent_faults_and_resets_on_a_completed_tick() {
        let mut budget = ReconcileBudget::default();
        for _ in 1..MAX_CONSECUTIVE_RECONCILES {
            assert!(!budget.note_reconcile(0));
        }
        assert!(
            budget.note_reconcile(0),
            "the limit is reached without a completed tick"
        );

        let mut budget = ReconcileBudget::default();
        for _ in 1..MAX_CONSECUTIVE_RECONCILES {
            assert!(!budget.note_reconcile(0));
        }
        assert!(
            !budget.note_reconcile(1),
            "a completed tick resets the count"
        );
        assert_eq!(budget.consecutive, 1);
    }

    #[test]
    fn a_quiet_housekeeping_tick_completes_and_keeps_the_record_fresh() {
        let (_work, _remote, crosslink, identity) = ready_connected();
        let before = readiness::read_record(&crosslink).unwrap().unwrap();
        let outcome = housekeeping_tick(
            &crosslink,
            &identity,
            &crosslink.join("issues.db"),
            &crosslink.join("session.json"),
            || Ok(()),
        )
        .unwrap();
        assert!(matches!(outcome, Housekeeping::Done { .. }));
        let after = readiness::read_record(&crosslink).unwrap().unwrap();
        assert!(after.sequence > before.sequence);
        assert_eq!(after.attempt_id, before.attempt_id);
    }

    #[test]
    fn expired_record_of_another_daemon_is_still_an_error() {
        let (_work, _remote, crosslink, identity) = ready_connected();
        expire_latest_record(&crosslink);
        let other = DaemonIdentity {
            daemon_epoch: Uuid::new_v4().to_string(),
            ..identity
        };
        assert!(acquire_housekeeping_unless_expired(&crosslink, &other).is_err());
    }

    #[test]
    fn housekeeping_failure_with_a_fresh_record_is_still_an_error() {
        let (_work, _remote, crosslink, identity) = ready_connected();
        readiness::write_record(
            &crosslink,
            ReadinessDraft {
                daemon_epoch: &identity.daemon_epoch,
                daemon_pid: identity.pid,
                attempt_id: "reconciling",
                state: ReadinessState::Reconciling,
                generation_id: None,
                reason: None,
            },
        )
        .unwrap();
        assert!(acquire_housekeeping_unless_expired(&crosslink, &identity).is_err());
    }

    #[test]
    fn wait_ready_outlasts_an_expired_record_until_the_daemon_republishes() {
        let (_work, _remote, crosslink, identity) = ready_connected();
        let ready = readiness::read_record(&crosslink).unwrap().unwrap();
        expire_latest_record(&crosslink);
        let waited = ensure_with_deadline(&crosslink, true, Duration::from_millis(300))
            .unwrap_err()
            .to_string();
        assert!(waited.contains("timed out waiting for daemon readiness"));
        let unwaited = ensure_with_deadline(&crosslink, false, Duration::from_millis(300))
            .unwrap_err()
            .to_string();
        assert!(unwaited.contains("readiness record is stale"));
        record_ready(
            &crosslink,
            &identity,
            "republished",
            ready.state,
            ready.generation_id.as_deref().unwrap(),
        )
        .unwrap();
        let record = ensure_with_deadline(&crosslink, true, Duration::from_secs(5)).unwrap();
        assert!(record.state.grants_mutations());
        assert_eq!(record.attempt_id, "republished");
    }

    #[test]
    fn housekeeping_waits_for_foreground_readiness_publication() {
        let (_work, _remote, crosslink, identity) = ready_connected();
        let ready = readiness::read_record(&crosslink).unwrap().unwrap();
        let foreground = readiness::acquire_mutation_operation_permit(&crosslink).unwrap();
        readiness::write_record(
            &crosslink,
            ReadinessDraft {
                daemon_epoch: &identity.daemon_epoch,
                daemon_pid: identity.pid,
                attempt_id: "foreground",
                state: ReadinessState::Reconciling,
                generation_id: None,
                reason: None,
            },
        )
        .unwrap();
        let (result_tx, result_rx) = mpsc::sync_channel(1);
        let worker_dir = crosslink.clone();
        let worker = thread::spawn(move || {
            result_tx
                .send(acquire_housekeeping_operation(&worker_dir))
                .unwrap();
        });
        assert!(result_rx.recv_timeout(Duration::from_millis(100)).is_err());
        record_ready(
            &crosslink,
            &identity,
            "foreground-complete",
            ready.state,
            ready.generation_id.as_deref().unwrap(),
        )
        .unwrap();
        drop(foreground);
        let housekeeping = result_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        drop(housekeeping.unwrap());
        worker.join().unwrap();
    }

    #[test]
    fn authority_advance_during_deferral_is_hydrated_without_reconciliation() {
        let (_work, _remote, crosslink, identity) = ready_connected();
        let long_mutation = readiness::acquire_mutation_permit(&crosslink).unwrap();
        let sync = SyncManager::new(&crosslink).unwrap();
        let cache = sync.cache_path().to_path_buf();
        assert!(
            defer_reconciliation_for_active_mutations_with(&crosslink, &identity, || {
                crate::hub_v3::append_event_to_ref(
                    &cache,
                    "interleaving-agent",
                    &crate::events::EventEnvelope {
                        agent_id: "interleaving-agent".to_string(),
                        agent_seq: 1,
                        timestamp: chrono::Utc::now(),
                        event: crate::events::Event::LockClaimed {
                            issue_display_id: 1,
                            branch: None,
                        },
                        signed_by: None,
                        signature: None,
                    },
                )?;
                Ok(())
            },)
            .unwrap()
        );
        assert!(!readiness::projection_is_current(&crosslink).unwrap());
        drop(long_mutation);
        let housekeeping = acquire_housekeeping_operation(&crosslink).unwrap();
        let db = Database::open(&crosslink.join("issues.db")).unwrap();
        assert!(crate::hydration::maybe_auto_hydrate_under_operation(&crosslink, &db).unwrap());
        assert!(refresh_ready_record_with(&crosslink, &identity, || Ok(())).unwrap());
        drop(housekeeping);
        let record = readiness::read_record(&crosslink).unwrap().unwrap();
        readiness::validate_record(&crosslink, &record).unwrap();
        assert!(readiness::projection_is_current(&crosslink).unwrap());
        assert!(!crosslink.join("readiness").join("transition.lock").exists());
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
            let result = reconcile_until_ready(&worker_dir, &worker_identity, &worker_exit, None);
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
        let result = match done_rx.recv_timeout(Duration::from_secs(
            MAX_RETRY_SECS + START_LOCK_DEADLINE_SECS,
        )) {
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
