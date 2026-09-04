use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::core::SyncManager;
use super::HUB_BRANCH;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconciliationCacheOutcome {
    Ready,
    WaitingForRemote { reason: String },
    BlockedCorrupt { reason: String },
}

#[derive(Debug)]
enum ReconciliationRemoteError {
    Unavailable(String),
    Rejected(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CheckpointAdoption {
    AdoptRemote,
    KeepLocal,
    Recompute,
}

fn checkpoint_adoption(
    local: &crate::checkpoint::CheckpointState,
    remote: &crate::checkpoint::CheckpointState,
) -> CheckpointAdoption {
    match remote.frontier.relation(&local.frontier) {
        crate::checkpoint::FrontierRelation::Dominates => CheckpointAdoption::AdoptRemote,
        crate::checkpoint::FrontierRelation::Dominated => CheckpointAdoption::KeepLocal,
        crate::checkpoint::FrontierRelation::Concurrent => CheckpointAdoption::Recompute,
        crate::checkpoint::FrontierRelation::Equal => {
            let equal = match (serde_json::to_value(local), serde_json::to_value(remote)) {
                (Ok(local), Ok(remote)) => local == remote,
                _ => false,
            };
            if equal {
                CheckpointAdoption::KeepLocal
            } else {
                CheckpointAdoption::Recompute
            }
        }
    }
}

impl std::fmt::Display for ReconciliationRemoteError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(reason) | Self::Rejected(reason) => formatter.write_str(reason),
        }
    }
}

impl std::error::Error for ReconciliationRemoteError {}

fn classify_reconciliation_remote_error(
    operation: &str,
    message: &str,
) -> ReconciliationRemoteError {
    let reason = format!("{operation} failed: {message}");
    let lower = message.to_ascii_lowercase();
    if lower.contains("could not resolve host")
        || lower.contains("could not read from remote repository")
        || lower.contains("connection timed out")
        || lower.contains("connection refused")
        || lower.contains("network is unreachable")
        || lower.contains("no such file or directory")
        || lower.contains("does not appear to be a git repository")
    {
        ReconciliationRemoteError::Unavailable(reason)
    } else {
        ReconciliationRemoteError::Rejected(reason)
    }
}

pub struct HubWriteLock {
    path: PathBuf,
    _file: std::fs::File,
}

impl Drop for HubWriteLock {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_file(&self.path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(
                    "failed to release hub write lock {}: {}",
                    self.path.display(),
                    e
                );
            }
        }
    }
}

fn try_create_lock(lock_path: &Path) -> std::io::Result<HubWriteLock> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(lock_path)?;
    writeln!(f, "{}", std::process::id())?;
    Ok(HubWriteLock {
        path: lock_path.to_path_buf(),
        _file: f,
    })
}

#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .output()
        .is_ok_and(|output| output.status.success())
}

#[cfg(target_os = "windows")]
fn process_is_alive(pid: u32) -> bool {
    crate::reconcile::readiness::is_process_running(pid)
}

#[cfg(not(any(unix, target_os = "windows")))]
fn process_is_alive(_pid: u32) -> bool {
    false
}

pub fn acquire_hub_lock(lock_path: &Path) -> Result<HubWriteLock> {
    acquire_hub_lock_with_timeout(lock_path, Duration::from_secs(30))
}

fn acquire_hub_lock_with_timeout(lock_path: &Path, max_wait: Duration) -> Result<HubWriteLock> {
    let poll_interval = Duration::from_millis(100);
    let start = std::time::Instant::now();

    loop {
        match try_create_lock(lock_path) {
            Ok(guard) => return Ok(guard),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let holder_alive = std::fs::read_to_string(lock_path)
                    .ok()
                    .and_then(|content| content.trim().parse::<u32>().ok())
                    .is_some_and(process_is_alive);

                if !holder_alive {
                    let _ = std::fs::remove_file(lock_path);
                    if let Ok(guard) = try_create_lock(lock_path) {
                        return Ok(guard);
                    }
                }

                if start.elapsed() > max_wait {
                    if holder_alive {
                        bail!(
                            "hub write lock held by live process for >30s ({}); \
                             waiting aborted to avoid concurrent worktree mutation — \
                             retry, or remove the lock file if the process is hung: {}",
                            std::fs::read_to_string(lock_path)
                                .ok()
                                .and_then(|c| c.trim().parse::<u32>().ok())
                                .map_or_else(
                                    || "<unknown PID>".to_string(),
                                    |pid| format!("PID {pid}")
                                ),
                            lock_path.display()
                        );
                    }

                    let _ = std::fs::remove_file(lock_path);
                    match try_create_lock(lock_path) {
                        Ok(guard) => return Ok(guard),
                        Err(_) => bail!(
                            "Hub lock held for >30s and could not be acquired after force-removal"
                        ),
                    }
                }
                std::thread::sleep(poll_interval);
            }
            Err(e) => return Err(e.into()),
        }
    }
}

