use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use chrono::Utc;
use rusqlite::OptionalExtension;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::checkpoint::{
    CheckpointState, CompactComment, CompactIssue, CompactMilestone, CompactTimeEntry,
};
use crate::compaction;
use crate::events::OrderingKey;
use crate::hub_source::{HubSource, RefHubSource};
use crate::hub_v3::{self, HubMeta, CHECKPOINT_REF, META_REF};
use crate::issue_file::{
    read_all_issue_files, read_all_milestone_files, read_comment_files, read_counters, IssueFile,
};
use crate::reconcile::publication::{
    refresh_generation_ref, CanonicalSemantic, GenerationRefreshOutcome, HistoricalImporter,
    PreparedImport, PublicationOutcome, RepositoryReconciler, SourceEvidence,
};
use crate::reconcile::readiness::{
    self, DaemonIdentity, ReadinessDraft, ReadinessRecord, ReadinessState,
};
use crate::reconcile::SharedStoreFormat;
use crate::sync::SyncManager;

const V2_HUB_BRANCH: &str = "refs/heads/crosslink/hub";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepositoryActivation {
    ReadyCurrent { generation_id: String },
    ReadyMigrated { generation_id: String },
    ReadyAdopted { generation_id: String },
    WaitingForRemote { reason: String },
    BlockedCorrupt { reason: String },
}

pub fn activate_repository(crosslink_dir: &Path) -> Result<RepositoryActivation> {
    let sync = SyncManager::new(crosslink_dir)?;
    match sync.init_cache_for_reconciliation() {
        crate::sync::ReconciliationCacheOutcome::Ready => {}
        crate::sync::ReconciliationCacheOutcome::WaitingForRemote { reason } => {
            return Ok(RepositoryActivation::WaitingForRemote { reason });
        }
        crate::sync::ReconciliationCacheOutcome::BlockedCorrupt { reason } => {
            return Ok(RepositoryActivation::BlockedCorrupt { reason });
        }
    }
    let hub_lock = sync.acquire_lock()?;
    let outcome = reconcile_repository(
        crosslink_dir,
        sync.cache_path(),
        sync.reconciliation_remote(),
        &hub_lock,
    )?;
    Ok(match outcome {
        PublicationOutcome::ReadyCurrent { generation_id } => {
            RepositoryActivation::ReadyCurrent { generation_id }
        }
        PublicationOutcome::Published { generation_id, .. } => {
            RepositoryActivation::ReadyMigrated { generation_id }
        }
        PublicationOutcome::Adopted { generation_id } => {
            RepositoryActivation::ReadyAdopted { generation_id }
        }
        PublicationOutcome::WaitingForRemote { reason } => {
            RepositoryActivation::WaitingForRemote { reason }
        }
        PublicationOutcome::BlockedCorrupt { reason } => {
            RepositoryActivation::BlockedCorrupt { reason }
        }
    })
}

pub(crate) fn write_ready_activation(
    crosslink_dir: &Path,
    identity: &DaemonIdentity,
    attempt_id: &str,
    state: ReadinessState,
    generation_id: &str,
) -> Result<()> {
    readiness::write_record(
        crosslink_dir,
        ReadinessDraft {
            daemon_epoch: &identity.daemon_epoch,
            daemon_pid: identity.pid,
            attempt_id,
            state: ReadinessState::Rebuilding,
            generation_id: Some(generation_id),
            reason: None,
        },
    )?;
    anyhow::ensure!(
        readiness::projection_frontier(crosslink_dir)?.is_some()
            && readiness::projection_is_current(crosslink_dir)?,
        "reconciliation completed without a projection frontier"
    );
    anyhow::ensure!(
        readiness::projection_schema_version(crosslink_dir)? == Some(crate::db::SCHEMA_VERSION),
        "reconciliation completed without a current projection schema"
    );
    let sync = SyncManager::new(crosslink_dir)?;
    anyhow::ensure!(
        crate::reconcile::publication::generation_id_at_ref(
            sync.cache_path(),
            crate::reconcile::publication::GENERATION_REF,
        )?
        .as_deref()
            == Some(generation_id),
        "reconciliation generation identifier does not match the verified descriptor"
    );
    readiness::write_record(
        crosslink_dir,
        ReadinessDraft {
            daemon_epoch: &identity.daemon_epoch,
            daemon_pid: identity.pid,
            attempt_id,
            state,
            generation_id: Some(generation_id),
            reason: None,
        },
    )?;
    Ok(())
}

#[cfg(test)]
pub(crate) fn establish_verified_readiness_for_test(
    crosslink_dir: &Path,
    attempt_id: &str,
) -> Result<ReadinessRecord> {
    let identity = DaemonIdentity {
        schema_version: readiness::READINESS_SCHEMA_VERSION,
        repository_id: readiness::repository_id(crosslink_dir)?,
        daemon_epoch: Uuid::new_v4().to_string(),
        pid: std::process::id(),
        process_start: readiness::current_process_start_token()?,
    };
    readiness::write_daemon_identity(crosslink_dir, &identity)?;
    let transition = readiness::acquire_transition_permit(crosslink_dir)?;
    let activation = activate_repository(crosslink_dir)?;
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
            bail!("test repository is waiting_for_remote: {reason}")
        }
        RepositoryActivation::BlockedCorrupt { reason } => {
            bail!("test repository is blocked_corrupt: {reason}")
        }
    };
    write_ready_activation(crosslink_dir, &identity, attempt_id, state, &generation_id)?;
    drop(transition);
    let record = readiness::read_record(crosslink_dir)?
        .ok_or_else(|| anyhow::anyhow!("test repository readiness record is missing"))?;
    readiness::validate_record(crosslink_dir, &record)?;
    Ok(record)
}

pub(crate) fn refresh_repository_authority(crosslink_dir: &Path) -> Result<ReadinessRecord> {
    readiness::require_mutation_ready(crosslink_dir)?;
    let identity = readiness::read_daemon_identity(crosslink_dir)?
        .ok_or_else(|| anyhow::anyhow!("repository daemon identity is missing"))?;
    let attempt_id = Uuid::new_v4().to_string();
    let transition = readiness::acquire_transition_permit(crosslink_dir)?;
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
        RepositoryActivation::ReadyCurrent { generation_id } => write_ready_activation(
            crosslink_dir,
            &identity,
            &attempt_id,
            ReadinessState::ReadyCurrent,
            &generation_id,
        )?,
        RepositoryActivation::ReadyMigrated { generation_id } => write_ready_activation(
            crosslink_dir,
            &identity,
            &attempt_id,
            ReadinessState::ReadyMigrated,
            &generation_id,
        )?,
        RepositoryActivation::ReadyAdopted { generation_id } => write_ready_activation(
            crosslink_dir,
            &identity,
            &attempt_id,
            ReadinessState::ReadyAdopted,
            &generation_id,
        )?,
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
        }
    }
    drop(transition);
    let record = readiness::read_record(crosslink_dir)?
        .ok_or_else(|| anyhow::anyhow!("repository readiness record is missing after refresh"))?;
    readiness::validate_record(crosslink_dir, &record)?;
    Ok(record)
}

struct MigrationImporter<'a> {
    crosslink_dir: &'a Path,
    cache_dir: &'a Path,
    hub_lock: &'a crate::sync::HubWriteLock,
    agent_id: String,
}

impl MigrationImporter<'_> {
    fn prepare_file_source(
        &self,
        source: &SourceEvidence,
        generation_id: &str,
    ) -> Result<PreparedImport> {
        let evidence = source
            .refs()
            .get(V2_HUB_BRANCH)
            .or_else(|| source.refs().get("refs/heads/crosslink/locks"))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "historical file-backed hub source is missing for {:?}",
                    source.format()
                )
            })?;
        let materialized = tempfile::tempdir().context("creating pinned source workspace")?;
        let (source_tip, mut genesis, mut signers, mut merged) =
            if let Some(remote_oid) = evidence.remote_oid() {
                if remote_oid == evidence.oid() {
                    materialize_commit_tree(self.cache_dir, evidence.oid(), materialized.path())?;
                    (
                        evidence.oid().to_string(),
                        build_genesis_from_files(materialized.path())?,
                        read_allowed_signers(materialized.path())?,
                        false,
                    )
                } else {
                    let (source_tip, genesis, signers) = prepare_diverged_file_source(
                        self.cache_dir,
                        evidence.authority_oid(),
                        evidence.oid(),
                        remote_oid,
                        materialized.path(),
                    )?;
                    (source_tip, genesis, signers, true)
                }
            } else {
                materialize_commit_tree(self.cache_dir, evidence.oid(), materialized.path())?;
                (
                    evidence.oid().to_string(),
                    build_genesis_from_files(materialized.path())?,
                    read_allowed_signers(materialized.path())?,
                    false,
                )
            };
        let current_targets = if matches!(
            source.format().shared_store,
            SharedStoreFormat::Mixed { .. }
        ) && has_complete_v3_source(source)
        {
            Some(direct_v3_targets(source)?)
        } else {
            None
        };
        if let Some(targets) = &current_targets {
            (genesis, signers) = merge_file_source_with_current_v3(
                self.cache_dir,
                targets,
                &genesis,
                signers.as_deref(),
            )?;
            merged = true;
        }
        if let Some(database_evidence) = source.refs().get("local/issues.db") {
            let database_source =
                tempfile::tempdir().context("creating pinned local database workspace")?;
            materialize_commit_tree(
                self.cache_dir,
                database_evidence.oid(),
                database_source.path(),
            )?;
            let database_state = build_genesis_from_database(
                &database_source.path().join("issues.db"),
                &self.agent_id,
            )?;
            let (combined, changed) = merge_local_database_projection(&genesis, &database_state)?;
            genesis = combined;
            merged |= changed;
        }
        write_allowed_signers(materialized.path(), signers.as_deref())?;
        let repeated = if merged {
            serde_json::from_slice::<CheckpointState>(&serde_json::to_vec(&genesis)?)?
        } else {
            build_genesis_from_files(materialized.path())?
        };
        anyhow::ensure!(
            serde_json::to_value(&genesis)? == serde_json::to_value(&repeated)?,
            "independent historical file reads produced different canonical states"
        );
        let targets = seed_v3_targets(
            self.cache_dir,
            materialized.path(),
            &genesis,
            &source_tip,
            generation_id,
            current_targets.as_ref(),
        )?;
        Ok(PreparedImport::new(
            targets,
            canonical_semantic(&genesis, signers)?,
        ))
    }

    fn prepare_local_source(
        &self,
        source: &SourceEvidence,
        generation_id: &str,
    ) -> Result<PreparedImport> {
        let materialized = tempfile::tempdir().context("creating pinned database workspace")?;
        let (source_tip, genesis) = if let Some(evidence) = source.refs().get("local/issues.db") {
            materialize_commit_tree(self.cache_dir, evidence.oid(), materialized.path())?;
            let path = materialized.path().join("issues.db");
            (
                evidence.oid().to_string(),
                build_genesis_from_database(&path, &self.agent_id)?,
            )
        } else {
            let evidence = source
                .refs()
                .get("local/absent")
                .ok_or_else(|| anyhow::anyhow!("absent source evidence is missing"))?;
            (evidence.oid().to_string(), CheckpointState::default())
        };
        let targets = seed_v3_targets(
            self.cache_dir,
            materialized.path(),
            &genesis,
            &source_tip,
            generation_id,
            None,
        )?;
        Ok(PreparedImport::new(
            targets,
            canonical_semantic(&genesis, None)?,
        ))
    }
}

impl HistoricalImporter for MigrationImporter<'_> {
    fn stabilize_source(&self, _repository: &Path) -> Result<()> {
        let source_tip = match git_rev_parse(self.cache_dir, V2_HUB_BRANCH)? {
            Some(tip) => Some(tip),
            None => git_rev_parse(self.cache_dir, "refs/heads/crosslink/locks")?,
        };
        let Some(source_tip) = source_tip else {
            return Ok(());
        };
        if git_rev_parse(self.cache_dir, "HEAD")?.as_deref() != Some(&source_tip) {
            return Ok(());
        }
        let pending = find_pending_offline(self.cache_dir)?;
        let promotable: Vec<&IssueFile> = pending
            .iter()
            .filter(|issue| issue.created_by == self.agent_id)
            .collect();
        if !promotable.is_empty() {
            let names = promotable
                .iter()
                .map(|issue| format!("  {} (\"{}\")", issue.uuid, issue.title))
                .collect::<Vec<_>>()
                .join("\n");
            bail!(
                "refusing to reconcile: {} offline issue(s) created by this agent are pending promotion\n{names}",
                promotable.len()
            );
        }
        if let Some(result) =
            compaction::compact(self.cache_dir, &self.agent_id, true, self.hub_lock)
                .context("pre-reconciliation compaction failed")?
        {
            tracing::info!(
                events = result.events_processed,
                issues = result.issues_materialized,
                locks = result.locks_materialized,
                skew = result.skew_warnings,
                unsigned = result.unsigned_warnings,
                git_skew = result.git_skew_violations,
                "stabilized historical hub source"
            );
        }
        Ok(())
    }

    fn snapshot_source_refs(
        &self,
        repository: &Path,
        source: &SourceEvidence,
    ) -> Result<BTreeMap<String, String>> {
        let mut snapshots = BTreeMap::new();
        if matches!(source.format().shared_store, SharedStoreFormat::Absent) {
            let database = self.crosslink_dir.join("issues.db");
            let (name, oid) = if database.is_file() {
                (
                    "local/issues.db",
                    snapshot_sqlite_file(repository, &database, "issues.db")?,
                )
            } else {
                ("local/absent", snapshot_empty(repository)?)
            };
            snapshots.insert(name.to_string(), oid);
            return Ok(snapshots);
        }
        let database = self.crosslink_dir.join("issues.db");
        if database.is_file() && has_complete_v3_source(source) {
            let targets = direct_v3_targets(source)?;
            let current = reduce_v3_state(self.cache_dir, &targets)?;
            let (_workspace, snapshot) = logical_sqlite_snapshot(&database, "issues.db")?;
            let local = build_genesis_from_database(&snapshot, &self.agent_id)?;
            let requires_import = merge_local_database_projection(&current, &local)
                .map_or(true, |(_, changed)| changed);
            if requires_import {
                snapshots.insert(
                    "local/issues.db".to_string(),
                    snapshot_file(repository, &snapshot, "issues.db")?,
                );
            }
        }
        let Some((name, evidence)) = source
            .refs()
            .get_key_value(V2_HUB_BRANCH)
            .or_else(|| source.refs().get_key_value("refs/heads/crosslink/locks"))
        else {
            return Ok(snapshots);
        };
        let oid = snapshot_worktree(repository, evidence.authority_oid())?;
        snapshots.insert(name.clone(), oid);
        Ok(snapshots)
    }

    fn prepare_file_source(
        &self,
        _repository: &Path,
        source: &SourceEvidence,
        generation_id: &str,
    ) -> Result<PreparedImport> {
        MigrationImporter::prepare_file_source(self, source, generation_id)
    }

    fn prepare_local_source(
        &self,
        _repository: &Path,
        source: &SourceEvidence,
        generation_id: &str,
    ) -> Result<PreparedImport> {
        MigrationImporter::prepare_local_source(self, source, generation_id)
    }

    fn prepare_current_source(
        &self,
        repository: &Path,
        source: &SourceEvidence,
    ) -> Result<PreparedImport> {
        anyhow::ensure!(
            has_complete_v3_source(source),
            "the historical v3 source is incomplete or corrupt"
        );
        let current_targets = direct_v3_targets(source)?;
        let Some(database_evidence) = source.refs().get("local/issues.db") else {
            let semantic = self.read_target_semantic(repository, &current_targets)?;
            return Ok(PreparedImport::new(current_targets, semantic));
        };
        let materialized =
            tempfile::tempdir().context("creating pinned local database merge workspace")?;
        materialize_commit_tree(self.cache_dir, database_evidence.oid(), materialized.path())?;
        let local =
            build_genesis_from_database(&materialized.path().join("issues.db"), &self.agent_id)?;
        let current = reduce_v3_state(self.cache_dir, &current_targets)?;
        let (merged, changed) = merge_local_database_projection(&current, &local)?;
        anyhow::ensure!(changed, "local database contains no unshared state");
        let targets = seed_v3_targets(
            self.cache_dir,
            materialized.path(),
            &merged,
            database_evidence.oid(),
            source.fingerprint(),
            Some(&current_targets),
        )?;
        let signers = read_v3_allowed_signers(self.cache_dir, &current_targets)?;
        Ok(PreparedImport::new(
            targets,
            canonical_semantic(&merged, signers)?,
        ))
    }

    fn file_source_is_newer(&self, repository: &Path, source: &SourceEvidence) -> Result<bool> {
        if !has_complete_v3_source(source) {
            return Ok(true);
        }
        mixed_file_source_advanced(repository, source, &direct_v3_targets(source)?)
    }

    fn read_target_semantic(
        &self,
        repository: &Path,
        targets: &BTreeMap<String, String>,
    ) -> Result<CanonicalSemantic> {
        let checkpoint = targets.get(CHECKPOINT_REF).cloned();
        let meta = targets.get(META_REF).cloned();
        let agents = targets
            .iter()
            .filter_map(|(name, oid)| {
                name.strip_prefix(hub_v3::AGENT_REF_PREFIX)
                    .map(|agent| (agent.to_string(), oid.clone()))
            })
            .collect();
        let source = RefHubSource::at_tips(repository, checkpoint, meta, agents)?;
        let outcome =
            compaction::reduce(&source).context("reducing prepared reconciliation targets")?;
        let allowed_signers = source
            .allowed_signers_file()?
            .map(fs::read)
            .transpose()
            .context("reading allowed_signers from prepared reconciliation target")?;
        canonical_semantic(&outcome.state, allowed_signers)
    }
}

fn read_allowed_signers(source_dir: &Path) -> Result<Option<Vec<u8>>> {
    let path = source_dir.join("trust").join("allowed_signers");
    if path.exists() {
        fs::read(&path)
            .with_context(|| format!("reading pinned trust source {}", path.display()))
            .map(Some)
    } else {
        Ok(None)
    }
}

fn write_allowed_signers(source_dir: &Path, signers: Option<&[u8]>) -> Result<()> {
    let path = source_dir.join("trust").join("allowed_signers");
    if let Some(signers) = signers {
        let parent = path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("allowed_signers path has no parent"))?;
        fs::create_dir_all(parent)
            .with_context(|| format!("creating merged trust directory {}", parent.display()))?;
        fs::write(&path, signers)
            .with_context(|| format!("writing merged trust source {}", path.display()))?;
    }
    Ok(())
}

fn prepare_diverged_file_source(
    repository: &Path,
    authority_oid: &str,
    local_oid: &str,
    remote_oid: &str,
    destination: &Path,
) -> Result<(String, CheckpointState, Option<Vec<u8>>)> {
    anyhow::ensure!(
        git_is_ancestor(repository, authority_oid, local_oid)?,
        "local dirty snapshot {local_oid} is not descended from local authority {authority_oid}"
    );
    if git_is_ancestor(repository, local_oid, remote_oid)? {
        materialize_commit_tree(repository, remote_oid, destination)?;
        return Ok((
            remote_oid.to_string(),
            build_genesis_from_files(destination)?,
            read_allowed_signers(destination)?,
        ));
    }
    if git_is_ancestor(repository, remote_oid, local_oid)? {
        materialize_commit_tree(repository, local_oid, destination)?;
        return Ok((
            local_oid.to_string(),
            build_genesis_from_files(destination)?,
            read_allowed_signers(destination)?,
        ));
    }
    let base_oid = git_merge_base(repository, local_oid, remote_oid)?.ok_or_else(|| {
        anyhow::anyhow!(
            "local snapshot {local_oid} and remote authority {remote_oid} have no common history"
        )
    })?;
    let base = tempfile::tempdir().context("creating base source workspace")?;
    let local = tempfile::tempdir().context("creating local source workspace")?;
    let remote = tempfile::tempdir().context("creating remote source workspace")?;
    materialize_commit_tree(repository, &base_oid, base.path())?;
    materialize_commit_tree(repository, local_oid, local.path())?;
    materialize_commit_tree(repository, remote_oid, remote.path())?;
    let base_state = build_genesis_from_files(base.path())?;
    let local_state = build_genesis_from_files(local.path())?;
    let remote_state = build_genesis_from_files(remote.path())?;
    let state = merge_checkpoint_states(&base_state, &local_state, &remote_state)?;
    let base_signers = read_allowed_signers(base.path())?;
    let local_signers = read_allowed_signers(local.path())?;
    let remote_signers = read_allowed_signers(remote.path())?;
    let signers = merge_optional_bytes(
        base_signers.as_deref(),
        local_signers.as_deref(),
        remote_signers.as_deref(),
    )?;
    Ok((remote_oid.to_string(), state, signers))
}

