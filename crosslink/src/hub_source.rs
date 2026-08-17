use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::checkpoint::{read_checkpoint, read_watermark, CheckpointState};
use crate::events::{read_events_from_bytes, EventEnvelope, OrderingKey};

pub trait HubSource {
    fn agent_ids(&self) -> Result<Vec<String>>;

    fn read_events(
        &self,
        agent_id: &str,
        after: Option<&OrderingKey>,
    ) -> Result<Vec<EventEnvelope>>;

    fn read_checkpoint(&self) -> Result<CheckpointState>;

    fn read_legacy_watermark(&self) -> Result<Option<OrderingKey>>;

    fn allowed_signers_file(&self) -> Result<Option<PathBuf>>;
}

pub struct WorktreeSource {
    cache_dir: PathBuf,
}

impl WorktreeSource {
    pub fn new(cache_dir: &Path) -> Self {
        Self {
            cache_dir: cache_dir.to_path_buf(),
        }
    }
}

impl HubSource for WorktreeSource {
    fn agent_ids(&self) -> Result<Vec<String>> {
        let agents_dir = self.cache_dir.join("agents");
        if !agents_dir.exists() {
            return Ok(Vec::new());
        }
        let mut ids = Vec::new();
        for entry in std::fs::read_dir(&agents_dir)
            .with_context(|| format!("Failed to read agents dir: {}", agents_dir.display()))?
        {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                if let Some(name) = entry.file_name().to_str() {
                    ids.push(name.to_string());
                }
            }
        }
        Ok(ids)
    }

    fn read_events(
        &self,
        agent_id: &str,
        after: Option<&OrderingKey>,
    ) -> Result<Vec<EventEnvelope>> {
        let log_path = self
            .cache_dir
            .join("agents")
            .join(agent_id)
            .join("events.log");
        after.map_or_else(
            || crate::events::read_events(&log_path),
            |wm| crate::events::read_events_after(&log_path, wm),
        )
    }

    fn read_checkpoint(&self) -> Result<CheckpointState> {
        read_checkpoint(&self.cache_dir)
    }

    fn read_legacy_watermark(&self) -> Result<Option<OrderingKey>> {
        read_watermark(&self.cache_dir)
    }

    fn allowed_signers_file(&self) -> Result<Option<PathBuf>> {
        let p = self.cache_dir.join("trust").join("allowed_signers");
        if p.exists() {
            Ok(Some(p))
        } else {
            Ok(None)
        }
    }
}

#[derive(Debug)]
#[allow(dead_code)]
pub struct ObjectStoreSource {
    repo_path: PathBuf,

    commit_sha: String,

    ref_name: String,

    _allowed_signers_dir: Option<tempfile::TempDir>,

    allowed_signers_path: Option<PathBuf>,
}

#[allow(dead_code)]
impl ObjectStoreSource {
    pub fn new(repo_path: &Path, ref_name: &str) -> Result<Self> {
        let commit_sha = git_rev_parse(repo_path, ref_name)
            .with_context(|| format!("ref '{ref_name}' not found in {}", repo_path.display()))?;

        let (allowed_signers_dir, allowed_signers_path) =
            extract_allowed_signers(repo_path, &commit_sha)?;

        Ok(Self {
            repo_path: repo_path.to_path_buf(),
            commit_sha,
            ref_name: ref_name.to_string(),
            _allowed_signers_dir: allowed_signers_dir,
            allowed_signers_path,
        })
    }

    #[must_use]
    pub fn commit_sha(&self) -> &str {
        &self.commit_sha
    }

    #[must_use]
    pub fn ref_name(&self) -> &str {
        &self.ref_name
    }
}

impl HubSource for ObjectStoreSource {
    fn agent_ids(&self) -> Result<Vec<String>> {
        let tree_path = format!("{}:agents", self.commit_sha);
        let output = Command::new("git")
            .current_dir(&self.repo_path)
            .args(["ls-tree", "--name-only", &tree_path])
            .output()
            .with_context(|| {
                format!(
                    "failed to run git ls-tree for agents in ref '{}' ({})",
                    self.ref_name, self.commit_sha
                )
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);

            if stderr.contains("Not a tree object")
                || stderr.contains("does not exist")
                || stderr.contains("not found")
                || stderr.contains("invalid object")
                || stderr.contains(&format!("Not a valid object name {tree_path}"))
            {
                if !commit_object_exists(&self.repo_path, &self.commit_sha)? {
                    anyhow::bail!(
                        "pinned commit {} for ref '{}' no longer exists in the \
                         repository object store (pruned or corrupt); refusing to \
                         treat it as an empty hub",
                        self.commit_sha,
                        self.ref_name
                    );
                }

                return Ok(Vec::new());
            }
            anyhow::bail!(
                "git ls-tree failed for agents in ref '{}' ({}): {}",
                self.ref_name,
                self.commit_sha,
                stderr.trim()
            );
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let ids: Vec<String> = stdout
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect();
        Ok(ids)
    }

    fn read_events(
        &self,
        agent_id: &str,
        after: Option<&OrderingKey>,
    ) -> Result<Vec<EventEnvelope>> {
        let rel_path = format!("agents/{agent_id}/events.log");
        let bytes =
            git_cat_file_blob(&self.repo_path, &self.commit_sha, &rel_path).with_context(|| {
                format!(
                    "failed to read events.log for agent '{}' from ref '{}' ({})",
                    agent_id, self.ref_name, self.commit_sha
                )
            })?;

        let Some(bytes) = bytes else {
            return Ok(Vec::new());
        };

        let events = read_events_from_bytes(&bytes).with_context(|| {
            format!(
                "failed to parse events.log for agent '{}' from ref '{}' ({})",
                agent_id, self.ref_name, self.commit_sha
            )
        })?;

        if let Some(wm) = after {
            Ok(events
                .into_iter()
                .filter(|e| OrderingKey::from_envelope(e) > *wm)
                .collect())
        } else {
            Ok(events)
        }
    }

    fn read_checkpoint(&self) -> Result<CheckpointState> {
        let bytes = git_cat_file_blob(&self.repo_path, &self.commit_sha, "checkpoint/state.json")
            .with_context(|| {
            format!(
                "failed to read checkpoint/state.json from ref '{}' ({})",
                self.ref_name, self.commit_sha
            )
        })?;

        bytes.map_or_else(
            || Ok(CheckpointState::default()),
            |b| {
                CheckpointState::from_slice(&b).with_context(|| {
                    format!(
                        "failed to parse checkpoint/state.json from ref '{}' ({})",
                        self.ref_name, self.commit_sha
                    )
                })
            },
        )
    }

    fn read_legacy_watermark(&self) -> Result<Option<OrderingKey>> {
        let bytes = git_cat_file_blob(
            &self.repo_path,
            &self.commit_sha,
            "checkpoint/watermark.json",
        )
        .with_context(|| {
            format!(
                "failed to read checkpoint/watermark.json from ref '{}' ({})",
                self.ref_name, self.commit_sha
            )
        })?;

        match bytes {
            None => Ok(None),
            Some(b) => {
                let key: OrderingKey = serde_json::from_slice(&b).with_context(|| {
                    format!(
                        "failed to parse checkpoint/watermark.json from ref '{}' ({})",
                        self.ref_name, self.commit_sha
                    )
                })?;
                Ok(Some(key))
            }
        }
    }

    fn allowed_signers_file(&self) -> Result<Option<PathBuf>> {
        Ok(self.allowed_signers_path.clone())
    }
}

#[derive(Debug)]
#[allow(dead_code)]
pub struct RefHubSource {
    repo_path: PathBuf,

