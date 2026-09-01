use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::publication::GENERATION_REF;

pub const READINESS_SCHEMA_VERSION: u32 = 1;
pub const READINESS_PROTOCOL_VERSION: u32 = 1;
const READINESS_DIR: &str = "readiness";
const DAEMON_IDENTITY_FILE: &str = "daemon.pid";
const TRANSITION_FILE: &str = "transition.lock";
const OPERATION_FILE: &str = "mutation-operation.lock";
const RECORD_APPEND_FILE: &str = "record-append.lock";
const MUTATION_PERMITS_DIR: &str = "mutation-permits";
const MAX_READINESS_RECORDS: usize = 64;
const MAX_RECORD_AGE_SECONDS: i64 = 90;
const PERMIT_POLL_MILLIS: u64 = 25;
const MUTATION_TRANSITION_WAIT_SECS: u64 = 5;
const LIVENESS_SWEEP_POLLS: u8 = 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadinessState {
    Starting,
    Reconciling,
    Rebuilding,
    ReadyCurrent,
    ReadyMigrated,
    ReadyAdopted,
    WaitingForRemote,
    BlockedCorrupt,
}

impl ReadinessState {
    #[must_use]
    pub const fn grants_mutations(self) -> bool {
        matches!(
            self,
            Self::ReadyCurrent | Self::ReadyMigrated | Self::ReadyAdopted
        )
    }

    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::ReadyCurrent
                | Self::ReadyMigrated
                | Self::ReadyAdopted
                | Self::WaitingForRemote
                | Self::BlockedCorrupt
        )
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Reconciling => "reconciling",
            Self::Rebuilding => "rebuilding",
            Self::ReadyCurrent => "ready_current",
            Self::ReadyMigrated => "ready_migrated",
            Self::ReadyAdopted => "ready_adopted",
            Self::WaitingForRemote => "waiting_for_remote",
            Self::BlockedCorrupt => "blocked_corrupt",
        }
    }

    #[must_use]
    pub const fn outcome(self) -> Option<ReadinessOutcomeState> {
        match self {
            Self::ReadyCurrent => Some(ReadinessOutcomeState::ReadyCurrent),
            Self::ReadyMigrated => Some(ReadinessOutcomeState::ReadyMigrated),
            Self::ReadyAdopted => Some(ReadinessOutcomeState::ReadyAdopted),
            Self::WaitingForRemote => Some(ReadinessOutcomeState::WaitingForRemote),
            Self::BlockedCorrupt => Some(ReadinessOutcomeState::BlockedCorrupt),
            Self::Starting | Self::Reconciling | Self::Rebuilding => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadinessOutcomeState {
    ReadyCurrent,
    ReadyMigrated,
    ReadyAdopted,
    WaitingForRemote,
    BlockedCorrupt,
}

impl ReadinessOutcomeState {
    #[must_use]
    pub const fn grants_mutations(self) -> bool {
        matches!(
            self,
            Self::ReadyCurrent | Self::ReadyMigrated | Self::ReadyAdopted
        )
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReadyCurrent => "ready_current",
            Self::ReadyMigrated => "ready_migrated",
            Self::ReadyAdopted => "ready_adopted",
            Self::WaitingForRemote => "waiting_for_remote",
            Self::BlockedCorrupt => "blocked_corrupt",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadinessRecord {
    pub schema_version: u32,
    pub protocol_version: u32,
    pub sequence: u64,
    pub repository_id: String,
    pub daemon_epoch: String,
    pub daemon_pid: u32,
    pub attempt_id: String,
    pub source_fingerprint: String,
    pub generation_id: Option<String>,
    pub projection_frontier: Option<String>,
    pub projection_schema_version: Option<i32>,
    pub updated_at: String,
    pub state: ReadinessState,
    pub reason: Option<String>,
    pub evidence_path: Option<String>,
    pub evidence_sha256: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DaemonResponse {
    pub schema_version: u32,
    pub protocol_version: u32,
    pub state: Option<ReadinessOutcomeState>,
    pub ready: bool,
    pub running: bool,
    pub repository_id: Option<String>,
    pub daemon_epoch: Option<String>,
    pub daemon_pid: Option<u32>,
    pub attempt_id: Option<String>,
    pub generation_id: Option<String>,
    pub updated_at: Option<String>,
    pub reason: Option<String>,
    pub evidence_path: Option<String>,
    pub evidence_sha256: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DaemonResponseWire {
    schema_version: u32,
    protocol_version: u32,
    state: Option<ReadinessOutcomeState>,
    ready: bool,
    running: bool,
    repository_id: Option<String>,
    daemon_epoch: Option<String>,
    daemon_pid: Option<u32>,
    attempt_id: Option<String>,
    generation_id: Option<String>,
    updated_at: Option<String>,
    reason: Option<String>,
    evidence_path: Option<String>,
    evidence_sha256: Option<String>,
}

impl<'de> Deserialize<'de> for DaemonResponse {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = DaemonResponseWire::deserialize(deserializer)?;
        let response = Self {
            schema_version: wire.schema_version,
            protocol_version: wire.protocol_version,
            state: wire.state,
            ready: wire.ready,
            running: wire.running,
            repository_id: wire.repository_id,
            daemon_epoch: wire.daemon_epoch,
            daemon_pid: wire.daemon_pid,
            attempt_id: wire.attempt_id,
            generation_id: wire.generation_id,
            updated_at: wire.updated_at,
            reason: wire.reason,
            evidence_path: wire.evidence_path,
            evidence_sha256: wire.evidence_sha256,
        };
        response
            .validate_contract()
            .map_err(serde::de::Error::custom)?;
        Ok(response)
    }
}

impl DaemonResponse {
    fn validate_contract(&self) -> Result<()> {
        anyhow::ensure!(
            self.schema_version == READINESS_SCHEMA_VERSION
                && self.protocol_version == READINESS_PROTOCOL_VERSION,
            "unsupported readiness response version"
        );
        anyhow::ensure!(
            self.ready
                == self
                    .state
                    .is_some_and(ReadinessOutcomeState::grants_mutations),
            "readiness response ready flag contradicts state"
        );
        anyhow::ensure!(
            self.state.is_none() || self.running,
            "readiness response outcome requires a running daemon"
        );
        if self.running {
            anyhow::ensure!(
                self.repository_id
                    .as_deref()
                    .is_some_and(|value| !value.is_empty())
                    && self
                        .daemon_epoch
                        .as_deref()
                        .is_some_and(|value| !value.is_empty())
                    && self.daemon_pid.is_some()
                    && self
                        .attempt_id
                        .as_deref()
                        .is_some_and(|value| !value.is_empty())
                    && self
                        .updated_at
                        .as_deref()
                        .is_some_and(|value| !value.is_empty()),
                "running readiness response has incomplete daemon identity"
            );
        } else {
            anyhow::ensure!(
                self.state.is_none()
                    && self.repository_id.is_none()
                    && self.daemon_epoch.is_none()
                    && self.daemon_pid.is_none()
                    && self.attempt_id.is_none()
                    && self.generation_id.is_none()
                    && self.updated_at.is_none()
                    && self.evidence_path.is_none()
                    && self.evidence_sha256.is_none(),
                "stopped readiness response retains live daemon fields"
            );
        }
        if self.ready {
            anyhow::ensure!(
                self.generation_id
                    .as_deref()
                    .is_some_and(|value| !value.is_empty()),
                "ready response has no generation identifier"
            );
        }
        let requires_evidence = matches!(
            self.state,
            Some(ReadinessOutcomeState::WaitingForRemote | ReadinessOutcomeState::BlockedCorrupt)
        );
        anyhow::ensure!(
            self.evidence_path.is_some() == self.evidence_sha256.is_some(),
            "readiness response evidence binding is incomplete"
        );
        if requires_evidence {
            anyhow::ensure!(
                self.reason
                    .as_deref()
                    .is_some_and(|value| !value.is_empty())
                    && self
                        .evidence_path
                        .as_deref()
                        .is_some_and(|value| !value.is_empty()),
                "non-ready response has no reason or evidence"
            );
        }
        if let Some(digest) = &self.evidence_sha256 {
            anyhow::ensure!(
                digest.len() == 64
                    && digest
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
                "readiness response evidence digest is invalid"
            );
        }
        Ok(())
    }

    #[must_use]
    pub fn from_record(record: &ReadinessRecord) -> Self {
        Self {
            schema_version: READINESS_SCHEMA_VERSION,
            protocol_version: READINESS_PROTOCOL_VERSION,
            state: record.state.outcome(),
            ready: record.state.grants_mutations(),
            running: true,
            repository_id: Some(record.repository_id.clone()),
            daemon_epoch: Some(record.daemon_epoch.clone()),
            daemon_pid: Some(record.daemon_pid),
            attempt_id: Some(record.attempt_id.clone()),
            generation_id: record.generation_id.clone(),
            updated_at: Some(record.updated_at.clone()),
            reason: record.reason.clone(),
            evidence_path: record.evidence_path.clone(),
            evidence_sha256: record.evidence_sha256.clone(),
        }
    }

    #[must_use]
    pub const fn stopped() -> Self {
        Self {
            schema_version: READINESS_SCHEMA_VERSION,
            protocol_version: READINESS_PROTOCOL_VERSION,
            state: None,
            ready: false,
            running: false,
            repository_id: None,
            daemon_epoch: None,
            daemon_pid: None,
            attempt_id: None,
            generation_id: None,
            updated_at: None,
            reason: None,
            evidence_path: None,
            evidence_sha256: None,
        }
    }

    #[must_use]
    pub fn error(reason: String) -> Self {
        let mut response = Self::stopped();
        response.reason = Some(reason);
        response
    }

    #[must_use]
    pub fn live_error(record: &ReadinessRecord, reason: String) -> Self {
        let mut response = Self::from_record(record);
        response.state = None;
        response.ready = false;
        response.reason = Some(reason);
        response.evidence_path = None;
        response.evidence_sha256 = None;
        response
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonIdentity {
    pub schema_version: u32,
    pub repository_id: String,
    pub daemon_epoch: String,
    pub pid: u32,
    pub process_start: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReconciliationObservationEvidence {
    schema_version: u32,
    protocol_version: u32,
    sequence: u64,
    repository_id: String,
    attempt_id: String,
    source_fingerprint: String,
    state: ReadinessState,
    observed_at: String,
    reason: Option<String>,
    publication_journal: Option<String>,
    related_evidence: Option<String>,
}

struct PublishedObservationEvidence {
    path: PathBuf,
    sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PermitOwner {
    pid: u32,
    process_start: String,
    token: String,
}

#[derive(Debug)]
pub struct MutationPermit {
    path: PathBuf,
    contents: Vec<u8>,
}

#[derive(Debug)]
pub struct TransitionPermit {
    path: PathBuf,
    contents: Vec<u8>,
}

#[derive(Debug)]
struct OperationLease {
    path: PathBuf,
    contents: Vec<u8>,
}

#[derive(Debug)]
pub struct MutationOperationPermit {
    _authority: Arc<OperationAuthority>,
}

#[derive(Debug)]
struct OperationAuthority {
    _readiness: Option<MutationPermit>,
    _operation: OperationLease,
}

thread_local! {
    static HELD_OPERATIONS: RefCell<HashMap<PathBuf, Weak<OperationAuthority>>> =
        RefCell::new(HashMap::new());
}

impl Drop for MutationPermit {
    fn drop(&mut self) {
        remove_owned_file(&self.path, &self.contents);
    }
}

impl Drop for TransitionPermit {
    fn drop(&mut self) {
        remove_owned_file(&self.path, &self.contents);
    }
}

impl Drop for OperationLease {
    fn drop(&mut self) {
        remove_owned_file(&self.path, &self.contents);
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ReadinessDraft<'a> {
    pub daemon_epoch: &'a str,
    pub daemon_pid: u32,
    pub attempt_id: &'a str,
    pub state: ReadinessState,
    pub generation_id: Option<&'a str>,
    pub reason: Option<&'a str>,
}

pub fn repository_id(crosslink_dir: &Path) -> Result<String> {
    let root = crosslink_dir
        .parent()
        .ok_or_else(|| anyhow::anyhow!(".crosslink directory has no repository root"))?;
    let canonical = root
        .canonicalize()
        .with_context(|| format!("canonicalizing repository root {}", root.display()))?;
    Ok(hex::encode(Sha256::digest(
        canonical.to_string_lossy().as_bytes(),
    )))
}

pub fn source_fingerprint(crosslink_dir: &Path) -> Result<String> {
    let format = super::check_repository(crosslink_dir).format;
    let local = match &format.local_database {
        super::LocalDatabaseFormat::Missing => serde_json::json!({"kind": "missing"}),
        super::LocalDatabaseFormat::Sqlite {
            version,
            schema_fingerprint,
            ..
        } => serde_json::json!({
            "kind": "sqlite",
            "version": version,
            "schema_fingerprint": schema_fingerprint,
        }),
        super::LocalDatabaseFormat::Future {
            version,
            supported_version,
            schema_fingerprint,
            ..
        } => serde_json::json!({
            "kind": "future",
            "version": version,
            "supported_version": supported_version,
            "schema_fingerprint": schema_fingerprint,
        }),
        super::LocalDatabaseFormat::Unreadable { reason } => {
            serde_json::json!({"kind": "unreadable", "reason": reason})
        }
    };
    let shared = match &format.shared_store {
        super::SharedStoreFormat::Absent => serde_json::json!({"kind": "absent"}),
        super::SharedStoreFormat::LegacyLocks { .. } => {
            serde_json::json!({"kind": "legacy_locks"})
        }
        super::SharedStoreFormat::V2 { .. } => serde_json::json!({"kind": "v2"}),
        super::SharedStoreFormat::HiddenV3 { .. } => {
            serde_json::json!({"kind": "hidden_v3"})
        }
        super::SharedStoreFormat::VisibleV3 { .. } => {
            serde_json::json!({"kind": "visible_v3"})
        }
        super::SharedStoreFormat::Mixed { families, .. } => {
            serde_json::json!({"kind": "mixed", "families": families})
        }
        super::SharedStoreFormat::Unreadable { reason } => {
            serde_json::json!({"kind": "unreadable", "reason": reason})
        }
    };
    let mut evidence =
        vec![
            serde_json::to_string(&serde_json::json!({"local": local, "shared": shared}))
                .context("serializing repository compatibility evidence")?,
        ];
    let root = crosslink_dir
        .parent()
        .ok_or_else(|| anyhow::anyhow!(".crosslink directory has no repository root"))?;
    evidence.extend(reconciliation_source_refs("repository", root)?);
    if let Ok(sync) = crate::sync::SyncManager::new(crosslink_dir) {
        if sync.cache_path().is_dir() {
            evidence.extend(reconciliation_source_refs("cache", sync.cache_path())?);
        }
    }
    evidence.sort();
    let bytes = serde_json::to_vec(&evidence).context("serializing repository source evidence")?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

pub fn projection_frontier(crosslink_dir: &Path) -> Result<Option<String>> {
    let value = reconciliation_generation_frontier(crosslink_dir)?;
    let Some(value) = value else {
        return Ok(None);
    };
    let hydrated =
        crate::hydration::hydrated_frontier(crosslink_dir)?.filter(|contents| !contents.is_empty());
    let authority = crate::hydration::projection_authority_ref(crosslink_dir)?;
    if hydrated.is_none() || authority.is_none() {
        return Ok(None);
    }
    Ok(Some(format!(
        "generation={};hydrated={};authority={}",
        value,
        hydrated.as_deref().unwrap_or_default(),
        authority.as_deref().unwrap_or_default()
    )))
}

fn reconciliation_generation_frontier(crosslink_dir: &Path) -> Result<Option<String>> {
    let sync = crate::sync::SyncManager::new(crosslink_dir)?;
    if !sync.is_initialized() {
        return Ok(None);
    }
    let output = Command::new("git")
        .current_dir(sync.cache_path())
        .args(["rev-parse", "--verify", GENERATION_REF])
        .output()
        .context("reading reconciliation generation frontier")?;
    if !output.status.success() {
        return Ok(None);
    }
    let value = String::from_utf8(output.stdout)
        .context("reconciliation frontier was not UTF-8")?
        .trim()
        .to_string();
    Ok((!value.is_empty()).then_some(value))
}

pub fn projection_is_current(crosslink_dir: &Path) -> Result<bool> {
    let hydrated = crate::hydration::hydrated_frontier(crosslink_dir)?;
    Ok(
        hydrated.is_some()
            && hydrated == crate::hydration::projection_authority_ref(crosslink_dir)?,
    )
}

pub fn projection_schema_version(crosslink_dir: &Path) -> Result<Option<i32>> {
    let path = crosslink_dir.join("issues.db");
    if !path.is_file() {
        return Ok(None);
    }
    let connection =
        rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("opening projection {} read-only", path.display()))?;
    connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map(Some)
        .context("reading projection schema version")
}

pub fn write_record(crosslink_dir: &Path, draft: ReadinessDraft<'_>) -> Result<ReadinessRecord> {
    let directory = crosslink_dir.join(READINESS_DIR);
    fs::create_dir_all(&directory)
        .with_context(|| format!("creating readiness directory {}", directory.display()))?;
    let _append = acquire_record_append_lease(&directory)?;
    let sequence = latest_sequence(&directory)?.saturating_add(1);
    let mut observation_errors = Vec::new();
    let source_fingerprint = match source_fingerprint(crosslink_dir) {
        Ok(value) => value,
        Err(error) if !draft.state.grants_mutations() => {
            let detail = format!("source observation failed: {error:#}");
            observation_errors.push(detail.clone());
            format!(
                "unavailable:{}",
                hex::encode(Sha256::digest(detail.as_bytes()))
            )
        }
        Err(error) => return Err(error),
    };
    let projection_frontier = match projection_frontier(crosslink_dir) {
        Ok(value) => value,
        Err(error) if !draft.state.grants_mutations() => {
            observation_errors.push(format!("projection frontier observation failed: {error:#}"));
            None
        }
        Err(error) => return Err(error),
    };
    let projection_schema_version = match projection_schema_version(crosslink_dir) {
        Ok(value) => value,
        Err(error) if !draft.state.grants_mutations() => {
            observation_errors.push(format!("projection schema observation failed: {error:#}"));
            None
        }
        Err(error) => return Err(error),
    };
    if draft.state.grants_mutations() {
        anyhow::ensure!(
            projection_frontier.is_some() && projection_is_current(crosslink_dir)?,
            "refusing to publish ready state for a stale projection"
        );
        anyhow::ensure!(
            projection_schema_version == Some(crate::db::SCHEMA_VERSION),
            "refusing to publish ready state for an incompatible projection schema"
        );
    }
    let mut reason = draft.reason.map(str::to_string);
    if !observation_errors.is_empty() {
        let observations = observation_errors.join("; ");
        reason = Some(match reason {
            Some(existing) => format!("{existing}; {observations}"),
            None => observations,
        });
    }
    let repository_id = repository_id(crosslink_dir)?;
    let observation = if matches!(
        draft.state,
        ReadinessState::WaitingForRemote | ReadinessState::BlockedCorrupt
    ) || !observation_errors.is_empty()
    {
        Some(write_observation_evidence(
            crosslink_dir,
            sequence,
            &repository_id,
            draft.attempt_id,
            &source_fingerprint,
            draft.state,
            reason.as_deref(),
        )?)
    } else {
        None
    };
    let record = ReadinessRecord {
        schema_version: READINESS_SCHEMA_VERSION,
        protocol_version: READINESS_PROTOCOL_VERSION,
        sequence,
        repository_id,
        daemon_epoch: draft.daemon_epoch.to_string(),
        daemon_pid: draft.daemon_pid,
        attempt_id: draft.attempt_id.to_string(),
        source_fingerprint,
        generation_id: draft.generation_id.map(str::to_string),
        projection_frontier,
        projection_schema_version,
        updated_at: Utc::now().to_rfc3339(),
        state: draft.state,
        reason,
        evidence_path: observation
            .as_ref()
            .map(|evidence| evidence.path.display().to_string()),
        evidence_sha256: observation.map(|evidence| evidence.sha256),
    };
    let bytes = serde_json::to_vec_pretty(&record).context("serializing readiness record")?;
    let temporary = directory.join(format!(".{sequence:020}-{}.tmp", Uuid::new_v4()));
    let destination = directory.join(format!("{sequence:020}-{}.json", Uuid::new_v4()));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)
        .with_context(|| format!("creating readiness temporary file {}", temporary.display()))?;
    file.write_all(&bytes)
        .context("writing readiness temporary file")?;
    file.sync_all()
        .context("syncing readiness temporary file")?;
    crate::utils::durable_rename(&temporary, &destination, false).with_context(|| {
        format!(
            "publishing readiness record {} to {}",
            temporary.display(),
            destination.display()
        )
    })?;
    sync_directory(&directory)?;
    prune_records(&directory)?;
    Ok(record)
}

fn acquire_record_append_lease(directory: &Path) -> Result<OperationLease> {
    let path = directory.join(RECORD_APPEND_FILE);
    let owner = PermitOwner::current()?;
    let contents = serde_json::to_vec(&owner).context("serializing readiness append owner")?;
    let mut liveness_sweep = 0_u8;
    loop {
        match create_owned_file(&path, &contents) {
            Ok(()) => return Ok(OperationLease { path, contents }),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if liveness_probe_due(&mut liveness_sweep) && remove_stale_owned_file(&path)? {
                    continue;
                }
                thread::sleep(Duration::from_millis(PERMIT_POLL_MILLIS));
            }
            Err(error) => return Err(error).context("acquiring readiness append lease"),
        }
    }
}

pub fn read_record(crosslink_dir: &Path) -> Result<Option<ReadinessRecord>> {
    let directory = crosslink_dir.join(READINESS_DIR);
    if !directory.is_dir() {
        return Ok(None);
    }
    let mut records = record_paths(&directory)?;
    records.sort();
    let Some(path) = records.last() else {
        return Ok(None);
    };
    let bytes =
        fs::read(path).with_context(|| format!("reading readiness record {}", path.display()))?;
    let record = serde_json::from_slice::<ReadinessRecord>(&bytes)
        .with_context(|| format!("parsing readiness record {}", path.display()))?;
    Ok(Some(record))
}

pub fn require_mutation_ready(crosslink_dir: &Path) -> Result<()> {
    if !requires_readiness(crosslink_dir) {
        return Ok(());
    }
    let record = read_record(crosslink_dir)?.ok_or_else(|| {
        anyhow::anyhow!(
            "repository readiness is missing; run `crosslink daemon ensure --wait-ready --json`"
        )
    })?;
    validate_record(crosslink_dir, &record)?;
    if !record.state.grants_mutations() {
        bail!(
            "repository is {}; run `crosslink daemon ensure --wait-ready --json`{}",
            state_name(record.state),
            record
                .reason
                .as_deref()
                .map_or_else(String::new, |reason| format!(": {reason}"))
        );
    }
    Ok(())
}

pub fn refresh_ready_record_after_projection(crosslink_dir: &Path) -> Result<()> {
    if !requires_readiness(crosslink_dir)
        || crosslink_dir
            .join(READINESS_DIR)
            .join(TRANSITION_FILE)
            .exists()
    {
        return Ok(());
    }
    let Some(record) = read_record(crosslink_dir)? else {
        return Ok(());
    };
    if !record.state.grants_mutations() {
        return Ok(());
    }
    let current_frontier = projection_frontier(crosslink_dir)?
        .ok_or_else(|| anyhow::anyhow!("projection frontier is missing"))?;
    if record.projection_frontier.as_deref() == Some(current_frontier.as_str()) {
        return Ok(());
    }
    anyhow::ensure!(
        projection_is_current(crosslink_dir)?,
        "local projection is not hydrated to the current authority frontier"
    );
    anyhow::ensure!(
        projection_schema_version(crosslink_dir)? == Some(crate::db::SCHEMA_VERSION),
        "projection schema is not current"
    );
    anyhow::ensure!(
        record.source_fingerprint == source_fingerprint(crosslink_dir)?,
        "repository format changed while refreshing readiness"
    );
    let identity = read_daemon_identity(crosslink_dir)?
        .ok_or_else(|| anyhow::anyhow!("daemon identity is missing"))?;
    anyhow::ensure!(
        (
            identity.repository_id.as_str(),
            identity.daemon_epoch.as_str(),
            identity.pid,
        ) == (
            record.repository_id.as_str(),
            record.daemon_epoch.as_str(),
            record.daemon_pid,
        ) && daemon_identity_is_live(&identity),
        "readiness record does not match the active daemon epoch"
    );
    let sync = crate::sync::SyncManager::new(crosslink_dir)?;
    let descriptor_generation =
        super::publication::generation_id_at_ref(sync.cache_path(), GENERATION_REF)?;
    anyhow::ensure!(
        record.generation_id.as_deref() == descriptor_generation.as_deref(),
        "readiness generation identifier does not match the verified descriptor"
    );
    write_record(
        crosslink_dir,
        ReadinessDraft {
            daemon_epoch: &record.daemon_epoch,
            daemon_pid: record.daemon_pid,
            attempt_id: &record.attempt_id,
            state: record.state,
            generation_id: record.generation_id.as_deref(),
            reason: record.reason.as_deref(),
        },
    )?;
    Ok(())
}

pub fn acquire_mutation_permit(crosslink_dir: &Path) -> Result<Option<MutationPermit>> {
    if !requires_readiness(crosslink_dir) {
        return Ok(None);
    }
    let permit = acquire_raw_mutation_permit(crosslink_dir)?;
    if let Err(error) = require_mutation_ready(crosslink_dir) {
        drop(permit);
        return Err(error);
    }
    Ok(Some(permit))
}

pub fn acquire_mutation_operation_permit(crosslink_dir: &Path) -> Result<MutationOperationPermit> {
    let directory = crosslink_dir.join(READINESS_DIR);
    fs::create_dir_all(&directory)
        .with_context(|| format!("creating readiness directory {}", directory.display()))?;
    let path = directory.join(OPERATION_FILE);
    if let Some(authority) =
        HELD_OPERATIONS.with(|held| held.borrow().get(&path).and_then(Weak::upgrade))
    {
        return Ok(MutationOperationPermit {
            _authority: authority,
        });
    }
    let owner = PermitOwner::current()?;
    let contents = serde_json::to_vec(&owner).context("serializing mutation operation owner")?;
    let mut liveness_sweep = 0_u8;
    let operation = loop {
        match create_owned_file(&path, &contents) {
            Ok(()) => {
                break OperationLease {
                    path: path.clone(),
                    contents,
                };
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if liveness_probe_due(&mut liveness_sweep) && remove_stale_owned_file(&path)? {
                    continue;
                }
                thread::sleep(Duration::from_millis(PERMIT_POLL_MILLIS));
            }
            Err(error) => return Err(error).context("acquiring mutation operation permit"),
        }
    };
    let readiness = acquire_mutation_permit(crosslink_dir)?;
    let authority = Arc::new(OperationAuthority {
        _readiness: readiness,
        _operation: operation,
    });
    HELD_OPERATIONS.with(|held| {
        held.borrow_mut().insert(path, Arc::downgrade(&authority));
    });
    Ok(MutationOperationPermit {
        _authority: authority,
    })
}

pub fn try_acquire_mutation_operation_permit(
    crosslink_dir: &Path,
) -> Result<Option<MutationOperationPermit>> {
    if requires_readiness(crosslink_dir) {
        require_mutation_ready(crosslink_dir)?;
    }
    let directory = crosslink_dir.join(READINESS_DIR);
    fs::create_dir_all(&directory)
        .with_context(|| format!("creating readiness directory {}", directory.display()))?;
    let path = directory.join(OPERATION_FILE);
    if let Some(authority) =
        HELD_OPERATIONS.with(|held| held.borrow().get(&path).and_then(Weak::upgrade))
    {
        return Ok(Some(MutationOperationPermit {
            _authority: authority,
        }));
    }
    let owner = PermitOwner::current()?;
    let contents = serde_json::to_vec(&owner).context("serializing mutation operation owner")?;
    let operation = loop {
        match create_owned_file(&path, &contents) {
            Ok(()) => {
                break OperationLease {
                    path: path.clone(),
                    contents,
                };
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if remove_stale_owned_file(&path)? {
                    continue;
                }
                return Ok(None);
            }
            Err(error) => return Err(error).context("trying mutation operation permit"),
        }
    };
    let readiness = match acquire_mutation_permit(crosslink_dir) {
        Ok(readiness) => readiness,
        Err(error) => {
            drop(operation);
            return Err(error);
        }
    };
    let authority = Arc::new(OperationAuthority {
        _readiness: readiness,
        _operation: operation,
    });
    HELD_OPERATIONS.with(|held| {
        held.borrow_mut().insert(path, Arc::downgrade(&authority));
    });
    Ok(Some(MutationOperationPermit {
        _authority: authority,
    }))
}

pub fn mutation_operation_is_held(crosslink_dir: &Path) -> bool {
    let path = crosslink_dir.join(READINESS_DIR).join(OPERATION_FILE);
    HELD_OPERATIONS.with(|held| held.borrow().get(&path).and_then(Weak::upgrade).is_some())
}

pub fn has_active_mutation_permits(crosslink_dir: &Path) -> Result<bool> {
    let directory = crosslink_dir.join(READINESS_DIR).join(MUTATION_PERMITS_DIR);
    if !directory.is_dir() {
        return Ok(false);
    }
    for entry in fs::read_dir(&directory)
        .with_context(|| format!("reading mutation permit directory {}", directory.display()))?
    {
        let path = entry?.path();
        if path
            .extension()
            .is_none_or(|extension| extension != "permit")
        {
            continue;
        }
        if !remove_stale_owned_file(&path)? {
            return Ok(true);
        }
    }
    Ok(false)
}

pub fn acquire_transition_permit(crosslink_dir: &Path) -> Result<TransitionPermit> {
    acquire_transition_permit_interruptible(crosslink_dir, None, None)
}

pub fn acquire_transition_permit_interruptible(
    crosslink_dir: &Path,
    shutdown: Option<&AtomicBool>,
    timeout: Option<Duration>,
) -> Result<TransitionPermit> {
    acquire_transition_permit_observed(crosslink_dir, shutdown, timeout, || Ok(()))
}

pub fn acquire_transition_permit_observed<F>(
    crosslink_dir: &Path,
    shutdown: Option<&AtomicBool>,
    timeout: Option<Duration>,
    mut observe_wait: F,
) -> Result<TransitionPermit>
where
    F: FnMut() -> Result<()>,
{
    let directory = crosslink_dir.join(READINESS_DIR);
    fs::create_dir_all(directory.join(MUTATION_PERMITS_DIR)).with_context(|| {
        format!(
            "creating mutation permit directory {}",
            directory.join(MUTATION_PERMITS_DIR).display()
        )
    })?;
    let path = directory.join(TRANSITION_FILE);
    let owner = PermitOwner::current()?;
    let contents = serde_json::to_vec(&owner).context("serializing transition owner")?;
    loop {
        match create_owned_file(&path, &contents) {
            Ok(()) => break,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if !remove_stale_owned_file(&path)? {
                    bail!("repository reconciliation transition is already active");
                }
            }
            Err(error) => return Err(error).context("acquiring reconciliation transition permit"),
        }
    }
    let permit = TransitionPermit { path, contents };
    wait_for_mutation_permits(crosslink_dir, shutdown, timeout, &mut observe_wait)?;
    Ok(permit)
}

pub fn validate_record(crosslink_dir: &Path, record: &ReadinessRecord) -> Result<()> {
    anyhow::ensure!(
        record.schema_version == READINESS_SCHEMA_VERSION,
        "unsupported readiness schema {}",
        record.schema_version
    );
    anyhow::ensure!(
        record.protocol_version == READINESS_PROTOCOL_VERSION,
        "unsupported readiness protocol {}",
        record.protocol_version
    );
    let updated_at = chrono::DateTime::parse_from_rfc3339(&record.updated_at)
        .context("readiness timestamp is invalid")?
        .with_timezone(&Utc);
    let age_seconds = Utc::now().signed_duration_since(updated_at).num_seconds();
    if record.state != ReadinessState::BlockedCorrupt {
        anyhow::ensure!(
            age_seconds.abs() <= MAX_RECORD_AGE_SECONDS,
            "readiness record is stale"
        );
    }
    anyhow::ensure!(
        record.repository_id == repository_id(crosslink_dir)?,
        "readiness record belongs to a different repository"
    );
    validate_observation_evidence(crosslink_dir, record)?;
    let identity = read_daemon_identity(crosslink_dir)?
        .ok_or_else(|| anyhow::anyhow!("daemon identity is missing"))?;
    anyhow::ensure!(
        identity.schema_version == READINESS_SCHEMA_VERSION,
        "unsupported daemon identity schema {}",
        identity.schema_version
    );
    anyhow::ensure!(
        (
            identity.repository_id.as_str(),
            identity.daemon_epoch.as_str(),
            identity.pid,
        ) == (
            record.repository_id.as_str(),
            record.daemon_epoch.as_str(),
            record.daemon_pid,
        ),
        "readiness record does not match the active daemon epoch"
    );
    anyhow::ensure!(
        daemon_identity_is_live(&identity),
        "readiness daemon process {} is not running",
        record.daemon_pid
    );
    if record.state.grants_mutations() {
        let current_generation = reconciliation_generation_frontier(crosslink_dir)?;
        anyhow::ensure!(
            current_generation.is_some(),
            "reconciliation generation is missing"
        );
        let current_frontier = projection_frontier(crosslink_dir)?;
        anyhow::ensure!(
            record.projection_frontier.is_some() && current_frontier.is_some(),
            "projection frontier is missing"
        );
        anyhow::ensure!(
            record.projection_frontier == current_frontier,
            "readiness record projection frontier is stale"
        );
        anyhow::ensure!(
            projection_is_current(crosslink_dir)?,
            "local projection is not hydrated to the current authority frontier"
        );
        anyhow::ensure!(
            record.projection_schema_version == Some(crate::db::SCHEMA_VERSION)
                && projection_schema_version(crosslink_dir)? == Some(crate::db::SCHEMA_VERSION),
            "projection schema changed after readiness was recorded"
        );
        anyhow::ensure!(
            record.source_fingerprint == source_fingerprint(crosslink_dir)?,
            "repository format changed after readiness was recorded"
        );
        let recorded_generation = record
            .projection_frontier
            .as_deref()
            .and_then(|frontier| frontier.split(';').next())
            .and_then(|value| value.strip_prefix("generation="));
        anyhow::ensure!(
            recorded_generation == current_generation.as_deref(),
            "reconciliation generation changed after readiness was recorded"
        );
        let sync = crate::sync::SyncManager::new(crosslink_dir)?;
        let descriptor_generation =
            super::publication::generation_id_at_ref(sync.cache_path(), GENERATION_REF)?;
        anyhow::ensure!(
            record.generation_id.as_deref() == descriptor_generation.as_deref(),
            "readiness generation identifier does not match the verified descriptor"
        );
    }
    Ok(())
}

fn validate_observation_evidence(crosslink_dir: &Path, record: &ReadinessRecord) -> Result<()> {
    let required = matches!(
        record.state,
        ReadinessState::WaitingForRemote | ReadinessState::BlockedCorrupt
    );
    let (path, expected_digest) = match (&record.evidence_path, &record.evidence_sha256) {
        (Some(path), Some(digest)) => (Path::new(path), digest),
        (None, None) if !required => return Ok(()),
        (None, None) => bail!("readiness state has no bound observation evidence"),
        _ => bail!("readiness observation evidence binding is incomplete"),
    };
    let integrity = crosslink_dir.join(crate::db::snapshot::SNAPSHOT_DIR);
    anyhow::ensure!(
        path.parent() == Some(integrity.as_path())
            && path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    name.starts_with("reconciliation-observation-")
                        && Path::new(name)
                            .extension()
                            .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
                }),
        "readiness observation evidence path is outside the repository evidence directory"
    );
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("reading readiness observation evidence {}", path.display()))?;
    anyhow::ensure!(
        metadata.file_type().is_file(),
        "readiness observation evidence is not a regular file"
    );
    let bytes = fs::read(path)
        .with_context(|| format!("reading readiness observation evidence {}", path.display()))?;
    anyhow::ensure!(
        expected_digest.len() == 64
            && expected_digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            && hex::encode(Sha256::digest(&bytes)) == *expected_digest,
        "readiness observation evidence digest does not match"
    );
    let evidence: ReconciliationObservationEvidence = serde_json::from_slice(&bytes)
        .context("readiness observation evidence schema is invalid")?;
    anyhow::ensure!(
        evidence.schema_version == READINESS_SCHEMA_VERSION
            && evidence.protocol_version == READINESS_PROTOCOL_VERSION,
        "readiness observation evidence version is unsupported"
    );
    anyhow::ensure!(
        evidence.sequence == record.sequence
            && evidence.repository_id == record.repository_id
            && evidence.attempt_id == record.attempt_id
            && evidence.source_fingerprint == record.source_fingerprint
            && evidence.state == record.state
            && evidence.reason == record.reason,
        "readiness observation evidence does not match its record"
    );
    chrono::DateTime::parse_from_rfc3339(&evidence.observed_at)
        .context("readiness observation evidence timestamp is invalid")?;
    Ok(())
}

pub fn requires_readiness(crosslink_dir: &Path) -> bool {
    crosslink_dir.join("hook-config.json").is_file()
}

pub fn write_daemon_identity(crosslink_dir: &Path, identity: &DaemonIdentity) -> Result<()> {
    let path = crosslink_dir.join(DAEMON_IDENTITY_FILE);
    if path.is_file() {
        let existing = read_daemon_identity(crosslink_dir)?;
        anyhow::ensure!(
            existing.as_ref() == Some(identity),
            "daemon identity already belongs to another epoch"
        );
        return Ok(());
    }
    let temporary = crosslink_dir.join(format!(".daemon-{}.tmp", Uuid::new_v4()));
    let bytes = serde_json::to_vec(identity).context("serializing daemon identity")?;
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)
        .with_context(|| {
            format!(
                "creating daemon identity temporary file {}",
                temporary.display()
            )
        })?;
    file.write_all(&bytes)
        .context("writing daemon identity temporary file")?;
    file.sync_all()
        .context("syncing daemon identity temporary file")?;
    match fs::hard_link(&temporary, &path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let existing = read_daemon_identity(crosslink_dir)?;
            anyhow::ensure!(
                existing.as_ref() == Some(identity),
                "daemon identity already belongs to another epoch"
            );
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "publishing daemon identity {} to {}",
                    temporary.display(),
                    path.display()
                )
            });
        }
    }
    fs::remove_file(&temporary).with_context(|| {
        format!(
            "removing daemon identity temporary file {}",
            temporary.display()
        )
    })?;
    sync_directory(crosslink_dir)?;
    Ok(())
}

