use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use chrono::Utc;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::checkpoint::{
    CheckpointState, CompactComment, CompactIssue, CompactMilestone, CompactTimeEntry,
};
use crate::compaction;
use crate::events::OrderingKey;
use crate::hub_source::RefHubSource;
use crate::hub_v3::{
    self, agent_ref_name, read_hub_meta, HubMeta, HubVersion, PushOutcome, CHECKPOINT_REF, META_REF,
};
use crate::issue_file::{
    read_all_issue_files, read_all_milestone_files, read_comment_files, read_counters, IssueFile,
};
use crate::sync::SyncManager;

const V2_HUB_BRANCH: &str = "refs/heads/crosslink/hub";

pub fn hub_v3(
    crosslink_dir: &Path,
    finalize: bool,
    yes_delete_v2: bool,
    adopt_stale: bool,
    remigrate_from_v2: bool,
) -> Result<()> {
    if remigrate_from_v2 && finalize {
        bail!(
            "`--remigrate-from-v2` and `--finalize` are mutually exclusive: remigrate first, \
               verify the full issue count, then finalize as a separate step."
        );
    }

    let sync = SyncManager::new(crosslink_dir)?;
    sync.init_cache()?;
    sync.fetch()?;

    let hub_lock = sync.acquire_lock()?;

    let cache_dir = sync.cache_path().to_path_buf();
    let remote = sync.remote().to_string();

    if remigrate_from_v2 {
        remigrate_from_v2_path(crosslink_dir, &cache_dir, &remote, &hub_lock)
    } else if finalize {
        finalize_migration(&cache_dir, &remote, yes_delete_v2, &hub_lock)
    } else {
        migrate_phase_a(crosslink_dir, &cache_dir, &remote, &hub_lock, adopt_stale)
    }
}

struct RenamePair {
    old: String,

    new: String,
}

fn adopt_remote_v3(
    crosslink_dir: &Path,
    cache_dir: &Path,
    remote: &str,
    adopt_stale: bool,
) -> Result<()> {
    match read_remote_genesis_commit(cache_dir, remote)? {
        Some(genesis_commit) => {
            if let Some(div) = compute_v2_divergence(cache_dir, &genesis_commit)? {
                if div.is_stale() {
                    if adopt_stale {
                        println!(
                            "WARNING: adopting a STALE remote v3 hub (--adopt-stale). {}",
                            div.summary_line()
                        );
                    } else {
                        bail!("{}", div.adopt_refusal_message());
                    }
                }
            }
        }
        None => {
            tracing::warn!(
                "adopt: could not read the remote v3 genesis commit; \
                 skipping the v2-divergence guard"
            );
        }
    }

    println!("remote '{remote}' already hosts a v3 hub — adopting it (no migration performed).");

    hub_v3::fetch_v3_refs_for_join(cache_dir, remote)
        .context("fetching the remote's v3 branches for adoption failed")?;

    match hub_v3::detect_hub_version(cache_dir)? {
        HubVersion::V3 { .. } => {}
        other => bail!(
            "adoption fetch completed but local detection still reports {other:?}; \
             the remote's v3 refs may be incomplete — inspect \
             `git ls-remote {remote} 'refs/heads/crosslink/*'`"
        ),
    }

    if let Some(meta) = read_hub_meta(cache_dir)? {
        print_hub_meta(&meta);
    }

    let db = crate::db::Database::open(&crosslink_dir.join("issues.db"))
        .context("opening issues.db for post-adoption hydration")?;
    let source = crate::hub_source::RefHubSource::new(cache_dir)?;
    let outcome = crate::compaction::reduce(&source)?;
    let stats = crate::hydration::hydrate_from_state(&outcome.state, &db)
        .context("post-adoption hydration failed")?;
    println!(
        "adopted v3 hub: {} issue(s), {} comment(s) hydrated. This machine now \
         operates v3; its agent branch is created on first write.",
        stats.issues, stats.comments
    );

    if stats.issues == 0 {
        let v2_issue_count = read_all_issue_files(&cache_dir.join("issues")).map_or(0, |v| v.len());
        if v2_issue_count > 0 {
            println!(
                "WARNING: the adopted v3 hub is EMPTY but the local v2 hub holds \
                 {v2_issue_count} issue(s). The remote's v3 genesis did not come from this \
                 project's v2 data. Your v2 data is intact on the frozen crosslink/hub \
                 branch but has NOT been migrated — this usually means a machine \
                 bootstrapped a fresh hub against a remote that did not advertise the \
                 v2 branch. Consider deleting the remote's empty v3 branches and \
                 re-running the migration from a machine with the populated v2 hub."
            );
        }
    }
    print_mixed_version_warning();
    Ok(())
}

fn old_to_new_ref(old: &str) -> Option<String> {
    if old == hub_v3::OLD_CHECKPOINT_REF {
        Some(hub_v3::CHECKPOINT_REF.to_string())
    } else if old == hub_v3::OLD_META_REF {
        Some(hub_v3::META_REF.to_string())
    } else {
        old.strip_prefix(hub_v3::OLD_AGENT_REF_PREFIX)
            .map(|agent_id| format!("{}{agent_id}", hub_v3::AGENT_REF_PREFIX))
    }
}

pub fn hub_branches(crosslink_dir: &Path) -> Result<()> {
    let sync = SyncManager::new(crosslink_dir)?;
    sync.init_cache()?;

    let hub_lock = sync.acquire_lock()?;
    let cache_dir = sync.cache_path().to_path_buf();
    let remote = sync.remote().to_string();
    let has_remote = sync.remote_exists();

    let mut old_refs: BTreeSet<String> = BTreeSet::new();
    for r in for_each_ref(&cache_dir, "refs/crosslink/*")? {
        if old_to_new_ref(&r).is_some() {
            old_refs.insert(r);
        }
    }
    if has_remote {
        match ls_remote_old_namespace(&cache_dir, &remote) {
            Ok(remote_refs) => {
                for r in remote_refs.keys() {
                    if old_to_new_ref(r).is_some() {
                        old_refs.insert(r.clone());
                    }
                }
            }
            Err(e) => {
                tracing::warn!("hub-branches: could not list remote old refs (continuing): {e}");
            }
        }
    }

    if old_refs.is_empty() {
        println!(
            "no old-namespace hub refs found — the hub is already on visible \
             branches (or is fresh). Nothing to rename."
        );

        maybe_compact_after_rename(crosslink_dir, &cache_dir, &remote, has_remote, &hub_lock);
        return Ok(());
    }

    let pairs: Vec<RenamePair> = old_refs
        .iter()
        .filter_map(|old| {
            old_to_new_ref(old).map(|new| RenamePair {
                old: old.clone(),
                new,
            })
        })
        .collect();

    println!("Renaming {} hub ref(s) to visible branches:", pairs.len());
    let remote_old = if has_remote {
        ls_remote_old_namespace(&cache_dir, &remote).unwrap_or_default()
    } else {
        BTreeMap::new()
    };

    for pair in &pairs {
        rename_one_ref(&cache_dir, &remote, has_remote, pair, &remote_old)?;
    }

    maybe_compact_after_rename(crosslink_dir, &cache_dir, &remote, has_remote, &hub_lock);

    print_hub_branches_summary(&cache_dir, &remote, has_remote);
    Ok(())
}

fn rename_one_ref(
    cache_dir: &Path,
    remote: &str,
    has_remote: bool,
    pair: &RenamePair,
    remote_old: &BTreeMap<String, String>,
) -> Result<()> {
    let local_sha = git_rev_parse(cache_dir, &pair.old)?;
    let remote_sha = remote_old.get(&pair.old).cloned();

    let sha = local_sha.clone().or_else(|| remote_sha.clone());
    let Some(sha) = sha else {
        return Ok(());
    };

    if local_sha.is_some() {
        let existing_new = git_rev_parse(cache_dir, &pair.new)?;
        if existing_new.as_deref() != Some(sha.as_str()) {
            git_update_ref(cache_dir, &pair.new, &sha)?;
        }
        println!("  local  {} -> {}", pair.old, pair.new);
    }

    if has_remote {
        let create_spec = format!("{sha}:{}", pair.new);
        match run_git(cache_dir, &["push", remote, &create_spec]) {
            Ok(_) => println!("  remote {} -> {} (created)", pair.old, pair.new),
            Err(e) => {
                tracing::warn!("hub-branches: remote create of {} failed: {e}", pair.new);
                println!(
                    "  remote {} -> {}: SKIPPED (push failed: {e})",
                    pair.old, pair.new
                );
            }
        }

        if let Some(old_remote_sha) = &remote_sha {
            let delete_spec = format!(":{}", pair.old);

            let lease = format!("--force-with-lease={}:{old_remote_sha}", pair.old);
            let args = ["push", &lease, remote, &delete_spec];
            match run_git(cache_dir, &args) {
                Ok(_) => println!("  remote deleted {}", pair.old),
                Err(e) => {
                    tracing::warn!("hub-branches: remote delete of {} failed: {e}", pair.old);
                    println!("  remote delete of {}: SKIPPED ({e})", pair.old);
                }
            }
        }
    }

    if local_sha.is_some() && git_rev_parse(cache_dir, &pair.new)?.is_some() {
        git_delete_ref(cache_dir, &pair.old)?;
    }

    Ok(())
}