impl SyncManager {
    pub(crate) fn acquire_lock(&self) -> Result<HubWriteLock> {
        let lock_path = self.cache_dir.join(".hub-write-lock");
        acquire_hub_lock(&lock_path)
    }

    pub fn init_cache(&self) -> Result<()> {
        self.migrate_from_locks_branch()?;

        if self.cache_dir.exists() {
            return Ok(());
        }

        let has_remote_v2 = self
            .git_in_repo(&["ls-remote", "--heads", &self.remote, HUB_BRANCH])
            .is_ok_and(|o| !String::from_utf8_lossy(&o.stdout).trim().is_empty());
        let has_local_v2 = self
            .git_in_repo(&["rev-parse", "--verify", HUB_BRANCH])
            .is_ok();

        if has_remote_v2 || has_local_v2 {
            self.init_v2_worktree(has_remote_v2, has_local_v2)?;
        } else {
            self.init_v3_host_worktree()?;
            let remote = self.remote_exists().then(|| self.remote.clone());

            let remote_with_v3 = remote.clone().filter(|r| {
                matches!(
                    crate::hub_v3::detect_remote_hub_version(&self.repo_root, r),
                    Ok(crate::hub_v3::HubVersion::V3 { .. })
                )
            });
            if let Some(remote) = remote_with_v3 {
                crate::hub_v3::fetch_v3_refs_for_join(&self.cache_dir, &remote)?;
            } else {
                let agent_id = crate::identity::AgentConfig::load(&self.crosslink_dir)?
                    .map_or_else(|| "hub-v3-bootstrap".to_string(), |a| a.agent_id);
                let outcome =
                    crate::hub_v3::bootstrap_v3_hub(&self.cache_dir, &agent_id, remote.as_deref())?;
                if let Some(pushes) = &outcome.pushed {
                    for (ref_name, push) in pushes {
                        if !matches!(
                            push,
                            crate::hub_v3::PushOutcome::Pushed
                                | crate::hub_v3::PushOutcome::NoRemote
                        ) {
                            tracing::warn!(
                                "v3 bootstrap: pushing {ref_name} did not complete: {push:?} \
                                 (local hub is ready; a later `crosslink sync` retries the push)"
                            );
                        }
                    }
                }
            }

            self.hub_mode.set(crate::hub_v3::HubMode::V3);
        }

        self.ensure_cache_git_identity()?;

        self.propagate_agent_hooks()?;

        Ok(())
    }

    pub fn init_cache_for_reconciliation(&self) -> ReconciliationCacheOutcome {
        match self.init_cache_for_reconciliation_inner() {
            Ok(()) => ReconciliationCacheOutcome::Ready,
            Err(error) => match error.downcast_ref::<ReconciliationRemoteError>() {
                Some(ReconciliationRemoteError::Unavailable(reason)) => {
                    ReconciliationCacheOutcome::WaitingForRemote {
                        reason: reason.clone(),
                    }
                }
                Some(ReconciliationRemoteError::Rejected(reason)) => {
                    ReconciliationCacheOutcome::BlockedCorrupt {
                        reason: reason.clone(),
                    }
                }
                None => ReconciliationCacheOutcome::BlockedCorrupt {
                    reason: format!("repository authority cache observation failed: {error:#}"),
                },
            },
        }
    }