pub fn read_daemon_identity(crosslink_dir: &Path) -> Result<Option<DaemonIdentity>> {
    let path = crosslink_dir.join(DAEMON_IDENTITY_FILE);
    if !path.is_file() {
        return Ok(None);
    }
    let bytes =
        fs::read(&path).with_context(|| format!("reading daemon identity {}", path.display()))?;
    daemon_identity_from_bytes(crosslink_dir, bytes).map(Some)
}

fn daemon_identity_from_bytes(crosslink_dir: &Path, bytes: Vec<u8>) -> Result<DaemonIdentity> {
    if let Ok(identity) = serde_json::from_slice::<DaemonIdentity>(&bytes) {
        return Ok(identity);
    }
    let pid = String::from_utf8(bytes)
        .context("legacy daemon PID file was not UTF-8")?
        .trim()
        .parse::<u32>()
        .context("legacy daemon PID file was invalid")?;
    Ok(DaemonIdentity {
        schema_version: READINESS_SCHEMA_VERSION,
        repository_id: repository_id(crosslink_dir)?,
        daemon_epoch: "legacy".to_string(),
        pid,
        process_start: String::new(),
    })
}

pub fn remove_daemon_identity_if(crosslink_dir: &Path, expected: &DaemonIdentity) -> Result<()> {
    remove_daemon_identity_if_with(crosslink_dir, expected, |_| Ok(()))
}

