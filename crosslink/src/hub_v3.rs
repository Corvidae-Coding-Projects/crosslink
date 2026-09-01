use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::process::Command;

use crate::events::{read_events_from_bytes, EventEnvelope};
use crate::utils::is_windows_reserved_name;

pub const AGENT_REF_PREFIX: &str = "refs/heads/crosslink/agents/";

pub const CHECKPOINT_REF: &str = "refs/heads/crosslink/checkpoint";

pub const META_REF: &str = "refs/heads/crosslink/meta";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum CasExpectation<'a> {
    MustNotExist,

    MustMatch(&'a str),

    CurrentTip,
}

pub fn agent_ref_name(agent_id: &str) -> Result<String> {
    validate_agent_id(agent_id)?;
    Ok(format!("{AGENT_REF_PREFIX}{agent_id}"))
}

pub const OLD_AGENT_REF_PREFIX: &str = "refs/crosslink/agents/";

pub const OLD_CHECKPOINT_REF: &str = "refs/crosslink/checkpoint";

pub const OLD_META_REF: &str = "refs/crosslink/meta";

#[must_use]
pub fn is_v3_hub_ref(ref_name: &str) -> bool {
    ref_name == CHECKPOINT_REF || ref_name == META_REF || ref_name.starts_with(AGENT_REF_PREFIX)
}

fn validate_agent_id(agent_id: &str) -> Result<()> {
    anyhow::ensure!(!agent_id.is_empty(), "agent_id cannot be empty");
    anyhow::ensure!(
        agent_id.len() >= 3,
        "agent_id must be at least 3 characters"
    );
    anyhow::ensure!(agent_id.len() <= 64, "agent_id must be <= 64 characters");
    anyhow::ensure!(
        agent_id
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_'),
        "agent_id must be alphanumeric with hyphens/underscores only, got: {agent_id}"
    );
    anyhow::ensure!(
        !is_windows_reserved_name(agent_id),
        "agent_id '{agent_id}' is a Windows reserved filename and cannot be used"
    );
    Ok(())
}

#[derive(Debug)]
#[allow(dead_code)]
pub struct RefAppendOutcome {
    pub new_commit: String,

    pub old_commit: Option<String>,

    pub events_in_log: usize,
}

#[derive(Debug)]
pub enum PushOutcome {
    Pushed,

    NonFastForward,

    NoRemote,

    Failed(String),
}

#[cfg(test)]
#[derive(Clone, Copy)]
pub(crate) enum AbortPoint {
    HashObject,

    Mktree,

    CommitTree,
}

pub fn append_event_to_ref(
    repo_dir: &Path,
    agent_id: &str,
    envelope: &EventEnvelope,
) -> Result<RefAppendOutcome> {
    #[cfg(test)]
    return append_inner(repo_dir, agent_id, envelope, None);
    #[cfg(not(test))]
    append_inner(repo_dir, agent_id, envelope)
}

#[cfg(test)]
fn append_inner(
    repo_dir: &Path,
    agent_id: &str,
    envelope: &EventEnvelope,
    abort: Option<AbortPoint>,
) -> Result<RefAppendOutcome> {
    append_inner_impl(repo_dir, agent_id, envelope, abort)
}

#[cfg(not(test))]
fn append_inner(
    repo_dir: &Path,
    agent_id: &str,
    envelope: &EventEnvelope,
) -> Result<RefAppendOutcome> {
    append_inner_impl(repo_dir, agent_id, envelope, None::<()>)
}

fn append_inner_impl<A: IntoAbortPoint>(
    repo_dir: &Path,
    agent_id: &str,
    envelope: &EventEnvelope,
    abort_opt: A,
) -> Result<RefAppendOutcome> {
    validate_agent_id(agent_id)?;
    let ref_name = format!("{AGENT_REF_PREFIX}{agent_id}");

    let old_commit = git_rev_parse_optional(repo_dir, &ref_name)?;

    let existing_bytes: Vec<u8> = match &old_commit {
        None => Vec::new(),
        Some(sha) => {
            let spec = format!("{sha}:events.log");
            git_cat_file_blob_optional(repo_dir, &spec)?.unwrap_or_default()
        }
    };

    let existing_events = read_events_from_bytes(&existing_bytes)
        .with_context(|| format!("corrupt events.log on ref '{ref_name}'; refusing to extend"))?;
    let events_in_log = existing_events.len() + 1;

    let new_line = serde_json::to_string(envelope).context("failed to serialise event envelope")?;
    let mut new_bytes = existing_bytes;
    new_bytes.extend_from_slice(new_line.as_bytes());
    new_bytes.push(b'\n');

    let blob_sha = git_hash_object(repo_dir, &new_bytes)?;

    if abort_opt.should_abort_after_hash_object() {
        return Ok(RefAppendOutcome {
            new_commit: String::new(),
            old_commit,
            events_in_log,
        });
    }

    let tree_sha = write_tree_with(
        repo_dir,
        old_commit.as_deref(),
        &[("events.log", BlobRef::Existing(&blob_sha))],
        &[],
    )?;

    if abort_opt.should_abort_after_mktree() {
        return Ok(RefAppendOutcome {
            new_commit: String::new(),
            old_commit,
            events_in_log,
        });
    }

    let commit_msg = format!(
        "crosslink event: agent {} seq {}",
        agent_id, envelope.agent_seq
    );
    let commit_sha = git_commit_tree(
        repo_dir,
        &tree_sha,
        old_commit.as_deref(),
        &commit_msg,
        agent_id,
    )?;

    if abort_opt.should_abort_after_commit_tree() {
        return Ok(RefAppendOutcome {
            new_commit: String::new(),
            old_commit,
            events_in_log,
        });
    }

    git_update_ref_cas(repo_dir, &ref_name, &commit_sha, old_commit.as_deref())?;

    Ok(RefAppendOutcome {
        new_commit: commit_sha,
        old_commit,
        events_in_log,
    })
}

pub fn commit_log_bytes(
    repo_dir: &Path,
    agent_id: &str,
    log_bytes: &[u8],
    message: &str,
) -> Result<String> {
    validate_agent_id(agent_id)?;
    let ref_name = format!("{AGENT_REF_PREFIX}{agent_id}");

    read_events_from_bytes(log_bytes)
        .context("refusing to commit unparseable events.log bytes to the agent ref")?;

    commit_single_file_tree(
        repo_dir,
        &ref_name,
        "events.log",
        log_bytes,
        message,
        agent_id,
        CasExpectation::CurrentTip,
    )
}

trait IntoAbortPoint: Copy {
    fn should_abort_after_hash_object(self) -> bool;
    fn should_abort_after_mktree(self) -> bool;
    fn should_abort_after_commit_tree(self) -> bool;
}

impl IntoAbortPoint for Option<()> {
    fn should_abort_after_hash_object(self) -> bool {
        false
    }
    fn should_abort_after_mktree(self) -> bool {
        false
    }
    fn should_abort_after_commit_tree(self) -> bool {
        false
    }
}

#[cfg(test)]
impl IntoAbortPoint for Option<AbortPoint> {
    fn should_abort_after_hash_object(self) -> bool {
        matches!(self, Some(AbortPoint::HashObject))
    }
    fn should_abort_after_mktree(self) -> bool {
        matches!(self, Some(AbortPoint::Mktree))
    }
    fn should_abort_after_commit_tree(self) -> bool {
        matches!(self, Some(AbortPoint::CommitTree))
    }
}

pub fn push_agent_ref(repo_dir: &Path, remote: &str, agent_id: &str) -> Result<PushOutcome> {
    validate_agent_id(agent_id)?;
    let ref_name = agent_ref_name(agent_id)?;
    push_ref(repo_dir, remote, &ref_name)
}

#[allow(dead_code)]
pub fn push_ref(repo_dir: &Path, remote: &str, ref_name: &str) -> Result<PushOutcome> {
    let refspec = format!("{ref_name}:{ref_name}");

    let output = Command::new("git")
        .current_dir(repo_dir)
        .args(["push", remote, &refspec])
        .output()
        .with_context(|| format!("failed to run git push for ref '{ref_name}'"))?;

    Ok(classify_push_output(&output))
}

#[allow(dead_code)]
pub fn push_ref_with_lease(
    repo_dir: &Path,
    remote: &str,
    ref_name: &str,
    expected_remote: Option<&str>,
) -> Result<PushOutcome> {
    let lease = expected_remote.map_or_else(
        || format!("--force-with-lease={ref_name}"),
        |sha| format!("--force-with-lease={ref_name}:{sha}"),
    );
    let refspec = format!("{ref_name}:{ref_name}");

    let output = Command::new("git")
        .current_dir(repo_dir)
        .args(["push", &lease, remote, &refspec])
        .output()
        .with_context(|| {
            format!("failed to run git push --force-with-lease for ref '{ref_name}'")
        })?;

    Ok(classify_push_output(&output))
}

fn classify_push_output(output: &std::process::Output) -> PushOutcome {
    if output.status.success() {
        return PushOutcome::Pushed;
    }

    let stderr = String::from_utf8_lossy(&output.stderr);

    if stderr.contains("non-fast-forward")
        || stderr.contains("rejected")
        || stderr.contains("stale info")
    {
        return PushOutcome::NonFastForward;
    }

    if stderr.contains("does not appear to be a git repository")
        || stderr.contains("repository not found")
        || stderr.contains("Could not read from remote repository")
        || stderr.contains("No such remote")
        || stderr.contains('\'') && stderr.contains("' does not")
    {
        return PushOutcome::NoRemote;
    }

    PushOutcome::Failed(stderr.trim().to_string())
}

const V2_HUB_BRANCH: &str = "refs/heads/crosslink/hub";

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub enum HubVersion {
    V2Only,

    V3 { v2_branch_present: bool },

    Absent,
}

#[allow(dead_code)]
pub fn detect_hub_version(repo_dir: &Path) -> Result<HubVersion> {
    let meta = git_rev_parse_optional(repo_dir, META_REF)?.is_some();
    let checkpoint = git_rev_parse_optional(repo_dir, CHECKPOINT_REF)?.is_some();
    let v2 = git_rev_parse_optional(repo_dir, V2_HUB_BRANCH)?.is_some();

    Ok(classify_hub_version(meta, checkpoint, v2))
}

