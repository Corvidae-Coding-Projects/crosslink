use anyhow::{bail, Context, Result};
use chrono::Utc;
use std::cell::Cell;
use std::path::{Path, PathBuf};
use uuid::Uuid;

use crate::db::Database;
use crate::identity::AgentConfig;
use crate::issue_file::{IssueFile, MilestoneEntry};
use crate::sync::SyncManager;

pub(super) const KIND_INTERVENTION: &str = "intervention";

pub(super) const SIGNING_NAMESPACE: &str = "crosslink-comment";

pub(super) struct WriteSet {
    pub events: Vec<crate::events::Event>,
}

pub(super) const V2_WRITE_REFUSAL: &str = "this hub uses the legacy v2 layout; run `crosslink migrate hub-v3` to reconcile it into verified v3 authority";

pub(super) const LOCK_CONFIRM_TIMEOUT_SECS: u64 = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushOutcome {
    Pushed,

    LocalOnly,
}

pub struct SharedWriter {
    pub(super) sync: SyncManager,
    pub(super) agent: AgentConfig,
    pub(super) cache_dir: PathBuf,
    pub(super) readiness_dir: PathBuf,

    pub(super) event_seq: Cell<u64>,

    pub(super) last_v3_state: std::cell::RefCell<Option<crate::checkpoint::CheckpointState>>,
}