fn maybe_compact_after_rename(
    crosslink_dir: &Path,
    cache_dir: &Path,
    remote: &str,
    has_remote: bool,
    hub_lock: &crate::sync::HubWriteLock,
) {
    if !matches!(
        hub_v3::detect_hub_version(cache_dir),
        Ok(HubVersion::V3 { .. })
    ) {
        return;
    }
    let agent_id = crate::identity::AgentConfig::load(crosslink_dir)
        .ok()
        .flatten()
        .map_or_else(|| "hub-v3-bootstrap".to_string(), |a| a.agent_id);
    let remote_opt = has_remote.then_some(remote);
    match hub_v3::compact_v3(cache_dir, &agent_id, hub_lock, remote_opt) {
        Ok(_) => println!("Materialized the browsable state tree on crosslink/checkpoint."),
        Err(e) => tracing::warn!(
            "hub-branches: post-rename compaction failed (non-fatal; run `crosslink compact`): {e}"
        ),
    }
}

fn ls_remote_old_namespace(repo_dir: &Path, remote: &str) -> Result<BTreeMap<String, String>> {
    let output = Command::new("git")
        .current_dir(repo_dir)
        .args(["ls-remote", remote, "refs/crosslink/*"])
        .output()
        .with_context(|| format!("failed to run git ls-remote for '{remote}'"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git ls-remote failed for '{remote}': {}", stderr.trim());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut map = BTreeMap::new();
    for line in stdout.lines() {
        if let Some((sha, name)) = line.split_once('\t') {
            map.insert(name.trim().to_string(), sha.trim().to_string());
        }
    }
    Ok(map)
}

fn print_hub_branches_summary(cache_dir: &Path, remote: &str, has_remote: bool) {
    println!("\nDone. The hub now lives on visible branches under crosslink/* .");
    if has_remote {
        if let Some(url) = github_branches_url(cache_dir, remote) {
            println!("Browse it on GitHub: {url}");
        } else {
            println!("Browse the crosslink/checkpoint branch on your git host's web UI.");
        }
    }
}

fn github_branches_url(cache_dir: &Path, remote: &str) -> Option<String> {
    let output = Command::new("git")
        .current_dir(cache_dir)
        .args(["remote", "get-url", remote])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let url = String::from_utf8_lossy(&output.stdout).trim().to_string();

    let slug = url
        .strip_prefix("git@github.com:")
        .or_else(|| url.strip_prefix("https://github.com/"))?
        .to_string();
    let slug = slug.strip_suffix(".git").unwrap_or(&slug);
    Some(format!("https://github.com/{slug}/branches"))
}

fn migrate_phase_a(
    crosslink_dir: &Path,
    cache_dir: &Path,
    remote: &str,
    hub_lock: &crate::sync::HubWriteLock,
    adopt_stale: bool,
) -> Result<()> {
    if !cache_dir.exists() {
        bail!(
            "no hub cache at {} — nothing to migrate (run `crosslink sync` first, \
             or this repo has no shared hub)",
            cache_dir.display()
        );
    }

    match hub_v3::detect_hub_version(cache_dir)? {
        HubVersion::V3 { .. } => {
            println!("hub already migrated to v3 — no migration performed.");
            if let Some(meta) = read_hub_meta(cache_dir)? {
                print_hub_meta(&meta);
            }
            retry_push_missing_refs(cache_dir, remote)?;
            print_mixed_version_warning();
            return Ok(());
        }
        HubVersion::Absent => {
            bail!(
                "no v2 hub detected at {} (neither a crosslink/hub branch nor v3 marker refs) — \
                 nothing to migrate",
                cache_dir.display()
            );
        }
        HubVersion::V2Only => match hub_v3::detect_remote_hub_version(cache_dir, remote) {
            Ok(HubVersion::V3 { .. }) => {
                return adopt_remote_v3(crosslink_dir, cache_dir, remote, adopt_stale);
            }
            Ok(_) => {}
            Err(e) => {
                bail!(
                    "cannot determine the remote hub version ({e}). Refusing to migrate \
                         blind: if another machine already migrated, a second migration here \
                         would mint a conflicting genesis from stale local state. Retry when \
                         the remote '{remote}' is reachable."
                );
            }
        },
    }

    build_and_publish_v3(crosslink_dir, cache_dir, remote, hub_lock, false)
}

fn build_and_publish_v3(
    crosslink_dir: &Path,
    cache_dir: &Path,
    remote: &str,
    hub_lock: &crate::sync::HubWriteLock,
    force_push: bool,
) -> Result<()> {
    let agent_id = crate::identity::AgentConfig::load(crosslink_dir)?
        .map_or_else(|| "hub-v3-migrate".to_string(), |a| a.agent_id);

    let pending = find_pending_offline(cache_dir)?;
    let promotable: Vec<&IssueFile> = pending
        .iter()
        .filter(|i| i.created_by == agent_id)
        .collect();
    if !promotable.is_empty() {
        let names: Vec<String> = promotable
            .iter()
            .map(|i| format!("  {} (\"{}\")", i.uuid, i.title))
            .collect();
        bail!(
            "refusing to migrate: {} offline issue(s) created by this agent have no display_id \
             yet (pending promotion). Run `crosslink sync` to promote and publish them first, \
             then re-run the migration.\n{}",
            promotable.len(),
            names.join("\n")
        );
    }

    if let Some(result) = compaction::compact(cache_dir, &agent_id, true, hub_lock)
        .context("forced pre-migration compaction failed")?
    {
        println!(
            "  pre-migration compaction: {} event(s) reduced, {} issue(s) / {} lock(s) materialized.",
            result.events_processed, result.issues_materialized, result.locks_materialized
        );
        if result.skew_warnings > 0 || result.git_skew_violations > 0 {
            tracing::warn!(
                "pre-migration compaction saw {} skew warning(s) and {} git-skew violation(s)",
                result.skew_warnings,
                result.git_skew_violations
            );
        }
        if result.unsigned_warnings > 0 {
            tracing::warn!(
                "pre-migration compaction saw {} unsigned event(s)",
                result.unsigned_warnings
            );
        }
    }

    let genesis = build_genesis_from_files(cache_dir)?;

    for orphan in &pending {
        if let Some(id) = genesis.display_id_map.get(&orphan.uuid) {
            println!(
                "  minted genesis display id #{id} for orphaned offline issue {} (\"{}\", created_by {})",
                orphan.uuid, orphan.title, orphan.created_by
            );
        }
    }

    let v2_tip = git_rev_parse(cache_dir, V2_HUB_BRANCH)?
        .ok_or_else(|| anyhow::anyhow!("crosslink/hub branch vanished mid-migration"))?;

    let pre_tips = snapshot_crosslink_refs(cache_dir)?;

    let seed_result = seed_v3_refs(cache_dir, &genesis, &v2_tip);
    let seeded = match seed_result {
        Ok(s) => s,
        Err(e) => {
            rollback_refs(cache_dir, &pre_tips)?;
            return Err(e.context("migration seeding failed; rolled back all v3 hub branches"));
        }
    };

    let report = match verify_against_files(cache_dir, &genesis) {
        Ok(r) => r,
        Err(e) => {
            rollback_refs(cache_dir, &pre_tips)?;
            return Err(e.context(
                "AC-6 verification gate failed; rolled back all v3 hub branches \
                 (the crosslink/hub branch was never touched)",
            ));
        }
    };

    let push_summary = push_v3_refs(cache_dir, remote, &seeded, force_push);

    print_phase_a_summary(&seeded, &genesis, &report, &push_summary);
    print_mixed_version_warning();
    Ok(())
}

fn remigrate_from_v2_path(
    crosslink_dir: &Path,
    cache_dir: &Path,
    remote: &str,
    hub_lock: &crate::sync::HubWriteLock,
) -> Result<()> {
    if !cache_dir.exists() {
        bail!(
            "no hub cache at {} — nothing to re-migrate (run `crosslink sync` first)",
            cache_dir.display()
        );
    }

    if git_rev_parse(cache_dir, V2_HUB_BRANCH)?.is_none() {
        bail!(
            "refusing to re-migrate: no crosslink/hub (v2) branch exists at {}. \
             `--remigrate-from-v2` rebuilds v3 from the live v2 branch; there is none to \
             rebuild from (the hub may already be finalized).",
            cache_dir.display()
        );
    }

    match hub_v3::detect_hub_version(cache_dir)? {
        HubVersion::V3 { .. } => {
            println!("re-migrating v3 from the current crosslink/hub (v2) tip.");
            if let Some(meta) = read_hub_meta(cache_dir)? {
                println!("  superseding the existing v3 hub:");
                print_hub_meta(&meta);
            }
        }
        HubVersion::V2Only => {
            println!("re-migrating v3 from the current crosslink/hub (v2) tip.");
        }
        HubVersion::Absent => {
            bail!(
                "refusing to re-migrate: no hub detected at {} (neither a crosslink/hub branch \
                 nor v3 marker refs)",
                cache_dir.display()
            );
        }
    }

    build_and_publish_v3(crosslink_dir, cache_dir, remote, hub_lock, true)
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
        next_milestone_id,
        skew_warnings: Vec::new(),
        compaction_lease: None,
        unsigned_event_warnings: Vec::new(),
        watermark: Some(watermark),
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

struct SeededRefs {
    agents: Vec<String>,

    checkpoint_written: bool,

    meta_written: bool,
}

fn seed_v3_refs(cache_dir: &Path, genesis: &CheckpointState, v2_tip: &str) -> Result<SeededRefs> {
    let agents_dir = cache_dir.join("agents");
    let mut agents = Vec::new();

    let mut seed_agent_tips = std::collections::BTreeMap::new();
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
            let log_bytes = std::fs::read(&log_path)
                .with_context(|| format!("failed to read {}", log_path.display()))?;
            if log_bytes.is_empty() {
                continue;
            }
            let seed_sha = hub_v3::commit_log_bytes(
                cache_dir,
                &agent_id,
                &log_bytes,
                &format!("hub-v3 genesis: seed agent {agent_id} from v2 events.log"),
            )
            .with_context(|| format!("failed to seed agent ref for '{agent_id}'"))?;
            seed_agent_tips.insert(agent_id.clone(), seed_sha);
            agents.push(agent_id);
        }
    }
    agents.sort();

    let state_bytes = serde_json::to_vec_pretty(genesis)
        .context("failed to serialize genesis checkpoint state")?;
    let genesis_checkpoint_commit = hub_v3::commit_blob_to_ref(
        cache_dir,
        CHECKPOINT_REF,
        "state.json",
        &state_bytes,
        "hub-v3 genesis: checkpoint state",
    )
    .context("failed to commit genesis checkpoint")?;

    let meta = HubMeta {
        hub_version: 3,
        migrated_from_commit: v2_tip.to_string(),
        migrated_at: Utc::now(),
        finalized_at: None,
        genesis_checkpoint_commit: Some(genesis_checkpoint_commit),
        seed_agent_tips: Some(seed_agent_tips),
    };
    let hub_json = serde_json::to_vec_pretty(&meta).context("failed to serialize HubMeta")?;

    let signers_path = cache_dir.join("trust").join("allowed_signers");
    let signers_bytes = if signers_path.exists() {
        Some(
            std::fs::read(&signers_path)
                .with_context(|| format!("failed to read {}", signers_path.display()))?,
        )
    } else {
        None
    };

    let mut files: Vec<(&str, &[u8])> = vec![("hub.json", &hub_json)];
    if let Some(bytes) = &signers_bytes {
        files.push(("allowed_signers", bytes));
    }
    hub_v3::commit_files_to_ref(cache_dir, META_REF, &files, "hub-v3 genesis: meta marker")
        .context("failed to commit meta marker")?;

    Ok(SeededRefs {
        agents,
        checkpoint_written: true,
        meta_written: true,
    })
}

struct VerifyReport {
    issues: usize,
    comments: usize,
    milestones: usize,
    locks: usize,
}

fn verify_against_files(cache_dir: &Path, genesis: &CheckpointState) -> Result<VerifyReport> {
    let rebuilt = build_genesis_from_files(cache_dir)?;

    let source = RefHubSource::new(cache_dir)?;
    let outcome = compaction::reduce(&source)?;
    let reduced = &outcome.state;

    let genesis_val = serde_json::to_value(genesis).context("serialize genesis")?;
    let rebuilt_val = serde_json::to_value(&rebuilt).context("serialize rebuilt genesis")?;
    let reduced_val = serde_json::to_value(reduced).context("serialize reduced state")?;
    if genesis_val != rebuilt_val {
        bail!("verification failed: independent file re-read disagrees with genesis state");
    }
    if reduced_val != genesis_val {
        bail!(
            "verification failed: reduce(RefHubSource) does not equal the genesis state \
             (events were applied above/around the watermark, or a ref is wrong)"
        );
    }

    let issues_dir = cache_dir.join("issues");
    let issue_files = read_all_issue_files(&issues_dir)?;

    if issue_files.len() != reduced.issues.len() {
        bail!(
            "verification failed: issue count mismatch (files {}, reduced {})",
            issue_files.len(),
            reduced.issues.len()
        );
    }

    let mut comment_total = 0usize;
    let mut display_ids_seen: BTreeSet<i64> = BTreeSet::new();
    for issue in &issue_files {
        let Some(reduced_issue) = reduced.issues.get(&issue.uuid) else {
            bail!(
                "verification failed: issue {} present on disk but missing from reduced state",
                issue.uuid
            );
        };

        let g = genesis
            .issues
            .get(&issue.uuid)
            .ok_or_else(|| anyhow::anyhow!("genesis missing issue {}", issue.uuid))?;
        if serde_json::to_value(g)? != serde_json::to_value(reduced_issue)? {
            bail!(
                "verification failed: issue {} differs between genesis and reduced state",
                issue.uuid
            );
        }

        if let Some(did) = issue.display_id {
            if !display_ids_seen.insert(did) {
                bail!("verification failed: duplicate display_id #{did} across issue files");
            }
        }

        let layout = issue_layout(&issues_dir, issue);
        let on_disk_comments = count_disk_comments(issue, &layout)?;
        if on_disk_comments != reduced_issue.comments.len() {
            bail!(
                "verification failed: comment count mismatch for issue {} (disk {on_disk_comments}, \
                 reduced {})",
                issue.uuid,
                reduced_issue.comments.len()
            );
        }
        comment_total += on_disk_comments;
    }

    let milestones_dir = cache_dir.join("meta").join("milestones");
    let milestone_files = read_all_milestone_files(&milestones_dir)?;
    if milestone_files.len() != reduced.milestones.len() {
        bail!(
            "verification failed: milestone count mismatch (files {}, reduced {})",
            milestone_files.len(),
            reduced.milestones.len()
        );
    }

    let counters = read_counters(&cache_dir.join("meta").join("counters.json"))?;
    if reduced.next_display_id < counters.next_display_id
        || reduced.next_milestone_id < counters.next_milestone_id
        || reduced.next_comment_id < counters.next_comment_id
    {
        bail!("verification failed: next_* counters regressed below counters.json");
    }

    let compacted = crate::checkpoint::read_checkpoint(cache_dir)?;
    if serde_json::to_value(&compacted.locks)? != serde_json::to_value(&reduced.locks)? {
        bail!("verification failed: lock state differs from the compacted checkpoint");
    }

    Ok(VerifyReport {
        issues: reduced.issues.len(),
        comments: comment_total,
        milestones: reduced.milestones.len(),
        locks: reduced.locks.len(),
    })
}

fn count_disk_comments(issue: &IssueFile, layout: &IssueLayout) -> Result<usize> {
    let mut keys: BTreeSet<Uuid> = BTreeSet::new();
    if let Some(dir) = &layout.comments_dir {
        for cf in read_comment_files(dir)? {
            keys.insert(cf.uuid);
        }
    }
    for ce in &issue.comments {
        keys.insert(derive_uuid("comment", issue.uuid, ce.id));
    }
    Ok(keys.len())
}

struct RefTip {
    name: String,

    old_sha: Option<String>,
}

fn snapshot_crosslink_refs(cache_dir: &Path) -> Result<Vec<RefTip>> {
    let mut names: BTreeSet<String> = BTreeSet::new();
    for r in for_each_ref(cache_dir, "refs/heads/crosslink/*")? {
        if hub_v3::is_v3_hub_ref(&r) {
            names.insert(r);
        }
    }

    names.insert(CHECKPOINT_REF.to_string());
    names.insert(META_REF.to_string());

    let agents_dir = cache_dir.join("agents");
    if agents_dir.exists() {
        for entry in std::fs::read_dir(&agents_dir)?.flatten() {
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                if let Some(id) = entry.file_name().to_str() {
                    if let Ok(name) = agent_ref_name(id) {
                        names.insert(name);
                    }
                }
            }
        }
    }

    let mut tips = Vec::with_capacity(names.len());
    for name in names {
        let old_sha = git_rev_parse(cache_dir, &name)?;
        tips.push(RefTip { name, old_sha });
    }
    Ok(tips)
}