    fn init_cache_for_reconciliation_inner(&self) -> Result<()> {
        if self.cache_dir.exists() {
            self.validate_cache_repository()?;
        }
        let remote_configured = self.remote_exists();
        let mut fetched_v3 = false;
        let mut candidates = Vec::new();
        for branch in [HUB_BRANCH, super::OLD_BRANCH] {
            let has_remote = if remote_configured {
                let output = self.reconciliation_remote_git(&[
                    "ls-remote",
                    "--heads",
                    &self.remote,
                    branch,
                ])?;
                !String::from_utf8_lossy(&output.stdout).trim().is_empty()
            } else {
                false
            };
            let has_local = self.git_in_repo(&["rev-parse", "--verify", branch]).is_ok();
            candidates.push((branch, has_remote, has_local));
        }
        if self.cache_dir.exists() {
            for (branch, has_remote, has_local) in &candidates {
                if !has_remote {
                    continue;
                }
                self.reconciliation_remote_git(&["fetch", &self.remote, branch])?;
                let remote_ref = format!("{}/{}", self.remote, branch);
                if !has_local {
                    self.git_in_repo(&["branch", branch, &remote_ref])?;
                    continue;
                }
                let local_tip = self.git_in_repo(&["rev-parse", branch])?;
                let local_tip = String::from_utf8_lossy(&local_tip.stdout)
                    .trim()
                    .to_string();
                let remote_tip = self.git_in_repo(&["rev-parse", &remote_ref])?;
                let remote_tip = String::from_utf8_lossy(&remote_tip.stdout)
                    .trim()
                    .to_string();
                if local_tip == remote_tip
                    || !git_is_ancestor(&self.repo_root, &local_tip, &remote_tip)?
                {
                    continue;
                }
                let checked_ref = std::process::Command::new("git")
                    .current_dir(&self.cache_dir)
                    .args(["symbolic-ref", "-q", "HEAD"])
                    .output()?;
                let checked_ref = String::from_utf8_lossy(&checked_ref.stdout)
                    .trim()
                    .to_string();
                let source_ref = format!("refs/heads/{branch}");
                if checked_ref == source_ref {
                    let status = std::process::Command::new("git")
                        .current_dir(&self.cache_dir)
                        .args([
                            "status",
                            "--porcelain",
                            "--untracked-files=all",
                            "--",
                            "issues",
                            "meta",
                            "locks",
                            "trust",
                            "agents",
                            "checkpoint",
                            "locks.json",
                        ])
                        .output()?;
                    if status.status.success() && status.stdout.is_empty() {
                        self.git_in_repo(&[
                            "-C",
                            &self.cache_path_str(),
                            "reset",
                            "--hard",
                            &remote_ref,
                        ])?;
                    }
                } else {
                    self.git_in_repo(&["update-ref", &source_ref, &remote_tip, &local_tip])?;
                }
            }
            if remote_configured {
                self.fetch_v3_refs_for_reconciliation()?;
            }
            return Ok(());
        }
        for (branch, has_remote, _) in &candidates {
            if *has_remote {
                self.reconciliation_remote_git(&["fetch", &self.remote, branch])?;
            }
        }
        let selected = candidates
            .iter()
            .copied()
            .find(|(_, has_remote, has_local)| *has_remote || *has_local);
        if let Some((branch, _, has_local)) = selected {
            if has_local {
                self.git_in_repo(&["worktree", "add", &self.cache_path_str(), branch])?;
            } else {
                let remote_ref = format!("{}/{}", self.remote, branch);
                self.git_in_repo(&[
                    "worktree",
                    "add",
                    "-b",
                    branch,
                    &self.cache_path_str(),
                    &remote_ref,
                ])?;
            }
            for (other, other_remote, other_local) in &candidates {
                if *other != branch && *other_remote && !other_local {
                    let remote_ref = format!("{}/{}", self.remote, other);
                    self.git_in_repo(&["branch", other, &remote_ref])?;
                }
            }
            self.hub_mode.set(crate::hub_v3::HubMode::V2);
        } else {
            self.init_v3_host_worktree()?;
            if remote_configured {
                self.fetch_v3_refs_for_reconciliation()?;
                fetched_v3 = true;
            }
            if matches!(
                self.reconciliation_shared_store()?,
                crate::reconcile::SharedStoreFormat::Absent
            ) {
                let agent_id = crate::identity::AgentConfig::load(&self.crosslink_dir)?
                    .map_or_else(|| "hub-v3-bootstrap".to_string(), |agent| agent.agent_id);
                crate::hub_v3::bootstrap_v3_hub(&self.cache_dir, &agent_id, None)?;
            }
            self.hub_mode.set(crate::hub_v3::HubMode::V3);
        }
        if remote_configured && !fetched_v3 {
            self.fetch_v3_refs_for_reconciliation()?;
        }
        self.ensure_cache_git_identity()?;
        self.propagate_agent_hooks()?;
        Ok(())
    }