impl SharedWriter {
    pub fn new(crosslink_dir: &Path) -> Result<Option<Self>> {
        let configured_agent = AgentConfig::load(crosslink_dir)?;
        let initial_sync = SyncManager::new(crosslink_dir)?;
        if configured_agent.is_none()
            && !initial_sync.is_initialized()
            && !initial_sync.remote_exists()
        {
            return Ok(None);
        }
        let _operation =
            crate::reconcile::readiness::acquire_mutation_operation_permit(crosslink_dir)?;
        let agent = if let Some(a) = configured_agent {
            a
        } else {
            let sync = initial_sync;
            if !sync.is_initialized() {
                if !sync.remote_exists() {
                    return Ok(None);
                }
                sync.init_cache().context(
                    "shared Git authority exists but its cache could not be initialized",
                )?;
                if !sync.is_initialized() {
                    bail!("shared Git authority cache is unavailable after initialization");
                }
            }
            AgentConfig::anonymous(crosslink_dir)
        };
        let sync = SyncManager::new(crosslink_dir)?;
        if !sync.is_initialized() {
            if !sync.remote_exists() {
                bail!(
                    "shared Git authority is configured but its cache and remote are unavailable"
                );
            }
            bail!("Sync cache not initialized. Run `crosslink sync` first.");
        }
        let cache_dir = sync.cache_path().to_path_buf();

        std::fs::create_dir_all(cache_dir.join("issues"))?;
        std::fs::create_dir_all(cache_dir.join("meta").join("milestones"))?;

        let event_seq = Cell::new(Self::read_max_event_seq(
            &cache_dir,
            &agent.agent_id,
            sync.hub_mode(),
        ));

        crate::hub_v3::warn_if_migrated_v2_operation(&cache_dir, sync.hub_mode());

        Ok(Some(Self {
            sync,
            agent,
            cache_dir,
            readiness_dir: crosslink_dir.to_path_buf(),
            event_seq,
            last_v3_state: std::cell::RefCell::new(None),
        }))
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn agent_id(&self) -> &str {
        &self.agent.agent_id
    }

    pub(super) const fn hub_mode(&self) -> crate::hub_v3::HubMode {
        self.sync.hub_mode()
    }

    pub(super) const fn is_v3(&self) -> bool {
        self.hub_mode().is_v3()
    }

    #[must_use]
    pub const fn is_v3_public(&self) -> bool {
        self.is_v3()
    }

    #[must_use]
    pub fn cache_dir_public(&self) -> &Path {
        &self.cache_dir
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn crosslink_dir(&self) -> &Path {
        self.cache_dir.parent().unwrap_or_else(|| {
            tracing::warn!("cache_dir has no parent, falling back to cache_dir itself");
            &self.cache_dir
        })
    }

    pub fn hydrate_with_retry(&self, db: &Database) {
        let result = self
            .acquire_mutation_operation_permit()
            .and_then(|_operation| self.hydrate_after_mutation(db));
        if let Err(error) = result {
            tracing::warn!("hydration failed after shared mutation: {error}");
        }
    }

    pub(crate) fn hydrate_after_mutation(&self, db: &Database) -> Result<()> {
        if self.is_v3() {
            if self.last_v3_state.borrow().is_none() {
                self.refresh_v3_state()?;
            }
            if let Some(state) = self.last_v3_state.borrow().as_ref() {
                crate::hydration::hydrate_from_state(state, db)?;
            }
            crate::hydration::record_hydrated_ref_durable(&self.readiness_dir)?;
            return Ok(());
        }
        match crate::hydration::hydrate_to_sqlite(&self.cache_dir, db) {
            Ok(_) => {}
            Err(first_err) => {
                tracing::warn!(
                    "Warning: hydration failed ({}), retrying once...",
                    first_err
                );
                crate::hydration::hydrate_to_sqlite(&self.cache_dir, db).with_context(|| {
                    format!("hydration retry failed after initial error: {first_err}")
                })?;
            }
        }
        crate::hydration::record_hydrated_ref_durable(&self.readiness_dir)?;
        Ok(())
    }

    pub(super) fn read_max_event_seq(
        cache_dir: &Path,
        agent_id: &str,
        mode: crate::hub_v3::HubMode,
    ) -> u64 {
        if mode.is_v3() {
            return crate::hub_v3::read_max_event_seq_from_ref(cache_dir, agent_id).unwrap_or(0);
        }
        let log_path = cache_dir.join("agents").join(agent_id).join("events.log");
        crate::events::read_events(&log_path).map_or(0, |events| {
            events.iter().map(|e| e.agent_seq).max().unwrap_or(0)
        })
    }

    pub(super) fn next_event_seq(&self) -> u64 {
        let seq = self.event_seq.get() + 1;
        self.event_seq.set(seq);
        seq
    }

    pub(super) fn resolve_ssh_key_path(&self) -> Option<PathBuf> {
        let rel = self.agent.ssh_key_path.as_ref()?;
        let crosslink_dir = self
            .sync
            .cache_path()
            .parent()
            .unwrap_or_else(|| self.sync.cache_path());
        let abs = crosslink_dir.join(rel);
        if abs.exists() {
            Some(abs)
        } else {
            None
        }
    }

    pub(super) fn create_envelope(
        &self,
        event: crate::events::Event,
    ) -> crate::events::EventEnvelope {
        let seq = self.next_event_seq();
        let mut envelope = crate::events::EventEnvelope {
            agent_id: self.agent.agent_id.clone(),
            agent_seq: seq,
            timestamp: Utc::now(),
            event,
            signed_by: None,
            signature: None,
        };

        if let (Some(key_path), Some(fingerprint)) = (
            self.resolve_ssh_key_path(),
            self.agent.ssh_fingerprint.as_ref(),
        ) {
            if let Err(e) = crate::events::sign_event(&mut envelope, &key_path, fingerprint) {
                tracing::warn!(
                    "event signing failed (key: {}, fingerprint: {}): {}",
                    key_path.display(),
                    fingerprint,
                    e
                );
            }
        }

        envelope
    }

    pub(super) fn emit_compact_push_inner(
        &self,
        event: crate::events::Event,
        _message: &str,
    ) -> Result<PushOutcome> {
        if !self.is_v3() {
            bail!(V2_WRITE_REFUSAL);
        }

        let lock_guard = self.sync.acquire_lock()?;
        let outcome = self.commit_v3(vec![event], &lock_guard)?;
        let db = Database::open(&self.readiness_dir.join("issues.db"))?;
        crate::hydration::hydrate_current_authority_under_operation(&self.readiness_dir, &db)?;
        Ok(outcome)
    }

    pub fn write_agent_request(
        &self,
        target_agent_id: &str,
        request: &crate::agent_requests::AgentRequest,
    ) -> Result<PushOutcome> {
        let _permit = self.acquire_mutation_operation_permit()?;
        let _lock_guard = self.sync.acquire_lock()?;

        if !self.is_v3() {
            bail!(V2_WRITE_REFUSAL);
        }
        crate::hub_v3::write_request_to_own_ref(
            &self.cache_dir,
            &self.agent.agent_id,
            target_agent_id,
            request,
        )?;
        let outcome = self.push_own_ref_outcome();
        crate::hydration::record_hydrated_ref_durable(&self.readiness_dir)?;
        Ok(outcome)
    }

    pub fn write_agent_ack(
        &self,
        _target_agent_id: &str,
        ack: &crate::agent_requests::AgentRequestAck,
    ) -> Result<PushOutcome> {
        let _permit = self.acquire_mutation_operation_permit()?;
        let _lock_guard = self.sync.acquire_lock()?;

        if !self.is_v3() {
            bail!(V2_WRITE_REFUSAL);
        }
        crate::hub_v3::write_ack_to_own_ref(
            &self.cache_dir,
            &self.agent.agent_id,
            &ack.request_id,
            ack,
        )?;
        let outcome = self.push_own_ref_outcome();
        crate::hydration::record_hydrated_ref_durable(&self.readiness_dir)?;
        Ok(outcome)
    }

    pub(super) fn sign_comment(
        &self,
        content: &str,
        author: &str,
        comment_id: i64,
    ) -> (Option<String>, Option<String>) {
        let (key_path, fingerprint) = match (&self.agent.ssh_key_path, &self.agent.ssh_fingerprint)
        {
            (Some(rel), Some(fp)) => {
                let crosslink_dir = self
                    .sync
                    .cache_path()
                    .parent()
                    .unwrap_or_else(|| self.sync.cache_path());
                let abs = crosslink_dir.join(rel);
                (abs, fp.clone())
            }
            _ => return (None, None),
        };

        if !key_path.exists() {
            return (None, None);
        }

        let canonical = crate::signing::canonicalize_for_signing(&[
            ("author", author),
            ("comment_id", &comment_id.to_string()),
            ("content", content),
        ]);

        crate::signing::sign_content(&key_path, &canonical, SIGNING_NAMESPACE)
            .map_or((None, None), |sig| (Some(fingerprint), Some(sig)))
    }

    pub(super) fn load_milestone_by_id(&self, display_id: i64) -> Result<MilestoneEntry> {
        if self.last_v3_state.borrow().is_none() {
            self.refresh_v3_state()?;
        }
        let state = self.last_v3_state.borrow();
        let state = state.as_ref().ok_or_else(|| {
            anyhow::anyhow!("v3 state unavailable while loading milestone {display_id}")
        })?;
        let cm = state
            .milestones
            .values()
            .find(|m| m.display_id == Some(display_id))
            .ok_or_else(|| anyhow::anyhow!("Milestone #{display_id} not found in v3 state"))?;
        Ok(MilestoneEntry {
            uuid: cm.uuid,
            display_id,
            name: cm.name.clone(),
            description: cm.description.clone(),
            status: cm.status,
            created_at: cm.created_at,
            closed_at: cm.closed_at,
        })
    }

    pub(super) fn load_issue_by_display_id(&self, display_id: i64) -> Result<IssueFile> {
        if self.last_v3_state.borrow().is_none() {
            self.refresh_v3_state()?;
        }
        let state = self.last_v3_state.borrow();
        let state = state.as_ref().ok_or_else(|| {
            anyhow::anyhow!("v3 state unavailable while loading issue {display_id}")
        })?;
        let ci = state
            .issues
            .values()
            .find(|i| i.display_id == Some(display_id))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Issue {} not found in v3 state",
                    crate::utils::format_issue_id(display_id)
                )
            })?;
        Ok(IssueFile {
            uuid: ci.uuid,
            display_id: ci.display_id,
            title: ci.title.clone(),
            description: ci.description.clone(),
            status: ci.status,
            priority: ci.priority,
            parent_uuid: ci.parent_uuid,
            created_by: ci.created_by.clone(),
            created_at: ci.created_at,
            updated_at: ci.updated_at,
            closed_at: ci.closed_at,
            scheduled_at: ci.scheduled_at,
            due_at: ci.due_at,
            labels: ci.labels.iter().cloned().collect(),
            comments: vec![],
            blockers: ci.blockers.iter().copied().collect(),
            related: ci.related.iter().copied().collect(),
            milestone_uuid: ci.milestone_uuid,
            time_entries: vec![],
        })
    }

    pub(super) fn load_issue_by_id(&self, id: i64, db: &Database) -> Result<IssueFile> {
        let resolved = db.resolve_id(id);
        if resolved >= 0 {
            self.load_issue_by_display_id(resolved)
        } else {
            bail!(
                "negative (offline) issue id L{} is not valid on a v3 hub",
                resolved.unsigned_abs()
            )
        }
    }

    pub(super) fn resolve_uuid(&self, id: i64, db: &Database) -> Result<Uuid> {
        let resolved = db.resolve_id(id);

        if resolved >= 0 {
            if let Ok(issue) = self.load_issue_by_display_id(resolved) {
                Ok(issue.uuid)
            } else {
                let uuid_str = db.get_issue_uuid_by_id(resolved)?;
                uuid_str.parse().with_context(|| {
                    format!("Invalid UUID for issue #{resolved} from SQLite fallback")
                })
            }
        } else {
            let uuid_str = db.get_issue_uuid_by_id(resolved)?;
            uuid_str.parse().with_context(|| {
                format!("Invalid UUID for local issue L{}", resolved.unsigned_abs())
            })
        }
    }

    fn normalize_events_for_v3(events: Vec<crate::events::Event>) -> Vec<crate::events::Event> {
        use crate::events::Event;
        events
            .into_iter()
            .map(|e| match e {
                Event::IssueCreated {
                    uuid,
                    title,
                    description,
                    priority,
                    labels,
                    parent_uuid,
                    created_by,
                    display_id: _,
                    scheduled_at,
                    due_at,
                } => Event::IssueCreated {
                    uuid,
                    title,
                    description,
                    priority,
                    labels,
                    parent_uuid,
                    created_by,
                    display_id: None,
                    scheduled_at,
                    due_at,
                },
                Event::CommentAdded {
                    issue_uuid,
                    comment_uuid,
                    display_id: _,
                    author,
                    content,
                    created_at,
                    kind,
                    trigger_type,
                    intervention_context,
                    driver_key_fingerprint,
                    signed_by,
                    signature,
                } => Event::CommentAdded {
                    issue_uuid,
                    comment_uuid,
                    display_id: None,
                    author,
                    content,
                    created_at,
                    kind,
                    trigger_type,
                    intervention_context,
                    driver_key_fingerprint,
                    signed_by,
                    signature,
                },
                Event::TimeEntryAdded {
                    issue_uuid,
                    entry_uuid,
                    display_id: _,
                    started_at,
                    ended_at,
                    duration_seconds,
                } => Event::TimeEntryAdded {
                    issue_uuid,
                    entry_uuid,
                    display_id: None,
                    started_at,
                    ended_at,
                    duration_seconds,
                },
                Event::MilestoneCreated {
                    uuid,
                    display_id: _,
                    name,
                    description,
                    created_at,
                } => Event::MilestoneCreated {
                    uuid,
                    display_id: None,
                    name,
                    description,
                    created_at,
                },
                other => other,
            })
            .collect()
    }

    fn commit_v3(
        &self,
        events: Vec<crate::events::Event>,
        _lock: &crate::sync::HubWriteLock,
    ) -> Result<PushOutcome> {
        let agent_id = self.agent.agent_id.clone();
        let remote = self.sync.remote();
        let requires_publication = self.sync.remote_exists();
        let normalized = Self::normalize_events_for_v3(events);
        let envelopes = normalized
            .into_iter()
            .map(|event| self.create_envelope(event))
            .collect::<Vec<_>>();
        let append = match envelopes.as_slice() {
            [] => None,
            [envelope] => Some(
                crate::hub_v3::append_event_to_ref(&self.cache_dir, &agent_id, envelope)
                    .context("v3: failed to append event to agent ref")?,
            ),
            _ => Some(
                crate::hub_v3::append_events_to_ref(&self.cache_dir, &agent_id, &envelopes)
                    .context("v3: failed to append event batch to agent ref")?,
            ),
        };

        let publication = if requires_publication {
            match crate::hub_v3::push_agent_ref(&self.cache_dir, remote, &agent_id) {
                Ok(crate::hub_v3::PushOutcome::Pushed) => Ok(()),
                Ok(crate::hub_v3::PushOutcome::NonFastForward) => Err(anyhow::anyhow!(
                    "v3 own-ref publication for agent '{agent_id}' was rejected as non-fast-forward"
                )),
                Ok(crate::hub_v3::PushOutcome::NoRemote) => Err(anyhow::anyhow!(
                    "shared Git authority remote '{remote}' is unavailable"
                )),
                Ok(crate::hub_v3::PushOutcome::Failed(detail)) => Err(anyhow::anyhow!(
                    "v3 own-ref publication for agent '{agent_id}' failed: {detail}"
                )),
                Err(error) => Err(error),
            }
        } else {
            Ok(())
        };
        if let Err(error) = publication {
            if let Some(append) = append {
                let reference = crate::hub_v3::agent_ref_name(&agent_id)?;
                crate::hub_v3::restore_ref_after_failed_publication(
                    &self.cache_dir,
                    &reference,
                    &append.new_commit,
                    append.old_commit.as_deref(),
                )
                .context("failed to roll back unpublished shared-domain event")?;
            }
            self.event_seq
                .set(self.event_seq.get().saturating_sub(envelopes.len() as u64));
            return Err(error);
        }

        if self.sync.remote_exists() {
            self.sync.fetch_and_adopt_v3_refs();
        }

        self.refresh_v3_state()?;
        self.write_and_push_v3_checkpoint();

        Ok(if requires_publication {
            PushOutcome::Pushed
        } else {
            PushOutcome::LocalOnly
        })
    }

    fn write_and_push_v3_checkpoint(&self) {
        let bytes = {
            let state = self.last_v3_state.borrow();
            let Some(state) = state.as_ref() else {
                return;
            };
            let mut state = state.clone();
            state.compaction_lease = None;
            match serde_json::to_vec_pretty(&state) {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!("v3: checkpoint serialization failed (non-fatal): {e}");
                    return;
                }
            }
        };

        if let Ok(Some(tip)) =
            crate::hub_v3::git_rev_parse_optional(&self.cache_dir, crate::hub_v3::CHECKPOINT_REF)
        {
            let spec = format!("{tip}:state.json");
            if let Ok(Some(existing)) =
                crate::hub_v3::git_cat_file_blob_optional(&self.cache_dir, &spec)
            {
                if existing == bytes {
                    return;
                }
            }
        }
        if let Err(e) = crate::hub_v3::commit_blob_to_ref(
            &self.cache_dir,
            crate::hub_v3::CHECKPOINT_REF,
            "state.json",
            &bytes,
            "crosslink v3 checkpoint",
        ) {
            tracing::warn!("v3: checkpoint write failed (non-fatal): {e}");
            return;
        }
        if self.sync.remote_exists() {
            let expected = crate::hub_v3::git_rev_parse_optional(
                &self.cache_dir,
                "refs/crosslink-remote/checkpoint",
            )
            .ok()
            .flatten();
            match crate::hub_v3::push_ref_with_lease(
                &self.cache_dir,
                self.sync.remote(),
                crate::hub_v3::CHECKPOINT_REF,
                expected.as_deref(),
            ) {
                Ok(
                    crate::hub_v3::PushOutcome::Pushed | crate::hub_v3::PushOutcome::NonFastForward,
                ) => {}
                Ok(other) => tracing::debug!("v3: checkpoint push did not complete: {other:?}"),
                Err(e) => tracing::debug!("v3: checkpoint push error (benign): {e}"),
            }
        }
    }

    pub(super) fn refresh_v3_state(&self) -> Result<()> {
        let source = crate::hub_source::RefHubSource::new(&self.cache_dir)
            .context("v3: failed to construct RefHubSource for state refresh")?;
        let outcome =
            crate::compaction::reduce(&source).context("v3: reduction for state refresh failed")?;
        *self.last_v3_state.borrow_mut() = Some(outcome.state);
        Ok(())
    }

    fn push_own_ref_outcome(&self) -> PushOutcome {
        if !self.sync.remote_exists() {
            return PushOutcome::LocalOnly;
        }
        match crate::hub_v3::push_agent_ref(
            &self.cache_dir,
            self.sync.remote(),
            &self.agent.agent_id,
        ) {
            Ok(crate::hub_v3::PushOutcome::Pushed) => PushOutcome::Pushed,
            Ok(other) => {
                tracing::warn!(
                    "v3 own-ref push for '{}' did not complete: {other:?}; saved locally",
                    self.agent.agent_id
                );
                PushOutcome::LocalOnly
            }
            Err(e) => {
                tracing::warn!("v3 own-ref push for '{}' error: {e}", self.agent.agent_id);
                PushOutcome::LocalOnly
            }
        }
    }

    pub(super) fn confirm_v3_locks(&self) -> Result<()> {
        if let Err(e) = self.sync.fetch() {
            tracing::warn!("v3 lock confirm: fetch failed ({e}); confirming against local view");
        }
        self.refresh_v3_state()
    }

    pub(super) fn v3_assigned_display_id(&self, uuid: &Uuid) -> Option<i64> {
        self.last_v3_state
            .borrow()
            .as_ref()
            .and_then(|s| s.display_id_map.get(uuid).copied())
    }

    pub(super) fn v3_assigned_comment_id(
        &self,
        issue_display_id: i64,
        comment_uuid: &Uuid,
    ) -> Option<i64> {
        let state = self.last_v3_state.borrow();
        let state = state.as_ref()?;
        let issue = state
            .issues
            .values()
            .find(|i| i.display_id == Some(issue_display_id))?;
        issue.comments.get(comment_uuid).and_then(|c| c.display_id)
    }

    pub(super) fn v3_assigned_milestone_id(&self, uuid: &Uuid) -> Option<i64> {
        self.last_v3_state
            .borrow()
            .as_ref()
            .and_then(|s| s.milestones.get(uuid).and_then(|m| m.display_id))
    }

    pub(super) fn write_commit_push<F>(
        &self,
        db: &Database,
        mut prepare: F,
        _message: &str,
    ) -> Result<PushOutcome>
    where
        F: FnMut(&Self) -> Result<WriteSet>,
    {
        let _permit = self.acquire_mutation_operation_permit()?;
        if !self.is_v3() {
            bail!(V2_WRITE_REFUSAL);
        }

        let lock_guard = self.sync.acquire_lock()?;

        let write_set = prepare(self)?;
        let outcome = self.commit_v3(write_set.events, &lock_guard)?;
        self.hydrate_after_mutation(db)?;
        Ok(outcome)
    }

    pub(super) fn acquire_mutation_operation_permit(
        &self,
    ) -> Result<crate::reconcile::readiness::MutationOperationPermit> {
        crate::reconcile::readiness::acquire_mutation_operation_permit(&self.readiness_dir)
    }
}