fn rollback_refs(cache_dir: &Path, tips: &[RefTip]) -> Result<()> {
    for tip in tips {
        let current = git_rev_parse(cache_dir, &tip.name)?;
        match (&tip.old_sha, &current) {
            (Some(old), _) => {
                git_update_ref(cache_dir, &tip.name, old)?;
            }
            (None, Some(_)) => {
                git_delete_ref(cache_dir, &tip.name)?;
            }
            (None, None) => {}
        }
    }
    Ok(())
}

struct PushSummary {
    pushed: Vec<String>,
    failed: Vec<(String, String)>,
}

fn push_v3_refs(cache_dir: &Path, remote: &str, seeded: &SeededRefs, force: bool) -> PushSummary {
    let mut summary = PushSummary {
        pushed: Vec::new(),
        failed: Vec::new(),
    };

    let mut record = |name: String, outcome: Result<PushOutcome>| match outcome {
        Ok(PushOutcome::Pushed) => summary.pushed.push(name),
        Ok(PushOutcome::NonFastForward) => summary
            .failed
            .push((name, "non-fast-forward (remote diverged)".to_string())),
        Ok(PushOutcome::NoRemote) => {
            summary
                .failed
                .push((name, "remote not available".to_string()));
        }
        Ok(PushOutcome::Failed(e)) => summary.failed.push((name, e)),
        Err(e) => summary.failed.push((name, e.to_string())),
    };

    for agent_id in &seeded.agents {
        if let Ok(name) = agent_ref_name(agent_id) {
            let outcome = if force {
                hub_v3::push_ref_force(cache_dir, remote, &name)
            } else {
                hub_v3::push_ref(cache_dir, remote, &name)
            };
            record(name, outcome);
        }
    }
    if seeded.meta_written {
        let outcome = if force {
            hub_v3::push_ref_force(cache_dir, remote, META_REF)
        } else {
            hub_v3::push_ref(cache_dir, remote, META_REF)
        };
        record(META_REF.to_string(), outcome);
    }
    if seeded.checkpoint_written {
        let outcome = if force {
            hub_v3::push_ref_force(cache_dir, remote, CHECKPOINT_REF)
        } else {
            hub_v3::push_ref_with_lease(cache_dir, remote, CHECKPOINT_REF, None)
        };
        record(CHECKPOINT_REF.to_string(), outcome);
    }

    summary
}