    fn reconciliation_remote_git(&self, args: &[&str]) -> Result<std::process::Output> {
        let output = std::process::Command::new("git")
            .current_dir(&self.repo_root)
            .args(args)
            .output()
            .with_context(|| format!("running reconciliation remote git {args:?}"))?;
        if output.status.success() {
            return Ok(output);
        }
        let message = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(classify_reconciliation_remote_error(&format!("git {args:?}"), &message).into())
    }

    fn fetch_v3_refs_for_reconciliation(&self) -> Result<()> {
        crate::hub_v3::fetch_v3_refs_for_join(&self.cache_dir, &self.remote).map_err(|error| {
            classify_reconciliation_remote_error(
                "v3 reconciliation discovery",
                &format!("{error:#}"),
            )
            .into()
        })
    }

    fn reconciliation_shared_store(&self) -> Result<crate::reconcile::SharedStoreFormat> {
        let output = std::process::Command::new("git")
            .current_dir(&self.cache_dir)
            .args([
                "for-each-ref",
                "--format=%(refname)",
                "refs/crosslink/",
                "refs/heads/crosslink/",
            ])
            .output()
            .with_context(|| {
                format!(
                    "inspecting shared authority in {}",
                    self.cache_dir.display()
                )
            })?;
        anyhow::ensure!(
            output.status.success(),
            "git for-each-ref failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        let refs = String::from_utf8(output.stdout)
            .context("shared authority refs were not UTF-8")?
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect();
        Ok(crate::reconcile::classify_shared_store(&refs))
    }

    fn init_v2_worktree(&self, has_remote_v2: bool, has_local_v2: bool) -> Result<()> {
        if has_remote_v2 {
            self.git_in_repo(&["fetch", &self.remote, HUB_BRANCH])?;
        }
        if has_local_v2 {
            self.git_in_repo(&["worktree", "add", &self.cache_path_str(), HUB_BRANCH])?;
        } else {
            let remote_ref = format!("{}/{}", self.remote, HUB_BRANCH);
            self.git_in_repo(&[
                "worktree",
                "add",
                "-b",
                HUB_BRANCH,
                &self.cache_path_str(),
                &remote_ref,
            ])?;
        }
        Ok(())
    }

    fn init_v3_host_worktree(&self) -> Result<()> {
        crate::git_compat::add_orphan_worktree(
            &self.repo_root,
            super::HUB_V3_HOST_BRANCH,
            &self.cache_path_str(),
        )?;
        self.ensure_cache_git_identity()?;
        self.git_commit_in_cache(&[
            "--allow-empty",
            "-m",
            "Initialize crosslink v3 hub worktree",
        ])?;
        Ok(())
    }

    pub fn fetch(&self) -> Result<()> {
        let lock_guard = self.acquire_lock()?;

        if self.hub_mode.get().is_v3() {
            self.fetch_v3(&lock_guard);
            return Ok(());
        }

        let _lock_guard = lock_guard;

        self.fetch_v2_readonly()
    }

    fn fetch_v2_readonly(&self) -> Result<()> {
        let fetch_result = self.git_in_cache(&["fetch", &self.remote, HUB_BRANCH]);
        if let Err(e) = &fetch_result {
            let err_str = e.to_string();
            if err_str.contains("Could not resolve host")
                || err_str.contains("Could not read from remote")
                || err_str.contains("does not appear to be a git repository")
                || err_str.contains("No such remote")
                || err_str.contains("couldn't find remote ref")
            {
                return Ok(());
            }
            fetch_result?;
        }

        let remote_ref = format!("{}/{}", self.remote, HUB_BRANCH);
        let reset_result = self.git_in_cache(&["reset", "--hard", &remote_ref]);
        if let Err(e) = &reset_result {
            let err_str = e.to_string();

            if err_str.contains("unknown revision") || err_str.contains("ambiguous argument") {
                return Ok(());
            }
            reset_result?;
        }

        Ok(())
    }

    fn fetch_v3(&self, hub_lock: &super::HubWriteLock) {
        let _ = hub_lock;
        self.fetch_and_adopt_v3_refs();

        self.refresh_local_checkpoint();
    }

    pub(crate) fn fetch_and_adopt_v3_refs(&self) {
        let fetch_result = self.git_in_cache(&[
            "fetch",
            &self.remote,
            "+refs/heads/crosslink/checkpoint:refs/crosslink-remote/checkpoint",
            "refs/heads/crosslink/agents/*:refs/crosslink-remote/agents/*",
        ]);
        if fetch_result.is_err() {
            return;
        }

        let own_agent_id = crate::identity::AgentConfig::load(&self.crosslink_dir)
            .ok()
            .flatten()
            .map(|a| a.agent_id);

        let tips = match self.list_remote_agent_tips() {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!("v3 fetch: could not list remote agent tips: {e}");
                return;
            }
        };
        for (agent_id, remote_tip) in tips {
            if own_agent_id.as_deref() == Some(agent_id.as_str()) {
                continue;
            }
            let local_ref = format!("{}{agent_id}", crate::hub_v3::AGENT_REF_PREFIX);

            if let Err(e) = self.git_in_cache(&["update-ref", &local_ref, &remote_tip]) {
                tracing::warn!("v3 fetch: failed to adopt ref '{local_ref}': {e}");
            }
        }

        self.adopt_checkpoint_by_frontier();
    }