    checkpoint_sha: Option<String>,

    meta_sha: Option<String>,

    agent_tips: Vec<(String, String)>,

    _allowed_signers_dir: Option<tempfile::TempDir>,

    allowed_signers_path: Option<PathBuf>,
}

#[allow(dead_code)]
impl RefHubSource {
    pub fn new(repo_dir: &Path) -> Result<Self> {
        let checkpoint_sha = git_rev_parse_optional(repo_dir, crate::hub_v3::CHECKPOINT_REF)?;
        let meta_sha = git_rev_parse_optional(repo_dir, crate::hub_v3::META_REF)?;

        let mut agent_tips = Vec::new();
        for ref_name in for_each_ref(repo_dir, &format!("{}*", crate::hub_v3::AGENT_REF_PREFIX))? {
            let Some(agent_id) = ref_name.strip_prefix(crate::hub_v3::AGENT_REF_PREFIX) else {
                continue;
            };

            let Some(sha) = git_rev_parse_optional(repo_dir, &ref_name)? else {
                continue;
            };
            agent_tips.push((agent_id.to_string(), sha));
        }

        let (allowed_signers_dir, allowed_signers_path) = match &meta_sha {
            Some(sha) => extract_meta_allowed_signers(repo_dir, sha)?,
            None => (None, None),
        };

        Ok(Self {
            repo_path: repo_dir.to_path_buf(),
            checkpoint_sha,
            meta_sha,
            agent_tips,
            _allowed_signers_dir: allowed_signers_dir,
            allowed_signers_path,
        })
    }

    pub fn at_tips(
        repo_dir: &Path,
        checkpoint_sha: Option<String>,
        meta_sha: Option<String>,
        agent_tips: Vec<(String, String)>,
    ) -> Result<Self> {
        let (allowed_signers_dir, allowed_signers_path) = match &meta_sha {
            Some(sha) => extract_meta_allowed_signers(repo_dir, sha)?,
            None => (None, None),
        };
        Ok(Self {
            repo_path: repo_dir.to_path_buf(),
            checkpoint_sha,
            meta_sha,
            agent_tips,
            _allowed_signers_dir: allowed_signers_dir,
            allowed_signers_path,
        })
    }

    #[must_use]
    pub fn checkpoint_sha(&self) -> Option<&str> {
        self.checkpoint_sha.as_deref()
    }

    #[must_use]
    pub fn meta_sha(&self) -> Option<&str> {
        self.meta_sha.as_deref()
    }
}

impl HubSource for RefHubSource {
    fn agent_ids(&self) -> Result<Vec<String>> {
        Ok(self.agent_tips.iter().map(|(id, _)| id.clone()).collect())
    }

    fn read_events(
        &self,
        agent_id: &str,
        after: Option<&OrderingKey>,
    ) -> Result<Vec<EventEnvelope>> {
        let Some((_, sha)) = self.agent_tips.iter().find(|(id, _)| id == agent_id) else {
            return Ok(Vec::new());
        };

        let bytes = git_cat_file_blob(&self.repo_path, sha, "events.log").with_context(|| {
            format!("failed to read events.log for agent '{agent_id}' from pinned tip {sha}")
        })?;

        let Some(bytes) = bytes else {
            return Ok(Vec::new());
        };

        let events = read_events_from_bytes(&bytes).with_context(|| {
            format!("failed to parse events.log for agent '{agent_id}' from pinned tip {sha}")
        })?;

        if let Some(wm) = after {
            Ok(events
                .into_iter()
                .filter(|e| OrderingKey::from_envelope(e) > *wm)
                .collect())
        } else {
            Ok(events)
        }
    }