fn retry_push_missing_refs(cache_dir: &Path, remote: &str) -> Result<()> {
    let local_refs: Vec<String> = for_each_ref(cache_dir, "refs/heads/crosslink/*")?
        .into_iter()
        .filter(|r| hub_v3::is_v3_hub_ref(r))
        .collect();
    if local_refs.is_empty() {
        return Ok(());
    }
    let remote_shas = ls_remote_crosslink(cache_dir, remote).unwrap_or_default();

    let mut retried = 0usize;
    for name in &local_refs {
        let local_sha = git_rev_parse(cache_dir, name)?;
        let remote_sha = remote_shas.get(name);
        if local_sha.as_deref() != remote_sha.map(String::as_str) {
            let outcome = if name == CHECKPOINT_REF {
                hub_v3::push_ref_with_lease(cache_dir, remote, name, None)
            } else {
                hub_v3::push_ref(cache_dir, remote, name)
            };
            match outcome {
                Ok(PushOutcome::Pushed) => {
                    println!("  re-pushed {name}");
                    retried += 1;
                }
                Ok(PushOutcome::NoRemote) => return Ok(()),
                Ok(other) => {
                    tracing::warn!("re-run push of {name} did not complete: {other:?}");
                }
                Err(e) => tracing::warn!("re-run push of {name} failed: {e}"),
            }
        }
    }
    if retried > 0 {
        println!("re-pushed {retried} ref(s) the remote was missing.");
    }
    Ok(())
}

fn git_is_ancestor(repo_dir: &Path, ancestor_sha: &str, descendant_rev: &str) -> Result<bool> {
    let out = Command::new("git")
        .current_dir(repo_dir)
        .args(["merge-base", "--is-ancestor", ancestor_sha, descendant_rev])
        .output()
        .with_context(|| {
            format!("failed to run git merge-base --is-ancestor {ancestor_sha} {descendant_rev}")
        })?;
    match out.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => bail!(
            "git merge-base --is-ancestor {ancestor_sha} {descendant_rev} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ),
    }
}

struct FinalizeSeedMetadata {
    genesis_checkpoint_commit: String,
    seed_agent_tips: BTreeMap<String, String>,
}

fn git_rev_list_oldest_first(repo_dir: &Path, rev: &str) -> Result<Vec<String>> {
    let output = Command::new("git")
        .current_dir(repo_dir)
        .args(["rev-list", "--reverse", rev])
        .output()
        .with_context(|| format!("failed to run git rev-list for '{rev}'"))?;
    if !output.status.success() {
        bail!(
            "git rev-list failed for '{rev}': {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect())
}

fn recover_pre_metadata_seed(
    cache_dir: &Path,
    genesis: &CheckpointState,
) -> Result<FinalizeSeedMetadata> {
    let genesis_value = serde_json::to_value(genesis).context("serialize rebuilt genesis")?;
    let mut genesis_checkpoint_commit = None;
    for sha in git_rev_list_oldest_first(cache_dir, CHECKPOINT_REF)? {
        let spec = format!("{sha}:state.json");
        let Some(bytes) = git_cat_file_blob_optional(cache_dir, &spec)? else {
            continue;
        };
        let Ok(candidate) = CheckpointState::from_slice(&bytes) else {
            continue;
        };
        if serde_json::to_value(candidate).context("serialize checkpoint candidate")?
            == genesis_value
        {
            genesis_checkpoint_commit = Some(sha);
            break;
        }
    }
    let genesis_checkpoint_commit = genesis_checkpoint_commit.ok_or_else(|| {
        anyhow::anyhow!(
            "refusing to finalize: no checkpoint ancestor reproduces the frozen v2 state; \
             the pre-metadata seed boundary cannot be recovered safely"
        )
    })?;

    let mut seed_agent_tips = BTreeMap::new();
    let agents_dir = cache_dir.join("agents");
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
            let expected = std::fs::read(&log_path)
                .with_context(|| format!("failed to read {}", log_path.display()))?;
            if expected.is_empty() {
                continue;
            }
            let ref_name = agent_ref_name(&agent_id)?;
            let mut seed_tip = None;
            for sha in git_rev_list_oldest_first(cache_dir, &ref_name)? {
                let spec = format!("{sha}:events.log");
                if git_cat_file_blob_optional(cache_dir, &spec)?.as_deref()
                    == Some(expected.as_slice())
                {
                    seed_tip = Some(sha);
                    break;
                }
            }
            let seed_tip = seed_tip.ok_or_else(|| {
                anyhow::anyhow!(
                    "refusing to finalize: agent '{agent_id}' has no reachable historical \
                     events.log matching the frozen v2 seed"
                )
            })?;
            seed_agent_tips.insert(agent_id, seed_tip);
        }
    }

    Ok(FinalizeSeedMetadata {
        genesis_checkpoint_commit,
        seed_agent_tips,
    })
}

fn verify_finalize(cache_dir: &Path) -> Result<FinalizeSeedMetadata> {
    let meta = read_hub_meta(cache_dir)?
        .ok_or_else(|| anyhow::anyhow!("v3 meta marker missing during finalize"))?;

    let seed = if let (Some(genesis_checkpoint_commit), Some(seed_agent_tips)) =
        (meta.genesis_checkpoint_commit, meta.seed_agent_tips)
    {
        FinalizeSeedMetadata {
            genesis_checkpoint_commit,
            seed_agent_tips,
        }
    } else {
        let genesis = build_genesis_from_files(cache_dir)?;
        recover_pre_metadata_seed(cache_dir, &genesis).context(
            "refusing to finalize: this hub was migrated before seed metadata was recorded",
        )?
    };
    let genesis_ckpt = &seed.genesis_checkpoint_commit;
    let seed_tips = &seed.seed_agent_tips;

    let spec = format!("{genesis_ckpt}:state.json");
    let bytes = git_cat_file_blob_optional(cache_dir, &spec)?.ok_or_else(|| {
        anyhow::anyhow!(
            "refusing to finalize: the seeded genesis commit {genesis_ckpt} has no state.json \
             (missing or corrupt object)"
        )
    })?;
    let genesis = CheckpointState::from_slice(&bytes)
        .context("refusing to finalize: persisted genesis state.json does not parse")?;
    let genesis_val = serde_json::to_value(&genesis).context("serialize persisted genesis")?;

    let rebuilt = build_genesis_from_files(cache_dir)?;
    if serde_json::to_value(&rebuilt).context("serialize rebuilt genesis")? != genesis_val {
        bail!(
            "refusing to finalize: the v2 hub files no longer reproduce the genesis seeded at \
             migration — the v2 branch changed after migration (a pre-v3 binary kept \
             writing?). Run `crosslink migrate hub-v3 --remigrate-from-v2` to adopt the newer \
             v2 state, then finalize immediately."
        );
    }

    if !git_is_ancestor(cache_dir, genesis_ckpt, CHECKPOINT_REF)? {
        bail!(
            "refusing to finalize: the seeded genesis checkpoint {genesis_ckpt} is no longer \
             an ancestor of {CHECKPOINT_REF} (force-reset or tampering). Run `crosslink \
             migrate hub-v3 --remigrate-from-v2`, then finalize immediately."
        );
    }
    for (agent_id, seed_sha) in seed_tips {
        let ref_name = format!("{}{agent_id}", crate::hub_v3::AGENT_REF_PREFIX);
        if !git_is_ancestor(cache_dir, seed_sha, &ref_name)? {
            bail!(
                "refusing to finalize: agent '{agent_id}''s seeded tip {seed_sha} is no \
                 longer an ancestor of {ref_name} (deleted, force-reset, or tampered). Run \
                 `crosslink migrate hub-v3 --remigrate-from-v2`, then finalize immediately."
            );
        }
    }

    let meta_sha = crate::hub_v3::git_rev_parse_optional(cache_dir, META_REF)?;
    let source = RefHubSource::at_tips(
        cache_dir,
        Some(genesis_ckpt.clone()),
        meta_sha,
        seed_tips
            .iter()
            .map(|(a, s)| (a.clone(), s.clone()))
            .collect(),
    )?;
    let outcome = compaction::reduce(&source).context("reduce at seeded tips")?;
    if serde_json::to_value(&outcome.state).context("serialize seeded reduce")? != genesis_val {
        bail!(
            "refusing to finalize: reducing the seeded refs does not reproduce the persisted \
             genesis (seeded history altered — object corruption or ref surgery). Run \
             `crosslink migrate hub-v3 --remigrate-from-v2`, then finalize immediately."
        );
    }

    Ok(seed)
}