    fn list_remote_agent_tips(&self) -> Result<Vec<(String, String)>> {
        let output = self.git_in_cache(&[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "refs/crosslink-remote/agents/*",
        ])?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut out = Vec::new();
        for line in stdout.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Some((refname, sha)) = line.split_once(' ') else {
                continue;
            };
            if let Some(agent_id) = refname.strip_prefix("refs/crosslink-remote/agents/") {
                out.push((agent_id.to_string(), sha.to_string()));
            }
        }
        Ok(out)
    }

    fn adopt_checkpoint_by_frontier(&self) {
        let remote_tracking = "refs/crosslink-remote/checkpoint";
        let Some(remote_tip) =
            crate::hub_v3::git_rev_parse_optional(&self.cache_dir, remote_tracking)
                .ok()
                .flatten()
        else {
            return;
        };
        let remote = match self.verified_checkpoint(remote_tracking) {
            Ok(Some(state)) if !state.is_legacy() => state,
            Ok(Some(_)) => {
                tracing::warn!("v3 fetch: remote checkpoint uses the legacy watermark schema");
                return;
            }
            Ok(None) => return,
            Err(error) => {
                tracing::warn!("v3 fetch: rejected invalid remote checkpoint: {error:#}");
                return;
            }
        };
        let local = match self.verified_checkpoint(crate::hub_v3::CHECKPOINT_REF) {
            Ok(Some(state)) if !state.is_legacy() => state,
            Ok(Some(_)) => {
                tracing::warn!(
                    "v3 fetch: local checkpoint uses the legacy watermark schema and will be recomputed"
                );
                self.recompute_checkpoint_from_authority();
                return;
            }
            Ok(None) => crate::checkpoint::CheckpointState::default(),
            Err(error) => {
                tracing::warn!(
                    "v3 fetch: local checkpoint is invalid and will be recovered from pinned authority: {error:#}"
                );
                self.recompute_checkpoint_from_authority();
                return;
            }
        };
        match checkpoint_adoption(&local, &remote) {
            CheckpointAdoption::AdoptRemote => {
                if let Err(error) =
                    self.git_in_cache(&["update-ref", crate::hub_v3::CHECKPOINT_REF, &remote_tip])
                {
                    tracing::warn!(
                        "v3 fetch: failed to adopt causally dominant remote checkpoint: {error}"
                    );
                }
            }
            CheckpointAdoption::KeepLocal => {}
            CheckpointAdoption::Recompute => {
                self.recompute_checkpoint_from_authority();
            }
        }
    }

    fn verified_checkpoint(
        &self,
        ref_name: &str,
    ) -> Result<Option<crate::checkpoint::CheckpointState>> {
        let Some(tip) = crate::hub_v3::git_rev_parse_optional(&self.cache_dir, ref_name)
            .with_context(|| format!("reading checkpoint ref '{ref_name}'"))?
        else {
            return Ok(None);
        };
        let source = crate::hub_source::RefHubSource::at_checkpoint(&self.cache_dir, Some(tip))?;
        let state = crate::hub_source::HubSource::read_checkpoint(&source)?;
        crate::compaction::reduce(&source)?;
        Ok(Some(state))
    }

    fn recompute_checkpoint_from_authority(&self) {
        let source = match crate::hub_source::RefHubSource::at_checkpoint(&self.cache_dir, None) {
            Ok(source) => source,
            Err(error) => {
                tracing::warn!("v3 fetch: could not pin authority for recomputation: {error:#}");
                return;
            }
        };
        let state = match crate::compaction::rebuild_from_authority(&source) {
            Ok(outcome) => outcome.state,
            Err(error) => {
                tracing::warn!("v3 fetch: authority recomputation failed: {error:#}");
                return;
            }
        };
        let bytes = match serde_json::to_vec_pretty(&state) {
            Ok(bytes) => bytes,
            Err(error) => {
                tracing::warn!("v3 fetch: recomputed checkpoint serialization failed: {error}");
                return;
            }
        };
        if let Err(error) = crate::hub_v3::commit_blob_to_ref(
            &self.cache_dir,
            crate::hub_v3::CHECKPOINT_REF,
            "state.json",
            &bytes,
            "crosslink v3 checkpoint (causal recompute)",
        ) {
            tracing::warn!("v3 fetch: authority recomputation publication failed: {error:#}");
        }
    }

    fn refresh_local_checkpoint(&self) {
        let source = match crate::hub_source::RefHubSource::new(&self.cache_dir) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("v3 fetch: RefHubSource construction failed (non-fatal): {e}");
                return;
            }
        };
        let mut state = match crate::compaction::reduce(&source) {
            Ok(o) => o.state,
            Err(e) => {
                tracing::warn!("v3 fetch: reduction failed (non-fatal): {e}");
                return;
            }
        };
        state.compaction_lease = None;
        let bytes = match serde_json::to_vec_pretty(&state) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!("v3 fetch: checkpoint serialization failed (non-fatal): {e}");
                return;
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
            "crosslink v3 checkpoint (fetch refresh)",
        ) {
            tracing::warn!("v3 fetch: local checkpoint refresh failed (non-fatal): {e}");
        }
    }
}

