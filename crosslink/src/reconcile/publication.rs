use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::{error::Error, fmt};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{LocalDatabaseFormat, RepositoryFormat, SharedStoreFormat};

pub(crate) const GENERATION_REF: &str = "refs/heads/crosslink/reconciliation/current";
const GENERATION_ROOT: &str = "refs/heads/crosslink/reconciliation/generations";
const ARCHIVE_ROOT: &str = "refs/heads/crosslink/reconciliation/archives";
const LOCAL_ROOT: &str = "refs/crosslink/reconciliation";
const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RefEvidence {
    authority_oid: String,
    oid: String,
    tree_oid: String,
    remote_oid: Option<String>,
    remote_tree_oid: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceEvidence {
    format: RepositoryFormat,
    refs: BTreeMap<String, RefEvidence>,
    local_fingerprint: Option<String>,
    fingerprint: String,
}

impl RefEvidence {
    pub fn authority_oid(&self) -> &str {
        &self.authority_oid
    }

    pub fn oid(&self) -> &str {
        &self.oid
    }

    pub fn tree_oid(&self) -> &str {
        &self.tree_oid
    }

    pub fn remote_oid(&self) -> Option<&str> {
        self.remote_oid.as_deref()
    }

    pub fn remote_tree_oid(&self) -> Option<&str> {
        self.remote_tree_oid.as_deref()
    }
}

impl SourceEvidence {
    pub fn format(&self) -> &RepositoryFormat {
        &self.format
    }

    pub fn refs(&self) -> &BTreeMap<String, RefEvidence> {
        &self.refs
    }

    pub fn local_fingerprint(&self) -> Option<&str> {
        self.local_fingerprint.as_deref()
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TargetRef {
    pub(crate) oid: String,
    pub(crate) immutable_ref: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GenerationDescriptor {
    pub(crate) protocol_version: u32,
    pub(crate) generation_id: String,
    pub(crate) source: SourceEvidence,
    pub(crate) semantic_digest: String,
    pub(crate) targets: BTreeMap<String, TargetRef>,
    pub(crate) archives: BTreeMap<String, TargetRef>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalSemantic {
    value: Value,
    digest: String,
}

impl CanonicalSemantic {
    pub fn from_value(value: Value) -> Result<Self> {
        let bytes = serde_json::to_vec(&value).context("serializing canonical semantic state")?;
        Ok(Self {
            value,
            digest: hex::encode(Sha256::digest(bytes)),
        })
    }

    pub fn digest(&self) -> &str {
        &self.digest
    }

    pub fn value(&self) -> &Value {
        &self.value
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedImport {
    targets: BTreeMap<String, String>,
    semantic: CanonicalSemantic,
}

impl PreparedImport {
    pub fn new(targets: BTreeMap<String, String>, semantic: CanonicalSemantic) -> Self {
        Self { targets, semantic }
    }
}

pub trait HistoricalImporter {
    fn stabilize_source(&self, _repository: &Path) -> Result<()> {
        Ok(())
    }

    fn snapshot_source_refs(
        &self,
        _repository: &Path,
        _source: &SourceEvidence,
    ) -> Result<BTreeMap<String, String>> {
        Ok(BTreeMap::new())
    }

    fn prepare_file_source(
        &self,
        repository: &Path,
        source: &SourceEvidence,
        generation_id: &str,
    ) -> Result<PreparedImport>;

    fn prepare_local_source(
        &self,
        repository: &Path,
        source: &SourceEvidence,
        generation_id: &str,
    ) -> Result<PreparedImport>;

    fn prepare_current_source(
        &self,
        repository: &Path,
        source: &SourceEvidence,
    ) -> Result<PreparedImport>;

    fn file_source_is_newer(&self, repository: &Path, source: &SourceEvidence) -> Result<bool>;

    fn read_target_semantic(
        &self,
        repository: &Path,
        targets: &BTreeMap<String, String>,
    ) -> Result<CanonicalSemantic>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum JournalStage {
    IntentRecorded,
    Prepared,
    Verified,
    ArchivesPublished,
    AuthorityCommitted,
    AliasesMaterialized,
    Adopted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReconciliationJournal {
    pub(crate) descriptor: GenerationDescriptor,
    pub(crate) descriptor_oid: String,
    pub(crate) stage: JournalStage,
    pub(crate) atomic_publication: Option<bool>,
    pub(crate) alias_expectations: BTreeMap<String, Option<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct JournalIntent {
    source: SourceEvidence,
    provisional_id: String,
    canonical_refs: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(tag = "record", rename_all = "snake_case")]
enum JournalRecord {
    Intent(JournalIntent),
    Generation(ReconciliationJournal),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublicationOutcome {
    ReadyCurrent { generation_id: String },
    Published { generation_id: String, atomic: bool },
    Adopted { generation_id: String },
    WaitingForRemote { reason: String },
    BlockedCorrupt { reason: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Transition {
    Journal,
    Prepare,
    Verify,
    Archive,
    Publish,
    Adopt,
    Alias,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TransitionPosition {
    Before,
    After,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Failpoint {
    pub(crate) transition: Transition,
    pub(crate) position: TransitionPosition,
    pub(crate) occurrence: usize,
}

#[derive(Debug, Default)]
struct FailureController {
    failpoint: Option<Failpoint>,
    occurrences: BTreeMap<(u8, u8), usize>,
}

impl FailureController {
    #[cfg(test)]
    fn new(failpoint: Option<Failpoint>) -> Self {
        Self {
            failpoint,
            occurrences: BTreeMap::new(),
        }
    }

    fn hit(&mut self, transition: Transition, position: TransitionPosition) -> Result<()> {
        let key = (transition as u8, position as u8);
        let occurrence = self.occurrences.entry(key).or_default();
        *occurrence += 1;
        if self.failpoint.is_some_and(|failpoint| {
            failpoint.transition == transition
                && failpoint.position == position
                && failpoint.occurrence == *occurrence
        }) {
            bail!("injected reconciliation failure at {transition:?} {position:?} occurrence {occurrence}");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AtomicCapability {
    Auto,
    UnsupportedForTest,
}

#[derive(Debug)]
pub struct RepositoryReconciler<'a, I> {
    repository: &'a Path,
    journal_path: PathBuf,
    remote: &'a str,
    importer: &'a I,
    atomic_capability: AtomicCapability,
    failure: FailureController,
}

impl<'a, I: HistoricalImporter> RepositoryReconciler<'a, I> {
    pub fn new(
        repository: &'a Path,
        journal_path: PathBuf,
        remote: &'a str,
        importer: &'a I,
    ) -> Self {
        Self {
            repository,
            journal_path,
            remote,
            importer,
            atomic_capability: AtomicCapability::Auto,
            failure: FailureController::default(),
        }
    }

    #[cfg(test)]
    fn with_atomic_capability(mut self, capability: AtomicCapability) -> Self {
        self.atomic_capability = capability;
        self
    }

    #[cfg(test)]
    fn with_failpoint(mut self, failpoint: Failpoint) -> Self {
        self.failure = FailureController::new(Some(failpoint));
        self
    }

    pub fn reconcile(&mut self, format: RepositoryFormat) -> Result<PublicationOutcome> {
        match self.reconcile_inner(format) {
            Err(error) => match error.downcast_ref::<RemoteGitError>() {
                Some(RemoteGitError::Unavailable(reason)) => {
                    Ok(PublicationOutcome::WaitingForRemote {
                        reason: reason.clone(),
                    })
                }
                Some(RemoteGitError::Rejected(reason)) => Ok(PublicationOutcome::BlockedCorrupt {
                    reason: reason.clone(),
                }),
                None => Err(error),
            },
            result => result,
        }
    }

    fn reconcile_inner(&mut self, format: RepositoryFormat) -> Result<PublicationOutcome> {
        if let SharedStoreFormat::Unreadable { reason } = &format.shared_store {
            return Ok(PublicationOutcome::BlockedCorrupt {
                reason: format!("historical shared store is unreadable: {reason}"),
            });
        }
        if matches!(format.shared_store, SharedStoreFormat::Absent) {
            match &format.local_database {
                LocalDatabaseFormat::Future {
                    version,
                    supported_version,
                    ..
                } => {
                    return Ok(PublicationOutcome::BlockedCorrupt {
                        reason: format!(
                            "local database schema {version} is newer than supported schema {supported_version}"
                        ),
                    });
                }
                LocalDatabaseFormat::Unreadable { reason } => {
                    return Ok(PublicationOutcome::BlockedCorrupt {
                        reason: format!("local database is unreadable: {reason}"),
                    });
                }
                LocalDatabaseFormat::Missing | LocalDatabaseFormat::Sqlite { .. } => {}
            }
        }
        let journal_record = match read_journal(self.repository, &self.journal_path) {
            Ok(record) => record,
            Err(error) => {
                return Ok(PublicationOutcome::BlockedCorrupt {
                    reason: format!("reconciliation journal is corrupt: {error:#}"),
                });
            }
        };
        if let Some(record) = journal_record {
            return match record {
                JournalRecord::Intent(intent) => self.prepare_from_intent(intent, &format),
                JournalRecord::Generation(journal) => self.resume(journal, &format),
            };
        }

        match self.ready_current(&format) {
            Ok(Some(outcome)) => return Ok(outcome),
            Ok(None) => {}
            Err(error) if error.downcast_ref::<RemoteGitError>().is_some() => return Err(error),
            Err(error) => {
                return Ok(PublicationOutcome::BlockedCorrupt {
                    reason: format!("existing remote generation is unverifiable: {error:#}"),
                });
            }
        }

        self.importer
            .stabilize_source(self.repository)
            .context("stabilizing the historical source before pinning")?;

        let source = self.pin_source(format.clone())?;
        let canonical_refs =
            remote_ref_map(self.repository, self.remote, &canonical_ref_patterns())?;
        validate_observed_canonical_refs(self.repository, self.remote, &canonical_refs)?;
        let provisional_id = source
            .fingerprint
            .get(..24)
            .unwrap_or(&source.fingerprint)
            .to_string();
        let intent = JournalIntent {
            source,
            provisional_id,
            canonical_refs,
        };
        materialize_intent_evidence(self.repository, &intent)?;
        self.persist_record(&JournalRecord::Intent(intent.clone()))?;
        self.prepare_from_intent(intent, &format)
    }

    fn prepare_from_intent(
        &mut self,
        intent: JournalIntent,
        observed_format: &RepositoryFormat,
    ) -> Result<PublicationOutcome> {
        let current = self.pin_source(intent.source.format.clone())?;
        if current.fingerprint != intent.source.fingerprint
            || has_unrecorded_legacy_source(&intent.source, observed_format)
        {
            return Ok(PublicationOutcome::BlockedCorrupt {
                reason: "historical source changed after reconciliation intent was recorded"
                    .to_string(),
            });
        }
        let authority_before = local_authority_refs(self.repository)?;
        self.failure
            .hit(Transition::Prepare, TransitionPosition::Before)?;
        let prepared = match self.prepare_source(&intent.source, &intent.provisional_id) {
            Ok(prepared) => prepared,
            Err(error) => {
                ensure_authority_unchanged(self.repository, &authority_before)?;
                return Ok(PublicationOutcome::BlockedCorrupt {
                    reason: format!(
                        "historical source could not be imported without loss: {error:#}"
                    ),
                });
            }
        };
        self.failure
            .hit(Transition::Prepare, TransitionPosition::After)?;
        ensure_authority_unchanged(self.repository, &authority_before)?;

        let generation_id = generation_id(
            &intent.source,
            prepared.semantic.digest(),
            &prepared.targets,
        );
        let targets = target_refs(&generation_id, &prepared.targets);
        let archives = archive_refs(&generation_id, &intent.source);
        let descriptor = GenerationDescriptor {
            protocol_version: PROTOCOL_VERSION,
            generation_id,
            source: intent.source,
            semantic_digest: prepared.semantic.digest().to_string(),
            targets,
            archives,
        };
        validate_descriptor_schema(&descriptor)?;
        validate_descriptor_objects(self.repository, &descriptor)?;
        let descriptor_oid = write_descriptor(self.repository, &descriptor)?;
        let alias_expectations = descriptor
            .targets
            .keys()
            .map(|reference| {
                (
                    reference.clone(),
                    intent.canonical_refs.get(reference).cloned(),
                )
            })
            .collect();
        let mut journal = ReconciliationJournal {
            descriptor,
            descriptor_oid,
            stage: JournalStage::IntentRecorded,
            atomic_publication: None,
            alias_expectations,
        };
        materialize_local_evidence(
            self.repository,
            &journal.descriptor,
            &journal.descriptor_oid,
        )?;
        self.persist(&journal)?;
        journal.stage = JournalStage::Prepared;
        self.persist(&journal)?;

        self.failure
            .hit(Transition::Verify, TransitionPosition::Before)?;
        self.verify_prepared(&journal, &prepared.semantic)?;
        self.failure
            .hit(Transition::Verify, TransitionPosition::After)?;
        ensure_authority_unchanged(self.repository, &authority_before)?;
        journal.stage = JournalStage::Verified;
        self.persist(&journal)?;
        self.publish(journal)
    }

    fn prepare_source(
        &self,
        source: &SourceEvidence,
        generation_id: &str,
    ) -> Result<PreparedImport> {
        match &source.format.shared_store {
            SharedStoreFormat::VisibleV3 { .. } | SharedStoreFormat::HiddenV3 { .. } => self
                .importer
                .prepare_current_source(self.repository, source),
            SharedStoreFormat::Mixed { .. }
                if self
                    .importer
                    .file_source_is_newer(self.repository, source)? =>
            {
                self.importer
                    .prepare_file_source(self.repository, source, generation_id)
            }
            SharedStoreFormat::Mixed { .. } => self
                .importer
                .prepare_current_source(self.repository, source),
            SharedStoreFormat::V2 { .. } | SharedStoreFormat::LegacyLocks { .. } => self
                .importer
                .prepare_file_source(self.repository, source, generation_id),
            SharedStoreFormat::Absent => {
                self.importer
                    .prepare_local_source(self.repository, source, generation_id)
            }
            SharedStoreFormat::Unreadable { reason } => {
                bail!("the historical shared store is unreadable: {reason}")
            }
        }
    }

    fn resume(
        &mut self,
        journal: ReconciliationJournal,
        observed_format: &RepositoryFormat,
    ) -> Result<PublicationOutcome> {
        if !self.source_still_pinned(&journal.descriptor.source, observed_format)? {
            return Ok(PublicationOutcome::BlockedCorrupt {
                reason: "historical source changed after reconciliation was prepared".to_string(),
            });
        }
        let current = remote_ref_map(self.repository, self.remote, &[GENERATION_REF.to_string()])?;
        if let Some(remote_oid) = current.get(GENERATION_REF) {
            if remote_oid == &journal.descriptor_oid {
                let fallback = journal.atomic_publication == Some(false);
                return self.resume_after_commit(journal, fallback);
            }
            return self.publish(journal);
        }
        match journal.stage {
            JournalStage::IntentRecorded | JournalStage::Prepared => {
                let targets = descriptor_target_oids(&journal.descriptor);
                let target_semantic = self
                    .importer
                    .read_target_semantic(self.repository, &targets)
                    .context("verifying prepared targets while resuming")?;
                if target_semantic.digest() != journal.descriptor.semantic_digest {
                    return Ok(PublicationOutcome::BlockedCorrupt {
                        reason: "prepared targets no longer match the journal semantic digest"
                            .to_string(),
                    });
                }
                let mut resumed = journal;
                resumed.stage = JournalStage::Verified;
                self.persist(&resumed)?;
                self.publish(resumed)
            }
            JournalStage::Verified | JournalStage::ArchivesPublished => self.publish(journal),
            JournalStage::AuthorityCommitted
            | JournalStage::AliasesMaterialized
            | JournalStage::Adopted => Ok(PublicationOutcome::BlockedCorrupt {
                reason: "journal records committed authority but the remote generation pointer is absent"
                    .to_string(),
            }),
        }
    }

    fn verify_prepared(
        &self,
        journal: &ReconciliationJournal,
        source_semantic: &CanonicalSemantic,
    ) -> Result<()> {
        let target_semantic = self.importer.read_target_semantic(
            self.repository,
            &descriptor_target_oids(&journal.descriptor),
        )?;
        anyhow::ensure!(
            target_semantic.value() == source_semantic.value(),
            "prepared target semantics differ from the pinned historical source"
        );
        anyhow::ensure!(
            target_semantic.digest() == journal.descriptor.semantic_digest,
            "prepared target digest differs from the generation descriptor"
        );
        Ok(())
    }

    fn publish(&mut self, mut journal: ReconciliationJournal) -> Result<PublicationOutcome> {
        for _ in 0..8 {
            match self.publish_attempt(journal.clone()) {
                Err(error) if error.downcast_ref::<PublicationPointerAdvanced>().is_some() => {}
                result => return result,
            }
            let JournalRecord::Generation(current) =
                read_journal(self.repository, &self.journal_path)?
                    .ok_or_else(|| anyhow::anyhow!("publication retry lost its durable journal"))?
            else {
                bail!("publication retry found an intent-only journal")
            };
            journal = current;
        }
        Ok(PublicationOutcome::WaitingForRemote {
            reason: "remote generation pointer kept advancing during publication".to_string(),
        })
    }

    fn publish_attempt(
        &mut self,
        mut journal: ReconciliationJournal,
    ) -> Result<PublicationOutcome> {
        validate_descriptor_schema(&journal.descriptor)?;
        validate_descriptor_objects(self.repository, &journal.descriptor)?;
        self.failure
            .hit(Transition::Publish, TransitionPosition::Before)?;
        let mut remote_refs = remote_ref_map(
            self.repository,
            self.remote,
            &publication_ref_patterns(&journal.descriptor),
        )?;
        if let Err(error) = ensure_immutable_refs_compatible(&journal, &remote_refs) {
            return Ok(PublicationOutcome::BlockedCorrupt {
                reason: format!("immutable reconciliation evidence conflicts: {error:#}"),
            });
        }
        if let Some(winner) = remote_refs.get(GENERATION_REF) {
            if winner == &journal.descriptor_oid {
                let fallback = journal.atomic_publication == Some(false);
                return self.resume_after_commit(journal, fallback);
            }
            fetch_oid(self.repository, self.remote, winner)?;
            let committed = read_descriptor(self.repository, winner)?;
            if committed.source.fingerprint == journal.descriptor.source.fingerprint {
                return self.adopt_winner(journal, winner);
            }
            if let Err(error) = self.verified_baseline_semantic(&committed) {
                if error.downcast_ref::<RemoteGitError>().is_some() {
                    return Err(error);
                }
                return Ok(PublicationOutcome::BlockedCorrupt {
                    reason: format!("committed remote generation is unverifiable: {error:#}"),
                });
            }
            let expectations = alias_expectations_from_source(&committed);
            match self.complete_committed_generation(&committed, winner, &expectations, true)? {
                AliasConvergence::Converged => {}
                AliasConvergence::Waiting(reason) => {
                    return Ok(PublicationOutcome::WaitingForRemote { reason });
                }
                AliasConvergence::Blocked(reason) => {
                    return Ok(PublicationOutcome::BlockedCorrupt { reason });
                }
                AliasConvergence::PointerChanged(_) => {
                    return Err(PublicationPointerAdvanced.into());
                }
            }
            if !self.can_supersede(winner, &journal.descriptor)? {
                return self.adopt_winner(journal, winner);
            }
        }
        if let Err(error) = ensure_source_expectations(&journal.descriptor.source, &remote_refs) {
            return Ok(PublicationOutcome::BlockedCorrupt {
                reason: format!("pinned historical authority changed: {error:#}"),
            });
        }
        if journal.alias_expectations.is_empty() {
            journal.alias_expectations = alias_expectations_from_source(&journal.descriptor);
            self.persist(&journal)?;
        }
        let mut alias_plan =
            match plan_alias_publication(self.repository, self.remote, &journal, &remote_refs) {
                Ok(plan) => plan,
                Err(error) => {
                    return Ok(PublicationOutcome::BlockedCorrupt {
                        reason: format!("canonical refs changed before publication: {error:#}"),
                    });
                }
            };

        let atomic = if self.atomic_capability == AtomicCapability::UnsupportedForTest {
            AtomicAttempt::Unsupported
        } else {
            let mut attempt = 0;
            loop {
                let result = self.atomic_push(&journal, &remote_refs, &alias_plan)?;
                if result != AtomicAttempt::Race {
                    break result;
                }
                if remote_ref_oid(self.repository, self.remote, GENERATION_REF)?.is_some() {
                    break result;
                }
                attempt += 1;
                if attempt == 8 {
                    break AtomicAttempt::Waiting(
                        "canonical refs kept advancing while atomic publication was retried"
                            .to_string(),
                    );
                }
                remote_refs = remote_ref_map(
                    self.repository,
                    self.remote,
                    &publication_ref_patterns(&journal.descriptor),
                )?;
                if let Err(error) = ensure_immutable_refs_compatible(&journal, &remote_refs) {
                    return Ok(PublicationOutcome::BlockedCorrupt {
                        reason: format!("immutable reconciliation evidence conflicts: {error:#}"),
                    });
                }
                if let Err(error) =
                    ensure_source_expectations(&journal.descriptor.source, &remote_refs)
                {
                    return Ok(PublicationOutcome::BlockedCorrupt {
                        reason: format!("pinned historical authority changed: {error:#}"),
                    });
                }
                alias_plan = match plan_alias_publication(
                    self.repository,
                    self.remote,
                    &journal,
                    &remote_refs,
                ) {
                    Ok(plan) => plan,
                    Err(error) => {
                        return Ok(PublicationOutcome::BlockedCorrupt {
                            reason: format!("canonical refs changed before publication: {error:#}"),
                        });
                    }
                };
            }
        };
        match atomic {
            AtomicAttempt::Published => {
                journal.atomic_publication = Some(true);
                self.failure
                    .hit(Transition::Publish, TransitionPosition::After)?;
                match self.complete_committed_generation(
                    &journal.descriptor,
                    &journal.descriptor_oid,
                    &journal.alias_expectations,
                    false,
                )? {
                    AliasConvergence::Converged => {}
                    AliasConvergence::Waiting(reason) => {
                        return Ok(PublicationOutcome::WaitingForRemote { reason });
                    }
                    AliasConvergence::Blocked(reason) => {
                        return Ok(PublicationOutcome::BlockedCorrupt { reason });
                    }
                    AliasConvergence::PointerChanged(actual) => {
                        return self.adopt_winner(journal, &actual);
                    }
                }
                journal.stage = JournalStage::AliasesMaterialized;
                self.persist(&journal)?;
                remove_journal(&self.journal_path)?;
                Ok(PublicationOutcome::Published {
                    generation_id: journal.descriptor.generation_id,
                    atomic: true,
                })
            }
            AtomicAttempt::Race => {
                match remote_ref_oid(self.repository, self.remote, GENERATION_REF)? {
                    Some(winner) => self.adopt_winner(journal, &winner),
                    None => Ok(PublicationOutcome::BlockedCorrupt {
                        reason: "atomic publication lost a source or alias lease without a committed generation; all pinned evidence remains preserved".to_string(),
                    }),
                }
            }
            AtomicAttempt::Waiting(reason) => Ok(PublicationOutcome::WaitingForRemote { reason }),
            AtomicAttempt::Rejected(reason) => {
                if let Some(winner) = remote_ref_oid(self.repository, self.remote, GENERATION_REF)?
                {
                    self.adopt_winner(journal, &winner)
                } else {
                    Ok(PublicationOutcome::BlockedCorrupt { reason })
                }
            }
            AtomicAttempt::Unsupported => self.publish_fallback(journal, remote_refs),
        }
    }

    fn atomic_push(
        &self,
        journal: &ReconciliationJournal,
        remote_refs: &BTreeMap<String, String>,
        alias_plan: &BTreeMap<String, String>,
    ) -> Result<AtomicAttempt> {
        validate_descriptor_schema(&journal.descriptor)?;
        validate_descriptor_objects(self.repository, &journal.descriptor)?;
        let mut args = vec!["push".to_string(), "--atomic".to_string()];
        append_push_leases(journal, remote_refs, &mut args, alias_plan);
        args.push(self.remote.to_string());
        append_push_refspecs(journal, remote_refs, &mut args, alias_plan);
        let output = run_git_raw(self.repository, &args)?;
        Ok(classify_atomic_push(&output))
    }

    fn publish_fallback(
        &mut self,
        mut journal: ReconciliationJournal,
        remote_refs: BTreeMap<String, String>,
    ) -> Result<PublicationOutcome> {
        for target in journal
            .descriptor
            .targets
            .values()
            .chain(journal.descriptor.archives.values())
        {
            self.failure
                .hit(Transition::Archive, TransitionPosition::Before)?;
            match push_oid_if_absent(
                self.repository,
                self.remote,
                &target.oid,
                &target.immutable_ref,
            )? {
                SinglePush::Published | SinglePush::AlreadyPresent => {}
                SinglePush::Race(reason) | SinglePush::Rejected(reason) => {
                    return Ok(PublicationOutcome::BlockedCorrupt { reason });
                }
                SinglePush::Waiting(reason) => {
                    return Ok(PublicationOutcome::WaitingForRemote { reason });
                }
            }
            self.failure
                .hit(Transition::Archive, TransitionPosition::After)?;
        }
        let descriptor_ref = descriptor_immutable_ref(&journal.descriptor.generation_id);
        self.failure
            .hit(Transition::Archive, TransitionPosition::Before)?;
        match push_oid_if_absent(
            self.repository,
            self.remote,
            &journal.descriptor_oid,
            &descriptor_ref,
        )? {
            SinglePush::Published | SinglePush::AlreadyPresent => {}
            SinglePush::Race(reason) | SinglePush::Rejected(reason) => {
                return Ok(PublicationOutcome::BlockedCorrupt { reason });
            }
            SinglePush::Waiting(reason) => {
                return Ok(PublicationOutcome::WaitingForRemote { reason });
            }
        }
        self.failure
            .hit(Transition::Archive, TransitionPosition::After)?;
        journal.stage = JournalStage::ArchivesPublished;
        journal.atomic_publication = Some(false);
        self.persist(&journal)?;

        let mut commit_patterns = retired_source_refs(&journal.descriptor.source)
            .into_keys()
            .collect::<Vec<_>>();
        commit_patterns.push(GENERATION_REF.to_string());
        let immediately_before = remote_ref_map(self.repository, self.remote, &commit_patterns)?;
        if let Some(winner) = immediately_before.get(GENERATION_REF) {
            return self.adopt_winner(journal, winner);
        }
        if let Err(error) =
            ensure_source_expectations(&journal.descriptor.source, &immediately_before)
        {
            return Ok(PublicationOutcome::BlockedCorrupt {
                reason: format!(
                    "historical authority advanced before the fallback commit point: {error:#}"
                ),
            });
        }
        let expected = remote_refs.get(GENERATION_REF).map(String::as_str);
        match push_oid_with_lease(
            self.repository,
            self.remote,
            &journal.descriptor_oid,
            GENERATION_REF,
            expected,
        )? {
            SinglePush::Published => {
                journal.stage = JournalStage::AuthorityCommitted;
                self.persist(&journal)?;
                self.failure
                    .hit(Transition::Publish, TransitionPosition::After)?;
                let source_refs = retired_source_refs(&journal.descriptor.source);
                if !source_refs.is_empty() {
                    let observed = remote_ref_map(
                        self.repository,
                        self.remote,
                        &source_refs.keys().cloned().collect::<Vec<_>>(),
                    )?;
                    if let Err(error) = ensure_source_not_advanced_after_commit(
                        &journal.descriptor.source,
                        &observed,
                    ) {
                        return Ok(PublicationOutcome::BlockedCorrupt {
                            reason: format!(
                                "historical authority advanced across the fallback commit point: {error:#}"
                            ),
                        });
                    }
                }
                self.resume_after_commit(journal, true)
            }
            SinglePush::AlreadyPresent => self.resume_after_commit(journal, true),
            SinglePush::Race(_) => {
                let winner = remote_ref_oid(self.repository, self.remote, GENERATION_REF)?
                    .ok_or_else(|| {
                        anyhow::anyhow!("generation CAS lost but no winner is visible")
                    })?;
                self.adopt_winner(journal, &winner)
            }
            SinglePush::Waiting(reason) => Ok(PublicationOutcome::WaitingForRemote { reason }),
            SinglePush::Rejected(reason) => Ok(PublicationOutcome::BlockedCorrupt { reason }),
        }
    }

    fn resume_after_commit(
        &mut self,
        mut journal: ReconciliationJournal,
        fallback: bool,
    ) -> Result<PublicationOutcome> {
        match self.complete_committed_generation(
            &journal.descriptor,
            &journal.descriptor_oid,
            &journal.alias_expectations,
            false,
        )? {
            AliasConvergence::Converged => {}
            AliasConvergence::Waiting(reason) => {
                return Ok(PublicationOutcome::WaitingForRemote { reason });
            }
            AliasConvergence::Blocked(reason) => {
                return Ok(PublicationOutcome::BlockedCorrupt { reason });
            }
            AliasConvergence::PointerChanged(actual) => {
                return self.adopt_winner(journal, &actual);
            }
        }
        journal.stage = JournalStage::AliasesMaterialized;
        self.persist(&journal)?;
        remove_journal(&self.journal_path)?;
        Ok(PublicationOutcome::Published {
            generation_id: journal.descriptor.generation_id,
            atomic: !fallback,
        })
    }

    fn complete_committed_generation(
        &mut self,
        descriptor: &GenerationDescriptor,
        descriptor_oid: &str,
        alias_expectations: &BTreeMap<String, Option<String>>,
        preserve_advanced_sources: bool,
    ) -> Result<AliasConvergence> {
        validate_alias_expectations(descriptor, alias_expectations)?;
        if let Err(error) = ensure_generation_pointer(self.repository, self.remote, descriptor_oid)
        {
            if let Some(outcome) = pointer_change_outcome(&error) {
                return Ok(outcome);
            }
            return Err(error);
        }
        for (reference, expected) in retired_source_refs(&descriptor.source) {
            let mut completed = false;
            for _ in 0..8 {
                if let Err(error) =
                    ensure_generation_pointer(self.repository, self.remote, descriptor_oid)
                {
                    if let Some(outcome) = pointer_change_outcome(&error) {
                        return Ok(outcome);
                    }
                    return Err(error);
                }
                match delete_ref_with_lease(self.repository, self.remote, &reference, &expected)? {
                    SinglePush::Published | SinglePush::AlreadyPresent => {
                        completed = true;
                        break;
                    }
                    SinglePush::Waiting(reason) => {
                        return Ok(AliasConvergence::Waiting(reason));
                    }
                    SinglePush::Rejected(reason) => {
                        return Ok(AliasConvergence::Blocked(reason));
                    }
                    SinglePush::Race(_) => {
                        match remote_ref_oid(self.repository, self.remote, &reference)? {
                            None => {
                                completed = true;
                                break;
                            }
                            Some(actual) if actual == expected => {}
                            Some(actual)
                                if preserve_advanced_sources
                                    && is_ancestor(self.repository, &expected, &actual)? =>
                            {
                                completed = true;
                                break;
                            }
                            Some(actual) => {
                                return Ok(AliasConvergence::Blocked(format!(
                                    "historical source {reference} advanced from {expected} to {actual} after cutover"
                                )));
                            }
                        }
                    }
                }
            }
            if !completed {
                return Ok(AliasConvergence::Blocked(format!(
                    "historical source {reference} kept changing during retirement"
                )));
            }
        }
        for (canonical, target) in &descriptor.targets {
            self.failure
                .hit(Transition::Alias, TransitionPosition::Before)?;
            let expected = alias_expectations
                .get(canonical)
                .ok_or_else(|| anyhow::anyhow!("missing alias expectation for {canonical}"))?;
            let mut completed = false;
            for _ in 0..8 {
                if let Err(error) =
                    ensure_generation_pointer(self.repository, self.remote, descriptor_oid)
                {
                    if let Some(outcome) = pointer_change_outcome(&error) {
                        return Ok(outcome);
                    }
                    return Err(error);
                }
                let remote_tip = remote_ref_oid(self.repository, self.remote, canonical)?;
                if remote_tip.as_deref() == Some(target.oid.as_str()) {
                    completed = true;
                    break;
                }
                if remote_tip.as_ref() != expected.as_ref() {
                    if let Some(remote_tip) = &remote_tip {
                        fetch_ref(self.repository, self.remote, canonical)?;
                        if is_ancestor(self.repository, &target.oid, remote_tip)? {
                            completed = true;
                            break;
                        }
                    }
                    return Ok(AliasConvergence::Blocked(format!(
                        "canonical ref {canonical} changed after the authority commit and cannot be safely preserved"
                    )));
                }
                match push_oid_with_lease(
                    self.repository,
                    self.remote,
                    &target.oid,
                    canonical,
                    expected.as_deref(),
                )? {
                    SinglePush::Published | SinglePush::AlreadyPresent => {
                        completed = true;
                        break;
                    }
                    SinglePush::Waiting(reason) => {
                        return Ok(AliasConvergence::Waiting(reason));
                    }
                    SinglePush::Rejected(reason) => {
                        return Ok(AliasConvergence::Blocked(reason));
                    }
                    SinglePush::Race(_) => {}
                }
            }
            if !completed {
                return Ok(AliasConvergence::Blocked(format!(
                    "canonical ref {canonical} kept changing during compatibility materialization"
                )));
            }
            self.failure
                .hit(Transition::Alias, TransitionPosition::After)?;
        }
        let live = match self.verify_remote_targets(
            descriptor,
            descriptor_oid,
            !preserve_advanced_sources,
        ) {
            Ok(live) => live,
            Err(error) if pointer_change_outcome(&error).is_some() => {
                return Ok(pointer_change_outcome(&error).unwrap());
            }
            Err(error) if error.downcast_ref::<RemoteGitError>().is_some() => return Err(error),
            Err(error) => {
                return Ok(AliasConvergence::Blocked(format!(
                    "committed generation did not converge: {error:#}"
                )));
            }
        };
        self.converge_live_aliases(
            descriptor,
            descriptor_oid,
            !preserve_advanced_sources,
            live,
            Some(alias_expectations),
        )
    }

    fn adopt_winner(
        &mut self,
        mut journal: ReconciliationJournal,
        winner_oid: &str,
    ) -> Result<PublicationOutcome> {
        self.failure
            .hit(Transition::Adopt, TransitionPosition::Before)?;
        let mut candidate_oid = winner_oid.to_string();
        for _ in 0..8 {
            fetch_oid(self.repository, self.remote, &candidate_oid)?;
            let winner = read_descriptor(self.repository, &candidate_oid)?;
            if winner.protocol_version != PROTOCOL_VERSION {
                return Ok(PublicationOutcome::BlockedCorrupt {
                    reason: format!(
                        "remote generation protocol {} is unsupported",
                        winner.protocol_version
                    ),
                });
            }
            if winner.source.fingerprint != journal.descriptor.source.fingerprint {
                return Ok(PublicationOutcome::BlockedCorrupt {
                    reason: format!(
                        "remote winner was built from different pinned source evidence (winner {}, local {})",
                        winner.source.fingerprint, journal.descriptor.source.fingerprint
                    ),
                });
            }
            let semantic = match self.verified_baseline_semantic(&winner) {
                Ok(semantic) => semantic,
                Err(error) if error.downcast_ref::<RemoteGitError>().is_some() => {
                    return Err(error);
                }
                Err(error) => {
                    return Ok(PublicationOutcome::BlockedCorrupt {
                        reason: format!("remote winner target evidence is corrupt: {error:#}"),
                    });
                }
            };
            if semantic.digest() != winner.semantic_digest
                || semantic.digest() != journal.descriptor.semantic_digest
            {
                return Ok(PublicationOutcome::BlockedCorrupt {
                    reason: format!(
                        "remote winner failed independent semantic verification (winner {}, local {}, verified {})",
                        winner.semantic_digest,
                        journal.descriptor.semantic_digest,
                        semantic.digest()
                    ),
                });
            }
            let expectations = alias_expectations_from_source(&winner);
            match self.complete_committed_generation(
                &winner,
                &candidate_oid,
                &expectations,
                false,
            )? {
                AliasConvergence::Converged => {}
                AliasConvergence::Waiting(reason) => {
                    return Ok(PublicationOutcome::WaitingForRemote { reason });
                }
                AliasConvergence::Blocked(reason) => {
                    return Ok(PublicationOutcome::BlockedCorrupt { reason });
                }
                AliasConvergence::PointerChanged(actual) => {
                    candidate_oid = actual;
                    continue;
                }
            }
            self.failure
                .hit(Transition::Adopt, TransitionPosition::After)?;
            journal.descriptor = winner;
            journal.descriptor_oid = candidate_oid;
            journal.stage = JournalStage::Adopted;
            self.persist(&journal)?;
            let generation_id = journal.descriptor.generation_id.clone();
            remove_journal(&self.journal_path)?;
            return Ok(PublicationOutcome::Adopted { generation_id });
        }
        Ok(PublicationOutcome::WaitingForRemote {
            reason: "remote generation pointer kept advancing during adoption".to_string(),
        })
    }

    fn ready_current(
        &mut self,
        observed_format: &RepositoryFormat,
    ) -> Result<Option<PublicationOutcome>> {
        let mut descriptor_oid = match remote_ref_oid(self.repository, self.remote, GENERATION_REF)?
        {
            Some(oid) => oid,
            None => return Ok(None),
        };
        for _ in 0..8 {
            fetch_oid(self.repository, self.remote, &descriptor_oid)?;
            let descriptor = read_descriptor(self.repository, &descriptor_oid)?;
            anyhow::ensure!(
                descriptor.protocol_version == PROTOCOL_VERSION,
                "remote generation protocol {} is unsupported",
                descriptor.protocol_version
            );
            if !self.source_still_pinned(&descriptor.source, observed_format)? {
                return Ok(None);
            }
            self.verified_baseline_semantic(&descriptor)?;
            let expectations = alias_expectations_from_source(&descriptor);
            match self.complete_committed_generation(
                &descriptor,
                &descriptor_oid,
                &expectations,
                false,
            )? {
                AliasConvergence::Converged => {
                    return Ok(Some(PublicationOutcome::ReadyCurrent {
                        generation_id: descriptor.generation_id,
                    }));
                }
                AliasConvergence::Waiting(reason) => {
                    return Ok(Some(PublicationOutcome::WaitingForRemote { reason }));
                }
                AliasConvergence::Blocked(reason) => {
                    return Ok(Some(PublicationOutcome::BlockedCorrupt { reason }));
                }
                AliasConvergence::PointerChanged(actual) => descriptor_oid = actual,
            }
        }
        Ok(Some(PublicationOutcome::WaitingForRemote {
            reason: "remote generation pointer kept advancing during verification".to_string(),
        }))
    }

    fn can_supersede(&self, winner_oid: &str, proposed: &GenerationDescriptor) -> Result<bool> {
        fetch_oid(self.repository, self.remote, winner_oid)?;
        let winner = read_descriptor(self.repository, winner_oid)?;
        let live = self.verify_remote_targets(&winner, winner_oid, false)?;
        for (canonical, oid) in &live {
            if local_ref_oid(self.repository, canonical)?.as_deref() != Some(oid) {
                return Ok(false);
            }
        }
        let winner_semantic = self.importer.read_target_semantic(self.repository, &live)?;
        let proposed_semantic = self
            .importer
            .read_target_semantic(self.repository, &descriptor_target_oids(proposed))?;
        let source_binds_live = source_binds_live_semantic(&proposed.source, &live);
        if !semantic_preserves_identities(
            winner_semantic.value(),
            proposed_semantic.value(),
            source_binds_live,
        ) {
            return Ok(false);
        }
        let baseline = live.iter().all(|(canonical, oid)| {
            winner
                .targets
                .get(canonical)
                .is_some_and(|target| target.oid == *oid)
        });
        if baseline && winner_semantic.digest() != winner.semantic_digest {
            return Ok(false);
        }
        let mut advanced = false;
        for (name, winner_evidence) in &winner.source.refs {
            if name != "refs/heads/crosslink/hub" && name != "refs/heads/crosslink/locks" {
                continue;
            }
            let Some(proposed_evidence) = proposed.source.refs.get(name) else {
                return Ok(false);
            };
            if proposed_evidence.authority_oid != winner_evidence.authority_oid {
                if !is_ancestor(
                    self.repository,
                    &winner_evidence.authority_oid,
                    &proposed_evidence.authority_oid,
                )? {
                    return Ok(false);
                }
                advanced = true;
            }
            if proposed_evidence.oid != winner_evidence.oid {
                if proposed_evidence.oid != proposed_evidence.authority_oid
                    && !is_ancestor(
                        self.repository,
                        &proposed_evidence.authority_oid,
                        &proposed_evidence.oid,
                    )?
                {
                    return Ok(false);
                }
                advanced = true;
            }
            match (&winner_evidence.remote_oid, &proposed_evidence.remote_oid) {
                (Some(winner_remote), Some(proposed_remote))
                    if winner_remote != proposed_remote =>
                {
                    if !is_ancestor(self.repository, winner_remote, proposed_remote)? {
                        return Ok(false);
                    }
                    advanced = true;
                }
                (None, Some(_)) => advanced = true,
                (Some(_), None) => {}
                (Some(_), Some(_)) | (None, None) => {}
            }
        }
        for (name, proposed_evidence) in &proposed.source.refs {
            if (name == "refs/heads/crosslink/hub" || name == "refs/heads/crosslink/locks")
                && !winner.source.refs.contains_key(name)
            {
                if !is_ancestor(
                    self.repository,
                    &proposed_evidence.authority_oid,
                    &proposed_evidence.oid,
                )? {
                    return Ok(false);
                }
                advanced = true;
            }
        }
        if proposed.source.refs.contains_key("local/issues.db")
            && winner.source.fingerprint != proposed.source.fingerprint
        {
            advanced = true;
        }
        Ok(advanced)
    }

    fn converge_live_aliases(
        &self,
        descriptor: &GenerationDescriptor,
        descriptor_oid: &str,
        require_retired_sources: bool,
        mut remote_live: BTreeMap<String, String>,
        safe_replacements: Option<&BTreeMap<String, Option<String>>>,
    ) -> Result<AliasConvergence> {
        validate_descriptor_schema(descriptor)?;
        validate_descriptor_objects(self.repository, descriptor)?;
        for _ in 0..8 {
            if let Err(error) =
                ensure_generation_pointer(self.repository, self.remote, descriptor_oid)
            {
                if let Some(outcome) = pointer_change_outcome(&error) {
                    return Ok(outcome);
                }
                return Err(error);
            }
            let local = local_canonical_refs(self.repository)?;
            let names = local
                .keys()
                .chain(remote_live.keys())
                .cloned()
                .collect::<BTreeSet<_>>();
            let mut candidate = remote_live.clone();
            let mut remote_updates = Vec::new();
            let mut local_updates = Vec::new();
            for canonical in names {
                validate_canonical_ref(&canonical)?;
                match (local.get(&canonical), remote_live.get(&canonical)) {
                    (Some(local_oid), Some(remote_oid)) if local_oid == remote_oid => {}
                    (Some(local_oid), Some(remote_oid)) => {
                        validate_object_type(self.repository, local_oid, "commit")?;
                        validate_object_type(self.repository, remote_oid, "commit")?;
                        if is_ancestor(self.repository, local_oid, remote_oid)? {
                            local_updates.push((
                                canonical,
                                remote_oid.clone(),
                                Some(local_oid.clone()),
                            ));
                        } else if is_ancestor(self.repository, remote_oid, local_oid)? {
                            candidate.insert(canonical.clone(), local_oid.clone());
                            remote_updates.push((
                                canonical,
                                local_oid.clone(),
                                Some(remote_oid.clone()),
                            ));
                        } else if safe_replacements
                            .and_then(|expectations| expectations.get(&canonical))
                            .and_then(Option::as_deref)
                            == Some(local_oid.as_str())
                            || descriptor
                                .source
                                .refs
                                .get(&canonical)
                                .is_some_and(|evidence| evidence.oid == *local_oid)
                        {
                            local_updates.push((
                                canonical,
                                remote_oid.clone(),
                                Some(local_oid.clone()),
                            ));
                        } else {
                            return Ok(AliasConvergence::Blocked(format!(
                                "canonical ref {canonical} diverged locally at {local_oid} and remotely at {remote_oid}"
                            )));
                        }
                    }
                    (Some(local_oid), None) => {
                        if !canonical.starts_with(crate::hub_v3::AGENT_REF_PREFIX) {
                            return Ok(AliasConvergence::Blocked(format!(
                                "required canonical ref {canonical} exists only locally at {local_oid}"
                            )));
                        }
                        validate_object_type(self.repository, local_oid, "commit")?;
                        candidate.insert(canonical.clone(), local_oid.clone());
                        remote_updates.push((canonical, local_oid.clone(), None));
                    }
                    (None, Some(remote_oid)) => {
                        validate_object_type(self.repository, remote_oid, "commit")?;
                        local_updates.push((canonical, remote_oid.clone(), None));
                    }
                    (None, None) => {}
                }
            }
            if remote_updates.is_empty() && local_updates.is_empty() {
                return Ok(AliasConvergence::Converged);
            }
            if let Err(error) = self
                .importer
                .read_target_semantic(self.repository, &candidate)
            {
                return Ok(AliasConvergence::Blocked(format!(
                    "combined live canonical state is unsafe: {error:#}"
                )));
            }
            if !remote_updates.is_empty() {
                for (canonical, oid, expected) in remote_updates {
                    if let Err(error) =
                        ensure_generation_pointer(self.repository, self.remote, descriptor_oid)
                    {
                        if let Some(outcome) = pointer_change_outcome(&error) {
                            return Ok(outcome);
                        }
                        return Err(error);
                    }
                    match push_oid_with_lease(
                        self.repository,
                        self.remote,
                        &oid,
                        &canonical,
                        expected.as_deref(),
                    )? {
                        SinglePush::Published
                        | SinglePush::AlreadyPresent
                        | SinglePush::Race(_) => {}
                        SinglePush::Waiting(reason) => {
                            return Ok(AliasConvergence::Waiting(reason));
                        }
                        SinglePush::Rejected(reason) => {
                            return Ok(AliasConvergence::Blocked(reason));
                        }
                    }
                }
                remote_live = match self.verify_remote_targets(
                    descriptor,
                    descriptor_oid,
                    require_retired_sources,
                ) {
                    Ok(live) => live,
                    Err(error) => {
                        if let Some(outcome) = pointer_change_outcome(&error) {
                            return Ok(outcome);
                        }
                        return Err(error);
                    }
                };
                continue;
            }
            let refreshed = match self.verify_remote_targets(
                descriptor,
                descriptor_oid,
                require_retired_sources,
            ) {
                Ok(live) => live,
                Err(error) => {
                    if let Some(outcome) = pointer_change_outcome(&error) {
                        return Ok(outcome);
                    }
                    return Err(error);
                }
            };
            if refreshed != remote_live {
                remote_live = refreshed;
                continue;
            }
            for (canonical, oid, expected) in local_updates {
                if let Err(error) =
                    ensure_generation_pointer(self.repository, self.remote, descriptor_oid)
                {
                    if let Some(outcome) = pointer_change_outcome(&error) {
                        return Ok(outcome);
                    }
                    return Err(error);
                }
                if !update_ref_cas(self.repository, &canonical, &oid, expected.as_deref())? {
                    let actual = local_ref_oid(self.repository, &canonical)?;
                    return Ok(AliasConvergence::Blocked(format!(
                        "canonical ref {canonical} changed locally during convergence from {expected:?} to {actual:?}"
                    )));
                }
            }
            remote_live = match self.verify_remote_targets(
                descriptor,
                descriptor_oid,
                require_retired_sources,
            ) {
                Ok(live) => live,
                Err(error) => {
                    if let Some(outcome) = pointer_change_outcome(&error) {
                        return Ok(outcome);
                    }
                    return Err(error);
                }
            };
        }
        Ok(AliasConvergence::Blocked(
            "canonical refs kept changing during live convergence".to_string(),
        ))
    }

    fn fetch_and_verify_target_objects(
        &self,
        descriptor: &GenerationDescriptor,
    ) -> Result<BTreeMap<String, String>> {
        validate_descriptor_schema(descriptor)?;
        let mut targets = BTreeMap::new();
        for (canonical, target) in &descriptor.targets {
            fetch_ref(self.repository, self.remote, &target.immutable_ref)?;
            anyhow::ensure!(
                object_exists(self.repository, &target.oid)?,
                "remote target object {} is unavailable after fetching {}",
                target.oid,
                target.immutable_ref
            );
            validate_object_type(self.repository, &target.oid, "commit")?;
            let advertised = remote_ref_oid(self.repository, self.remote, &target.immutable_ref)?;
            anyhow::ensure!(
                advertised.as_deref() == Some(target.oid.as_str()),
                "remote immutable target {} does not match its descriptor",
                target.immutable_ref
            );
            targets.insert(canonical.clone(), target.oid.clone());
        }
        Ok(targets)
    }

    fn verified_baseline_semantic(
        &self,
        descriptor: &GenerationDescriptor,
    ) -> Result<CanonicalSemantic> {
        let targets = self.fetch_and_verify_target_objects(descriptor)?;
        for archive in descriptor.archives.values() {
            fetch_ref(self.repository, self.remote, &archive.immutable_ref)?;
            let advertised = remote_ref_oid(self.repository, self.remote, &archive.immutable_ref)?;
            anyhow::ensure!(
                advertised.as_deref() == Some(archive.oid.as_str()),
                "remote immutable archive {} does not match its descriptor",
                archive.immutable_ref
            );
            validate_object_type(self.repository, &archive.oid, "commit")?;
        }
        validate_descriptor_objects(self.repository, descriptor)?;
        let semantic = self
            .importer
            .read_target_semantic(self.repository, &targets)?;
        anyhow::ensure!(
            semantic.digest() == descriptor.semantic_digest,
            "immutable generation targets differ from the descriptor semantic digest"
        );
        Ok(semantic)
    }

    fn verify_remote_targets(
        &self,
        descriptor: &GenerationDescriptor,
        descriptor_oid: &str,
        require_retired_sources: bool,
    ) -> Result<BTreeMap<String, String>> {
        validate_descriptor_schema(descriptor)?;
        let mut patterns = vec![
            GENERATION_REF.to_string(),
            crate::hub_v3::CHECKPOINT_REF.to_string(),
            crate::hub_v3::META_REF.to_string(),
            format!("{}*", crate::hub_v3::AGENT_REF_PREFIX),
        ];
        patterns.extend(
            descriptor
                .targets
                .values()
                .chain(descriptor.archives.values())
                .map(|target| target.immutable_ref.clone()),
        );
        patterns.push(descriptor_immutable_ref(&descriptor.generation_id));
        patterns.extend(retired_source_refs(&descriptor.source).into_keys());
        let refs = remote_ref_map(self.repository, self.remote, &patterns)?;
        if refs.get(GENERATION_REF).map(String::as_str) != Some(descriptor_oid) {
            return Err(GenerationPointerChanged {
                actual: refs.get(GENERATION_REF).cloned(),
            }
            .into());
        }
        for target in descriptor
            .targets
            .values()
            .chain(descriptor.archives.values())
        {
            anyhow::ensure!(
                refs.get(&target.immutable_ref) == Some(&target.oid),
                "remote immutable evidence {} does not match generation {}",
                target.immutable_ref,
                descriptor.generation_id
            );
        }
        anyhow::ensure!(
            refs.get(&descriptor_immutable_ref(&descriptor.generation_id))
                == Some(&descriptor_oid.to_string()),
            "remote generation descriptor evidence does not match generation {}",
            descriptor.generation_id
        );
        if require_retired_sources {
            for retired in retired_source_refs(&descriptor.source).into_keys() {
                anyhow::ensure!(
                    !refs.contains_key(&retired),
                    "retired historical authority {retired} is still writable"
                );
            }
        }
        let mut live = refs
            .iter()
            .filter(|(reference, _)| {
                *reference == crate::hub_v3::CHECKPOINT_REF
                    || *reference == crate::hub_v3::META_REF
                    || reference.starts_with(crate::hub_v3::AGENT_REF_PREFIX)
            })
            .map(|(reference, oid)| (reference.clone(), oid.clone()))
            .collect::<BTreeMap<_, _>>();
        for reference in live.keys() {
            validate_canonical_ref(reference)?;
        }
        anyhow::ensure!(
            live.contains_key(crate::hub_v3::CHECKPOINT_REF)
                && live.contains_key(crate::hub_v3::META_REF),
            "remote generation is missing checkpoint or meta"
        );
        let local = local_canonical_refs(self.repository)?;
        for reference in local.keys() {
            validate_canonical_ref(reference)?;
        }
        let mut progressed = false;
        for (canonical, target) in &descriptor.targets {
            let oid = live.get(canonical).ok_or_else(|| {
                anyhow::anyhow!("remote generation removed baseline ref {canonical}")
            })?;
            fetch_ref(self.repository, self.remote, canonical)?;
            validate_object_type(self.repository, oid, "commit")?;
            if oid != &target.oid {
                anyhow::ensure!(
                    is_ancestor(self.repository, &target.oid, oid)?,
                    "remote canonical ref {canonical} is not descended from the migration baseline"
                );
                progressed = true;
            }
        }
        for (canonical, oid) in &live {
            if !descriptor.targets.contains_key(canonical) {
                fetch_ref(self.repository, self.remote, canonical)?;
                validate_object_type(self.repository, oid, "commit")?;
                progressed = true;
            }
        }
        let semantic = self.importer.read_target_semantic(self.repository, &live)?;
        if !progressed {
            anyhow::ensure!(
                semantic.digest() == descriptor.semantic_digest,
                "remote baseline semantic digest differs from its generation descriptor"
            );
        }
        live.retain(|reference, _| {
            reference == crate::hub_v3::CHECKPOINT_REF
                || reference == crate::hub_v3::META_REF
                || reference.starts_with(crate::hub_v3::AGENT_REF_PREFIX)
        });
        Ok(live)
    }

    fn persist(&mut self, journal: &ReconciliationJournal) -> Result<()> {
        self.persist_record(&JournalRecord::Generation(journal.clone()))
    }

    fn persist_record(&mut self, record: &JournalRecord) -> Result<()> {
        self.failure
            .hit(Transition::Journal, TransitionPosition::Before)?;
        write_journal(&self.journal_path, record)?;
        self.failure
            .hit(Transition::Journal, TransitionPosition::After)?;
        Ok(())
    }

    fn pin_source(&self, format: RepositoryFormat) -> Result<SourceEvidence> {
        let mut source = pin_source(self.repository, format)?;
        for (name, oid) in self
            .importer
            .snapshot_source_refs(self.repository, &source)?
        {
            let tree_oid = rev_parse(self.repository, &format!("{oid}^{{tree}}"))?;
            if let Some(evidence) = source.refs.get_mut(&name) {
                evidence.oid = oid;
                evidence.tree_oid = tree_oid;
            } else {
                anyhow::ensure!(
                    name.starts_with("local/"),
                    "historical importer returned an unknown source ref {name}"
                );
                source.refs.insert(
                    name,
                    RefEvidence {
                        authority_oid: oid.clone(),
                        oid,
                        tree_oid,
                        remote_oid: None,
                        remote_tree_oid: None,
                    },
                );
            }
        }
        if !matches!(source.format.shared_store, SharedStoreFormat::Absent)
            && !source.refs.contains_key("local/issues.db")
        {
            source.format.local_database = LocalDatabaseFormat::Missing;
            source.local_fingerprint = None;
        }
        let source_names = source_ref_names(&source.format.shared_store);
        let remote_refs = if source_names.is_empty() {
            BTreeMap::new()
        } else {
            remote_ref_map(self.repository, self.remote, &source_names)?
        };
        for (name, evidence) in &mut source.refs {
            if name.starts_with("local/") {
                continue;
            }
            let Some(remote_oid) = remote_refs.get(name) else {
                continue;
            };
            fetch_oid(self.repository, self.remote, remote_oid)?;
            validate_object_type(self.repository, remote_oid, "commit")?;
            let remote_tree_oid = rev_parse(self.repository, &format!("{remote_oid}^{{tree}}"))?;
            evidence.remote_oid = Some(remote_oid.clone());
            evidence.remote_tree_oid = Some(remote_tree_oid);
        }
        source.fingerprint =
            source_evidence_fingerprint(&source.format, &source.refs, &source.local_fingerprint)?;
        validate_source_schema(&source)?;
        validate_source_objects(self.repository, &source)?;
        Ok(source)
    }

    fn source_still_pinned(
        &self,
        source: &SourceEvidence,
        observed_format: &RepositoryFormat,
    ) -> Result<bool> {
        if has_unrecorded_legacy_source(source, observed_format) {
            return Ok(false);
        }
        if !matches!(observed_format.local_database, LocalDatabaseFormat::Missing) {
            let observed = self.pin_source(observed_format.clone())?;
            if observed.refs.contains_key("local/issues.db")
                && !source.refs.contains_key("local/issues.db")
            {
                return Ok(false);
            }
        }
        let retired = retired_source_refs(source);
        if !retired.is_empty() {
            let mut historical_source_present = false;
            for reference in retired.keys() {
                if local_ref_oid(self.repository, reference)?.is_some()
                    || remote_ref_oid(self.repository, self.remote, reference)?.is_some()
                {
                    historical_source_present = true;
                    break;
                }
            }
            if !historical_source_present {
                return Ok(true);
            }
        }
        let current = self.pin_source(source.format.clone())?;
        let local_projection_consumed = source.refs.contains_key("local/issues.db")
            && !current.refs.contains_key("local/issues.db");
        if current.format.shared_store != source.format.shared_store
            || (!local_projection_consumed
                && (current.format.local_database != source.format.local_database
                    || current.local_fingerprint != source.local_fingerprint))
            || current
                .refs
                .keys()
                .filter(|name| name.as_str() != "local/issues.db")
                .ne(source
                    .refs
                    .keys()
                    .filter(|name| name.as_str() != "local/issues.db"))
            || (!local_projection_consumed
                && current.refs.contains_key("local/issues.db")
                    != source.refs.contains_key("local/issues.db"))
        {
            return Ok(false);
        }
        for (name, expected) in &source.refs {
            if local_projection_consumed && name == "local/issues.db" {
                continue;
            }
            let actual = &current.refs[name];
            if name == crate::hub_v3::CHECKPOINT_REF
                || name == crate::hub_v3::META_REF
                || name.starts_with(crate::hub_v3::AGENT_REF_PREFIX)
            {
                continue;
            }
            if actual.authority_oid != expected.authority_oid
                || actual.oid != expected.oid
                || actual.tree_oid != expected.tree_oid
            {
                return Ok(false);
            }
            let retired = retired_source_refs(source).contains_key(name);
            if actual.remote_oid != expected.remote_oid && !(retired && actual.remote_oid.is_none())
            {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

fn has_unrecorded_legacy_source(
    source: &SourceEvidence,
    observed_format: &RepositoryFormat,
) -> bool {
    source_ref_names(&observed_format.shared_store)
        .into_iter()
        .filter(|name| name == "refs/heads/crosslink/hub" || name == "refs/heads/crosslink/locks")
        .any(|name| !source.refs.contains_key(&name))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AtomicAttempt {
    Published,
    Race,
    Unsupported,
    Waiting(String),
    Rejected(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SinglePush {
    Published,
    AlreadyPresent,
    Race(String),
    Waiting(String),
    Rejected(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AliasConvergence {
    Converged,
    Waiting(String),
    Blocked(String),
    PointerChanged(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RemoteGitError {
    Unavailable(String),
    Rejected(String),
}

#[derive(Debug)]
struct GenerationPointerChanged {
    actual: Option<String>,
}

#[derive(Debug)]
struct PublicationPointerAdvanced;

impl fmt::Display for GenerationPointerChanged {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "generation pointer changed to {}",
            self.actual.as_deref().unwrap_or("an absent value")
        )
    }
}

impl Error for GenerationPointerChanged {}

impl fmt::Display for PublicationPointerAdvanced {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("remote generation pointer advanced during publication")
    }
}

impl Error for PublicationPointerAdvanced {}

impl fmt::Display for RemoteGitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(reason) | Self::Rejected(reason) => formatter.write_str(reason),
        }
    }
}

impl Error for RemoteGitError {}

fn generation_id(
    source: &SourceEvidence,
    semantic_digest: &str,
    targets: &BTreeMap<String, String>,
) -> String {
    let mut hash = Sha256::new();
    hash.update(source.fingerprint.as_bytes());
    hash.update([0]);
    hash.update(semantic_digest.as_bytes());
    hash.update([0]);
    if let Ok(encoded) = serde_json::to_vec(targets) {
        hash.update(encoded);
    }
    let digest = hex::encode(hash.finalize());
    digest.get(..32).unwrap_or(&digest).to_string()
}

fn source_binds_live_semantic(source: &SourceEvidence, live: &BTreeMap<String, String>) -> bool {
    live.iter().all(|(reference, oid)| {
        source.refs.get(reference).is_some_and(|evidence| {
            evidence.oid == *oid || evidence.remote_oid.as_deref() == Some(oid.as_str())
        })
    })
}

fn semantic_preserves_identities(base: &Value, candidate: &Value, source_binds_live: bool) -> bool {
    let base_state = base.get("state").unwrap_or(base);
    let candidate_state = candidate.get("state").unwrap_or(candidate);
    let deleted = candidate_state
        .get("deleted_issues")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<BTreeSet<_>>();
    let base_issues = base_state
        .get("issues")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let candidate_issues = candidate_state
        .get("issues")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    for (uuid, issue) in &base_issues {
        let Some(candidate_issue) = candidate_issues.get(uuid) else {
            if deleted.contains(uuid.as_str()) {
                continue;
            }
            return false;
        };
        if !semantic_value_preserved(issue, candidate_issue, source_binds_live) {
            return false;
        }
    }
    for collection in ["milestones", "comments", "locks"] {
        if !semantic_value_preserved(
            base_state.get(collection).unwrap_or(&Value::Null),
            candidate_state.get(collection).unwrap_or(&Value::Null),
            source_binds_live,
        ) {
            return false;
        }
    }
    base.get("trust") == candidate.get("trust")
}

fn semantic_value_preserved(base: &Value, candidate: &Value, allow_scalar_change: bool) -> bool {
    match base {
        Value::Object(base) => candidate.as_object().is_some_and(|candidate| {
            base.iter().all(|(key, value)| {
                candidate.get(key).is_some_and(|candidate| {
                    semantic_value_preserved(value, candidate, allow_scalar_change)
                })
            })
        }),
        Value::Array(base) => candidate
            .as_array()
            .is_some_and(|candidate| base.iter().all(|value| candidate.contains(value))),
        _ => allow_scalar_change || base == candidate,
    }
}

fn validate_descriptor_schema(descriptor: &GenerationDescriptor) -> Result<()> {
    anyhow::ensure!(
        descriptor.protocol_version == PROTOCOL_VERSION,
        "generation descriptor protocol {} is unsupported",
        descriptor.protocol_version
    );
    validate_source_schema(&descriptor.source)?;
    anyhow::ensure!(
        is_hex_identifier(&descriptor.semantic_digest, 64),
        "generation descriptor has an invalid semantic digest"
    );
    let mut target_oids = BTreeMap::new();
    for (canonical, target) in &descriptor.targets {
        validate_canonical_ref(canonical)?;
        validate_oid(&target.oid)?;
        target_oids.insert(canonical.clone(), target.oid.clone());
    }
    anyhow::ensure!(
        target_oids.contains_key(crate::hub_v3::CHECKPOINT_REF)
            && target_oids.contains_key(crate::hub_v3::META_REF),
        "generation descriptor is missing checkpoint or meta"
    );
    let expected_id = generation_id(
        &descriptor.source,
        &descriptor.semantic_digest,
        &target_oids,
    );
    anyhow::ensure!(
        descriptor.generation_id == expected_id,
        "generation descriptor identity does not match its exact proposal"
    );
    anyhow::ensure!(
        is_hex_identifier(&descriptor.generation_id, 32),
        "generation descriptor has an invalid generation identity"
    );
    for (canonical, target) in &descriptor.targets {
        let expected = format!(
            "{GENERATION_ROOT}/{}/targets/{}",
            descriptor.generation_id,
            encode_ref_name(canonical)
        );
        anyhow::ensure!(
            target.immutable_ref == expected,
            "generation target {canonical} has an inconsistent immutable ref"
        );
    }
    let expected_archives = archive_refs(&descriptor.generation_id, &descriptor.source);
    anyhow::ensure!(
        descriptor.archives == expected_archives,
        "generation descriptor archives do not exactly match source evidence"
    );
    let mut immutable_refs = BTreeSet::new();
    for immutable_ref in descriptor
        .targets
        .values()
        .chain(descriptor.archives.values())
        .map(|target| target.immutable_ref.as_str())
        .chain(std::iter::once(
            descriptor_immutable_ref(&descriptor.generation_id).as_str(),
        ))
    {
        anyhow::ensure!(
            immutable_refs.insert(immutable_ref.to_string()),
            "generation descriptor contains colliding immutable refs"
        );
    }
    Ok(())
}

fn validate_source_schema(source: &SourceEvidence) -> Result<()> {
    let names = source_ref_names(&source.format.shared_store);
    let expected = names.iter().cloned().collect::<BTreeSet<_>>();
    anyhow::ensure!(
        names.len() == expected.len(),
        "source format contains duplicate refs"
    );
    let actual = source
        .refs
        .keys()
        .filter(|name| !name.starts_with("local/"))
        .cloned()
        .collect::<BTreeSet<_>>();
    anyhow::ensure!(
        actual == expected,
        "source evidence refs do not match the detected source format"
    );
    let local = source
        .refs
        .keys()
        .filter(|name| name.starts_with("local/"))
        .cloned()
        .collect::<Vec<_>>();
    if matches!(source.format.shared_store, SharedStoreFormat::Absent) {
        anyhow::ensure!(
            local.len() == 1 && matches!(local[0].as_str(), "local/issues.db" | "local/absent"),
            "absent shared source must have exactly one local snapshot"
        );
    } else {
        anyhow::ensure!(
            local.len() <= 1
                && local
                    .first()
                    .is_none_or(|name| name.as_str() == "local/issues.db"),
            "shared source contains invalid local-only evidence"
        );
    }
    for (name, evidence) in &source.refs {
        if !name.starts_with("local/") {
            validate_source_ref(name)?;
        }
        validate_oid(&evidence.authority_oid)?;
        validate_oid(&evidence.oid)?;
        validate_oid(&evidence.tree_oid)?;
        match (&evidence.remote_oid, &evidence.remote_tree_oid) {
            (Some(oid), Some(tree)) => {
                validate_oid(oid)?;
                validate_oid(tree)?;
            }
            (None, None) => {}
            _ => bail!("source evidence {name} has incomplete remote authority evidence"),
        }
    }
    let fingerprint =
        source_evidence_fingerprint(&source.format, &source.refs, &source.local_fingerprint)?;
    anyhow::ensure!(
        source.fingerprint == fingerprint,
        "source evidence fingerprint is inconsistent"
    );
    Ok(())
}

fn validate_descriptor_objects(repository: &Path, descriptor: &GenerationDescriptor) -> Result<()> {
    validate_source_objects(repository, &descriptor.source)?;
    for target in descriptor
        .targets
        .values()
        .chain(descriptor.archives.values())
    {
        validate_object_type(repository, &target.oid, "commit")?;
    }
    Ok(())
}

fn validate_source_objects(repository: &Path, source: &SourceEvidence) -> Result<()> {
    for evidence in source.refs.values() {
        validate_object_type(repository, &evidence.authority_oid, "commit")?;
        validate_object_type(repository, &evidence.oid, "commit")?;
        validate_object_type(repository, &evidence.tree_oid, "tree")?;
        anyhow::ensure!(
            rev_parse(repository, &format!("{}^{{tree}}", evidence.oid))? == evidence.tree_oid,
            "source snapshot tree does not match its commit"
        );
        if let Some(remote_oid) = &evidence.remote_oid {
            validate_object_type(repository, remote_oid, "commit")?;
        }
        if let Some(remote_tree_oid) = &evidence.remote_tree_oid {
            validate_object_type(repository, remote_tree_oid, "tree")?;
            let remote_oid = evidence
                .remote_oid
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("remote source tree has no commit"))?;
            anyhow::ensure!(
                rev_parse(repository, &format!("{remote_oid}^{{tree}}"))? == *remote_tree_oid,
                "remote source tree does not match its commit"
            );
        }
    }
    Ok(())
}

fn validate_canonical_ref(reference: &str) -> Result<()> {
    if matches!(
        reference,
        crate::hub_v3::CHECKPOINT_REF | crate::hub_v3::META_REF
    ) {
        return Ok(());
    }
    let agent = reference
        .strip_prefix(crate::hub_v3::AGENT_REF_PREFIX)
        .ok_or_else(|| anyhow::anyhow!("descriptor target {reference} is not allowlisted"))?;
    anyhow::ensure!(
        crate::hub_v3::agent_ref_name(agent)? == reference,
        "descriptor target {reference} has an invalid agent identity"
    );
    Ok(())
}

fn validate_source_ref(reference: &str) -> Result<()> {
    if matches!(
        reference,
        "refs/heads/crosslink/hub"
            | "refs/heads/crosslink/locks"
            | crate::hub_v3::CHECKPOINT_REF
            | crate::hub_v3::META_REF
            | crate::hub_v3::OLD_CHECKPOINT_REF
            | crate::hub_v3::OLD_META_REF
    ) {
        return Ok(());
    }
    if let Some(agent) = reference.strip_prefix(crate::hub_v3::AGENT_REF_PREFIX) {
        anyhow::ensure!(
            crate::hub_v3::agent_ref_name(agent)? == reference,
            "source ref {reference} has an invalid agent identity"
        );
        return Ok(());
    }
    if let Some(agent) = reference.strip_prefix(crate::hub_v3::OLD_AGENT_REF_PREFIX) {
        crate::hub_v3::agent_ref_name(agent)?;
        return Ok(());
    }
    bail!("source ref {reference} is not allowlisted")
}

fn validate_oid(oid: &str) -> Result<()> {
    anyhow::ensure!(
        (oid.len() == 40 || oid.len() == 64)
            && oid
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "invalid Git object identifier"
    );
    Ok(())
}

fn is_hex_identifier(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn validate_object_type(repository: &Path, oid: &str, expected: &str) -> Result<()> {
    validate_oid(oid)?;
    let output = Command::new("git")
        .current_dir(repository)
        .args(["cat-file", "-t", oid])
        .output()
        .with_context(|| format!("checking Git object type for {oid}"))?;
    ensure_git_success(&output, "git cat-file -t")?;
    anyhow::ensure!(
        String::from_utf8_lossy(&output.stdout).trim() == expected,
        "Git object {oid} is not a {expected}"
    );
    Ok(())
}

fn pin_source(repository: &Path, format: RepositoryFormat) -> Result<SourceEvidence> {
    let names = source_ref_names(&format.shared_store);
    let mut refs = BTreeMap::new();
    for name in names {
        let oid = local_ref_oid(repository, &name)?.ok_or_else(|| {
            anyhow::anyhow!("historical source ref {name} disappeared while it was being pinned")
        })?;
        let tree_oid = rev_parse(repository, &format!("{oid}^{{tree}}"))?;
        refs.insert(
            name,
            RefEvidence {
                authority_oid: oid.clone(),
                oid,
                tree_oid,
                remote_oid: None,
                remote_tree_oid: None,
            },
        );
    }
    let local_fingerprint = local_database_fingerprint(&format.local_database);
    let fingerprint = source_evidence_fingerprint(&format, &refs, &local_fingerprint)?;
    Ok(SourceEvidence {
        format,
        refs,
        local_fingerprint,
        fingerprint,
    })
}

fn source_evidence_fingerprint(
    format: &RepositoryFormat,
    refs: &BTreeMap<String, RefEvidence>,
    local_fingerprint: &Option<String>,
) -> Result<String> {
    let encoded = serde_json::to_vec(&(format, refs, local_fingerprint))
        .context("serializing pinned source evidence")?;
    Ok(hex::encode(Sha256::digest(encoded)))
}

fn source_ref_names(format: &SharedStoreFormat) -> Vec<String> {
    match format {
        SharedStoreFormat::Absent | SharedStoreFormat::Unreadable { .. } => Vec::new(),
        SharedStoreFormat::LegacyLocks { refs }
        | SharedStoreFormat::V2 { refs }
        | SharedStoreFormat::HiddenV3 { refs }
        | SharedStoreFormat::VisibleV3 { refs }
        | SharedStoreFormat::Mixed { refs, .. } => refs.clone(),
    }
}

fn local_database_fingerprint(format: &LocalDatabaseFormat) -> Option<String> {
    match format {
        LocalDatabaseFormat::Sqlite {
            version,
            schema_fingerprint,
            issue_count,
            size_bytes,
        } => Some(format!(
            "sqlite:{version}:{schema_fingerprint}:{issue_count:?}:{size_bytes}"
        )),
        LocalDatabaseFormat::Future {
            version,
            supported_version,
            schema_fingerprint,
            size_bytes,
        } => Some(format!(
            "future:{version}:{supported_version}:{schema_fingerprint}:{size_bytes}"
        )),
        LocalDatabaseFormat::Unreadable { reason } => Some(format!("unreadable:{reason}")),
        LocalDatabaseFormat::Missing => None,
    }
}

fn target_refs(
    generation_id: &str,
    targets: &BTreeMap<String, String>,
) -> BTreeMap<String, TargetRef> {
    targets
        .iter()
        .map(|(canonical, oid)| {
            (
                canonical.clone(),
                TargetRef {
                    oid: oid.clone(),
                    immutable_ref: format!(
                        "{GENERATION_ROOT}/{generation_id}/targets/{}",
                        encode_ref_name(canonical)
                    ),
                },
            )
        })
        .collect()
}

fn archive_refs(generation_id: &str, source: &SourceEvidence) -> BTreeMap<String, TargetRef> {
    source_archive_oids(source)
        .iter()
        .map(|(name, oid)| {
            (
                name.clone(),
                TargetRef {
                    oid: oid.clone(),
                    immutable_ref: format!(
                        "{ARCHIVE_ROOT}/{generation_id}/{}",
                        encode_ref_name(name)
                    ),
                },
            )
        })
        .collect()
}

fn source_archive_oids(source: &SourceEvidence) -> BTreeMap<String, String> {
    let mut archives = BTreeMap::new();
    for (name, evidence) in &source.refs {
        archives.insert(name.clone(), evidence.oid.clone());
        archives.insert(format!("authority:{name}"), evidence.authority_oid.clone());
        if let Some(remote_oid) = &evidence.remote_oid {
            archives.insert(format!("remote:{name}"), remote_oid.clone());
        }
    }
    archives
}

fn encode_ref_name(name: &str) -> String {
    let digest = hex::encode(Sha256::digest(name.as_bytes()));
    digest.get(..32).unwrap_or(&digest).to_string()
}

fn descriptor_immutable_ref(generation_id: &str) -> String {
    format!("{GENERATION_ROOT}/{generation_id}/descriptor")
}

fn descriptor_target_oids(descriptor: &GenerationDescriptor) -> BTreeMap<String, String> {
    descriptor
        .targets
        .iter()
        .map(|(name, target)| (name.clone(), target.oid.clone()))
        .collect()
}

fn materialize_local_evidence(
    repository: &Path,
    descriptor: &GenerationDescriptor,
    descriptor_oid: &str,
) -> Result<()> {
    validate_descriptor_schema(descriptor)?;
    validate_descriptor_objects(repository, descriptor)?;
    validate_object_type(repository, descriptor_oid, "commit")?;
    update_ref(
        repository,
        &format!("{LOCAL_ROOT}/{}/descriptor", descriptor.generation_id),
        descriptor_oid,
    )?;
    for target in descriptor
        .targets
        .values()
        .chain(descriptor.archives.values())
    {
        let local_ref = format!(
            "{LOCAL_ROOT}/{}/{}",
            descriptor.generation_id,
            encode_ref_name(&target.immutable_ref)
        );
        update_ref(repository, &local_ref, &target.oid)?;
    }
    Ok(())
}

fn materialize_intent_evidence(repository: &Path, intent: &JournalIntent) -> Result<()> {
    validate_source_schema(&intent.source)?;
    validate_source_objects(repository, &intent.source)?;
    for (name, oid) in source_archive_oids(&intent.source) {
        update_ref(
            repository,
            &format!(
                "{LOCAL_ROOT}/intents/{}/{}",
                intent.provisional_id,
                encode_ref_name(&name)
            ),
            &oid,
        )?;
    }
    Ok(())
}

fn local_authority_refs(repository: &Path) -> Result<BTreeMap<String, String>> {
    let refs = local_ref_list(repository, "refs/heads/crosslink/")?;
    Ok(refs
        .into_iter()
        .filter(|(name, _)| !name.starts_with(GENERATION_ROOT))
        .collect())
}

fn local_canonical_refs(repository: &Path) -> Result<BTreeMap<String, String>> {
    Ok(local_ref_list(repository, "refs/heads/crosslink/")?
        .into_iter()
        .filter(|(reference, _)| {
            reference == crate::hub_v3::CHECKPOINT_REF
                || reference == crate::hub_v3::META_REF
                || reference.starts_with(crate::hub_v3::AGENT_REF_PREFIX)
        })
        .collect())
}

fn ensure_authority_unchanged(
    repository: &Path,
    expected: &BTreeMap<String, String>,
) -> Result<()> {
    let actual = local_authority_refs(repository)?;
    anyhow::ensure!(
        &actual == expected,
        "historical importer changed authoritative refs during prepare or verify"
    );
    Ok(())
}

fn write_descriptor(repository: &Path, descriptor: &GenerationDescriptor) -> Result<String> {
    validate_descriptor_schema(descriptor)?;
    validate_descriptor_objects(repository, descriptor)?;
    let bytes =
        serde_json::to_vec_pretty(descriptor).context("serializing generation descriptor")?;
    let blob = hash_object(repository, &bytes)?;
    let tree_input = format!("100644 blob {blob}\tgeneration.json\n");
    let tree = run_git_input(repository, &["mktree"], tree_input.as_bytes())?;
    let mut command = Command::new("git");
    command
        .current_dir(repository)
        .args([
            "-c",
            "commit.gpgsign=false",
            "commit-tree",
            tree.trim(),
            "-m",
            "crosslink reconciliation generation",
        ])
        .env("GIT_AUTHOR_NAME", "crosslink-reconciler")
        .env("GIT_AUTHOR_EMAIL", "reconciler@crosslink")
        .env("GIT_COMMITTER_NAME", "crosslink-reconciler")
        .env("GIT_COMMITTER_EMAIL", "reconciler@crosslink")
        .env("GIT_AUTHOR_DATE", "1970-01-01T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "1970-01-01T00:00:00Z");
    let output = command
        .output()
        .context("creating generation descriptor commit")?;
    ensure_git_success(&output, "git commit-tree generation descriptor")?;
    nonempty_stdout(&output, "git commit-tree generation descriptor")
}

fn read_descriptor(repository: &Path, oid: &str) -> Result<GenerationDescriptor> {
    validate_oid(oid)?;
    validate_object_type(repository, oid, "commit")?;
    let output = Command::new("git")
        .current_dir(repository)
        .args(["show", &format!("{oid}:generation.json")])
        .output()
        .context("reading remote generation descriptor")?;
    ensure_git_success(&output, "git show generation descriptor")?;
    let descriptor: GenerationDescriptor =
        serde_json::from_slice(&output.stdout).context("parsing remote generation descriptor")?;
    validate_descriptor_schema(&descriptor)?;
    Ok(descriptor)
}

fn write_journal(path: &Path, record: &JournalRecord) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("journal path has no parent: {}", path.display()))?;
    let parent_existed = parent.exists();
    let journal_existed = path.exists();
    fs::create_dir_all(parent)
        .with_context(|| format!("creating journal directory {}", parent.display()))?;
    if !parent_existed {
        if let Some(grandparent) = parent.parent() {
            sync_directory(grandparent)?;
        }
    }
    fs::create_dir_all(path).with_context(|| format!("creating journal {}", path.display()))?;
    if !journal_existed {
        sync_directory(parent)?;
    }
    let sequence = next_journal_sequence(path)?;
    let destination = path.join(format!("{sequence:020}.json"));
    let temp = path.join(format!(".{sequence:020}.{}.tmp", std::process::id()));
    let bytes = serde_json::to_vec_pretty(record).context("serializing reconciliation journal")?;
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&temp)
        .with_context(|| format!("opening temporary journal {}", temp.display()))?;
    file.write_all(&bytes)
        .with_context(|| format!("writing temporary journal {}", temp.display()))?;
    file.sync_all()
        .with_context(|| format!("syncing temporary journal {}", temp.display()))?;
    fs::rename(&temp, &destination).with_context(|| {
        format!(
            "atomically appending reconciliation journal {}",
            destination.display()
        )
    })?;
    sync_directory(path)?;
    Ok(())
}

fn read_journal(repository: &Path, path: &Path) -> Result<Option<JournalRecord>> {
    if !path.exists() {
        return Ok(None);
    }
    let latest = latest_journal_record(path)?
        .ok_or_else(|| anyhow::anyhow!("reconciliation journal {} is empty", path.display()))?;
    let bytes = fs::read(&latest)
        .with_context(|| format!("reading reconciliation journal {}", latest.display()))?;
    let record: JournalRecord = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing reconciliation journal {}", latest.display()))?;
    match &record {
        JournalRecord::Intent(intent) => {
            validate_source_schema(&intent.source)?;
            validate_source_objects(repository, &intent.source)?;
            validate_canonical_snapshot_objects(repository, &intent.canonical_refs)?;
            anyhow::ensure!(
                intent.provisional_id
                    == intent
                        .source
                        .fingerprint
                        .get(..24)
                        .unwrap_or(&intent.source.fingerprint),
                "journal intent proposal identity is inconsistent"
            );
        }
        JournalRecord::Generation(journal) => {
            validate_descriptor_schema(&journal.descriptor)?;
            validate_descriptor_objects(repository, &journal.descriptor)?;
            validate_oid(&journal.descriptor_oid)?;
            validate_object_type(repository, &journal.descriptor_oid, "commit")?;
            let stored = read_descriptor(repository, &journal.descriptor_oid)?;
            anyhow::ensure!(
                stored == journal.descriptor,
                "journal descriptor object differs from the recorded descriptor"
            );
            if !journal.alias_expectations.is_empty() {
                anyhow::ensure!(
                    journal.alias_expectations.len() == journal.descriptor.targets.len()
                        && journal
                            .descriptor
                            .targets
                            .keys()
                            .all(|reference| journal.alias_expectations.contains_key(reference)),
                    "journal alias expectations do not match descriptor targets"
                );
                for oid in journal.alias_expectations.values().flatten() {
                    validate_oid(oid)?;
                }
            }
        }
    }
    Ok(Some(record))
}

fn remove_journal(path: &Path) -> Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => {
            if let Some(parent) = path.parent() {
                sync_directory(parent)?;
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error)
            .with_context(|| format!("removing reconciliation journal {}", path.display())),
    }
}

fn next_journal_sequence(path: &Path) -> Result<u64> {
    Ok(latest_journal_record(path)?
        .and_then(|record| {
            record
                .file_stem()
                .and_then(|stem| stem.to_str())
                .and_then(|stem| stem.parse::<u64>().ok())
        })
        .unwrap_or(0)
        + 1)
}

fn latest_journal_record(path: &Path) -> Result<Option<PathBuf>> {
    let mut records = fs::read_dir(path)
        .with_context(|| format!("reading reconciliation journal {}", path.display()))?
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .filter(|record| record.extension().and_then(|ext| ext.to_str()) == Some("json"))
        .collect::<Vec<_>>();
    records.sort();
    Ok(records.pop())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)
        .with_context(|| format!("opening directory {} for journal sync", path.display()))?
        .sync_all()
        .with_context(|| format!("syncing journal directory {}", path.display()))
}

#[cfg(windows)]
fn sync_directory(path: &Path) -> Result<()> {
    use std::os::windows::fs::OpenOptionsExt as _;

    OpenOptions::new()
        .read(true)
        .custom_flags(0x0200_0000 | 0x8000_0000)
        .open(path)
        .with_context(|| format!("opening directory {} for journal sync", path.display()))?
        .sync_all()
        .with_context(|| format!("syncing journal directory {}", path.display()))
}

#[cfg(not(any(unix, windows)))]
fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)
        .with_context(|| format!("opening directory {} for journal sync", path.display()))?
        .sync_all()
        .with_context(|| format!("syncing journal directory {}", path.display()))
}

fn publication_ref_patterns(descriptor: &GenerationDescriptor) -> Vec<String> {
    let mut refs = vec![
        GENERATION_REF.to_string(),
        descriptor_immutable_ref(&descriptor.generation_id),
        format!("{}*", crate::hub_v3::AGENT_REF_PREFIX),
    ];
    refs.extend(descriptor.targets.keys().cloned());
    refs.extend(retired_source_refs(&descriptor.source).into_keys());
    refs.extend(
        descriptor
            .targets
            .values()
            .chain(descriptor.archives.values())
            .map(|target| target.immutable_ref.clone()),
    );
    refs.sort();
    refs.dedup();
    refs
}

fn canonical_ref_patterns() -> Vec<String> {
    vec![
        crate::hub_v3::CHECKPOINT_REF.to_string(),
        crate::hub_v3::META_REF.to_string(),
        format!("{}*", crate::hub_v3::AGENT_REF_PREFIX),
    ]
}

fn validate_observed_canonical_refs(
    repository: &Path,
    remote: &str,
    refs: &BTreeMap<String, String>,
) -> Result<()> {
    for (reference, oid) in refs {
        validate_canonical_ref(reference)?;
        validate_oid(oid)?;
        fetch_ref(repository, remote, reference)?;
        validate_object_type(repository, oid, "commit")?;
    }
    Ok(())
}

fn validate_canonical_snapshot_objects(
    repository: &Path,
    refs: &BTreeMap<String, String>,
) -> Result<()> {
    for (reference, oid) in refs {
        validate_canonical_ref(reference)?;
        validate_oid(oid)?;
        validate_object_type(repository, oid, "commit")?;
    }
    Ok(())
}

fn plan_alias_publication(
    repository: &Path,
    remote: &str,
    journal: &ReconciliationJournal,
    remote_refs: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>> {
    validate_alias_expectations(&journal.descriptor, &journal.alias_expectations)?;
    let mut plan = BTreeMap::new();
    for (reference, target) in &journal.descriptor.targets {
        let expected = journal
            .alias_expectations
            .get(reference)
            .ok_or_else(|| anyhow::anyhow!("missing alias expectation for {reference}"))?;
        let actual = remote_refs.get(reference);
        if actual == Some(&target.oid) || actual == expected.as_ref() {
            plan.insert(reference.clone(), target.oid.clone());
            continue;
        }
        if let Some(actual) = actual {
            fetch_ref(repository, remote, reference)?;
            validate_object_type(repository, actual, "commit")?;
            anyhow::ensure!(
                is_ancestor(repository, &target.oid, actual)?,
                "canonical ref {reference} diverged from its prepared target"
            );
            plan.insert(reference.clone(), actual.clone());
            continue;
        }
        anyhow::bail!("canonical ref {reference} disappeared after expectations were recorded");
    }
    for (reference, oid) in remote_refs.iter().filter(|(reference, _)| {
        reference.starts_with(crate::hub_v3::AGENT_REF_PREFIX)
            && !journal.descriptor.targets.contains_key(*reference)
    }) {
        validate_canonical_ref(reference)?;
        fetch_ref(repository, remote, reference)?;
        validate_object_type(repository, oid, "commit")?;
        plan.insert(reference.clone(), oid.clone());
    }
    Ok(plan)
}

fn alias_expectations_from_source(
    descriptor: &GenerationDescriptor,
) -> BTreeMap<String, Option<String>> {
    descriptor
        .targets
        .keys()
        .map(|canonical| {
            (
                canonical.clone(),
                descriptor
                    .source
                    .refs
                    .get(canonical)
                    .and_then(|evidence| evidence.remote_oid.clone()),
            )
        })
        .collect()
}

fn validate_alias_expectations(
    descriptor: &GenerationDescriptor,
    expectations: &BTreeMap<String, Option<String>>,
) -> Result<()> {
    anyhow::ensure!(
        expectations.len() == descriptor.targets.len()
            && descriptor
                .targets
                .keys()
                .all(|reference| expectations.contains_key(reference)),
        "alias expectations do not match descriptor targets"
    );
    for oid in expectations.values().flatten() {
        validate_oid(oid)?;
    }
    Ok(())
}

fn retired_source_refs(source: &SourceEvidence) -> BTreeMap<String, String> {
    source
        .refs
        .iter()
        .filter_map(|(name, evidence)| {
            let retired = matches!(
                name.as_str(),
                "refs/heads/crosslink/hub"
                    | "refs/heads/crosslink/locks"
                    | crate::hub_v3::OLD_CHECKPOINT_REF
                    | crate::hub_v3::OLD_META_REF
            ) || name.starts_with(crate::hub_v3::OLD_AGENT_REF_PREFIX);
            retired
                .then(|| {
                    evidence
                        .remote_oid
                        .as_ref()
                        .map(|oid| (name.clone(), oid.clone()))
                })
                .flatten()
        })
        .collect()
}

fn ensure_source_expectations(
    source: &SourceEvidence,
    remote_refs: &BTreeMap<String, String>,
) -> Result<()> {
    for (reference, expected) in retired_source_refs(source) {
        anyhow::ensure!(
            remote_refs.get(&reference) == Some(&expected),
            "remote historical source {reference} changed after it was pinned"
        );
    }
    Ok(())
}

fn ensure_source_not_advanced_after_commit(
    source: &SourceEvidence,
    remote_refs: &BTreeMap<String, String>,
) -> Result<()> {
    for (reference, expected) in retired_source_refs(source) {
        if let Some(actual) = remote_refs.get(&reference) {
            anyhow::ensure!(
                actual == &expected,
                "remote historical source {reference} changed after the authority commit"
            );
        }
    }
    Ok(())
}

fn ensure_immutable_refs_compatible(
    journal: &ReconciliationJournal,
    remote_refs: &BTreeMap<String, String>,
) -> Result<()> {
    validate_descriptor_schema(&journal.descriptor)?;
    let mut expected = BTreeMap::from([(
        descriptor_immutable_ref(&journal.descriptor.generation_id),
        journal.descriptor_oid.clone(),
    )]);
    expected.extend(
        journal
            .descriptor
            .targets
            .values()
            .chain(journal.descriptor.archives.values())
            .map(|target| (target.immutable_ref.clone(), target.oid.clone())),
    );
    for (reference, oid) in expected {
        if let Some(remote_oid) = remote_refs.get(&reference) {
            anyhow::ensure!(
                remote_oid == &oid,
                "remote immutable ref {reference} already points to a different object"
            );
        }
    }
    Ok(())
}

fn append_push_leases(
    journal: &ReconciliationJournal,
    remote_refs: &BTreeMap<String, String>,
    args: &mut Vec<String>,
    alias_plan: &BTreeMap<String, String>,
) {
    let mut destinations = vec![
        GENERATION_REF.to_string(),
        descriptor_immutable_ref(&journal.descriptor.generation_id),
    ];
    destinations.extend(
        journal
            .descriptor
            .targets
            .values()
            .chain(journal.descriptor.archives.values())
            .map(|target| target.immutable_ref.clone()),
    );
    destinations.extend(alias_plan.keys().cloned());
    destinations.extend(retired_source_refs(&journal.descriptor.source).into_keys());
    destinations.sort();
    destinations.dedup();
    for destination in destinations {
        let expected = remote_refs.get(&destination).map_or("", String::as_str);
        args.push(format!("--force-with-lease={destination}:{expected}"));
    }
}

fn append_push_refspecs(
    journal: &ReconciliationJournal,
    remote_refs: &BTreeMap<String, String>,
    args: &mut Vec<String>,
    alias_plan: &BTreeMap<String, String>,
) {
    let mut updates = vec![
        (journal.descriptor_oid.clone(), GENERATION_REF.to_string()),
        (
            journal.descriptor_oid.clone(),
            descriptor_immutable_ref(&journal.descriptor.generation_id),
        ),
    ];
    updates.extend(
        journal
            .descriptor
            .targets
            .values()
            .chain(journal.descriptor.archives.values())
            .map(|target| (target.oid.clone(), target.immutable_ref.clone())),
    );
    updates.extend(
        alias_plan
            .iter()
            .map(|(canonical, oid)| (oid.clone(), canonical.clone())),
    );
    updates.extend(
        retired_source_refs(&journal.descriptor.source)
            .into_keys()
            .map(|reference| (String::new(), reference)),
    );
    updates.sort_by(|left, right| left.1.cmp(&right.1));
    updates.dedup_by(|left, right| left.1 == right.1);
    for (oid, destination) in updates {
        let is_alias = alias_plan.contains_key(&destination);
        if !is_alias
            && ((!oid.is_empty()
                && remote_refs
                    .get(&destination)
                    .is_some_and(|remote| remote == &oid))
                || (oid.is_empty() && !remote_refs.contains_key(&destination)))
        {
            continue;
        }
        args.push(format!("{oid}:{destination}"));
    }
}

fn classify_atomic_push(output: &Output) -> AtomicAttempt {
    if output.status.success() {
        return AtomicAttempt::Published;
    }
    let message = output_message(output);
    if is_explicit_atomic_unsupported(output, &message) {
        AtomicAttempt::Unsupported
    } else if is_lease_rejection(&message) {
        AtomicAttempt::Race
    } else {
        match classify_remote_error("git push --atomic", message) {
            RemoteGitError::Unavailable(reason) => AtomicAttempt::Waiting(reason),
            RemoteGitError::Rejected(reason) => AtomicAttempt::Rejected(reason),
        }
    }
}

fn push_oid_if_absent(
    repository: &Path,
    remote: &str,
    oid: &str,
    destination: &str,
) -> Result<SinglePush> {
    if let Some(existing) = remote_ref_oid(repository, remote, destination)? {
        return if existing == oid {
            Ok(SinglePush::AlreadyPresent)
        } else {
            Ok(SinglePush::Race(format!(
                "immutable remote ref {destination} already points to a different object"
            )))
        };
    }
    push_oid_with_lease(repository, remote, oid, destination, None)
}

fn push_oid_with_lease(
    repository: &Path,
    remote: &str,
    oid: &str,
    destination: &str,
    expected: Option<&str>,
) -> Result<SinglePush> {
    if remote_ref_oid(repository, remote, destination)?.as_deref() == Some(oid) {
        return Ok(SinglePush::AlreadyPresent);
    }
    let lease = format!(
        "--force-with-lease={destination}:{}",
        expected.unwrap_or("")
    );
    let refspec = format!("{oid}:{destination}");
    let output = Command::new("git")
        .current_dir(repository)
        .args(["push", &lease, remote, &refspec])
        .output()
        .with_context(|| format!("pushing {destination} with a ref lease"))?;
    if output.status.success() {
        return Ok(SinglePush::Published);
    }
    let message = output_message(&output);
    if is_lease_rejection(&message) {
        Ok(SinglePush::Race(message))
    } else {
        match classify_remote_error("git push", message) {
            RemoteGitError::Unavailable(reason) => Ok(SinglePush::Waiting(reason)),
            RemoteGitError::Rejected(reason) => Ok(SinglePush::Rejected(reason)),
        }
    }
}

fn delete_ref_with_lease(
    repository: &Path,
    remote: &str,
    destination: &str,
    expected: &str,
) -> Result<SinglePush> {
    let current = remote_ref_oid(repository, remote, destination)?;
    let Some(current) = current else {
        return Ok(SinglePush::AlreadyPresent);
    };
    if current != expected {
        return Ok(SinglePush::Race(format!(
            "historical source {destination} advanced from {expected} to {current} after cutover"
        )));
    }
    let lease = format!("--force-with-lease={destination}:{expected}");
    let refspec = format!(":{destination}");
    let output = Command::new("git")
        .current_dir(repository)
        .args(["push", &lease, remote, &refspec])
        .output()
        .with_context(|| format!("retiring historical source {destination}"))?;
    if output.status.success() {
        return Ok(SinglePush::Published);
    }
    let message = output_message(&output);
    if is_lease_rejection(&message) {
        Ok(SinglePush::Race(message))
    } else {
        match classify_remote_error("git push retirement", message) {
            RemoteGitError::Unavailable(reason) => Ok(SinglePush::Waiting(reason)),
            RemoteGitError::Rejected(reason) => Ok(SinglePush::Rejected(reason)),
        }
    }
}

fn remote_ref_oid(repository: &Path, remote: &str, reference: &str) -> Result<Option<String>> {
    Ok(remote_ref_map(repository, remote, &[reference.to_string()])?.remove(reference))
}

fn ensure_generation_pointer(repository: &Path, remote: &str, expected: &str) -> Result<()> {
    let actual = remote_ref_oid(repository, remote, GENERATION_REF)?;
    if actual.as_deref() == Some(expected) {
        Ok(())
    } else {
        Err(GenerationPointerChanged { actual }.into())
    }
}

fn pointer_change_outcome(error: &anyhow::Error) -> Option<AliasConvergence> {
    error
        .downcast_ref::<GenerationPointerChanged>()
        .map(|changed| match &changed.actual {
            Some(actual) => AliasConvergence::PointerChanged(actual.clone()),
            None => AliasConvergence::Blocked(
                "committed generation pointer disappeared during reconciliation".to_string(),
            ),
        })
}

fn remote_ref_map(
    repository: &Path,
    remote: &str,
    patterns: &[String],
) -> Result<BTreeMap<String, String>> {
    let mut command = Command::new("git");
    command.current_dir(repository).args(["ls-remote", remote]);
    command.args(patterns);
    let output = command
        .output()
        .with_context(|| format!("listing reconciliation refs from remote {remote}"))?;
    if !output.status.success() {
        return Err(classify_remote_error(
            &format!("git ls-remote {remote}"),
            output_message(&output),
        )
        .into());
    }
    let mut refs = BTreeMap::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if let Some((oid, name)) = line.split_once('\t') {
            let oid = oid.trim();
            let name = name.trim();
            validate_oid(oid)?;
            anyhow::ensure!(
                patterns
                    .iter()
                    .any(|pattern| remote_pattern_matches(pattern, name)),
                "remote returned unexpected ref {name}"
            );
            refs.insert(name.to_string(), oid.to_string());
        }
    }
    Ok(refs)
}

fn remote_pattern_matches(pattern: &str, reference: &str) -> bool {
    pattern.strip_suffix('*').map_or_else(
        || pattern == reference,
        |prefix| reference.starts_with(prefix),
    )
}

fn local_ref_oid(repository: &Path, reference: &str) -> Result<Option<String>> {
    let output = Command::new("git")
        .current_dir(repository)
        .args(["rev-parse", "--verify", "--quiet", reference])
        .output()
        .with_context(|| format!("pinning local ref {reference}"))?;
    if !output.status.success() {
        return Ok(None);
    }
    let oid = nonempty_stdout(&output, "git rev-parse")?;
    Ok(Some(oid))
}

fn local_ref_list(repository: &Path, prefix: &str) -> Result<BTreeMap<String, String>> {
    let output = Command::new("git")
        .current_dir(repository)
        .args([
            "for-each-ref",
            "--format=%(refname)%00%(objectname)",
            prefix,
        ])
        .output()
        .with_context(|| format!("listing local refs below {prefix}"))?;
    ensure_git_success(&output, "git for-each-ref")?;
    let mut refs = BTreeMap::new();
    for line in output.stdout.split(|byte| *byte == b'\n') {
        let mut fields = line.split(|byte| *byte == 0);
        let Some(name) = fields.next() else {
            continue;
        };
        let Some(oid) = fields.next() else {
            continue;
        };
        if name.is_empty() || oid.is_empty() {
            continue;
        }
        refs.insert(
            String::from_utf8_lossy(name).to_string(),
            String::from_utf8_lossy(oid).to_string(),
        );
    }
    Ok(refs)
}

fn rev_parse(repository: &Path, revision: &str) -> Result<String> {
    let output = Command::new("git")
        .current_dir(repository)
        .args(["rev-parse", "--verify", revision])
        .output()
        .with_context(|| format!("resolving Git revision {revision}"))?;
    ensure_git_success(&output, "git rev-parse")?;
    nonempty_stdout(&output, "git rev-parse")
}

fn is_ancestor(repository: &Path, ancestor: &str, descendant: &str) -> Result<bool> {
    let output = Command::new("git")
        .current_dir(repository)
        .args(["merge-base", "--is-ancestor", ancestor, descendant])
        .output()
        .with_context(|| format!("comparing source history {ancestor}..{descendant}"))?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => bail!(
            "git merge-base failed while comparing source history: {}",
            output_message(&output)
        ),
    }
}

fn update_ref(repository: &Path, reference: &str, oid: &str) -> Result<()> {
    let output = Command::new("git")
        .current_dir(repository)
        .args(["update-ref", reference, oid])
        .output()
        .with_context(|| format!("recording local reconciliation evidence {reference}"))?;
    ensure_git_success(&output, "git update-ref")
}

fn update_ref_cas(
    repository: &Path,
    reference: &str,
    oid: &str,
    expected: Option<&str>,
) -> Result<bool> {
    let absent = "0".repeat(oid.len());
    let output = Command::new("git")
        .current_dir(repository)
        .args(["update-ref", reference, oid, expected.unwrap_or(&absent)])
        .output()
        .with_context(|| format!("updating local canonical ref {reference} with a lease"))?;
    Ok(output.status.success())
}

fn hash_object(repository: &Path, bytes: &[u8]) -> Result<String> {
    run_git_input(repository, &["hash-object", "-w", "--stdin"], bytes)
}

fn run_git_input(repository: &Path, args: &[&str], bytes: &[u8]) -> Result<String> {
    let mut child = Command::new("git")
        .current_dir(repository)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("spawning git {args:?}"))?;
    child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("git {args:?} stdin was unavailable"))?
        .write_all(bytes)
        .with_context(|| format!("writing stdin for git {args:?}"))?;
    let output = child
        .wait_with_output()
        .with_context(|| format!("waiting for git {args:?}"))?;
    ensure_git_success(&output, &format!("git {args:?}"))?;
    nonempty_stdout(&output, &format!("git {args:?}"))
}

fn run_git_raw(repository: &Path, args: &[String]) -> Result<Output> {
    Command::new("git")
        .current_dir(repository)
        .args(args)
        .output()
        .with_context(|| format!("running git {args:?}"))
}

fn fetch_oid(repository: &Path, remote: &str, oid: &str) -> Result<()> {
    validate_oid(oid)?;
    let output = Command::new("git")
        .current_dir(repository)
        .args(["fetch", "--no-tags", remote, oid])
        .output()
        .with_context(|| format!("fetching generation object {oid}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(classify_remote_error("git fetch generation object", output_message(&output)).into())
    }
}

fn fetch_ref(repository: &Path, remote: &str, reference: &str) -> Result<()> {
    let output = Command::new("git")
        .current_dir(repository)
        .args(["fetch", "--no-tags", remote, reference])
        .output()
        .with_context(|| format!("fetching immutable reconciliation ref {reference}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(classify_remote_error(
            "git fetch immutable reconciliation ref",
            output_message(&output),
        )
        .into())
    }
}

fn object_exists(repository: &Path, oid: &str) -> Result<bool> {
    let output = Command::new("git")
        .current_dir(repository)
        .args(["cat-file", "-e", &format!("{oid}^{{object}}")])
        .output()
        .with_context(|| format!("checking Git object {oid}"))?;
    Ok(output.status.success())
}

fn ensure_git_success(output: &Output, operation: &str) -> Result<()> {
    anyhow::ensure!(
        output.status.success(),
        "{operation} failed: {}",
        output_message(output)
    );
    Ok(())
}

fn nonempty_stdout(output: &Output, operation: &str) -> Result<String> {
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    anyhow::ensure!(!value.is_empty(), "{operation} returned empty output");
    Ok(value)
}

fn output_message(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if stderr.is_empty() {
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    } else {
        stderr
    }
}

fn is_lease_rejection(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("stale info")
        || lower.contains("non-fast-forward")
        || lower.contains("fetch first")
        || (lower.contains("cannot lock ref") && lower.contains("reference already exists"))
}

fn is_remote_unavailable_message(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("could not resolve host")
        || lower.contains("could not read from remote repository")
        || lower.contains("connection timed out")
        || lower.contains("connection refused")
        || lower.contains("network is unreachable")
        || lower.contains("no such file or directory")
        || lower.contains("does not appear to be a git repository")
}

fn classify_remote_error(operation: &str, message: String) -> RemoteGitError {
    let reason = format!("{operation} failed: {message}");
    if is_remote_unavailable_message(&message) {
        RemoteGitError::Unavailable(reason)
    } else {
        RemoteGitError::Rejected(reason)
    }
}

fn is_explicit_atomic_unsupported(output: &Output, message: &str) -> bool {
    output.status.code() == Some(128)
        && message.lines().any(|line| {
            matches!(
                line.trim().to_ascii_lowercase().as_str(),
                "fatal: the receiving end does not support --atomic push"
                    | "fatal: the remote end does not support --atomic push"
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const CHECKPOINT: &str = "refs/heads/crosslink/checkpoint";
    const META: &str = "refs/heads/crosslink/meta";
    const V2: &str = "refs/heads/crosslink/hub";
    const LEGACY: &str = "refs/heads/crosslink/locks";
    const HIDDEN_CHECKPOINT: &str = "refs/crosslink/checkpoint";
    const HIDDEN_META: &str = "refs/crosslink/meta";

    #[derive(Debug)]
    struct ObjectImporter {
        semantic: Value,
        salt: String,
    }

    impl ObjectImporter {
        fn new(salt: &str) -> Self {
            Self {
                semantic: serde_json::json!({
                    "issues": {"issue-1": {"title": "preserved"}},
                    "comments": {},
                    "milestones": {},
                    "locks": {"issue-1": "agent-a"},
                    "trust": {"allowed": ["agent-a"]}
                }),
                salt: salt.to_string(),
            }
        }
    }

    impl HistoricalImporter for ObjectImporter {
        fn snapshot_source_refs(
            &self,
            repository: &Path,
            source: &SourceEvidence,
        ) -> Result<BTreeMap<String, String>> {
            if matches!(source.format.shared_store, SharedStoreFormat::Absent) {
                return Ok(BTreeMap::from([(
                    "local/absent".to_string(),
                    state_commit(repository, &serde_json::json!({}), "absent source")?,
                )]));
            }
            Ok(BTreeMap::new())
        }

        fn prepare_file_source(
            &self,
            repository: &Path,
            source: &SourceEvidence,
            generation_id: &str,
        ) -> Result<PreparedImport> {
            let checkpoint = state_commit(
                repository,
                &self.semantic,
                &format!("checkpoint-{generation_id}-{}", self.salt),
            )?;
            let meta = state_commit(
                repository,
                &serde_json::json!({"hub_version": 3}),
                &format!("meta-{generation_id}-{}", self.salt),
            )?;
            let mut targets = BTreeMap::from([
                (CHECKPOINT.to_string(), checkpoint),
                (META.to_string(), meta),
            ]);
            targets.extend(
                source
                    .refs
                    .iter()
                    .filter(|(reference, _)| reference.starts_with(crate::hub_v3::AGENT_REF_PREFIX))
                    .map(|(reference, evidence)| (reference.clone(), evidence.oid.clone())),
            );
            Ok(PreparedImport {
                targets,
                semantic: CanonicalSemantic::from_value(self.semantic.clone())?,
            })
        }

        fn prepare_local_source(
            &self,
            repository: &Path,
            source: &SourceEvidence,
            generation_id: &str,
        ) -> Result<PreparedImport> {
            self.prepare_file_source(repository, source, generation_id)
        }

        fn prepare_current_source(
            &self,
            repository: &Path,
            source: &SourceEvidence,
        ) -> Result<PreparedImport> {
            self.prepare_file_source(repository, source, source.fingerprint())
        }

        fn file_source_is_newer(
            &self,
            _repository: &Path,
            _source: &SourceEvidence,
        ) -> Result<bool> {
            Ok(false)
        }

        fn read_target_semantic(
            &self,
            repository: &Path,
            targets: &BTreeMap<String, String>,
        ) -> Result<CanonicalSemantic> {
            let checkpoint = targets
                .get(CHECKPOINT)
                .ok_or_else(|| anyhow::anyhow!("checkpoint target is missing"))?;
            let output = Command::new("git")
                .current_dir(repository)
                .args(["show", &format!("{checkpoint}:state.json")])
                .output()?;
            ensure_git_success(&output, "read object-backed target state")?;
            CanonicalSemantic::from_value(serde_json::from_slice(&output.stdout)?)
        }
    }

    struct PointerAdvancingImporter {
        inner: ObjectImporter,
        publications: Vec<(PathBuf, ReconciliationJournal)>,
        next: std::cell::Cell<usize>,
    }

    impl HistoricalImporter for PointerAdvancingImporter {
        fn snapshot_source_refs(
            &self,
            repository: &Path,
            source: &SourceEvidence,
        ) -> Result<BTreeMap<String, String>> {
            self.inner.snapshot_source_refs(repository, source)
        }

        fn prepare_file_source(
            &self,
            repository: &Path,
            source: &SourceEvidence,
            generation_id: &str,
        ) -> Result<PreparedImport> {
            self.inner
                .prepare_file_source(repository, source, generation_id)
        }

        fn prepare_local_source(
            &self,
            repository: &Path,
            source: &SourceEvidence,
            generation_id: &str,
        ) -> Result<PreparedImport> {
            self.inner
                .prepare_local_source(repository, source, generation_id)
        }

        fn prepare_current_source(
            &self,
            repository: &Path,
            source: &SourceEvidence,
        ) -> Result<PreparedImport> {
            self.inner.prepare_current_source(repository, source)
        }

        fn file_source_is_newer(&self, repository: &Path, source: &SourceEvidence) -> Result<bool> {
            self.inner.file_source_is_newer(repository, source)
        }

        fn read_target_semantic(
            &self,
            repository: &Path,
            targets: &BTreeMap<String, String>,
        ) -> Result<CanonicalSemantic> {
            let index = self.next.get();
            if let Some((publisher, journal)) = self.publications.get(index) {
                let mut args = vec![
                    "push".to_string(),
                    "--atomic".to_string(),
                    "--force".to_string(),
                    "origin".to_string(),
                ];
                args.extend(
                    journal
                        .descriptor
                        .targets
                        .iter()
                        .map(|(reference, target)| format!("{}:{reference}", target.oid)),
                );
                args.extend(
                    journal
                        .descriptor
                        .targets
                        .values()
                        .chain(journal.descriptor.archives.values())
                        .map(|target| format!("{}:{}", target.oid, target.immutable_ref)),
                );
                args.push(format!(
                    "{}:{}",
                    journal.descriptor_oid,
                    descriptor_immutable_ref(&journal.descriptor.generation_id)
                ));
                args.push(format!("{}:{GENERATION_REF}", journal.descriptor_oid));
                let borrowed = args.iter().map(String::as_str).collect::<Vec<_>>();
                git(publisher, &borrowed);
                self.next.set(index + 1);
            }
            self.inner.read_target_semantic(repository, targets)
        }
    }

    struct Fixture {
        _remote: TempDir,
        repository: TempDir,
        journal: PathBuf,
        format: RepositoryFormat,
    }

    fn git(repository: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(repository)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn init_repository(path: &Path) {
        git(path, &["init", "-b", "main"]);
        git(path, &["config", "user.name", "Test"]);
        git(path, &["config", "user.email", "test@crosslink"]);
        git(path, &["config", "commit.gpgsign", "false"]);
    }

    fn state_commit(repository: &Path, value: &Value, message: &str) -> Result<String> {
        let bytes = serde_json::to_vec(value)?;
        let blob = hash_object(repository, &bytes)?;
        let tree = run_git_input(
            repository,
            &["mktree"],
            format!("100644 blob {blob}\tstate.json\n").as_bytes(),
        )?;
        let output = Command::new("git")
            .current_dir(repository)
            .args([
                "-c",
                "commit.gpgsign=false",
                "commit-tree",
                tree.trim(),
                "-m",
                message,
            ])
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@crosslink")
            .env("GIT_AUTHOR_DATE", "2000-01-01T00:00:00Z")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@crosslink")
            .env("GIT_COMMITTER_DATE", "2000-01-01T00:00:00Z")
            .output()?;
        ensure_git_success(&output, "create state commit")?;
        nonempty_stdout(&output, "create state commit")
    }

    fn child_state_commit(
        repository: &Path,
        parent: &str,
        value: &Value,
        message: &str,
    ) -> Result<String> {
        let bytes = serde_json::to_vec(value)?;
        let blob = hash_object(repository, &bytes)?;
        let tree = run_git_input(
            repository,
            &["mktree"],
            format!("100644 blob {blob}\tstate.json\n").as_bytes(),
        )?;
        let output = Command::new("git")
            .current_dir(repository)
            .args([
                "-c",
                "commit.gpgsign=false",
                "commit-tree",
                tree.trim(),
                "-p",
                parent,
                "-m",
                message,
            ])
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@crosslink")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@crosslink")
            .output()?;
        ensure_git_success(&output, "create child state commit")?;
        nonempty_stdout(&output, "create child state commit")
    }

    fn fixture(shared: SharedStoreFormat) -> Fixture {
        let remote = tempfile::tempdir().unwrap();
        git(remote.path(), &["init", "--bare"]);
        let repository = tempfile::tempdir().unwrap();
        init_repository(repository.path());
        git(
            repository.path(),
            &["remote", "add", "origin", remote.path().to_str().unwrap()],
        );
        for reference in source_ref_names(&shared) {
            let oid = state_commit(
                repository.path(),
                &serde_json::json!({"source": reference}),
                "historical source",
            )
            .unwrap();
            update_ref(repository.path(), &reference, &oid).unwrap();
            git(
                repository.path(),
                &["push", "origin", &format!("{reference}:{reference}")],
            );
        }
        let journal = repository.path().join("reconciliation-journal.json");
        Fixture {
            _remote: remote,
            repository,
            journal,
            format: RepositoryFormat {
                local_database: LocalDatabaseFormat::Missing,
                shared_store: shared,
            },
        }
    }

    fn v2_fixture() -> Fixture {
        fixture(SharedStoreFormat::V2 {
            refs: vec![V2.to_string()],
        })
    }

    fn second_v2_fixture(first: &Fixture) -> Fixture {
        let repository = tempfile::tempdir().unwrap();
        init_repository(repository.path());
        git(
            repository.path(),
            &[
                "remote",
                "add",
                "origin",
                first._remote.path().to_str().unwrap(),
            ],
        );
        git(
            repository.path(),
            &["fetch", "origin", &format!("+{V2}:{V2}")],
        );
        Fixture {
            _remote: tempfile::tempdir().unwrap(),
            journal: repository.path().join("reconciliation-journal.json"),
            repository,
            format: first.format.clone(),
        }
    }

    fn reconcile_fixture(
        fixture: &Fixture,
        importer: &ObjectImporter,
    ) -> Result<PublicationOutcome> {
        RepositoryReconciler::new(
            fixture.repository.path(),
            fixture.journal.clone(),
            "origin",
            importer,
        )
        .reconcile(fixture.format.clone())
    }

    fn remote_oid(fixture: &Fixture, reference: &str) -> Option<String> {
        remote_ref_oid(fixture.repository.path(), "origin", reference).unwrap()
    }

    fn prepare_journal(fixture: &Fixture, importer: &ObjectImporter) -> ReconciliationJournal {
        let failure = RepositoryReconciler::new(
            fixture.repository.path(),
            fixture.journal.clone(),
            "origin",
            importer,
        )
        .with_failpoint(Failpoint {
            transition: Transition::Publish,
            position: TransitionPosition::Before,
            occurrence: 1,
        })
        .reconcile(fixture.format.clone());
        assert!(failure.is_err());
        let JournalRecord::Generation(journal) =
            read_journal(fixture.repository.path(), &fixture.journal)
                .unwrap()
                .unwrap()
        else {
            panic!("expected prepared generation journal");
        };
        journal
    }

    #[test]
    fn pins_every_historical_family_with_object_evidence() {
        let cases = vec![
            SharedStoreFormat::Absent,
            SharedStoreFormat::LegacyLocks {
                refs: vec![LEGACY.to_string()],
            },
            SharedStoreFormat::V2 {
                refs: vec![V2.to_string()],
            },
            SharedStoreFormat::HiddenV3 {
                refs: vec![HIDDEN_META.to_string(), HIDDEN_CHECKPOINT.to_string()],
            },
            SharedStoreFormat::VisibleV3 {
                refs: vec![META.to_string(), CHECKPOINT.to_string()],
            },
            SharedStoreFormat::Mixed {
                families: vec![
                    super::super::SharedStoreFamily::V2,
                    super::super::SharedStoreFamily::VisibleV3,
                ],
                refs: vec![V2.to_string(), META.to_string(), CHECKPOINT.to_string()],
            },
        ];
        for shared in cases {
            let fixture = fixture(shared);
            let evidence = pin_source(fixture.repository.path(), fixture.format.clone()).unwrap();
            for item in evidence.refs.values() {
                assert_eq!(item.oid.len(), 40);
                assert_eq!(item.tree_oid.len(), 40);
            }
            assert_eq!(
                evidence,
                pin_source(fixture.repository.path(), fixture.format).unwrap()
            );
        }
    }

    #[test]
    fn pins_local_only_database_fingerprint() {
        let mut fixture = fixture(SharedStoreFormat::Absent);
        fixture.format.local_database = LocalDatabaseFormat::Sqlite {
            version: 18,
            schema_fingerprint: "schema".to_string(),
            issue_count: Some(3),
            size_bytes: 2048,
        };
        let evidence = pin_source(fixture.repository.path(), fixture.format).unwrap();
        assert_eq!(
            evidence.local_fingerprint.as_deref(),
            Some("sqlite:18:schema:Some(3):2048")
        );
    }

    #[test]
    fn object_backed_historical_families_publish_or_block_without_source_loss() {
        let cases = [
            SharedStoreFormat::LegacyLocks {
                refs: vec![LEGACY.to_string()],
            },
            SharedStoreFormat::HiddenV3 {
                refs: vec![HIDDEN_META.to_string(), HIDDEN_CHECKPOINT.to_string()],
            },
            SharedStoreFormat::VisibleV3 {
                refs: vec![META.to_string(), CHECKPOINT.to_string()],
            },
            SharedStoreFormat::Mixed {
                families: vec![
                    super::super::SharedStoreFamily::V2,
                    super::super::SharedStoreFamily::VisibleV3,
                ],
                refs: vec![V2.to_string(), CHECKPOINT.to_string()],
            },
        ];
        for (index, shared) in cases.into_iter().enumerate() {
            let fixture = fixture(shared);
            let importer = ObjectImporter::new(&format!("family-{index}"));
            let source_before = source_ref_names(&fixture.format.shared_store)
                .into_iter()
                .map(|name| {
                    let oid = local_ref_oid(fixture.repository.path(), &name)
                        .unwrap()
                        .unwrap();
                    (name, oid)
                })
                .collect::<BTreeMap<_, _>>();
            let outcome = reconcile_fixture(&fixture, &importer).unwrap();
            assert!(matches!(outcome, PublicationOutcome::Published { .. }));
            let descriptor_oid = remote_oid(&fixture, GENERATION_REF).unwrap();
            let descriptor = read_descriptor(fixture.repository.path(), &descriptor_oid).unwrap();
            for (name, oid) in source_before {
                assert_eq!(descriptor.archives[&name].oid, oid);
                assert_eq!(
                    remote_oid(&fixture, &descriptor.archives[&name].immutable_ref),
                    Some(oid)
                );
            }
        }

        let fixture = fixture(SharedStoreFormat::Unreadable {
            reason: "malformed ref object".to_string(),
        });
        let outcome = reconcile_fixture(&fixture, &ObjectImporter::new("corrupt")).unwrap();
        assert!(
            matches!(outcome, PublicationOutcome::BlockedCorrupt { .. }),
            "unexpected corrupt journal outcome: {outcome:?}"
        );
        assert!(remote_oid(&fixture, GENERATION_REF).is_none());
    }

    #[test]
    fn atomic_publication_has_one_commit_point_and_preserves_archive() {
        let fixture = v2_fixture();
        let importer = ObjectImporter::new("atomic");
        let source_tip = local_ref_oid(fixture.repository.path(), V2)
            .unwrap()
            .unwrap();
        let outcome = reconcile_fixture(&fixture, &importer).unwrap();
        assert!(matches!(
            outcome,
            PublicationOutcome::Published { atomic: true, .. }
        ));
        let descriptor_oid = remote_oid(&fixture, GENERATION_REF).unwrap();
        let descriptor = read_descriptor(fixture.repository.path(), &descriptor_oid).unwrap();
        assert_eq!(
            remote_oid(&fixture, CHECKPOINT),
            Some(descriptor.targets[CHECKPOINT].oid.clone())
        );
        assert_eq!(
            remote_oid(&fixture, META),
            Some(descriptor.targets[META].oid.clone())
        );
        assert_eq!(
            remote_oid(&fixture, &descriptor.archives[V2].immutable_ref),
            Some(source_tip)
        );
        assert!(remote_oid(&fixture, V2).is_none());
        assert!(!fixture.journal.exists());
        assert!(matches!(
            reconcile_fixture(&fixture, &importer).unwrap(),
            PublicationOutcome::ReadyCurrent { .. }
        ));
    }

    #[test]
    fn explicit_unsupported_atomic_uses_generation_pointer_fallback() {
        let fixture = v2_fixture();
        let importer = ObjectImporter::new("fallback");
        git(
            fixture._remote.path(),
            &["config", "receive.advertiseAtomic", "false"],
        );
        let outcome = RepositoryReconciler::new(
            fixture.repository.path(),
            fixture.journal.clone(),
            "origin",
            &importer,
        )
        .reconcile(fixture.format.clone())
        .unwrap();
        assert!(matches!(
            outcome,
            PublicationOutcome::Published { atomic: false, .. }
        ));
        let descriptor_oid = remote_oid(&fixture, GENERATION_REF).unwrap();
        let descriptor = read_descriptor(fixture.repository.path(), &descriptor_oid).unwrap();
        assert_eq!(
            remote_oid(&fixture, CHECKPOINT),
            Some(descriptor.targets[CHECKPOINT].oid.clone())
        );
        assert_eq!(
            remote_oid(&fixture, META),
            Some(descriptor.targets[META].oid.clone())
        );
    }

    #[test]
    fn corrupt_immutable_remote_evidence_blocks_current_adoption() {
        let fixture = v2_fixture();
        let importer = ObjectImporter::new("corrupt-evidence");
        reconcile_fixture(&fixture, &importer).unwrap();
        let descriptor_oid = remote_oid(&fixture, GENERATION_REF).unwrap();
        let descriptor = read_descriptor(fixture.repository.path(), &descriptor_oid).unwrap();
        let target_ref = descriptor.targets[CHECKPOINT].immutable_ref.clone();
        let corrupt = state_commit(
            fixture.repository.path(),
            &serde_json::json!({"corrupt": true}),
            "corrupt immutable evidence",
        )
        .unwrap();
        git(
            fixture.repository.path(),
            &[
                "push",
                "--force",
                "origin",
                &format!("{corrupt}:{target_ref}"),
            ],
        );
        let outcome = reconcile_fixture(&fixture, &importer).unwrap();
        assert!(matches!(outcome, PublicationOutcome::BlockedCorrupt { .. }));
        assert_eq!(remote_oid(&fixture, GENERATION_REF), Some(descriptor_oid));
        assert_eq!(
            local_ref_oid(fixture.repository.path(), V2).unwrap(),
            Some(descriptor.source.refs[V2].authority_oid.clone())
        );
    }

    #[test]
    fn unreachable_remote_waits_without_mutating_authority() {
        let fixture = v2_fixture();
        let importer = ObjectImporter::new("offline");
        git(
            fixture.repository.path(),
            &[
                "remote",
                "set-url",
                "origin",
                "/definitely/missing/crosslink.git",
            ],
        );
        let before = local_authority_refs(fixture.repository.path()).unwrap();
        let outcome = reconcile_fixture(&fixture, &importer).unwrap();
        assert!(matches!(
            outcome,
            PublicationOutcome::WaitingForRemote { .. }
        ));
        assert_eq!(
            before,
            local_authority_refs(fixture.repository.path()).unwrap()
        );
    }

    #[test]
    fn late_historical_write_blocks_resume_and_preserves_both_states() {
        let fixture = v2_fixture();
        let importer = ObjectImporter::new("late");
        let failpoint = Failpoint {
            transition: Transition::Publish,
            position: TransitionPosition::Before,
            occurrence: 1,
        };
        let error = RepositoryReconciler::new(
            fixture.repository.path(),
            fixture.journal.clone(),
            "origin",
            &importer,
        )
        .with_failpoint(failpoint)
        .reconcile(fixture.format.clone())
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("injected reconciliation failure"));
        let old_tip = local_ref_oid(fixture.repository.path(), V2)
            .unwrap()
            .unwrap();
        let new_tip = state_commit(
            fixture.repository.path(),
            &serde_json::json!({"late": true}),
            "late legacy write",
        )
        .unwrap();
        update_ref(fixture.repository.path(), V2, &new_tip).unwrap();
        let outcome = reconcile_fixture(&fixture, &importer).unwrap();
        assert!(matches!(outcome, PublicationOutcome::BlockedCorrupt { .. }));
        assert_eq!(
            local_ref_oid(fixture.repository.path(), V2).unwrap(),
            Some(new_tip)
        );
        let record = read_journal(fixture.repository.path(), &fixture.journal)
            .unwrap()
            .unwrap();
        let JournalRecord::Generation(journal) = record else {
            panic!("expected prepared generation journal");
        };
        assert_eq!(journal.descriptor.source.refs[V2].oid, old_tip);
    }

    #[test]
    fn committed_late_legacy_descendant_publishes_a_new_generation() {
        let fixture = v2_fixture();
        let importer = ObjectImporter::new("late-descendant");
        let first = reconcile_fixture(&fixture, &importer).unwrap();
        let first_generation = match first {
            PublicationOutcome::Published { generation_id, .. } => generation_id,
            outcome => panic!("unexpected first outcome: {outcome:?}"),
        };
        let old_tip = local_ref_oid(fixture.repository.path(), V2)
            .unwrap()
            .unwrap();
        let new_tip = child_state_commit(
            fixture.repository.path(),
            &old_tip,
            &serde_json::json!({"late": "committed"}),
            "late legacy descendant",
        )
        .unwrap();
        update_ref(fixture.repository.path(), V2, &new_tip).unwrap();
        git(
            fixture.repository.path(),
            &["push", "origin", &format!("{V2}:{V2}")],
        );
        let second = reconcile_fixture(&fixture, &importer).unwrap();
        let second_generation = match second {
            PublicationOutcome::Published { generation_id, .. } => generation_id,
            outcome => panic!("unexpected second outcome: {outcome:?}"),
        };
        assert_ne!(first_generation, second_generation);
        let descriptor_oid = remote_oid(&fixture, GENERATION_REF).unwrap();
        let descriptor = read_descriptor(fixture.repository.path(), &descriptor_oid).unwrap();
        assert_eq!(descriptor.source.refs[V2].authority_oid, new_tip);
        assert_eq!(
            remote_oid(&fixture, &descriptor.archives[V2].immutable_ref),
            Some(new_tip)
        );
    }

    #[test]
    fn contradictory_same_identity_cannot_supersede_without_live_base() {
        let fixture = v2_fixture();
        let importer = ObjectImporter::new("contradictory-winner");
        let first = reconcile_fixture(&fixture, &importer).unwrap();
        assert!(matches!(first, PublicationOutcome::Published { .. }));
        let winner = remote_oid(&fixture, GENERATION_REF).unwrap();
        let checkpoint = remote_oid(&fixture, CHECKPOINT).unwrap();
        let old_tip = local_ref_oid(fixture.repository.path(), V2)
            .unwrap()
            .unwrap();
        let new_tip = child_state_commit(
            fixture.repository.path(),
            &old_tip,
            &serde_json::json!({"late": "conflicting"}),
            "conflicting legacy descendant",
        )
        .unwrap();
        update_ref(fixture.repository.path(), V2, &new_tip).unwrap();
        git(
            fixture.repository.path(),
            &["push", "origin", &format!("{V2}:{V2}")],
        );
        let conflicting = ObjectImporter {
            semantic: serde_json::json!({
                "issues": {"issue-1": {"title": "contradicted"}},
                "comments": {},
                "milestones": {},
                "locks": {"issue-1": "agent-a"},
                "trust": {"allowed": ["agent-a"]}
            }),
            salt: "contradictory-candidate".to_string(),
        };
        let outcome = reconcile_fixture(&fixture, &conflicting).unwrap();
        assert!(matches!(outcome, PublicationOutcome::BlockedCorrupt { .. }));
        assert_eq!(remote_oid(&fixture, GENERATION_REF), Some(winner));
        assert_eq!(remote_oid(&fixture, CHECKPOINT), Some(checkpoint));
        assert_eq!(
            local_ref_oid(fixture.repository.path(), V2).unwrap(),
            Some(new_tip)
        );
    }

    #[test]
    fn newly_appearing_legacy_source_supersedes_current_generation() {
        let fixture = fixture(SharedStoreFormat::Absent);
        let importer = ObjectImporter::new("new-late-legacy");
        let first = reconcile_fixture(&fixture, &importer).unwrap();
        let first_generation = match first {
            PublicationOutcome::Published { generation_id, .. } => generation_id,
            outcome => panic!("unexpected first outcome: {outcome:?}"),
        };
        let late_tip = state_commit(
            fixture.repository.path(),
            &serde_json::json!({"late": "legacy"}),
            "new late legacy source",
        )
        .unwrap();
        update_ref(fixture.repository.path(), V2, &late_tip).unwrap();
        git(
            fixture.repository.path(),
            &["push", "origin", &format!("{V2}:{V2}")],
        );
        let observed = RepositoryFormat {
            local_database: LocalDatabaseFormat::Missing,
            shared_store: SharedStoreFormat::Mixed {
                families: vec![
                    super::super::SharedStoreFamily::V2,
                    super::super::SharedStoreFamily::VisibleV3,
                ],
                refs: vec![V2.to_string(), META.to_string(), CHECKPOINT.to_string()],
            },
        };
        let second = RepositoryReconciler::new(
            fixture.repository.path(),
            fixture.journal.clone(),
            "origin",
            &importer,
        )
        .reconcile(observed)
        .unwrap();
        let second_generation = match second {
            PublicationOutcome::Published { generation_id, .. } => generation_id,
            outcome => panic!("unexpected second outcome: {outcome:?}"),
        };
        assert_ne!(first_generation, second_generation);
        let descriptor_oid = remote_oid(&fixture, GENERATION_REF).unwrap();
        let descriptor = read_descriptor(fixture.repository.path(), &descriptor_oid).unwrap();
        assert_eq!(descriptor.source.refs[V2].authority_oid, late_tip);
    }

    #[test]
    fn prepared_two_clone_race_has_one_publisher_and_one_verified_adopter() {
        let first = v2_fixture();
        let second = second_v2_fixture(&first);
        let importer_a = ObjectImporter::new("racer-a");
        let importer_b = ObjectImporter::new("racer-b");
        let failpoint = Failpoint {
            transition: Transition::Publish,
            position: TransitionPosition::Before,
            occurrence: 1,
        };
        for (fixture, importer) in [(&first, &importer_a), (&second, &importer_b)] {
            RepositoryReconciler::new(
                fixture.repository.path(),
                fixture.journal.clone(),
                "origin",
                importer,
            )
            .with_failpoint(failpoint)
            .reconcile(fixture.format.clone())
            .unwrap_err();
        }
        let barrier = std::sync::Barrier::new(3);
        let (first_outcome, second_outcome) = std::thread::scope(|scope| {
            let first_handle = scope.spawn(|| {
                barrier.wait();
                reconcile_fixture(&first, &importer_a).unwrap()
            });
            let second_handle = scope.spawn(|| {
                barrier.wait();
                reconcile_fixture(&second, &importer_b).unwrap()
            });
            barrier.wait();
            (first_handle.join().unwrap(), second_handle.join().unwrap())
        });
        let mut published_generation = None;
        let mut adopted_generation = None;
        for outcome in [first_outcome, second_outcome] {
            match outcome {
                PublicationOutcome::Published { generation_id, .. } => {
                    assert!(published_generation.replace(generation_id).is_none());
                }
                PublicationOutcome::Adopted { generation_id } => {
                    assert!(adopted_generation.replace(generation_id).is_none());
                }
                outcome => panic!("unexpected race outcome: {outcome:?}"),
            }
        }
        let published_generation = published_generation.unwrap();
        let adopted_generation = adopted_generation.unwrap();
        assert_eq!(published_generation, adopted_generation);
    }

    #[test]
    fn adopter_publishes_new_local_agent_before_materializing_winner() {
        let first = v2_fixture();
        let second = second_v2_fixture(&first);
        let importer_a = ObjectImporter::new("adopt-live-a");
        let importer_b = ObjectImporter::new("adopt-live-b");
        prepare_journal(&first, &importer_a);
        prepare_journal(&second, &importer_b);
        let published = reconcile_fixture(&first, &importer_a).unwrap();
        assert!(matches!(published, PublicationOutcome::Published { .. }));
        let agent_ref = "refs/heads/crosslink/agents/local-adopter";
        let local_tip = state_commit(
            second.repository.path(),
            &serde_json::json!({"local": "adopter"}),
            "local adopter agent",
        )
        .unwrap();
        update_ref(second.repository.path(), agent_ref, &local_tip).unwrap();
        let adopted = reconcile_fixture(&second, &importer_b).unwrap();
        assert!(
            matches!(adopted, PublicationOutcome::Adopted { .. }),
            "unexpected adoption outcome: {adopted:?}"
        );
        assert_eq!(
            local_ref_oid(second.repository.path(), agent_ref).unwrap(),
            Some(local_tip.clone())
        );
        assert_eq!(remote_oid(&second, agent_ref), Some(local_tip));
    }

    #[test]
    fn fallback_pointer_window_is_completed_by_concurrent_adopter() {
        let first = v2_fixture();
        let second = second_v2_fixture(&first);
        let first_importer = ObjectImporter::new("pointer-publisher");
        let second_importer = ObjectImporter::new("pointer-adopter");
        prepare_journal(&second, &second_importer);
        let interrupted = RepositoryReconciler::new(
            first.repository.path(),
            first.journal.clone(),
            "origin",
            &first_importer,
        )
        .with_atomic_capability(AtomicCapability::UnsupportedForTest)
        .with_failpoint(Failpoint {
            transition: Transition::Publish,
            position: TransitionPosition::After,
            occurrence: 1,
        })
        .reconcile(first.format.clone());
        assert!(interrupted.is_err());
        assert!(remote_oid(&first, GENERATION_REF).is_some());
        assert!(remote_oid(&first, CHECKPOINT).is_none());
        assert!(remote_oid(&first, META).is_none());
        let adopted = reconcile_fixture(&second, &second_importer).unwrap();
        assert!(matches!(adopted, PublicationOutcome::Adopted { .. }));
        assert!(remote_oid(&first, CHECKPOINT).is_some());
        assert!(remote_oid(&first, META).is_some());
        assert!(remote_oid(&first, V2).is_none());
        let resumed = reconcile_fixture(&first, &first_importer).unwrap();
        assert!(matches!(resumed, PublicationOutcome::Published { .. }));
    }

    #[test]
    fn fallback_pointer_window_is_completed_by_ready_observer_without_journal() {
        let first = v2_fixture();
        let observer = second_v2_fixture(&first);
        let first_importer = ObjectImporter::new("pointer-owner");
        let observer_importer = ObjectImporter::new("pointer-observer");
        let interrupted = RepositoryReconciler::new(
            first.repository.path(),
            first.journal.clone(),
            "origin",
            &first_importer,
        )
        .with_atomic_capability(AtomicCapability::UnsupportedForTest)
        .with_failpoint(Failpoint {
            transition: Transition::Publish,
            position: TransitionPosition::After,
            occurrence: 1,
        })
        .reconcile(first.format.clone());
        assert!(interrupted.is_err());
        assert!(read_journal(observer.repository.path(), &observer.journal)
            .unwrap()
            .is_none());
        let observed = reconcile_fixture(&observer, &observer_importer).unwrap();
        assert!(matches!(observed, PublicationOutcome::ReadyCurrent { .. }));
        assert!(remote_oid(&first, CHECKPOINT).is_some());
        assert!(remote_oid(&first, META).is_some());
        assert!(remote_oid(&first, V2).is_none());
    }

    #[test]
    fn every_precommit_transition_preserves_authority() {
        let cases = [
            (Transition::Journal, TransitionPosition::Before),
            (Transition::Journal, TransitionPosition::After),
            (Transition::Prepare, TransitionPosition::Before),
            (Transition::Prepare, TransitionPosition::After),
            (Transition::Verify, TransitionPosition::Before),
            (Transition::Verify, TransitionPosition::After),
            (Transition::Archive, TransitionPosition::Before),
            (Transition::Archive, TransitionPosition::After),
            (Transition::Publish, TransitionPosition::Before),
        ];
        for (transition, position) in cases {
            let fixture = v2_fixture();
            let importer = ObjectImporter::new("failpoint");
            let before = local_authority_refs(fixture.repository.path()).unwrap();
            let remote_before = [GENERATION_REF, CHECKPOINT, META, V2, LEGACY]
                .into_iter()
                .map(|reference| (reference, remote_oid(&fixture, reference)))
                .collect::<BTreeMap<_, _>>();
            let failpoint = Failpoint {
                transition,
                position,
                occurrence: 1,
            };
            let result = RepositoryReconciler::new(
                fixture.repository.path(),
                fixture.journal.clone(),
                "origin",
                &importer,
            )
            .with_atomic_capability(AtomicCapability::UnsupportedForTest)
            .with_failpoint(failpoint)
            .reconcile(fixture.format.clone());
            assert!(result.is_err(), "{transition:?} {position:?} did not fail");
            assert_eq!(
                before,
                local_authority_refs(fixture.repository.path()).unwrap(),
                "{transition:?} {position:?} changed authority"
            );
            assert_eq!(
                remote_before,
                [GENERATION_REF, CHECKPOINT, META, V2, LEGACY]
                    .into_iter()
                    .map(|reference| (reference, remote_oid(&fixture, reference)))
                    .collect::<BTreeMap<_, _>>(),
                "{transition:?} {position:?} changed remote authority"
            );
            assert!(local_ref_oid(fixture.repository.path(), V2)
                .unwrap()
                .is_some());
            let resumed = RepositoryReconciler::new(
                fixture.repository.path(),
                fixture.journal.clone(),
                "origin",
                &importer,
            )
            .with_atomic_capability(AtomicCapability::UnsupportedForTest)
            .reconcile(fixture.format.clone())
            .unwrap();
            assert!(matches!(
                resumed,
                PublicationOutcome::Published { atomic: false, .. }
            ));
            assert!(!fixture.journal.exists());
        }
    }

    #[test]
    fn postcommit_alias_failure_resumes_forward() {
        let fixture = v2_fixture();
        let importer = ObjectImporter::new("postcommit");
        let result = RepositoryReconciler::new(
            fixture.repository.path(),
            fixture.journal.clone(),
            "origin",
            &importer,
        )
        .with_atomic_capability(AtomicCapability::UnsupportedForTest)
        .with_failpoint(Failpoint {
            transition: Transition::Alias,
            position: TransitionPosition::Before,
            occurrence: 1,
        })
        .reconcile(fixture.format.clone());
        assert!(result.is_err());
        assert!(remote_oid(&fixture, GENERATION_REF).is_some());
        let outcome = reconcile_fixture(&fixture, &importer).unwrap();
        assert!(matches!(
            outcome,
            PublicationOutcome::Published { atomic: false, .. }
        ));
        assert_eq!(
            remote_oid(&fixture, CHECKPOINT),
            local_ref_oid(fixture.repository.path(), CHECKPOINT).unwrap()
        );
    }

    #[test]
    fn every_journal_failpoint_resumes_idempotently() {
        for position in [TransitionPosition::Before, TransitionPosition::After] {
            for occurrence in 1..=7 {
                let fixture = v2_fixture();
                let importer = ObjectImporter::new(&format!("journal-{position:?}-{occurrence}"));
                let result = RepositoryReconciler::new(
                    fixture.repository.path(),
                    fixture.journal.clone(),
                    "origin",
                    &importer,
                )
                .with_atomic_capability(AtomicCapability::UnsupportedForTest)
                .with_failpoint(Failpoint {
                    transition: Transition::Journal,
                    position,
                    occurrence,
                })
                .reconcile(fixture.format.clone());
                assert!(
                    result.is_err(),
                    "journal {position:?} {occurrence} did not fail"
                );
                let resumed = RepositoryReconciler::new(
                    fixture.repository.path(),
                    fixture.journal.clone(),
                    "origin",
                    &importer,
                )
                .with_atomic_capability(AtomicCapability::UnsupportedForTest)
                .reconcile(fixture.format.clone())
                .unwrap();
                assert!(matches!(
                    resumed,
                    PublicationOutcome::Published { atomic: false, .. }
                ));
                assert!(!fixture.journal.exists());
            }
        }
    }

    #[test]
    fn postcommit_publish_and_alias_failpoints_resume_same_generation() {
        let cases = [
            (Transition::Publish, TransitionPosition::After),
            (Transition::Alias, TransitionPosition::Before),
            (Transition::Alias, TransitionPosition::After),
        ];
        for (transition, position) in cases {
            let fixture = v2_fixture();
            let importer = ObjectImporter::new(&format!("post-{transition:?}-{position:?}"));
            let result = RepositoryReconciler::new(
                fixture.repository.path(),
                fixture.journal.clone(),
                "origin",
                &importer,
            )
            .with_atomic_capability(AtomicCapability::UnsupportedForTest)
            .with_failpoint(Failpoint {
                transition,
                position,
                occurrence: 1,
            })
            .reconcile(fixture.format.clone());
            assert!(result.is_err());
            let committed = remote_oid(&fixture, GENERATION_REF).unwrap();
            let resumed = RepositoryReconciler::new(
                fixture.repository.path(),
                fixture.journal.clone(),
                "origin",
                &importer,
            )
            .with_atomic_capability(AtomicCapability::UnsupportedForTest)
            .reconcile(fixture.format.clone())
            .unwrap();
            assert!(matches!(resumed, PublicationOutcome::Published { .. }));
            assert_eq!(remote_oid(&fixture, GENERATION_REF), Some(committed));
        }

        for (transition, position) in cases {
            let fixture = v2_fixture();
            let importer = ObjectImporter::new(&format!("post-atomic-{transition:?}-{position:?}"));
            let result = RepositoryReconciler::new(
                fixture.repository.path(),
                fixture.journal.clone(),
                "origin",
                &importer,
            )
            .with_failpoint(Failpoint {
                transition,
                position,
                occurrence: 1,
            })
            .reconcile(fixture.format.clone());
            assert!(result.is_err());
            let committed = remote_oid(&fixture, GENERATION_REF).unwrap();
            let resumed = reconcile_fixture(&fixture, &importer).unwrap();
            assert!(matches!(
                resumed,
                PublicationOutcome::Published { atomic: true, .. }
            ));
            assert_eq!(remote_oid(&fixture, GENERATION_REF), Some(committed));
        }
    }

    #[test]
    fn adopt_failpoints_resume_with_independent_verification() {
        for position in [TransitionPosition::Before, TransitionPosition::After] {
            let first = v2_fixture();
            let second_repository = tempfile::tempdir().unwrap();
            init_repository(second_repository.path());
            git(
                second_repository.path(),
                &[
                    "remote",
                    "add",
                    "origin",
                    first._remote.path().to_str().unwrap(),
                ],
            );
            git(
                second_repository.path(),
                &["fetch", "origin", &format!("+{V2}:{V2}")],
            );
            let second = Fixture {
                _remote: tempfile::tempdir().unwrap(),
                journal: second_repository.path().join("reconciliation-journal.json"),
                repository: second_repository,
                format: first.format.clone(),
            };
            let importer_a = ObjectImporter::new("adopt-winner");
            let importer_b = ObjectImporter::new("adopt-loser");
            let prepare_stop = Failpoint {
                transition: Transition::Publish,
                position: TransitionPosition::Before,
                occurrence: 1,
            };
            for (fixture, importer) in [(&first, &importer_a), (&second, &importer_b)] {
                RepositoryReconciler::new(
                    fixture.repository.path(),
                    fixture.journal.clone(),
                    "origin",
                    importer,
                )
                .with_failpoint(prepare_stop)
                .reconcile(fixture.format.clone())
                .unwrap_err();
            }
            reconcile_fixture(&first, &importer_a).unwrap();
            let failed = RepositoryReconciler::new(
                second.repository.path(),
                second.journal.clone(),
                "origin",
                &importer_b,
            )
            .with_failpoint(Failpoint {
                transition: Transition::Adopt,
                position,
                occurrence: 1,
            })
            .reconcile(second.format.clone());
            assert!(failed.is_err(), "adoption failpoint returned {failed:?}");
            let resumed = reconcile_fixture(&second, &importer_b).unwrap();
            assert!(matches!(resumed, PublicationOutcome::Adopted { .. }));
        }
    }

    #[test]
    fn descriptor_validation_rejects_untrusted_structure_and_identity() {
        let fixture = v2_fixture();
        let importer = ObjectImporter::new("descriptor-validation");
        let journal = prepare_journal(&fixture, &importer);
        let descriptor = journal.descriptor;

        let mut malicious = descriptor.clone();
        malicious.targets.insert(
            "refs/heads/main".to_string(),
            TargetRef {
                oid: malicious.targets[CHECKPOINT].oid.clone(),
                immutable_ref: format!(
                    "{GENERATION_ROOT}/{}/targets/main",
                    malicious.generation_id
                ),
            },
        );
        assert!(validate_descriptor_schema(&malicious).is_err());

        let mut malformed_oid = descriptor.clone();
        malformed_oid.targets.get_mut(CHECKPOINT).unwrap().oid = "not-an-oid".to_string();
        assert!(validate_descriptor_schema(&malformed_oid).is_err());

        let mut malformed_ref = descriptor.clone();
        malformed_ref.targets.insert(
            "refs/heads/crosslink/agents/../../main".to_string(),
            descriptor.targets[CHECKPOINT].clone(),
        );
        assert!(validate_descriptor_schema(&malformed_ref).is_err());

        let mut inconsistent_archive = descriptor.clone();
        inconsistent_archive.archives.remove(V2);
        assert!(validate_descriptor_schema(&inconsistent_archive).is_err());

        let mut inconsistent_fingerprint = descriptor.clone();
        inconsistent_fingerprint.source.fingerprint = "0".repeat(64);
        assert!(validate_descriptor_schema(&inconsistent_fingerprint).is_err());

        let mut inconsistent_id = descriptor.clone();
        inconsistent_id.generation_id = "1".repeat(24);
        assert!(validate_descriptor_schema(&inconsistent_id).is_err());

        let wrong_commit = state_commit(
            fixture.repository.path(),
            &serde_json::json!({"wrong": "tree"}),
            "wrong source tree",
        )
        .unwrap();
        let wrong_tree = rev_parse(
            fixture.repository.path(),
            &format!("{wrong_commit}^{{tree}}"),
        )
        .unwrap();
        let mut inconsistent_tree = descriptor.clone();
        inconsistent_tree.source.refs.get_mut(V2).unwrap().tree_oid = wrong_tree.clone();
        assert!(
            validate_descriptor_objects(fixture.repository.path(), &inconsistent_tree).is_err()
        );

        let mut inconsistent_remote_tree = descriptor.clone();
        inconsistent_remote_tree
            .source
            .refs
            .get_mut(V2)
            .unwrap()
            .remote_tree_oid = Some(wrong_tree);
        assert!(
            validate_descriptor_objects(fixture.repository.path(), &inconsistent_remote_tree)
                .is_err()
        );

        let mut value = serde_json::to_value(&descriptor).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("extra".to_string(), serde_json::json!("refs/heads/main"));
        assert!(serde_json::from_value::<GenerationDescriptor>(value).is_err());
    }

    #[test]
    fn corrupt_journal_maps_to_blocked_corrupt_without_remote_mutation() {
        let fixture = v2_fixture();
        let importer = ObjectImporter::new("corrupt-journal");
        prepare_journal(&fixture, &importer);
        let latest = latest_journal_record(&fixture.journal).unwrap().unwrap();
        let mut value: Value = serde_json::from_slice(&fs::read(&latest).unwrap()).unwrap();
        value.as_object_mut().unwrap().insert(
            "unexpected".to_string(),
            serde_json::json!("refs/heads/main"),
        );
        fs::write(&latest, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
        let outcome = reconcile_fixture(&fixture, &importer).unwrap();
        assert!(
            matches!(outcome, PublicationOutcome::BlockedCorrupt { .. }),
            "unexpected corrupt journal outcome: {outcome:?}"
        );
        assert!(remote_oid(&fixture, GENERATION_REF).is_none());
        assert!(remote_oid(&fixture, V2).is_some());
    }

    #[test]
    fn mismatched_preexisting_immutable_ref_blocks_without_overwrite() {
        let fixture = v2_fixture();
        let importer = ObjectImporter::new("immutable-conflict");
        let journal = prepare_journal(&fixture, &importer);
        let immutable = journal.descriptor.targets[CHECKPOINT].immutable_ref.clone();
        let corrupt = state_commit(
            fixture.repository.path(),
            &serde_json::json!({"different": true}),
            "immutable conflict",
        )
        .unwrap();
        git(
            fixture.repository.path(),
            &["push", "origin", &format!("{corrupt}:{immutable}")],
        );
        let outcome = reconcile_fixture(&fixture, &importer).unwrap();
        assert!(matches!(outcome, PublicationOutcome::BlockedCorrupt { .. }));
        assert_eq!(remote_oid(&fixture, &immutable), Some(corrupt));
        assert!(remote_oid(&fixture, GENERATION_REF).is_none());
        assert!(remote_oid(&fixture, V2).is_some());
    }

    #[test]
    fn remote_source_advance_before_publication_blocks_stale_commit() {
        let fixture = v2_fixture();
        let importer = ObjectImporter::new("remote-advance");
        let journal = prepare_journal(&fixture, &importer);
        let original = journal.descriptor.source.refs[V2].remote_oid().unwrap();
        let advanced = child_state_commit(
            fixture.repository.path(),
            original,
            &serde_json::json!({"remote": "advanced"}),
            "remote source advance",
        )
        .unwrap();
        git(
            fixture.repository.path(),
            &["push", "origin", &format!("{advanced}:{V2}")],
        );
        let outcome = reconcile_fixture(&fixture, &importer).unwrap();
        assert!(matches!(outcome, PublicationOutcome::BlockedCorrupt { .. }));
        assert_eq!(remote_oid(&fixture, V2), Some(advanced));
        assert!(remote_oid(&fixture, GENERATION_REF).is_none());
        for archive in journal.descriptor.archives.values() {
            assert!(object_exists(fixture.repository.path(), &archive.oid).unwrap());
        }
    }

    #[test]
    fn live_v3_descendants_and_new_agents_remain_ready() {
        let fixture = v2_fixture();
        let importer = ObjectImporter::new("live-progress");
        reconcile_fixture(&fixture, &importer).unwrap();
        let checkpoint = remote_oid(&fixture, CHECKPOINT).unwrap();
        let advanced = child_state_commit(
            fixture.repository.path(),
            &checkpoint,
            &importer.semantic,
            "live checkpoint advance",
        )
        .unwrap();
        let new_agent = state_commit(
            fixture.repository.path(),
            &serde_json::json!({"agent": "live-agent"}),
            "live agent",
        )
        .unwrap();
        let agent_ref = "refs/heads/crosslink/agents/live-agent";
        git(
            fixture.repository.path(),
            &[
                "push",
                "origin",
                &format!("{advanced}:{CHECKPOINT}"),
                &format!("{new_agent}:{agent_ref}"),
            ],
        );
        let outcome = reconcile_fixture(&fixture, &importer).unwrap();
        assert!(matches!(outcome, PublicationOutcome::ReadyCurrent { .. }));
        assert_eq!(
            local_ref_oid(fixture.repository.path(), CHECKPOINT).unwrap(),
            Some(advanced)
        );
        assert_eq!(
            local_ref_oid(fixture.repository.path(), agent_ref).unwrap(),
            Some(new_agent)
        );
    }

    #[test]
    fn fallback_two_clone_race_has_one_publisher_and_one_verified_adopter() {
        let first = v2_fixture();
        git(
            first._remote.path(),
            &["config", "receive.advertiseAtomic", "false"],
        );
        let second_repository = tempfile::tempdir().unwrap();
        init_repository(second_repository.path());
        git(
            second_repository.path(),
            &[
                "remote",
                "add",
                "origin",
                first._remote.path().to_str().unwrap(),
            ],
        );
        git(
            second_repository.path(),
            &["fetch", "origin", &format!("+{V2}:{V2}")],
        );
        let second = Fixture {
            _remote: tempfile::tempdir().unwrap(),
            journal: second_repository.path().join("reconciliation-journal.json"),
            repository: second_repository,
            format: first.format.clone(),
        };
        let importer_a = ObjectImporter::new("fallback-racer-a");
        let importer_b = ObjectImporter::new("fallback-racer-b");
        prepare_journal(&first, &importer_a);
        prepare_journal(&second, &importer_b);
        let barrier = std::sync::Barrier::new(3);
        let (first_outcome, second_outcome) = std::thread::scope(|scope| {
            let first_handle = scope.spawn(|| {
                barrier.wait();
                reconcile_fixture(&first, &importer_a).unwrap()
            });
            let second_handle = scope.spawn(|| {
                barrier.wait();
                reconcile_fixture(&second, &importer_b).unwrap()
            });
            barrier.wait();
            (first_handle.join().unwrap(), second_handle.join().unwrap())
        });
        let mut published = None;
        let mut adopted = None;
        for outcome in [first_outcome, second_outcome] {
            match outcome {
                PublicationOutcome::Published {
                    generation_id,
                    atomic: false,
                } => assert!(published.replace(generation_id).is_none()),
                PublicationOutcome::Adopted { generation_id } => {
                    assert!(adopted.replace(generation_id).is_none());
                }
                outcome => panic!("unexpected fallback race outcome: {outcome:?}"),
            }
        }
        assert_eq!(published.unwrap(), adopted.unwrap());
    }

    #[test]
    fn every_fallback_archive_iteration_recovers_without_authority_drift() {
        for position in [TransitionPosition::Before, TransitionPosition::After] {
            for occurrence in 1..=6 {
                let fixture = v2_fixture();
                let importer = ObjectImporter::new(&format!("archive-{position:?}-{occurrence}"));
                let before = remote_oid(&fixture, V2);
                let result = RepositoryReconciler::new(
                    fixture.repository.path(),
                    fixture.journal.clone(),
                    "origin",
                    &importer,
                )
                .with_atomic_capability(AtomicCapability::UnsupportedForTest)
                .with_failpoint(Failpoint {
                    transition: Transition::Archive,
                    position,
                    occurrence,
                })
                .reconcile(fixture.format.clone());
                assert!(
                    result.is_err(),
                    "archive {position:?} {occurrence} did not fail"
                );
                assert_eq!(remote_oid(&fixture, GENERATION_REF), None);
                assert_eq!(remote_oid(&fixture, V2), before);
                let resumed = RepositoryReconciler::new(
                    fixture.repository.path(),
                    fixture.journal.clone(),
                    "origin",
                    &importer,
                )
                .with_atomic_capability(AtomicCapability::UnsupportedForTest)
                .reconcile(fixture.format.clone())
                .unwrap();
                assert!(matches!(
                    resumed,
                    PublicationOutcome::Published { atomic: false, .. }
                ));
            }
        }
    }

    #[test]
    fn fallback_preserves_descendants_interleaved_before_each_alias() {
        for occurrence in 1..=2 {
            let fixture = v2_fixture();
            let importer = ObjectImporter::new(&format!("alias-descendant-{occurrence}"));
            let failed = RepositoryReconciler::new(
                fixture.repository.path(),
                fixture.journal.clone(),
                "origin",
                &importer,
            )
            .with_atomic_capability(AtomicCapability::UnsupportedForTest)
            .with_failpoint(Failpoint {
                transition: Transition::Alias,
                position: TransitionPosition::Before,
                occurrence,
            })
            .reconcile(fixture.format.clone());
            assert!(failed.is_err());
            let JournalRecord::Generation(journal) =
                read_journal(fixture.repository.path(), &fixture.journal)
                    .unwrap()
                    .unwrap()
            else {
                panic!("expected fallback journal");
            };
            let (canonical, target) = journal
                .descriptor
                .targets
                .iter()
                .nth(occurrence - 1)
                .unwrap();
            let meta_value = serde_json::json!({"hub_version": 3});
            let value = if canonical == CHECKPOINT {
                &importer.semantic
            } else {
                &meta_value
            };
            let descendant = child_state_commit(
                fixture.repository.path(),
                &target.oid,
                value,
                "concurrent canonical descendant",
            )
            .unwrap();
            git(
                fixture.repository.path(),
                &["push", "origin", &format!("{descendant}:{canonical}")],
            );
            let resumed = RepositoryReconciler::new(
                fixture.repository.path(),
                fixture.journal.clone(),
                "origin",
                &importer,
            )
            .with_atomic_capability(AtomicCapability::UnsupportedForTest)
            .reconcile(fixture.format.clone())
            .unwrap();
            assert!(matches!(
                resumed,
                PublicationOutcome::Published { atomic: false, .. }
            ));
            assert_eq!(remote_oid(&fixture, canonical), Some(descendant));
        }
    }

    #[test]
    fn fallback_preserves_new_agent_interleaved_after_pointer_commit() {
        let fixture = v2_fixture();
        let importer = ObjectImporter::new("alias-new-agent");
        let failed = RepositoryReconciler::new(
            fixture.repository.path(),
            fixture.journal.clone(),
            "origin",
            &importer,
        )
        .with_atomic_capability(AtomicCapability::UnsupportedForTest)
        .with_failpoint(Failpoint {
            transition: Transition::Alias,
            position: TransitionPosition::Before,
            occurrence: 1,
        })
        .reconcile(fixture.format.clone());
        assert!(failed.is_err());
        let agent_ref = "refs/heads/crosslink/agents/new-live-agent";
        let agent_oid = state_commit(
            fixture.repository.path(),
            &serde_json::json!({"agent": "new-live-agent"}),
            "concurrent new agent",
        )
        .unwrap();
        git(
            fixture.repository.path(),
            &["push", "origin", &format!("{agent_oid}:{agent_ref}")],
        );
        let resumed = RepositoryReconciler::new(
            fixture.repository.path(),
            fixture.journal.clone(),
            "origin",
            &importer,
        )
        .with_atomic_capability(AtomicCapability::UnsupportedForTest)
        .reconcile(fixture.format.clone())
        .unwrap();
        assert!(matches!(
            resumed,
            PublicationOutcome::Published { atomic: false, .. }
        ));
        assert_eq!(remote_oid(&fixture, agent_ref), Some(agent_oid.clone()));
        assert_eq!(
            local_ref_oid(fixture.repository.path(), agent_ref).unwrap(),
            Some(agent_oid)
        );
    }

    #[test]
    fn atomic_and_fallback_preserve_agent_descendant_before_commit() {
        let agent_ref = "refs/heads/crosslink/agents/precommit-descendant";
        for fallback in [false, true] {
            let fixture = fixture(SharedStoreFormat::VisibleV3 {
                refs: vec![
                    CHECKPOINT.to_string(),
                    META.to_string(),
                    agent_ref.to_string(),
                ],
            });
            let importer = ObjectImporter::new(&format!("precommit-descendant-{fallback}"));
            let journal = prepare_journal(&fixture, &importer);
            let target = journal.descriptor.targets[agent_ref].oid.clone();
            let descendant = child_state_commit(
                fixture.repository.path(),
                &target,
                &serde_json::json!({"agent": "advanced"}),
                "precommit agent descendant",
            )
            .unwrap();
            git(
                fixture.repository.path(),
                &["push", "origin", &format!("{descendant}:{agent_ref}")],
            );
            let mut reconciler = RepositoryReconciler::new(
                fixture.repository.path(),
                fixture.journal.clone(),
                "origin",
                &importer,
            );
            if fallback {
                reconciler =
                    reconciler.with_atomic_capability(AtomicCapability::UnsupportedForTest);
            }
            let outcome = reconciler.reconcile(fixture.format.clone()).unwrap();
            assert!(matches!(outcome, PublicationOutcome::Published { .. }));
            assert_eq!(remote_oid(&fixture, agent_ref), Some(descendant.clone()));
            assert_eq!(
                local_ref_oid(fixture.repository.path(), agent_ref).unwrap(),
                Some(descendant)
            );
        }
    }

    #[test]
    fn atomic_and_fallback_preserve_new_agent_before_commit() {
        let agent_ref = "refs/heads/crosslink/agents/precommit-new";
        for fallback in [false, true] {
            let fixture = v2_fixture();
            let importer = ObjectImporter::new(&format!("precommit-new-{fallback}"));
            prepare_journal(&fixture, &importer);
            let agent_oid = state_commit(
                fixture.repository.path(),
                &serde_json::json!({"agent": "new"}),
                "precommit new agent",
            )
            .unwrap();
            git(
                fixture.repository.path(),
                &["push", "origin", &format!("{agent_oid}:{agent_ref}")],
            );
            let mut reconciler = RepositoryReconciler::new(
                fixture.repository.path(),
                fixture.journal.clone(),
                "origin",
                &importer,
            );
            if fallback {
                reconciler =
                    reconciler.with_atomic_capability(AtomicCapability::UnsupportedForTest);
            }
            let outcome = reconciler.reconcile(fixture.format.clone()).unwrap();
            assert!(matches!(outcome, PublicationOutcome::Published { .. }));
            assert_eq!(remote_oid(&fixture, agent_ref), Some(agent_oid.clone()));
            assert_eq!(
                local_ref_oid(fixture.repository.path(), agent_ref).unwrap(),
                Some(agent_oid)
            );
        }
    }

    #[test]
    fn ready_observer_follows_multiple_pointer_advances_without_stale_mutation() {
        let first = v2_fixture();
        let second = second_v2_fixture(&first);
        let third = second_v2_fixture(&first);
        let first_importer = ObjectImporter::new("pointer-first");
        let second_importer = ObjectImporter::new("pointer-second");
        let third_importer = ObjectImporter::new("pointer-third");
        let second_journal = prepare_journal(&second, &second_importer);
        let third_journal = prepare_journal(&third, &third_importer);
        let observer = second_v2_fixture(&first);
        let first_outcome = reconcile_fixture(&first, &first_importer).unwrap();
        assert!(matches!(
            first_outcome,
            PublicationOutcome::Published { .. }
        ));
        let importer = PointerAdvancingImporter {
            inner: ObjectImporter::new("pointer-observer"),
            publications: vec![
                (second.repository.path().to_path_buf(), second_journal),
                (third.repository.path().to_path_buf(), third_journal.clone()),
            ],
            next: std::cell::Cell::new(0),
        };
        let outcome = RepositoryReconciler::new(
            observer.repository.path(),
            observer.journal.clone(),
            "origin",
            &importer,
        )
        .reconcile(observer.format.clone())
        .unwrap();
        assert!(matches!(
            outcome,
            PublicationOutcome::ReadyCurrent { ref generation_id }
                if generation_id == &third_journal.descriptor.generation_id
        ));
        assert_eq!(
            remote_oid(&first, GENERATION_REF),
            Some(third_journal.descriptor_oid.clone())
        );
        for (reference, target) in &third_journal.descriptor.targets {
            assert_eq!(
                local_ref_oid(observer.repository.path(), reference).unwrap(),
                Some(target.oid.clone())
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn unsupported_atomic_hook_prose_never_enables_fallback() {
        use std::os::unix::fs::PermissionsExt as _;

        let fixture = v2_fixture();
        let importer = ObjectImporter::new("unsupported-prose");
        let hook = fixture._remote.path().join("hooks/pre-receive");
        fs::write(
            &hook,
            "#!/bin/sh\necho 'fatal: the receiving end does not support --atomic push' >&2\nexit 1\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&hook).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&hook, permissions).unwrap();
        let outcome = reconcile_fixture(&fixture, &importer).unwrap();
        assert!(matches!(outcome, PublicationOutcome::BlockedCorrupt { .. }));
        assert!(remote_oid(&fixture, GENERATION_REF).is_none());
        assert!(remote_ref_map(
            fixture.repository.path(),
            "origin",
            &[format!("{GENERATION_ROOT}/*")]
        )
        .unwrap()
        .is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn arbitrary_remote_rejection_never_selects_fallback() {
        use std::os::unix::fs::PermissionsExt as _;

        let fixture = v2_fixture();
        let importer = ObjectImporter::new("reject");
        let hook = fixture._remote.path().join("hooks/pre-receive");
        fs::write(&hook, "#!/bin/sh\necho policy-denied >&2\nexit 1\n").unwrap();
        let mut permissions = fs::metadata(&hook).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&hook, permissions).unwrap();
        let outcome = reconcile_fixture(&fixture, &importer).unwrap();
        assert!(matches!(outcome, PublicationOutcome::BlockedCorrupt { .. }));
        assert!(remote_oid(&fixture, GENERATION_REF).is_none());
        assert!(remote_ref_map(
            fixture.repository.path(),
            "origin",
            &[format!("{GENERATION_ROOT}/*")]
        )
        .unwrap()
        .is_empty());
    }
}