fn finalize_migration(
    cache_dir: &Path,
    remote: &str,
    yes_delete_v2: bool,
    _hub_lock: &crate::sync::HubWriteLock,
) -> Result<()> {
    if !yes_delete_v2 {
        bail!(
            "`migrate hub-v3 --finalize` is destructive: it deletes the legacy crosslink/hub \
             branch locally and on the remote. After finalize, already-deployed v2 binaries will \
             FAIL LOUDLY (the branch they read is gone) — that is the intended hard stop. \
             Re-run with `--yes-delete-v2` to confirm."
        );
    }

    match hub_v3::detect_hub_version(cache_dir)? {
        HubVersion::V3 { v2_branch_present } => {
            if !v2_branch_present {
                println!("crosslink/hub branch already deleted — finalize is a no-op.");
                if let Some(meta) = read_hub_meta(cache_dir)? {
                    print_hub_meta(&meta);
                }
                return Ok(());
            }
        }
        HubVersion::V2Only | HubVersion::Absent => {
            bail!("refusing to finalize: hub is not v3 (run `crosslink migrate hub-v3` first)");
        }
    }

    if let Some(genesis_commit) = read_hub_meta(cache_dir)?.map(|m| m.migrated_from_commit) {
        if let Some(div) = compute_v2_divergence(cache_dir, &genesis_commit)? {
            if div.is_stale() {
                bail!("{}", div.finalize_refusal_message());
            }
        }
    }

    let repo_root = git_main_repo_root(cache_dir)?;

    let seed = verify_finalize(cache_dir)?;

    let mut meta = read_hub_meta(cache_dir)?
        .ok_or_else(|| anyhow::anyhow!("v3 meta marker missing during finalize"))?;
    meta.genesis_checkpoint_commit = Some(seed.genesis_checkpoint_commit);
    meta.seed_agent_tips = Some(seed.seed_agent_tips);
    meta.finalized_at = Some(Utc::now());
    let hub_json = serde_json::to_vec_pretty(&meta).context("serialize finalized HubMeta")?;

    let signers = read_meta_allowed_signers(cache_dir)?;
    let mut files: Vec<(&str, &[u8])> = vec![("hub.json", &hub_json)];
    if let Some(bytes) = &signers {
        files.push(("allowed_signers", bytes));
    }
    hub_v3::commit_files_to_ref(
        cache_dir,
        META_REF,
        &files,
        "hub-v3 finalize: stamp finalized_at",
    )
    .context("failed to update meta marker with finalized_at")?;

    delete_v2_branch_local(cache_dir, &repo_root)?;
    push_v2_branch_deletion(&repo_root, remote)?;

    match hub_v3::push_ref(&repo_root, remote, META_REF) {
        Ok(PushOutcome::Pushed | PushOutcome::NoRemote) => {}
        Ok(other) => tracing::warn!("finalize: meta ref push did not complete: {other:?}"),
        Err(e) => tracing::warn!("finalize: meta ref push failed: {e}"),
    }

    println!("Finalized hub-v3 migration:");
    println!("  deleted crosslink/hub branch (local + remote)");
    println!(
        "  HubMeta.finalized_at = {}",
        meta.finalized_at.unwrap_or_else(Utc::now)
    );
    println!();
    println!(
        "Note: any already-deployed v0.5.x / v2 binary will now FAIL LOUDLY when it tries to \
         read the deleted crosslink/hub branch. This is the intended hard cutover."
    );
    Ok(())
}

fn read_meta_allowed_signers(cache_dir: &Path) -> Result<Option<Vec<u8>>> {
    let Some(tip) = git_rev_parse(cache_dir, META_REF)? else {
        return Ok(None);
    };
    let spec = format!("{tip}:allowed_signers");
    git_cat_file_blob_optional(cache_dir, &spec)
}

fn delete_v2_branch_local(cache_dir: &Path, repo_root: &Path) -> Result<()> {
    let mut teardown_errors: Vec<String> = Vec::new();

    for path in worktrees_on_hub_branch(repo_root)? {
        let removed = run_git(
            repo_root,
            &["worktree", "remove", "--force", "--force", &path],
        );
        if let Err(e) = removed {
            teardown_errors.push(format!("worktree remove {path}: {e}"));
            let _ = std::fs::remove_dir_all(&path);
        }
    }
    let _ = run_git(repo_root, &["worktree", "prune"]);

    let remaining = worktrees_on_hub_branch(repo_root)?;
    if !remaining.is_empty() {
        bail!(
            "cannot delete crosslink/hub: worktree(s) still host it after teardown: {}\n\
             teardown errors:\n{}",
            remaining.join(", "),
            teardown_errors.join("\n")
        );
    }

    run_git(repo_root, &["branch", "-D", "crosslink/hub"])
        .context("failed to delete local crosslink/hub branch")?;

    if cache_dir.exists() {
        let still_registered = worktree_paths(repo_root)?
            .iter()
            .any(|p| Path::new(p) == cache_dir);
        if !still_registered {
            std::fs::remove_dir_all(cache_dir).with_context(|| {
                format!(
                    "failed to remove leftover v2 cache dir {}",
                    cache_dir.display()
                )
            })?;
        }
    }
    Ok(())
}

fn worktree_paths(repo_root: &Path) -> Result<Vec<String>> {
    let output = Command::new("git")
        .current_dir(repo_root)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .context("failed to run git worktree list")?;
    if !output.status.success() {
        bail!(
            "git worktree list failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|l| l.strip_prefix("worktree "))
        .map(str::to_string)
        .collect())
}

fn worktrees_on_hub_branch(repo_root: &Path) -> Result<Vec<String>> {
    let output = Command::new("git")
        .current_dir(repo_root)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .context("failed to run git worktree list")?;
    if !output.status.success() {
        bail!(
            "git worktree list failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut result = Vec::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            current = Some(p.to_string());
        } else if line == "branch refs/heads/crosslink/hub" {
            if let Some(p) = current.take() {
                result.push(p);
            }
        }
    }
    Ok(result)
}

fn push_v2_branch_deletion(repo_root: &Path, remote: &str) -> Result<()> {
    let output = Command::new("git")
        .current_dir(repo_root)
        .args(["push", remote, ":refs/heads/crosslink/hub"])
        .output()
        .context("failed to run git push to delete crosslink/hub on the remote")?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);

    if stderr.contains("remote ref does not exist")
        || stderr.contains("Could not read from remote")
        || stderr.contains("does not appear to be a git repository")
        || stderr.contains("No such remote")
    {
        tracing::warn!("remote crosslink/hub deletion skipped: {}", stderr.trim());
        return Ok(());
    }
    bail!(
        "failed to delete crosslink/hub on remote '{remote}': {}",
        stderr.trim()
    );
}

fn find_pending_offline(cache_dir: &Path) -> Result<Vec<IssueFile>> {
    let issues_dir = cache_dir.join("issues");
    let all = read_all_issue_files(&issues_dir)?;
    Ok(all.into_iter().filter(|i| i.display_id.is_none()).collect())
}

struct V2Divergence {
    v2_tip: String,

    genesis_commit: String,

    commits_ahead: Option<usize>,

    v2_issue_count: usize,
}

impl V2Divergence {
    fn is_stale(&self) -> bool {
        self.commits_ahead.is_none_or(|n| n > 0)
    }

    fn summary_line(&self) -> String {
        let tip = short_sha(&self.v2_tip);
        self.commits_ahead.map_or_else(
            || {
                format!(
                    "crosslink/hub (v2, tip {tip}) does not descend from the v3 genesis \
                     (unrelated history; {} issue(s) on v2)",
                    self.v2_issue_count
                )
            },
            |n| {
                format!(
                    "crosslink/hub (v2, tip {tip}) is {n} commit(s) ahead of the v3 genesis \
                     ({} issue(s) on v2)",
                    self.v2_issue_count
                )
            },
        )
    }