fn remove_daemon_identity_if_with<F>(
    crosslink_dir: &Path,
    expected: &DaemonIdentity,
    after_claim: F,
) -> Result<()>
where
    F: FnOnce(&Path) -> Result<()>,
{
    let path = crosslink_dir.join(DAEMON_IDENTITY_FILE);
    if !path.exists() {
        return Ok(());
    }
    let quarantine = crosslink_dir.join(format!(".daemon-remove-{}.tmp", Uuid::new_v4()));
    match crate::utils::durable_rename(&path, &quarantine, false) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "claiming daemon identity {} as {}",
                    path.display(),
                    quarantine.display()
                )
            })
        }
    }
    if let Err(error) = after_claim(&path) {
        let retained = restore_daemon_identity_claim(&path, &quarantine)?;
        if retained {
            bail!(
                "daemon identity removal failed after claim; retained claim at {}: {error:#}",
                quarantine.display()
            );
        }
        return Err(error).context("daemon identity removal failed after claim and was restored");
    }
    let contents = fs::read(&quarantine)
        .with_context(|| format!("reading claimed daemon identity {}", quarantine.display()))?;
    let current = match daemon_identity_from_bytes(crosslink_dir, contents) {
        Ok(identity) => identity,
        Err(error) => {
            let retained = restore_daemon_identity_claim(&path, &quarantine)?;
            if retained {
                bail!(
                    "claimed daemon identity was malformed and replacement exists; retained claim at {}: {error:#}",
                    quarantine.display()
                );
            }
            bail!("daemon identity was malformed and was restored: {error:#}");
        }
    };
    if current != *expected {
        let retained = restore_daemon_identity_claim(&path, &quarantine)?;
        if retained {
            bail!(
                "daemon identity changed before removal; retained claim at {}",
                quarantine.display()
            );
        }
        bail!("daemon identity changed before removal and was restored");
    }
    fs::remove_file(&quarantine)
        .with_context(|| format!("removing claimed daemon identity {}", quarantine.display()))?;
    sync_directory(crosslink_dir)?;
    Ok(())
}

