use anyhow::{Context, Result};

use super::core::SharedWriter;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockClaimResult {
    Claimed,

    AlreadyHeld,

    Contended {
        winner_agent_id: String,
    },

    /// The claim was published, but confirmation against a fresh fetch did not
    /// complete. Not ownership: a rerun confirms it without publishing again.
    Unconfirmed {
        cause: String,
    },
}

impl SharedWriter {
    pub fn claim_lock_v2(
        &self,
        issue_display_id: i64,
        branch: Option<&str>,
    ) -> Result<LockClaimResult> {
        let _permit = self.acquire_mutation_operation_permit()?;
        self.claim_lock_v2_inner(issue_display_id, branch)
    }

    /// Claims the lock and reports the outcome confirmed against a fresh
    /// fetch. Publication time does not count: only the confirmation's
    /// hub-lock wait and fetch are bounded. A claim that was published but
    /// could not be confirmed is `Unconfirmed`, which is not ownership; a
    /// rerun confirms it without publishing a second claim.
    fn claim_lock_v2_inner(
        &self,
        issue_display_id: i64,
        branch: Option<&str>,
    ) -> Result<LockClaimResult> {
        if let Some(lock) = self.read_lock_v2(issue_display_id)? {
            if lock.agent_id == self.agent.agent_id {
                if !self.is_v3() {
                    return Ok(LockClaimResult::AlreadyHeld);
                }
                if let Err(error) = self.confirm_v3_locks_bounded(self.lock_confirm_deadline()) {
                    return Ok(unconfirmed(&error));
                }
                match self.read_lock_v2(issue_display_id)? {
                    Some(lock) if lock.agent_id == self.agent.agent_id => {
                        return Ok(LockClaimResult::AlreadyHeld);
                    }
                    Some(lock) => {
                        return Ok(LockClaimResult::Contended {
                            winner_agent_id: lock.agent_id,
                        });
                    }
                    None => {}
                }
            }
        }

        let event = crate::events::Event::LockClaimed {
            issue_display_id,
            branch: branch.map(std::string::ToString::to_string),
        };
        let published = self.emit_compact_push_tracked(event)?;
        if let Some(error) = published.after_push_error {
            return Ok(unconfirmed(&error));
        }

        if self.is_v3() {
            if let Err(error) = self.confirm_v3_locks_bounded(self.lock_confirm_deadline()) {
                return Ok(unconfirmed(&error));
            }
        }

        match self.read_lock_v2(issue_display_id)? {
            Some(lock) if lock.agent_id == self.agent.agent_id => Ok(LockClaimResult::Claimed),
            Some(lock) => {
                let release = crate::events::Event::LockReleased { issue_display_id };

                if let Err(e) = self.emit_compact_push_inner(
                    release,
                    &format!("release lock on #{issue_display_id} (contention cleanup)"),
                ) {
                    tracing::info!("contention cleanup push deferred: {}", e);
                }
                Ok(LockClaimResult::Contended {
                    winner_agent_id: lock.agent_id,
                })
            }
            // On v3 the confirmed view must show the claim; "no lock" is not
            // ownership (for example, the reducer dropped the event).
            None if self.is_v3() => Ok(LockClaimResult::Unconfirmed {
                cause: "the claim is not visible after confirmation".to_string(),
            }),
            None => Ok(LockClaimResult::Claimed),
        }
    }

    /// The lock's holder, confirmed against a fresh fetch under the same
    /// bound as a claim's confirmation. An `Err` means it could not be
    /// confirmed; the local view is never returned in its place.
    pub fn confirmed_lock_holder(&self, issue_display_id: i64) -> Result<Option<String>> {
        let _permit = self.acquire_mutation_operation_permit()?;
        if self.is_v3() {
            self.confirm_v3_locks_bounded(self.lock_confirm_deadline())?;
        }
        Ok(self
            .read_lock_v2(issue_display_id)?
            .map(|lock| lock.agent_id))
    }

    pub fn release_lock_v2(&self, issue_display_id: i64) -> Result<bool> {
        let _permit = self.acquire_mutation_operation_permit()?;
        match self.read_lock_v2(issue_display_id)? {
            Some(lock) if lock.agent_id == self.agent.agent_id => {
                let event = crate::events::Event::LockReleased { issue_display_id };
                self.emit_compact_push_inner(
                    event,
                    &format!("release lock on #{issue_display_id}"),
                )?;
                Ok(true)
            }
            Some(_) | None => Ok(false),
        }
    }

    fn clear_stale_lock_state(&self, issue_display_id: i64, stale_agent_id: &str) -> Result<()> {
        crate::compaction::prune_events(&self.cache_dir, stale_agent_id)?;

        let mut state = crate::checkpoint::read_checkpoint(&self.cache_dir)?;
        state.locks.remove(&issue_display_id);
        crate::checkpoint::write_checkpoint(&self.cache_dir, &state)?;

        let lock_path = self
            .cache_dir
            .join("locks")
            .join(format!("{issue_display_id}.json"));
        if lock_path.exists() {
            std::fs::remove_file(&lock_path)?;
        }

        Ok(())
    }

    pub fn steal_lock_v2(
        &self,
        issue_display_id: i64,
        stale_agent_id: &str,
        branch: Option<&str>,
    ) -> Result<LockClaimResult> {
        let _permit = self.acquire_mutation_operation_permit()?;
        self.clear_stale_lock_state(issue_display_id, stale_agent_id)?;
        self.claim_lock_v2_inner(issue_display_id, branch)
    }

    pub fn force_release_lock_v2(
        &self,
        issue_display_id: i64,
        stale_agent_id: &str,
    ) -> Result<bool> {
        let _permit = self.acquire_mutation_operation_permit()?;
        self.clear_stale_lock_state(issue_display_id, stale_agent_id)?;

        let event = crate::events::Event::LockReleased { issue_display_id };
        self.emit_compact_push_inner(
            event,
            &format!("force-release stale lock on #{issue_display_id}"),
        )?;

        Ok(true)
    }

    pub fn read_lock_v2(
        &self,
        issue_display_id: i64,
    ) -> Result<Option<crate::issue_file::LockFileV2>> {
        if self.is_v3() {
            if self.last_v3_state.borrow().is_none() {
                self.refresh_v3_state()?;
            }
            let state = self.last_v3_state.borrow();
            return Ok(state.as_ref().and_then(|s| {
                s.locks
                    .get(&issue_display_id)
                    .map(|entry| crate::issue_file::LockFileV2 {
                        issue_id: issue_display_id,
                        agent_id: entry.agent_id.clone(),
                        branch: entry.branch.clone(),
                        claimed_at: entry.claimed_at,
                        signed_by: None,
                    })
            }));
        }

        let lock_path = self
            .cache_dir
            .join("locks")
            .join(format!("{issue_display_id}.json"));
        if !lock_path.exists() {
            return Ok(None);
        }
        let content = std::fs::read_to_string(&lock_path)
            .with_context(|| format!("Failed to read lock file: {}", lock_path.display()))?;
        let lock: crate::issue_file::LockFileV2 = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse lock file: {}", lock_path.display()))?;
        Ok(Some(lock))
    }
}

fn unconfirmed(error: &anyhow::Error) -> LockClaimResult {
    LockClaimResult::Unconfirmed {
        cause: format!("{error:#}"),
    }
}