    fn adopt_refusal_message(&self) -> String {
        format!(
            "refusing to adopt the remote v3 hub: it is STALE relative to your live \
             crosslink/hub (v2) branch.\n\n\
             The remote v3 hub froze at genesis commit {genesis}.\n\
             {summary}.\n\
             Adopting would switch this machine to the frozen v3 hub and HIDE every issue \
             created or edited after the genesis. The data is safe on crosslink/hub but would \
             not be visible.\n\n\
             To recover, regenerate v3 from your current v2 tip (recommended):\n\
            \x20 crosslink migrate hub-v3 --remigrate-from-v2\n\n\
             Or, if you intentionally want the frozen v3 hub and accept hiding the newer v2 \
             work:\n\
            \x20 crosslink migrate hub-v3 --adopt-stale",
            genesis = short_sha(&self.genesis_commit),
            summary = self.summary_line(),
        )
    }

    fn finalize_refusal_message(&self) -> String {
        format!(
            "refusing to finalize: your live crosslink/hub (v2) branch is AHEAD of the v3 hub, \
             so deleting it would destroy the only complete copy of recent work.\n\n\
             The v3 hub froze at genesis commit {genesis}.\n\
             {summary}.\n\n\
             Re-migrate from the current v2 tip first, then finalize:\n\
            \x20 crosslink migrate hub-v3 --remigrate-from-v2\n\
            \x20 crosslink migrate hub-v3 --finalize --yes-delete-v2",
            genesis = short_sha(&self.genesis_commit),
            summary = self.summary_line(),
        )
    }
}

fn compute_v2_divergence(cache_dir: &Path, genesis_commit: &str) -> Result<Option<V2Divergence>> {
    let Some(v2_tip) = git_rev_parse(cache_dir, V2_HUB_BRANCH)? else {
        return Ok(None);
    };
    let commits_ahead = if v2_tip == genesis_commit {
        Some(0)
    } else {
        git_rev_list_count(cache_dir, genesis_commit, &v2_tip)?
    };
    let v2_issue_count = read_all_issue_files(&cache_dir.join("issues")).map_or(0, |v| v.len());
    Ok(Some(V2Divergence {
        v2_tip,
        genesis_commit: genesis_commit.to_string(),
        commits_ahead,
        v2_issue_count,
    }))
}

fn read_remote_genesis_commit(cache_dir: &Path, remote: &str) -> Result<Option<String>> {
    let fetch = run_git(cache_dir, &["fetch", remote, META_REF])?;
    if !fetch.status.success() {
        tracing::warn!(
            "could not fetch the remote meta ref for divergence check: {}",
            String::from_utf8_lossy(&fetch.stderr).trim()
        );
        return Ok(None);
    }
    let Some(bytes) = git_cat_file_blob_optional(cache_dir, "FETCH_HEAD:hub.json")? else {
        return Ok(None);
    };
    let meta: HubMeta = serde_json::from_slice(&bytes)
        .context("parsing remote hub.json for the divergence check")?;
    Ok(Some(meta.migrated_from_commit))
}

fn git_rev_list_count(repo_dir: &Path, exclude: &str, include: &str) -> Result<Option<usize>> {
    let range = format!("{exclude}..{include}");
    let output = run_git(repo_dir, &["rev-list", "--count", &range])?;
    if !output.status.success() {
        return Ok(None);
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<usize>()
        .ok())
}

fn short_sha(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
}

fn print_hub_meta(meta: &HubMeta) {
    println!("  hub_version: {}", meta.hub_version);
    println!("  migrated_from_commit: {}", meta.migrated_from_commit);
    println!("  migrated_at: {}", meta.migrated_at.to_rfc3339());
    if let Some(f) = meta.finalized_at {
        println!("  finalized_at: {}", f.to_rfc3339());
    }
}

fn print_phase_a_summary(
    seeded: &SeededRefs,
    genesis: &CheckpointState,
    report: &VerifyReport,
    push: &PushSummary,
) {
    println!("hub-v3 migration complete (verified).");
    println!();
    println!("  agents seeded:       {}", seeded.agents.len());
    println!("  issues in genesis:   {}", genesis.issues.len());
    println!("  comments in genesis: {}", report.comments);
    println!("  milestones:          {}", genesis.milestones.len());
    println!("  locks:               {}", genesis.locks.len());
    println!();
    println!(
        "  verification checked: issues={} comments={} milestones={} locks={}",
        report.issues, report.comments, report.milestones, report.locks
    );
    println!();
    println!("  refs pushed: {}", push.pushed.len());
    for name in &push.pushed {
        println!("    {name}");
    }
    if !push.failed.is_empty() {
        println!(
            "  refs NOT pushed ({}) — local migration is complete; re-run `crosslink migrate \
             hub-v3` to retry pushing:",
            push.failed.len()
        );
        for (name, why) in &push.failed {
            println!("    {name}: {why}");
        }
    }
    println!();
    println!(
        "Next: soak/cutover. The old crosslink/hub branch is left intact as a read-only escape \
         hatch. When you are confident the v3 refs are correct, run \
         `crosslink migrate hub-v3 --finalize --yes-delete-v2` to delete it."
    );
}