fn restore_daemon_identity_claim(path: &Path, quarantine: &Path) -> Result<bool> {
    match fs::hard_link(quarantine, path) {
        Ok(()) => {
            fs::remove_file(quarantine).with_context(|| {
                format!("removing restored identity claim {}", quarantine.display())
            })?;
            Ok(false)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(true),
        Err(error) => {
            Err(error).with_context(|| format!("restoring daemon identity {}", path.display()))
        }
    }
}

#[cfg(windows)]
pub fn is_process_running(pid: u32) -> bool {
    process_start_token(pid).is_some()
}

pub fn daemon_identity_is_live(identity: &DaemonIdentity) -> bool {
    process_identity_is_live(identity.pid, &identity.process_start)
}

pub fn process_identity_is_live(pid: u32, process_start: &str) -> bool {
    process_start_token(pid).as_deref() == Some(process_start)
}

pub fn current_process_start_token() -> Result<String> {
    static CURRENT_PROCESS_START: OnceLock<String> = OnceLock::new();
    if let Some(token) = CURRENT_PROCESS_START.get() {
        return Ok(token.clone());
    }
    let token = process_start_token_for(std::process::id())?;
    let _ = CURRENT_PROCESS_START.set(token.clone());
    Ok(token)
}

pub fn process_start_token_for(pid: u32) -> Result<String> {
    for _ in 0..20 {
        if let Some(token) = process_start_token(pid) {
            return Ok(token);
        }
        thread::sleep(Duration::from_millis(10));
    }
    bail!("unable to determine process start identity for PID {pid}")
}

#[cfg(target_os = "linux")]
fn process_start_token(pid: u32) -> Option<String> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, fields) = stat.rsplit_once(") ")?;
    fields.split_whitespace().nth(19).map(str::to_string)
}