#[allow(dead_code)]
pub fn detect_remote_hub_version(repo_dir: &Path, remote: &str) -> Result<HubVersion> {
    let output = Command::new("git")
        .current_dir(repo_dir)
        .args([
            "ls-remote",
            remote,
            "refs/heads/crosslink/*",
            "refs/crosslink/*",
        ])
        .output()
        .with_context(|| format!("failed to run git ls-remote for remote '{remote}'"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "git ls-remote failed for remote '{remote}' (cannot determine remote hub version; \
             remote unreachable, unauthenticated, or unknown): {}",
            stderr.trim()
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut meta = false;
    let mut checkpoint = false;
    let mut v2 = false;

    for line in stdout.lines() {
        let Some((_, refname)) = line.split_once('\t') else {
            continue;
        };
        let refname = refname.trim();
        match refname {
            META_REF | OLD_META_REF => meta = true,
            CHECKPOINT_REF | OLD_CHECKPOINT_REF => checkpoint = true,
            V2_HUB_BRANCH => v2 = true,
            _ => {}
        }
    }

    Ok(classify_hub_version(meta, checkpoint, v2))
}

const fn classify_hub_version(
    meta_present: bool,
    checkpoint_present: bool,
    v2_present: bool,
) -> HubVersion {
    if meta_present && checkpoint_present {
        HubVersion::V3 {
            v2_branch_present: v2_present,
        }
    } else if v2_present {
        HubVersion::V2Only
    } else {
        HubVersion::Absent
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HubMode {
    V2,

    V3,
}

impl HubMode {
    #[must_use]
    pub fn resolve(repo_dir: &Path) -> Self {
        match detect_hub_version(repo_dir) {
            Ok(HubVersion::V3 { .. }) => HubMode::V3,
            Ok(HubVersion::V2Only | HubVersion::Absent) => HubMode::V2,
            Err(e) => {
                tracing::debug!(
                    "HubMode::resolve: detect_hub_version failed for {}: {e}; defaulting to V2",
                    repo_dir.display()
                );
                HubMode::V2
            }
        }
    }

    #[must_use]
    pub const fn is_v3(self) -> bool {
        matches!(self, HubMode::V3)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[allow(dead_code)]
pub struct HubMeta {
    pub hub_version: u32,

    pub migrated_from_commit: String,

    pub migrated_at: chrono::DateTime<chrono::Utc>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finalized_at: Option<chrono::DateTime<chrono::Utc>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub genesis_checkpoint_commit: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed_agent_tips: Option<std::collections::BTreeMap<String, String>>,
}

#[allow(dead_code)]
pub fn read_hub_meta(repo_dir: &Path) -> Result<Option<HubMeta>> {
    let Some(tip) = git_rev_parse_optional(repo_dir, META_REF)? else {
        return Ok(None);
    };
    let spec = format!("{tip}:hub.json");
    let Some(bytes) = git_cat_file_blob_optional(repo_dir, &spec)? else {
        return Ok(None);
    };
    let meta: HubMeta = serde_json::from_slice(&bytes)
        .context("failed to parse hub.json on the meta ref as HubMeta")?;
    Ok(Some(meta))
}

#[allow(dead_code)]
pub fn write_heartbeat_to_ref(
    repo_dir: &Path,
    agent_id: &str,
    heartbeat: &crate::locks::Heartbeat,
) -> Result<String> {
    validate_agent_id(agent_id)?;
    let ref_name = format!("{AGENT_REF_PREFIX}{agent_id}");
    let bytes = serde_json::to_vec_pretty(heartbeat)
        .context("failed to serialize heartbeat for the agent ref")?;
    let message = format!("crosslink heartbeat: agent {agent_id}");
    commit_upserts_to_ref(
        repo_dir,
        &ref_name,
        &[("heartbeat.json", BlobRef::Bytes(&bytes))],
        &[],
        &message,
        agent_id,
        CasExpectation::CurrentTip,
    )
}

#[allow(dead_code)]
pub fn read_heartbeats_from_refs(
    repo_dir: &Path,
) -> Result<Vec<(String, crate::locks::Heartbeat)>> {
    let mut out = Vec::new();
    for ref_name in for_each_agent_ref(repo_dir)? {
        let Some(agent_id) = ref_name.strip_prefix(AGENT_REF_PREFIX) else {
            continue;
        };
        let Some(tip) = git_rev_parse_optional(repo_dir, &ref_name)? else {
            continue;
        };
        let spec = format!("{tip}:heartbeat.json");
        let Some(bytes) = git_cat_file_blob_optional(repo_dir, &spec)? else {
            continue;
        };
        let hb: crate::locks::Heartbeat = serde_json::from_slice(&bytes).with_context(|| {
            format!("failed to parse heartbeat.json on ref '{ref_name}' as Heartbeat")
        })?;
        out.push((agent_id.to_string(), hb));
    }
    Ok(out)
}

const REQUESTS_OUT_DIR: &str = "requests-out";

const REQUESTS_ACK_DIR: &str = "requests-ack";

fn request_out_path(target_agent_id: &str, request_id: &str) -> String {
    format!("{REQUESTS_OUT_DIR}/{target_agent_id}--{request_id}.json")
}

fn parse_request_out_name(file_name: &str) -> Option<(String, String)> {
    let stem = file_name.strip_suffix(".json")?;
    let (target, ulid) = stem.rsplit_once("--")?;
    if target.is_empty() || ulid.is_empty() {
        return None;
    }
    Some((target.to_string(), ulid.to_string()))
}

#[allow(dead_code)]
pub fn write_request_to_own_ref(
    repo_dir: &Path,
    driver_agent_id: &str,
    target_agent_id: &str,
    request: &crate::agent_requests::AgentRequest,
) -> Result<String> {
    validate_agent_id(driver_agent_id)?;
    validate_agent_id(target_agent_id)?;
    let ref_name = format!("{AGENT_REF_PREFIX}{driver_agent_id}");
    let path = request_out_path(target_agent_id, &request.request_id);
    let bytes = serde_json::to_vec_pretty(request).context("failed to serialize agent request")?;
    let message = format!(
        "crosslink request: {driver_agent_id} -> {target_agent_id} ({})",
        request.request_id
    );
    commit_upserts_to_ref(
        repo_dir,
        &ref_name,
        &[(&path, BlobRef::Bytes(&bytes))],
        &[],
        &message,
        driver_agent_id,
        CasExpectation::CurrentTip,
    )
}

#[allow(dead_code)]
pub fn write_ack_to_own_ref(
    repo_dir: &Path,
    my_agent_id: &str,
    request_id: &str,
    ack: &crate::agent_requests::AgentRequestAck,
) -> Result<String> {
    validate_agent_id(my_agent_id)?;
    let ref_name = format!("{AGENT_REF_PREFIX}{my_agent_id}");
    let path = format!("{REQUESTS_ACK_DIR}/{request_id}.json");
    let bytes = serde_json::to_vec_pretty(ack).context("failed to serialize agent request ack")?;
    let message = format!("crosslink ack: {my_agent_id} ({request_id})");
    commit_upserts_to_ref(
        repo_dir,
        &ref_name,
        &[(&path, BlobRef::Bytes(&bytes))],
        &[],
        &message,
        my_agent_id,
        CasExpectation::CurrentTip,
    )
}

#[allow(dead_code)]
pub fn poll_requests_for_agent(
    repo_dir: &Path,
    my_agent_id: &str,
) -> Result<Vec<(String, crate::agent_requests::AgentRequest)>> {
    validate_agent_id(my_agent_id)?;

    let my_ref = format!("{AGENT_REF_PREFIX}{my_agent_id}");
    let acked: std::collections::HashSet<String> = match git_rev_parse_optional(repo_dir, &my_ref)?
    {
        None => std::collections::HashSet::new(),
        Some(tip) => list_subtree_leaf_stems(repo_dir, &tip, REQUESTS_ACK_DIR)?
            .into_iter()
            .collect(),
    };

    let mut out = Vec::new();
    for ref_name in for_each_agent_ref(repo_dir)? {
        let Some(driver_id) = ref_name.strip_prefix(AGENT_REF_PREFIX) else {
            continue;
        };
        let Some(tip) = git_rev_parse_optional(repo_dir, &ref_name)? else {
            continue;
        };
        for leaf in list_subtree_leaf_names(repo_dir, &tip, REQUESTS_OUT_DIR)? {
            let Some((target, ulid)) = parse_request_out_name(&leaf) else {
                continue;
            };
            if target != my_agent_id {
                continue;
            }
            if acked.contains(&ulid) {
                continue;
            }
            let spec = format!("{tip}:{REQUESTS_OUT_DIR}/{leaf}");
            let Some(bytes) = git_cat_file_blob_optional(repo_dir, &spec)? else {
                continue;
            };
            let request: crate::agent_requests::AgentRequest = serde_json::from_slice(&bytes)
                .with_context(|| format!("failed to parse request '{leaf}' on ref '{ref_name}'"))?;
            out.push((driver_id.to_string(), request));
        }
    }

    out.sort_by(|a, b| a.1.request_id.cmp(&b.1.request_id));
    Ok(out)
}

pub fn read_all_agent_requests(
    repo_dir: &Path,
) -> Result<Vec<(String, Vec<crate::agent_requests::RequestWithAck>)>> {
    use std::collections::BTreeMap;

    let mut by_target: BTreeMap<String, Vec<(String, crate::agent_requests::AgentRequest)>> =
        BTreeMap::new();

    let mut acks: BTreeMap<String, BTreeMap<String, crate::agent_requests::AgentRequestAck>> =
        BTreeMap::new();

    for ref_name in for_each_agent_ref(repo_dir)? {
        let Some(owner) = ref_name.strip_prefix(AGENT_REF_PREFIX) else {
            continue;
        };
        let Some(tip) = git_rev_parse_optional(repo_dir, &ref_name)? else {
            continue;
        };

        for leaf in list_subtree_leaf_names(repo_dir, &tip, REQUESTS_OUT_DIR)? {
            let Some((target, ulid)) = parse_request_out_name(&leaf) else {
                continue;
            };
            let spec = format!("{tip}:{REQUESTS_OUT_DIR}/{leaf}");
            let Some(bytes) = git_cat_file_blob_optional(repo_dir, &spec)? else {
                continue;
            };
            let request: crate::agent_requests::AgentRequest = serde_json::from_slice(&bytes)
                .with_context(|| format!("failed to parse request '{leaf}' on '{ref_name}'"))?;
            by_target.entry(target).or_default().push((ulid, request));
        }

        for leaf in list_subtree_leaf_names(repo_dir, &tip, REQUESTS_ACK_DIR)? {
            let spec = format!("{tip}:{REQUESTS_ACK_DIR}/{leaf}");
            let Some(bytes) = git_cat_file_blob_optional(repo_dir, &spec)? else {
                continue;
            };
            let ack: crate::agent_requests::AgentRequestAck = serde_json::from_slice(&bytes)
                .with_context(|| format!("failed to parse ack '{leaf}' on '{ref_name}'"))?;
            acks.entry(owner.to_string())
                .or_default()
                .insert(ack.request_id.clone(), ack);
        }
    }

    let mut out = Vec::new();
    for (target, mut requests) in by_target {
        requests.sort_by(|a, b| a.0.cmp(&b.0));
        let target_acks = acks.get(&target);
        let paired = requests
            .into_iter()
            .map(|(ulid, request)| crate::agent_requests::RequestWithAck {
                request,
                ack: target_acks.and_then(|m| m.get(&ulid).cloned()),
            })
            .collect();
        out.push((target, paired));
    }
    Ok(out)
}

pub fn read_max_event_seq_from_ref(repo_dir: &Path, agent_id: &str) -> Result<u64> {
    validate_agent_id(agent_id)?;
    let ref_name = format!("{AGENT_REF_PREFIX}{agent_id}");
    let Some(tip) = git_rev_parse_optional(repo_dir, &ref_name)? else {
        return Ok(0);
    };
    let spec = format!("{tip}:events.log");
    let Some(bytes) = git_cat_file_blob_optional(repo_dir, &spec)? else {
        return Ok(0);
    };
    let events = read_events_from_bytes(&bytes)
        .with_context(|| format!("failed to parse events.log on '{ref_name}' for seq init"))?;
    Ok(events.iter().map(|e| e.agent_seq).max().unwrap_or(0))
}

fn for_each_agent_ref(repo_dir: &Path) -> Result<Vec<String>> {
    let pattern = format!("{AGENT_REF_PREFIX}*");
    let output = Command::new("git")
        .current_dir(repo_dir)
        .args(["for-each-ref", "--format=%(refname)", &pattern])
        .output()
        .with_context(|| format!("failed to run git for-each-ref for '{pattern}'"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "git for-each-ref failed for '{}': {}",
            pattern,
            stderr.trim()
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect())
}

fn list_subtree_leaf_names(repo_dir: &Path, commit_sha: &str, dir: &str) -> Result<Vec<String>> {
    let spec = format!("{commit_sha}:{dir}");
    let output = Command::new("git")
        .current_dir(repo_dir)
        .args(["ls-tree", "--name-only", &spec])
        .output()
        .with_context(|| format!("failed to run git ls-tree for '{spec}'"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);

        if stderr.contains("Not a valid object name")
            || stderr.contains("does not exist")
            || stderr.contains("not a tree")
            || stderr.contains("Not a tree")
        {
            return Ok(Vec::new());
        }
        anyhow::bail!("git ls-tree failed for '{}': {}", spec, stderr.trim());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect())
}

fn list_subtree_leaf_stems(repo_dir: &Path, commit_sha: &str, dir: &str) -> Result<Vec<String>> {
    Ok(list_subtree_leaf_names(repo_dir, commit_sha, dir)?
        .into_iter()
        .filter_map(|n| n.strip_suffix(".json").map(str::to_string))
        .collect())
}

#[derive(serde::Serialize)]
struct BrowseIssue<'a> {
    uuid: uuid::Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    display_id: Option<i64>,
    title: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<&'a str>,
    status: crate::models::IssueStatus,
    priority: crate::models::Priority,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_uuid: Option<uuid::Uuid>,
    created_by: &'a str,
    created_at: chrono::DateTime<chrono::Utc>,
    updated_at: chrono::DateTime<chrono::Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    closed_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    scheduled_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    due_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    labels: Vec<&'a str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    blockers: Vec<uuid::Uuid>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    related: Vec<uuid::Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    milestone_uuid: Option<uuid::Uuid>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    comments: Vec<BrowseComment<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    time_entries: Vec<BrowseTimeEntry>,
}

#[derive(serde::Serialize)]
struct BrowseComment<'a> {
    uuid: uuid::Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    display_id: Option<i64>,
    author: &'a str,
    content: &'a str,
    created_at: chrono::DateTime<chrono::Utc>,
    kind: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    trigger_type: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    intervention_context: Option<&'a str>,
}

#[derive(serde::Serialize)]
struct BrowseTimeEntry {
    uuid: uuid::Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    display_id: Option<i64>,
    started_at: chrono::DateTime<chrono::Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ended_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_seconds: Option<i64>,
}

fn render_browse_issue(issue: &crate::checkpoint::CompactIssue) -> Result<Vec<u8>> {
    let mut comments: Vec<BrowseComment<'_>> = issue
        .comments
        .iter()
        .map(|(uuid, c)| BrowseComment {
            uuid: *uuid,
            display_id: c.display_id,
            author: &c.author,
            content: &c.content,
            created_at: c.created_at,
            kind: &c.kind,
            trigger_type: c.trigger_type.as_deref(),
            intervention_context: c.intervention_context.as_deref(),
        })
        .collect();
    comments.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.uuid.cmp(&b.uuid)));

    let mut time_entries: Vec<BrowseTimeEntry> = issue
        .time_entries
        .iter()
        .map(|(uuid, t)| BrowseTimeEntry {
            uuid: *uuid,
            display_id: t.display_id,
            started_at: t.started_at,
            ended_at: t.ended_at,
            duration_seconds: t.duration_seconds,
        })
        .collect();
    time_entries.sort_by(|a, b| a.started_at.cmp(&b.started_at).then(a.uuid.cmp(&b.uuid)));

    let browse = BrowseIssue {
        uuid: issue.uuid,
        display_id: issue.display_id,
        title: &issue.title,
        description: issue.description.as_deref(),
        status: issue.status,
        priority: issue.priority,
        parent_uuid: issue.parent_uuid,
        created_by: &issue.created_by,
        created_at: issue.created_at,
        updated_at: issue.updated_at,
        closed_at: issue.closed_at,
        scheduled_at: issue.scheduled_at,
        due_at: issue.due_at,
        labels: issue.labels.iter().map(String::as_str).collect(),
        blockers: issue.blockers.iter().copied().collect(),
        related: issue.related.iter().copied().collect(),
        milestone_uuid: issue.milestone_uuid,
        comments,
        time_entries,
    };
    serde_json::to_vec_pretty(&browse).context("failed to render browse issue JSON")
}

#[derive(serde::Serialize)]
struct BrowseMilestones<'a> {
    milestones: Vec<&'a crate::checkpoint::CompactMilestone>,
}

fn render_browse_milestones(state: &crate::checkpoint::CheckpointState) -> Result<Vec<u8>> {
    let milestones: Vec<&crate::checkpoint::CompactMilestone> = state.milestones.values().collect();
    serde_json::to_vec_pretty(&BrowseMilestones { milestones })
        .context("failed to render browse milestones JSON")
}

fn render_browse_readme(state: &crate::checkpoint::CheckpointState) -> Vec<u8> {
    let live_issues = state
        .issues
        .keys()
        .filter(|u| !state.deleted_issues.contains(u))
        .count();
    let milestones = state.milestones.len();
    let watermark = state
        .watermark
        .as_ref()
        .map_or_else(|| "(none)".to_string(), |w| w.timestamp.to_rfc3339());

    let body = format!(
        "# crosslink hub (v3 checkpoint)\n\
\n\
This branch is the **machine-written** materialized state of the crosslink issue\n\
tracker. It is regenerated by compaction after every mutation — do not edit it by\n\
hand; manual changes are overwritten on the next compaction.\n\
\n\
## What lives here\n\
\n\
- `state.json` — the compacted [`CheckpointState`] (the authoritative cache the\n\
  CLI hydrates from).\n\
- `issues/<uuid>.json` — one rendered file per live issue, comments and time\n\
  entries inline. Browse them directly in the GitHub web UI.\n\
- `meta/milestones.json` — the milestone registry.\n\
\n\
The append-only **event logs** that this state is reduced from live on the\n\
per-agent branches `crosslink/agents/<agent-id>`. Each agent is the single writer\n\
of its own branch.\n\
\n\
## Snapshot\n\
\n\
- Live issues: {live_issues}\n\
- Milestones: {milestones}\n\
- Compaction watermark: {watermark}\n"
    );
    body.into_bytes()
}

fn browse_tree_present(repo_dir: &Path, tip: &str) -> Result<bool> {
    let spec = format!("{tip}:README.md");
    Ok(git_cat_file_blob_optional(repo_dir, &spec)?.is_some())
}

type BrowseOps = (Vec<(String, Vec<u8>)>, Vec<String>);

fn build_browse_ops(
    state: &crate::checkpoint::CheckpointState,
    changed: &std::collections::HashSet<uuid::Uuid>,
    full: bool,
) -> Result<BrowseOps> {
    let mut upserts: Vec<(String, Vec<u8>)> = Vec::new();
    let mut deletes: Vec<String> = Vec::new();

    let render_path = |uuid: &uuid::Uuid| -> String { format!("issues/{uuid}.json") };

    if full {
        for (uuid, issue) in &state.issues {
            if state.deleted_issues.contains(uuid) {
                continue;
            }
            upserts.push((render_path(uuid), render_browse_issue(issue)?));
        }
    } else {
        for uuid in changed {
            if state.deleted_issues.contains(uuid) {
                deletes.push(render_path(uuid));
            } else if let Some(issue) = state.issues.get(uuid) {
                upserts.push((render_path(uuid), render_browse_issue(issue)?));
            }
        }
    }

    upserts.push((
        "meta/milestones.json".to_string(),
        render_browse_milestones(state)?,
    ));
    upserts.push(("README.md".to_string(), render_browse_readme(state)));

    Ok((upserts, deletes))
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct CompactV3Result {
    pub events_processed: usize,

    pub checkpoint_commit: Option<String>,

    pub checkpoint_pushed: bool,

    pub events_pruned: usize,
}

#[allow(dead_code)]
pub fn compact_v3(
    repo_dir: &Path,
    agent_id: &str,
    _hub_lock: &crate::sync::HubWriteLock,
    remote: Option<&str>,
) -> Result<CompactV3Result> {
    validate_agent_id(agent_id)?;

    let source = crate::hub_source::RefHubSource::new(repo_dir)
        .context("failed to construct RefHubSource for v3 compaction")?;
    let outcome = crate::compaction::reduce(&source).context("v3 reduction failed")?;
    let events_processed = outcome.events_processed;
    let watermark = outcome.state.watermark.clone();

    let mut state = outcome.state;
    state.compaction_lease = None;
    let state_bytes =
        serde_json::to_vec_pretty(&state).context("failed to serialize v3 checkpoint state")?;

    let existing_tip = git_rev_parse_optional(repo_dir, CHECKPOINT_REF)?;
    let browse_present = match &existing_tip {
        Some(tip) => browse_tree_present(repo_dir, tip)?,
        None => false,
    };

    if browse_present {
        if let Some(tip) = &existing_tip {
            let spec = format!("{tip}:state.json");
            if let Some(existing_bytes) = git_cat_file_blob_optional(repo_dir, &spec)? {
                if existing_bytes == state_bytes {
                    tracing::debug!(
                        "v3 compaction: checkpoint already current (byte-identical); no-op"
                    );
                    return Ok(CompactV3Result {
                        events_processed,
                        checkpoint_commit: Some(tip.clone()),
                        checkpoint_pushed: false,
                        events_pruned: 0,
                    });
                }
            }
        }
    }

    let (browse_upserts, browse_deletes) =
        build_browse_ops(&state, &outcome.changed_issues, !browse_present)?;

    let mut upserts: Vec<(&str, BlobRef<'_>)> = vec![("state.json", BlobRef::Bytes(&state_bytes))];
    for (path, bytes) in &browse_upserts {
        upserts.push((path.as_str(), BlobRef::Bytes(bytes)));
    }
    let deletes: Vec<&str> = browse_deletes.iter().map(String::as_str).collect();

    let checkpoint_commit = match commit_upserts_to_ref(
        repo_dir,
        CHECKPOINT_REF,
        &upserts,
        &deletes,
        "crosslink v3 checkpoint",
        "crosslink",
        CasExpectation::CurrentTip,
    ) {
        Ok(sha) => Some(sha),
        Err(e) => {
            let msg = format!("{e:?}");
            if msg.contains("ref moved concurrently") {
                tracing::debug!(
                    "v3 compaction: checkpoint CAS lost to a concurrent compactor (benign): {msg}"
                );
                return Ok(CompactV3Result {
                    events_processed,
                    checkpoint_commit: None,
                    checkpoint_pushed: false,
                    events_pruned: 0,
                });
            }
            return Err(e).context("failed to commit v3 checkpoint");
        }
    };

    let checkpoint_pushed = match remote {
        None => false,
        Some(rem) => {
            let expected = remote_checkpoint_sha(repo_dir, rem);
            match push_ref_with_lease(repo_dir, rem, CHECKPOINT_REF, expected.as_deref())? {
                PushOutcome::Pushed => true,
                PushOutcome::NonFastForward => {
                    tracing::debug!(
                        "v3 compaction: checkpoint lease lost (benign, deterministic content); \
                         skipping prune this cycle"
                    );
                    false
                }
                other => {
                    tracing::debug!(
                        "v3 compaction: checkpoint push did not succeed ({other:?}); \
                         skipping prune this cycle"
                    );
                    false
                }
            }
        }
    };

    let prune_ok = checkpoint_commit.is_some() && (remote.is_none() || checkpoint_pushed);
    let events_pruned = if prune_ok {
        match &watermark {
            Some(wm) => prune_own_ref(repo_dir, agent_id, wm)?,
            None => 0,
        }
    } else {
        0
    };

    if events_pruned > 0 {
        if let Some(rem) = remote {
            let ref_name = format!("{AGENT_REF_PREFIX}{agent_id}");
            let refspec = format!("+{ref_name}:{ref_name}");
            match Command::new("git")
                .current_dir(repo_dir)
                .args(["push", rem, &refspec])
                .output()
            {
                Ok(out) if out.status.success() => {}
                Ok(out) => tracing::debug!(
                    "v3 compaction: own-ref prune force-push did not complete (benign): {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
                Err(e) => tracing::debug!(
                    "v3 compaction: own-ref prune force-push could not run (benign): {e}"
                ),
            }
        }
    }

    Ok(CompactV3Result {
        events_processed,
        checkpoint_commit,
        checkpoint_pushed,
        events_pruned,
    })
}

fn remote_checkpoint_sha(repo_dir: &Path, _remote: &str) -> Option<String> {
    let tracking = "refs/crosslink-remote/checkpoint";
    git_rev_parse_optional(repo_dir, tracking).ok().flatten()
}

fn prune_own_ref(
    repo_dir: &Path,
    agent_id: &str,
    watermark: &crate::events::OrderingKey,
) -> Result<usize> {
    let ref_name = format!("{AGENT_REF_PREFIX}{agent_id}");
    let Some(tip) = git_rev_parse_optional(repo_dir, &ref_name)? else {
        return Ok(0);
    };
    let spec = format!("{tip}:events.log");
    let Some(bytes) = git_cat_file_blob_optional(repo_dir, &spec)? else {
        return Ok(0);
    };
    let all = read_events_from_bytes(&bytes)
        .with_context(|| format!("failed to parse events.log on '{ref_name}' for prune"))?;
    let before = all.len();
    let remaining: Vec<_> = all
        .into_iter()
        .filter(|e| crate::events::OrderingKey::from_envelope(e) > *watermark)
        .collect();
    let pruned = before - remaining.len();
    if pruned == 0 {
        return Ok(0);
    }

    let mut out = Vec::new();
    for ev in &remaining {
        let line = serde_json::to_string(ev).context("failed to serialize pruned event")?;
        out.extend_from_slice(line.as_bytes());
        out.push(b'\n');
    }

    commit_log_bytes(
        repo_dir,
        agent_id,
        &out,
        &format!("crosslink v3 prune: dropped {pruned} covered events"),
    )?;
    Ok(pruned)
}

static MIGRATED_V2_WARNED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub fn warn_if_migrated_v2_operation(repo_dir: &Path, mode: HubMode) {
    use std::sync::atomic::Ordering;

    if mode.is_v3() {
        return;
    }
    if MIGRATED_V2_WARNED.load(Ordering::Relaxed) {
        return;
    }
    if let Ok(HubVersion::V3 { .. }) = detect_hub_version(repo_dir) {
        if !MIGRATED_V2_WARNED.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                "hub has been migrated to v3; v2 writes are not reflected in v3 — \
                 finish the cutover or avoid mixed operation"
            );
        }
    }
}

#[must_use]
pub fn genesis_sentinel_watermark() -> crate::events::OrderingKey {
    crate::events::OrderingKey {
        timestamp: chrono::DateTime::<chrono::Utc>::UNIX_EPOCH,
        agent_id: "hub-v3-genesis".to_string(),
        agent_seq: 0,
    }
}

pub fn bootstrap_v3_hub(
    repo_dir: &Path,
    agent_id: &str,
    remote: Option<&str>,
) -> Result<BootstrapOutcome> {
    validate_agent_id(agent_id)?;

    let meta = HubMeta {
        hub_version: 3,
        migrated_from_commit: "genesis".to_string(),
        migrated_at: chrono::Utc::now(),
        finalized_at: None,
        genesis_checkpoint_commit: None,
        seed_agent_tips: None,
    };
    let hub_json = serde_json::to_vec_pretty(&meta).context("failed to serialize HubMeta")?;
    let signers_path = repo_dir.join("trust").join("allowed_signers");
    let signers_bytes = if signers_path.exists() {
        Some(
            std::fs::read(&signers_path)
                .with_context(|| format!("failed to read {}", signers_path.display()))?,
        )
    } else {
        None
    };
    let mut meta_files: Vec<(&str, &[u8])> = vec![("hub.json", &hub_json)];
    if let Some(bytes) = &signers_bytes {
        meta_files.push(("allowed_signers", bytes));
    }
    commit_files_to_ref(
        repo_dir,
        META_REF,
        &meta_files,
        "hub-v3 bootstrap: meta marker",
    )
    .context("failed to write genesis meta marker")?;

    let genesis = crate::checkpoint::CheckpointState {
        watermark: Some(genesis_sentinel_watermark()),
        ..crate::checkpoint::CheckpointState::default()
    };
    let state_bytes =
        serde_json::to_vec_pretty(&genesis).context("failed to serialize genesis checkpoint")?;
    commit_blob_to_ref(
        repo_dir,
        CHECKPOINT_REF,
        "state.json",
        &state_bytes,
        "hub-v3 bootstrap: genesis checkpoint",
    )
    .context("failed to write genesis checkpoint")?;

    commit_log_bytes(repo_dir, agent_id, &[], "hub-v3 bootstrap: agent ref")
        .with_context(|| format!("failed to write genesis agent ref for '{agent_id}'"))?;

    let pushed = remote.map(|remote| push_bootstrap_refs(repo_dir, remote, agent_id));

    Ok(BootstrapOutcome { pushed })
}

#[derive(Debug)]
pub struct BootstrapOutcome {
    pub pushed: Option<Vec<(String, PushOutcome)>>,
}

fn push_bootstrap_refs(
    repo_dir: &Path,
    remote: &str,
    agent_id: &str,
) -> Vec<(String, PushOutcome)> {
    let agent_ref = format!("{AGENT_REF_PREFIX}{agent_id}");
    let mut out = Vec::with_capacity(3);
    for ref_name in [META_REF, CHECKPOINT_REF, agent_ref.as_str()] {
        let outcome = match push_ref(repo_dir, remote, ref_name) {
            Ok(o) => o,
            Err(e) => PushOutcome::Failed(e.to_string()),
        };
        out.push((ref_name.to_string(), outcome));
    }
    out
}

pub fn fetch_v3_refs_for_join(repo_dir: &Path, remote: &str) -> Result<()> {
    let listed = Command::new("git")
        .current_dir(repo_dir)
        .args([
            "ls-remote",
            remote,
            META_REF,
            CHECKPOINT_REF,
            "refs/heads/crosslink/agents/*",
            OLD_META_REF,
            OLD_CHECKPOINT_REF,
            "refs/crosslink/agents/*",
        ])
        .output()
        .with_context(|| format!("failed to discover v3 refs from remote '{remote}'"))?;
    if !listed.status.success() {
        anyhow::bail!(
            "discovering v3 refs from remote '{remote}' failed: {}",
            String::from_utf8_lossy(&listed.stderr).trim()
        );
    }
    let mut refs = Vec::new();
    for line in String::from_utf8_lossy(&listed.stdout).lines() {
        let Some((oid, reference)) = line.split_once('\t') else {
            anyhow::bail!("remote '{remote}' returned a malformed v3 ref advertisement");
        };
        let oid = oid.trim();
        let reference = reference.trim();
        anyhow::ensure!(
            matches!(oid.len(), 40 | 64)
                && oid
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
            "remote '{remote}' advertised an invalid v3 object id"
        );
        anyhow::ensure!(
            reference == META_REF
                || reference == CHECKPOINT_REF
                || reference == OLD_META_REF
                || reference == OLD_CHECKPOINT_REF
                || reference
                    .strip_prefix(AGENT_REF_PREFIX)
                    .is_some_and(|agent| agent_ref_name(agent).is_ok())
                || reference
                    .strip_prefix(OLD_AGENT_REF_PREFIX)
                    .is_some_and(|agent| agent_ref_name(agent).is_ok()),
            "remote '{remote}' advertised an invalid v3 ref '{reference}'"
        );
        refs.push((reference.to_string(), oid.to_string()));
    }
    refs.sort();
    refs.dedup();
    if refs.is_empty() {
        return Ok(());
    }
    let mut args = vec!["fetch".to_string(), remote.to_string()];
    args.extend(refs.iter().map(|(reference, _)| {
        let digest = hex::encode(Sha256::digest(reference.as_bytes()));
        format!("+{reference}:refs/crosslink/reconciliation/observed/{digest}")
    }));
    let output = Command::new("git")
        .current_dir(repo_dir)
        .args(&args)
        .output()
        .with_context(|| format!("failed to fetch v3 refs from remote '{remote}'"))?;
    if !output.status.success() {
        anyhow::bail!(
            "fetching v3 refs from remote '{remote}' failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    for (reference, oid) in refs {
        let current = Command::new("git")
            .current_dir(repo_dir)
            .args(["rev-parse", "--verify", &reference])
            .output()
            .with_context(|| format!("checking local v3 ref '{reference}'"))?;
        if current.status.success() {
            continue;
        }
        let absent = "0".repeat(oid.len());
        let created = Command::new("git")
            .current_dir(repo_dir)
            .args(["update-ref", &reference, &oid, &absent])
            .output()
            .with_context(|| format!("creating discovered v3 ref '{reference}'"))?;
        if !created.status.success() {
            let raced = Command::new("git")
                .current_dir(repo_dir)
                .args(["rev-parse", "--verify", &reference])
                .output()
                .with_context(|| format!("rechecking raced v3 ref '{reference}'"))?;
            anyhow::ensure!(
                raced.status.success(),
                "creating discovered v3 ref '{reference}' failed: {}",
                String::from_utf8_lossy(&created.stderr).trim()
            );
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn append_event_to_ref_with_abort(
    repo_dir: &Path,
    agent_id: &str,
    envelope: &EventEnvelope,
    abort: Option<AbortPoint>,
) -> Result<RefAppendOutcome> {
    append_inner(repo_dir, agent_id, envelope, abort)
}

pub(crate) fn git_rev_parse_optional(repo_dir: &Path, ref_name: &str) -> Result<Option<String>> {
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

pub(crate) fn git_cat_file_blob_optional(
    repo_dir: &Path,
    blob_spec: &str,
) -> Result<Option<Vec<u8>>> {
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
        || stderr.contains("could not get object info")
    {
        return Ok(None);
    }

    anyhow::bail!("git cat-file failed for '{}': {}", blob_spec, stderr.trim())
}

fn git_hash_object(repo_dir: &Path, data: &[u8]) -> Result<String> {
    use std::io::Write as _;

    let mut child = Command::new("git")
        .current_dir(repo_dir)
        .args(["hash-object", "-w", "--stdin"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("failed to spawn git hash-object")?;

    child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("git hash-object stdin pipe unavailable"))?
        .write_all(data)
        .context("failed to write to git hash-object stdin")?;

    let output = child
        .wait_with_output()
        .context("failed to wait for git hash-object")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git hash-object failed: {}", stderr.trim());
    }

    let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if sha.is_empty() {
        anyhow::bail!("git hash-object returned empty SHA");
    }
    Ok(sha)
}

fn git_commit_tree(
    repo_dir: &Path,
    tree_sha: &str,
    parent_sha: Option<&str>,
    message: &str,
    agent_id: &str,
) -> Result<String> {
    let author_name = agent_id;
    let author_email = format!("{agent_id}@crosslink");

    let mut args: Vec<&str> = vec!["-c", "commit.gpgsign=false", "commit-tree", tree_sha];
    let parent_arg;
    if let Some(p) = parent_sha {
        parent_arg = p.to_string();
        args.push("-p");
        args.push(&parent_arg);
    }
    args.push("-m");
    args.push(message);

    let mut child = Command::new("git")
        .current_dir(repo_dir)
        .args(&args)
        .env("GIT_AUTHOR_NAME", author_name)
        .env("GIT_AUTHOR_EMAIL", &author_email)
        .env("GIT_COMMITTER_NAME", author_name)
        .env("GIT_COMMITTER_EMAIL", &author_email)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("failed to spawn git commit-tree")?;

    let _ = child.stdin.take();

    let output = child
        .wait_with_output()
        .context("failed to wait for git commit-tree")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git commit-tree failed: {}", stderr.trim());
    }

    let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if sha.is_empty() {
        anyhow::bail!("git commit-tree returned empty SHA");
    }
    Ok(sha)
}

fn git_update_ref_cas(
    repo_dir: &Path,
    ref_name: &str,
    new_sha: &str,
    old_sha: Option<&str>,
) -> Result<()> {
    let old_value = old_sha.unwrap_or("");
    let output = Command::new("git")
        .current_dir(repo_dir)
        .args(["update-ref", ref_name, new_sha, old_value])
        .output()
        .with_context(|| format!("failed to run git update-ref for '{ref_name}'"))?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);

    anyhow::bail!(
        "ref moved concurrently: {ref_name} (git update-ref failed: {})",
        stderr.trim()
    )
}

fn commit_single_file_tree(
    repo_dir: &Path,
    ref_name: &str,
    file_name: &str,
    bytes: &[u8],
    message: &str,
    committer_id: &str,
    expected: CasExpectation<'_>,
) -> Result<String> {
    commit_upserts_to_ref(
        repo_dir,
        ref_name,
        &[(file_name, BlobRef::Bytes(bytes))],
        &[],
        message,
        committer_id,
        expected,
    )
}

fn commit_upserts_to_ref(
    repo_dir: &Path,
    ref_name: &str,
    upserts: &[(&str, BlobRef<'_>)],
    deletes: &[&str],
    message: &str,
    committer_id: &str,
    expected: CasExpectation<'_>,
) -> Result<String> {
    let old_commit = resolve_cas_old(repo_dir, ref_name, expected)?;
    let tree_sha = write_tree_with(repo_dir, old_commit.as_deref(), upserts, deletes)?;
    let commit_sha = git_commit_tree(
        repo_dir,
        &tree_sha,
        old_commit.as_deref(),
        message,
        committer_id,
    )?;
    git_update_ref_cas(repo_dir, ref_name, &commit_sha, old_commit.as_deref())?;
    Ok(commit_sha)
}

fn resolve_cas_old(
    repo_dir: &Path,
    ref_name: &str,
    expected: CasExpectation<'_>,
) -> Result<Option<String>> {
    match expected {
        CasExpectation::MustNotExist => {
            if git_rev_parse_optional(repo_dir, ref_name)?.is_some() {
                anyhow::bail!(
                    "ref moved concurrently: {ref_name} (expected absent, but it exists)"
                );
            }
            Ok(None)
        }
        CasExpectation::MustMatch(sha) => Ok(Some(sha.to_string())),
        CasExpectation::CurrentTip => git_rev_parse_optional(repo_dir, ref_name),
    }
}

#[allow(dead_code)]
pub fn commit_blob_to_ref(
    repo_dir: &Path,
    ref_name: &str,
    file_name: &str,
    bytes: &[u8],
    message: &str,
) -> Result<String> {
    commit_single_file_tree(
        repo_dir,
        ref_name,
        file_name,
        bytes,
        message,
        "crosslink",
        CasExpectation::CurrentTip,
    )
}

#[allow(dead_code)]
pub fn commit_files_to_ref(
    repo_dir: &Path,
    ref_name: &str,
    files: &[(&str, &[u8])],
    message: &str,
) -> Result<String> {
    anyhow::ensure!(
        !files.is_empty(),
        "commit_files_to_ref requires at least one file"
    );

    for (name, _) in files {
        anyhow::ensure!(
            !name.contains('/') && !name.is_empty(),
            "commit_files_to_ref file name must be a non-empty tree-root name, got '{name}'"
        );
    }
    let mut sorted: Vec<&str> = files.iter().map(|(n, _)| *n).collect();
    sorted.sort_unstable();
    for pair in sorted.windows(2) {
        anyhow::ensure!(
            pair[0] != pair[1],
            "commit_files_to_ref got duplicate file name '{}'",
            pair[0]
        );
    }

    let upserts: Vec<(&str, BlobRef<'_>)> = files
        .iter()
        .map(|(name, bytes)| (*name, BlobRef::Bytes(bytes)))
        .collect();
    commit_upserts_to_ref(
        repo_dir,
        ref_name,
        &upserts,
        &[],
        message,
        "crosslink",
        CasExpectation::CurrentTip,
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TreeEntry {
    mode: String,

    object_type: String,

    sha: String,

    name: String,
}

impl TreeEntry {
    fn mktree_line(&self) -> String {
        format!(
            "{} {} {}\t{}",
            self.mode, self.object_type, self.sha, self.name
        )
    }
}

enum BlobRef<'a> {
    Existing(&'a str),

    Bytes(&'a [u8]),
}

impl BlobRef<'_> {
    fn resolve(&self, repo_dir: &Path) -> Result<String> {
        match self {
            BlobRef::Existing(sha) => Ok((*sha).to_string()),
            BlobRef::Bytes(bytes) => git_hash_object(repo_dir, bytes),
        }
    }
}

fn read_tree_entries(repo_dir: &Path, commit_sha: &str) -> Result<Vec<TreeEntry>> {
    let spec = format!("{commit_sha}^{{tree}}");
    let output = Command::new("git")
        .current_dir(repo_dir)
        .args(["ls-tree", &spec])
        .output()
        .with_context(|| format!("failed to run git ls-tree for '{spec}'"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git ls-tree failed for '{}': {}", spec, stderr.trim());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut entries = Vec::new();
    for line in stdout.lines() {
        if line.trim().is_empty() {
            continue;
        }

        let (meta, name) = line
            .split_once('\t')
            .ok_or_else(|| anyhow::anyhow!("malformed git ls-tree line (no TAB): {line}"))?;
        let mut parts = meta.split_whitespace();
        let mode = parts
            .next()
            .ok_or_else(|| anyhow::anyhow!("malformed git ls-tree line (no mode): {line}"))?;
        let object_type = parts
            .next()
            .ok_or_else(|| anyhow::anyhow!("malformed git ls-tree line (no type): {line}"))?;
        let sha = parts
            .next()
            .ok_or_else(|| anyhow::anyhow!("malformed git ls-tree line (no sha): {line}"))?;
        entries.push(TreeEntry {
            mode: mode.to_string(),
            object_type: object_type.to_string(),
            sha: sha.to_string(),
            name: name.to_string(),
        });
    }
    Ok(entries)
}

fn write_tree_with(
    repo_dir: &Path,
    base: Option<&str>,
    upserts: &[(&str, BlobRef<'_>)],
    deletes: &[&str],
) -> Result<String> {
    use std::collections::BTreeMap;

    let mut root: BTreeMap<String, TreeEntry> = BTreeMap::new();
    if let Some(commit_sha) = base {
        for entry in read_tree_entries(repo_dir, commit_sha)? {
            root.insert(entry.name.clone(), entry);
        }
    }

    let mut subtree_ops: BTreeMap<String, BTreeMap<String, Option<String>>> = BTreeMap::new();

    for (path, blob) in upserts {
        match split_one_level(path)? {
            (None, file) => {
                let blob_sha = blob.resolve(repo_dir)?;
                root.insert(
                    file.to_string(),
                    TreeEntry {
                        mode: "100644".to_string(),
                        object_type: "blob".to_string(),
                        sha: blob_sha,
                        name: file.to_string(),
                    },
                );
            }
            (Some(dir), leaf) => {
                let blob_sha = blob.resolve(repo_dir)?;
                subtree_ops
                    .entry(dir.to_string())
                    .or_default()
                    .insert(leaf.to_string(), Some(blob_sha));
            }
        }
    }

    for path in deletes {
        match split_one_level(path)? {
            (None, file) => {
                root.remove(file);
            }
            (Some(dir), leaf) => {
                subtree_ops
                    .entry(dir.to_string())
                    .or_default()
                    .insert(leaf.to_string(), None);
            }
        }
    }

    for (dir, ops) in subtree_ops {
        let mut leaves: BTreeMap<String, TreeEntry> = BTreeMap::new();
        if let Some(existing) = root.get(&dir) {
            anyhow::ensure!(
                existing.object_type == "tree",
                "write_tree_with: '{dir}' exists as a {} but a subtree path targets it",
                existing.object_type
            );
            for entry in read_subtree_entries(repo_dir, &existing.sha)? {
                leaves.insert(entry.name.clone(), entry);
            }
        }

        for (leaf, maybe_sha) in ops {
            match maybe_sha {
                Some(sha) => {
                    leaves.insert(
                        leaf.clone(),
                        TreeEntry {
                            mode: "100644".to_string(),
                            object_type: "blob".to_string(),
                            sha,
                            name: leaf.clone(),
                        },
                    );
                }
                None => {
                    leaves.remove(&leaf);
                }
            }
        }

        if leaves.is_empty() {
            root.remove(&dir);
        } else {
            let subtree_sha = mktree_from_entries(repo_dir, leaves.values())?;
            root.insert(
                dir.clone(),
                TreeEntry {
                    mode: "040000".to_string(),
                    object_type: "tree".to_string(),
                    sha: subtree_sha,
                    name: dir,
                },
            );
        }
    }

    mktree_from_entries(repo_dir, root.values())
}

fn read_subtree_entries(repo_dir: &Path, tree_sha: &str) -> Result<Vec<TreeEntry>> {
    read_tree_entries_raw(repo_dir, tree_sha)
}

fn read_tree_entries_raw(repo_dir: &Path, sha: &str) -> Result<Vec<TreeEntry>> {
    let output = Command::new("git")
        .current_dir(repo_dir)
        .args(["ls-tree", sha])
        .output()
        .with_context(|| format!("failed to run git ls-tree for '{sha}'"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git ls-tree failed for '{}': {}", sha, stderr.trim());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut entries = Vec::new();
    for line in stdout.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let (meta, name) = line
            .split_once('\t')
            .ok_or_else(|| anyhow::anyhow!("malformed git ls-tree line (no TAB): {line}"))?;
        let mut parts = meta.split_whitespace();
        let mode = parts
            .next()
            .ok_or_else(|| anyhow::anyhow!("malformed git ls-tree line (no mode): {line}"))?;
        let object_type = parts
            .next()
            .ok_or_else(|| anyhow::anyhow!("malformed git ls-tree line (no type): {line}"))?;
        let sha_field = parts
            .next()
            .ok_or_else(|| anyhow::anyhow!("malformed git ls-tree line (no sha): {line}"))?;
        entries.push(TreeEntry {
            mode: mode.to_string(),
            object_type: object_type.to_string(),
            sha: sha_field.to_string(),
            name: name.to_string(),
        });
    }
    Ok(entries)
}

fn mktree_from_entries<'a, I>(repo_dir: &Path, entries: I) -> Result<String>
where
    I: Iterator<Item = &'a TreeEntry>,
{
    use std::io::Write as _;

    let mut input = String::new();
    let mut any = false;
    for entry in entries {
        any = true;
        input.push_str(&entry.mktree_line());
        input.push('\n');
    }

    if !any {
        input.clear();
    }

    let mut child = Command::new("git")
        .current_dir(repo_dir)
        .args(["mktree"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("failed to spawn git mktree")?;

    child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("git mktree stdin pipe unavailable"))?
        .write_all(input.as_bytes())
        .context("failed to write to git mktree stdin")?;

    let output = child
        .wait_with_output()
        .context("failed to wait for git mktree")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git mktree failed: {}", stderr.trim());
    }

    let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if sha.is_empty() {
        anyhow::bail!("git mktree returned empty SHA");
    }
    Ok(sha)
}

fn split_one_level(path: &str) -> Result<(Option<&str>, &str)> {
    anyhow::ensure!(!path.is_empty(), "write_tree_with: empty path");
    let mut parts = path.split('/');
    let first = parts.next().unwrap_or("");
    match parts.next() {
        None => {
            anyhow::ensure!(!first.is_empty(), "write_tree_with: empty path component");
            Ok((None, first))
        }
        Some(leaf) => {
            anyhow::ensure!(
                parts.next().is_none(),
                "write_tree_with: path '{path}' nests deeper than one level (only \
                 root-level files and one-level subtree paths are supported)"
            );
            anyhow::ensure!(
                !first.is_empty() && !leaf.is_empty(),
                "write_tree_with: empty path component in '{path}'"
            );
            Ok((Some(first), leaf))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{append_event, Event, EventEnvelope};
    use chrono::Utc;
    use uuid::Uuid;

    fn git_init(path: &Path) {
        run_git(path, &["init"]);
        run_git(path, &["config", "user.email", "test@crosslink.test"]);
        run_git(path, &["config", "user.name", "Test"]);
    }

    #[test]
    fn append_immune_to_dangling_signing_config() {
        let dir = tempfile::tempdir().unwrap();
        git_init(dir.path());
        run_git(dir.path(), &["config", "gpg.format", "ssh"]);
        run_git(
            dir.path(),
            &[
                "config",
                "user.signingkey",
                "/nonexistent/deleted-worktree/keys/dead_ed25519",
            ],
        );
        run_git(dir.path(), &["config", "commit.gpgsign", "true"]);

        let agent_id = "test-agent";
        let outcome = append_event_to_ref(dir.path(), agent_id, &make_envelope(agent_id, 1))
            .expect("dangling signing config must not break plumbing ref writes");

        let raw = std::process::Command::new("git")
            .current_dir(dir.path())
            .args(["cat-file", "commit", &outcome.new_commit])
            .output()
            .unwrap();
        let commit_text = String::from_utf8_lossy(&raw.stdout).to_string();
        assert!(
            !commit_text.contains("gpgsig"),
            "ref commits must be unsigned regardless of git config, got:\n{commit_text}"
        );
    }

    fn run_git(repo_dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .current_dir(repo_dir)
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

    fn run_git_output(repo_dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .current_dir(repo_dir)
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

    fn make_envelope(agent_id: &str, seq: u64) -> EventEnvelope {
        EventEnvelope {
            agent_id: agent_id.to_string(),
            agent_seq: seq,
            timestamp: Utc::now(),
            event: Event::IssueCreated {
                uuid: Uuid::new_v4(),
                title: format!("Issue seq {seq}"),
                description: None,
                priority: "medium".to_string(),
                labels: vec![],
                parent_uuid: None,
                created_by: agent_id.to_string(),
                display_id: None,
                scheduled_at: None,
                due_at: None,
            },
            signed_by: None,
            signature: None,
        }
    }

    #[test]
    fn genesis_append_creates_ref_with_single_event() {
        let dir = tempfile::tempdir().unwrap();
        git_init(dir.path());

        let agent_id = "test-agent";
        let envelope = make_envelope(agent_id, 1);

        let outcome = append_event_to_ref(dir.path(), agent_id, &envelope).unwrap();

        assert!(
            outcome.old_commit.is_none(),
            "genesis write must have no parent"
        );
        assert_eq!(outcome.events_in_log, 1);
        assert!(!outcome.new_commit.is_empty());

        let ref_name = agent_ref_name(agent_id).unwrap();
        let sha = run_git_output(dir.path(), &["rev-parse", &ref_name]);
        assert_eq!(sha, outcome.new_commit);

        let parent_count_output = std::process::Command::new("git")
            .current_dir(dir.path())
            .args(["log", "--oneline", &ref_name])
            .output()
            .unwrap();
        let log = String::from_utf8_lossy(&parent_count_output.stdout);
        assert_eq!(log.lines().count(), 1, "genesis commit must have no parent");

        let blob_spec = format!("{}:events.log", outcome.new_commit);
        let blob = run_git_output(dir.path(), &["cat-file", "blob", &blob_spec]);
        let expected_line = serde_json::to_string(&envelope).unwrap();
        assert_eq!(blob, expected_line.trim());
    }

    #[test]
    fn sequential_appends_chain_commits_and_preserve_order() {
        let dir = tempfile::tempdir().unwrap();
        git_init(dir.path());

        let agent_id = "chain-agent";
        let e1 = make_envelope(agent_id, 1);
        let e2 = make_envelope(agent_id, 2);
        let e3 = make_envelope(agent_id, 3);

        let r1 = append_event_to_ref(dir.path(), agent_id, &e1).unwrap();
        let r2 = append_event_to_ref(dir.path(), agent_id, &e2).unwrap();
        let r3 = append_event_to_ref(dir.path(), agent_id, &e3).unwrap();

        assert_eq!(r1.events_in_log, 1);
        assert_eq!(r2.events_in_log, 2);
        assert_eq!(r3.events_in_log, 3);

        let ref_name = agent_ref_name(agent_id).unwrap();
        let rev_list = run_git_output(dir.path(), &["rev-list", "--count", &ref_name]);
        assert_eq!(rev_list.trim(), "3", "must have exactly 3 commits in chain");

        let blob_spec = format!("{}:events.log", r3.new_commit);
        let blob_bytes = std::process::Command::new("git")
            .current_dir(dir.path())
            .args(["cat-file", "blob", &blob_spec])
            .output()
            .unwrap()
            .stdout;

        let parsed = read_events_from_bytes(&blob_bytes).unwrap();
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].agent_seq, 1);
        assert_eq!(parsed[1].agent_seq, 2);
        assert_eq!(parsed[2].agent_seq, 3);

        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("events.log");
        append_event(&log_path, &e1).unwrap();
        append_event(&log_path, &e2).unwrap();
        append_event(&log_path, &e3).unwrap();
        let file_bytes = std::fs::read(&log_path).unwrap();
        assert_eq!(
            blob_bytes, file_bytes,
            "hub_v3 log bytes must be byte-identical to events::append_event output"
        );
    }

    #[test]
    fn stale_cas_loses_loudly_and_winning_state_survives() {
        let dir = tempfile::tempdir().unwrap();
        git_init(dir.path());

        let agent_id = "cas-agent";

        let r1 = append_event_to_ref(dir.path(), agent_id, &make_envelope(agent_id, 1)).unwrap();
        let tip_after_first = r1.new_commit;

        append_event_to_ref(dir.path(), agent_id, &make_envelope(agent_id, 2)).unwrap();

        let ref_name = agent_ref_name(agent_id).unwrap();

        let current_tip = run_git_output(dir.path(), &["rev-parse", &ref_name]);

        let stale_result = git_update_ref_cas(
            dir.path(),
            &ref_name,
            &tip_after_first,
            Some(&tip_after_first),
        );

        assert!(stale_result.is_err(), "stale CAS must fail with an error");
        let err_msg = format!("{:?}", stale_result.unwrap_err());
        assert!(
            err_msg.contains("ref moved concurrently"),
            "error must mention concurrent move, got: {err_msg}"
        );

        let tip_now = run_git_output(dir.path(), &["rev-parse", &ref_name]);
        assert_eq!(
            tip_now, current_tip,
            "winning state must survive the stale CAS attempt"
        );
    }

    fn run_crash_injection_test(abort: AbortPoint, label: &str) {
        let dir = tempfile::tempdir().unwrap();
        git_init(dir.path());

        let agent_id = "crash-agent";
        let ref_name = agent_ref_name(agent_id).unwrap();
        let envelope = make_envelope(agent_id, 1);

        let result = append_event_to_ref_with_abort(dir.path(), agent_id, &envelope, Some(abort));
        assert!(result.is_ok(), "{label}: abort should return Ok");
        let outcome = result.unwrap();
        assert!(
            outcome.new_commit.is_empty(),
            "{label}: aborted outcome must have empty new_commit"
        );

        let ref_exists = std::process::Command::new("git")
            .current_dir(dir.path())
            .args(["rev-parse", "--verify", "--quiet", &ref_name])
            .output()
            .unwrap();
        assert!(
            !ref_exists.status.success(),
            "{label}: ref must not exist after aborted genesis write"
        );

        let fsck = std::process::Command::new("git")
            .current_dir(dir.path())
            .args(["fsck", "--strict"])
            .output()
            .unwrap();
        assert_eq!(
            fsck.status.code(),
            Some(0),
            "{label}: git fsck --strict must exit 0; stderr: {}",
            String::from_utf8_lossy(&fsck.stderr)
        );

        let normal = append_event_to_ref(dir.path(), agent_id, &envelope).unwrap();
        assert_eq!(
            normal.events_in_log, 1,
            "{label}: recovery must have 1 event"
        );
        assert!(
            normal.old_commit.is_none(),
            "{label}: recovery must be a genesis commit"
        );
    }

    #[test]
    fn crash_after_hash_object_leaves_ref_unmoved() {
        run_crash_injection_test(AbortPoint::HashObject, "HashObject");
    }

    #[test]
    fn crash_after_mktree_leaves_ref_unmoved() {
        run_crash_injection_test(AbortPoint::Mktree, "Mktree");
    }

    #[test]
    fn crash_after_commit_tree_leaves_ref_unmoved() {
        run_crash_injection_test(AbortPoint::CommitTree, "CommitTree");
    }

    #[test]
    fn two_agents_no_contention_on_separate_refs() {
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        git_init(dir.path());
        let repo_dir = Arc::new(dir.path().to_path_buf());

        const EVENTS_PER_AGENT: u64 = 25;

        let repo_a = Arc::clone(&repo_dir);
        let handle_a = std::thread::spawn(move || {
            let agent_id = "concurrent-agent-a";
            for seq in 1..=EVENTS_PER_AGENT {
                append_event_to_ref(&repo_a, agent_id, &make_envelope(agent_id, seq))
                    .unwrap_or_else(|e| panic!("agent-a seq {seq} failed: {e}"));
            }
        });

        let repo_b = Arc::clone(&repo_dir);
        let handle_b = std::thread::spawn(move || {
            let agent_id = "concurrent-agent-b";
            for seq in 1..=EVENTS_PER_AGENT {
                append_event_to_ref(&repo_b, agent_id, &make_envelope(agent_id, seq))
                    .unwrap_or_else(|e| panic!("agent-b seq {seq} failed: {e}"));
            }
        });

        handle_a.join().expect("agent-a thread panicked");
        handle_b.join().expect("agent-b thread panicked");

        for agent_id in &["concurrent-agent-a", "concurrent-agent-b"] {
            let ref_name = agent_ref_name(agent_id).unwrap();
            let rev_count = run_git_output(&repo_dir, &["rev-list", "--count", &ref_name]);
            assert_eq!(
                rev_count.trim(),
                "25",
                "agent {agent_id} must have 25 commits"
            );

            let tip = run_git_output(&repo_dir, &["rev-parse", &ref_name]);
            let blob_spec = format!("{tip}:events.log");
            let blob_bytes = std::process::Command::new("git")
                .current_dir(repo_dir.as_ref())
                .args(["cat-file", "blob", &blob_spec])
                .output()
                .unwrap()
                .stdout;
            let events = read_events_from_bytes(&blob_bytes).unwrap();
            assert_eq!(events.len(), 25, "agent {agent_id} log must have 25 events");
            for (i, ev) in events.iter().enumerate() {
                assert_eq!(
                    ev.agent_seq,
                    (i + 1) as u64,
                    "agent {agent_id} event at position {i} must have seq {}",
                    i + 1
                );
            }
        }
    }

    #[test]
    fn push_outcomes_genesis_fast_forward_nff_noremote() {
        let local_dir = tempfile::tempdir().unwrap();
        let remote_dir = tempfile::tempdir().unwrap();

        git_init(local_dir.path());

        run_git(remote_dir.path(), &["init", "--bare"]);

        run_git(
            local_dir.path(),
            &[
                "remote",
                "add",
                "origin",
                remote_dir.path().to_str().unwrap(),
            ],
        );

        let agent_id = "push-agent";

        append_event_to_ref(local_dir.path(), agent_id, &make_envelope(agent_id, 1)).unwrap();
        let push1 = push_agent_ref(local_dir.path(), "origin", agent_id).unwrap();
        assert!(
            matches!(push1, PushOutcome::Pushed),
            "genesis push must be Pushed"
        );

        let ref_name = agent_ref_name(agent_id).unwrap();
        run_git_output(remote_dir.path(), &["rev-parse", &ref_name]);

        append_event_to_ref(local_dir.path(), agent_id, &make_envelope(agent_id, 2)).unwrap();
        let push2 = push_agent_ref(local_dir.path(), "origin", agent_id).unwrap();
        assert!(
            matches!(push2, PushOutcome::Pushed),
            "second push must be Pushed (fast-forward)"
        );

        append_event_to_ref(local_dir.path(), agent_id, &make_envelope(agent_id, 3)).unwrap();
        let local_tip = run_git_output(local_dir.path(), &["rev-parse", &ref_name]);

        let divergent_bytes = b"divergent log content\n";

        let other_agent = "divergent-helper";
        append_event_to_ref(
            local_dir.path(),
            other_agent,
            &make_envelope(other_agent, 1),
        )
        .unwrap();
        let other_ref = agent_ref_name(other_agent).unwrap();

        push_agent_ref(local_dir.path(), "origin", other_agent).unwrap();
        let remote_other_tip = run_git_output(remote_dir.path(), &["rev-parse", &other_ref]);

        run_git(
            remote_dir.path(),
            &["update-ref", &ref_name, &remote_other_tip],
        );

        let push3 = push_agent_ref(local_dir.path(), "origin", agent_id).unwrap();
        assert!(
            matches!(push3, PushOutcome::NonFastForward),
            "divergent remote must yield NonFastForward"
        );

        let remote_tip_after = run_git_output(remote_dir.path(), &["rev-parse", &ref_name]);
        assert_eq!(
            remote_tip_after, remote_other_tip,
            "remote must be unchanged after rejected push"
        );
        assert_ne!(
            remote_tip_after, local_tip,
            "local tip must not have been force-pushed"
        );

        let push4 = push_agent_ref(local_dir.path(), "nonexistent-remote", agent_id).unwrap();
        assert!(
            matches!(push4, PushOutcome::NoRemote | PushOutcome::Failed(_)),
            "unknown remote must be NoRemote or Failed"
        );

        let _ = divergent_bytes;
    }

    #[test]
    fn agent_ref_name_validation() {
        assert!(agent_ref_name("abc").is_ok());
        assert!(agent_ref_name("my-agent-42").is_ok());
        assert!(agent_ref_name("agent_xyz").is_ok());
        assert!(agent_ref_name(&"a".repeat(64)).is_ok());

        assert!(agent_ref_name("ab").is_err());
        assert!(agent_ref_name("").is_err());

        assert!(agent_ref_name(&"a".repeat(65)).is_err());
        assert!(agent_ref_name(&"a".repeat(200)).is_err());

        assert!(agent_ref_name("../x").is_err(), "../x must be rejected");
        assert!(agent_ref_name("a/b").is_err(), "slash must be rejected");
        assert!(agent_ref_name("a b").is_err(), "space must be rejected");
        assert!(agent_ref_name("a@b").is_err(), "@ must be rejected");
        assert!(
            agent_ref_name("a.b").is_err(),
            "dot (path-traversal) must be rejected"
        );

        assert!(agent_ref_name("CON").is_err(), "CON must be rejected");
        assert!(agent_ref_name("NUL").is_err(), "NUL must be rejected");
        assert!(agent_ref_name("PRN").is_err(), "PRN must be rejected");
    }

    #[test]
    fn commit_blob_to_ref_genesis_and_cas_conflict() {
        let dir = tempfile::tempdir().unwrap();
        git_init(dir.path());

        let ref_name = CHECKPOINT_REF;

        let sha1 = commit_blob_to_ref(
            dir.path(),
            ref_name,
            "state.json",
            b"{\"v\":1}\n",
            "genesis checkpoint",
        )
        .unwrap();
        let tip = run_git_output(dir.path(), &["rev-parse", ref_name]);
        assert_eq!(tip, sha1, "ref must point at the genesis commit");

        let log = run_git_output(dir.path(), &["log", "--oneline", ref_name]);
        assert_eq!(log.lines().count(), 1, "genesis commit must have no parent");
        let blob = run_git_output(
            dir.path(),
            &["cat-file", "blob", &format!("{sha1}:state.json")],
        );
        assert_eq!(blob, "{\"v\":1}");

        let sha2 = commit_blob_to_ref(
            dir.path(),
            ref_name,
            "state.json",
            b"{\"v\":2}\n",
            "second checkpoint",
        )
        .unwrap();
        assert_ne!(sha1, sha2);
        let count = run_git_output(dir.path(), &["rev-list", "--count", ref_name]);
        assert_eq!(count, "2", "second commit must chain onto the first");

        let stale = commit_single_file_tree(
            dir.path(),
            ref_name,
            "state.json",
            b"{\"v\":3}\n",
            "stale checkpoint",
            "crosslink",
            CasExpectation::MustMatch(&sha1),
        );
        assert!(stale.is_err(), "stale MustMatch CAS must fail");
        let msg = format!("{:?}", stale.unwrap_err());
        assert!(
            msg.contains("ref moved concurrently"),
            "CAS conflict error must mention concurrent move, got: {msg}"
        );

        let exists = commit_single_file_tree(
            dir.path(),
            ref_name,
            "state.json",
            b"{}\n",
            "genesis on existing",
            "crosslink",
            CasExpectation::MustNotExist,
        );
        assert!(exists.is_err(), "MustNotExist on an existing ref must fail");
    }

    #[test]
    fn commit_files_to_ref_multi_file_ordering() {
        let dir = tempfile::tempdir().unwrap();
        git_init(dir.path());

        let files: &[(&str, &[u8])] = &[
            ("hub.json", b"{\"hub_version\":3}\n"),
            ("allowed_signers", b"signer line\n"),
        ];
        let sha = commit_files_to_ref(dir.path(), META_REF, files, "meta genesis").unwrap();

        let hub = run_git_output(
            dir.path(),
            &["cat-file", "blob", &format!("{sha}:hub.json")],
        );
        assert_eq!(hub, "{\"hub_version\":3}");
        let signers = run_git_output(
            dir.path(),
            &["cat-file", "blob", &format!("{sha}:allowed_signers")],
        );
        assert_eq!(signers, "signer line");

        let listing = run_git_output(dir.path(), &["ls-tree", "--name-only", sha.as_str()]);
        let names: Vec<&str> = listing.lines().collect();
        assert_eq!(names, vec!["allowed_signers", "hub.json"]);

        let dup: &[(&str, &[u8])] = &[("a.json", b"1"), ("a.json", b"2")];
        assert!(
            commit_files_to_ref(dir.path(), META_REF, dup, "dup").is_err(),
            "duplicate file names must be rejected"
        );
    }

    #[test]
    fn push_ref_with_lease_success_and_rejection() {
        let local_dir = tempfile::tempdir().unwrap();
        let remote_dir = tempfile::tempdir().unwrap();
        git_init(local_dir.path());
        run_git(remote_dir.path(), &["init", "--bare"]);
        run_git(
            local_dir.path(),
            &[
                "remote",
                "add",
                "origin",
                remote_dir.path().to_str().unwrap(),
            ],
        );

        let ref_name = CHECKPOINT_REF;

        commit_blob_to_ref(
            local_dir.path(),
            ref_name,
            "state.json",
            b"{\"v\":1}\n",
            "cp1",
        )
        .unwrap();
        let p1 = push_ref_with_lease(local_dir.path(), "origin", ref_name, None).unwrap();
        assert!(
            matches!(p1, PushOutcome::Pushed),
            "genesis lease push must succeed"
        );
        let remote_tip1 = run_git_output(remote_dir.path(), &["rev-parse", ref_name]);

        commit_blob_to_ref(
            local_dir.path(),
            ref_name,
            "state.json",
            b"{\"v\":2}\n",
            "cp2",
        )
        .unwrap();
        let p2 =
            push_ref_with_lease(local_dir.path(), "origin", ref_name, Some(&remote_tip1)).unwrap();
        assert!(
            matches!(p2, PushOutcome::Pushed),
            "matching-lease push must succeed"
        );
        let remote_tip2 = run_git_output(remote_dir.path(), &["rev-parse", ref_name]);

        commit_blob_to_ref(
            remote_dir.path(),
            "refs/crosslink/scratch",
            "state.json",
            b"X\n",
            "scratch",
        )
        .unwrap();
        let divergent = run_git_output(remote_dir.path(), &["rev-parse", "refs/crosslink/scratch"]);
        run_git(remote_dir.path(), &["update-ref", ref_name, &divergent]);

        commit_blob_to_ref(
            local_dir.path(),
            ref_name,
            "state.json",
            b"{\"v\":3}\n",
            "cp3",
        )
        .unwrap();
        let p3 =
            push_ref_with_lease(local_dir.path(), "origin", ref_name, Some(&remote_tip2)).unwrap();
        assert!(
            matches!(p3, PushOutcome::NonFastForward),
            "stale lease must be rejected as NonFastForward, got a different outcome"
        );

        let remote_after = run_git_output(remote_dir.path(), &["rev-parse", ref_name]);
        assert_eq!(
            remote_after, divergent,
            "rejected lease push must not move the remote"
        );
    }

    #[test]
    fn detect_hub_version_matrix() {
        let dir = tempfile::tempdir().unwrap();
        git_init(dir.path());

        assert_eq!(detect_hub_version(dir.path()).unwrap(), HubVersion::Absent);

        let cp = commit_blob_to_ref(dir.path(), "refs/crosslink/tmp", "x", b"x\n", "tmp").unwrap();
        run_git(dir.path(), &["update-ref", "refs/heads/crosslink/hub", &cp]);
        assert_eq!(detect_hub_version(dir.path()).unwrap(), HubVersion::V2Only);

        commit_files_to_ref(dir.path(), META_REF, &[("hub.json", b"{}\n")], "meta").unwrap();
        commit_blob_to_ref(dir.path(), CHECKPOINT_REF, "state.json", b"{}\n", "cp").unwrap();
        assert_eq!(
            detect_hub_version(dir.path()).unwrap(),
            HubVersion::V3 {
                v2_branch_present: true
            }
        );

        run_git(
            dir.path(),
            &["update-ref", "-d", "refs/heads/crosslink/hub"],
        );
        assert_eq!(
            detect_hub_version(dir.path()).unwrap(),
            HubVersion::V3 {
                v2_branch_present: false
            }
        );
    }

    #[test]
    fn detect_remote_hub_version_matrix_and_unreachable() {
        let local_dir = tempfile::tempdir().unwrap();
        let remote_dir = tempfile::tempdir().unwrap();
        git_init(local_dir.path());
        run_git(remote_dir.path(), &["init", "--bare"]);
        run_git(
            local_dir.path(),
            &[
                "remote",
                "add",
                "origin",
                remote_dir.path().to_str().unwrap(),
            ],
        );

        assert_eq!(
            detect_remote_hub_version(local_dir.path(), "origin").unwrap(),
            HubVersion::Absent
        );

        commit_blob_to_ref(remote_dir.path(), "refs/crosslink/tmp", "x", b"x\n", "tmp").unwrap();
        let tmp = run_git_output(remote_dir.path(), &["rev-parse", "refs/crosslink/tmp"]);
        run_git(
            remote_dir.path(),
            &["update-ref", "refs/heads/crosslink/hub", &tmp],
        );
        run_git(
            remote_dir.path(),
            &["update-ref", "-d", "refs/crosslink/tmp"],
        );
        assert_eq!(
            detect_remote_hub_version(local_dir.path(), "origin").unwrap(),
            HubVersion::V2Only
        );

        commit_files_to_ref(
            remote_dir.path(),
            META_REF,
            &[("hub.json", b"{}\n")],
            "meta",
        )
        .unwrap();
        commit_blob_to_ref(
            remote_dir.path(),
            CHECKPOINT_REF,
            "state.json",
            b"{}\n",
            "cp",
        )
        .unwrap();
        assert_eq!(
            detect_remote_hub_version(local_dir.path(), "origin").unwrap(),
            HubVersion::V3 {
                v2_branch_present: true
            }
        );

        run_git(
            remote_dir.path(),
            &["update-ref", "-d", "refs/heads/crosslink/hub"],
        );
        assert_eq!(
            detect_remote_hub_version(local_dir.path(), "origin").unwrap(),
            HubVersion::V3 {
                v2_branch_present: false
            }
        );

        let err = detect_remote_hub_version(local_dir.path(), "/no/such/remote/path").unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            msg.contains("ls-remote")
                || msg.contains("remote unreachable")
                || msg.contains("cannot determine"),
            "unreachable remote must hard-error, got: {msg}"
        );
    }

    #[test]
    fn hub_meta_roundtrip_via_commit_files_and_read() {
        let dir = tempfile::tempdir().unwrap();
        git_init(dir.path());

        assert!(read_hub_meta(dir.path()).unwrap().is_none());

        let meta = HubMeta {
            hub_version: 3,
            migrated_from_commit: "deadbeefcafe1234".to_string(),
            migrated_at: chrono::Utc::now(),
            finalized_at: None,
            genesis_checkpoint_commit: None,
            seed_agent_tips: None,
        };
        let meta_bytes = serde_json::to_vec(&meta).unwrap();
        commit_files_to_ref(
            dir.path(),
            META_REF,
            &[("hub.json", &meta_bytes), ("allowed_signers", b"sig\n")],
            "meta with marker",
        )
        .unwrap();

        let read = read_hub_meta(dir.path())
            .unwrap()
            .expect("meta must be present");
        assert_eq!(read.hub_version, 3);
        assert_eq!(read.migrated_from_commit, "deadbeefcafe1234");

        assert_eq!(read, meta);

        let dir2 = tempfile::tempdir().unwrap();
        git_init(dir2.path());
        commit_files_to_ref(
            dir2.path(),
            META_REF,
            &[("allowed_signers", b"sig\n")],
            "meta no marker",
        )
        .unwrap();
        assert!(read_hub_meta(dir2.path()).unwrap().is_none());
    }

    fn make_heartbeat(agent_id: &str, issue: Option<i64>) -> crate::locks::Heartbeat {
        crate::locks::Heartbeat {
            agent_id: agent_id.to_string(),
            last_heartbeat: Utc::now(),
            active_issue_id: issue,
            machine_id: format!("machine-{agent_id}"),
        }
    }

    fn cat_blob(repo_dir: &Path, spec: &str) -> Option<Vec<u8>> {
        let out = std::process::Command::new("git")
            .current_dir(repo_dir)
            .args(["cat-file", "blob", spec])
            .output()
            .unwrap();
        if out.status.success() {
            Some(out.stdout)
        } else {
            None
        }
    }

    fn make_request(request_id: &str) -> crate::agent_requests::AgentRequest {
        crate::agent_requests::AgentRequest {
            request_id: request_id.to_string(),
            kind: crate::agent_requests::RequestKind::Pause,
            subject: crate::agent_requests::RequestSubject::default(),
            requested_by: "SHA256:driver".to_string(),
            requested_at: Utc::now().to_rfc3339(),
            reason: None,
        }
    }

    fn make_ack(request_id: &str) -> crate::agent_requests::AgentRequestAck {
        crate::agent_requests::AgentRequestAck {
            request_id: request_id.to_string(),
            ack_at: Utc::now().to_rfc3339(),
            acted: true,
            result: "paused".to_string(),
            notes: None,
        }
    }

    #[test]
    fn append_event_preserves_sibling_heartbeat() {
        let dir = tempfile::tempdir().unwrap();
        git_init(dir.path());
        let agent_id = "sibling-agent";

        append_event_to_ref(dir.path(), agent_id, &make_envelope(agent_id, 1)).unwrap();

        write_heartbeat_to_ref(dir.path(), agent_id, &make_heartbeat(agent_id, Some(7))).unwrap();

        let ref_name = agent_ref_name(agent_id).unwrap();
        let tip = run_git_output(dir.path(), &["rev-parse", &ref_name]);
        let hb_before = cat_blob(dir.path(), &format!("{tip}:heartbeat.json")).unwrap();

        append_event_to_ref(dir.path(), agent_id, &make_envelope(agent_id, 2)).unwrap();

        let tip2 = run_git_output(dir.path(), &["rev-parse", &ref_name]);
        let hb_after = cat_blob(dir.path(), &format!("{tip2}:heartbeat.json")).unwrap();
        assert_eq!(hb_before, hb_after, "heartbeat.json must survive an append");

        let log = cat_blob(dir.path(), &format!("{tip2}:events.log")).unwrap();
        assert_eq!(read_events_from_bytes(&log).unwrap().len(), 2);
    }

    #[test]
    fn interleaved_writers_preserve_all_siblings() {
        let dir = tempfile::tempdir().unwrap();
        git_init(dir.path());
        let agent_id = "multi-writer";
        let ref_name = agent_ref_name(agent_id).unwrap();

        append_event_to_ref(dir.path(), agent_id, &make_envelope(agent_id, 1)).unwrap();
        write_heartbeat_to_ref(dir.path(), agent_id, &make_heartbeat(agent_id, None)).unwrap();
        write_ack_to_own_ref(dir.path(), agent_id, "01ACK0001", &make_ack("01ACK0001")).unwrap();

        append_event_to_ref(dir.path(), agent_id, &make_envelope(agent_id, 2)).unwrap();

        write_ack_to_own_ref(dir.path(), agent_id, "01ACK0002", &make_ack("01ACK0002")).unwrap();

        let tip = run_git_output(dir.path(), &["rev-parse", &ref_name]);

        let log = cat_blob(dir.path(), &format!("{tip}:events.log")).unwrap();
        assert_eq!(read_events_from_bytes(&log).unwrap().len(), 2);

        assert!(cat_blob(dir.path(), &format!("{tip}:heartbeat.json")).is_some());

        assert!(cat_blob(dir.path(), &format!("{tip}:requests-ack/01ACK0001.json")).is_some());
        assert!(cat_blob(dir.path(), &format!("{tip}:requests-ack/01ACK0002.json")).is_some());
    }

    #[test]
    fn write_tree_with_rejects_deep_nesting() {
        let dir = tempfile::tempdir().unwrap();
        git_init(dir.path());
        let res = write_tree_with(
            dir.path(),
            None,
            &[("a/b/c.json", BlobRef::Bytes(b"x"))],
            &[],
        );
        assert!(res.is_err(), "deeper-than-one-level path must be rejected");
        let msg = format!("{:?}", res.unwrap_err());
        assert!(
            msg.contains("nests deeper than one level"),
            "error must mention nesting depth, got: {msg}"
        );

        assert!(split_one_level("a/b/c").is_err());
        assert_eq!(split_one_level("file.json").unwrap(), (None, "file.json"));
        assert_eq!(
            split_one_level("dir/leaf.json").unwrap(),
            (Some("dir"), "leaf.json")
        );
    }

    #[test]
    fn write_tree_with_subtree_delete_drops_empty_subtree() {
        let dir = tempfile::tempdir().unwrap();
        git_init(dir.path());
        let agent_id = "subtree-agent";
        let ref_name = agent_ref_name(agent_id).unwrap();

        append_event_to_ref(dir.path(), agent_id, &make_envelope(agent_id, 1)).unwrap();
        write_ack_to_own_ref(dir.path(), agent_id, "01ONLY", &make_ack("01ONLY")).unwrap();
        let tip = run_git_output(dir.path(), &["rev-parse", &ref_name]);
        assert!(cat_blob(dir.path(), &format!("{tip}:requests-ack/01ONLY.json")).is_some());

        let new_tree =
            write_tree_with(dir.path(), Some(&tip), &[], &["requests-ack/01ONLY.json"]).unwrap();
        let listing = run_git_output(dir.path(), &["ls-tree", "--name-only", &new_tree]);
        let names: Vec<&str> = listing.lines().collect();
        assert_eq!(names, vec!["events.log"], "empty subtree must be dropped");
    }

    #[test]
    fn heartbeats_roundtrip_across_three_refs() {
        let dir = tempfile::tempdir().unwrap();
        git_init(dir.path());

        for (id, issue) in [("hb-a", Some(1)), ("hb-b", None)] {
            append_event_to_ref(dir.path(), id, &make_envelope(id, 1)).unwrap();
            write_heartbeat_to_ref(dir.path(), id, &make_heartbeat(id, issue)).unwrap();
        }

        append_event_to_ref(dir.path(), "hb-c", &make_envelope("hb-c", 1)).unwrap();

        let mut beats = read_heartbeats_from_refs(dir.path()).unwrap();
        beats.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(beats.len(), 2, "only agents with a heartbeat are returned");
        assert_eq!(beats[0].0, "hb-a");
        assert_eq!(beats[0].1.active_issue_id, Some(1));
        assert_eq!(beats[1].0, "hb-b");
        assert_eq!(beats[1].1.active_issue_id, None);
    }

    #[test]
    fn heartbeat_overwrites_previous() {
        let dir = tempfile::tempdir().unwrap();
        git_init(dir.path());
        let id = "hb-update";
        write_heartbeat_to_ref(dir.path(), id, &make_heartbeat(id, Some(1))).unwrap();
        write_heartbeat_to_ref(dir.path(), id, &make_heartbeat(id, Some(2))).unwrap();
        let beats = read_heartbeats_from_refs(dir.path()).unwrap();
        assert_eq!(beats.len(), 1);
        assert_eq!(beats[0].1.active_issue_id, Some(2), "latest heartbeat wins");
    }

    #[test]
    fn request_poll_ack_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        git_init(dir.path());
        let driver = "driver-1";
        let target = "target-1";

        let req = make_request("01REQ0001");
        write_request_to_own_ref(dir.path(), driver, target, &req).unwrap();

        let pending = poll_requests_for_agent(dir.path(), target).unwrap();
        assert_eq!(pending.len(), 1, "target must see the pending request");
        assert_eq!(pending[0].0, driver, "driver id recovered");
        assert_eq!(pending[0].1.request_id, "01REQ0001");

        write_ack_to_own_ref(dir.path(), target, "01REQ0001", &make_ack("01REQ0001")).unwrap();

        let after = poll_requests_for_agent(dir.path(), target).unwrap();
        assert!(after.is_empty(), "acked request must not be returned");
    }

    #[test]
    fn read_all_agent_requests_pairs_request_with_ack() {
        let dir = tempfile::tempdir().unwrap();
        git_init(dir.path());
        let driver = "driver-1";
        let target = "target-1";
        write_request_to_own_ref(dir.path(), driver, target, &make_request("01REQ0001")).unwrap();

        let all = read_all_agent_requests(dir.path()).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].0, target, "grouped by target agent");
        assert_eq!(all[0].1.len(), 1);
        assert_eq!(all[0].1[0].request.request_id, "01REQ0001");
        assert!(all[0].1[0].ack.is_none(), "no ack yet");

        write_ack_to_own_ref(dir.path(), target, "01REQ0001", &make_ack("01REQ0001")).unwrap();
        let all = read_all_agent_requests(dir.path()).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(
            all[0].1.len(),
            1,
            "acked request stays visible to dashboard"
        );
        assert_eq!(
            all[0].1[0].ack.as_ref().map(|a| a.request_id.as_str()),
            Some("01REQ0001"),
            "ack is paired with its request"
        );
    }

    #[test]
    fn request_for_different_target_not_returned() {
        let dir = tempfile::tempdir().unwrap();
        git_init(dir.path());
        let driver = "driver-2";
        write_request_to_own_ref(
            dir.path(),
            driver,
            "someone-else",
            &make_request("01REQOTHER"),
        )
        .unwrap();
        let pending = poll_requests_for_agent(dir.path(), "me-myself").unwrap();
        assert!(pending.is_empty(), "request for another target is filtered");
    }

    #[test]
    fn request_separator_handles_hyphenated_agent_ids() {
        let dir = tempfile::tempdir().unwrap();
        git_init(dir.path());
        let driver = "ops-driver";
        let target = "my-hyphenated-agent";
        write_request_to_own_ref(dir.path(), driver, target, &make_request("01HYPH001")).unwrap();

        let pending = poll_requests_for_agent(dir.path(), target).unwrap();
        assert_eq!(pending.len(), 1, "hyphenated target id must parse");
        assert_eq!(pending[0].0, driver);

        let name = format!("{target}--01HYPH001.json");
        let (t, u) = parse_request_out_name(&name).unwrap();
        assert_eq!(t, target);
        assert_eq!(u, "01HYPH001");

        assert!(parse_request_out_name("nodelim.json").is_none());
    }

    #[test]
    fn requests_from_multiple_drivers_to_same_target() {
        let dir = tempfile::tempdir().unwrap();
        git_init(dir.path());
        let target = "busy-target";
        write_request_to_own_ref(dir.path(), "drv-a", target, &make_request("01AAA")).unwrap();
        write_request_to_own_ref(dir.path(), "drv-b", target, &make_request("01BBB")).unwrap();

        let pending = poll_requests_for_agent(dir.path(), target).unwrap();
        assert_eq!(pending.len(), 2, "requests from both drivers visible");

        assert_eq!(pending[0].1.request_id, "01AAA");
        assert_eq!(pending[1].1.request_id, "01BBB");

        write_ack_to_own_ref(dir.path(), target, "01AAA", &make_ack("01AAA")).unwrap();
        let remaining = poll_requests_for_agent(dir.path(), target).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].1.request_id, "01BBB");
    }

    fn test_hub_lock(dir: &Path) -> crate::sync::HubWriteLock {
        let lock_path = dir.join(".hub-write-lock");
        crate::sync::acquire_hub_lock(&lock_path).expect("failed to acquire hub write lock")
    }

    fn seed_v3_hub(dir: &Path, agent_id: &str, count: u64) {
        commit_files_to_ref(
            dir,
            META_REF,
            &[("hub.json", b"{\"hub_version\":3}\n")],
            "meta",
        )
        .unwrap();
        for seq in 1..=count {
            append_event_to_ref(dir, agent_id, &make_envelope(agent_id, seq)).unwrap();
        }
    }

    #[test]
    fn compact_v3_local_only_writes_checkpoint_and_prunes() {
        let dir = tempfile::tempdir().unwrap();
        git_init(dir.path());
        let agent_id = "cv3-agent";
        seed_v3_hub(dir.path(), agent_id, 4);

        let lock = test_hub_lock(dir.path());
        let result = compact_v3(dir.path(), agent_id, &lock, None).unwrap();
        drop(lock);

        assert_eq!(result.events_processed, 4);
        assert!(result.checkpoint_commit.is_some(), "checkpoint committed");
        assert!(!result.checkpoint_pushed, "no remote → no push");

        assert_eq!(result.events_pruned, 4, "all covered events pruned locally");

        let cp_tip = run_git_output(dir.path(), &["rev-parse", CHECKPOINT_REF]);
        let state_bytes = cat_blob(dir.path(), &format!("{cp_tip}:state.json")).unwrap();
        let state = crate::checkpoint::CheckpointState::from_slice(&state_bytes).unwrap();
        assert_eq!(state.issues.len(), 4, "4 issues materialized");

        let agent_tip = run_git_output(
            dir.path(),
            &["rev-parse", &agent_ref_name(agent_id).unwrap()],
        );
        let log = cat_blob(dir.path(), &format!("{agent_tip}:events.log")).unwrap();
        assert!(read_events_from_bytes(&log).unwrap().is_empty());
    }

    #[test]
    fn compact_v3_remote_push_then_prune_and_fresh_reduce_matches() {
        let dir = tempfile::tempdir().unwrap();
        let remote = tempfile::tempdir().unwrap();
        git_init(dir.path());
        run_git(remote.path(), &["init", "--bare"]);
        run_git(
            dir.path(),
            &["remote", "add", "origin", remote.path().to_str().unwrap()],
        );

        let agent_id = "cv3-remote";
        seed_v3_hub(dir.path(), agent_id, 3);

        push_agent_ref(dir.path(), "origin", agent_id).unwrap();

        let pre =
            crate::compaction::reduce(&crate::hub_source::RefHubSource::new(dir.path()).unwrap())
                .unwrap();

        let lock = test_hub_lock(dir.path());
        let result = compact_v3(dir.path(), agent_id, &lock, Some("origin")).unwrap();
        drop(lock);

        assert!(result.checkpoint_pushed, "checkpoint pushed to remote");
        assert_eq!(result.events_pruned, 3, "prune after successful push");

        push_agent_ref(dir.path(), "origin", agent_id).unwrap();

        let fresh = tempfile::tempdir().unwrap();
        run_git(fresh.path(), &["init"]);
        run_git(
            fresh.path(),
            &["remote", "add", "origin", remote.path().to_str().unwrap()],
        );
        run_git(
            fresh.path(),
            &[
                "fetch",
                "origin",
                "+refs/heads/crosslink/*:refs/heads/crosslink/*",
            ],
        );
        let fresh_outcome =
            crate::compaction::reduce(&crate::hub_source::RefHubSource::new(fresh.path()).unwrap())
                .unwrap();

        let pre_state = serde_json::to_value(&pre.state).unwrap();
        let fresh_state = serde_json::to_value(&fresh_outcome.state).unwrap();
        assert_eq!(
            pre_state, fresh_state,
            "fresh clone (checkpoint + pruned ref) must reduce to identical state"
        );
    }

    #[test]
    fn compact_v3_skips_prune_when_push_fails() {
        let dir = tempfile::tempdir().unwrap();
        git_init(dir.path());

        run_git(
            dir.path(),
            &["remote", "add", "origin", "/no/such/bare/remote"],
        );
        let agent_id = "cv3-nopush";
        seed_v3_hub(dir.path(), agent_id, 2);

        let lock = test_hub_lock(dir.path());
        let result = compact_v3(dir.path(), agent_id, &lock, Some("origin")).unwrap();
        drop(lock);

        assert!(
            result.checkpoint_commit.is_some(),
            "checkpoint still committed locally"
        );
        assert!(!result.checkpoint_pushed, "push to dead remote fails");
        assert_eq!(
            result.events_pruned, 0,
            "prune MUST be skipped when the checkpoint push fails"
        );

        let tip = run_git_output(
            dir.path(),
            &["rev-parse", &agent_ref_name(agent_id).unwrap()],
        );
        let log = cat_blob(dir.path(), &format!("{tip}:events.log")).unwrap();
        assert_eq!(read_events_from_bytes(&log).unwrap().len(), 2);
    }

    #[test]
    fn compact_v3_concurrent_cas_loss_is_benign() {
        let dir = tempfile::tempdir().unwrap();
        git_init(dir.path());
        let agent_id = "cv3-cas";
        seed_v3_hub(dir.path(), agent_id, 2);

        let lock = test_hub_lock(dir.path());
        let r1 = compact_v3(dir.path(), agent_id, &lock, None).unwrap();
        drop(lock);
        assert!(r1.checkpoint_commit.is_some());
        assert_eq!(r1.events_pruned, 2);

        let stale = commit_single_file_tree(
            dir.path(),
            CHECKPOINT_REF,
            "state.json",
            b"{}\n",
            "stale",
            "crosslink",
            CasExpectation::MustNotExist,
        );
        assert!(stale.is_err());
        assert!(
            format!("{:?}", stale.unwrap_err()).contains("ref moved concurrently"),
            "the benign branch keys off this exact substring"
        );

        let lock2 = test_hub_lock(dir.path());
        let r2 = compact_v3(dir.path(), agent_id, &lock2, None).unwrap();
        drop(lock2);
        assert_eq!(r2.events_processed, 0);
        assert_eq!(r2.events_pruned, 0);
    }

    #[test]
    fn compact_v3_two_compactors_produce_consistent_checkpoint() {
        use std::sync::Arc;
        let dir = tempfile::tempdir().unwrap();
        git_init(dir.path());
        let agent_id = "cv3-race";
        seed_v3_hub(dir.path(), agent_id, 3);
        let repo = Arc::new(dir.path().to_path_buf());

        let r_a = {
            let lock = test_hub_lock(&repo);
            let r = compact_v3(&repo, agent_id, &lock, None).unwrap();
            drop(lock);
            r
        };

        let r_b = {
            let lock = test_hub_lock(&repo);
            let r = compact_v3(&repo, agent_id, &lock, None).unwrap();
            drop(lock);
            r
        };
        assert!(r_a.checkpoint_commit.is_some());
        assert_eq!(r_a.events_processed, 3);
        assert_eq!(r_b.events_processed, 0, "second compactor sees pruned ref");

        let cp_tip = run_git_output(&repo, &["rev-parse", CHECKPOINT_REF]);
        let state_bytes = cat_blob(&repo, &format!("{cp_tip}:state.json")).unwrap();
        let state = crate::checkpoint::CheckpointState::from_slice(&state_bytes).unwrap();
        assert_eq!(state.issues.len(), 3);
    }

    #[test]
    fn detect_excludes_v2_branch_and_host_branch() {
        let dir = tempfile::tempdir().unwrap();
        git_init(dir.path());

        let cp = commit_blob_to_ref(dir.path(), "refs/heads/scratch", "x", b"x\n", "x").unwrap();

        run_git(dir.path(), &["update-ref", "refs/heads/crosslink/hub", &cp]);
        assert_eq!(
            detect_hub_version(dir.path()).unwrap(),
            HubVersion::V2Only,
            "crosslink/hub alone must be V2Only"
        );

        run_git(
            dir.path(),
            &["update-ref", "refs/heads/crosslink/hub-v3-host", &cp],
        );
        assert_eq!(
            detect_hub_version(dir.path()).unwrap(),
            HubVersion::V2Only,
            "crosslink/hub-v3-host must never count as hub state"
        );
        assert!(
            !is_v3_hub_ref("refs/heads/crosslink/hub-v3-host"),
            "host branch is not a v3 hub ref"
        );
        assert!(
            !is_v3_hub_ref("refs/heads/crosslink/hub"),
            "v2 branch is not a v3 hub ref"
        );
    }

    #[test]
    fn detect_v3_with_v2_branch_present() {
        let dir = tempfile::tempdir().unwrap();
        git_init(dir.path());
        let cp = commit_blob_to_ref(dir.path(), "refs/heads/scratch", "x", b"x\n", "x").unwrap();

        run_git(dir.path(), &["update-ref", "refs/heads/crosslink/hub", &cp]);
        run_git(
            dir.path(),
            &["update-ref", "refs/heads/crosslink/hub-v3-host", &cp],
        );

        commit_files_to_ref(dir.path(), META_REF, &[("hub.json", b"{}\n")], "meta").unwrap();
        commit_blob_to_ref(dir.path(), CHECKPOINT_REF, "state.json", b"{}\n", "cp").unwrap();

        assert_eq!(
            detect_hub_version(dir.path()).unwrap(),
            HubVersion::V3 {
                v2_branch_present: true
            },
            "checkpoint+meta present, v2 still around → V3 with v2_branch_present true"
        );

        run_git(
            dir.path(),
            &["update-ref", "-d", "refs/heads/crosslink/hub"],
        );
        assert_eq!(
            detect_hub_version(dir.path()).unwrap(),
            HubVersion::V3 {
                v2_branch_present: false
            }
        );
    }

    fn make_issue_envelope(agent_id: &str, seq: u64, uuid: Uuid, title: &str) -> EventEnvelope {
        EventEnvelope {
            agent_id: agent_id.to_string(),
            agent_seq: seq,
            timestamp: Utc::now(),
            event: Event::IssueCreated {
                uuid,
                title: title.to_string(),
                description: None,
                priority: "medium".to_string(),
                labels: vec![],
                parent_uuid: None,
                created_by: agent_id.to_string(),
                display_id: None,
                scheduled_at: None,
                due_at: None,
            },
            signed_by: None,
            signature: None,
        }
    }

    fn make_comment_envelope(
        agent_id: &str,
        seq: u64,
        issue_uuid: Uuid,
        content: &str,
    ) -> EventEnvelope {
        EventEnvelope {
            agent_id: agent_id.to_string(),
            agent_seq: seq,
            timestamp: Utc::now(),
            event: Event::CommentAdded {
                issue_uuid,
                comment_uuid: Uuid::new_v4(),
                display_id: None,
                author: agent_id.to_string(),
                content: content.to_string(),
                created_at: Utc::now(),
                kind: "note".to_string(),
                trigger_type: None,
                intervention_context: None,
                driver_key_fingerprint: None,
                signed_by: None,
                signature: None,
            },
            signed_by: None,
            signature: None,
        }
    }

    fn read_browse_tree(
        repo_dir: &Path,
        commit: &str,
    ) -> std::collections::BTreeMap<String, Vec<u8>> {
        let mut out = std::collections::BTreeMap::new();
        let listing = run_git_output(repo_dir, &["ls-tree", "-r", &format!("{commit}^{{tree}}")]);
        for line in listing.lines() {
            let Some((_meta, name)) = line.split_once('\t') else {
                continue;
            };
            let bytes = cat_blob(repo_dir, &format!("{commit}:{name}")).unwrap();
            out.insert(name.to_string(), bytes);
        }
        out
    }

    #[test]
    fn browse_tree_incremental_equals_one_shot() {
        let agent = "browse-agent";
        let u1 = Uuid::new_v4();
        let u2 = Uuid::new_v4();
        let u3 = Uuid::new_v4();

        let events: Vec<EventEnvelope> = vec![
            make_issue_envelope(agent, 1, u1, "one"),
            make_issue_envelope(agent, 2, u2, "two"),
            make_issue_envelope(agent, 3, u3, "three"),
            make_comment_envelope(agent, 4, u2, "hello"),
        ];

        let one = tempfile::tempdir().unwrap();
        git_init(one.path());
        commit_files_to_ref(one.path(), META_REF, &[("hub.json", b"{}\n")], "meta").unwrap();
        for ev in &events {
            append_event_to_ref(one.path(), agent, ev).unwrap();
        }
        {
            let lock = test_hub_lock(one.path());
            compact_v3(one.path(), agent, &lock, None).unwrap();
        }
        let one_tip = run_git_output(one.path(), &["rev-parse", CHECKPOINT_REF]);
        let one_tree = read_browse_tree(one.path(), &one_tip);

        let inc = tempfile::tempdir().unwrap();
        git_init(inc.path());
        commit_files_to_ref(inc.path(), META_REF, &[("hub.json", b"{}\n")], "meta").unwrap();
        for ev in &events {
            append_event_to_ref(inc.path(), agent, ev).unwrap();
            let lock = test_hub_lock(inc.path());
            compact_v3(inc.path(), agent, &lock, None).unwrap();
            drop(lock);
        }
        let inc_tip = run_git_output(inc.path(), &["rev-parse", CHECKPOINT_REF]);
        let inc_tree = read_browse_tree(inc.path(), &inc_tip);

        assert_eq!(
            one_tree, inc_tree,
            "incremental and one-shot browse trees must be byte-identical"
        );

        let issue_file = one_tree
            .get(&format!("issues/{u2}.json"))
            .expect("issue file present");
        let text = String::from_utf8_lossy(issue_file);
        assert!(
            text.contains("\"comments\""),
            "inline comments present: {text}"
        );
        assert!(text.contains("hello"), "comment content inline: {text}");

        assert!(one_tree.contains_key("README.md"));
        assert!(one_tree.contains_key("meta/milestones.json"));
        assert!(one_tree.contains_key(&format!("issues/{u1}.json")));
    }

    #[test]
    fn browse_tree_tombstone_deletes_file() {
        let agent = "tomb-agent";
        let dir = tempfile::tempdir().unwrap();
        git_init(dir.path());
        commit_files_to_ref(dir.path(), META_REF, &[("hub.json", b"{}\n")], "meta").unwrap();
        let u1 = Uuid::new_v4();
        append_event_to_ref(
            dir.path(),
            agent,
            &make_issue_envelope(agent, 1, u1, "doomed"),
        )
        .unwrap();
        {
            let lock = test_hub_lock(dir.path());
            compact_v3(dir.path(), agent, &lock, None).unwrap();
        }
        let tip = run_git_output(dir.path(), &["rev-parse", CHECKPOINT_REF]);
        assert!(
            cat_blob(dir.path(), &format!("{tip}:issues/{u1}.json")).is_some(),
            "issue browse file exists before delete"
        );

        let del = EventEnvelope {
            agent_id: agent.to_string(),
            agent_seq: 2,
            timestamp: Utc::now(),
            event: Event::IssueDeleted { uuid: u1 },
            signed_by: None,
            signature: None,
        };
        append_event_to_ref(dir.path(), agent, &del).unwrap();
        {
            let lock = test_hub_lock(dir.path());
            compact_v3(dir.path(), agent, &lock, None).unwrap();
        }
        let tip2 = run_git_output(dir.path(), &["rev-parse", CHECKPOINT_REF]);
        assert!(
            cat_blob(dir.path(), &format!("{tip2}:issues/{u1}.json")).is_none(),
            "tombstoned issue browse file removed"
        );
    }

    #[test]
    fn browse_readme_deterministic_across_compactors() {
        let agent = "readme-agent";
        let u1 = Uuid::new_v4();

        let ev = make_issue_envelope(agent, 1, u1, "x");
        let mk = |dir: &Path| -> Vec<u8> {
            git_init(dir);
            commit_files_to_ref(dir, META_REF, &[("hub.json", b"{}\n")], "meta").unwrap();
            append_event_to_ref(dir, agent, &ev).unwrap();
            let lock = test_hub_lock(dir);
            compact_v3(dir, agent, &lock, None).unwrap();
            drop(lock);
            let tip = run_git_output(dir, &["rev-parse", CHECKPOINT_REF]);
            cat_blob(dir, &format!("{tip}:README.md")).unwrap()
        };
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        assert_eq!(
            mk(a.path()),
            mk(b.path()),
            "README.md must be byte-identical across compactors"
        );
    }
}