fn print_mixed_version_warning() {
    println!();
    println!(
        "WARNING: until you finalize, already-deployed v2 binaries keep operating the \
         crosslink/hub branch and their writes are NOT reflected in v3. Avoid mixed-version \
         operation, or finish the cutover."
    );
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

fn for_each_ref(repo_dir: &Path, pattern: &str) -> Result<Vec<String>> {
    let output = Command::new("git")
        .current_dir(repo_dir)
        .args(["for-each-ref", "--format=%(refname)", pattern])
        .output()
        .with_context(|| format!("failed to run git for-each-ref for '{pattern}'"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git for-each-ref failed for '{pattern}': {}", stderr.trim());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect())
}

fn ls_remote_crosslink(repo_dir: &Path, remote: &str) -> Result<BTreeMap<String, String>> {
    let output = Command::new("git")
        .current_dir(repo_dir)
        .args(["ls-remote", remote, "refs/heads/crosslink/*"])
        .output()
        .with_context(|| format!("failed to run git ls-remote for '{remote}'"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git ls-remote failed for '{remote}': {}", stderr.trim());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut map = BTreeMap::new();
    for line in stdout.lines() {
        if let Some((sha, name)) = line.split_once('\t') {
            map.insert(name.trim().to_string(), sha.trim().to_string());
        }
    }
    Ok(map)
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

fn run_git(repo_dir: &Path, args: &[&str]) -> Result<std::process::Output> {
    let output = Command::new("git")
        .current_dir(repo_dir)
        .args(args)
        .output()
        .with_context(|| format!("failed to run git {args:?}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git {args:?} failed: {}", stderr.trim());
    }
    Ok(output)
}

fn git_main_repo_root(repo_dir: &Path) -> Result<PathBuf> {
    let output = Command::new("git")
        .current_dir(repo_dir)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .output()
        .context("failed to run git rev-parse --git-common-dir")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git rev-parse --git-common-dir failed: {}", stderr.trim());
    }
    let common = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let common_path = PathBuf::from(&common);
    let root = if common_path.file_name().and_then(|n| n.to_str()) == Some(".git") {
        common_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or(common_path)
    } else {
        common_path
    };
    Ok(root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::{AgentConfig, AgentRole};
    use std::process::Command;
    use tempfile::TempDir;

    fn setup_v2_hub() -> (TempDir, TempDir, std::path::PathBuf, std::path::PathBuf) {
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
                v2_branch_present: true
            }
        );

        let cp = rev(&cache_dir, CHECKPOINT_REF);
        let mt = rev(&cache_dir, META_REF);
        let al = rev(&cache_dir, &agent_ref_name("alpha").unwrap());
        hub_v3(&crosslink_dir, false, false, false, false).unwrap();
        assert_eq!(cp, rev(&cache_dir, CHECKPOINT_REF));
        assert_eq!(mt, rev(&cache_dir, META_REF));
        assert_eq!(al, rev(&cache_dir, &agent_ref_name("alpha").unwrap()));

        assert!(rev(&cache_dir, V2_HUB_BRANCH).is_some());
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
    fn rollback_restores_all_refs_when_verify_fails() {
        let (_w, _r, _crosslink_dir, cache_dir) = setup_v2_hub();

        let pre = snapshot_crosslink_refs(&cache_dir).unwrap();

        let mut genesis = build_genesis_from_files(&cache_dir).unwrap();
        let victim = *genesis.issues.keys().next().unwrap();
        genesis.issues.remove(&victim);

        let v2_tip = git_rev_parse(&cache_dir, V2_HUB_BRANCH).unwrap().unwrap();
        let pre_tips = snapshot_crosslink_refs(&cache_dir).unwrap();

        seed_v3_refs(&cache_dir, &genesis, &v2_tip).unwrap();
        assert!(rev(&cache_dir, CHECKPOINT_REF).is_some());

        let result = verify_against_files(&cache_dir, &genesis);
        assert!(result.is_err(), "tampered genesis must fail verification");

        rollback_refs(&cache_dir, &pre_tips).unwrap();
        for tip in &pre {
            assert_eq!(
                rev(&cache_dir, &tip.name),
                tip.old_sha,
                "ref {} must be restored to its pre-migration tip",
                tip.name
            );
        }

        assert!(rev(&cache_dir, V2_HUB_BRANCH).is_some());
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
    fn finalize_requires_confirmation_then_deletes_v2() {
        let (_w, _r, crosslink_dir, cache_dir) = setup_v2_hub();
        hub_v3(&crosslink_dir, false, false, false, false).unwrap();

        let err = hub_v3(&crosslink_dir, true, false, false, false).unwrap_err();
        assert!(
            err.to_string().contains("yes-delete-v2"),
            "finalize without confirmation must refuse, got: {err}"
        );

        assert!(rev(&cache_dir, V2_HUB_BRANCH).is_some());

        let repo_root = wp_of(&crosslink_dir);
        hub_v3(&crosslink_dir, true, true, false, false).unwrap();

        assert!(
            rev(&repo_root, V2_HUB_BRANCH).is_none(),
            "local v2 branch must be gone"
        );

        let ls = Command::new("git")
            .current_dir(&repo_root)
            .args(["ls-remote", "--heads", "origin", "crosslink/hub"])
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&ls.stdout).trim().is_empty(),
            "remote crosslink/hub must be deleted"
        );

        let meta = read_hub_meta(&repo_root).unwrap().unwrap();
        assert!(meta.finalized_at.is_some(), "finalized_at must be stamped");

        let ls2 = Command::new("git")
            .current_dir(&repo_root)
            .args(["fetch", "origin", "crosslink/hub"])
            .output()
            .unwrap();
        assert!(
            !ls2.status.success(),
            "fetching the deleted crosslink/hub branch must fail (old-binary hard stop)"
        );
        let stderr = String::from_utf8_lossy(&ls2.stderr);
        assert!(
            stderr.contains("crosslink/hub") || stderr.contains("couldn't find remote ref"),
            "fetch error should reference the missing branch: {stderr}"
        );
    }

    fn wp_of(crosslink_dir: &Path) -> std::path::PathBuf {
        crosslink_dir.parent().unwrap().to_path_buf()
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
    fn finalize_teardown_survives_self_referential_worktree() {
        let (work, _r, crosslink_dir, cache_dir) = setup_v2_hub();
        let repo_root = work.path();

        let git_link = std::fs::read_to_string(cache_dir.join(".git")).unwrap();
        let admin = git_link
            .trim()
            .strip_prefix("gitdir: ")
            .unwrap()
            .to_string();
        let admin = std::path::PathBuf::from(admin);
        assert!(admin.join("HEAD").exists(), "fixture admin dir must exist");

        std::fs::write(admin.join("gitdir"), format!("{}/.git\n", admin.display())).unwrap();
        std::fs::write(admin.join(".git"), format!("gitdir: {}\n", admin.display())).unwrap();
        std::fs::remove_dir_all(&cache_dir).unwrap();

        let hosts = worktrees_on_hub_branch(repo_root).unwrap();
        assert_eq!(hosts.len(), 1, "corrupt registration must be visible");
        assert!(hosts[0].contains(".git"), "host is the admin dir itself");

        delete_v2_branch_local(&cache_dir, repo_root)
            .expect("teardown must survive the self-referential registration");

        assert!(
            worktrees_on_hub_branch(repo_root).unwrap().is_empty(),
            "no worktree may still host crosslink/hub"
        );
        let branch_gone = std::process::Command::new("git")
            .current_dir(repo_root)
            .args(["rev-parse", "--verify", "crosslink/hub"])
            .output()
            .unwrap();
        assert!(
            !branch_gone.status.success(),
            "crosslink/hub must be deleted"
        );
        assert!(!cache_dir.exists(), "leftover cache dir must be cleaned");

        let _ = crosslink_dir;
    }

    #[test]
    fn v2_local_v3_remote_adopts_instead_of_migrating() {
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

        hub_v3(&cl_b, false, false, false, false).expect("B must adopt the remote v3 hub");

        assert!(
            matches!(
                hub_v3::detect_hub_version(&cache_b).unwrap(),
                HubVersion::V3 { .. }
            ),
            "B must operate v3 after adoption"
        );

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
            "adoption must never move the remote checkpoint"
        );

        let source = crate::hub_source::RefHubSource::new(&cache_b).unwrap();
        let state = crate::compaction::reduce(&source).unwrap().state;
        assert!(
            !state.issues.is_empty(),
            "B must see the migrated issues after adoption"
        );

        hub_v3(&cl_b, false, false, false, false).expect("re-run after adoption must be a no-op");
    }

    fn advance_v2_branch(cache_dir: &Path, marker: &str) -> String {
        std::fs::write(cache_dir.join("post-migration-marker.txt"), marker).unwrap();
        run(cache_dir, &["add", "-A"]);
        run(
            cache_dir,
            &["commit", "-m", "v2: post-migration write", "--no-gpg-sign"],
        );
        rev(cache_dir, V2_HUB_BRANCH).unwrap()
    }

    #[test]
    fn compute_v2_divergence_detects_advanced_v2() {
        let (_w, _r, crosslink_dir, cache_dir) = setup_v2_hub();
        hub_v3(&crosslink_dir, false, false, false, false).unwrap();

        let genesis = read_hub_meta(&cache_dir)
            .unwrap()
            .unwrap()
            .migrated_from_commit;

        let div = compute_v2_divergence(&cache_dir, &genesis)
            .unwrap()
            .expect("v2 branch present");
        assert_eq!(div.commits_ahead, Some(0));
        assert!(!div.is_stale(), "matching tips must not be stale");

        advance_v2_branch(&cache_dir, "one");
        let div = compute_v2_divergence(&cache_dir, &genesis)
            .unwrap()
            .expect("v2 branch present");
        assert_eq!(div.commits_ahead, Some(1));
        assert!(div.is_stale(), "an advanced v2 branch must read as stale");
        assert!(
            div.adopt_refusal_message().contains("--remigrate-from-v2"),
            "refusal must point at the recovery path"
        );
    }

    #[test]
    fn finalize_refuses_when_v2_is_ahead_of_v3() {
        let (_w, _r, crosslink_dir, cache_dir) = setup_v2_hub();
        hub_v3(&crosslink_dir, false, false, false, false).unwrap();

        advance_v2_branch(&cache_dir, "stale");

        let err = hub_v3(&crosslink_dir, true, true, false, false).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("refusing to finalize") && msg.contains("AHEAD"),
            "finalize must refuse with a divergence message, got: {msg}"
        );
        assert!(
            msg.contains("--remigrate-from-v2"),
            "finalize refusal must point at the recovery path: {msg}"
        );

        assert!(
            rev(&cache_dir, V2_HUB_BRANCH).is_some(),
            "v2 branch must still exist after a refused finalize"
        );
    }

    #[test]
    fn remigrate_regenerates_genesis_from_current_v2_tip() {
        let (_w, _r, crosslink_dir, cache_dir) = setup_v2_hub();
        hub_v3(&crosslink_dir, false, false, false, false).unwrap();

        let genesis_t0 = read_hub_meta(&cache_dir)
            .unwrap()
            .unwrap()
            .migrated_from_commit;
        let checkpoint_t0 = rev(&cache_dir, CHECKPOINT_REF).unwrap();

        let t1 = advance_v2_branch(&cache_dir, "remigrate");
        assert_ne!(t1, genesis_t0, "v2 tip must have advanced");

        hub_v3(&crosslink_dir, false, false, false, true).unwrap();

        let meta = read_hub_meta(&cache_dir).unwrap().unwrap();
        assert_eq!(
            meta.migrated_from_commit, t1,
            "remigrate must anchor the genesis at the current v2 tip"
        );
        assert_ne!(meta.migrated_from_commit, genesis_t0);

        assert_ne!(
            rev(&cache_dir, CHECKPOINT_REF).unwrap(),
            checkpoint_t0,
            "remigrate must mint a new checkpoint"
        );

        let div = compute_v2_divergence(&cache_dir, &meta.migrated_from_commit)
            .unwrap()
            .expect("v2 branch present");
        assert!(
            !div.is_stale(),
            "after remigrate the v3 genesis must match the v2 tip"
        );

        let remote_cp = std::process::Command::new("git")
            .current_dir(&cache_dir)
            .args(["ls-remote", "origin", "refs/heads/crosslink/checkpoint"])
            .output()
            .unwrap();
        let remote_sha = String::from_utf8_lossy(&remote_cp.stdout)
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_string();
        assert_eq!(
            remote_sha,
            rev(&cache_dir, CHECKPOINT_REF).unwrap(),
            "the remote checkpoint must be force-updated to the regenerated genesis"
        );
    }

    #[test]
    fn adopt_refuses_stale_remote_v3_unless_forced() {
        let (_wa, remote_dir, cl_a, cache_a) = setup_v2_hub();

        run(&cache_a, &["push", "origin", "crosslink/hub"]);

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

        hub_v3(&cl_a, false, false, false, false).expect("A migrates");

        advance_v2_branch(&cache_a, "post-migration");
        run(&cache_a, &["push", "origin", "crosslink/hub"]);

        let err = hub_v3(&cl_b, false, false, false, false).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("refusing to adopt") && msg.contains("STALE"),
            "B must refuse to adopt a stale v3 hub, got: {msg}"
        );

        assert!(
            matches!(
                hub_v3::detect_hub_version(&cache_b).unwrap(),
                HubVersion::V2Only
            ),
            "B must remain v2-only after a refused adopt"
        );

        hub_v3(&cl_b, false, false, true, false).expect("B adopts with --adopt-stale");
        assert!(
            matches!(
                hub_v3::detect_hub_version(&cache_b).unwrap(),
                HubVersion::V3 { .. }
            ),
            "B must operate v3 after a forced adopt"
        );
    }

    fn append_post_genesis_event(cache_dir: &Path, agent_id: &str, agent_seq: u64) -> Uuid {
        use crate::events::{Event, EventEnvelope};
        let new_uuid = Uuid::new_v4();
        let env = EventEnvelope {
            agent_id: agent_id.to_string(),
            agent_seq,
            timestamp: Utc::now() + chrono::Duration::seconds(60),
            event: Event::IssueCreated {
                uuid: new_uuid,
                title: "Post-genesis issue".to_string(),
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
        };
        hub_v3::append_event_to_ref(cache_dir, agent_id, &env).unwrap();
        new_uuid
    }

    fn strip_seed_metadata(cache_dir: &Path) {
        let mut meta = read_hub_meta(cache_dir).unwrap().unwrap();
        meta.genesis_checkpoint_commit = None;
        meta.seed_agent_tips = None;
        let hub_json = serde_json::to_vec_pretty(&meta).unwrap();
        let files: Vec<(&str, &[u8])> = vec![("hub.json", &hub_json)];
        hub_v3::commit_files_to_ref(cache_dir, META_REF, &files, "test: strip seed metadata")
            .unwrap();
    }

    #[test]
    fn finalize_succeeds_after_post_migration_v3_activity() {
        let (_w, _r, crosslink_dir, cache_dir) = setup_v2_hub();
        hub_v3(&crosslink_dir, false, false, false, false).unwrap();

        let meta = read_hub_meta(&cache_dir).unwrap().unwrap();
        assert!(
            meta.genesis_checkpoint_commit.is_some(),
            "genesis commit recorded"
        );
        assert!(
            meta.seed_agent_tips.unwrap().contains_key("alpha"),
            "alpha seed tip recorded"
        );

        append_post_genesis_event(&cache_dir, "alpha", 9999);
        let advanced = crate::compaction::reduce(&RefHubSource::new(&cache_dir).unwrap())
            .unwrap()
            .state;
        let state_bytes = serde_json::to_vec_pretty(&advanced).unwrap();
        hub_v3::commit_blob_to_ref(
            &cache_dir,
            CHECKPOINT_REF,
            "state.json",
            &state_bytes,
            "test: advance checkpoint past genesis",
        )
        .unwrap();

        let repo_root = wp_of(&crosslink_dir);
        hub_v3(&crosslink_dir, true, true, false, false).unwrap();
        assert!(
            rev(&repo_root, V2_HUB_BRANCH).is_none(),
            "v2 branch deleted"
        );
        let meta = read_hub_meta(&repo_root).unwrap().unwrap();
        assert!(meta.finalized_at.is_some(), "finalized_at stamped");
    }

    #[test]
    fn finalize_refuses_when_seed_ancestry_broken() {
        let (_w, _r, crosslink_dir, cache_dir) = setup_v2_hub();
        hub_v3(&crosslink_dir, false, false, false, false).unwrap();

        let alpha_ref = agent_ref_name("alpha").unwrap();
        let ckpt = rev(&cache_dir, CHECKPOINT_REF).unwrap();
        let out = Command::new("git")
            .current_dir(&cache_dir)
            .args(["update-ref", &alpha_ref, &ckpt])
            .output()
            .unwrap();
        assert!(out.status.success());

        let err = hub_v3(&crosslink_dir, true, true, false, false).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("no longer an ancestor") && msg.contains("remigrate-from-v2"),
            "ancestry break must refuse with remigrate guidance, got: {msg}"
        );
        assert!(
            rev(&cache_dir, V2_HUB_BRANCH).is_some(),
            "v2 branch preserved"
        );
    }

    #[test]
    fn finalize_refuses_when_v2_files_changed_after_migration() {
        let (_w, _r, crosslink_dir, cache_dir) = setup_v2_hub();
        hub_v3(&crosslink_dir, false, false, false, false).unwrap();

        let issues_dir = cache_dir.join("issues");
        let entry = std::fs::read_dir(&issues_dir)
            .unwrap()
            .flatten()
            .find(|e| e.path().join("issue.json").exists())
            .expect("fixture has at least one issue file");
        let p = entry.path().join("issue.json");
        let mut v: serde_json::Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        v["title"] = serde_json::Value::String("tampered after migration".into());
        std::fs::write(&p, serde_json::to_vec_pretty(&v).unwrap()).unwrap();

        let err = hub_v3(&crosslink_dir, true, true, false, false).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("no longer reproduce the genesis"),
            "v2 file change must refuse via the frozen-check, got: {msg}"
        );
        assert!(
            rev(&cache_dir, V2_HUB_BRANCH).is_some(),
            "v2 branch preserved"
        );
    }

    #[test]
    fn finalize_pre_metadata_hub_unused_falls_back_and_succeeds() {
        let (_w, _r, crosslink_dir, cache_dir) = setup_v2_hub();
        hub_v3(&crosslink_dir, false, false, false, false).unwrap();
        strip_seed_metadata(&cache_dir);

        let repo_root = wp_of(&crosslink_dir);
        hub_v3(&crosslink_dir, true, true, false, false).unwrap();
        let meta = read_hub_meta(&repo_root).unwrap().unwrap();
        assert!(meta.finalized_at.is_some(), "fallback finalize stamped");
        assert!(meta.genesis_checkpoint_commit.is_some());
        assert!(meta.seed_agent_tips.is_some());
    }

    #[test]
    fn finalize_pre_metadata_hub_preserves_post_migration_activity() {
        let (_w, _r, crosslink_dir, cache_dir) = setup_v2_hub();
        hub_v3(&crosslink_dir, false, false, false, false).unwrap();
        strip_seed_metadata(&cache_dir);
        let post_uuid = append_post_genesis_event(&cache_dir, "alpha", 9999);
        let alpha_ref = agent_ref_name("alpha").unwrap();
        let alpha_tip = rev(&cache_dir, &alpha_ref).unwrap();

        let repo_root = wp_of(&crosslink_dir);
        hub_v3(&crosslink_dir, true, true, false, false).unwrap();

        assert_eq!(
            rev(&repo_root, &alpha_ref).as_deref(),
            Some(alpha_tip.as_str())
        );
        let state = compaction::reduce(&RefHubSource::new(&repo_root).unwrap())
            .unwrap()
            .state;
        assert!(state.issues.contains_key(&post_uuid));
        let meta = read_hub_meta(&repo_root).unwrap().unwrap();
        assert!(meta.finalized_at.is_some());
        assert!(meta.genesis_checkpoint_commit.is_some());
        assert!(meta.seed_agent_tips.unwrap().contains_key("alpha"));
    }

    #[test]
    fn finalize_pre_metadata_hub_preserves_v3_only_agent_ref() {
        let (_w, _r, crosslink_dir, cache_dir) = setup_v2_hub();
        hub_v3(&crosslink_dir, false, false, false, false).unwrap();
        strip_seed_metadata(&cache_dir);
        let post_uuid = append_post_genesis_event(&cache_dir, "gamma", 1);
        let gamma_ref = agent_ref_name("gamma").unwrap();
        let gamma_tip = rev(&cache_dir, &gamma_ref).unwrap();

        let repo_root = wp_of(&crosslink_dir);
        hub_v3(&crosslink_dir, true, true, false, false).unwrap();

        assert_eq!(
            rev(&repo_root, &gamma_ref).as_deref(),
            Some(gamma_tip.as_str())
        );
        let state = compaction::reduce(&RefHubSource::new(&repo_root).unwrap())
            .unwrap()
            .state;
        assert!(state.issues.contains_key(&post_uuid));
        let meta = read_hub_meta(&repo_root).unwrap().unwrap();
        assert!(!meta.seed_agent_tips.unwrap().contains_key("gamma"));
    }
}