#[cfg(all(unix, not(target_os = "linux")))]
fn process_start_token(pid: u32) -> Option<String> {
    let output = Command::new("ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(windows)]
fn process_start_token(pid: u32) -> Option<String> {
    use std::ffi::c_void;

    #[repr(C)]
    #[derive(Default)]
    struct FileTime {
        low: u32,
        high: u32,
    }

    #[link(name = "kernel32")]
    extern "system" {
        #[link_name = "OpenProcess"]
        fn open_process(access: u32, inherit: i32, process_id: u32) -> *mut c_void;
        #[link_name = "GetProcessTimes"]
        fn get_process_times(
            process: *mut c_void,
            created: *mut FileTime,
            exited: *mut FileTime,
            kernel: *mut FileTime,
            user: *mut FileTime,
        ) -> i32;
        #[link_name = "CloseHandle"]
        fn close_handle(handle: *mut c_void) -> i32;
    }

    let handle = unsafe { open_process(0x1000, 0, pid) };
    if handle.is_null() {
        return None;
    }
    let mut created = FileTime::default();
    let mut exited = FileTime::default();
    let mut kernel = FileTime::default();
    let mut user = FileTime::default();
    let result =
        unsafe { get_process_times(handle, &mut created, &mut exited, &mut kernel, &mut user) };
    unsafe {
        close_handle(handle);
    }
    (result != 0).then(|| {
        let file_time = (u64::from(created.high) << 32) | u64::from(created.low);
        file_time
            .saturating_add(504_911_232_000_000_000)
            .to_string()
    })
}

#[cfg(not(windows))]
pub fn is_process_running(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

const fn state_name(state: ReadinessState) -> &'static str {
    match state {
        ReadinessState::Starting => "starting",
        ReadinessState::Reconciling => "reconciling",
        ReadinessState::Rebuilding => "rebuilding",
        ReadinessState::ReadyCurrent => "ready_current",
        ReadinessState::ReadyMigrated => "ready_migrated",
        ReadinessState::ReadyAdopted => "ready_adopted",
        ReadinessState::WaitingForRemote => "waiting_for_remote",
        ReadinessState::BlockedCorrupt => "blocked_corrupt",
    }
}

fn evidence_path(crosslink_dir: &Path) -> Option<String> {
    let journal = crosslink_dir.join("reconciliation-journal.json");
    if journal.is_file() {
        return Some(journal.display().to_string());
    }
    let integrity = crosslink_dir.join(crate::db::snapshot::SNAPSHOT_DIR);
    let mut evidence = fs::read_dir(integrity)
        .ok()?
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| {
                        name.starts_with("reconciliation-before-")
                            || name.starts_with("reconciliation-shadow-")
                            || name.starts_with("reconciliation-observation-")
                    })
        })
        .collect::<Vec<_>>();
    evidence.sort();
    evidence.pop().map(|path| path.display().to_string())
}

fn write_observation_evidence(
    crosslink_dir: &Path,
    sequence: u64,
    repository_id: &str,
    attempt_id: &str,
    source_fingerprint: &str,
    state: ReadinessState,
    reason: Option<&str>,
) -> Result<PublishedObservationEvidence> {
    let integrity = crosslink_dir.join(crate::db::snapshot::SNAPSHOT_DIR);
    fs::create_dir_all(&integrity)
        .with_context(|| format!("creating reconciliation evidence {}", integrity.display()))?;
    let destination = integrity.join(format!(
        "reconciliation-observation-{sequence:020}-{}.json",
        Uuid::new_v4()
    ));
    let temporary = integrity.join(format!(
        ".reconciliation-observation-{}.tmp",
        Uuid::new_v4()
    ));
    let evidence = ReconciliationObservationEvidence {
        schema_version: READINESS_SCHEMA_VERSION,
        protocol_version: READINESS_PROTOCOL_VERSION,
        sequence,
        repository_id: repository_id.to_string(),
        attempt_id: attempt_id.to_string(),
        source_fingerprint: source_fingerprint.to_string(),
        state,
        observed_at: Utc::now().to_rfc3339(),
        reason: reason.map(str::to_string),
        publication_journal: crosslink_dir
            .join("reconciliation-journal.json")
            .is_file()
            .then(|| {
                crosslink_dir
                    .join("reconciliation-journal.json")
                    .display()
                    .to_string()
            }),
        related_evidence: evidence_path(crosslink_dir),
    };
    let bytes = serde_json::to_vec_pretty(&evidence)
        .context("serializing reconciliation observation evidence")?;
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)
        .with_context(|| format!("creating observation evidence {}", temporary.display()))?;
    file.write_all(&bytes)
        .context("writing reconciliation observation evidence")?;
    file.sync_all()
        .context("syncing reconciliation observation evidence")?;
    crate::utils::durable_rename(&temporary, &destination, false).with_context(|| {
        format!(
            "publishing reconciliation observation evidence {}",
            destination.display()
        )
    })?;
    sync_directory(&integrity)?;
    let mut observations = fs::read_dir(&integrity)?
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    name.starts_with("reconciliation-observation-")
                        && Path::new(name)
                            .extension()
                            .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
                })
        })
        .collect::<Vec<_>>();
    observations.sort();
    let remove_count = observations.len().saturating_sub(16);
    for path in observations.into_iter().take(remove_count) {
        fs::remove_file(&path)
            .with_context(|| format!("pruning reconciliation observation {}", path.display()))?;
    }
    if remove_count > 0 {
        sync_directory(&integrity)?;
    }
    Ok(PublishedObservationEvidence {
        path: destination,
        sha256: hex::encode(Sha256::digest(&bytes)),
    })
}

impl PermitOwner {
    fn current() -> Result<Self> {
        Ok(Self {
            pid: std::process::id(),
            process_start: current_process_start_token()?,
            token: Uuid::new_v4().to_string(),
        })
    }

    fn is_live(&self) -> bool {
        process_start_token(self.pid).as_deref() == Some(self.process_start.as_str())
    }
}

fn acquire_raw_mutation_permit(crosslink_dir: &Path) -> Result<MutationPermit> {
    let readiness_dir = crosslink_dir.join(READINESS_DIR);
    let directory = readiness_dir.join(MUTATION_PERMITS_DIR);
    fs::create_dir_all(&directory)
        .with_context(|| format!("creating mutation permit directory {}", directory.display()))?;
    let owner = PermitOwner::current()?;
    let contents = serde_json::to_vec(&owner).context("serializing mutation permit owner")?;
    let path = directory.join(format!("{}.permit", owner.token));
    let deadline = Instant::now() + Duration::from_secs(MUTATION_TRANSITION_WAIT_SECS);
    loop {
        if transition_is_live(&readiness_dir)? {
            if Instant::now() >= deadline {
                bail!(
                    "timed out waiting {MUTATION_TRANSITION_WAIT_SECS} seconds for repository reconciliation transition"
                );
            }
            thread::sleep(Duration::from_millis(PERMIT_POLL_MILLIS));
            continue;
        }
        create_owned_file(&path, &contents).context("acquiring mutation permit")?;
        let permit = MutationPermit {
            path: path.clone(),
            contents: contents.clone(),
        };
        if !transition_is_live(&readiness_dir)? {
            return Ok(permit);
        }
        drop(permit);
        if Instant::now() >= deadline {
            bail!(
                "timed out waiting {MUTATION_TRANSITION_WAIT_SECS} seconds for repository reconciliation transition"
            );
        }
        thread::sleep(Duration::from_millis(PERMIT_POLL_MILLIS));
    }
}