fn git_is_ancestor(repository: &Path, ancestor: &str, descendant: &str) -> Result<bool> {
    let output = std::process::Command::new("git")
        .current_dir(repository)
        .args(["merge-base", "--is-ancestor", ancestor, descendant])
        .output()?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => bail!(
            "git merge-base --is-ancestor failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::tempdir;

    fn checkpoint_with_sequences(sequences: &[(&str, u64)]) -> crate::checkpoint::CheckpointState {
        let mut state = crate::checkpoint::CheckpointState::default();
        for (agent, sequence) in sequences {
            state.frontier.agents.insert(
                (*agent).to_string(),
                crate::checkpoint::AgentFrontier {
                    sequence: *sequence,
                    tip_oid: format!("{agent}-{sequence}"),
                    prefix_sha256: format!("hash-{agent}-{sequence}"),
                },
            );
        }
        state
    }

    #[test]
    fn phase2_causality_checkpoint_adoption_requires_dominance_and_recomputes_concurrency() {
        let local = checkpoint_with_sequences(&[("a", 2), ("b", 1)]);
        let dominant = checkpoint_with_sequences(&[("a", 2), ("b", 2)]);
        let concurrent = checkpoint_with_sequences(&[("a", 1), ("b", 2)]);
        assert_eq!(
            checkpoint_adoption(&local, &dominant),
            CheckpointAdoption::AdoptRemote
        );
        assert_eq!(
            checkpoint_adoption(&dominant, &local),
            CheckpointAdoption::KeepLocal
        );
        assert_eq!(
            checkpoint_adoption(&local, &concurrent),
            CheckpointAdoption::Recompute
        );
        assert_eq!(
            checkpoint_adoption(&local, &local),
            CheckpointAdoption::KeepLocal
        );
        let mut inconsistent = local.clone();
        inconsistent.next_display_id += 1;
        assert_eq!(
            checkpoint_adoption(&local, &inconsistent),
            CheckpointAdoption::Recompute
        );
    }

    #[test]
    fn phase2_causality_concurrent_remote_checkpoint_recomputes_union_without_losing_local_agent_history(
    ) {
        let root = initialize_repository();
        let remote = tempdir().unwrap();
        assert!(Command::new("git")
            .current_dir(remote.path())
            .args(["init", "--bare", "-b", "main"])
            .status()
            .unwrap()
            .success());
        assert!(Command::new("git")
            .current_dir(root.path())
            .args(["remote", "add", "origin", remote.path().to_str().unwrap()])
            .status()
            .unwrap()
            .success());
        assert!(Command::new("git")
            .current_dir(root.path())
            .args(["push", "-u", "origin", "main"])
            .status()
            .unwrap()
            .success());
        let crosslink = root.path().join(".crosslink");
        crate::identity::AgentConfig::init(&crosslink, "agent-b", None).unwrap();
        let sync = SyncManager::new(&crosslink).unwrap();
        sync.init_cache().unwrap();
        let cache = sync.cache_path();

        let make_event = |agent: &str, sequence: u64| crate::events::EventEnvelope {
            agent_id: agent.to_string(),
            agent_seq: sequence,
            timestamp: chrono::Utc::now(),
            event: crate::events::Event::IssueCreated {
                uuid: uuid::Uuid::new_v4(),
                title: format!("{agent}-{sequence}"),
                description: None,
                priority: "medium".to_string(),
                labels: Vec::new(),
                parent_uuid: None,
                created_by: agent.to_string(),
                display_id: None,
                scheduled_at: None,
                due_at: None,
            },
            signed_by: None,
            signature: None,
        };
        let log_bytes = |events: &[crate::events::EventEnvelope]| {
            let dir = tempdir().unwrap();
            let path = dir.path().join("events.log");
            for event in events {
                crate::events::append_event(&path, event).unwrap();
            }
            std::fs::read(path).unwrap()
        };
        let a1 = make_event("agent-a", 1);
        let a2 = make_event("agent-a", 2);
        let b1 = make_event("agent-b", 1);
        let b2 = make_event("agent-b", 2);
        let a1_tip = crate::hub_v3::commit_log_bytes(
            cache,
            "agent-a",
            &log_bytes(std::slice::from_ref(&a1)),
            "agent a sequence 1",
        )
        .unwrap();
        let a2_tip = crate::hub_v3::commit_log_bytes(
            cache,
            "agent-a",
            &log_bytes(&[a1, a2]),
            "agent a sequence 2",
        )
        .unwrap();
        let b1_tip = crate::hub_v3::commit_log_bytes(
            cache,
            "agent-b",
            &log_bytes(std::slice::from_ref(&b1)),
            "agent b sequence 1",
        )
        .unwrap();
        let b2_tip = crate::hub_v3::commit_log_bytes(
            cache,
            "agent-b",
            &log_bytes(&[b1, b2]),
            "agent b sequence 2",
        )
        .unwrap();
        let a_ref = format!("{}agent-a", crate::hub_v3::AGENT_REF_PREFIX);
        assert!(Command::new("git")
            .current_dir(cache)
            .args(["update-ref", &a_ref, &a1_tip, &a2_tip])
            .status()
            .unwrap()
            .success());

        let local = crate::compaction::rebuild_from_authority(
            &crate::hub_source::RefHubSource::at_checkpoint(cache, None).unwrap(),
        )
        .unwrap()
        .state;
        crate::hub_v3::commit_blob_to_ref(
            cache,
            crate::hub_v3::CHECKPOINT_REF,
            "state.json",
            &serde_json::to_vec_pretty(&local).unwrap(),
            "local concurrent checkpoint",
        )
        .unwrap();

        let remote_source = crate::hub_source::RefHubSource::at_tips(
            cache,
            None,
            None,
            vec![
                ("agent-a".to_string(), a2_tip.clone()),
                ("agent-b".to_string(), b1_tip.clone()),
            ],
        )
        .unwrap();
        let remote_state = crate::compaction::rebuild_from_authority(&remote_source)
            .unwrap()
            .state;
        let remote_checkpoint = crate::hub_v3::commit_blob_to_ref(
            cache,
            "refs/crosslink/test/remote-checkpoint",
            "state.json",
            &serde_json::to_vec_pretty(&remote_state).unwrap(),
            "remote concurrent checkpoint",
        )
        .unwrap();
        for refspec in [
            format!("{a2_tip}:refs/heads/crosslink/agents/agent-a"),
            format!("{b1_tip}:refs/heads/crosslink/agents/agent-b"),
            format!("{remote_checkpoint}:refs/heads/crosslink/checkpoint"),
        ] {
            assert!(Command::new("git")
                .current_dir(cache)
                .args(["push", "--force", "origin", &refspec])
                .status()
                .unwrap()
                .success());
        }

        sync.fetch_and_adopt_v3_refs();
        let state = crate::hub_source::HubSource::read_checkpoint(
            &crate::hub_source::RefHubSource::new(cache).unwrap(),
        )
        .unwrap();
        assert_eq!(state.frontier.agents["agent-a"].sequence, 2);
        assert_eq!(state.frontier.agents["agent-b"].sequence, 2);
        assert_eq!(state.issues.len(), 4);
        assert_eq!(
            crate::hub_v3::git_rev_parse_optional(
                cache,
                &format!("{}agent-b", crate::hub_v3::AGENT_REF_PREFIX)
            )
            .unwrap(),
            Some(b2_tip)
        );
    }

    fn initialize_repository() -> tempfile::TempDir {
        let root = tempdir().unwrap();
        let status = Command::new("git")
            .current_dir(root.path())
            .args(["init", "-b", "main"])
            .status()
            .unwrap();
        assert!(status.success());
        for (key, value) in [
            ("user.email", "test@example.invalid"),
            ("user.name", "Test"),
        ] {
            assert!(Command::new("git")
                .current_dir(root.path())
                .args(["config", key, value])
                .status()
                .unwrap()
                .success());
        }
        std::fs::write(root.path().join("README.md"), "test").unwrap();
        assert!(Command::new("git")
            .current_dir(root.path())
            .args(["add", "README.md"])
            .status()
            .unwrap()
            .success());
        assert!(Command::new("git")
            .current_dir(root.path())
            .args(["commit", "-m", "initial", "--no-gpg-sign"])
            .status()
            .unwrap()
            .success());
        let crosslink = root.path().join(".crosslink");
        std::fs::create_dir(&crosslink).unwrap();
        std::fs::write(crosslink.join("hook-config.json"), "{}").unwrap();
        root
    }

    #[test]
    fn fresh_initialized_repository_bootstraps_verified_v3_authority() {
        let root = initialize_repository();
        let crosslink = root.path().join(".crosslink");
        let sync = SyncManager::new(&crosslink).unwrap();
        assert_eq!(
            sync.init_cache_for_reconciliation(),
            ReconciliationCacheOutcome::Ready
        );
        for name in [crate::hub_v3::CHECKPOINT_REF, crate::hub_v3::META_REF] {
            assert!(Command::new("git")
                .current_dir(sync.cache_path())
                .args(["rev-parse", "--verify", name])
                .status()
                .unwrap()
                .success());
        }
    }

    #[test]
    fn corrupt_initialized_cache_is_typed_blocked() {
        let root = initialize_repository();
        let crosslink = root.path().join(".crosslink");
        let cache = crosslink.join(super::super::HUB_CACHE_DIR);
        std::fs::create_dir(&cache).unwrap();
        std::fs::write(cache.join("not-a-repository"), "invalid").unwrap();
        let outcome = SyncManager::new(&crosslink)
            .unwrap()
            .init_cache_for_reconciliation();
        assert!(matches!(
            outcome,
            ReconciliationCacheOutcome::BlockedCorrupt { .. }
        ));
        assert!(!crosslink.join("issues.db").exists());
    }

    #[test]
    fn unavailable_remote_is_typed_waiting_without_projection_creation() {
        let root = initialize_repository();
        let crosslink = root.path().join(".crosslink");
        let unavailable = root.path().join("missing-remote.git");
        assert!(Command::new("git")
            .current_dir(root.path())
            .args(["remote", "add", "origin", unavailable.to_str().unwrap()])
            .status()
            .unwrap()
            .success());
        let outcome = SyncManager::new(&crosslink)
            .unwrap()
            .init_cache_for_reconciliation();
        assert!(matches!(
            outcome,
            ReconciliationCacheOutcome::WaitingForRemote { .. }
        ));
        assert!(!crosslink.join("issues.db").exists());
    }

    #[test]
    fn test_acquire_hub_lock_live_holder_bails_without_stealing() {
        let dir = tempdir().unwrap();
        let lock_path = dir.path().join(".hub-write-lock");

        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&lock_path)
                .expect("failed to create lock file");
            writeln!(f, "{}", std::process::id()).unwrap();
        }

        let timeout = Duration::from_millis(300);
        let err = match acquire_hub_lock_with_timeout(&lock_path, timeout) {
            Err(e) => e,
            Ok(_guard) => panic!("expected acquire to fail when a live process holds the lock"),
        };

        let msg = err.to_string();
        assert!(
            msg.contains(&std::process::id().to_string()),
            "error should include holder PID, got: {msg}"
        );
        assert!(
            msg.contains("live process"),
            "error should mention live process, got: {msg}"
        );

        assert!(
            lock_path.exists(),
            "lock file was removed by the acquire attempt (lock was stolen)"
        );
    }
}