    fn read_checkpoint(&self) -> Result<CheckpointState> {
        let Some(sha) = &self.checkpoint_sha else {
            return Ok(CheckpointState::default());
        };

        let bytes = git_cat_file_blob(&self.repo_path, sha, "state.json").with_context(|| {
            format!("failed to read state.json from pinned checkpoint tip {sha}")
        })?;

        bytes.map_or_else(
            || Ok(CheckpointState::default()),
            |b| {
                CheckpointState::from_slice(&b).with_context(|| {
                    format!("failed to parse state.json from pinned checkpoint tip {sha}")
                })
            },
        )
    }

    fn read_legacy_watermark(&self) -> Result<Option<OrderingKey>> {
        Ok(None)
    }

    fn allowed_signers_file(&self) -> Result<Option<PathBuf>> {
        Ok(self.allowed_signers_path.clone())
    }
}

#[allow(dead_code)]
fn extract_meta_allowed_signers(
    repo_path: &Path,
    meta_sha: &str,
) -> Result<(Option<tempfile::TempDir>, Option<PathBuf>)> {
    let bytes = git_cat_file_blob(repo_path, meta_sha, "allowed_signers")
        .with_context(|| format!("failed to read allowed_signers from meta tip {meta_sha}"))?;

    let Some(bytes) = bytes else {
        return Ok((None, None));
    };

    let dir = tempfile::tempdir().context("failed to create temp dir for allowed_signers")?;
    let path = dir.path().join("allowed_signers");
    std::fs::write(&path, &bytes)
        .with_context(|| format!("failed to write allowed_signers to {}", path.display()))?;

    Ok((Some(dir), Some(path)))
}

#[allow(dead_code)]
fn git_rev_parse_optional(repo_path: &Path, ref_name: &str) -> Result<Option<String>> {
    let output = Command::new("git")
        .current_dir(repo_path)
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

#[allow(dead_code)]
fn for_each_ref(repo_path: &Path, pattern: &str) -> Result<Vec<String>> {
    let output = Command::new("git")
        .current_dir(repo_path)
        .args(["for-each-ref", "--format=%(refname)", pattern])
        .output()
        .with_context(|| format!("failed to run git for-each-ref for '{pattern}'"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git for-each-ref failed for '{pattern}': {}", stderr.trim());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect())
}

#[allow(dead_code)]
fn git_rev_parse(repo_path: &Path, ref_name: &str) -> Result<String> {
    let output = Command::new("git")
        .current_dir(repo_path)
        .args(["rev-parse", "--verify", ref_name])
        .output()
        .with_context(|| format!("failed to run git rev-parse for '{ref_name}'"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("ref '{ref_name}' does not exist: {}", stderr.trim());
    }

    let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if sha.is_empty() {
        anyhow::bail!("git rev-parse returned empty SHA for '{ref_name}'");
    }
    Ok(sha)
}

#[allow(dead_code)]
fn commit_object_exists(repo_path: &Path, sha: &str) -> Result<bool> {
    let spec = format!("{sha}^{{commit}}");
    let output = Command::new("git")
        .current_dir(repo_path)
        .args(["cat-file", "-e", &spec])
        .output()
        .with_context(|| format!("failed to run git cat-file -e for '{sha}'"))?;
    Ok(output.status.success())
}

#[allow(dead_code)]
fn git_cat_file_blob(
    repo_path: &Path,
    commit_sha: &str,
    rel_path: &str,
) -> Result<Option<Vec<u8>>> {
    let blob_spec = format!("{commit_sha}:{rel_path}");
    let output = Command::new("git")
        .current_dir(repo_path)
        .args(["cat-file", "blob", &blob_spec])
        .output()
        .with_context(|| format!("failed to run git cat-file for '{blob_spec}'"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);

        if stderr.contains("does not exist")
            || stderr.contains("Not a valid object name")
            || stderr.contains("not found")
            || stderr.contains("could not get object info")
        {
            if !commit_object_exists(repo_path, commit_sha)? {
                anyhow::bail!(
                    "pinned commit {commit_sha} no longer exists in the repository \
                     object store (pruned or corrupt); refusing to treat \
                     '{rel_path}' as missing"
                );
            }
            return Ok(None);
        }
        anyhow::bail!("git cat-file failed for '{}': {}", blob_spec, stderr.trim());
    }

    Ok(Some(output.stdout))
}

#[allow(dead_code)]
fn extract_allowed_signers(
    repo_path: &Path,
    commit_sha: &str,
) -> Result<(Option<tempfile::TempDir>, Option<PathBuf>)> {
    let bytes = git_cat_file_blob(repo_path, commit_sha, "trust/allowed_signers")
        .with_context(|| format!("failed to read trust/allowed_signers from {commit_sha}"))?;

    let Some(bytes) = bytes else {
        return Ok((None, None));
    };

    let dir = tempfile::tempdir().context("failed to create temp dir for allowed_signers")?;
    let path = dir.path().join("allowed_signers");
    std::fs::write(&path, &bytes)
        .with_context(|| format!("failed to write allowed_signers to {}", path.display()))?;

    Ok((Some(dir), Some(path)))
}

#[cfg(test)]
pub(crate) mod test_support {

    use super::*;
    use crate::checkpoint::{write_checkpoint, CheckpointState};
    use crate::events::{append_event, EventEnvelope};
    use std::path::Path;

    pub fn setup_hub_layout(dir: &Path) {
        std::fs::create_dir_all(dir.join("agents")).unwrap();
        std::fs::create_dir_all(dir.join("issues")).unwrap();
        std::fs::create_dir_all(dir.join("locks")).unwrap();
        std::fs::create_dir_all(dir.join("checkpoint")).unwrap();
        crate::issue_file::write_layout_version(
            &dir.join("meta"),
            crate::issue_file::CURRENT_LAYOUT_VERSION,
        )
        .unwrap();
    }

    pub fn write_agent_events(hub_dir: &Path, agent_id: &str, events: &[EventEnvelope]) {
        let log_path = hub_dir.join("agents").join(agent_id).join("events.log");
        for ev in events {
            append_event(&log_path, ev).unwrap();
        }
    }

    pub fn git_commit_hub_layout(repo_path: &Path, hub_dir: &Path, ref_name: &str) -> String {
        run_git(repo_path, &["init"]);
        run_git(repo_path, &["config", "user.email", "test@crosslink.test"]);
        run_git(repo_path, &["config", "user.name", "Test"]);

        let git_dir = repo_path.join(".git");
        let status = std::process::Command::new("git")
            .args([
                "--git-dir",
                git_dir.to_str().unwrap(),
                "--work-tree",
                hub_dir.to_str().unwrap(),
                "add",
                "-A",
            ])
            .output()
            .expect("git add failed");
        assert!(
            status.status.success(),
            "git add -A failed: {}",
            String::from_utf8_lossy(&status.stderr)
        );

        let out = std::process::Command::new("git")
            .args([
                "--git-dir",
                git_dir.to_str().unwrap(),
                "--work-tree",
                hub_dir.to_str().unwrap(),
                "commit",
                "-m",
                "hub layout",
            ])
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@crosslink.test")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@crosslink.test")
            .output()
            .expect("git commit failed");
        assert!(
            out.status.success(),
            "git commit failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );

        let head_ref = get_head_ref(repo_path);

        if head_ref != ref_name {
            let sha = get_head_sha(repo_path);
            update_ref(repo_path, ref_name, &sha);

            delete_ref(repo_path, &head_ref);
        }

        get_head_sha_for_ref(repo_path, ref_name)
    }

    pub fn run_git(repo_path: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .current_dir(repo_path)
            .args(args)
            .output()
            .unwrap_or_else(|e| panic!("git {args:?} failed to run: {e}"));
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn git_stdout(repo_path: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .current_dir(repo_path)
            .args(args)
            .output()
            .unwrap_or_else(|e| panic!("git {args:?} failed to run: {e}"));
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn get_head_ref(repo_path: &Path) -> String {
        git_stdout(repo_path, &["symbolic-ref", "HEAD"])
    }

    fn get_head_sha(repo_path: &Path) -> String {
        git_stdout(repo_path, &["rev-parse", "HEAD"])
    }

    fn get_head_sha_for_ref(repo_path: &Path, ref_name: &str) -> String {
        git_stdout(repo_path, &["rev-parse", ref_name])
    }

    fn update_ref(repo_path: &Path, ref_name: &str, sha: &str) {
        git_stdout(repo_path, &["update-ref", ref_name, sha]);
    }

    fn delete_ref(repo_path: &Path, ref_name: &str) {
        git_stdout(repo_path, &["update-ref", "-d", ref_name]);
    }

    pub fn write_hub_checkpoint(hub_dir: &Path, state: &CheckpointState) {
        write_checkpoint(hub_dir, state).unwrap();
    }

    pub fn reduce_worktree(hub_dir: &Path) -> crate::compaction::ReductionOutcome {
        let source = WorktreeSource::new(hub_dir);
        crate::compaction::reduce(&source).unwrap()
    }

    pub fn reduce_object_store(
        repo_path: &Path,
        ref_name: &str,
    ) -> crate::compaction::ReductionOutcome {
        let source = ObjectStoreSource::new(repo_path, ref_name).unwrap();
        crate::compaction::reduce(&source).unwrap()
    }

    pub fn assert_outcomes_equal(
        a: &crate::compaction::ReductionOutcome,
        b: &crate::compaction::ReductionOutcome,
        label: &str,
    ) {
        assert_eq!(
            a.events_processed, b.events_processed,
            "{label}: events_processed mismatch"
        );
        assert_eq!(
            a.changed_issues, b.changed_issues,
            "{label}: changed_issues mismatch"
        );
        assert_eq!(
            a.changed_locks, b.changed_locks,
            "{label}: changed_locks mismatch"
        );
        let state_a = serde_json::to_value(&a.state).unwrap();
        let state_b = serde_json::to_value(&b.state).unwrap();
        assert_eq!(state_a, state_b, "{label}: checkpoint state mismatch");
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use crate::checkpoint::CheckpointState;
    use crate::events::{Event, EventEnvelope, OrderingKey};
    use chrono::{Duration, Utc};
    use uuid::Uuid;

    fn make_envelope(agent_id: &str, seq: u64, event: Event) -> EventEnvelope {
        EventEnvelope {
            agent_id: agent_id.to_string(),
            agent_seq: seq,
            timestamp: Utc::now(),
            event,
            signed_by: None,
            signature: None,
        }
    }

    fn make_issue_created(agent_id: &str, seq: u64, uuid: Uuid) -> EventEnvelope {
        make_envelope(
            agent_id,
            seq,
            Event::IssueCreated {
                uuid,
                title: format!("Issue {uuid}"),
                description: None,
                priority: "medium".to_string(),
                labels: vec![],
                parent_uuid: None,
                created_by: agent_id.to_string(),
                display_id: None,
                scheduled_at: None,
                due_at: None,
            },
        )
    }

    #[test]
    fn parity_multi_agent_interleaved() {
        let hub_tmp = tempfile::tempdir().unwrap();
        let hub_dir = hub_tmp.path();
        setup_hub_layout(hub_dir);

        let uuid1 = Uuid::new_v4();
        let uuid2 = Uuid::new_v4();
        let uuid3 = Uuid::new_v4();
        let now = Utc::now();

        let mut e1 = make_issue_created("agent-1", 1, uuid1);
        e1.timestamp = now - Duration::seconds(30);
        let mut e2 = make_issue_created("agent-1", 2, uuid2);
        e2.timestamp = now - Duration::seconds(20);

        let mut e3 = make_issue_created("agent-2", 1, uuid3);
        e3.timestamp = now - Duration::seconds(25);
        let mut e4 = make_envelope(
            "agent-2",
            2,
            Event::LabelAdded {
                issue_uuid: uuid1,
                label: "bug".to_string(),
            },
        );
        e4.timestamp = now - Duration::seconds(15);

        let mut e5 = make_envelope(
            "agent-3",
            1,
            Event::StatusChanged {
                uuid: uuid2,
                new_status: "closed".to_string(),
                closed_at: Some(now - Duration::seconds(10)),
            },
        );
        e5.timestamp = now - Duration::seconds(10);

        write_agent_events(hub_dir, "agent-1", &[e1, e2]);
        write_agent_events(hub_dir, "agent-2", &[e3, e4]);
        write_agent_events(hub_dir, "agent-3", &[e5]);

        let worktree_outcome = reduce_worktree(hub_dir);

        let repo_tmp = tempfile::tempdir().unwrap();
        let repo_path = repo_tmp.path();
        let ref_name = "refs/crosslink/hub";
        git_commit_hub_layout(repo_path, hub_dir, ref_name);
        let obj_outcome = reduce_object_store(repo_path, ref_name);

        assert_outcomes_equal(&worktree_outcome, &obj_outcome, "multi-agent");
    }

    #[test]
    fn parity_lock_contention() {
        let hub_tmp = tempfile::tempdir().unwrap();
        let hub_dir = hub_tmp.path();
        setup_hub_layout(hub_dir);

        let now = Utc::now();

        let mut ea = make_envelope(
            "agent-a",
            1,
            Event::LockClaimed {
                issue_display_id: 1,
                branch: Some("feature/a".to_string()),
            },
        );
        ea.timestamp = now - Duration::seconds(10);

        let mut eb = make_envelope(
            "agent-b",
            1,
            Event::LockClaimed {
                issue_display_id: 1,
                branch: Some("feature/b".to_string()),
            },
        );
        eb.timestamp = now - Duration::seconds(8);

        let mut ec = make_envelope(
            "agent-c",
            1,
            Event::LockClaimed {
                issue_display_id: 1,
                branch: Some("feature/c".to_string()),
            },
        );
        ec.timestamp = now - Duration::seconds(6);

        write_agent_events(hub_dir, "agent-a", &[ea]);
        write_agent_events(hub_dir, "agent-b", &[eb]);
        write_agent_events(hub_dir, "agent-c", &[ec]);

        let worktree_outcome = reduce_worktree(hub_dir);

        let repo_tmp = tempfile::tempdir().unwrap();
        let repo_path = repo_tmp.path();
        let ref_name = "refs/crosslink/hub";
        git_commit_hub_layout(repo_path, hub_dir, ref_name);
        let obj_outcome = reduce_object_store(repo_path, ref_name);

        assert_outcomes_equal(&worktree_outcome, &obj_outcome, "lock-contention");

        assert_eq!(worktree_outcome.state.locks[&1].agent_id, "agent-a");
    }

    #[test]
    fn parity_incremental_with_watermark() {
        let hub_tmp = tempfile::tempdir().unwrap();
        let hub_dir = hub_tmp.path();
        setup_hub_layout(hub_dir);

        let uuid1 = Uuid::new_v4();
        let now = Utc::now();

        let mut e1 = make_issue_created("agent-1", 1, uuid1);
        e1.timestamp = now - Duration::seconds(20);
        write_agent_events(hub_dir, "agent-1", &[e1.clone()]);

        let wm = OrderingKey::from_envelope(&e1);
        let mut state = CheckpointState {
            watermark: Some(wm),
            ..Default::default()
        };

        state.issues.insert(
            uuid1,
            crate::checkpoint::CompactIssue {
                uuid: uuid1,
                display_id: Some(1),
                title: format!("Issue {uuid1}"),
                description: None,
                status: crate::models::IssueStatus::Open,
                priority: crate::models::Priority::Medium,
                parent_uuid: None,
                created_by: "agent-1".to_string(),
                created_at: e1.timestamp,
                updated_at: e1.timestamp,
                closed_at: None,
                scheduled_at: None,
                due_at: None,
                labels: std::collections::BTreeSet::new(),
                blockers: std::collections::BTreeSet::new(),
                related: std::collections::BTreeSet::new(),
                milestone_uuid: None,
                comments: std::collections::BTreeMap::new(),
                time_entries: std::collections::BTreeMap::new(),
            },
        );
        state.display_id_map.insert(uuid1, 1);
        state.next_display_id = 2;
        write_hub_checkpoint(hub_dir, &state);

        let uuid2 = Uuid::new_v4();
        let mut e2 = make_issue_created("agent-1", 2, uuid2);
        e2.timestamp = now - Duration::seconds(5);
        write_agent_events(hub_dir, "agent-1", &[e2]);

        let worktree_outcome = reduce_worktree(hub_dir);

        let repo_tmp = tempfile::tempdir().unwrap();
        let repo_path = repo_tmp.path();
        let ref_name = "refs/crosslink/hub";
        git_commit_hub_layout(repo_path, hub_dir, ref_name);
        let obj_outcome = reduce_object_store(repo_path, ref_name);

        assert_outcomes_equal(&worktree_outcome, &obj_outcome, "incremental");
        assert_eq!(worktree_outcome.events_processed, 1);
    }

    #[test]
    fn parity_full_compaction_no_checkpoint() {
        let hub_tmp = tempfile::tempdir().unwrap();
        let hub_dir = hub_tmp.path();
        setup_hub_layout(hub_dir);

        let uuid1 = Uuid::new_v4();
        let uuid2 = Uuid::new_v4();
        let now = Utc::now();

        let mut e1 = make_issue_created("agent-1", 1, uuid1);
        e1.timestamp = now - Duration::seconds(10);
        let mut e2 = make_issue_created("agent-2", 1, uuid2);
        e2.timestamp = now - Duration::seconds(5);

        write_agent_events(hub_dir, "agent-1", &[e1]);
        write_agent_events(hub_dir, "agent-2", &[e2]);

        let worktree_outcome = reduce_worktree(hub_dir);

        let repo_tmp = tempfile::tempdir().unwrap();
        let repo_path = repo_tmp.path();
        let ref_name = "refs/crosslink/hub";
        git_commit_hub_layout(repo_path, hub_dir, ref_name);
        let obj_outcome = reduce_object_store(repo_path, ref_name);

        assert_outcomes_equal(&worktree_outcome, &obj_outcome, "full-compaction");
        assert_eq!(worktree_outcome.events_processed, 2);
    }

    #[test]
    fn parity_unsigned_events_both_sources() {
        let hub_tmp = tempfile::tempdir().unwrap();
        let hub_dir = hub_tmp.path();
        setup_hub_layout(hub_dir);

        let trust_dir = hub_dir.join("trust");
        std::fs::create_dir_all(&trust_dir).unwrap();
        std::fs::write(
            trust_dir.join("allowed_signers"),
            "# empty allowed signers\n",
        )
        .unwrap();

        let uuid1 = Uuid::new_v4();
        let mut e1 = make_issue_created("agent-1", 1, uuid1);
        e1.timestamp = Utc::now() - Duration::seconds(5);

        write_agent_events(hub_dir, "agent-1", &[e1]);

        let worktree_outcome = reduce_worktree(hub_dir);
        assert!(!worktree_outcome.state.unsigned_event_warnings.is_empty());

        let repo_tmp = tempfile::tempdir().unwrap();
        let repo_path = repo_tmp.path();
        let ref_name = "refs/crosslink/hub";
        git_commit_hub_layout(repo_path, hub_dir, ref_name);
        let obj_outcome = reduce_object_store(repo_path, ref_name);

        assert_outcomes_equal(&worktree_outcome, &obj_outcome, "unsigned-events");
    }

    #[test]
    fn parity_empty_hub() {
        let hub_tmp = tempfile::tempdir().unwrap();
        let hub_dir = hub_tmp.path();

        std::fs::create_dir_all(hub_dir.join("issues")).unwrap();
        std::fs::create_dir_all(hub_dir.join("locks")).unwrap();
        std::fs::create_dir_all(hub_dir.join("checkpoint")).unwrap();
        crate::issue_file::write_layout_version(
            &hub_dir.join("meta"),
            crate::issue_file::CURRENT_LAYOUT_VERSION,
        )
        .unwrap();

        let worktree_outcome = reduce_worktree(hub_dir);
        assert_eq!(worktree_outcome.events_processed, 0);
        assert!(worktree_outcome.state.issues.is_empty());

        let commit_file = hub_dir.join(".gitkeep");
        std::fs::write(&commit_file, "").unwrap();

        let repo_tmp = tempfile::tempdir().unwrap();
        let repo_path = repo_tmp.path();
        let ref_name = "refs/crosslink/hub";
        git_commit_hub_layout(repo_path, hub_dir, ref_name);
        let obj_outcome = reduce_object_store(repo_path, ref_name);

        assert_outcomes_equal(&worktree_outcome, &obj_outcome, "empty-hub");
    }

    #[test]
    fn object_store_missing_ref_error_contains_ref_name() {
        let repo_tmp = tempfile::tempdir().unwrap();
        let repo_path = repo_tmp.path();

        run_git(repo_path, &["init"]);

        let ref_name = "refs/crosslink/hub";
        let result = ObjectStoreSource::new(repo_path, ref_name);
        assert!(result.is_err());
        let err = result.unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            msg.contains(ref_name),
            "error message should contain the ref name: {msg}"
        );
    }

    #[test]
    fn object_store_pinned_to_commit_at_construction() {
        let hub_tmp = tempfile::tempdir().unwrap();
        let hub_dir = hub_tmp.path();
        setup_hub_layout(hub_dir);

        let uuid1 = Uuid::new_v4();
        let mut e1 = make_issue_created("agent-1", 1, uuid1);
        e1.timestamp = Utc::now() - Duration::seconds(10);
        write_agent_events(hub_dir, "agent-1", &[e1]);

        let repo_tmp = tempfile::tempdir().unwrap();
        let repo_path = repo_tmp.path();
        let ref_name = "refs/crosslink/hub";
        let first_sha = git_commit_hub_layout(repo_path, hub_dir, ref_name);

        let source = ObjectStoreSource::new(repo_path, ref_name).unwrap();
        assert_eq!(source.commit_sha(), first_sha.trim());

        let uuid2 = Uuid::new_v4();
        let mut e2 = make_issue_created("agent-1", 2, uuid2);
        e2.timestamp = Utc::now() - Duration::seconds(5);
        write_agent_events(hub_dir, "agent-1", &[e2]);

        let second_sha = git_commit_hub_layout(repo_path, hub_dir, ref_name);
        assert_ne!(
            first_sha, second_sha,
            "ref must move to a new commit for the pinning assertion to be meaningful"
        );

        let outcome = crate::compaction::reduce(&source).unwrap();
        assert!(
            !outcome.state.issues.contains_key(&uuid2),
            "source should still see only the first commit's state"
        );
        assert!(outcome.state.issues.contains_key(&uuid1));
    }

    #[test]
    fn object_store_vanished_commit_errors_instead_of_empty() {
        let hub_tmp = tempfile::tempdir().unwrap();
        let hub_dir = hub_tmp.path();
        setup_hub_layout(hub_dir);

        let uuid1 = Uuid::new_v4();
        let mut e1 = make_issue_created("agent-1", 1, uuid1);
        e1.timestamp = Utc::now() - Duration::seconds(10);
        write_agent_events(hub_dir, "agent-1", &[e1]);

        let repo_tmp = tempfile::tempdir().unwrap();
        let repo_path = repo_tmp.path();
        let ref_name = "refs/crosslink/hub";
        git_commit_hub_layout(repo_path, hub_dir, ref_name);

        let source = ObjectStoreSource::new(repo_path, ref_name).unwrap();

        std::fs::remove_dir_all(repo_path.join(".git").join("objects")).unwrap();
        std::fs::create_dir_all(repo_path.join(".git").join("objects")).unwrap();

        let err = source
            .agent_ids()
            .expect_err("vanished commit must not read as an empty hub");
        let msg = format!("{err:?}");
        assert!(
            msg.contains(source.commit_sha()),
            "error should name the pinned commit, got: {msg}"
        );
        assert!(
            msg.contains("no longer exists"),
            "error should describe the vanished object store, got: {msg}"
        );

        let err = source
            .read_checkpoint()
            .expect_err("vanished commit must not read as a default checkpoint");
        let msg = format!("{err:?}");
        assert!(
            msg.contains("no longer exists"),
            "checkpoint read should describe the vanished object store, got: {msg}"
        );
    }

    fn ref_repo_init(path: &Path) {
        run_git(path, &["init"]);
        run_git(path, &["config", "user.email", "test@crosslink.test"]);
        run_git(path, &["config", "user.name", "Test"]);
    }

    fn log_bytes(events: &[EventEnvelope]) -> Vec<u8> {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("events.log");
        for ev in events {
            crate::events::append_event(&log_path, ev).unwrap();
        }
        std::fs::read(&log_path).unwrap()
    }

    #[test]
    fn ref_hub_source_reduces_genesis_plus_post_watermark_events() {
        let repo_tmp = tempfile::tempdir().unwrap();
        let repo = repo_tmp.path();
        ref_repo_init(repo);

        let now = Utc::now();
        let uuid_pre = Uuid::new_v4();
        let uuid_post1 = Uuid::new_v4();
        let uuid_post2 = Uuid::new_v4();

        let mut e_pre = make_issue_created("agent-1", 1, uuid_pre);
        e_pre.timestamp = now - Duration::seconds(60);
        let mut e_post1 = make_issue_created("agent-1", 2, uuid_post1);
        e_post1.timestamp = now - Duration::seconds(10);

        let mut e_post2 = make_issue_created("agent-2", 1, uuid_post2);
        e_post2.timestamp = now - Duration::seconds(5);

        crate::hub_v3::commit_log_bytes(
            repo,
            "agent-1",
            &log_bytes(&[e_pre.clone(), e_post1]),
            "agent-1 log",
        )
        .unwrap();
        crate::hub_v3::commit_log_bytes(repo, "agent-2", &log_bytes(&[e_post2]), "agent-2 log")
            .unwrap();

        let wm = OrderingKey::from_envelope(&e_pre);
        let mut state = CheckpointState {
            watermark: Some(wm),
            ..Default::default()
        };
        state.issues.insert(
            uuid_pre,
            crate::checkpoint::CompactIssue {
                uuid: uuid_pre,
                display_id: Some(1),
                title: format!("Issue {uuid_pre}"),
                description: None,
                status: crate::models::IssueStatus::Open,
                priority: crate::models::Priority::Medium,
                parent_uuid: None,
                created_by: "agent-1".to_string(),
                created_at: e_pre.timestamp,
                updated_at: e_pre.timestamp,
                closed_at: None,
                scheduled_at: None,
                due_at: None,
                labels: std::collections::BTreeSet::new(),
                blockers: std::collections::BTreeSet::new(),
                related: std::collections::BTreeSet::new(),
                milestone_uuid: None,
                comments: std::collections::BTreeMap::new(),
                time_entries: std::collections::BTreeMap::new(),
            },
        );
        state.display_id_map.insert(uuid_pre, 1);
        state.next_display_id = 2;
        let state_bytes = serde_json::to_vec(&state).unwrap();
        crate::hub_v3::commit_blob_to_ref(
            repo,
            crate::hub_v3::CHECKPOINT_REF,
            "state.json",
            &state_bytes,
            "genesis checkpoint",
        )
        .unwrap();

        let source = RefHubSource::new(repo).unwrap();
        let outcome = crate::compaction::reduce(&source).unwrap();

        assert_eq!(
            outcome.events_processed, 2,
            "only post-watermark events apply"
        );

        assert!(
            outcome.state.issues.contains_key(&uuid_pre),
            "genesis issue retained"
        );
        assert!(
            outcome.state.issues.contains_key(&uuid_post1),
            "post-wm issue 1 applied"
        );
        assert!(
            outcome.state.issues.contains_key(&uuid_post2),
            "post-wm issue 2 applied"
        );
        assert_eq!(outcome.state.issues.len(), 3);
    }

    #[test]
    fn ref_hub_source_no_checkpoint_defaults_and_reduces_all() {
        let repo_tmp = tempfile::tempdir().unwrap();
        let repo = repo_tmp.path();
        ref_repo_init(repo);

        let uuid1 = Uuid::new_v4();
        let mut e1 = make_issue_created("agent-1", 1, uuid1);
        e1.timestamp = Utc::now() - Duration::seconds(5);
        crate::hub_v3::commit_log_bytes(repo, "agent-1", &log_bytes(&[e1]), "log").unwrap();

        let source = RefHubSource::new(repo).unwrap();
        assert!(source.checkpoint_sha().is_none());
        let outcome = crate::compaction::reduce(&source).unwrap();
        assert_eq!(outcome.events_processed, 1);
        assert!(outcome.state.issues.contains_key(&uuid1));
    }

    #[test]
    fn ref_hub_source_pinned_to_tips_at_construction() {
        let repo_tmp = tempfile::tempdir().unwrap();
        let repo = repo_tmp.path();
        ref_repo_init(repo);

        let uuid1 = Uuid::new_v4();
        let mut e1 = make_issue_created("agent-1", 1, uuid1);
        e1.timestamp = Utc::now() - Duration::seconds(20);
        crate::hub_v3::commit_log_bytes(repo, "agent-1", &log_bytes(&[e1.clone()]), "log1")
            .unwrap();

        let source = RefHubSource::new(repo).unwrap();

        let uuid2 = Uuid::new_v4();
        let mut e2 = make_issue_created("agent-1", 2, uuid2);
        e2.timestamp = Utc::now() - Duration::seconds(5);
        crate::hub_v3::commit_log_bytes(repo, "agent-1", &log_bytes(&[e1, e2]), "log2").unwrap();

        let outcome = crate::compaction::reduce(&source).unwrap();
        assert!(outcome.state.issues.contains_key(&uuid1));
        assert!(
            !outcome.state.issues.contains_key(&uuid2),
            "pinned source must not see the post-construction ref move"
        );
    }

    #[test]
    fn ref_hub_source_vanished_commit_hard_errors() {
        let repo_tmp = tempfile::tempdir().unwrap();
        let repo = repo_tmp.path();
        ref_repo_init(repo);

        let uuid1 = Uuid::new_v4();
        let mut e1 = make_issue_created("agent-1", 1, uuid1);
        e1.timestamp = Utc::now() - Duration::seconds(5);
        crate::hub_v3::commit_log_bytes(repo, "agent-1", &log_bytes(&[e1]), "log").unwrap();

        let source = RefHubSource::new(repo).unwrap();

        std::fs::remove_dir_all(repo.join(".git").join("objects")).unwrap();
        std::fs::create_dir_all(repo.join(".git").join("objects")).unwrap();

        let err = source
            .read_events("agent-1", None)
            .expect_err("vanished agent-ref commit must hard-error, not read empty");
        let msg = format!("{err:?}");
        assert!(
            msg.contains("no longer exists"),
            "vanished commit must describe the pruned object store, got: {msg}"
        );
    }

    #[test]
    fn ref_hub_source_reads_allowed_signers_from_meta() {
        let repo_tmp = tempfile::tempdir().unwrap();
        let repo = repo_tmp.path();
        ref_repo_init(repo);

        let source = RefHubSource::new(repo).unwrap();
        assert!(source.allowed_signers_file().unwrap().is_none());

        crate::hub_v3::commit_files_to_ref(
            repo,
            crate::hub_v3::META_REF,
            &[("hub.json", b"{}\n"), ("allowed_signers", b"# signers\n")],
            "meta",
        )
        .unwrap();
        let source = RefHubSource::new(repo).unwrap();
        let path = source
            .allowed_signers_file()
            .unwrap()
            .expect("allowed_signers must be extracted from the meta ref");
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents, "# signers\n");
    }

    #[test]
    fn worktree_and_ref_hub_reduce_to_identical_state() {
        let hub_tmp = tempfile::tempdir().unwrap();
        let hub_dir = hub_tmp.path();
        setup_hub_layout(hub_dir);

        let now = Utc::now();
        let uuid1 = Uuid::new_v4();
        let uuid2 = Uuid::new_v4();
        let uuid3 = Uuid::new_v4();

        let mut e1 = make_issue_created("agent-1", 1, uuid1);
        e1.timestamp = now - Duration::seconds(30);
        let mut e2 = make_issue_created("agent-1", 2, uuid2);
        e2.timestamp = now - Duration::seconds(20);
        let mut e3 = make_issue_created("agent-2", 1, uuid3);
        e3.timestamp = now - Duration::seconds(25);
        let mut e4 = make_envelope(
            "agent-2",
            2,
            Event::LabelAdded {
                issue_uuid: uuid1,
                label: "bug".to_string(),
            },
        );
        e4.timestamp = now - Duration::seconds(15);

        write_agent_events(hub_dir, "agent-1", &[e1.clone(), e2.clone()]);
        write_agent_events(hub_dir, "agent-2", &[e3.clone(), e4.clone()]);

        let worktree_outcome = reduce_worktree(hub_dir);

        let repo_tmp = tempfile::tempdir().unwrap();
        let repo = repo_tmp.path();
        ref_repo_init(repo);
        crate::hub_v3::commit_log_bytes(repo, "agent-1", &log_bytes(&[e1, e2]), "a1").unwrap();
        crate::hub_v3::commit_log_bytes(repo, "agent-2", &log_bytes(&[e3, e4]), "a2").unwrap();

        let ref_source = RefHubSource::new(repo).unwrap();
        let ref_outcome = crate::compaction::reduce(&ref_source).unwrap();

        let wt = serde_json::to_value(&worktree_outcome.state).unwrap();
        let rf = serde_json::to_value(&ref_outcome.state).unwrap();
        assert_eq!(
            wt, rf,
            "WorktreeSource and RefHubSource must reduce to identical state"
        );
        assert_eq!(
            worktree_outcome.events_processed, ref_outcome.events_processed,
            "both sources must process the same number of events"
        );
    }
}