fn wait_for_mutation_permits(
    crosslink_dir: &Path,
    shutdown: Option<&AtomicBool>,
    timeout: Option<Duration>,
    observe_wait: &mut impl FnMut() -> Result<()>,
) -> Result<()> {
    let directory = crosslink_dir.join(READINESS_DIR).join(MUTATION_PERMITS_DIR);
    let deadline = timeout.map(|duration| Instant::now() + duration);
    let mut liveness_sweep = 0_u8;
    let mut last_observation = Instant::now()
        .checked_sub(Duration::from_secs(30))
        .unwrap_or_else(Instant::now);
    loop {
        if last_observation.elapsed() >= Duration::from_secs(30) {
            observe_wait()?;
            last_observation = Instant::now();
        }
        if shutdown.is_some_and(|flag| flag.load(Ordering::SeqCst)) {
            bail!("reconciliation transition interrupted by daemon shutdown");
        }
        if deadline.is_some_and(|value| Instant::now() >= value) {
            bail!("timed out waiting for active repository mutations to finish");
        }
        let mut live = false;
        let probe_liveness = liveness_probe_due(&mut liveness_sweep);
        for entry in fs::read_dir(&directory)
            .with_context(|| format!("reading mutation permit directory {}", directory.display()))?
        {
            let path = entry?.path();
            if path
                .extension()
                .is_none_or(|extension| extension != "permit")
            {
                continue;
            }
            if if probe_liveness {
                !remove_stale_owned_file(&path)?
            } else {
                path.is_file()
            } {
                live = true;
            }
        }
        if !live {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(PERMIT_POLL_MILLIS));
    }
}

const fn liveness_probe_due(sweep: &mut u8) -> bool {
    let due = *sweep == 0;
    *sweep = (*sweep + 1) % LIVENESS_SWEEP_POLLS;
    due
}

fn transition_is_live(readiness_dir: &Path) -> Result<bool> {
    let path = readiness_dir.join(TRANSITION_FILE);
    if !path.is_file() {
        return Ok(false);
    }
    Ok(!remove_stale_owned_file(&path)?)
}

fn create_owned_file(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    create_owned_file_with(path, contents, |_| Ok(()))
}

fn create_owned_file_with<F>(path: &Path, contents: &[u8], before_publish: F) -> std::io::Result<()>
where
    F: FnOnce(&Path) -> std::io::Result<()>,
{
    let directory = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "ownership path has no parent",
        )
    })?;
    let temporary = directory.join(format!(".owner-{}.tmp", Uuid::new_v4()));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)?;
    file.write_all(contents)?;
    file.sync_all()?;
    drop(file);
    if let Err(error) = before_publish(&temporary) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    let result = fs::hard_link(&temporary, path);
    let remove_result = fs::remove_file(&temporary);
    if let Err(error) = result {
        let _ = remove_result;
        return Err(error);
    }
    if let Err(error) = remove_result {
        remove_owned_file(path, contents);
        return Err(error);
    }
    if let Err(error) = sync_directory(directory) {
        remove_owned_file(path, contents);
        return Err(std::io::Error::other(error));
    }
    Ok(())
}

fn remove_stale_owned_file(path: &Path) -> Result<bool> {
    let contents = match fs::read(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(error) => {
            return Err(error).with_context(|| format!("reading permit {}", path.display()))
        }
    };
    let owner = serde_json::from_slice::<PermitOwner>(&contents)
        .with_context(|| format!("ownership file {} is malformed", path.display()))?;
    if owner.is_live() {
        return Ok(false);
    }
    remove_owned_file(path, &contents);
    Ok(true)
}

fn remove_owned_file(path: &Path, contents: &[u8]) {
    if fs::read(path).ok().as_deref() == Some(contents) {
        let _ = fs::remove_file(path);
    }
}

fn reconciliation_source_refs(source: &str, repository: &Path) -> Result<Vec<String>> {
    let output = Command::new("git")
        .current_dir(repository)
        .args(["for-each-ref", "--format=%(refname)%00%(objectname)"])
        .output()
        .with_context(|| {
            format!(
                "reading reconciliation source refs in {}",
                repository.display()
            )
        })?;
    if !output.status.success() {
        return Ok(Vec::new());
    }
    Ok(String::from_utf8(output.stdout)
        .context("reconciliation source refs were not UTF-8")?
        .lines()
        .filter_map(|line| {
            let (name, oid) = line.split_once('\0')?;
            let relevant = name == "refs/heads/crosslink/hub"
                || name == "refs/heads/crosslink/locks"
                || name == GENERATION_REF
                || name == crate::hub_v3::OLD_META_REF
                || name == crate::hub_v3::OLD_CHECKPOINT_REF
                || name.starts_with(crate::hub_v3::OLD_AGENT_REF_PREFIX)
                || name.ends_with("/crosslink/hub")
                || name.ends_with("/crosslink/locks");
            relevant.then(|| format!("{source}:{name}:{oid}"))
        })
        .collect())
}

fn latest_sequence(directory: &Path) -> Result<u64> {
    record_paths(directory)?
        .into_iter()
        .filter_map(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.split_once('-'))
                .and_then(|(sequence, _)| sequence.parse::<u64>().ok())
        })
        .max()
        .map_or(Ok(0), Ok)
}

fn record_paths(directory: &Path) -> Result<Vec<PathBuf>> {
    let entries = fs::read_dir(directory)
        .with_context(|| format!("reading readiness directory {}", directory.display()))?;
    Ok(entries
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect())
}

fn prune_records(directory: &Path) -> Result<()> {
    let mut records = record_paths(directory)?;
    records.sort();
    let remove_count = records.len().saturating_sub(MAX_READINESS_RECORDS);
    for path in records.into_iter().take(remove_count) {
        fs::remove_file(&path)
            .with_context(|| format!("pruning readiness record {}", path.display()))?;
    }
    if remove_count > 0 {
        sync_directory(directory)?;
    }
    Ok(())
}

#[cfg(unix)]
fn sync_directory(directory: &Path) -> Result<()> {
    fs::File::open(directory)
        .with_context(|| format!("opening directory {} for sync", directory.display()))?
        .sync_all()
        .with_context(|| format!("syncing directory {}", directory.display()))
}