fn git_is_ancestor(repository: &Path, ancestor: &str, descendant: &str) -> Result<bool> {
    let output = Command::new("git")
        .current_dir(repository)
        .args(["merge-base", "--is-ancestor", ancestor, descendant])
        .output()
        .with_context(|| format!("comparing historical tips {ancestor}..{descendant}"))?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => bail!(
            "git merge-base failed while comparing historical tips: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    }
}

fn git_merge_base(repository: &Path, left: &str, right: &str) -> Result<Option<String>> {
    let output = Command::new("git")
        .current_dir(repository)
        .args(["merge-base", left, right])
        .output()
        .with_context(|| format!("finding common history for {left} and {right}"))?;
    match output.status.code() {
        Some(0) => {
            let oid = String::from_utf8(output.stdout)
                .context("git merge-base output was not UTF-8")?
                .trim()
                .to_string();
            anyhow::ensure!(
                !oid.is_empty(),
                "git merge-base returned an empty object id"
            );
            Ok(Some(oid))
        }
        Some(1) => Ok(None),
        _ => bail!(
            "git merge-base failed while finding common history: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    }
}

fn merge_checkpoint_states(
    base: &CheckpointState,
    local: &CheckpointState,
    remote: &CheckpointState,
) -> Result<CheckpointState> {
    let mut base_value = serde_json::to_value(base)?;
    let mut local_value = serde_json::to_value(local)?;
    let mut remote_value = serde_json::to_value(remote)?;
    for value in [&mut base_value, &mut local_value, &mut remote_value] {
        let object = value
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("checkpoint state is not an object"))?;
        object.remove("watermark");
        object.remove("compaction_lease");
        object.remove("next_display_id");
        object.remove("next_comment_id");
        object.remove("next_milestone_id");
    }
    let mut merged = merge_semantic_value(&base_value, &local_value, &remote_value, "state")?;
    let object = merged
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("merged checkpoint state is not an object"))?;
    for counter in ["next_display_id", "next_comment_id", "next_milestone_id"] {
        let maximum = [base, local, remote]
            .into_iter()
            .filter_map(|state| serde_json::to_value(state).ok())
            .filter_map(|value| value.get(counter).and_then(serde_json::Value::as_i64))
            .max()
            .unwrap_or(1);
        object.insert(counter.to_string(), serde_json::json!(maximum));
    }
    let mut state: CheckpointState = serde_json::from_value(merged)
        .context("deserializing losslessly merged checkpoint state")?;
    state.watermark = [
        base.watermark.clone(),
        local.watermark.clone(),
        remote.watermark.clone(),
    ]
    .into_iter()
    .flatten()
    .max();
    state.compaction_lease = None;
    let mut display_ids = BTreeSet::new();
    for display_id in state.display_id_map.values() {
        anyhow::ensure!(
            display_ids.insert(*display_id),
            "concurrent historical changes assigned duplicate display id {display_id}"
        );
    }
    state.next_display_id = state
        .next_display_id
        .max(display_ids.last().copied().unwrap_or(0) + 1);
    let max_comment = state
        .issues
        .values()
        .flat_map(|issue| issue.comments.values())
        .filter_map(|comment| comment.display_id)
        .max()
        .unwrap_or(0);
    state.next_comment_id = state.next_comment_id.max(max_comment + 1);
    let max_milestone = state
        .milestones
        .values()
        .filter_map(|milestone| milestone.display_id)
        .max()
        .unwrap_or(0);
    state.next_milestone_id = state.next_milestone_id.max(max_milestone + 1);
    Ok(state)
}

fn merge_semantic_value(
    base: &serde_json::Value,
    local: &serde_json::Value,
    remote: &serde_json::Value,
    path: &str,
) -> Result<serde_json::Value> {
    if local == remote {
        return Ok(local.clone());
    }
    if local == base {
        return Ok(remote.clone());
    }
    if remote == base {
        return Ok(local.clone());
    }
    match (base, local, remote) {
        (
            serde_json::Value::Object(base),
            serde_json::Value::Object(local),
            serde_json::Value::Object(remote),
        ) => {
            let keys = base
                .keys()
                .chain(local.keys())
                .chain(remote.keys())
                .cloned()
                .collect::<BTreeSet<_>>();
            let mut merged = serde_json::Map::new();
            for key in keys {
                let missing = serde_json::Value::Null;
                let value = merge_semantic_value(
                    base.get(&key).unwrap_or(&missing),
                    local.get(&key).unwrap_or(&missing),
                    remote.get(&key).unwrap_or(&missing),
                    &format!("{path}.{key}"),
                )?;
                if !value.is_null() {
                    merged.insert(key, value);
                }
            }
            Ok(serde_json::Value::Object(merged))
        }
        (
            serde_json::Value::Array(base),
            serde_json::Value::Array(local),
            serde_json::Value::Array(remote),
        ) => {
            let mut merged = base.clone();
            for value in local.iter().chain(remote) {
                if !merged.contains(value) {
                    merged.push(value.clone());
                }
            }
            Ok(serde_json::Value::Array(merged))
        }
        _ => bail!("concurrent historical changes conflict at {path}"),
    }
}

fn merge_optional_bytes(
    base: Option<&[u8]>,
    local: Option<&[u8]>,
    remote: Option<&[u8]>,
) -> Result<Option<Vec<u8>>> {
    if local == remote {
        Ok(local.map(<[u8]>::to_vec))
    } else if local == base {
        Ok(remote.map(<[u8]>::to_vec))
    } else if remote == base {
        Ok(local.map(<[u8]>::to_vec))
    } else {
        bail!("concurrent historical changes conflict in trust/allowed_signers")
    }
}

fn canonical_semantic(
    state: &CheckpointState,
    allowed_signers: Option<Vec<u8>>,
) -> Result<CanonicalSemantic> {
    CanonicalSemantic::from_value(serde_json::json!({
        "state": state,
        "trust": allowed_signers.map(hex::encode),
    }))
}

fn reduce_v3_state(
    repository: &Path,
    targets: &BTreeMap<String, String>,
) -> Result<CheckpointState> {
    let checkpoint = targets.get(CHECKPOINT_REF).cloned();
    let meta = targets.get(META_REF).cloned();
    let agents = targets
        .iter()
        .filter_map(|(name, oid)| {
            name.strip_prefix(hub_v3::AGENT_REF_PREFIX)
                .map(|agent| (agent.to_string(), oid.clone()))
        })
        .collect();
    let source = RefHubSource::at_tips(repository, checkpoint, meta, agents)?;
    Ok(compaction::reduce(&source)
        .context("reducing current v3 state for local database reconciliation")?
        .state)
}

fn read_v3_allowed_signers(
    repository: &Path,
    targets: &BTreeMap<String, String>,
) -> Result<Option<Vec<u8>>> {
    let checkpoint = targets.get(CHECKPOINT_REF).cloned();
    let meta = targets.get(META_REF).cloned();
    let agents = targets
        .iter()
        .filter_map(|(name, oid)| {
            name.strip_prefix(hub_v3::AGENT_REF_PREFIX)
                .map(|agent| (agent.to_string(), oid.clone()))
        })
        .collect();
    let source = RefHubSource::at_tips(repository, checkpoint, meta, agents)?;
    source
        .allowed_signers_file()?
        .map(fs::read)
        .transpose()
        .context("reading current v3 trust source for local database reconciliation")
}

fn merge_local_database_projection(
    shared: &CheckpointState,
    local: &CheckpointState,
) -> Result<(CheckpointState, bool)> {
    let mut merged = shared.clone();
    let mut changed = false;
    let mut issue_ids = merged
        .display_id_map
        .iter()
        .map(|(uuid, display_id)| (*display_id, *uuid))
        .collect::<BTreeMap<_, _>>();
    let mut next_issue_id = merged
        .next_display_id
        .max(issue_ids.keys().next_back().copied().unwrap_or(0) + 1);
    let mut milestone_ids = merged
        .milestones
        .iter()
        .filter_map(|(uuid, milestone)| milestone.display_id.map(|id| (id, *uuid)))
        .collect::<BTreeMap<_, _>>();
    let mut next_milestone_id = merged
        .next_milestone_id
        .max(milestone_ids.keys().next_back().copied().unwrap_or(0) + 1);
    let mut comment_ids = merged
        .issues
        .values()
        .flat_map(|issue| {
            issue
                .comments
                .iter()
                .filter_map(|(uuid, comment)| comment.display_id.map(|id| (id, *uuid)))
        })
        .collect::<BTreeMap<_, _>>();
    let mut next_comment_id = merged
        .next_comment_id
        .max(comment_ids.keys().next_back().copied().unwrap_or(0) + 1);

    for (uuid, local_milestone) in &local.milestones {
        anyhow::ensure!(
            !merged.deleted_milestones.contains(uuid),
            "local database milestone {uuid} conflicts with a shared deletion"
        );
        if let Some(shared_milestone) = merged.milestones.get(uuid) {
            anyhow::ensure!(
                shared_milestone == local_milestone,
                "local database milestone {uuid} conflicts with shared authority"
            );
            continue;
        }
        let mut milestone = local_milestone.clone();
        if let Some(display_id) = milestone.display_id {
            if milestone_ids.contains_key(&display_id) {
                milestone.display_id = Some(next_milestone_id);
                next_milestone_id += 1;
            }
        }
        if let Some(display_id) = milestone.display_id {
            milestone_ids.insert(display_id, *uuid);
        }
        merged.milestones.insert(*uuid, milestone);
        changed = true;
    }

    for (uuid, local_issue) in &local.issues {
        anyhow::ensure!(
            !merged.deleted_issues.contains(uuid),
            "local database issue {uuid} conflicts with a shared deletion"
        );
        if let Some(shared_issue) = merged.issues.get_mut(uuid) {
            let mut shared_identity = shared_issue.clone();
            let mut local_identity = local_issue.clone();
            for issue in [&mut shared_identity, &mut local_identity] {
                issue.labels.clear();
                issue.blockers.clear();
                issue.related.clear();
                issue.comments.clear();
                issue.time_entries.clear();
            }
            anyhow::ensure!(
                serde_json::to_value(&shared_identity)? == serde_json::to_value(&local_identity)?,
                "local database issue {uuid} conflicts with shared authority"
            );
            let before = (
                shared_issue.labels.len(),
                shared_issue.blockers.len(),
                shared_issue.related.len(),
            );
            shared_issue
                .labels
                .extend(local_issue.labels.iter().cloned());
            shared_issue
                .blockers
                .extend(local_issue.blockers.iter().copied());
            shared_issue
                .related
                .extend(local_issue.related.iter().copied());
            changed |= before
                != (
                    shared_issue.labels.len(),
                    shared_issue.blockers.len(),
                    shared_issue.related.len(),
                );
            for (comment_uuid, local_comment) in &local_issue.comments {
                if let Some(shared_comment) = shared_issue.comments.get(comment_uuid) {
                    let mut normalized = shared_comment.clone();
                    let mut local_normalized = local_comment.clone();
                    normalized.signed_by = None;
                    normalized.signature = None;
                    if normalized.display_id.is_none()
                        && local_normalized.display_id.is_some_and(|id| id < 0)
                    {
                        local_normalized.display_id = None;
                    }
                    anyhow::ensure!(
                        normalized == local_normalized,
                        "local database comment {comment_uuid} conflicts with shared authority"
                    );
                    continue;
                }
                let mut comment = local_comment.clone();
                if let Some(display_id) = comment.display_id {
                    if comment_ids.contains_key(&display_id) {
                        comment.display_id = Some(next_comment_id);
                        next_comment_id += 1;
                    }
                }
                if let Some(display_id) = comment.display_id {
                    comment_ids.insert(display_id, *comment_uuid);
                }
                shared_issue.comments.insert(*comment_uuid, comment);
                changed = true;
            }
            for (entry_uuid, local_entry) in &local_issue.time_entries {
                if let Some(shared_entry) = shared_issue.time_entries.get(entry_uuid) {
                    anyhow::ensure!(
                        shared_entry == local_entry,
                        "local database time entry {entry_uuid} conflicts with shared authority"
                    );
                } else {
                    shared_issue
                        .time_entries
                        .insert(*entry_uuid, local_entry.clone());
                    changed = true;
                }
            }
            continue;
        }
        let mut issue = local_issue.clone();
        if let Some(display_id) = issue.display_id {
            if issue_ids.contains_key(&display_id) {
                issue.display_id = Some(next_issue_id);
                next_issue_id += 1;
            }
        }
        let display_id = issue.display_id.unwrap_or_else(|| {
            let id = next_issue_id;
            next_issue_id += 1;
            id
        });
        issue.display_id = Some(display_id);
        issue_ids.insert(display_id, *uuid);
        merged.display_id_map.insert(*uuid, display_id);
        for (comment_uuid, comment) in &mut issue.comments {
            if let Some(display_id) = comment.display_id {
                if comment_ids.contains_key(&display_id) {
                    comment.display_id = Some(next_comment_id);
                    next_comment_id += 1;
                }
            }
            if let Some(display_id) = comment.display_id {
                comment_ids.insert(display_id, *comment_uuid);
            }
        }
        merged.issues.insert(*uuid, issue);
        changed = true;
    }

    merged.next_display_id = merged.next_display_id.max(next_issue_id);
    merged.next_comment_id = merged.next_comment_id.max(next_comment_id);
    merged.next_milestone_id = merged.next_milestone_id.max(next_milestone_id);
    Ok((merged, changed))
}

fn merge_file_source_with_current_v3(
    repository: &Path,
    targets: &BTreeMap<String, String>,
    file_state: &CheckpointState,
    file_signers: Option<&[u8]>,
) -> Result<(CheckpointState, Option<Vec<u8>>)> {
    let checkpoint = targets.get(CHECKPOINT_REF).cloned();
    let meta_oid = targets
        .get(META_REF)
        .ok_or_else(|| anyhow::anyhow!("mixed v3 source has no meta target"))?;
    let agents = targets
        .iter()
        .filter_map(|(name, oid)| {
            name.strip_prefix(hub_v3::AGENT_REF_PREFIX)
                .map(|agent| (agent.to_string(), oid.clone()))
        })
        .collect();
    let current_source =
        RefHubSource::at_tips(repository, checkpoint, Some(meta_oid.clone()), agents)?;
    let current_state = compaction::reduce(&current_source)
        .context("reducing current v3 state for mixed reconciliation")?
        .state;
    let current_signers = current_source
        .allowed_signers_file()?
        .map(fs::read)
        .transpose()
        .context("reading current v3 trust source for mixed reconciliation")?;
    let bytes = git_cat_file_blob_optional(repository, &format!("{meta_oid}:hub.json"))?
        .ok_or_else(|| anyhow::anyhow!("mixed v3 meta target has no hub.json"))?;
    let meta: HubMeta = serde_json::from_slice(&bytes).context("parsing mixed v3 hub meta")?;
    let base_agents = meta
        .seed_agent_tips
        .unwrap_or_default()
        .into_iter()
        .collect::<Vec<_>>();
    let base_state = if let Some(base_checkpoint) = meta.genesis_checkpoint_commit {
        let base_source = RefHubSource::at_tips(
            repository,
            Some(base_checkpoint),
            Some(meta_oid.clone()),
            base_agents,
        )?;
        compaction::reduce(&base_source)
            .context("reducing the committed v3 migration baseline")?
            .state
    } else {
        anyhow::ensure!(
            base_agents.is_empty(),
            "mixed v3 meta has seed agents but no genesis checkpoint"
        );
        CheckpointState::default()
    };
    let merged = merge_checkpoint_states(&base_state, file_state, &current_state)?;
    anyhow::ensure!(
        file_signers == current_signers.as_deref(),
        "late legacy and live v3 trust sources differ without a provable common trust base"
    );
    let signers = file_signers.map(<[u8]>::to_vec);
    Ok((merged, signers))
}

fn mixed_file_source_advanced(
    repository: &Path,
    source: &SourceEvidence,
    targets: &BTreeMap<String, String>,
) -> Result<bool> {
    let Some(file_tip) = source
        .refs()
        .get(V2_HUB_BRANCH)
        .or_else(|| source.refs().get("refs/heads/crosslink/locks"))
    else {
        return Ok(false);
    };
    let meta_oid = targets
        .get(META_REF)
        .ok_or_else(|| anyhow::anyhow!("mixed v3 source has no meta target"))?;
    let bytes = git_cat_file_blob_optional(repository, &format!("{meta_oid}:hub.json"))?
        .ok_or_else(|| anyhow::anyhow!("mixed v3 meta target has no hub.json"))?;
    let meta: HubMeta = serde_json::from_slice(&bytes).context("parsing mixed v3 hub meta")?;
    Ok(file_tip.authority_oid() != meta.migrated_from_commit
        || file_tip.oid() != file_tip.authority_oid())
}

fn snapshot_worktree(repository: &Path, authority_oid: &str) -> Result<String> {
    if git_rev_parse(repository, "HEAD")?.as_deref() != Some(authority_oid) {
        return Ok(authority_oid.to_string());
    }
    let temporary = tempfile::tempdir().context("creating reconciliation source index")?;
    let index = temporary.path().join("index");
    run_git_with_index(repository, &index, &["read-tree", authority_oid])?;
    for path in [
        "issues",
        "meta",
        "locks",
        "trust",
        "agents",
        "checkpoint",
        "locks.json",
    ] {
        let tracked = Command::new("git")
            .current_dir(repository)
            .args(["ls-tree", "--name-only", authority_oid, "--", path])
            .output()
            .with_context(|| format!("checking pinned historical path {path}"))?;
        if !tracked.status.success() {
            bail!(
                "git ls-tree failed while checking historical path {path}: {}",
                String::from_utf8_lossy(&tracked.stderr).trim()
            );
        }
        if repository.join(path).exists() || !tracked.stdout.is_empty() {
            run_git_with_index(repository, &index, &["add", "-A", "-f", "--", path])?;
        }
    }
    let tree = run_git_with_index(repository, &index, &["write-tree"])?;
    let output = Command::new("git")
        .current_dir(repository)
        .env("GIT_AUTHOR_NAME", "Crosslink Reconciler")
        .env("GIT_AUTHOR_EMAIL", "reconciler@crosslink.local")
        .env("GIT_AUTHOR_DATE", "2000-01-01T00:00:00Z")
        .env("GIT_COMMITTER_NAME", "Crosslink Reconciler")
        .env("GIT_COMMITTER_EMAIL", "reconciler@crosslink.local")
        .env("GIT_COMMITTER_DATE", "2000-01-01T00:00:00Z")
        .args([
            "-c",
            "commit.gpgSign=false",
            "commit-tree",
            tree.trim(),
            "-p",
            authority_oid,
            "-m",
            "crosslink reconciliation source snapshot",
        ])
        .output()
        .context("creating immutable reconciliation source snapshot")?;
    if !output.status.success() {
        bail!(
            "git commit-tree failed while snapshotting the historical source: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    String::from_utf8(output.stdout)
        .context("source snapshot oid was not UTF-8")
        .map(|oid| oid.trim().to_string())
}

fn snapshot_file(repository: &Path, path: &Path, name: &str) -> Result<String> {
    let output = Command::new("git")
        .current_dir(repository)
        .args(["hash-object", "-w", "--"])
        .arg(path)
        .output()
        .with_context(|| format!("snapshotting local source file {}", path.display()))?;
    if !output.status.success() {
        bail!(
            "git hash-object failed for local source file {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let blob_oid = String::from_utf8(output.stdout)
        .context("local source blob oid was not UTF-8")?
        .trim()
        .to_string();
    let tree = git_with_input(
        repository,
        &["mktree"],
        format!("100644 blob {blob_oid}\t{name}\n").as_bytes(),
    )?;
    commit_snapshot_tree(repository, tree.trim(), None)
}

fn snapshot_sqlite_file(repository: &Path, path: &Path, name: &str) -> Result<String> {
    let (_workspace, snapshot) = logical_sqlite_snapshot(path, name)?;
    snapshot_file(repository, &snapshot, name)
}

fn logical_sqlite_snapshot(path: &Path, name: &str) -> Result<(tempfile::TempDir, PathBuf)> {
    let workspace = tempfile::tempdir().context("creating logical database snapshot workspace")?;
    let snapshot = workspace.path().join(name);
    let connection =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("opening local database source {}", path.display()))?;
    let escaped = snapshot.to_string_lossy().replace('\'', "''");
    connection
        .execute(&format!("VACUUM INTO '{escaped}'"), [])
        .with_context(|| format!("snapshotting logical local database {}", path.display()))?;
    drop(connection);
    sync_file(&snapshot).with_context(|| {
        format!(
            "syncing logical local database snapshot {}",
            snapshot.display()
        )
    })?;
    Ok((workspace, snapshot))
}

fn snapshot_empty(repository: &Path) -> Result<String> {
    let tree = git_with_input(repository, &["mktree"], &[])?;
    commit_snapshot_tree(repository, tree.trim(), None)
}

fn commit_snapshot_tree(repository: &Path, tree_oid: &str, parent: Option<&str>) -> Result<String> {
    let mut command = Command::new("git");
    command
        .current_dir(repository)
        .env("GIT_AUTHOR_NAME", "Crosslink Reconciler")
        .env("GIT_AUTHOR_EMAIL", "reconciler@crosslink.local")
        .env("GIT_AUTHOR_DATE", "2000-01-01T00:00:00Z")
        .env("GIT_COMMITTER_NAME", "Crosslink Reconciler")
        .env("GIT_COMMITTER_EMAIL", "reconciler@crosslink.local")
        .env("GIT_COMMITTER_DATE", "2000-01-01T00:00:00Z")
        .args(["-c", "commit.gpgSign=false", "commit-tree", tree_oid]);
    if let Some(parent) = parent {
        command.args(["-p", parent]);
    }
    let output = command
        .args(["-m", "crosslink reconciliation local source snapshot"])
        .output()
        .context("creating immutable local source snapshot")?;
    if !output.status.success() {
        bail!(
            "git commit-tree failed for local source snapshot: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    String::from_utf8(output.stdout)
        .context("local source snapshot oid was not UTF-8")
        .map(|oid| oid.trim().to_string())
}

fn git_with_input(repository: &Path, args: &[&str], input: &[u8]) -> Result<String> {
    let mut child = Command::new("git")
        .current_dir(repository)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawning git {args:?}"))?;
    child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("git {args:?} stdin was unavailable"))?
        .write_all(input)
        .with_context(|| format!("writing input to git {args:?}"))?;
    let output = child
        .wait_with_output()
        .with_context(|| format!("waiting for git {args:?}"))?;
    if !output.status.success() {
        bail!(
            "git {args:?} failed while snapshotting local source: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    String::from_utf8(output.stdout).context("git snapshot output was not UTF-8")
}

fn run_git_with_index(repository: &Path, index: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .current_dir(repository)
        .env("GIT_INDEX_FILE", index)
        .args(args)
        .output()
        .with_context(|| format!("running git {args:?} with an isolated source index"))?;
    if !output.status.success() {
        bail!(
            "git {args:?} failed while snapshotting the historical source: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    String::from_utf8(output.stdout).context("git source snapshot output was not UTF-8")
}

fn materialize_commit_tree(repository: &Path, oid: &str, destination: &Path) -> Result<()> {
    let output = Command::new("git")
        .current_dir(repository)
        .args(["ls-tree", "-r", "-z", "--full-tree", oid])
        .output()
        .context("listing immutable reconciliation source tree")?;
    if !output.status.success() {
        bail!(
            "git ls-tree failed for source snapshot {oid}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    for encoded in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        let tab = encoded
            .iter()
            .position(|byte| *byte == b'\t')
            .ok_or_else(|| anyhow::anyhow!("malformed git tree entry in source snapshot"))?;
        let metadata = std::str::from_utf8(&encoded[..tab])
            .context("source snapshot tree metadata was not UTF-8")?;
        let mut fields = metadata.split_whitespace();
        let mode = fields.next().unwrap_or_default();
        let kind = fields.next().unwrap_or_default();
        let blob_oid = fields.next().unwrap_or_default();
        anyhow::ensure!(
            kind == "blob" && mode != "120000",
            "source snapshot contains unsupported tree entry {metadata}"
        );
        let relative = std::str::from_utf8(&encoded[tab + 1..])
            .context("source snapshot path was not UTF-8")?;
        let relative_path = Path::new(relative);
        anyhow::ensure!(
            !relative_path.is_absolute()
                && relative_path.components().all(|component| matches!(
                    component,
                    Component::Normal(name)
                        if !name.to_string_lossy().eq_ignore_ascii_case(".git")
                )),
            "source snapshot contains unsafe path {relative:?}"
        );
        let blob = Command::new("git")
            .current_dir(repository)
            .args(["cat-file", "blob", blob_oid])
            .output()
            .with_context(|| format!("reading source snapshot blob {blob_oid}"))?;
        if !blob.status.success() {
            bail!(
                "git cat-file failed for source snapshot blob {blob_oid}: {}",
                String::from_utf8_lossy(&blob.stderr).trim()
            );
        }
        let path = destination.join(relative_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating source snapshot path {}", parent.display()))?;
        }
        fs::write(&path, blob.stdout)
            .with_context(|| format!("materializing source snapshot file {}", path.display()))?;
    }
    Ok(())
}

fn has_complete_v3_source(source: &SourceEvidence) -> bool {
    let visible =
        source.refs().contains_key(META_REF) && source.refs().contains_key(CHECKPOINT_REF);
    let hidden = source.refs().contains_key(hub_v3::OLD_META_REF)
        && source.refs().contains_key(hub_v3::OLD_CHECKPOINT_REF);
    visible || hidden
}

fn direct_v3_targets(source: &SourceEvidence) -> Result<BTreeMap<String, String>> {
    let mut targets = BTreeMap::new();
    for (name, evidence) in source.refs() {
        let canonical = if name == hub_v3::OLD_META_REF {
            META_REF.to_string()
        } else if name == hub_v3::OLD_CHECKPOINT_REF {
            CHECKPOINT_REF.to_string()
        } else if let Some(agent) = name.strip_prefix(hub_v3::OLD_AGENT_REF_PREFIX) {
            format!("{}{agent}", hub_v3::AGENT_REF_PREFIX)
        } else if hub_v3::is_v3_hub_ref(name) {
            name.clone()
        } else {
            continue;
        };
        if let Some(existing) = targets.insert(canonical.clone(), evidence.oid().to_string()) {
            anyhow::ensure!(
                existing == evidence.oid(),
                "hidden and visible v3 refs disagree for {canonical}"
            );
        }
    }
    anyhow::ensure!(
        targets.contains_key(META_REF) && targets.contains_key(CHECKPOINT_REF),
        "v3 source is incomplete"
    );
    Ok(targets)
}

pub fn hub_v3(
    crosslink_dir: &Path,
    finalize: bool,
    yes_delete_v2: bool,
    adopt_stale: bool,
    remigrate_from_v2: bool,
) -> Result<()> {
    validate_compatibility_flags(finalize, yes_delete_v2, adopt_stale, remigrate_from_v2)?;
    run_forward_compatibility(crosslink_dir, "migrate hub-v3")?;
    if finalize {
        println!(
            "--finalize compatibility request satisfied: every verified reconciliation cutover archives the historical source and retires its remote refs; there is no separate finalize phase"
        );
    }
    Ok(())
}

pub fn run_forward_compatibility(crosslink_dir: &Path, command: &str) -> Result<()> {
    let _transition = super::readiness::acquire_transition_permit(crosslink_dir)?;
    match activate_repository(crosslink_dir)? {
        RepositoryActivation::ReadyCurrent { .. }
        | RepositoryActivation::ReadyMigrated { .. }
        | RepositoryActivation::ReadyAdopted { .. } => {}
        RepositoryActivation::WaitingForRemote { reason } => {
            bail!("{command} is waiting_for_remote: {reason}");
        }
        RepositoryActivation::BlockedCorrupt { reason } => {
            bail!("{command} is blocked_corrupt: {reason}");
        }
    }
    Ok(())
}

fn validate_compatibility_flags(
    finalize: bool,
    yes_delete_v2: bool,
    adopt_stale: bool,
    remigrate_from_v2: bool,
) -> Result<()> {
    if yes_delete_v2 && !finalize {
        bail!("--yes-delete-v2 requires --finalize");
    }
    if adopt_stale {
        bail!(
            "--adopt-stale is no longer supported because unverifiable authority cannot be adopted; run `crosslink migrate hub-v3` without it"
        );
    }
    if remigrate_from_v2 {
        bail!(
            "--remigrate-from-v2 is no longer required; automatic reconciliation detects and incorporates late legacy history, so run `crosslink migrate hub-v3` without it"
        );
    }
    Ok(())
}

pub fn hub_branches(crosslink_dir: &Path) -> Result<()> {
    run_forward_compatibility(crosslink_dir, "migrate hub-branches")
}

pub fn reconcile_repository(
    crosslink_dir: &Path,
    cache_dir: &Path,
    remote: &str,
    hub_lock: &crate::sync::HubWriteLock,
) -> Result<PublicationOutcome> {
    let agent_id = crate::identity::AgentConfig::load(crosslink_dir)?
        .map_or_else(|| "hub-v3-migrate".to_string(), |a| a.agent_id);
    let format = crate::reconcile::check_repository(crosslink_dir).format;
    if matches!(
        format.local_database,
        crate::reconcile::LocalDatabaseFormat::Missing
    ) && matches!(format.shared_store, SharedStoreFormat::Absent)
    {
        return Ok(PublicationOutcome::BlockedCorrupt {
            reason: "local projection is missing and no shared authority is available for recovery"
                .to_string(),
        });
    }
    let importer = MigrationImporter {
        crosslink_dir,
        cache_dir,
        hub_lock,
        agent_id,
    };
    let journal_path = crosslink_dir.join("reconciliation-journal.json");
    let outcome = RepositoryReconciler::new(cache_dir, journal_path, remote, &importer)
        .reconcile(format)
        .map_err(|error| anyhow::anyhow!("{error:#}"))?;
    let generation_id = match &outcome {
        PublicationOutcome::ReadyCurrent { generation_id }
        | PublicationOutcome::Published { generation_id, .. }
        | PublicationOutcome::Adopted { generation_id } => Some(generation_id.as_str()),
        PublicationOutcome::WaitingForRemote { .. } | PublicationOutcome::BlockedCorrupt { .. } => {
            None
        }
    };
    if let Some(generation_id) = generation_id {
        match refresh_generation_ref(cache_dir, remote, generation_id) {
            GenerationRefreshOutcome::Refreshed => {}
            GenerationRefreshOutcome::WaitingForRemote(reason) => {
                return Ok(PublicationOutcome::WaitingForRemote { reason });
            }
            GenerationRefreshOutcome::BlockedCorrupt(reason) => {
                return Ok(PublicationOutcome::BlockedCorrupt { reason });
            }
        }
    }
    match &outcome {
        PublicationOutcome::ReadyCurrent { generation_id } => {
            println!("hub reconciliation already current at generation {generation_id}.");
        }
        PublicationOutcome::Published {
            generation_id,
            atomic,
        } => {
            println!("hub reconciliation published generation {generation_id} (atomic={atomic}).");
        }
        PublicationOutcome::Adopted { generation_id } => {
            println!("hub reconciliation adopted verified generation {generation_id}.");
        }
        PublicationOutcome::WaitingForRemote { .. } | PublicationOutcome::BlockedCorrupt { .. } => {
        }
    }
    let grants_readiness = matches!(
        &outcome,
        PublicationOutcome::ReadyCurrent { .. }
            | PublicationOutcome::Published { .. }
            | PublicationOutcome::Adopted { .. }
    );
    let current_projection_valid = grants_readiness
        && crosslink_dir.join("issues.db").is_file()
        && verify_projection(crosslink_dir, cache_dir).is_ok();
    if grants_readiness && !current_projection_valid {
        let source = RefHubSource::new(cache_dir)?;
        let state = compaction::reduce(&source)?.state;
        rebuild_projection(crosslink_dir, &state)?;
    }
    if matches!(
        &outcome,
        PublicationOutcome::ReadyCurrent { .. }
            | PublicationOutcome::Published { .. }
            | PublicationOutcome::Adopted { .. }
    ) {
        verify_projection(crosslink_dir, cache_dir)?;
        crate::hydration::record_hydrated_ref_durable(crosslink_dir)
            .context("recording verified projection frontier")?;
    }
    Ok(outcome)
}

fn verify_projection(crosslink_dir: &Path, cache_dir: &Path) -> Result<()> {
    let db = crate::db::Database::open_read_only(&crosslink_dir.join("issues.db"))
        .context("opening reconciled projection read-only for verification")?;
    let source = RefHubSource::new(cache_dir)?;
    let state = compaction::reduce(&source)?.state;
    verify_projection_database(crosslink_dir, &db, &state)
}

fn verify_projection_database(
    crosslink_dir: &Path,
    db: &crate::db::Database,
    state: &CheckpointState,
) -> Result<()> {
    let integrity = db
        .conn
        .query_row("PRAGMA quick_check(1)", [], |row| row.get::<_, String>(0))
        .context("checking reconciled projection integrity")?;
    anyhow::ensure!(
        integrity == "ok",
        "reconciled projection failed integrity check: {integrity}"
    );
    let foreign_key_violation: Option<String> = db
        .conn
        .query_row(
            "SELECT printf('%s:%s', \"table\", rowid) FROM pragma_foreign_key_check LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()
        .context("checking reconciled projection foreign keys")?;
    anyhow::ensure!(
        foreign_key_violation.is_none(),
        "reconciled projection has a foreign key violation: {}",
        foreign_key_violation.unwrap_or_default()
    );
    let agent_id = crate::identity::AgentConfig::load(crosslink_dir)?
        .map_or_else(|| "projection-verifier".to_string(), |agent| agent.agent_id);
    let projected = build_genesis_from_open_database(db, &agent_id, Some(state))?;
    anyhow::ensure!(
        projected.issues.keys().eq(state.issues.keys()),
        "reconciled projection issue set differs from shared authority"
    );
    anyhow::ensure!(
        projected.milestones.keys().eq(state.milestones.keys()),
        "reconciled projection milestone set differs from shared authority"
    );
    anyhow::ensure!(
        projected.display_id_map == state.display_id_map,
        "reconciled projection identifiers differ from shared authority"
    );
    let shared = normalized_projection_state(state.clone())?;
    let mut projected = normalized_projection_state(projected)?;
    for (issue_uuid, issue) in &mut projected.issues {
        if let Some(shared_issue) = shared.issues.get(issue_uuid) {
            issue
                .time_entries
                .retain(|entry_uuid, _| shared_issue.time_entries.contains_key(entry_uuid));
            for (comment_uuid, comment) in &mut issue.comments {
                if shared_issue
                    .comments
                    .get(comment_uuid)
                    .is_some_and(|shared_comment| shared_comment.display_id.is_none())
                {
                    comment.display_id = None;
                }
            }
        }
    }
    anyhow::ensure!(
        serde_json::to_value(&shared)? == serde_json::to_value(&projected)?,
        "reconciled projection differs from shared authority"
    );
    Ok(())
}

fn rebuild_projection(crosslink_dir: &Path, state: &CheckpointState) -> Result<()> {
    rebuild_projection_with_checks(crosslink_dir, state, || Ok(()), |_| Ok(()))
}

fn rebuild_projection_with_checks<B, D>(
    crosslink_dir: &Path,
    state: &CheckpointState,
    before_install: B,
    during_install: D,
) -> Result<()>
where
    B: FnOnce() -> Result<()>,
    D: FnOnce(&crate::db::Database) -> Result<()>,
{
    let integrity_dir = crosslink_dir.join(crate::db::snapshot::SNAPSHOT_DIR);
    fs::create_dir_all(&integrity_dir)
        .with_context(|| format!("creating projection evidence {}", integrity_dir.display()))?;
    prune_projection_evidence_to(&integrity_dir, 15)?;
    let evidence_id = Uuid::new_v4();
    let backup_path = integrity_dir.join(format!("reconciliation-before-{evidence_id}.sqlite"));
    let shadow_path = integrity_dir.join(format!("reconciliation-shadow-{evidence_id}.sqlite"));
    let live_path = crosslink_dir.join("issues.db");
    let live_exists = live_path.is_file();
    if live_exists {
        fs::copy(&live_path, &backup_path)
            .with_context(|| format!("capturing projection backup {}", backup_path.display()))?;
        sync_file(&backup_path)?;
        for suffix in ["-wal", "-shm"] {
            let source = PathBuf::from(format!("{}{suffix}", live_path.display()));
            if source.is_file() {
                let destination = PathBuf::from(format!("{}{suffix}", backup_path.display()));
                fs::copy(&source, &destination).with_context(|| {
                    format!("capturing projection evidence {}", destination.display())
                })?;
                sync_file(&destination)?;
            }
        }
        let source = rusqlite::Connection::open_with_flags(
            &live_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .context("opening live projection read-only for shadow backup")?;
        let escaped_shadow = shadow_path.to_string_lossy().replace('\'', "''");
        source
            .execute(&format!("VACUUM INTO '{escaped_shadow}'"), [])
            .with_context(|| format!("creating projection shadow {}", shadow_path.display()))?;
        drop(source);
        sync_file(&shadow_path)?;
    }
    sync_projection_directory(&integrity_dir)?;

    let shadow = crate::db::Database::open(&shadow_path).context("opening projection shadow")?;
    crate::hydration::hydrate_from_state_verified(state, &shadow, |database| {
        verify_projection_database(crosslink_dir, database, state)
    })
    .context("building verified projection shadow")?;
    anyhow::ensure!(
        shadow.get_schema_version()? == crate::db::SCHEMA_VERSION,
        "projection shadow did not reach the current database schema version"
    );
    shadow
        .conn
        .execute_batch("PRAGMA optimize")
        .context("optimizing verified projection shadow")?;
    if !live_exists {
        before_install()?;
        during_install(&shadow)?;
        drop(shadow);
        sync_file(&shadow_path)?;
        let install_path =
            integrity_dir.join(format!("reconciliation-install-{evidence_id}.sqlite"));
        fs::copy(&shadow_path, &install_path)
            .with_context(|| format!("preparing projection install {}", install_path.display()))?;
        sync_file(&install_path)?;
        crate::utils::durable_rename(&install_path, &live_path, false)
            .with_context(|| format!("publishing projection {}", live_path.display()))?;
        sync_projection_directory(crosslink_dir)?;
        let installed = crate::db::Database::open_read_only(&live_path)
            .context("opening installed projection read-only")?;
        verify_projection_database(crosslink_dir, &installed, state)?;
        return Ok(());
    }
    drop(shadow);
    sync_file(&shadow_path)?;

    before_install()?;
    let live = crate::db::Database::open_without_migrations(&live_path)
        .context("opening live projection for transactional rebuild")?;
    live.conn
        .busy_timeout(std::time::Duration::from_secs(5))
        .context("configuring projection install deadline")?;
    live.transaction_with_schema_upgrade(|| {
        crate::hydration::hydrate_from_state_verified_in_transaction(state, &live, |database| {
            verify_projection_database(crosslink_dir, database, state)?;
            during_install(database)
        })
    })
    .context("installing verified projection transaction")?;
    anyhow::ensure!(
        live.get_schema_version()? == crate::db::SCHEMA_VERSION,
        "projection install did not reach the current database schema version"
    );
    verify_projection_database(crosslink_dir, &live, state)?;
    drop(live);
    sync_file(&live_path)?;
    sync_projection_directory(crosslink_dir)?;
    Ok(())
}

fn prune_projection_evidence_to(integrity_dir: &Path, retain: usize) -> Result<()> {
    let mut groups: BTreeMap<String, (std::time::SystemTime, Vec<PathBuf>)> = BTreeMap::new();
    for entry in fs::read_dir(integrity_dir)? {
        let entry = entry?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        let id = [
            "reconciliation-before-",
            "reconciliation-shadow-",
            "reconciliation-install-",
        ]
        .into_iter()
        .find_map(|prefix| name.strip_prefix(prefix))
        .and_then(|value| value.split_once(".sqlite").map(|(id, _)| id.to_string()));
        let Some(id) = id else {
            continue;
        };
        let modified = entry
            .metadata()?
            .modified()
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        let group = groups.entry(id).or_insert_with(|| (modified, Vec::new()));
        group.0 = group.0.max(modified);
        group.1.push(path);
    }
    let mut ordered = groups.into_iter().collect::<Vec<_>>();
    ordered.sort_by(|left, right| left.1 .0.cmp(&right.1 .0).then(left.0.cmp(&right.0)));
    let remove_count = ordered.len().saturating_sub(retain);
    for (_, (_, paths)) in ordered.into_iter().take(remove_count) {
        for path in paths {
            fs::remove_file(&path)
                .with_context(|| format!("pruning projection evidence {}", path.display()))?;
        }
    }
    if remove_count > 0 {
        sync_projection_directory(integrity_dir)?;
    }
    Ok(())
}

fn sync_file(path: &Path) -> Result<()> {
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("opening {} for sync", path.display()))?
        .sync_all()
        .with_context(|| format!("syncing {}", path.display()))
}

#[cfg(unix)]
fn sync_projection_directory(path: &Path) -> Result<()> {
    fs::File::open(path)
        .with_context(|| format!("opening projection directory {}", path.display()))?
        .sync_all()
        .with_context(|| format!("syncing projection directory {}", path.display()))
}

#[cfg(windows)]
fn sync_projection_directory(path: &Path) -> Result<()> {
    let _ = path;
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn sync_projection_directory(path: &Path) -> Result<()> {
    fs::File::open(path)
        .with_context(|| format!("opening projection directory {}", path.display()))?
        .sync_all()
        .with_context(|| format!("syncing projection directory {}", path.display()))
}

fn normalized_projection_state(mut state: CheckpointState) -> Result<CheckpointState> {
    state.next_display_id = 0;
    state.next_comment_id = 0;
    state.next_milestone_id = 0;
    state.locks.clear();
    state.deleted_issues.clear();
    state.deleted_milestones.clear();
    state.skew_warnings.clear();
    state.compaction_lease = None;
    state.unsigned_event_warnings.clear();
    state.watermark = None;
    for issue in state.issues.values_mut() {
        for comment in issue.comments.values_mut() {
            comment.signed_by = None;
            comment.signature = None;
        }
        let mut normalized_time = BTreeMap::new();
        for entry in issue.time_entries.values() {
            let display_id = entry
                .display_id
                .ok_or_else(|| anyhow::anyhow!("time entry has no display identifier"))?;
            normalized_time.insert(
                derive_uuid("projection-time-entry", issue.uuid, display_id),
                entry.clone(),
            );
        }
        issue.time_entries = normalized_time;
    }
    Ok(state)
}

struct IssueLayout {
    inline_comments: Vec<crate::issue_file::CommentEntry>,

    comments_dir: Option<PathBuf>,
}

fn build_genesis_from_files(cache_dir: &Path) -> Result<CheckpointState> {
    let issues_dir = cache_dir.join("issues");
    let issue_files = read_all_issue_files(&issues_dir)?;

    let mut issues: BTreeMap<Uuid, CompactIssue> = BTreeMap::new();
    let mut display_id_map: BTreeMap<Uuid, i64> = BTreeMap::new();
    let mut max_display_id: i64 = 0;
    let mut max_comment_id: i64 = 0;

    let mut display_id_owner: BTreeMap<i64, Uuid> = BTreeMap::new();

    for issue in &issue_files {
        if let Some(did) = issue.display_id {
            if let Some(prev) = display_id_owner.insert(did, issue.uuid) {
                bail!(
                    "duplicate display_id #{did} claimed by two issues ({prev} and {}); \
                     refusing to migrate — repair the v2 hub first \
                     (`crosslink integrity` / `crosslink compact`)",
                    issue.uuid
                );
            }
            display_id_map.insert(issue.uuid, did);
            max_display_id = max_display_id.max(did);
        }

        let layout = issue_layout(&issues_dir, issue);
        let (comments, comment_max) = build_comments(issue.uuid, issue, &layout)?;
        max_comment_id = max_comment_id.max(comment_max);
        let time_entries = build_time_entries(issue.uuid, issue);

        let compact = CompactIssue {
            uuid: issue.uuid,
            display_id: issue.display_id,
            title: issue.title.clone(),
            description: issue.description.clone(),
            status: issue.status,
            priority: issue.priority,
            parent_uuid: issue.parent_uuid,
            created_by: issue.created_by.clone(),
            created_at: issue.created_at,
            updated_at: issue.updated_at,
            closed_at: issue.closed_at,
            scheduled_at: issue.scheduled_at,
            due_at: issue.due_at,
            labels: issue.labels.iter().cloned().collect(),
            blockers: issue.blockers.iter().copied().collect(),
            related: issue.related.iter().copied().collect(),
            milestone_uuid: issue.milestone_uuid,
            comments,
            time_entries,
        };
        issues.insert(issue.uuid, compact);
    }

    let milestones_dir = cache_dir.join("meta").join("milestones");
    let milestone_files = read_all_milestone_files(&milestones_dir)?;
    let mut milestones: BTreeMap<Uuid, CompactMilestone> = BTreeMap::new();
    let mut max_milestone_id: i64 = 0;
    for ms in &milestone_files {
        max_milestone_id = max_milestone_id.max(ms.display_id);
        milestones.insert(
            ms.uuid,
            CompactMilestone {
                uuid: ms.uuid,
                display_id: Some(ms.display_id),
                name: ms.name.clone(),
                description: ms.description.clone(),
                status: ms.status,
                created_at: ms.created_at,
                closed_at: ms.closed_at,
            },
        );
    }

    let locks = crate::checkpoint::read_checkpoint(cache_dir)?.locks;

    let counters = read_counters(&cache_dir.join("meta").join("counters.json"))?;
    let mut next_display_id = counters.next_display_id.max(max_display_id + 1);
    let next_comment_id = counters.next_comment_id.max(max_comment_id + 1);
    let next_milestone_id = counters.next_milestone_id.max(max_milestone_id + 1);

    let mut orphan_keys: Vec<(chrono::DateTime<chrono::Utc>, Uuid)> = issues
        .values()
        .filter(|i| i.display_id.is_none())
        .map(|i| (i.created_at, i.uuid))
        .collect();
    orphan_keys.sort_unstable();
    for (_, uuid) in orphan_keys {
        let id = next_display_id;
        next_display_id += 1;
        display_id_map.insert(uuid, id);
        if let Some(ci) = issues.get_mut(&uuid) {
            ci.display_id = Some(id);
        }
    }

    let watermark =
        max_event_ordering_key(cache_dir)?.unwrap_or_else(hub_v3::genesis_sentinel_watermark);

    Ok(CheckpointState {
        next_display_id,
        next_comment_id,
        display_id_map,
        locks,
        issues,
        milestones,
        deleted_issues: BTreeSet::new(),
        deleted_milestones: BTreeSet::new(),
        next_milestone_id,
        skew_warnings: Vec::new(),
        compaction_lease: None,
        unsigned_event_warnings: Vec::new(),
        watermark: Some(watermark),
    })
}

fn build_genesis_from_database(path: &Path, agent_id: &str) -> Result<CheckpointState> {
    let database = crate::db::Database::open(path)
        .with_context(|| format!("opening pinned local database {}", path.display()))?;
    build_genesis_from_open_database(&database, agent_id, None)
}

fn build_genesis_from_open_database(
    database: &crate::db::Database,
    agent_id: &str,
    authority: Option<&CheckpointState>,
) -> Result<CheckpointState> {
    let mut source_issues = database.list_issues(Some("all"), None, None)?;
    if let Some(authority) = authority {
        let mut selected = Vec::new();
        for issue in source_issues {
            let (uuid, _) = database.get_issue_export_metadata(issue.id)?;
            let uuid = uuid
                .ok_or_else(|| anyhow::anyhow!("authority issue {} has no UUID", issue.id))?
                .parse::<Uuid>()
                .with_context(|| format!("authority issue {} has an invalid UUID", issue.id))?;
            if authority.issues.contains_key(&uuid) {
                selected.push(issue);
            }
        }
        source_issues = selected;
    }
    let mut source_milestones = database.list_milestones(Some("all"))?;
    if let Some(authority) = authority {
        let mut selected = Vec::new();
        for milestone in source_milestones {
            let uuid: Option<String> = database.conn.query_row(
                "SELECT uuid FROM milestones WHERE id = ?1",
                [milestone.id],
                |row| row.get(0),
            )?;
            let uuid = uuid
                .ok_or_else(|| anyhow::anyhow!("authority milestone {} has no UUID", milestone.id))?
                .parse::<Uuid>()
                .with_context(|| {
                    format!("authority milestone {} has an invalid UUID", milestone.id)
                })?;
            if authority.milestones.contains_key(&uuid) {
                selected.push(milestone);
            }
        }
        source_milestones = selected;
    }
    let mut issue_ids = BTreeMap::new();
    for issue in &source_issues {
        let (stored, _) = database.get_issue_export_metadata(issue.id)?;
        let uuid = stored
            .as_deref()
            .and_then(|value| value.parse().ok())
            .unwrap_or_else(|| derive_uuid("sqlite-issue", Uuid::nil(), issue.id));
        issue_ids.insert(issue.id, uuid);
    }
    let mut milestone_ids = BTreeMap::new();
    for milestone in &source_milestones {
        let stored: Option<String> = database.conn.query_row(
            "SELECT uuid FROM milestones WHERE id = ?1",
            [milestone.id],
            |row| row.get(0),
        )?;
        let uuid = stored
            .as_deref()
            .and_then(|value| value.parse().ok())
            .unwrap_or_else(|| derive_uuid("sqlite-milestone", Uuid::nil(), milestone.id));
        milestone_ids.insert(milestone.id, uuid);
    }

    let mut max_comment_id = 0;
    let mut issues = BTreeMap::new();
    let mut display_id_map = BTreeMap::new();
    for issue in source_issues {
        let uuid = issue_ids[&issue.id];
        display_id_map.insert(uuid, issue.id);
        let (_, created_by) = database.get_issue_export_metadata(issue.id)?;
        let mut comments = BTreeMap::new();
        for (
            id,
            author,
            content,
            created_at,
            kind,
            trigger_type,
            intervention_context,
            driver_key_fingerprint,
        ) in database.get_comments_with_author(issue.id)?
        {
            max_comment_id = max_comment_id.max(id);
            let stored: Option<String> = database.conn.query_row(
                "SELECT uuid FROM comments WHERE id = ?1",
                [id],
                |row| row.get(0),
            )?;
            let comment_uuid = stored
                .as_deref()
                .and_then(|value| value.parse().ok())
                .unwrap_or_else(|| derive_uuid("sqlite-comment", uuid, id));
            comments.insert(
                comment_uuid,
                CompactComment {
                    display_id: Some(id),
                    author: author.unwrap_or_else(|| agent_id.to_string()),
                    content,
                    created_at,
                    kind,
                    trigger_type,
                    intervention_context,
                    driver_key_fingerprint,
                    signed_by: None,
                    signature: None,
                },
            );
        }
        let blockers = database
            .get_blockers(issue.id)?
            .into_iter()
            .filter_map(|id| issue_ids.get(&id).copied())
            .collect();
        let related = database
            .get_related_issue_ids(issue.id)?
            .into_iter()
            .filter_map(|id| issue_ids.get(&id).copied())
            .collect();
        let milestone_uuid = database
            .get_issue_milestone(issue.id)?
            .and_then(|milestone| milestone_ids.get(&milestone.id).copied());
        let time_entries = database
            .get_time_entries_for_issue(issue.id)?
            .into_iter()
            .map(|(id, started_at, ended_at, duration_seconds)| {
                (
                    derive_uuid("sqlite-time-entry", uuid, id),
                    CompactTimeEntry {
                        display_id: Some(id),
                        started_at,
                        ended_at,
                        duration_seconds,
                    },
                )
            })
            .collect();
        issues.insert(
            uuid,
            CompactIssue {
                uuid,
                display_id: Some(issue.id),
                title: issue.title,
                description: issue.description,
                status: issue.status,
                priority: issue.priority,
                parent_uuid: issue.parent_id.and_then(|id| issue_ids.get(&id).copied()),
                created_by: created_by.unwrap_or_else(|| agent_id.to_string()),
                created_at: issue.created_at,
                updated_at: issue.updated_at,
                closed_at: issue.closed_at,
                scheduled_at: issue.scheduled_at,
                due_at: issue.due_at,
                labels: database.get_labels(issue.id)?.into_iter().collect(),
                blockers,
                related,
                milestone_uuid,
                comments,
                time_entries,
            },
        );
    }
    let milestones = source_milestones
        .into_iter()
        .map(|milestone| {
            let uuid = milestone_ids[&milestone.id];
            (
                uuid,
                CompactMilestone {
                    uuid,
                    display_id: Some(milestone.id),
                    name: milestone.name,
                    description: milestone.description,
                    status: milestone.status,
                    created_at: milestone.created_at,
                    closed_at: milestone.closed_at,
                },
            )
        })
        .collect();
    Ok(CheckpointState {
        next_display_id: issue_ids.keys().next_back().copied().unwrap_or(0) + 1,
        next_comment_id: max_comment_id + 1,
        display_id_map,
        locks: BTreeMap::new(),
        issues,
        milestones,
        deleted_issues: BTreeSet::new(),
        deleted_milestones: BTreeSet::new(),
        next_milestone_id: milestone_ids.keys().next_back().copied().unwrap_or(0) + 1,
        skew_warnings: Vec::new(),
        compaction_lease: None,
        unsigned_event_warnings: Vec::new(),
        watermark: Some(hub_v3::genesis_sentinel_watermark()),
    })
}

fn issue_layout(issues_dir: &Path, issue: &IssueFile) -> IssueLayout {
    let v2_comments = issues_dir.join(issue.uuid.to_string()).join("comments");
    let comments_dir = if v2_comments.is_dir() {
        Some(v2_comments)
    } else {
        None
    };
    IssueLayout {
        inline_comments: issue.comments.clone(),
        comments_dir,
    }
}

fn build_comments(
    issue_uuid: Uuid,
    issue: &IssueFile,
    layout: &IssueLayout,
) -> Result<(BTreeMap<Uuid, CompactComment>, i64)> {
    let mut map: BTreeMap<Uuid, CompactComment> = BTreeMap::new();
    let mut max_id: i64 = 0;

    if let Some(dir) = &layout.comments_dir {
        for cf in read_comment_files(dir)? {
            map.insert(
                cf.uuid,
                CompactComment {
                    display_id: None,
                    author: cf.author,
                    content: cf.content,
                    created_at: cf.created_at,
                    kind: cf.kind,
                    trigger_type: cf.trigger_type,
                    intervention_context: cf.intervention_context,
                    driver_key_fingerprint: cf.driver_key_fingerprint,
                    signed_by: cf.signed_by,
                    signature: cf.signature,
                },
            );
        }
    }

    let _ = issue;
    for ce in &layout.inline_comments {
        max_id = max_id.max(ce.id);
        let cuuid = derive_uuid("comment", issue_uuid, ce.id);
        map.entry(cuuid).or_insert_with(|| CompactComment {
            display_id: Some(ce.id),
            author: ce.author.clone(),
            content: ce.content.clone(),
            created_at: ce.created_at,
            kind: ce.kind.clone(),
            trigger_type: ce.trigger_type.clone(),
            intervention_context: ce.intervention_context.clone(),
            driver_key_fingerprint: ce.driver_key_fingerprint.clone(),
            signed_by: ce.signed_by.clone(),
            signature: ce.signature.clone(),
        });
    }

    Ok((map, max_id))
}

fn build_time_entries(issue_uuid: Uuid, issue: &IssueFile) -> BTreeMap<Uuid, CompactTimeEntry> {
    let mut map: BTreeMap<Uuid, CompactTimeEntry> = BTreeMap::new();
    for te in &issue.time_entries {
        let tuuid = derive_uuid("time-entry", issue_uuid, te.id);
        map.entry(tuuid).or_insert_with(|| CompactTimeEntry {
            display_id: Some(te.id),
            started_at: te.started_at,
            ended_at: te.ended_at,
            duration_seconds: te.duration_seconds,
        });
    }
    map
}

fn derive_uuid(kind: &str, issue_uuid: Uuid, id: i64) -> Uuid {
    let canonical = format!("crosslink-hub-v3:{kind}:{issue_uuid}:{id}");
    let digest = Sha256::digest(canonical.as_bytes());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[0..16]);
    Uuid::from_bytes(bytes)
}

fn max_event_ordering_key(cache_dir: &Path) -> Result<Option<OrderingKey>> {
    let agents_dir = cache_dir.join("agents");
    let mut max_key: Option<OrderingKey> = None;
    if !agents_dir.exists() {
        return Ok(None);
    }
    for entry in std::fs::read_dir(&agents_dir)
        .with_context(|| format!("failed to read agents dir {}", agents_dir.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let log_path = entry.path().join("events.log");
        if !log_path.exists() {
            continue;
        }
        let events = crate::events::read_events(&log_path)?;
        for ev in &events {
            let key = OrderingKey::from_envelope(ev);
            match &max_key {
                Some(m) if *m >= key => {}
                _ => max_key = Some(key),
            }
        }
    }
    Ok(max_key)
}

fn merge_agent_event_logs(agent_id: &str, left: &[u8], right: &[u8]) -> Result<Vec<u8>> {
    let mut events = BTreeMap::new();
    for bytes in [left, right] {
        for event in crate::events::read_events_from_bytes(bytes)
            .with_context(|| format!("agent {agent_id} has an invalid events.log"))?
        {
            anyhow::ensure!(
                event.agent_id == agent_id,
                "agent ref {agent_id} contains an event for {}",
                event.agent_id
            );
            let value = serde_json::to_value(&event)?;
            if let Some(existing) = events.insert(event.agent_seq, value.clone()) {
                anyhow::ensure!(
                    existing == value,
                    "agent {agent_id} has conflicting events at sequence {}",
                    event.agent_seq
                );
            }
        }
    }
    let mut merged = Vec::new();
    for event in events.into_values() {
        serde_json::to_writer(&mut merged, &event)?;
        merged.push(b'\n');
    }
    Ok(merged)
}

fn seed_v3_targets(
    repository: &Path,
    source_dir: &Path,
    genesis: &CheckpointState,
    source_tip: &str,
    generation_id: &str,
    current_targets: Option<&BTreeMap<String, String>>,
) -> Result<BTreeMap<String, String>> {
    let build_root = format!("refs/crosslink/reconciliation/build/{generation_id}");
    let mut targets = BTreeMap::new();
    let mut seed_agent_tips = BTreeMap::new();
    let mut source_agent_logs = BTreeMap::new();
    let agents_dir = source_dir.join("agents");
    if agents_dir.exists() {
        for entry in std::fs::read_dir(&agents_dir)
            .with_context(|| format!("failed to read agents dir {}", agents_dir.display()))?
        {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let Some(agent_id) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let log_path = entry.path().join("events.log");
            if !log_path.exists() {
                continue;
            }
            let bytes = std::fs::read(&log_path)
                .with_context(|| format!("failed to read {}", log_path.display()))?;
            if bytes.is_empty() {
                continue;
            }
            crate::events::read_events_from_bytes(&bytes)
                .with_context(|| format!("agent {agent_id} has an invalid events.log"))?;
            source_agent_logs.insert(agent_id, bytes);
        }
    }
    let current_agents = current_targets
        .into_iter()
        .flat_map(|targets| targets.iter())
        .filter_map(|(reference, oid)| {
            reference
                .strip_prefix(hub_v3::AGENT_REF_PREFIX)
                .map(|agent| (agent.to_string(), oid.clone()))
        })
        .collect::<BTreeMap<_, _>>();
    let agents = source_agent_logs
        .keys()
        .chain(current_agents.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    for agent_id in agents {
        let current = current_agents.get(&agent_id);
        let current_bytes = current
            .map(|oid| git_cat_file_blob_optional(repository, &format!("{oid}:events.log")))
            .transpose()?
            .flatten();
        let merged = match (source_agent_logs.get(&agent_id), current_bytes.as_deref()) {
            (Some(source), Some(current)) => merge_agent_event_logs(&agent_id, source, current)?,
            (Some(source), None) => source.clone(),
            (None, Some(current)) => current.to_vec(),
            (None, None) => continue,
        };
        let oid = if let Some(current) =
            current.filter(|_| current_bytes.as_deref() == Some(merged.as_slice()))
        {
            current.clone()
        } else {
            let staging_ref = format!("{build_root}/agents/{agent_id}");
            if let Some(current) = current {
                git_update_ref(repository, &staging_ref, current)?;
            }
            hub_v3::commit_blob_to_ref(
                repository,
                &staging_ref,
                "events.log",
                &merged,
                "crosslink reconciliation agent seed",
            )?
        };
        seed_agent_tips.insert(agent_id.clone(), oid.clone());
        targets.insert(format!("{}{agent_id}", hub_v3::AGENT_REF_PREFIX), oid);
    }

    let state_bytes = serde_json::to_vec_pretty(genesis)
        .context("serializing reconciliation checkpoint state")?;
    let checkpoint_ref = format!("{build_root}/checkpoint");
    let checkpoint_oid = hub_v3::commit_blob_to_ref(
        repository,
        &checkpoint_ref,
        "state.json",
        &state_bytes,
        "crosslink reconciliation checkpoint",
    )?;
    targets.insert(CHECKPOINT_REF.to_string(), checkpoint_oid.clone());

    let meta = HubMeta {
        hub_version: 3,
        migrated_from_commit: source_tip.to_string(),
        migrated_at: Utc::now(),
        finalized_at: None,
        genesis_checkpoint_commit: Some(checkpoint_oid),
        seed_agent_tips: Some(seed_agent_tips),
    };
    let hub_json =
        serde_json::to_vec_pretty(&meta).context("serializing reconciliation hub meta")?;
    let signers_path = source_dir.join("trust").join("allowed_signers");
    let signers = if signers_path.exists() {
        Some(
            std::fs::read(&signers_path)
                .with_context(|| format!("failed to read {}", signers_path.display()))?,
        )
    } else {
        None
    };
    let mut files = vec![("hub.json", hub_json.as_slice())];
    if let Some(bytes) = &signers {
        files.push(("allowed_signers", bytes.as_slice()));
    }
    let meta_ref = format!("{build_root}/meta");
    let meta_oid = hub_v3::commit_files_to_ref(
        repository,
        &meta_ref,
        &files,
        "crosslink reconciliation meta",
    )?;
    targets.insert(META_REF.to_string(), meta_oid);
    Ok(targets)
}

fn find_pending_offline(cache_dir: &Path) -> Result<Vec<IssueFile>> {
    let issues_dir = cache_dir.join("issues");
    let all = read_all_issue_files(&issues_dir)?;
    Ok(all.into_iter().filter(|i| i.display_id.is_none()).collect())
}

fn git_rev_parse(repo_dir: &Path, ref_name: &str) -> Result<Option<String>> {
    let output = Command::new("git")
        .current_dir(repo_dir)
        .args(["rev-parse", "--verify", "--quiet", ref_name])
        .output()
        .with_context(|| format!("failed to run git rev-parse for '{ref_name}'"))?;
    if output.status.success() {
        let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if sha.is_empty() {
            return Ok(None);
        }
        Ok(Some(sha))
    } else {
        Ok(None)
    }
}

fn git_update_ref(repo_dir: &Path, ref_name: &str, sha: &str) -> Result<()> {
    let output = Command::new("git")
        .current_dir(repo_dir)
        .args(["update-ref", ref_name, sha])
        .output()
        .with_context(|| format!("failed to run git update-ref for '{ref_name}'"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "git update-ref {ref_name} -> {sha} failed: {}",
            stderr.trim()
        );
    }
    Ok(())
}

#[cfg(test)]
fn git_delete_ref(repo_dir: &Path, ref_name: &str) -> Result<()> {
    let output = Command::new("git")
        .current_dir(repo_dir)
        .args(["update-ref", "-d", ref_name])
        .output()
        .with_context(|| format!("failed to run git update-ref -d for '{ref_name}'"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git update-ref -d {ref_name} failed: {}", stderr.trim());
    }
    Ok(())
}

fn git_cat_file_blob_optional(repo_dir: &Path, blob_spec: &str) -> Result<Option<Vec<u8>>> {
    let output = Command::new("git")
        .current_dir(repo_dir)
        .args(["cat-file", "blob", blob_spec])
        .output()
        .with_context(|| format!("failed to run git cat-file for '{blob_spec}'"))?;
    if output.status.success() {
        return Ok(Some(output.stdout));
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("does not exist")
        || stderr.contains("Not a valid object name")
        || stderr.contains("not found")
    {
        return Ok(None);
    }
    bail!("git cat-file failed for '{blob_spec}': {}", stderr.trim())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::hub_v3::{agent_ref_name, read_hub_meta, HubVersion};
    use crate::identity::{AgentConfig, AgentRole};
    use std::process::Command;
    use std::sync::{Arc, Barrier, Mutex};
    use tempfile::TempDir;

    const GENERATION_POINTER: &str = "refs/heads/crosslink/reconciliation/current";

    struct PreparedBarrierImporter<'a> {
        inner: MigrationImporter<'a>,
        barrier: Arc<Barrier>,
        fingerprints: Arc<Mutex<Vec<String>>>,
    }

    impl HistoricalImporter for PreparedBarrierImporter<'_> {
        fn stabilize_source(&self, repository: &Path) -> Result<()> {
            HistoricalImporter::stabilize_source(&self.inner, repository)
        }

        fn snapshot_source_refs(
            &self,
            repository: &Path,
            source: &SourceEvidence,
        ) -> Result<BTreeMap<String, String>> {
            HistoricalImporter::snapshot_source_refs(&self.inner, repository, source)
        }

        fn prepare_file_source(
            &self,
            repository: &Path,
            source: &SourceEvidence,
            generation_id: &str,
        ) -> Result<PreparedImport> {
            self.fingerprints
                .lock()
                .unwrap()
                .push(source.fingerprint().to_string());
            let prepared = HistoricalImporter::prepare_file_source(
                &self.inner,
                repository,
                source,
                generation_id,
            )?;
            self.barrier.wait();
            Ok(prepared)
        }

        fn prepare_local_source(
            &self,
            repository: &Path,
            source: &SourceEvidence,
            generation_id: &str,
        ) -> Result<PreparedImport> {
            HistoricalImporter::prepare_local_source(&self.inner, repository, source, generation_id)
        }

        fn prepare_current_source(
            &self,
            repository: &Path,
            source: &SourceEvidence,
        ) -> Result<PreparedImport> {
            HistoricalImporter::prepare_current_source(&self.inner, repository, source)
        }

        fn file_source_is_newer(&self, repository: &Path, source: &SourceEvidence) -> Result<bool> {
            HistoricalImporter::file_source_is_newer(&self.inner, repository, source)
        }

        fn read_target_semantic(
            &self,
            repository: &Path,
            targets: &BTreeMap<String, String>,
        ) -> Result<CanonicalSemantic> {
            HistoricalImporter::read_target_semantic(&self.inner, repository, targets)
        }
    }

    fn setup_absent_hub() -> (TempDir, TempDir, std::path::PathBuf) {
        let remote_dir = tempfile::tempdir().unwrap();
        let work_dir = tempfile::tempdir().unwrap();
        run(remote_dir.path(), &["init", "--bare", "-b", "main"]);
        run(work_dir.path(), &["init", "-b", "main"]);
        run(
            work_dir.path(),
            &["config", "user.email", "test@test.local"],
        );
        run(work_dir.path(), &["config", "user.name", "Test"]);
        run(work_dir.path(), &["config", "commit.gpgsign", "false"]);
        run(
            work_dir.path(),
            &[
                "remote",
                "add",
                "origin",
                remote_dir.path().to_str().unwrap(),
            ],
        );
        std::fs::write(work_dir.path().join("README.md"), "# test\n").unwrap();
        run(work_dir.path(), &["add", "."]);
        run(work_dir.path(), &["commit", "-m", "init", "--no-gpg-sign"]);
        run(work_dir.path(), &["push", "-u", "origin", "main"]);
        let crosslink_dir = work_dir.path().join(".crosslink");
        std::fs::create_dir_all(&crosslink_dir).unwrap();
        std::fs::write(
            crosslink_dir.join("hook-config.json"),
            r#"{"remote":"origin"}"#,
        )
        .unwrap();
        write_agent(&crosslink_dir, "alpha");
        (work_dir, remote_dir, crosslink_dir)
    }

    pub(crate) fn setup_v2_hub() -> (TempDir, TempDir, std::path::PathBuf, std::path::PathBuf) {
        let remote_dir = tempfile::tempdir().unwrap();
        let work_dir = tempfile::tempdir().unwrap();

        run(remote_dir.path(), &["init", "--bare", "-b", "main"]);
        run(work_dir.path(), &["init", "-b", "main"]);
        let wp = work_dir.path().to_path_buf();
        run(&wp, &["config", "user.email", "test@test.local"]);
        run(&wp, &["config", "user.name", "Test"]);
        run(&wp, &["config", "commit.gpgsign", "false"]);
        run(
            &wp,
            &[
                "remote",
                "add",
                "origin",
                remote_dir.path().to_str().unwrap(),
            ],
        );
        std::fs::write(wp.join("README.md"), "# test\n").unwrap();
        run(&wp, &["add", "."]);
        run(&wp, &["commit", "-m", "init", "--no-gpg-sign"]);
        run(&wp, &["push", "-u", "origin", "main"]);

        let crosslink_dir = wp.join(".crosslink");
        std::fs::create_dir_all(&crosslink_dir).unwrap();
        std::fs::write(
            crosslink_dir.join("hook-config.json"),
            r#"{"remote":"origin","layout":"v2"}"#,
        )
        .unwrap();

        write_agent(&crosslink_dir, "alpha");

        let sync = SyncManager::new(&crosslink_dir).unwrap();

        let cache_dir = sync.cache_path().to_path_buf();
        run(
            &wp,
            &[
                "worktree",
                "add",
                "--orphan",
                "-b",
                "crosslink/hub",
                cache_dir.to_str().unwrap(),
            ],
        );
        run(&cache_dir, &["config", "user.email", "test@test.local"]);
        run(&cache_dir, &["config", "user.name", "Test"]);
        run(&cache_dir, &["config", "commit.gpgsign", "false"]);
        let meta_dir = cache_dir.join("meta");
        std::fs::create_dir_all(meta_dir.join("milestones")).unwrap();
        std::fs::create_dir_all(cache_dir.join("issues")).unwrap();
        std::fs::create_dir_all(cache_dir.join("locks")).unwrap();
        std::fs::create_dir_all(cache_dir.join("trust")).unwrap();
        crate::issue_file::write_layout_version(
            &meta_dir,
            crate::issue_file::CURRENT_LAYOUT_VERSION,
        )
        .unwrap();
        std::fs::write(
            cache_dir.join("locks.json"),
            serde_json::to_string(&serde_json::json!({"version":1,"locks":{},"settings":{"stale_lock_timeout_minutes":60}})).unwrap(),
        )
        .unwrap();
        run(&cache_dir, &["add", "-A"]);
        run(
            &cache_dir,
            &[
                "commit",
                "-m",
                "Initialize crosslink/hub branch",
                "--no-gpg-sign",
            ],
        );

        populate_alpha_v2(&cache_dir);

        write_second_agent(&cache_dir);

        let lock = sync.acquire_lock().unwrap();
        crate::compaction::compact(&cache_dir, "alpha", true, &lock).unwrap();
        drop(lock);

        (work_dir, remote_dir, crosslink_dir, cache_dir)
    }

    fn populate_alpha_v2(cache_dir: &Path) {
        use crate::events::{append_event, Event, EventEnvelope};
        let i1 = Uuid::parse_str("a1a1a1a1-a1a1-a1a1-a1a1-a1a1a1a1a1a1").unwrap();
        let i2 = Uuid::parse_str("a2a2a2a2-a2a2-a2a2-a2a2-a2a2a2a2a2a2").unwrap();
        let ms = Uuid::parse_str("cccccccc-cccc-cccc-cccc-cccccccccccc").unwrap();
        let c1 = Uuid::parse_str("dddddddd-dddd-dddd-dddd-dddddddddddd").unwrap();
        let c2 = Uuid::parse_str("eeeeeeee-eeee-eeee-eeee-eeeeeeeeeeee").unwrap();
        let base = Utc::now() - chrono::Duration::seconds(300);
        let log_path = cache_dir.join("agents").join("alpha").join("events.log");

        let events = vec![
            Event::IssueCreated {
                uuid: i1,
                title: "First issue".to_string(),
                description: Some("desc one".to_string()),
                priority: "high".to_string(),
                labels: vec![],
                parent_uuid: None,
                created_by: "alpha".to_string(),
                display_id: Some(1),
                scheduled_at: None,
                due_at: None,
            },
            Event::IssueCreated {
                uuid: i2,
                title: "Second issue".to_string(),
                description: None,
                priority: "medium".to_string(),
                labels: vec![],
                parent_uuid: None,
                created_by: "alpha".to_string(),
                display_id: Some(2),
                scheduled_at: None,
                due_at: None,
            },
            Event::LabelAdded {
                issue_uuid: i1,
                label: "bug".to_string(),
            },
            Event::LabelAdded {
                issue_uuid: i1,
                label: "urgent".to_string(),
            },
            Event::CommentAdded {
                issue_uuid: i1,
                comment_uuid: c1,
                display_id: Some(1),
                author: "alpha".to_string(),
                content: "a note".to_string(),
                created_at: base,
                kind: "note".to_string(),
                trigger_type: None,
                intervention_context: None,
                driver_key_fingerprint: None,
                signed_by: None,
                signature: None,
            },
            Event::CommentAdded {
                issue_uuid: i1,
                comment_uuid: c2,
                display_id: Some(2),
                author: "alpha".to_string(),
                content: "a plan".to_string(),
                created_at: base,
                kind: "plan".to_string(),
                trigger_type: None,
                intervention_context: None,
                driver_key_fingerprint: None,
                signed_by: None,
                signature: None,
            },
            Event::DependencyAdded {
                blocked_uuid: i1,
                blocker_uuid: i2,
            },
            Event::RelationAdded {
                uuid_a: i1,
                uuid_b: i2,
            },
            Event::MilestoneCreated {
                uuid: ms,
                display_id: Some(1),
                name: "v1.0".to_string(),
                description: Some("first release".to_string()),
                created_at: base,
            },
            Event::LockClaimed {
                issue_display_id: 2,
                branch: Some("feature/x".to_string()),
            },
        ];

        for (i, event) in events.into_iter().enumerate() {
            let env = EventEnvelope {
                agent_id: "alpha".to_string(),
                agent_seq: (i + 1) as u64,
                timestamp: base + chrono::Duration::seconds(i as i64),
                event,
                signed_by: None,
                signature: None,
            };
            append_event(&log_path, &env).unwrap();
        }

        let comments_dir = cache_dir
            .join("issues")
            .join(i1.to_string())
            .join("comments");
        std::fs::create_dir_all(&comments_dir).unwrap();
        for (cuuid, content, kind) in [(c1, "a note", "note"), (c2, "a plan", "plan")] {
            let cf = crate::issue_file::CommentFile {
                uuid: cuuid,
                issue_uuid: i1,
                author: "alpha".to_string(),
                content: content.to_string(),
                created_at: base,
                kind: kind.to_string(),
                trigger_type: None,
                intervention_context: None,
                driver_key_fingerprint: None,
                signed_by: None,
                signature: None,
            };
            crate::issue_file::write_comment_file(&comments_dir.join(format!("{cuuid}.json")), &cf)
                .unwrap();
        }
    }

    fn write_agent(crosslink_dir: &Path, id: &str) {
        let agent = AgentConfig {
            agent_id: id.to_string(),
            machine_id: "test-machine".to_string(),
            description: Some("test".to_string()),
            role: AgentRole::Driver,
            ssh_key_path: None,
            ssh_fingerprint: None,
            ssh_public_key: None,
        };
        std::fs::write(
            crosslink_dir.join("agent.json"),
            serde_json::to_string_pretty(&agent).unwrap(),
        )
        .unwrap();
    }

    fn write_second_agent(cache_dir: &Path) {
        use crate::events::{append_event, Event, EventEnvelope};
        let uuid = Uuid::parse_str("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb").unwrap();
        let now = Utc::now();
        let env = EventEnvelope {
            agent_id: "beta".to_string(),
            agent_seq: 1,
            timestamp: now - chrono::Duration::seconds(120),
            event: Event::IssueCreated {
                uuid,
                title: "Beta issue".to_string(),
                description: None,
                priority: "low".to_string(),
                labels: vec![],
                parent_uuid: None,
                created_by: "beta".to_string(),
                display_id: Some(3),
                scheduled_at: None,
                due_at: None,
            },
            signed_by: None,
            signature: None,
        };
        let log_path = cache_dir.join("agents").join("beta").join("events.log");
        append_event(&log_path, &env).unwrap();

        let issue = crate::issue_file::IssueFile {
            uuid,
            display_id: Some(3),
            title: "Beta issue".to_string(),
            description: None,
            status: crate::models::IssueStatus::Open,
            priority: crate::models::Priority::Low,
            parent_uuid: None,
            created_by: "beta".to_string(),
            created_at: now - chrono::Duration::seconds(120),
            updated_at: now - chrono::Duration::seconds(120),
            closed_at: None,
            scheduled_at: None,
            due_at: None,
            labels: vec![],
            comments: vec![],
            blockers: vec![],
            related: vec![],
            milestone_uuid: None,
            time_entries: vec![],
        };
        let dir = cache_dir.join("issues").join(uuid.to_string());
        std::fs::create_dir_all(&dir).unwrap();
        crate::issue_file::write_issue_file(&dir.join("issue.json"), &issue).unwrap();
    }

    fn write_v2_issue(cache_dir: &Path, uuid: Uuid, display_id: i64, title: &str, agent: &str) {
        let now = Utc::now();
        let issue = crate::issue_file::IssueFile {
            uuid,
            display_id: Some(display_id),
            title: title.to_string(),
            description: None,
            status: crate::models::IssueStatus::Open,
            priority: crate::models::Priority::Medium,
            parent_uuid: None,
            created_by: agent.to_string(),
            created_at: now,
            updated_at: now,
            closed_at: None,
            scheduled_at: None,
            due_at: None,
            labels: vec![],
            comments: vec![],
            blockers: vec![],
            related: vec![],
            milestone_uuid: None,
            time_entries: vec![],
        };
        let directory = cache_dir.join("issues").join(uuid.to_string());
        std::fs::create_dir_all(&directory).unwrap();
        crate::issue_file::write_issue_file(&directory.join("issue.json"), &issue).unwrap();
    }

    fn append_live_issue(
        cache: &Path,
        agent: &str,
        sequence: u64,
        uuid: Uuid,
        display_id: i64,
        title: &str,
    ) -> String {
        use crate::events::{Event, EventEnvelope};

        let envelope = EventEnvelope {
            agent_id: agent.to_string(),
            agent_seq: sequence,
            timestamp: Utc::now() + chrono::Duration::seconds(sequence as i64),
            event: Event::IssueCreated {
                uuid,
                title: title.to_string(),
                description: None,
                priority: "medium".to_string(),
                labels: vec![],
                parent_uuid: None,
                created_by: agent.to_string(),
                display_id: Some(display_id),
                scheduled_at: None,
                due_at: None,
            },
            signed_by: None,
            signature: None,
        };
        hub_v3::append_event_to_ref(cache, agent, &envelope)
            .unwrap()
            .new_commit
    }

    fn run(dir: &std::path::Path, args: &[&str]) {
        let out = Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn rev(dir: &Path, name: &str) -> Option<String> {
        git_rev_parse(dir, name).unwrap()
    }

    fn remote_rev(dir: &Path, name: &str) -> Option<String> {
        let output = Command::new("git")
            .current_dir(dir)
            .args(["ls-remote", "origin", name])
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .next()
            .map(str::to_string)
    }

    fn open_with_uncheckpointed_wal(path: &Path) -> crate::db::Database {
        drop(crate::db::Database::open(path).unwrap());
        let connection = rusqlite::Connection::open(path).unwrap();
        let mode: String = connection
            .query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
        let checkpoint: (i64, i64, i64) = connection
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .unwrap();
        assert_eq!(checkpoint.0, 0);
        drop(connection);
        let database = crate::db::Database::open(path).unwrap();
        database
            .conn
            .pragma_update(None, "wal_autocheckpoint", 0)
            .unwrap();
        database
    }

    fn assert_local_database_archive_contains(cache: &Path, issue_uuid: Uuid) {
        let descriptor_oid = remote_rev(cache, GENERATION_POINTER).unwrap();
        let output = Command::new("git")
            .current_dir(cache)
            .args(["show", &format!("{descriptor_oid}:generation.json")])
            .output()
            .unwrap();
        assert!(output.status.success());
        let descriptor: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let archive_oid = descriptor["archives"]["local/issues.db"]["oid"]
            .as_str()
            .unwrap();
        let materialized = tempfile::tempdir().unwrap();
        materialize_commit_tree(cache, archive_oid, materialized.path()).unwrap();
        let archived =
            crate::db::Database::open_read_only(&materialized.path().join("issues.db")).unwrap();
        assert!(archived
            .get_issue_id_by_uuid(&issue_uuid.to_string())
            .is_ok());
    }

    fn fresh_clone(remote: &Path, agent: &str) -> (TempDir, PathBuf) {
        let work = tempfile::tempdir().unwrap();
        run(
            work.path(),
            &["clone", remote.to_str().unwrap(), "repository"],
        );
        let repository = work.path().join("repository");
        run(&repository, &["config", "user.email", "test@test.local"]);
        run(&repository, &["config", "user.name", "Test"]);
        run(&repository, &["config", "commit.gpgsign", "false"]);
        let crosslink_dir = repository.join(".crosslink");
        std::fs::create_dir_all(&crosslink_dir).unwrap();
        std::fs::write(
            crosslink_dir.join("hook-config.json"),
            r#"{"remote":"origin"}"#,
        )
        .unwrap();
        write_agent(&crosslink_dir, agent);
        (work, crosslink_dir)
    }

    fn convert_remote_to_hidden_only(cache_dir: &Path) {
        let visible = Command::new("git")
            .current_dir(cache_dir)
            .args([
                "for-each-ref",
                "--format=%(refname) %(objectname)",
                "refs/heads/crosslink/meta",
                "refs/heads/crosslink/checkpoint",
                "refs/heads/crosslink/agents/",
            ])
            .output()
            .unwrap();
        assert!(visible.status.success());
        for line in String::from_utf8_lossy(&visible.stdout).lines() {
            let (reference, oid) = line.split_once(' ').unwrap();
            let hidden = if reference == META_REF {
                hub_v3::OLD_META_REF.to_string()
            } else if reference == CHECKPOINT_REF {
                hub_v3::OLD_CHECKPOINT_REF.to_string()
            } else {
                format!(
                    "{}{}",
                    hub_v3::OLD_AGENT_REF_PREFIX,
                    reference.strip_prefix(hub_v3::AGENT_REF_PREFIX).unwrap()
                )
            };
            run(cache_dir, &["update-ref", &hidden, oid]);
            run(
                cache_dir,
                &["push", "origin", &format!("{hidden}:{hidden}")],
            );
            run(cache_dir, &["push", "origin", &format!(":{reference}")]);
            run(cache_dir, &["update-ref", "-d", reference]);
        }
        let reconciliation = Command::new("git")
            .current_dir(cache_dir)
            .args([
                "ls-remote",
                "origin",
                "refs/heads/crosslink/reconciliation/*",
            ])
            .output()
            .unwrap();
        assert!(reconciliation.status.success());
        for line in String::from_utf8_lossy(&reconciliation.stdout).lines() {
            let reference = line.split_once('\t').unwrap().1;
            run(cache_dir, &["push", "origin", &format!(":{reference}")]);
        }
    }

    #[test]
    fn historical_snapshot_excludes_operational_lock_and_tracks_source_changes() {
        let (_work, _remote, _crosslink_dir, cache_dir) = setup_v2_hub();
        let authority = rev(&cache_dir, V2_HUB_BRANCH).unwrap();
        std::fs::write(cache_dir.join(".hub-write-lock"), "first process").unwrap();
        let first = snapshot_worktree(&cache_dir, &authority).unwrap();
        std::fs::write(cache_dir.join(".hub-write-lock"), "second process").unwrap();
        let second = snapshot_worktree(&cache_dir, &authority).unwrap();
        assert_eq!(first, second);
        std::fs::write(cache_dir.join("issues").join("new-source-data"), "changed").unwrap();
        let changed = snapshot_worktree(&cache_dir, &authority).unwrap();
        assert_ne!(first, changed);
        let lock = Command::new("git")
            .current_dir(&cache_dir)
            .args(["cat-file", "-e", &format!("{first}:.hub-write-lock")])
            .output()
            .unwrap();
        assert!(!lock.status.success());
    }

    #[test]
    fn missing_projection_without_authority_blocks_without_creating_state() {
        let (work, _remote, crosslink_dir) = setup_absent_hub();
        let sync = SyncManager::new(&crosslink_dir).unwrap();
        run(
            work.path(),
            &[
                "worktree",
                "add",
                "--detach",
                sync.cache_path().to_str().unwrap(),
                "HEAD",
            ],
        );
        let activation = activate_repository(&crosslink_dir).unwrap();
        assert!(matches!(
            activation,
            RepositoryActivation::BlockedCorrupt { ref reason }
                if reason.contains("no shared authority")
        ));
        assert!(!crosslink_dir.join("issues.db").exists());
        assert!(remote_rev(sync.cache_path(), GENERATION_POINTER).is_none());
    }

    #[test]
    fn missing_projection_is_rebuilt_from_current_authority() {
        let (_work, _remote, crosslink_dir, cache_dir) = setup_v2_hub();
        run(&cache_dir, &["push", "origin", "crosslink/hub"]);
        hub_v3(&crosslink_dir, false, false, false, false).unwrap();
        let expected = compaction::reduce(&RefHubSource::new(&cache_dir).unwrap())
            .unwrap()
            .state;
        let path = crosslink_dir.join("issues.db");
        assert!(path.is_file());
        fs::remove_file(&path).unwrap();
        let activation = activate_repository(&crosslink_dir).unwrap();
        assert!(matches!(
            activation,
            RepositoryActivation::ReadyCurrent { .. } | RepositoryActivation::ReadyAdopted { .. }
        ));
        assert!(path.is_file());
        let rebuilt = crate::db::Database::open_read_only(&path).unwrap();
        verify_projection_database(&crosslink_dir, &rebuilt, &expected).unwrap();
    }

    #[test]
    fn concurrent_clones_publish_migrated_and_adopted_readiness_with_equal_state() {
        let (_source, remote, _source_crosslink, source_cache) = setup_v2_hub();
        run(&source_cache, &["push", "origin", "crosslink/hub"]);
        let (_first, first_crosslink) = fresh_clone(remote.path(), "racer-a");
        let (_second, second_crosslink) = fresh_clone(remote.path(), "racer-b");
        for crosslink in [&first_crosslink, &second_crosslink] {
            assert_eq!(
                SyncManager::new(crosslink)
                    .unwrap()
                    .init_cache_for_reconciliation(),
                crate::sync::ReconciliationCacheOutcome::Ready
            );
        }
        let barrier = Barrier::new(3);
        let (first_activation, second_activation) = std::thread::scope(|scope| {
            let first = scope.spawn(|| {
                barrier.wait();
                activate_repository(&first_crosslink).unwrap()
            });
            let second = scope.spawn(|| {
                barrier.wait();
                activate_repository(&second_crosslink).unwrap()
            });
            barrier.wait();
            (first.join().unwrap(), second.join().unwrap())
        });
        let mut migrated = None;
        let mut adopted = None;
        for (crosslink, activation) in [
            (&first_crosslink, first_activation),
            (&second_crosslink, second_activation),
        ] {
            let (state, generation) = match activation {
                RepositoryActivation::ReadyMigrated { generation_id } => (
                    crate::reconcile::readiness::ReadinessState::ReadyMigrated,
                    generation_id,
                ),
                RepositoryActivation::ReadyAdopted { generation_id } => (
                    crate::reconcile::readiness::ReadinessState::ReadyAdopted,
                    generation_id,
                ),
                other => panic!("unexpected concurrent activation: {other:?}"),
            };
            let identity = crate::reconcile::readiness::DaemonIdentity {
                schema_version: crate::reconcile::readiness::READINESS_SCHEMA_VERSION,
                repository_id: crate::reconcile::readiness::repository_id(crosslink).unwrap(),
                daemon_epoch: Uuid::new_v4().to_string(),
                pid: std::process::id(),
                process_start: crate::reconcile::readiness::current_process_start_token().unwrap(),
            };
            crate::reconcile::readiness::write_daemon_identity(crosslink, &identity).unwrap();
            crate::reconcile::readiness::write_record(
                crosslink,
                crate::reconcile::readiness::ReadinessDraft {
                    daemon_epoch: &identity.daemon_epoch,
                    daemon_pid: identity.pid,
                    attempt_id: "two-clone-activation",
                    state,
                    generation_id: Some(&generation),
                    reason: None,
                },
            )
            .unwrap();
            let record = crate::reconcile::readiness::read_record(crosslink)
                .unwrap()
                .unwrap();
            assert_eq!(record.state, state);
            match state {
                crate::reconcile::readiness::ReadinessState::ReadyMigrated => {
                    migrated = Some((crosslink, generation));
                }
                crate::reconcile::readiness::ReadinessState::ReadyAdopted => {
                    adopted = Some((crosslink, generation));
                }
                other => panic!("unexpected ready state: {other:?}"),
            }
        }
        let (migrated_crosslink, migrated_generation) = migrated.unwrap();
        let (adopted_crosslink, adopted_generation) = adopted.unwrap();
        assert_eq!(migrated_generation, adopted_generation);
        let migrated_cache = SyncManager::new(migrated_crosslink).unwrap();
        let adopted_cache = SyncManager::new(adopted_crosslink).unwrap();
        let migrated_state =
            compaction::reduce(&RefHubSource::new(migrated_cache.cache_path()).unwrap())
                .unwrap()
                .state;
        let adopted_state =
            compaction::reduce(&RefHubSource::new(adopted_cache.cache_path()).unwrap())
                .unwrap()
                .state;
        assert_eq!(
            serde_json::to_value(&migrated_state).unwrap(),
            serde_json::to_value(&adopted_state).unwrap()
        );
        for crosslink in [migrated_crosslink, adopted_crosslink] {
            let database =
                crate::db::Database::open_read_only(&crosslink.join("issues.db")).unwrap();
            verify_projection_database(crosslink, &database, &migrated_state).unwrap();
        }
        let current = activate_repository(migrated_crosslink).unwrap();
        let RepositoryActivation::ReadyCurrent { generation_id } = current else {
            panic!("expected current activation after migration: {current:?}");
        };
        let identity = crate::reconcile::readiness::read_daemon_identity(migrated_crosslink)
            .unwrap()
            .unwrap();
        crate::reconcile::readiness::write_record(
            migrated_crosslink,
            crate::reconcile::readiness::ReadinessDraft {
                daemon_epoch: &identity.daemon_epoch,
                daemon_pid: identity.pid,
                attempt_id: "current-activation",
                state: crate::reconcile::readiness::ReadinessState::ReadyCurrent,
                generation_id: Some(&generation_id),
                reason: None,
            },
        )
        .unwrap();
        assert_eq!(
            crate::reconcile::readiness::read_record(migrated_crosslink)
                .unwrap()
                .unwrap()
                .state,
            crate::reconcile::readiness::ReadinessState::ReadyCurrent
        );
    }

    #[test]
    fn fresh_clone_discovers_and_imports_remote_hidden_v3() {
        let (_source_work, remote, source_crosslink, source_cache) = setup_v2_hub();
        run(&source_cache, &["push", "origin", "crosslink/hub"]);
        hub_v3(&source_crosslink, false, false, false, false).unwrap();
        convert_remote_to_hidden_only(&source_cache);
        let (_clone, crosslink_dir) = fresh_clone(remote.path(), "hidden-reader");
        let sync = SyncManager::new(&crosslink_dir).unwrap();
        assert_eq!(
            sync.init_cache_for_reconciliation(),
            crate::sync::ReconciliationCacheOutcome::Ready
        );
        let format = crate::reconcile::check_repository(&crosslink_dir).format;
        assert!(matches!(
            format.shared_store,
            SharedStoreFormat::HiddenV3 { .. }
        ));
        hub_v3(&crosslink_dir, false, false, false, false).unwrap();
        let state = compaction::reduce(&RefHubSource::new(sync.cache_path()).unwrap())
            .unwrap()
            .state;
        assert_eq!(state.issues.len(), 3);
        assert!(remote_rev(sync.cache_path(), GENERATION_POINTER).is_some());
        assert!(remote_rev(sync.cache_path(), hub_v3::OLD_META_REF).is_none());
        assert!(remote_rev(sync.cache_path(), hub_v3::OLD_CHECKPOINT_REF).is_none());
    }

    #[test]
    fn fresh_clone_discovers_hidden_v3_alongside_remote_v2() {
        let (_source_work, remote, source_crosslink, source_cache) = setup_v2_hub();
        run(&source_cache, &["push", "origin", "crosslink/hub"]);
        let v2_tip = rev(&source_cache, V2_HUB_BRANCH).unwrap();
        hub_v3(&source_crosslink, false, false, false, false).unwrap();
        convert_remote_to_hidden_only(&source_cache);
        git_update_ref(&source_cache, V2_HUB_BRANCH, &v2_tip).unwrap();
        run(
            &source_cache,
            &[
                "push",
                "origin",
                &format!("{V2_HUB_BRANCH}:{V2_HUB_BRANCH}"),
            ],
        );
        let (_clone, crosslink_dir) = fresh_clone(remote.path(), "mixed-reader");
        let sync = SyncManager::new(&crosslink_dir).unwrap();
        assert_eq!(
            sync.init_cache_for_reconciliation(),
            crate::sync::ReconciliationCacheOutcome::Ready
        );
        let format = crate::reconcile::check_repository(&crosslink_dir).format;
        assert!(matches!(
            format.shared_store,
            SharedStoreFormat::Mixed { .. }
        ));
        assert!(rev(sync.cache_path(), hub_v3::OLD_META_REF).is_some());
        assert!(rev(sync.cache_path(), hub_v3::OLD_CHECKPOINT_REF).is_some());
    }

    #[test]
    fn unreachable_configured_remote_does_not_create_empty_cache() {
        let (work, _remote, crosslink_dir) = setup_absent_hub();
        let unavailable = work.path().join("unavailable-remote.git");
        run(
            work.path(),
            &["remote", "set-url", "origin", unavailable.to_str().unwrap()],
        );
        let sync = SyncManager::new(&crosslink_dir).unwrap();
        let error = hub_v3(&crosslink_dir, false, false, false, false).unwrap_err();
        assert!(error.to_string().contains("ls-remote"));
        assert!(!sync.cache_path().exists());
    }

    #[test]
    fn late_remote_v2_branch_is_discovered_after_empty_generation() {
        let (_work, _remote, crosslink_dir) = setup_absent_hub();
        drop(crate::db::Database::open(&crosslink_dir.join("issues.db")).unwrap());
        hub_v3(&crosslink_dir, false, false, false, false).unwrap();
        let sync = SyncManager::new(&crosslink_dir).unwrap();
        let cache_dir = sync.cache_path();
        let first_generation = remote_rev(cache_dir, GENERATION_POINTER).unwrap();
        let tree = git_with_input(cache_dir, &["mktree"], &[]).unwrap();
        let late_tip = commit_snapshot_tree(cache_dir, tree.trim(), None).unwrap();
        git_update_ref(cache_dir, V2_HUB_BRANCH, &late_tip).unwrap();
        run(
            cache_dir,
            &[
                "push",
                "origin",
                &format!("{V2_HUB_BRANCH}:{V2_HUB_BRANCH}"),
            ],
        );
        git_delete_ref(cache_dir, V2_HUB_BRANCH).unwrap();
        hub_v3(&crosslink_dir, false, false, false, false).unwrap();
        assert!(rev(cache_dir, V2_HUB_BRANCH).is_none());
        assert_ne!(
            remote_rev(cache_dir, GENERATION_POINTER),
            Some(first_generation)
        );
        let descriptor = remote_rev(cache_dir, GENERATION_POINTER).unwrap();
        let bytes = Command::new("git")
            .current_dir(cache_dir)
            .args(["show", &format!("{descriptor}:generation.json")])
            .output()
            .unwrap();
        assert!(bytes.status.success());
        let value: serde_json::Value = serde_json::from_slice(&bytes.stdout).unwrap();
        assert_eq!(
            value["source"]["refs"][V2_HUB_BRANCH]["authority_oid"],
            late_tip
        );
        let late_generation = remote_rev(cache_dir, GENERATION_POINTER).unwrap();
        let next_tip = commit_snapshot_tree(cache_dir, tree.trim(), Some(&late_tip)).unwrap();
        run(
            cache_dir,
            &["push", "origin", &format!("{next_tip}:{V2_HUB_BRANCH}")],
        );
        hub_v3(&crosslink_dir, false, false, false, false).unwrap();
        assert!(rev(cache_dir, V2_HUB_BRANCH).is_none());
        assert_ne!(
            remote_rev(cache_dir, GENERATION_POINTER),
            Some(late_generation)
        );
    }

    #[test]
    fn local_only_database_is_archived_and_imported_without_mutation() {
        let (_work, _remote, crosslink_dir) = setup_absent_hub();
        let path = crosslink_dir.join("issues.db");
        let database = open_with_uncheckpointed_wal(&path);
        let first = database
            .create_issue("local one", Some("source"), "high")
            .unwrap();
        let second = database.create_issue("local two", None, "low").unwrap();
        database
            .add_comment(first, "local comment", "note")
            .unwrap();
        database.add_dependency(first, second).unwrap();
        database.add_relation(first, second).unwrap();
        database.add_label(first, "local").unwrap();
        let first_uuid: Uuid = database
            .get_issue_uuid_by_id(first)
            .unwrap()
            .parse()
            .unwrap();
        let database_before = std::fs::read(&path).unwrap();
        let wal_path = PathBuf::from(format!("{}-wal", path.display()));
        let wal_before = std::fs::read(&wal_path).unwrap();
        assert!(!wal_before.is_empty());

        hub_v3(&crosslink_dir, false, false, false, false).unwrap();
        let sync = SyncManager::new(&crosslink_dir).unwrap();
        let cache_dir = sync.cache_path();
        let state = compaction::reduce(&RefHubSource::new(cache_dir).unwrap())
            .unwrap()
            .state;
        assert_eq!(state.issues.len(), 2);
        let imported = &state.issues[&first_uuid];
        assert_eq!(imported.comments.len(), 1);
        assert_eq!(imported.blockers.len(), 1);
        assert_eq!(imported.related.len(), 1);
        assert_eq!(std::fs::read(&path).unwrap(), database_before);
        assert_eq!(std::fs::read(&wal_path).unwrap(), wal_before);
        assert_local_database_archive_contains(cache_dir, first_uuid);
        let archives = Command::new("git")
            .current_dir(cache_dir)
            .args([
                "ls-remote",
                "origin",
                "refs/heads/crosslink/reconciliation/archives/*",
            ])
            .output()
            .unwrap();
        assert!(archives.status.success());
        assert!(!archives.stdout.is_empty());
        let generation = remote_rev(cache_dir, GENERATION_POINTER);
        hub_v3(&crosslink_dir, false, false, false, false).unwrap();
        assert_eq!(remote_rev(cache_dir, GENERATION_POINTER), generation);
        drop(database);
    }

    #[test]
    fn current_v3_merges_committed_wal_authority_without_mutating_local_files() {
        let (_work, _remote, crosslink_dir, cache_dir) = setup_v2_hub();
        hub_v3(&crosslink_dir, false, false, false, false).unwrap();
        let database = open_with_uncheckpointed_wal(&crosslink_dir.join("issues.db"));
        let local_id = database
            .create_issue("current v3 local WAL issue", None, "high")
            .unwrap();
        let local_uuid: Uuid = database
            .get_issue_uuid_by_id(local_id)
            .unwrap()
            .parse()
            .unwrap();
        let database_path = crosslink_dir.join("issues.db");
        let wal_path = PathBuf::from(format!("{}-wal", database_path.display()));
        let database_before = fs::read(&database_path).unwrap();
        let wal_before = fs::read(&wal_path).unwrap();
        assert!(!wal_before.is_empty());

        hub_v3(&crosslink_dir, false, false, false, false).unwrap();
        let state = compaction::reduce(&RefHubSource::new(&cache_dir).unwrap())
            .unwrap()
            .state;
        assert!(state.issues.contains_key(&local_uuid));
        assert_eq!(fs::read(&database_path).unwrap(), database_before);
        assert_eq!(fs::read(&wal_path).unwrap(), wal_before);
        assert_local_database_archive_contains(&cache_dir, local_uuid);
        drop(database);
    }

    #[test]
    fn distinct_pre_shared_local_databases_merge_after_first_publication() {
        let (_first_work, remote, first_crosslink) = setup_absent_hub();
        let (_second_work, second_crosslink) = fresh_clone(remote.path(), "beta");
        let first_database = crate::db::Database::open(&first_crosslink.join("issues.db")).unwrap();
        let first_id = first_database
            .create_issue("first local issue", None, "medium")
            .unwrap();
        let first_uuid: Uuid = first_database
            .get_issue_uuid_by_id(first_id)
            .unwrap()
            .parse()
            .unwrap();
        drop(first_database);
        let second_database =
            crate::db::Database::open(&second_crosslink.join("issues.db")).unwrap();
        let second_id = second_database
            .create_issue("second local issue", None, "high")
            .unwrap();
        let second_uuid: Uuid = second_database
            .get_issue_uuid_by_id(second_id)
            .unwrap()
            .parse()
            .unwrap();
        second_database.add_label(second_id, "second-only").unwrap();
        drop(second_database);

        hub_v3(&first_crosslink, false, false, false, false).unwrap();
        let first_cache = SyncManager::new(&first_crosslink).unwrap();
        let first_generation = remote_rev(first_cache.cache_path(), GENERATION_POINTER).unwrap();
        hub_v3(&second_crosslink, false, false, false, false).unwrap();
        let second_cache = SyncManager::new(&second_crosslink).unwrap();
        let second_generation = remote_rev(second_cache.cache_path(), GENERATION_POINTER).unwrap();
        assert_ne!(first_generation, second_generation);
        let state = compaction::reduce(&RefHubSource::new(second_cache.cache_path()).unwrap())
            .unwrap()
            .state;
        assert!(state.issues.contains_key(&first_uuid));
        assert!(state.issues.contains_key(&second_uuid));
        assert!(state.issues[&second_uuid].labels.contains("second-only"));
        let hydrated = crate::db::Database::open(&second_crosslink.join("issues.db")).unwrap();
        assert_eq!(
            hydrated.list_issues(Some("all"), None, None).unwrap().len(),
            2
        );
        assert!(hydrated
            .get_issue_id_by_uuid(&first_uuid.to_string())
            .is_ok());
        assert!(hydrated
            .get_issue_id_by_uuid(&second_uuid.to_string())
            .is_ok());
        drop(hydrated);
        hub_v3(&second_crosslink, false, false, false, false).unwrap();
        assert_eq!(
            remote_rev(second_cache.cache_path(), GENERATION_POINTER),
            Some(second_generation)
        );
    }

    #[test]
    fn conflicting_pre_shared_local_database_blocks_without_mutation() {
        let (_first_work, remote, first_crosslink) = setup_absent_hub();
        let (_second_work, second_crosslink) = fresh_clone(remote.path(), "beta");
        let first_database = crate::db::Database::open(&first_crosslink.join("issues.db")).unwrap();
        let first_id = first_database
            .create_issue("shared identity", None, "medium")
            .unwrap();
        let shared_uuid = first_database.get_issue_uuid_by_id(first_id).unwrap();
        drop(first_database);
        let second_path = second_crosslink.join("issues.db");
        let second_database = crate::db::Database::open(&second_path).unwrap();
        let second_id = second_database
            .create_issue("conflicting identity", None, "medium")
            .unwrap();
        second_database
            .conn
            .execute(
                "UPDATE issues SET uuid = ?1 WHERE id = ?2",
                rusqlite::params![shared_uuid, second_id],
            )
            .unwrap();
        drop(second_database);
        let database_before = fs::read(&second_path).unwrap();

        hub_v3(&first_crosslink, false, false, false, false).unwrap();
        let first_cache = SyncManager::new(&first_crosslink).unwrap();
        let generation = remote_rev(first_cache.cache_path(), GENERATION_POINTER).unwrap();
        let error = hub_v3(&second_crosslink, false, false, false, false).unwrap_err();
        assert!(format!("{error:#}").contains("conflicts with shared authority"));
        let second_cache = SyncManager::new(&second_crosslink).unwrap();
        assert_eq!(
            remote_rev(second_cache.cache_path(), GENERATION_POINTER),
            Some(generation)
        );
        assert_eq!(fs::read(&second_path).unwrap(), database_before);
        let anchored = Command::new("git")
            .current_dir(second_cache.cache_path())
            .args([
                "for-each-ref",
                "--format=%(refname)",
                "refs/crosslink/reconciliation/intents/",
            ])
            .output()
            .unwrap();
        assert!(anchored.status.success());
        assert!(!anchored.stdout.is_empty());
    }

    #[test]
    fn migrate_happy_path_and_rerun_is_noop() {
        let (_w, _r, crosslink_dir, cache_dir) = setup_v2_hub();

        hub_v3(&crosslink_dir, false, false, false, false).unwrap();

        assert!(rev(&cache_dir, CHECKPOINT_REF).is_some());
        assert!(rev(&cache_dir, META_REF).is_some());
        assert!(rev(&cache_dir, &agent_ref_name("alpha").unwrap()).is_some());
        assert!(rev(&cache_dir, &agent_ref_name("beta").unwrap()).is_some());

        let meta = read_hub_meta(&cache_dir).unwrap().unwrap();
        assert_eq!(meta.hub_version, 3);
        assert!(!meta.migrated_from_commit.is_empty());
        assert!(meta.finalized_at.is_none());

        assert_eq!(
            hub_v3::detect_hub_version(&cache_dir).unwrap(),
            HubVersion::V3 {
                v2_branch_present: false
            }
        );

        let cp = rev(&cache_dir, CHECKPOINT_REF);
        let mt = rev(&cache_dir, META_REF);
        let al = rev(&cache_dir, &agent_ref_name("alpha").unwrap());
        let projection_path = crosslink_dir.join("issues.db");
        let projection_before = fs::read(&projection_path).unwrap();
        hub_v3(&crosslink_dir, false, false, false, false).unwrap();
        assert_eq!(cp, rev(&cache_dir, CHECKPOINT_REF));
        assert_eq!(mt, rev(&cache_dir, META_REF));
        assert_eq!(al, rev(&cache_dir, &agent_ref_name("alpha").unwrap()));
        assert_eq!(fs::read(projection_path).unwrap(), projection_before);

        assert!(rev(&cache_dir, V2_HUB_BRANCH).is_none());
    }

    #[test]
    fn current_generation_allows_two_immediate_shared_mutations() {
        let (_work, _remote, crosslink_dir, cache_dir) = setup_v2_hub();
        hub_v3(&crosslink_dir, false, false, false, false).unwrap();
        let identity = crate::reconcile::readiness::DaemonIdentity {
            schema_version: crate::reconcile::readiness::READINESS_SCHEMA_VERSION,
            repository_id: crate::reconcile::readiness::repository_id(&crosslink_dir).unwrap(),
            daemon_epoch: "sequential-mutation-test".to_string(),
            pid: std::process::id(),
            process_start: crate::reconcile::readiness::current_process_start_token().unwrap(),
        };
        crate::reconcile::readiness::write_daemon_identity(&crosslink_dir, &identity).unwrap();
        let generation =
            crate::reconcile::publication::generation_id_at_ref(&cache_dir, GENERATION_POINTER)
                .unwrap()
                .unwrap();
        crate::reconcile::readiness::write_record(
            &crosslink_dir,
            crate::reconcile::readiness::ReadinessDraft {
                daemon_epoch: &identity.daemon_epoch,
                daemon_pid: identity.pid,
                attempt_id: "sequential-mutation-attempt",
                state: crate::reconcile::readiness::ReadinessState::ReadyCurrent,
                generation_id: Some(&generation),
                reason: None,
            },
        )
        .unwrap();
        let initial_frontier = crate::reconcile::readiness::read_record(&crosslink_dir)
            .unwrap()
            .unwrap()
            .projection_frontier
            .unwrap();
        let db = crate::db::Database::open(&crosslink_dir.join("issues.db")).unwrap();
        let writer = crate::shared_writer::SharedWriter::new(&crosslink_dir)
            .unwrap()
            .unwrap();
        let first = writer
            .create_issue(&db, "first immediate mutation", None, "medium", None, None)
            .unwrap();
        let after_first = crate::reconcile::readiness::read_record(&crosslink_dir)
            .unwrap()
            .unwrap();
        assert_ne!(
            after_first.projection_frontier.as_deref(),
            Some(initial_frontier.as_str())
        );
        crate::reconcile::readiness::validate_record(&crosslink_dir, &after_first).unwrap();
        let second = writer
            .create_issue(&db, "second immediate mutation", None, "medium", None, None)
            .unwrap();
        assert!(second > first);
        assert!(crate::reconcile::readiness::projection_is_current(&crosslink_dir).unwrap());
        crate::reconcile::readiness::require_mutation_ready(&crosslink_dir).unwrap();
        let current = crate::reconcile::readiness::read_record(&crosslink_dir)
            .unwrap()
            .unwrap();
        crate::reconcile::readiness::validate_record(&crosslink_dir, &current).unwrap();
        let mut stale = current;
        stale.projection_frontier = Some(initial_frontier);
        assert!(
            crate::reconcile::readiness::validate_record(&crosslink_dir, &stale)
                .unwrap_err()
                .to_string()
                .contains("projection frontier is stale")
        );
    }

    #[test]
    fn pinned_trust_payload_is_preserved_in_verified_meta() {
        let (_work, _remote, crosslink_dir, cache_dir) = setup_v2_hub();
        let allowed_signers = b"agent@example ssh-ed25519 AAAATEST\n";
        std::fs::write(
            cache_dir.join("trust").join("allowed_signers"),
            allowed_signers,
        )
        .unwrap();
        hub_v3(&crosslink_dir, false, false, false, false).unwrap();
        let meta = rev(&cache_dir, META_REF).unwrap();
        let preserved = Command::new("git")
            .current_dir(&cache_dir)
            .args(["show", &format!("{meta}:allowed_signers")])
            .output()
            .unwrap();
        assert!(preserved.status.success());
        assert_eq!(preserved.stdout, allowed_signers);
    }

    #[test]
    fn legacy_locks_source_publishes_and_preserves_archive() {
        let (_work, _remote, crosslink_dir, cache_dir) = setup_v2_hub();
        run(&cache_dir, &["branch", "-m", "crosslink/locks"]);
        assert!(rev(&cache_dir, V2_HUB_BRANCH).is_none());
        let legacy_tip = rev(&cache_dir, "refs/heads/crosslink/locks").unwrap();
        hub_v3(&crosslink_dir, false, false, false, false).unwrap();
        assert!(rev(&cache_dir, "refs/heads/crosslink/locks").is_none());
        let archives = Command::new("git")
            .current_dir(&cache_dir)
            .args([
                "ls-remote",
                "origin",
                "refs/heads/crosslink/reconciliation/archives/*",
            ])
            .output()
            .unwrap();
        assert!(archives.status.success());
        assert!(!archives.stdout.is_empty());
        assert!(String::from_utf8_lossy(&archives.stdout).contains(&legacy_tip));
    }

    #[test]
    fn mixed_partial_v3_is_rebuilt_from_pinned_v2_without_deletion() {
        let (_work, _remote, crosslink_dir, cache_dir) = setup_v2_hub();
        let partial = serde_json::to_vec(&CheckpointState::default()).unwrap();
        let partial_oid = hub_v3::commit_blob_to_ref(
            &cache_dir,
            CHECKPOINT_REF,
            "state.json",
            &partial,
            "partial v3 checkpoint",
        )
        .unwrap();
        let v2_tip = rev(&cache_dir, V2_HUB_BRANCH).unwrap();
        hub_v3(&crosslink_dir, false, false, false, false).unwrap();
        assert!(rev(&cache_dir, V2_HUB_BRANCH).is_none());
        assert_ne!(rev(&cache_dir, CHECKPOINT_REF), Some(partial_oid));
        let descriptor = remote_rev(&cache_dir, GENERATION_POINTER).unwrap();
        let bytes = Command::new("git")
            .current_dir(&cache_dir)
            .args(["show", &format!("{descriptor}:generation.json")])
            .output()
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes.stdout).unwrap();
        assert_eq!(
            value["source"]["refs"][V2_HUB_BRANCH]["authority_oid"],
            v2_tip
        );
        let state = compaction::reduce(&RefHubSource::new(&cache_dir).unwrap())
            .unwrap()
            .state;
        assert!(!state.issues.is_empty());
    }

    #[test]
    fn genesis_equals_files_via_refhubsource() {
        let (_w, _r, crosslink_dir, cache_dir) = setup_v2_hub();
        hub_v3(&crosslink_dir, false, false, false, false).unwrap();

        let genesis = build_genesis_from_files(&cache_dir).unwrap();
        let source = RefHubSource::new(&cache_dir).unwrap();
        let reduced = crate::compaction::reduce(&source).unwrap().state;

        assert_eq!(reduced.issues.len(), genesis.issues.len());
        assert_eq!(reduced.milestones.len(), genesis.milestones.len());

        for (uuid, g) in &genesis.issues {
            let r = reduced
                .issues
                .get(uuid)
                .expect("issue present after reduce");
            assert_eq!(
                serde_json::to_value(g).unwrap(),
                serde_json::to_value(r).unwrap(),
                "issue {uuid} must match"
            );
        }

        let with_comments = genesis
            .issues
            .values()
            .filter(|i| i.comments.len() == 2)
            .count();
        assert!(with_comments >= 1, "expected an issue with 2 comments");
    }

    #[test]
    fn new_event_above_watermark_is_applied_pre_genesis_is_not() {
        let (_w, _r, crosslink_dir, cache_dir) = setup_v2_hub();
        hub_v3(&crosslink_dir, false, false, false, false).unwrap();

        let genesis = build_genesis_from_files(&cache_dir).unwrap();

        let base = crate::compaction::reduce(&RefHubSource::new(&cache_dir).unwrap())
            .unwrap()
            .state;
        assert_eq!(
            serde_json::to_value(&genesis).unwrap(),
            serde_json::to_value(&base).unwrap(),
            "reduce must equal genesis (pre-genesis events not re-applied)"
        );

        use crate::events::{Event, EventEnvelope};
        let new_uuid = Uuid::new_v4();
        let env = EventEnvelope {
            agent_id: "alpha".to_string(),
            agent_seq: 9999,
            timestamp: Utc::now() + chrono::Duration::seconds(60),
            event: Event::IssueCreated {
                uuid: new_uuid,
                title: "Post-genesis issue".to_string(),
                description: None,
                priority: "medium".to_string(),
                labels: vec![],
                parent_uuid: None,
                created_by: "alpha".to_string(),
                display_id: None,
                scheduled_at: None,
                due_at: None,
            },
            signed_by: None,
            signature: None,
        };

        let tip = rev(&cache_dir, &agent_ref_name("alpha").unwrap()).unwrap();
        let mut bytes = git_cat_file_blob_optional(&cache_dir, &format!("{tip}:events.log"))
            .unwrap()
            .unwrap();
        bytes.extend_from_slice(serde_json::to_string(&env).unwrap().as_bytes());
        bytes.push(b'\n');
        hub_v3::commit_log_bytes(&cache_dir, "alpha", &bytes, "test: post-genesis event").unwrap();

        let after = crate::compaction::reduce(&RefHubSource::new(&cache_dir).unwrap())
            .unwrap()
            .state;
        assert!(
            after.issues.contains_key(&new_uuid),
            "event above watermark must be applied"
        );

        assert_eq!(after.issues.len(), genesis.issues.len() + 1);
    }

    #[test]
    fn duplicate_display_id_refuses_migration() {
        let (_w, _r, crosslink_dir, cache_dir) = setup_v2_hub();

        let dup_uuid = Uuid::new_v4();
        let issue = crate::issue_file::IssueFile {
            uuid: dup_uuid,
            display_id: Some(1),
            title: "Dup id".to_string(),
            description: None,
            status: crate::models::IssueStatus::Open,
            priority: crate::models::Priority::Medium,
            parent_uuid: None,
            created_by: "alpha".to_string(),
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
        };
        let dir = cache_dir.join("issues").join(dup_uuid.to_string());
        std::fs::create_dir_all(&dir).unwrap();
        crate::issue_file::write_issue_file(&dir.join("issue.json"), &issue).unwrap();

        let err = hub_v3(&crosslink_dir, false, false, false, false).unwrap_err();
        assert!(
            err.to_string().contains("duplicate display_id"),
            "must refuse on duplicate display_id, got: {err}"
        );

        assert!(rev(&cache_dir, CHECKPOINT_REF).is_none());
        assert!(rev(&cache_dir, META_REF).is_none());
    }

    #[test]
    fn orphaned_offline_issue_gets_minted_genesis_id() {
        let (_w, _r, crosslink_dir, cache_dir) = setup_v2_hub();

        let orphan_uuid = Uuid::new_v4();
        let issue = crate::issue_file::IssueFile {
            uuid: orphan_uuid,
            display_id: None,
            title: "Orphaned offline relic".to_string(),
            description: None,
            status: crate::models::IssueStatus::Closed,
            priority: crate::models::Priority::Medium,
            parent_uuid: None,
            created_by: "dead-kickoff-agent".to_string(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            closed_at: Some(Utc::now()),
            scheduled_at: None,
            due_at: None,
            labels: vec![],
            comments: vec![],
            blockers: vec![],
            related: vec![],
            milestone_uuid: None,
            time_entries: vec![],
        };
        let dir = cache_dir.join("issues").join(orphan_uuid.to_string());
        std::fs::create_dir_all(&dir).unwrap();
        crate::issue_file::write_issue_file(&dir.join("issue.json"), &issue).unwrap();

        hub_v3(&crosslink_dir, false, false, false, false)
            .expect("orphaned offline relic must not block");

        let source = crate::hub_source::RefHubSource::new(&cache_dir).unwrap();
        let state = crate::compaction::reduce(&source).unwrap().state;
        let minted = state
            .display_id_map
            .get(&orphan_uuid)
            .copied()
            .expect("orphan must receive a minted genesis id");
        assert!(minted > 0, "minted id must be a real positive id");
        assert_eq!(
            state.issues[&orphan_uuid].display_id,
            Some(minted),
            "CompactIssue must carry the minted id"
        );

        let mut seen = std::collections::BTreeSet::new();
        for id in state.display_id_map.values() {
            assert!(seen.insert(*id), "minted id collided: {id}");
        }
    }

    #[test]
    fn promotable_offline_issue_still_refuses_migration() {
        let (_w, _r, crosslink_dir, cache_dir) = setup_v2_hub();

        let mine_uuid = Uuid::new_v4();
        let issue = crate::issue_file::IssueFile {
            uuid: mine_uuid,
            display_id: None,
            title: "My pending offline issue".to_string(),
            description: None,
            status: crate::models::IssueStatus::Open,
            priority: crate::models::Priority::Medium,
            parent_uuid: None,
            created_by: "alpha".to_string(),
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
        };
        let dir = cache_dir.join("issues").join(mine_uuid.to_string());
        std::fs::create_dir_all(&dir).unwrap();
        crate::issue_file::write_issue_file(&dir.join("issue.json"), &issue).unwrap();

        let err = hub_v3(&crosslink_dir, false, false, false, false).unwrap_err();
        assert!(
            err.to_string().contains("created by this agent"),
            "promotable offline issue must still refuse, got: {err}"
        );
        assert!(rev(&cache_dir, CHECKPOINT_REF).is_none());
    }

    #[test]
    fn no_events_hub_migrates_with_synthesized_watermark() {
        let remote_dir = tempfile::tempdir().unwrap();
        let work_dir = tempfile::tempdir().unwrap();
        run(remote_dir.path(), &["init", "--bare", "-b", "main"]);
        let wp = work_dir.path().to_path_buf();
        run(&wp, &["init", "-b", "main"]);
        run(&wp, &["config", "user.email", "t@t.local"]);
        run(&wp, &["config", "user.name", "T"]);
        run(&wp, &["config", "commit.gpgsign", "false"]);
        run(
            &wp,
            &[
                "remote",
                "add",
                "origin",
                remote_dir.path().to_str().unwrap(),
            ],
        );
        std::fs::write(wp.join("README.md"), "# t\n").unwrap();
        run(&wp, &["add", "."]);
        run(&wp, &["commit", "-m", "init", "--no-gpg-sign"]);
        run(&wp, &["push", "-u", "origin", "main"]);

        let crosslink_dir = wp.join(".crosslink");
        std::fs::create_dir_all(&crosslink_dir).unwrap();
        std::fs::write(
            crosslink_dir.join("hook-config.json"),
            r#"{"remote":"origin","layout":"v2"}"#,
        )
        .unwrap();
        write_agent(&crosslink_dir, "alpha");
        let sync = SyncManager::new(&crosslink_dir).unwrap();
        sync.init_cache().unwrap();
        let cache_dir = sync.cache_path().to_path_buf();

        hub_v3(&crosslink_dir, false, false, false, false).unwrap();

        let cp = crate::checkpoint::read_checkpoint(&cache_dir);
        let _ = cp;
        let genesis = build_genesis_from_files(&cache_dir).unwrap();
        assert!(
            genesis.watermark.is_some(),
            "no-events genesis must have a watermark"
        );
        let reduced = crate::compaction::reduce(&RefHubSource::new(&cache_dir).unwrap())
            .unwrap()
            .state;
        assert_eq!(
            serde_json::to_value(&genesis).unwrap(),
            serde_json::to_value(&reduced).unwrap(),
            "no-events reduce must equal genesis"
        );
        assert!(reduced.issues.is_empty());
    }

    #[test]
    fn warn_detects_migrated_hub_for_v2_operation() {
        let (_w, _r, crosslink_dir, cache_dir) = setup_v2_hub();
        hub_v3(&crosslink_dir, false, false, false, false).unwrap();

        assert!(matches!(
            hub_v3::detect_hub_version(&cache_dir).unwrap(),
            HubVersion::V3 { .. }
        ));

        hub_v3::warn_if_migrated_v2_operation(&cache_dir, hub_v3::HubMode::V2);
        hub_v3::warn_if_migrated_v2_operation(&cache_dir, hub_v3::HubMode::V3);
    }

    #[test]
    fn clean_v2_clone_behind_dirty_snapshot_cannot_overwrite_generation() {
        let (_wa, remote_dir, cl_a, cache_a) = setup_v2_hub();

        let push = std::process::Command::new("git")
            .current_dir(&cache_a)
            .args(["push", "origin", "crosslink/hub"])
            .output()
            .unwrap();
        assert!(
            push.status.success(),
            "fixture must publish the v2 hub branch: {}",
            String::from_utf8_lossy(&push.stderr)
        );

        let work_b = tempfile::tempdir().unwrap();
        for args in [
            vec!["init", "-b", "main"],
            vec!["config", "user.email", "test@test.local"],
            vec!["config", "user.name", "Test"],
            vec!["config", "commit.gpgsign", "false"],
            vec![
                "remote",
                "add",
                "origin",
                remote_dir.path().to_str().unwrap(),
            ],
            vec!["fetch", "origin", "main"],
            vec!["checkout", "-b", "main", "origin/main"],
        ] {
            std::process::Command::new("git")
                .current_dir(work_b.path())
                .args(&args)
                .output()
                .unwrap();
        }
        let cl_b = work_b.path().join(".crosslink");
        std::fs::create_dir_all(&cl_b).unwrap();
        std::fs::write(cl_b.join("hook-config.json"), r#"{"remote":"origin"}"#).unwrap();
        write_agent(&cl_b, "beta");
        let sync_b = SyncManager::new(&cl_b).unwrap();
        sync_b.init_cache().unwrap();
        let cache_b = sync_b.cache_path().to_path_buf();
        let source_tip = rev(&cache_b, V2_HUB_BRANCH).unwrap();
        assert!(
            matches!(
                hub_v3::detect_hub_version(&cache_b).unwrap(),
                HubVersion::V2Only
            ),
            "B must start as a v2-only clone"
        );

        hub_v3(&cl_a, false, false, false, false).expect("A's migration must succeed");
        let remote_checkpoint_before = std::process::Command::new("git")
            .current_dir(&cache_a)
            .args(["ls-remote", "origin", "refs/heads/crosslink/checkpoint"])
            .output()
            .unwrap();
        let sha_before = String::from_utf8_lossy(&remote_checkpoint_before.stdout)
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_string();
        assert!(!sha_before.is_empty(), "remote checkpoint must exist");

        let error = hub_v3(&cl_b, false, false, false, false).unwrap_err();
        assert!(format!("{error:#}").contains("blocked_corrupt"));

        let remote_checkpoint_after = std::process::Command::new("git")
            .current_dir(&cache_a)
            .args(["ls-remote", "origin", "refs/heads/crosslink/checkpoint"])
            .output()
            .unwrap();
        let sha_after = String::from_utf8_lossy(&remote_checkpoint_after.stdout)
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_string();
        assert_eq!(
            sha_before, sha_after,
            "a source behind the published dirty snapshot must not move authority"
        );
        assert_eq!(
            rev(&cache_b, V2_HUB_BRANCH).as_deref(),
            Some(source_tip.as_str())
        );
        assert!(cl_b.join("reconciliation-journal.json").is_dir());
    }

    #[test]
    fn dirty_behind_remote_v2_merges_both_histories_losslessly() {
        let (_source_work, remote, _source_crosslink, source_cache) = setup_v2_hub();
        run(&source_cache, &["push", "origin", "crosslink/hub"]);
        let (_clone, crosslink_dir) = fresh_clone(remote.path(), "dirty-reader");
        let sync = SyncManager::new(&crosslink_dir).unwrap();
        assert_eq!(
            sync.init_cache_for_reconciliation(),
            crate::sync::ReconciliationCacheOutcome::Ready
        );
        let cache = sync.cache_path().to_path_buf();
        let base = rev(&cache, V2_HUB_BRANCH).unwrap();

        let remote_uuid = Uuid::parse_str("44444444-4444-4444-4444-444444444444").unwrap();
        write_v2_issue(&source_cache, remote_uuid, 4, "remote descendant", "alpha");
        run(&source_cache, &["add", "issues"]);
        run(
            &source_cache,
            &["commit", "-m", "remote v2 descendant", "--no-gpg-sign"],
        );
        run(&source_cache, &["push", "origin", "crosslink/hub"]);
        let remote_tip = rev(&source_cache, V2_HUB_BRANCH).unwrap();
        assert_ne!(base, remote_tip);

        let local_uuid = Uuid::parse_str("55555555-5555-5555-5555-555555555555").unwrap();
        write_v2_issue(
            &cache,
            local_uuid,
            5,
            "local dirty snapshot",
            "dirty-reader",
        );
        hub_v3(&crosslink_dir, false, false, false, false).unwrap();
        let state = compaction::reduce(&RefHubSource::new(&cache).unwrap())
            .unwrap()
            .state;
        assert!(state.issues.contains_key(&remote_uuid));
        assert!(state.issues.contains_key(&local_uuid));
        assert_eq!(state.issues.len(), 5);

        let descriptor_oid = remote_rev(&cache, GENERATION_POINTER).unwrap();
        let descriptor = Command::new("git")
            .current_dir(&cache)
            .args(["show", &format!("{descriptor_oid}:generation.json")])
            .output()
            .unwrap();
        assert!(descriptor.status.success());
        let descriptor: serde_json::Value = serde_json::from_slice(&descriptor.stdout).unwrap();
        let evidence = &descriptor["source"]["refs"][V2_HUB_BRANCH];
        assert_eq!(evidence["remote_oid"], remote_tip);
        assert_ne!(evidence["oid"], evidence["remote_oid"]);
        let archives = descriptor["archives"].as_object().unwrap();
        assert!(archives.contains_key(V2_HUB_BRANCH));
        assert!(archives.contains_key(&format!("authority:{V2_HUB_BRANCH}")));
        assert!(archives.contains_key(&format!("remote:{V2_HUB_BRANCH}")));
    }

    #[test]
    fn tracked_dirty_v2_edit_survives_remote_advance_and_reconciliation() {
        let (_source_work, remote, _source_crosslink, source_cache) = setup_v2_hub();
        run(&source_cache, &["add", "-A"]);
        run(
            &source_cache,
            &["commit", "-m", "stable tracked v2", "--no-gpg-sign"],
        );
        run(&source_cache, &["push", "origin", "crosslink/hub"]);
        let (_clone, crosslink_dir) = fresh_clone(remote.path(), "tracked-dirty");
        let sync = SyncManager::new(&crosslink_dir).unwrap();
        assert_eq!(
            sync.init_cache_for_reconciliation(),
            crate::sync::ReconciliationCacheOutcome::Ready
        );
        let cache = sync.cache_path().to_path_buf();
        let base = rev(&cache, V2_HUB_BRANCH).unwrap();
        let edited_uuid = Uuid::parse_str("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb").unwrap();
        let edited_path = cache
            .join("issues")
            .join(edited_uuid.to_string())
            .join("issue.json");
        let mut edited: crate::issue_file::IssueFile =
            serde_json::from_slice(&std::fs::read(&edited_path).unwrap()).unwrap();
        edited.title = "tracked local edit".to_string();
        crate::issue_file::write_issue_file(&edited_path, &edited).unwrap();
        let remote_uuid = Uuid::parse_str("45454545-4545-4545-4545-454545454545").unwrap();
        write_v2_issue(
            &source_cache,
            remote_uuid,
            4,
            "remote tracked peer",
            "alpha",
        );
        run(&source_cache, &["add", "issues"]);
        run(
            &source_cache,
            &["commit", "-m", "remote tracked peer", "--no-gpg-sign"],
        );
        run(&source_cache, &["push", "origin", "crosslink/hub"]);
        let remote_tip = rev(&source_cache, V2_HUB_BRANCH).unwrap();
        hub_v3(&crosslink_dir, false, false, false, false).unwrap();
        assert!(rev(&cache, V2_HUB_BRANCH).is_none());
        let retained: crate::issue_file::IssueFile =
            serde_json::from_slice(&std::fs::read(&edited_path).unwrap()).unwrap();
        assert_eq!(retained.title, "tracked local edit");
        let state = compaction::reduce(&RefHubSource::new(&cache).unwrap())
            .unwrap()
            .state;
        assert_eq!(state.issues[&edited_uuid].title, "tracked local edit");
        assert!(state.issues.contains_key(&remote_uuid));
        let descriptor_oid = remote_rev(&cache, GENERATION_POINTER).unwrap();
        let descriptor = Command::new("git")
            .current_dir(&cache)
            .args(["show", &format!("{descriptor_oid}:generation.json")])
            .output()
            .unwrap();
        let descriptor: serde_json::Value = serde_json::from_slice(&descriptor.stdout).unwrap();
        let evidence = &descriptor["source"]["refs"][V2_HUB_BRANCH];
        assert_eq!(evidence["authority_oid"], base);
        assert_eq!(evidence["remote_oid"], remote_tip);
        assert_ne!(evidence["oid"], evidence["authority_oid"]);
    }

    #[test]
    fn fresh_clone_after_retirement_verifies_current_generation() {
        let (_work, remote, crosslink_dir, cache) = setup_v2_hub();
        run(&cache, &["push", "origin", "crosslink/hub"]);
        hub_v3(&crosslink_dir, false, false, false, false).unwrap();
        let generation = remote_rev(&cache, GENERATION_POINTER).unwrap();
        let expected = compaction::reduce(&RefHubSource::new(&cache).unwrap())
            .unwrap()
            .state;
        assert!(remote_rev(&cache, V2_HUB_BRANCH).is_none());
        let (_fresh, fresh_crosslink) = fresh_clone(remote.path(), "fresh-reader");
        hub_v3(&fresh_crosslink, false, false, false, false).unwrap();
        let fresh_sync = SyncManager::new(&fresh_crosslink).unwrap();
        let fresh_cache = fresh_sync.cache_path();
        assert_eq!(
            remote_rev(fresh_cache, GENERATION_POINTER),
            Some(generation)
        );
        assert!(rev(fresh_cache, V2_HUB_BRANCH).is_none());
        let actual = compaction::reduce(&RefHubSource::new(fresh_cache).unwrap())
            .unwrap()
            .state;
        assert_eq!(
            serde_json::to_value(actual).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
    }

    #[test]
    fn late_v2_and_live_v3_progress_merge_into_verified_successor() {
        let (_work, _remote, crosslink_dir, cache) = setup_v2_hub();
        let signers = b"alpha ssh-ed25519 AAAATEST\n";
        std::fs::write(cache.join("trust").join("allowed_signers"), signers).unwrap();
        run(&cache, &["add", "trust"]);
        run(&cache, &["commit", "-m", "pin trust", "--no-gpg-sign"]);
        run(&cache, &["push", "origin", "crosslink/hub"]);
        let v2_tip = rev(&cache, V2_HUB_BRANCH).unwrap();
        hub_v3(&crosslink_dir, false, false, false, false).unwrap();
        let first_generation = remote_rev(&cache, GENERATION_POINTER).unwrap();
        let live_uuid = Uuid::parse_str("67676767-6767-6767-6767-676767676767").unwrap();
        let live_tip = append_live_issue(&cache, "live-v3", 1, live_uuid, 4, "live v3 issue");
        let live_ref = hub_v3::agent_ref_name("live-v3").unwrap();
        run(
            &cache,
            &["push", "origin", &format!("{live_tip}:{live_ref}")],
        );
        git_update_ref(&cache, V2_HUB_BRANCH, &v2_tip).unwrap();
        run(&cache, &["symbolic-ref", "HEAD", V2_HUB_BRANCH]);
        let late_uuid = Uuid::parse_str("68686868-6868-6868-6868-686868686868").unwrap();
        write_v2_issue(&cache, late_uuid, 5, "late v2 issue", "alpha");
        run(&cache, &["add", "issues"]);
        run(&cache, &["commit", "-m", "late v2 issue", "--no-gpg-sign"]);
        run(&cache, &["push", "origin", "crosslink/hub"]);
        hub_v3(&crosslink_dir, false, false, false, false).unwrap();
        let second_generation = remote_rev(&cache, GENERATION_POINTER).unwrap();
        assert_ne!(second_generation, first_generation);
        let source = RefHubSource::new(&cache).unwrap();
        let state = compaction::reduce(&source).unwrap().state;
        assert!(state.issues.contains_key(&live_uuid));
        assert!(state.issues.contains_key(&late_uuid));
        assert_eq!(state.issues.len(), 5);
        assert_eq!(state.display_id_map[&live_uuid], 4);
        assert_eq!(state.display_id_map[&late_uuid], 5);
        assert_eq!(
            std::fs::read(source.allowed_signers_file().unwrap().unwrap()).unwrap(),
            signers
        );
        assert_eq!(remote_rev(&cache, &live_ref), Some(live_tip.clone()));
        let descriptor_oid = remote_rev(&cache, GENERATION_POINTER).unwrap();
        let descriptor = Command::new("git")
            .current_dir(&cache)
            .args(["show", &format!("{descriptor_oid}:generation.json")])
            .output()
            .unwrap();
        let descriptor: serde_json::Value = serde_json::from_slice(&descriptor.stdout).unwrap();
        assert_eq!(descriptor["source"]["refs"][&live_ref]["oid"], live_tip);
        assert!(descriptor["archives"]
            .as_object()
            .unwrap()
            .contains_key(&live_ref));
        let continuity_uuid = Uuid::parse_str("69696969-6969-6969-6969-696969696969").unwrap();
        let continuity_tip = append_live_issue(
            &cache,
            "live-v3",
            2,
            continuity_uuid,
            6,
            "continued v3 issue",
        );
        run(
            &cache,
            &["merge-base", "--is-ancestor", &live_tip, &continuity_tip],
        );
        let continued = compaction::reduce(&RefHubSource::new(&cache).unwrap())
            .unwrap()
            .state;
        assert!(continued.issues.contains_key(&continuity_uuid));
        hub_v3(&crosslink_dir, false, false, false, false).unwrap();
        assert_eq!(
            remote_rev(&cache, GENERATION_POINTER),
            Some(second_generation)
        );
    }

    #[test]
    fn late_v2_scalar_update_three_way_merges_over_live_baseline() {
        let (_work, _remote, crosslink_dir, cache) = setup_v2_hub();
        run(&cache, &["push", "origin", "crosslink/hub"]);
        let v2_tip = rev(&cache, V2_HUB_BRANCH).unwrap();
        hub_v3(&crosslink_dir, false, false, false, false).unwrap();
        let first_generation = remote_rev(&cache, GENERATION_POINTER).unwrap();
        git_update_ref(&cache, V2_HUB_BRANCH, &v2_tip).unwrap();
        run(&cache, &["symbolic-ref", "HEAD", V2_HUB_BRANCH]);
        let uuid = Uuid::parse_str("a1a1a1a1-a1a1-a1a1-a1a1-a1a1a1a1a1a1").unwrap();
        let path = cache
            .join("issues")
            .join(uuid.to_string())
            .join("issue.json");
        let mut issue: crate::issue_file::IssueFile =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        issue.title = "late v2 scalar update".to_string();
        crate::issue_file::write_issue_file(&path, &issue).unwrap();
        run(&cache, &["add", "issues"]);
        run(
            &cache,
            &["commit", "-m", "late scalar update", "--no-gpg-sign"],
        );
        run(&cache, &["push", "origin", "crosslink/hub"]);
        hub_v3(&crosslink_dir, false, false, false, false).unwrap();
        let second_generation = remote_rev(&cache, GENERATION_POINTER).unwrap();
        assert_ne!(second_generation, first_generation);
        let state = compaction::reduce(&RefHubSource::new(&cache).unwrap())
            .unwrap()
            .state;
        assert_eq!(state.issues[&uuid].title, "late v2 scalar update");
    }

    #[test]
    fn dormant_dirty_clone_imports_remote_only_v2_resurrection() {
        let (_source_work, remote, source_crosslink, source_cache) = setup_v2_hub();
        run(&source_cache, &["add", "-A"]);
        run(
            &source_cache,
            &["commit", "-m", "stable dormant base", "--no-gpg-sign"],
        );
        run(&source_cache, &["push", "origin", "crosslink/hub"]);
        let (_dormant, dormant_crosslink) = fresh_clone(remote.path(), "dormant");
        let dormant_sync = SyncManager::new(&dormant_crosslink).unwrap();
        assert_eq!(
            dormant_sync.init_cache_for_reconciliation(),
            crate::sync::ReconciliationCacheOutcome::Ready
        );
        let dormant_cache = dormant_sync.cache_path().to_path_buf();
        let base = rev(&dormant_cache, V2_HUB_BRANCH).unwrap();
        let dirty_uuid = Uuid::parse_str("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb").unwrap();
        let dirty_path = dormant_cache
            .join("issues")
            .join(dirty_uuid.to_string())
            .join("issue.json");
        let mut dirty: crate::issue_file::IssueFile =
            serde_json::from_slice(&std::fs::read(&dirty_path).unwrap()).unwrap();
        dirty.title = "dormant tracked change".to_string();
        crate::issue_file::write_issue_file(&dirty_path, &dirty).unwrap();
        hub_v3(&source_crosslink, false, false, false, false).unwrap();
        let first_generation = remote_rev(&source_cache, GENERATION_POINTER).unwrap();
        assert!(remote_rev(&source_cache, V2_HUB_BRANCH).is_none());
        git_update_ref(&source_cache, V2_HUB_BRANCH, &base).unwrap();
        run(&source_cache, &["symbolic-ref", "HEAD", V2_HUB_BRANCH]);
        let remote_uuid = Uuid::parse_str("70707070-7070-7070-7070-707070707070").unwrap();
        write_v2_issue(
            &source_cache,
            remote_uuid,
            4,
            "obsolete remote advance",
            "obsolete",
        );
        run(&source_cache, &["add", "issues"]);
        run(
            &source_cache,
            &["commit", "-m", "obsolete remote advance", "--no-gpg-sign"],
        );
        run(&source_cache, &["push", "origin", "crosslink/hub"]);
        let remote_tip = rev(&source_cache, V2_HUB_BRANCH).unwrap();
        hub_v3(&dormant_crosslink, false, false, false, false).unwrap();
        let second_generation = remote_rev(&dormant_cache, GENERATION_POINTER).unwrap();
        assert_ne!(second_generation, first_generation);
        assert!(rev(&dormant_cache, V2_HUB_BRANCH).is_none());
        let state = compaction::reduce(&RefHubSource::new(&dormant_cache).unwrap())
            .unwrap()
            .state;
        assert_eq!(state.issues[&dirty_uuid].title, "dormant tracked change");
        assert!(state.issues.contains_key(&remote_uuid));
        let descriptor_oid = remote_rev(&dormant_cache, GENERATION_POINTER).unwrap();
        let descriptor = Command::new("git")
            .current_dir(&dormant_cache)
            .args(["show", &format!("{descriptor_oid}:generation.json")])
            .output()
            .unwrap();
        let descriptor: serde_json::Value = serde_json::from_slice(&descriptor.stdout).unwrap();
        assert_eq!(
            descriptor["source"]["refs"][V2_HUB_BRANCH]["remote_oid"],
            remote_tip
        );
        assert_ne!(
            descriptor["source"]["refs"][V2_HUB_BRANCH]["oid"],
            descriptor["source"]["refs"][V2_HUB_BRANCH]["authority_oid"]
        );
    }

    #[test]
    fn production_importer_accepts_normal_v3_progress_after_publication() {
        use crate::events::{Event, EventEnvelope};

        let (_work, _remote, crosslink_dir, cache) = setup_v2_hub();
        run(&cache, &["push", "origin", "crosslink/hub"]);
        hub_v3(&crosslink_dir, false, false, false, false).unwrap();
        let generation = remote_rev(&cache, GENERATION_POINTER).unwrap();
        let uuid = Uuid::parse_str("66666666-6666-6666-6666-666666666666").unwrap();
        let envelope = EventEnvelope {
            agent_id: "live-writer".to_string(),
            agent_seq: 1,
            timestamp: Utc::now() + chrono::Duration::seconds(1),
            event: Event::IssueCreated {
                uuid,
                title: "ordinary v3 write".to_string(),
                description: None,
                priority: "medium".to_string(),
                labels: vec![],
                parent_uuid: None,
                created_by: "live-writer".to_string(),
                display_id: Some(4),
                scheduled_at: None,
                due_at: None,
            },
            signed_by: None,
            signature: None,
        };
        let appended = hub_v3::append_event_to_ref(&cache, "live-writer", &envelope).unwrap();
        let agent_ref = hub_v3::agent_ref_name("live-writer").unwrap();
        run(
            &cache,
            &[
                "push",
                "origin",
                &format!("{}:{agent_ref}", appended.new_commit),
            ],
        );
        hub_v3(&crosslink_dir, false, false, false, false).unwrap();
        assert_eq!(remote_rev(&cache, GENERATION_POINTER), Some(generation));
        assert_eq!(remote_rev(&cache, &agent_ref), Some(appended.new_commit));
        let state = compaction::reduce(&RefHubSource::new(&cache).unwrap())
            .unwrap()
            .state;
        assert!(state.issues.contains_key(&uuid));
    }

    #[test]
    fn production_importer_publishes_unpushed_local_agent_descendant() {
        let (_work, _remote, crosslink_dir, cache) = setup_v2_hub();
        run(&cache, &["push", "origin", "crosslink/hub"]);
        hub_v3(&crosslink_dir, false, false, false, false).unwrap();
        let agent_ref = hub_v3::agent_ref_name("alpha").unwrap();
        let uuid = Uuid::parse_str("77777777-7777-7777-7777-777777777777").unwrap();
        let local_tip = append_live_issue(&cache, "alpha", 11, uuid, 4, "local descendant");
        let remote_before = remote_rev(&cache, &agent_ref).unwrap();
        assert_ne!(local_tip, remote_before);
        hub_v3(&crosslink_dir, false, false, false, false).unwrap();
        assert_eq!(rev(&cache, &agent_ref), Some(local_tip.clone()));
        assert_eq!(remote_rev(&cache, &agent_ref), Some(local_tip));
        let state = compaction::reduce(&RefHubSource::new(&cache).unwrap())
            .unwrap()
            .state;
        assert!(state.issues.contains_key(&uuid));
    }

    #[test]
    fn production_importer_publishes_entirely_new_local_agent_ref() {
        let (_work, _remote, crosslink_dir, cache) = setup_v2_hub();
        run(&cache, &["push", "origin", "crosslink/hub"]);
        hub_v3(&crosslink_dir, false, false, false, false).unwrap();
        let agent = "local-new-agent";
        let agent_ref = hub_v3::agent_ref_name(agent).unwrap();
        let uuid = Uuid::parse_str("88888888-8888-8888-8888-888888888888").unwrap();
        let local_tip = append_live_issue(&cache, agent, 1, uuid, 4, "new local agent");
        assert!(remote_rev(&cache, &agent_ref).is_none());
        hub_v3(&crosslink_dir, false, false, false, false).unwrap();
        assert_eq!(rev(&cache, &agent_ref), Some(local_tip.clone()));
        assert_eq!(remote_rev(&cache, &agent_ref), Some(local_tip));
        let state = compaction::reduce(&RefHubSource::new(&cache).unwrap())
            .unwrap()
            .state;
        assert!(state.issues.contains_key(&uuid));
    }

    #[test]
    fn production_importer_fast_forwards_local_agent_to_remote_descendant() {
        let (_work, _remote, crosslink_dir, cache) = setup_v2_hub();
        run(&cache, &["push", "origin", "crosslink/hub"]);
        hub_v3(&crosslink_dir, false, false, false, false).unwrap();
        let agent_ref = hub_v3::agent_ref_name("alpha").unwrap();
        let baseline = rev(&cache, &agent_ref).unwrap();
        let uuid = Uuid::parse_str("99999999-9999-9999-9999-999999999999").unwrap();
        let remote_tip = append_live_issue(&cache, "alpha", 11, uuid, 4, "remote descendant");
        run(
            &cache,
            &["push", "origin", &format!("{remote_tip}:{agent_ref}")],
        );
        git_update_ref(&cache, &agent_ref, &baseline).unwrap();
        hub_v3(&crosslink_dir, false, false, false, false).unwrap();
        assert_eq!(rev(&cache, &agent_ref), Some(remote_tip.clone()));
        assert_eq!(remote_rev(&cache, &agent_ref), Some(remote_tip));
        let state = compaction::reduce(&RefHubSource::new(&cache).unwrap())
            .unwrap()
            .state;
        assert!(state.issues.contains_key(&uuid));
    }

    #[test]
    fn production_importer_blocks_divergent_agent_refs_without_movement() {
        let (_work, _remote, crosslink_dir, cache) = setup_v2_hub();
        run(&cache, &["push", "origin", "crosslink/hub"]);
        hub_v3(&crosslink_dir, false, false, false, false).unwrap();
        let agent_ref = hub_v3::agent_ref_name("alpha").unwrap();
        let baseline = rev(&cache, &agent_ref).unwrap();
        let local_uuid = Uuid::parse_str("aaaaaaaa-7777-7777-7777-777777777777").unwrap();
        let remote_uuid = Uuid::parse_str("aaaaaaaa-8888-8888-8888-888888888888").unwrap();
        let local_tip = append_live_issue(&cache, "alpha", 11, local_uuid, 4, "local fork");
        git_update_ref(&cache, &agent_ref, &baseline).unwrap();
        let remote_tip = append_live_issue(&cache, "alpha", 11, remote_uuid, 4, "remote fork");
        run(
            &cache,
            &["push", "origin", &format!("{remote_tip}:{agent_ref}")],
        );
        git_update_ref(&cache, &agent_ref, &local_tip).unwrap();
        let error = hub_v3(&crosslink_dir, false, false, false, false).unwrap_err();
        assert!(format!("{error:#}").contains("blocked_corrupt"));
        assert_eq!(rev(&cache, &agent_ref), Some(local_tip));
        assert_eq!(remote_rev(&cache, &agent_ref), Some(remote_tip));
        let state = compaction::reduce(&RefHubSource::new(&cache).unwrap())
            .unwrap()
            .state;
        assert!(state.issues.contains_key(&local_uuid));
        assert!(!state.issues.contains_key(&remote_uuid));
    }

    #[test]
    fn production_importer_blocks_corrupt_journal_and_descriptor() {
        let (_work, _remote, crosslink_dir) = setup_absent_hub();
        drop(crate::db::Database::open(&crosslink_dir.join("issues.db")).unwrap());
        let journal = crosslink_dir.join("reconciliation-journal.json");
        std::fs::create_dir_all(&journal).unwrap();
        std::fs::write(journal.join("00000000000000000001.json"), b"{not-json").unwrap();
        let error = hub_v3(&crosslink_dir, false, false, false, false).unwrap_err();
        assert!(format!("{error:#}").contains("blocked_corrupt"));
        std::fs::remove_dir_all(&journal).unwrap();
        hub_v3(&crosslink_dir, false, false, false, false).unwrap();
        let sync = SyncManager::new(&crosslink_dir).unwrap();
        let cache = sync.cache_path();
        let corrupt = hub_v3::commit_blob_to_ref(
            cache,
            "refs/crosslink/test-corrupt-descriptor",
            "generation.json",
            br#"{"protocol_version":1,"targets":{"refs/heads/main":{}}}"#,
            "corrupt descriptor",
        )
        .unwrap();
        run(
            cache,
            &[
                "push",
                "--force",
                "origin",
                &format!("{corrupt}:{GENERATION_POINTER}"),
            ],
        );
        let error = hub_v3(&crosslink_dir, false, false, false, false).unwrap_err();
        assert!(format!("{error:#}").contains("blocked_corrupt"));
    }

    #[test]
    fn production_importer_never_overwrites_mismatched_immutable_ref() {
        let (_work, _remote, crosslink_dir, cache) = setup_v2_hub();
        run(&cache, &["push", "origin", "crosslink/hub"]);
        hub_v3(&crosslink_dir, false, false, false, false).unwrap();
        let descriptor_oid = remote_rev(&cache, GENERATION_POINTER).unwrap();
        let descriptor = Command::new("git")
            .current_dir(&cache)
            .args(["show", &format!("{descriptor_oid}:generation.json")])
            .output()
            .unwrap();
        assert!(descriptor.status.success());
        let descriptor: serde_json::Value = serde_json::from_slice(&descriptor.stdout).unwrap();
        let immutable = descriptor["targets"][CHECKPOINT_REF]["immutable_ref"]
            .as_str()
            .unwrap();
        let tree = git_with_input(&cache, &["mktree"], &[]).unwrap();
        let corrupt = commit_snapshot_tree(&cache, tree.trim(), None).unwrap();
        run(
            &cache,
            &[
                "push",
                "--force",
                "origin",
                &format!("{corrupt}:{immutable}"),
            ],
        );
        let error = hub_v3(&crosslink_dir, false, false, false, false).unwrap_err();
        assert!(format!("{error:#}").contains("blocked_corrupt"));
        assert_eq!(remote_rev(&cache, immutable), Some(corrupt));
        assert_eq!(remote_rev(&cache, GENERATION_POINTER), Some(descriptor_oid));
    }

    #[test]
    fn production_importer_fallback_two_clone_race_has_one_verified_adopter() {
        let (_source_work, remote, source_crosslink, source_cache) = setup_v2_hub();
        run(&source_cache, &["add", "-A"]);
        run(
            &source_cache,
            &["commit", "-m", "stable v2 source", "--no-gpg-sign"],
        );
        run(&source_cache, &["push", "origin", "crosslink/hub"]);
        run(
            remote.path(),
            &["config", "receive.advertiseAtomic", "false"],
        );
        let (_second_work, second_crosslink) = fresh_clone(remote.path(), "alpha");
        let source_sync = SyncManager::new(&source_crosslink).unwrap();
        let second_sync = SyncManager::new(&second_crosslink).unwrap();
        assert_eq!(
            source_sync.init_cache_for_reconciliation(),
            crate::sync::ReconciliationCacheOutcome::Ready
        );
        assert_eq!(
            second_sync.init_cache_for_reconciliation(),
            crate::sync::ReconciliationCacheOutcome::Ready
        );
        let source_cache = source_sync.cache_path().to_path_buf();
        let second_cache = second_sync.cache_path().to_path_buf();
        let source_lock = source_sync.acquire_lock().unwrap();
        let second_lock = second_sync.acquire_lock().unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let fingerprints = Arc::new(Mutex::new(Vec::new()));
        let source_importer = PreparedBarrierImporter {
            inner: MigrationImporter {
                crosslink_dir: &source_crosslink,
                cache_dir: &source_cache,
                hub_lock: &source_lock,
                agent_id: "alpha".to_string(),
            },
            barrier: Arc::clone(&barrier),
            fingerprints: Arc::clone(&fingerprints),
        };
        let second_importer = PreparedBarrierImporter {
            inner: MigrationImporter {
                crosslink_dir: &second_crosslink,
                cache_dir: &second_cache,
                hub_lock: &second_lock,
                agent_id: "alpha".to_string(),
            },
            barrier,
            fingerprints: Arc::clone(&fingerprints),
        };
        let source_format = crate::reconcile::check_repository(&source_crosslink).format;
        let second_format = crate::reconcile::check_repository(&second_crosslink).format;
        let source_journal = source_crosslink.join("reconciliation-journal.json");
        let second_journal = second_crosslink.join("reconciliation-journal.json");
        let (source_outcome, second_outcome) = std::thread::scope(|scope| {
            let source_handle = scope.spawn(|| {
                RepositoryReconciler::new(&source_cache, source_journal, "origin", &source_importer)
                    .reconcile(source_format)
                    .unwrap()
            });
            let second_handle = scope.spawn(|| {
                RepositoryReconciler::new(&second_cache, second_journal, "origin", &second_importer)
                    .reconcile(second_format)
                    .unwrap()
            });
            (source_handle.join().unwrap(), second_handle.join().unwrap())
        });
        let mut published = None;
        let mut adopted = None;
        let outcomes: [PublicationOutcome; 2] = (source_outcome, second_outcome).into();
        for outcome in outcomes {
            match outcome {
                PublicationOutcome::Published {
                    generation_id,
                    atomic: false,
                } => assert!(published.replace(generation_id).is_none()),
                PublicationOutcome::Adopted { generation_id } => {
                    assert!(adopted.replace(generation_id).is_none());
                }
                outcome => panic!("unexpected production fallback race outcome: {outcome:?}"),
            }
        }
        let generation = published.unwrap();
        assert_eq!(adopted.as_deref(), Some(generation.as_str()));
        let fingerprints = fingerprints.lock().unwrap();
        assert_eq!(fingerprints.len(), 2);
        assert_eq!(fingerprints[0], fingerprints[1]);
        assert_eq!(
            remote_rev(&source_cache, GENERATION_POINTER),
            Some(remote_rev(&second_cache, GENERATION_POINTER).unwrap())
        );
    }

    #[cfg(unix)]
    #[test]
    fn production_ready_observer_completes_fallback_pointer_window() {
        use std::os::unix::fs::PermissionsExt as _;

        let (_source_work, remote, source_crosslink, source_cache) = setup_v2_hub();
        run(&source_cache, &["add", "-A"]);
        run(
            &source_cache,
            &["commit", "-m", "stable v2 source", "--no-gpg-sign"],
        );
        run(&source_cache, &["push", "origin", "crosslink/hub"]);
        run(
            remote.path(),
            &["config", "receive.advertiseAtomic", "false"],
        );
        let (_observer_work, observer_crosslink) = fresh_clone(remote.path(), "observer");
        let control = tempfile::tempdir().unwrap();
        let marker = control.path().join("pointer-committed");
        let release = control.path().join("release");
        let hook = remote.path().join("hooks").join("post-receive");
        fs::write(
            &hook,
            format!(
                "#!/bin/sh\nwhile read old new ref; do\nif [ \"$ref\" = \"{GENERATION_POINTER}\" ]; then\n: > \"{}\"\nwhile [ ! -f \"{}\" ]; do sleep 0.01; done\nfi\ndone\n",
                marker.display(),
                release.display()
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&hook).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&hook, permissions).unwrap();

        std::thread::scope(|scope| {
            let publisher =
                scope.spawn(|| hub_v3(&source_crosslink, false, false, false, false).unwrap());
            let mut observed = false;
            for _ in 0..500 {
                if marker.exists() {
                    observed = true;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            if !observed {
                fs::write(&release, []).unwrap();
                publisher.join().unwrap();
                panic!("fallback pointer hook was not reached");
            }
            let observer_had_no_journal = !observer_crosslink
                .join("reconciliation-journal.json")
                .exists();
            let observer = hub_v3(&observer_crosslink, false, false, false, false);
            fs::write(&release, []).unwrap();
            publisher.join().unwrap();
            assert!(observer_had_no_journal);
            observer.unwrap();
        });
        let source_sync = SyncManager::new(&source_crosslink).unwrap();
        assert!(remote_rev(source_sync.cache_path(), CHECKPOINT_REF).is_some());
        assert!(remote_rev(source_sync.cache_path(), META_REF).is_some());
        assert!(remote_rev(source_sync.cache_path(), V2_HUB_BRANCH).is_none());
    }

    #[test]
    fn legacy_cli_flags_have_explicit_compatibility_behavior() {
        validate_compatibility_flags(false, false, false, false).unwrap();
        validate_compatibility_flags(true, false, false, false).unwrap();
        validate_compatibility_flags(true, true, false, false).unwrap();
        let delete_without_finalize =
            validate_compatibility_flags(false, true, false, false).unwrap_err();
        assert!(delete_without_finalize
            .to_string()
            .contains("--yes-delete-v2 requires --finalize"));
        let stale = validate_compatibility_flags(false, false, true, false).unwrap_err();
        assert!(stale
            .to_string()
            .contains("--adopt-stale is no longer supported"));
        let remigrate = validate_compatibility_flags(false, false, false, true).unwrap_err();
        assert!(remigrate
            .to_string()
            .contains("automatic reconciliation detects and incorporates late legacy history"));
    }

    fn projection_fixture(crosslink_dir: &Path) -> crate::db::Database {
        fs::create_dir_all(crosslink_dir).unwrap();
        write_agent(crosslink_dir, "projection-test");
        crate::db::Database::open(&crosslink_dir.join("issues.db")).unwrap()
    }

    fn authority_state(issue_uuid: Uuid) -> CheckpointState {
        let now = Utc::now();
        let comment_uuid = Uuid::new_v4();
        let mut comments = BTreeMap::new();
        comments.insert(
            comment_uuid,
            CompactComment {
                display_id: Some(1),
                author: "authority".to_string(),
                content: "authority comment".to_string(),
                created_at: now,
                kind: "note".to_string(),
                trigger_type: None,
                intervention_context: None,
                driver_key_fingerprint: None,
                signed_by: None,
                signature: None,
            },
        );
        let issue = CompactIssue {
            uuid: issue_uuid,
            display_id: Some(1),
            title: "authority issue".to_string(),
            description: Some("canonical".to_string()),
            status: crate::models::IssueStatus::Open,
            priority: crate::models::Priority::High,
            parent_uuid: None,
            created_by: "authority".to_string(),
            created_at: now,
            updated_at: now,
            closed_at: None,
            scheduled_at: None,
            due_at: None,
            labels: BTreeSet::from(["authority-label".to_string()]),
            blockers: BTreeSet::new(),
            related: BTreeSet::new(),
            milestone_uuid: None,
            comments,
            time_entries: BTreeMap::new(),
        };
        CheckpointState {
            next_display_id: 2,
            next_comment_id: 2,
            display_id_map: BTreeMap::from([(issue_uuid, 1)]),
            issues: BTreeMap::from([(issue_uuid, issue)]),
            ..CheckpointState::default()
        }
    }

    #[test]
    fn projection_install_failures_leave_live_bytes_and_schema_unchanged() {
        for during_install in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let crosslink_dir = root.path().join(".crosslink");
            let db = projection_fixture(&crosslink_dir);
            db.create_issue("local draft", None, "medium").unwrap();
            let schema = db.get_schema_version().unwrap();
            drop(db);
            let path = crosslink_dir.join("issues.db");
            let before = fs::read(&path).unwrap();
            let result = if during_install {
                rebuild_projection_with_checks(
                    &crosslink_dir,
                    &CheckpointState::default(),
                    || Ok(()),
                    |_| bail!("injected install failure"),
                )
            } else {
                rebuild_projection_with_checks(
                    &crosslink_dir,
                    &CheckpointState::default(),
                    || bail!("injected pre-install failure"),
                    |_| Ok(()),
                )
            };
            assert!(result.is_err());
            assert_eq!(fs::read(&path).unwrap(), before);
            let reopened = crate::db::Database::open(&path).unwrap();
            assert_eq!(reopened.get_schema_version().unwrap(), schema);
            assert_eq!(
                reopened.list_issues(Some("all"), None, None).unwrap().len(),
                1
            );
        }
    }

    #[test]
    fn old_schema_projection_failure_preserves_original_and_exact_backup() {
        let root = tempfile::tempdir().unwrap();
        let crosslink_dir = root.path().join(".crosslink");
        let db = projection_fixture(&crosslink_dir);
        drop(db);
        let path = crosslink_dir.join("issues.db");
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "PRAGMA foreign_keys = OFF;
                 DROP INDEX IF EXISTS idx_token_usage_provider;
                 CREATE TABLE token_usage_v17 (
                     id INTEGER PRIMARY KEY AUTOINCREMENT,
                     agent_id TEXT NOT NULL,
                     session_id INTEGER,
                     timestamp TEXT NOT NULL,
                     input_tokens INTEGER NOT NULL DEFAULT 0,
                     output_tokens INTEGER NOT NULL DEFAULT 0,
                     cache_read_tokens INTEGER,
                     cache_creation_tokens INTEGER,
                     model TEXT NOT NULL DEFAULT 'unknown',
                     cost_estimate REAL,
                     FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE SET NULL
                 );
                 INSERT INTO token_usage_v17 (
                     id, agent_id, session_id, timestamp, input_tokens, output_tokens,
                     cache_read_tokens, cache_creation_tokens, model, cost_estimate
                 ) SELECT id, agent_id, session_id, timestamp, input_tokens, output_tokens,
                     cache_read_tokens, cache_creation_tokens, model, cost_estimate FROM token_usage;
                 DROP TABLE token_usage;
                 ALTER TABLE token_usage_v17 RENAME TO token_usage;
                 CREATE INDEX idx_token_usage_agent ON token_usage(agent_id);
                 CREATE INDEX idx_token_usage_session ON token_usage(session_id);
                 CREATE INDEX idx_token_usage_timestamp ON token_usage(timestamp);
                 PRAGMA user_version = 17;
                 VACUUM;",
            )
            .unwrap();
        drop(connection);
        let before = fs::read(&path).unwrap();
        let error = rebuild_projection_with_checks(
            &crosslink_dir,
            &CheckpointState::default(),
            || Ok(()),
            |_| bail!("injected old-schema install failure"),
        )
        .unwrap_err();
        assert!(error.to_string().contains("installing verified projection"));
        assert_eq!(fs::read(&path).unwrap(), before);
        let read_only = crate::db::Database::open_read_only(&path).unwrap();
        assert_eq!(read_only.get_schema_version().unwrap(), 17);
        drop(read_only);
        let integrity = crosslink_dir.join(crate::db::snapshot::SNAPSHOT_DIR);
        let mut backups = fs::read_dir(integrity)
            .unwrap()
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.path())
            .filter(|candidate| {
                candidate
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| {
                        name.starts_with("reconciliation-before-") && name.ends_with(".sqlite")
                    })
            })
            .collect::<Vec<_>>();
        backups.sort();
        assert_eq!(fs::read(backups.last().unwrap()).unwrap(), before);
    }

    #[test]
    fn projection_evidence_prunes_complete_groups() {
        let directory = tempfile::tempdir().unwrap();
        for id in 0..18 {
            for name in [
                format!("reconciliation-before-{id:02}.sqlite"),
                format!("reconciliation-before-{id:02}.sqlite-wal"),
                format!("reconciliation-before-{id:02}.sqlite-shm"),
                format!("reconciliation-shadow-{id:02}.sqlite"),
                format!("reconciliation-install-{id:02}.sqlite"),
            ] {
                fs::write(directory.path().join(name), id.to_string()).unwrap();
            }
        }
        prune_projection_evidence_to(directory.path(), 16).unwrap();
        let mut retained: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for entry in fs::read_dir(directory.path()).unwrap() {
            let name = entry.unwrap().file_name().to_string_lossy().to_string();
            let id = name
                .split('-')
                .nth(2)
                .unwrap()
                .split('.')
                .next()
                .unwrap()
                .to_string();
            retained.entry(id).or_default().insert(name);
        }
        assert_eq!(retained.len(), 16);
        assert!(retained.values().all(|paths| paths.len() == 5));
    }

    #[test]
    fn projection_rebuild_reserves_evidence_retention_for_the_new_attempt() {
        let root = tempfile::tempdir().unwrap();
        let crosslink_dir = root.path().join(".crosslink");
        let db = projection_fixture(&crosslink_dir);
        drop(db);
        let integrity = crosslink_dir.join(crate::db::snapshot::SNAPSHOT_DIR);
        fs::create_dir_all(&integrity).unwrap();
        for id in 0..16 {
            for name in [
                format!("reconciliation-before-{id:02}.sqlite"),
                format!("reconciliation-shadow-{id:02}.sqlite"),
            ] {
                fs::write(integrity.join(name), id.to_string()).unwrap();
            }
        }
        let result = rebuild_projection_with_checks(
            &crosslink_dir,
            &CheckpointState::default(),
            || Ok(()),
            |_| bail!("retain failed attempt"),
        );
        assert!(result.is_err());
        let mut groups = BTreeMap::<String, BTreeSet<String>>::new();
        for entry in fs::read_dir(&integrity).unwrap() {
            let name = entry.unwrap().file_name().to_string_lossy().to_string();
            let Some(id) = [
                "reconciliation-before-",
                "reconciliation-shadow-",
                "reconciliation-install-",
            ]
            .into_iter()
            .find_map(|prefix| name.strip_prefix(prefix))
            .and_then(|value| value.split_once(".sqlite").map(|(id, _)| id.to_string())) else {
                continue;
            };
            groups.entry(id).or_default().insert(name);
        }
        assert_eq!(groups.len(), 16);
        assert!(!groups.contains_key("00"));
        assert!(groups.values().any(|paths| {
            paths
                .iter()
                .any(|path| path.starts_with("reconciliation-before-"))
                && paths
                    .iter()
                    .any(|path| path.starts_with("reconciliation-shadow-"))
        }));
    }

    #[test]
    fn projection_install_preserves_offline_issue_session_timer_and_sentinel_links() {
        let root = tempfile::tempdir().unwrap();
        let crosslink_dir = root.path().join(".crosslink");
        let db = projection_fixture(&crosslink_dir);
        let local_uuid = Uuid::new_v4();
        let local_id = db.create_issue("offline issue", None, "medium").unwrap();
        db.conn
            .execute(
                "UPDATE issues SET uuid = ?1, created_by = 'offline-agent' WHERE id = ?2",
                rusqlite::params![local_uuid.to_string(), local_id],
            )
            .unwrap();
        let session_id = db.start_session_with_agent(Some("offline-agent")).unwrap();
        db.set_session_issue(session_id, local_id).unwrap();
        db.start_timer(local_id).unwrap();
        db.insert_sentinel_run("projection-run", "test").unwrap();
        let dispatch_id = db
            .insert_sentinel_dispatch(&crate::db::sentinel::NewDispatch {
                run_id: "projection-run",
                signal_ref: "signal",
                signal_title: "signal title",
                source: "test",
                disposition: "dispatch",
                agent_id: Some("offline-agent"),
                crosslink_issue_id: Some(local_id),
                gh_issue_number: None,
                label: "test",
                attempt_number: 1,
                model_used: None,
            })
            .unwrap();
        drop(db);

        let authority_uuid = Uuid::new_v4();
        rebuild_projection(&crosslink_dir, &authority_state(authority_uuid)).unwrap();
        let db = crate::db::Database::open(&crosslink_dir.join("issues.db")).unwrap();
        let remapped_local_id: i64 = db
            .conn
            .query_row(
                "SELECT id FROM issues WHERE uuid = ?1",
                [local_uuid.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert!(remapped_local_id < 0);
        assert_eq!(
            db.get_current_session_for_agent(Some("offline-agent"))
                .unwrap()
                .unwrap()
                .active_issue_id,
            Some(remapped_local_id)
        );
        assert_eq!(db.get_active_timer().unwrap().unwrap().0, remapped_local_id);
        let dispatch_issue: Option<i64> = db
            .conn
            .query_row(
                "SELECT crosslink_issue_id FROM sentinel_dispatches WHERE id = ?1",
                [dispatch_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(dispatch_issue, Some(remapped_local_id));
        assert_eq!(
            db.conn
                .query_row("SELECT uuid FROM issues WHERE id = 1", [], |row| row
                    .get::<_, String>(0),)
                .unwrap(),
            authority_uuid.to_string()
        );
    }

    #[test]
    fn projection_verification_rejects_missing_authority_label_and_comment() {
        for table in ["labels", "comments"] {
            let root = tempfile::tempdir().unwrap();
            let crosslink_dir = root.path().join(".crosslink");
            let db = projection_fixture(&crosslink_dir);
            drop(db);
            let state = authority_state(Uuid::new_v4());
            rebuild_projection(&crosslink_dir, &state).unwrap();
            let db = crate::db::Database::open(&crosslink_dir.join("issues.db")).unwrap();
            db.conn
                .execute(&format!("DELETE FROM {table}"), [])
                .unwrap();
            assert!(verify_projection_database(&crosslink_dir, &db, &state).is_err());
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_projection_and_wal_manifest() {
        local_only_database_is_archived_and_imported_without_mutation();
        current_v3_merges_committed_wal_authority_without_mutating_local_files();
        projection_install_failures_leave_live_bytes_and_schema_unchanged();
    }
}