#[cfg(windows)]
fn sync_directory(directory: &Path) -> Result<()> {
    let _ = directory;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;
    use std::sync::mpsc;

    fn initialized() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join(".git")).unwrap();
        let crosslink = root.path().join(".crosslink");
        fs::create_dir(&crosslink).unwrap();
        fs::write(crosslink.join("hook-config.json"), "{}").unwrap();
        fs::write(crosslink.join("agent.json"), "{}").unwrap();
        root
    }

    fn git_initialized() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        assert!(Command::new("git")
            .current_dir(root.path())
            .args(["init", "-q"])
            .status()
            .unwrap()
            .success());
        for (name, value) in [("user.email", "test@example.com"), ("user.name", "Test")] {
            assert!(Command::new("git")
                .current_dir(root.path())
                .args(["config", name, value])
                .status()
                .unwrap()
                .success());
        }
        fs::write(root.path().join("seed"), "one").unwrap();
        assert!(Command::new("git")
            .current_dir(root.path())
            .args(["add", "seed"])
            .status()
            .unwrap()
            .success());
        assert!(Command::new("git")
            .current_dir(root.path())
            .args(["commit", "-qm", "seed"])
            .status()
            .unwrap()
            .success());
        let crosslink = root.path().join(".crosslink");
        fs::create_dir(&crosslink).unwrap();
        fs::write(crosslink.join("hook-config.json"), "{}").unwrap();
        Database::open(&crosslink.join("issues.db")).unwrap();
        root
    }

    fn git(root: &Path, arguments: &[&str]) {
        assert!(Command::new("git")
            .current_dir(root)
            .args(arguments)
            .status()
            .unwrap()
            .success());
    }

    fn file_snapshot(path: &Path) -> Vec<(String, Vec<u8>)> {
        fn collect(root: &Path, path: &Path, entries: &mut Vec<(String, Vec<u8>)>) {
            let mut children = fs::read_dir(path)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect::<Vec<_>>();
            children.sort();
            for child in children {
                if child.is_dir() {
                    collect(root, &child, entries);
                } else {
                    entries.push((
                        child.strip_prefix(root).unwrap().display().to_string(),
                        fs::read(child).unwrap(),
                    ));
                }
            }
        }
        let mut entries = Vec::new();
        collect(path, path, &mut entries);
        entries
    }

    #[test]
    fn states_have_stable_wire_names_and_authority() {
        let cases = [
            (ReadinessState::Starting, "\"starting\"", false),
            (ReadinessState::Reconciling, "\"reconciling\"", false),
            (ReadinessState::Rebuilding, "\"rebuilding\"", false),
            (ReadinessState::ReadyCurrent, "\"ready_current\"", true),
            (ReadinessState::ReadyMigrated, "\"ready_migrated\"", true),
            (ReadinessState::ReadyAdopted, "\"ready_adopted\"", true),
            (
                ReadinessState::WaitingForRemote,
                "\"waiting_for_remote\"",
                false,
            ),
            (ReadinessState::BlockedCorrupt, "\"blocked_corrupt\"", false),
        ];
        for (state, wire, grants) in cases {
            assert_eq!(serde_json::to_string(&state).unwrap(), wire);
            assert_eq!(state.grants_mutations(), grants);
        }
    }

    #[test]
    fn daemon_response_is_closed_and_uses_only_terminal_outcomes() {
        let outcomes = [
            ReadinessOutcomeState::ReadyCurrent,
            ReadinessOutcomeState::ReadyMigrated,
            ReadinessOutcomeState::ReadyAdopted,
            ReadinessOutcomeState::WaitingForRemote,
            ReadinessOutcomeState::BlockedCorrupt,
        ];
        for outcome in outcomes {
            let mut response = DaemonResponse::stopped();
            response.state = Some(outcome);
            response.ready = outcome.grants_mutations();
            response.running = true;
            response.repository_id = Some("repository".to_string());
            response.daemon_epoch = Some("epoch".to_string());
            response.daemon_pid = Some(std::process::id());
            response.attempt_id = Some("attempt".to_string());
            response.updated_at = Some(Utc::now().to_rfc3339());
            if outcome.grants_mutations() {
                response.generation_id = Some("generation".to_string());
            } else {
                response.reason = Some("not ready".to_string());
                response.evidence_path = Some("evidence".to_string());
                response.evidence_sha256 = Some("0".repeat(64));
            }
            let bytes = serde_json::to_vec(&response).unwrap();
            assert_eq!(
                serde_json::from_slice::<DaemonResponse>(&bytes).unwrap(),
                response
            );
        }
        let stopped = DaemonResponse::stopped();
        assert_eq!(stopped.state, None);
        assert!(!stopped.running);
        let mut value = serde_json::to_value(stopped).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("unexpected".to_string(), serde_json::json!(true));
        assert!(serde_json::from_value::<DaemonResponse>(value).is_err());

        let mut inconsistent = serde_json::to_value({
            let mut response = DaemonResponse::stopped();
            response.state = Some(ReadinessOutcomeState::ReadyCurrent);
            response.ready = true;
            response.running = true;
            response.repository_id = Some("repository".to_string());
            response.daemon_epoch = Some("epoch".to_string());
            response.daemon_pid = Some(std::process::id());
            response.attempt_id = Some("attempt".to_string());
            response.generation_id = Some("generation".to_string());
            response.updated_at = Some(Utc::now().to_rfc3339());
            response
        })
        .unwrap();
        inconsistent["ready"] = serde_json::json!(false);
        assert!(serde_json::from_value::<DaemonResponse>(inconsistent).is_err());

        let mut retained = serde_json::to_value(DaemonResponse::stopped()).unwrap();
        retained["daemon_pid"] = serde_json::json!(std::process::id());
        assert!(serde_json::from_value::<DaemonResponse>(retained).is_err());
    }

    #[test]
    fn source_fingerprint_ignores_internal_work_refs_and_tracks_legacy_tips() {
        let root = git_initialized();
        let crosslink = root.path().join(".crosslink");
        let initial = source_fingerprint(&crosslink).unwrap();
        git(
            root.path(),
            &[
                "update-ref",
                "refs/crosslink/reconciliation/build/test",
                "HEAD",
            ],
        );
        assert_eq!(source_fingerprint(&crosslink).unwrap(), initial);
        let legacy_ref = format!("{}legacy", crate::hub_v3::OLD_AGENT_REF_PREFIX);
        git(root.path(), &["update-ref", &legacy_ref, "HEAD"]);
        let legacy = source_fingerprint(&crosslink).unwrap();
        assert_ne!(legacy, initial);
        fs::write(root.path().join("seed"), "two").unwrap();
        git(root.path(), &["add", "seed"]);
        git(root.path(), &["commit", "-qm", "advance"]);
        git(root.path(), &["update-ref", &legacy_ref, "HEAD"]);
        assert_ne!(source_fingerprint(&crosslink).unwrap(), legacy);
    }

    #[cfg(unix)]
    #[test]
    fn source_fingerprint_is_stable_across_repository_path_aliases() {
        let root = git_initialized();
        git(
            root.path(),
            &["update-ref", "refs/heads/crosslink/hub", "HEAD"],
        );
        let aliases = tempfile::tempdir().unwrap();
        let alias = aliases.path().join("repository");
        std::os::unix::fs::symlink(root.path(), &alias).unwrap();

        let direct = source_fingerprint(&root.path().join(".crosslink")).unwrap();
        let indirect = source_fingerprint(&alias.join(".crosslink")).unwrap();
        assert_eq!(direct, indirect);
    }

    #[test]
    fn readiness_records_are_append_only_and_latest_wins() {
        let root = initialized();
        let crosslink = root.path().join(".crosslink");
        let first = write_record(
            &crosslink,
            ReadinessDraft {
                daemon_epoch: "epoch",
                daemon_pid: std::process::id(),
                attempt_id: "one",
                state: ReadinessState::Starting,
                generation_id: None,
                reason: None,
            },
        )
        .unwrap();
        let second = write_record(
            &crosslink,
            ReadinessDraft {
                daemon_epoch: "epoch",
                daemon_pid: std::process::id(),
                attempt_id: "two",
                state: ReadinessState::WaitingForRemote,
                generation_id: None,
                reason: Some("offline"),
            },
        )
        .unwrap();
        assert_eq!(first.sequence, 1);
        assert_eq!(second.sequence, 2);
        assert_eq!(read_record(&crosslink).unwrap(), Some(second));
        assert_eq!(
            record_paths(&crosslink.join(READINESS_DIR)).unwrap().len(),
            2
        );
    }

    #[test]
    fn concurrent_readiness_appends_have_unique_monotonic_sequences() {
        let root = initialized();
        let crosslink = root.path().join(".crosslink");
        let barrier = std::sync::Barrier::new(9);
        let records = std::thread::scope(|scope| {
            let workers = (0..8)
                .map(|index| {
                    let crosslink = crosslink.clone();
                    let barrier = &barrier;
                    scope.spawn(move || {
                        barrier.wait();
                        write_record(
                            &crosslink,
                            ReadinessDraft {
                                daemon_epoch: "epoch",
                                daemon_pid: std::process::id(),
                                attempt_id: &format!("concurrent-{index}"),
                                state: ReadinessState::Starting,
                                generation_id: None,
                                reason: None,
                            },
                        )
                        .unwrap()
                    })
                })
                .collect::<Vec<_>>();
            barrier.wait();
            workers
                .into_iter()
                .map(|worker| worker.join().unwrap())
                .collect::<Vec<_>>()
        });
        let mut sequences = records
            .iter()
            .map(|record| record.sequence)
            .collect::<Vec<_>>();
        sequences.sort_unstable();
        assert_eq!(sequences, (1..=8).collect::<Vec<_>>());
        let latest = read_record(&crosslink).unwrap().unwrap();
        assert_eq!(latest.sequence, 8);
        assert_eq!(
            latest,
            records
                .into_iter()
                .max_by_key(|record| record.sequence)
                .unwrap()
        );
    }

    #[test]
    fn denied_mutation_and_operation_permits_leave_no_filesystem_artifacts() {
        for state in [
            ReadinessState::WaitingForRemote,
            ReadinessState::BlockedCorrupt,
        ] {
            let root = initialized();
            let crosslink = root.path().join(".crosslink");
            let identity = DaemonIdentity {
                schema_version: READINESS_SCHEMA_VERSION,
                repository_id: repository_id(&crosslink).unwrap(),
                daemon_epoch: format!("denied-{state:?}"),
                pid: std::process::id(),
                process_start: current_process_start_token().unwrap(),
            };
            write_daemon_identity(&crosslink, &identity).unwrap();
            write_record(
                &crosslink,
                ReadinessDraft {
                    daemon_epoch: &identity.daemon_epoch,
                    daemon_pid: identity.pid,
                    attempt_id: "denied-attempt",
                    state,
                    generation_id: None,
                    reason: Some("not ready"),
                },
            )
            .unwrap();
            let before = file_snapshot(&crosslink);
            assert!(acquire_mutation_permit(&crosslink).is_err());
            assert!(acquire_mutation_operation_permit(&crosslink).is_err());
            assert_eq!(file_snapshot(&crosslink), before);
        }
    }

    #[test]
    fn repository_identity_rejects_copied_records() {
        let first = initialized();
        let second = initialized();
        let first_dir = first.path().join(".crosslink");
        let second_dir = second.path().join(".crosslink");
        let record = write_record(
            &first_dir,
            ReadinessDraft {
                daemon_epoch: "epoch",
                daemon_pid: std::process::id(),
                attempt_id: "attempt",
                state: ReadinessState::Starting,
                generation_id: None,
                reason: None,
            },
        )
        .unwrap();
        let error = validate_record(&second_dir, &record)
            .unwrap_err()
            .to_string();
        assert!(error.contains("different repository"));
    }

    #[test]
    fn malformed_latest_record_fails_closed() {
        let root = initialized();
        let directory = root.path().join(".crosslink").join(READINESS_DIR);
        fs::create_dir(&directory).unwrap();
        fs::write(directory.join("00000000000000000001-corrupt.json"), "{").unwrap();
        assert!(read_record(&root.path().join(".crosslink")).is_err());
    }

    #[test]
    fn non_ready_record_retains_corrupt_projection_observation() {
        let root = initialized();
        let crosslink = root.path().join(".crosslink");
        fs::write(crosslink.join("issues.db"), b"truncated sqlite").unwrap();
        let record = write_record(
            &crosslink,
            ReadinessDraft {
                daemon_epoch: "epoch",
                daemon_pid: std::process::id(),
                attempt_id: "attempt",
                state: ReadinessState::Starting,
                generation_id: None,
                reason: None,
            },
        )
        .unwrap();
        assert_eq!(record.state, ReadinessState::Starting);
        assert_eq!(record.projection_schema_version, None);
        assert!(record
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("projection schema observation failed")));
        assert_eq!(read_record(&crosslink).unwrap(), Some(record));
        let ready = write_record(
            &crosslink,
            ReadinessDraft {
                daemon_epoch: "epoch",
                daemon_pid: std::process::id(),
                attempt_id: "ready",
                state: ReadinessState::ReadyCurrent,
                generation_id: Some("generation"),
                reason: None,
            },
        );
        assert!(ready.is_err());
    }

    #[test]
    fn waiting_record_points_to_its_own_observation_evidence() {
        let root = initialized();
        let crosslink = root.path().join(".crosslink");
        let identity = DaemonIdentity {
            schema_version: READINESS_SCHEMA_VERSION,
            repository_id: repository_id(&crosslink).unwrap(),
            daemon_epoch: "epoch".to_string(),
            pid: std::process::id(),
            process_start: current_process_start_token().unwrap(),
        };
        write_daemon_identity(&crosslink, &identity).unwrap();
        let integrity = crosslink.join(crate::db::snapshot::SNAPSHOT_DIR);
        fs::create_dir_all(&integrity).unwrap();
        fs::write(
            integrity.join("reconciliation-shadow-zzzz.sqlite"),
            b"old shadow",
        )
        .unwrap();
        fs::write(
            integrity.join("reconciliation-before-zzzz.sqlite"),
            b"old backup",
        )
        .unwrap();
        let record = write_record(
            &crosslink,
            ReadinessDraft {
                daemon_epoch: "epoch",
                daemon_pid: std::process::id(),
                attempt_id: "current-waiting-attempt",
                state: ReadinessState::WaitingForRemote,
                generation_id: None,
                reason: Some("remote unavailable"),
            },
        )
        .unwrap();
        validate_record(&crosslink, &record).unwrap();
        let evidence = PathBuf::from(record.evidence_path.as_ref().unwrap());
        assert!(evidence
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("reconciliation-observation-")));
        let evidence_bytes = fs::read(&evidence).unwrap();
        let evidence_digest = hex::encode(Sha256::digest(&evidence_bytes));
        assert_eq!(
            record.evidence_sha256.as_deref(),
            Some(evidence_digest.as_str())
        );
        let value: serde_json::Value = serde_json::from_slice(&evidence_bytes).unwrap();
        assert_eq!(value["repository_id"], record.repository_id);
        assert_eq!(value["sequence"], record.sequence);
        assert_eq!(value["source_fingerprint"], record.source_fingerprint);
        assert_eq!(value["state"], "waiting_for_remote");
        assert_eq!(value["attempt_id"], "current-waiting-attempt");
        assert_eq!(value["reason"], "remote unavailable");
    }

    #[test]
    fn observation_evidence_tampering_and_schema_drift_fail_closed() {
        let root = initialized();
        let crosslink = root.path().join(".crosslink");
        let identity = DaemonIdentity {
            schema_version: READINESS_SCHEMA_VERSION,
            repository_id: repository_id(&crosslink).unwrap(),
            daemon_epoch: "epoch".to_string(),
            pid: std::process::id(),
            process_start: current_process_start_token().unwrap(),
        };
        write_daemon_identity(&crosslink, &identity).unwrap();
        let mut record = write_record(
            &crosslink,
            ReadinessDraft {
                daemon_epoch: &identity.daemon_epoch,
                daemon_pid: identity.pid,
                attempt_id: "bound-attempt",
                state: ReadinessState::BlockedCorrupt,
                generation_id: None,
                reason: Some("corrupt"),
            },
        )
        .unwrap();
        let evidence = PathBuf::from(record.evidence_path.as_ref().unwrap());
        let original = fs::read(&evidence).unwrap();
        fs::write(&evidence, b"tampered").unwrap();
        assert!(validate_record(&crosslink, &record)
            .unwrap_err()
            .to_string()
            .contains("digest does not match"));

        let mut value: serde_json::Value = serde_json::from_slice(&original).unwrap();
        value["attempt_id"] = serde_json::json!("different-attempt");
        let mismatched = serde_json::to_vec_pretty(&value).unwrap();
        fs::write(&evidence, &mismatched).unwrap();
        record.evidence_sha256 = Some(hex::encode(Sha256::digest(&mismatched)));
        assert!(validate_record(&crosslink, &record)
            .unwrap_err()
            .to_string()
            .contains("does not match its record"));

        value["attempt_id"] = serde_json::json!("bound-attempt");
        value["unexpected"] = serde_json::json!(true);
        let unknown = serde_json::to_vec_pretty(&value).unwrap();
        fs::write(&evidence, &unknown).unwrap();
        record.evidence_sha256 = Some(hex::encode(Sha256::digest(&unknown)));
        assert!(
            format!("{:#}", validate_record(&crosslink, &record).unwrap_err())
                .contains("unknown field")
        );
    }

    #[test]
    fn transition_drains_existing_mutation_and_queues_new_mutations() {
        let root = initialized();
        let crosslink = root.path().join(".crosslink");
        let mutation = acquire_raw_mutation_permit(&crosslink).unwrap();
        let (observed_tx, observed_rx) = mpsc::sync_channel(1);
        let transition_dir = crosslink.clone();
        let transition = thread::spawn(move || {
            acquire_transition_permit_observed(
                &transition_dir,
                None,
                Some(Duration::from_secs(2)),
                || {
                    let _ = observed_tx.try_send(());
                    Ok(())
                },
            )
        });
        observed_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let (mutation_tx, mutation_rx) = mpsc::sync_channel(1);
        let mutation_dir = crosslink.clone();
        let queued = thread::spawn(move || {
            mutation_tx
                .send(acquire_raw_mutation_permit(&mutation_dir))
                .unwrap();
        });
        assert!(mutation_rx
            .recv_timeout(Duration::from_millis(100))
            .is_err());
        drop(mutation);
        let transition = transition.join().unwrap().unwrap();
        assert!(mutation_rx
            .recv_timeout(Duration::from_millis(100))
            .is_err());
        drop(transition);
        let queued_mutation = mutation_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        drop(queued_mutation.unwrap());
        queued.join().unwrap();
        assert!(!crosslink.join(READINESS_DIR).join(TRANSITION_FILE).exists());
    }

    #[test]
    fn malformed_transition_and_mutation_ownership_fail_closed() {
        let root = initialized();
        let crosslink = root.path().join(".crosslink");
        let readiness = crosslink.join(READINESS_DIR);
        let mutations = readiness.join(MUTATION_PERMITS_DIR);
        fs::create_dir_all(&mutations).unwrap();
        fs::write(readiness.join(TRANSITION_FILE), b"{").unwrap();
        let transition_error = acquire_raw_mutation_permit(&crosslink).unwrap_err();
        assert!(transition_error.to_string().contains("malformed"));
        fs::remove_file(readiness.join(TRANSITION_FILE)).unwrap();
        fs::write(mutations.join("corrupt.permit"), b"{").unwrap();
        let mutation_error = acquire_transition_permit_interruptible(
            &crosslink,
            None,
            Some(Duration::from_millis(100)),
        )
        .unwrap_err();
        assert!(mutation_error.to_string().contains("malformed"));
        assert!(mutations.join("corrupt.permit").exists());
        assert!(!readiness.join(TRANSITION_FILE).exists());
    }

    #[test]
    fn dead_transition_owner_and_abandoned_temporary_resume_without_overlap() {
        let root = initialized();
        let crosslink = root.path().join(".crosslink");
        let readiness = crosslink.join(READINESS_DIR);
        fs::create_dir_all(&readiness).unwrap();
        let dead = PermitOwner {
            pid: u32::MAX,
            process_start: "dead-transition".to_string(),
            token: "crashed-owner".to_string(),
        };
        fs::write(
            readiness.join(TRANSITION_FILE),
            serde_json::to_vec(&dead).unwrap(),
        )
        .unwrap();
        fs::write(readiness.join(".owner-crashed.tmp"), b"{").unwrap();
        let permit =
            acquire_transition_permit_interruptible(&crosslink, None, Some(Duration::from_secs(1)))
                .unwrap();
        assert_eq!(
            serde_json::from_slice::<PermitOwner>(
                &fs::read(readiness.join(TRANSITION_FILE)).unwrap()
            )
            .unwrap()
            .pid,
            std::process::id()
        );
        drop(permit);
        assert!(!readiness.join(TRANSITION_FILE).exists());
    }

    #[test]
    fn transition_wait_is_cancelled_and_releases_barrier_during_shutdown() {
        let root = initialized();
        let crosslink = root.path().join(".crosslink");
        let mutation = acquire_raw_mutation_permit(&crosslink).unwrap();
        let shutdown = AtomicBool::new(true);
        let error = acquire_transition_permit_interruptible(
            &crosslink,
            Some(&shutdown),
            Some(Duration::from_secs(30)),
        )
        .unwrap_err();
        assert!(error.to_string().contains("shutdown"));
        assert!(!crosslink.join(READINESS_DIR).join(TRANSITION_FILE).exists());
        drop(mutation);
    }

    #[test]
    fn stale_owner_drop_cannot_remove_replacement_permit() {
        let root = initialized();
        let crosslink = root.path().join(".crosslink");
        let permit = acquire_raw_mutation_permit(&crosslink).unwrap();
        let replacement = PermitOwner::current().unwrap();
        let replacement_bytes = serde_json::to_vec(&replacement).unwrap();
        fs::remove_file(&permit.path).unwrap();
        fs::write(&permit.path, &replacement_bytes).unwrap();
        let path = permit.path.clone();
        drop(permit);
        assert_eq!(fs::read(path).unwrap(), replacement_bytes);
    }

    #[test]
    fn identity_removal_claim_preserves_replacement_epoch() {
        let root = initialized();
        let crosslink = root.path().join(".crosslink");
        let first = DaemonIdentity {
            schema_version: READINESS_SCHEMA_VERSION,
            repository_id: repository_id(&crosslink).unwrap(),
            daemon_epoch: "first".to_string(),
            pid: std::process::id(),
            process_start: current_process_start_token().unwrap(),
        };
        let replacement = DaemonIdentity {
            daemon_epoch: "replacement".to_string(),
            ..first.clone()
        };
        write_daemon_identity(&crosslink, &first).unwrap();
        let replacement_bytes = serde_json::to_vec(&replacement).unwrap();
        remove_daemon_identity_if_with(&crosslink, &first, |path| {
            fs::write(path, &replacement_bytes).context("publishing replacement identity")
        })
        .unwrap();
        assert_eq!(read_daemon_identity(&crosslink).unwrap(), Some(replacement));
    }

    #[test]
    fn ownership_path_is_published_complete_and_claimed_once() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("owner.lock");
        let worker_path = path.clone();
        let (prepared_tx, prepared_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            create_owned_file_with(&worker_path, b"first", |temporary| {
                assert_eq!(fs::read(temporary).unwrap(), b"first");
                prepared_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok(())
            })
        });
        prepared_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(!path.exists());
        create_owned_file(&path, b"second").unwrap();
        release_tx.send(()).unwrap();
        let error = worker.join().unwrap().unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(path).unwrap(), b"second");
    }

    #[test]
    fn repeated_observation_evidence_is_append_only_and_bounded() {
        let root = initialized();
        let crosslink = root.path().join(".crosslink");
        let repository = repository_id(&crosslink).unwrap();
        for sequence in 0..20 {
            write_observation_evidence(
                &crosslink,
                sequence,
                &repository,
                "evidence-test",
                "source-test",
                ReadinessState::BlockedCorrupt,
                Some(&format!("failure {sequence}")),
            )
            .unwrap();
        }
        let integrity = crosslink.join(crate::db::snapshot::SNAPSHOT_DIR);
        let mut observations = fs::read_dir(integrity)
            .unwrap()
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("reconciliation-observation-"))
            })
            .collect::<Vec<_>>();
        observations.sort();
        assert_eq!(observations.len(), 16);
        assert!(observations.iter().all(|path| {
            serde_json::from_slice::<serde_json::Value>(&fs::read(path).unwrap()).is_ok()
        }));
        let newest: serde_json::Value =
            serde_json::from_slice(&fs::read(observations.last().unwrap()).unwrap()).unwrap();
        assert_eq!(newest["reason"], "failure 19");
        assert_eq!(newest["repository_id"], repository);
        assert_eq!(newest["source_fingerprint"], "source-test");
    }

    #[test]
    fn ownership_liveness_checks_are_bounded_by_poll_cadence() {
        let mut sweep = 0;
        let probes = (0..400).filter(|_| liveness_probe_due(&mut sweep)).count();
        assert_eq!(probes, 10);
    }

    #[cfg(windows)]
    #[test]
    fn windows_readiness_record_publication_round_trips() {
        let root = initialized();
        let crosslink = root.path().join(".crosslink");
        let record = write_record(
            &crosslink,
            ReadinessDraft {
                daemon_epoch: "windows-publication",
                daemon_pid: std::process::id(),
                attempt_id: "windows-publication",
                state: ReadinessState::Starting,
                generation_id: None,
                reason: None,
            },
        )
        .unwrap();
        assert_eq!(read_record(&crosslink).unwrap(), Some(record));
    }

    #[cfg(windows)]
    #[test]
    fn windows_atomic_readiness_manifest() {
        ownership_path_is_published_complete_and_claimed_once();
        identity_removal_claim_preserves_replacement_epoch();
        windows_readiness_record_publication_round_trips();
    }
}
