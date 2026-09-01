use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::path::Path;
use std::process::Command;

use crate::knowledge::KnowledgeManager;
use crate::sync::SyncManager;

pub struct PruneOpts {
    pub dry_run: bool,
    pub force: bool,
    pub keep_commits: usize,
    pub hub_only: bool,
    pub knowledge_only: bool,
}

#[derive(Debug, Serialize)]
struct BranchStats {
    branch: String,
    commits_before: usize,
    commits_after: usize,
}

fn git_in_dir(dir: &Path, args: &[&str]) -> Result<std::process::Output> {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .with_context(|| format!("Failed to run git {:?} in {}", args, dir.display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "git {:?} in {} failed: {}",
            args,
            dir.display(),
            stderr.trim()
        );
    }
    Ok(output)
}

fn count_commits(cache_dir: &Path) -> Result<usize> {
    let output = git_in_dir(cache_dir, &["rev-list", "--count", "HEAD"])?;
    let count_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
    count_str
        .parse::<usize>()
        .with_context(|| format!("Failed to parse commit count: {count_str:?}"))
}

fn remove_stale_hub_data(cache_dir: &Path) -> Result<Vec<String>> {
    let mut removed = Vec::new();

    let heartbeats_dir = cache_dir.join("heartbeats");
    if heartbeats_dir.is_dir() {
        for entry in std::fs::read_dir(&heartbeats_dir)? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                let name = entry.file_name().to_string_lossy().to_string();
                removed.push(format!("heartbeats/{name}"));
                std::fs::remove_file(entry.path())?;
            }
        }

        if std::fs::read_dir(&heartbeats_dir)?.next().is_none() {
            std::fs::remove_dir(&heartbeats_dir)?;
        }
    }

    let agents_dir = cache_dir.join("agents");
    if agents_dir.is_dir() {
        for entry in std::fs::read_dir(&agents_dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let agent_dir = entry.path();
            let events_log = agent_dir.join("events.log");
            let has_events =
                events_log.exists() && std::fs::metadata(&events_log).is_ok_and(|m| m.len() > 0);

            if !has_events {
                let agent_name = entry.file_name().to_string_lossy().to_string();
                removed.push(format!("agents/{agent_name}/"));
                std::fs::remove_dir_all(&agent_dir)?;
            }
        }
    }

    Ok(removed)
}

fn count_stale_hub_data(cache_dir: &Path) -> Vec<String> {
    let mut stale = Vec::new();

    let heartbeats_dir = cache_dir.join("heartbeats");
    if heartbeats_dir.is_dir() {
        if let Ok(entries) = std::fs::read_dir(&heartbeats_dir) {
            for entry in entries.flatten() {
                if entry.file_type().is_ok_and(|t| t.is_file()) {
                    stale.push(format!(
                        "heartbeats/{}",
                        entry.file_name().to_string_lossy()
                    ));
                }
            }
        }
    }

    let agents_dir = cache_dir.join("agents");
    if agents_dir.is_dir() {
        if let Ok(entries) = std::fs::read_dir(&agents_dir) {
            for entry in entries.flatten() {
                if !entry.file_type().is_ok_and(|t| t.is_dir()) {
                    continue;
                }
                let events_log = entry.path().join("events.log");
                let has_events = events_log.exists()
                    && std::fs::metadata(&events_log).is_ok_and(|m| m.len() > 0);
                if !has_events {
                    stale.push(format!("agents/{}/", entry.file_name().to_string_lossy()));
                }
            }
        }
    }

    stale
}

fn squash_branch(
    cache_dir: &Path,
    branch: &str,
    remote: &str,
    keep_commits: usize,
    dry_run: bool,
) -> Result<BranchStats> {
    let commits_before = count_commits(cache_dir)?;

    if commits_before <= keep_commits.max(1) {
        return Ok(BranchStats {
            branch: branch.to_string(),
            commits_before,
            commits_after: commits_before,
        });
    }

    if dry_run {
        let commits_after = keep_commits.max(1);
        return Ok(BranchStats {
            branch: branch.to_string(),
            commits_before,
            commits_after,
        });
    }

    let commits_after = if keep_commits <= 1 {
        git_in_dir(cache_dir, &["add", "-A"])?;

        let tree_output = git_in_dir(cache_dir, &["write-tree"])?;
        let tree_hash = String::from_utf8_lossy(&tree_output.stdout)
            .trim()
            .to_string();

        let commit_output = Command::new("git")
            .current_dir(cache_dir)
            .args([
                "commit-tree",
                &tree_hash,
                "-m",
                &format!(
                    "prune: squash {branch} history to current state\n\nSquashed {commits_before} commit(s)."
                ),
            ])
            .output()
            .context("Failed to create squash commit")?;

        if !commit_output.status.success() {
            let stderr = String::from_utf8_lossy(&commit_output.stderr);
            bail!("git commit-tree failed: {}", stderr.trim());
        }

        let new_head = String::from_utf8_lossy(&commit_output.stdout)
            .trim()
            .to_string();

        git_in_dir(
            cache_dir,
            &["update-ref", &format!("refs/heads/{branch}"), &new_head],
        )?;

        git_in_dir(cache_dir, &["reset", "--hard", &new_head])?;

        1
    } else {
        let base_ref = format!("HEAD~{keep_commits}");
        let base_hash_output = git_in_dir(cache_dir, &["rev-parse", &base_ref])?;
        let base_hash = String::from_utf8_lossy(&base_hash_output.stdout)
            .trim()
            .to_string();

        let tree_output = git_in_dir(cache_dir, &["rev-parse", &format!("{base_hash}^{{tree}}")])?;
        let tree_hash = String::from_utf8_lossy(&tree_output.stdout)
            .trim()
            .to_string();

        let commit_output = Command::new("git")
            .current_dir(cache_dir)
            .args([
                "commit-tree",
                &tree_hash,
                "-m",
                &format!(
                    "prune: squash {} history (kept last {} commits)\n\nSquashed {} commit(s).",
                    branch,
                    keep_commits,
                    commits_before - keep_commits
                ),
            ])
            .output()
            .context("Failed to create base squash commit")?;

        if !commit_output.status.success() {
            let stderr = String::from_utf8_lossy(&commit_output.stderr);
            bail!("git commit-tree failed: {}", stderr.trim());
        }

        let new_base = String::from_utf8_lossy(&commit_output.stdout)
            .trim()
            .to_string();

        let tip_output = git_in_dir(cache_dir, &["rev-parse", "HEAD"])?;
        let tip_hash = String::from_utf8_lossy(&tip_output.stdout)
            .trim()
            .to_string();

        git_in_dir(cache_dir, &["checkout", "--detach", "HEAD"])?;
        git_in_dir(cache_dir, &["reset", "--hard", &new_base])?;

        let range = format!("{base_hash}..{tip_hash}");
        let log_output = git_in_dir(cache_dir, &["rev-list", "--reverse", &range])?;
        let log_text = String::from_utf8_lossy(&log_output.stdout)
            .trim()
            .to_string();

        for commit_hash in log_text.lines() {
            if !commit_hash.is_empty() {
                git_in_dir(cache_dir, &["cherry-pick", commit_hash])?;
            }
        }

        let new_tip_output = git_in_dir(cache_dir, &["rev-parse", "HEAD"])?;
        let new_tip = String::from_utf8_lossy(&new_tip_output.stdout)
            .trim()
            .to_string();
        git_in_dir(
            cache_dir,
            &["update-ref", &format!("refs/heads/{branch}"), &new_tip],
        )?;
        git_in_dir(cache_dir, &["checkout", branch])?;

        keep_commits + 1
    };

    let refspec = format!("{branch}:{branch}");
    git_in_dir(cache_dir, &["push", "--force", remote, &refspec])?;

    Ok(BranchStats {
        branch: branch.to_string(),
        commits_before,
        commits_after,
    })
}

pub fn run(crosslink_dir: &Path, opts: &PruneOpts, json: bool) -> Result<()> {
    if !opts.force && !opts.dry_run {
        tracing::warn!("This will rewrite branch history and force-push.");
        println!("Use --force to confirm, or --dry-run to preview.");
        return Ok(());
    }

    let mut results: Vec<BranchStats> = Vec::new();
    let mut stale_removed: Vec<String> = Vec::new();

    if !opts.knowledge_only {
        let sync = SyncManager::new(crosslink_dir)?;
        if opts.dry_run {
            anyhow::ensure!(
                sync.is_initialized(),
                "Hub cache is unavailable. Retry after repository readiness is restored."
            );
        } else {
            sync.init_cache()?;
            sync.fetch()?;
        }

        let cache_dir = sync.cache_path();

        if opts.dry_run {
            stale_removed = count_stale_hub_data(cache_dir);
        } else {
            let removed = remove_stale_hub_data(cache_dir)?;
            if !removed.is_empty() {
                let _ = git_in_dir(cache_dir, &["add", "-A"]);
                let has_changes = git_in_dir(cache_dir, &["diff", "--cached", "--quiet"]).is_err();
                if has_changes {
                    git_in_dir(cache_dir, &["commit", "-m", "prune: remove stale hub data"])?;
                }
                stale_removed = removed;
            }
        }

        let stats = squash_branch(
            cache_dir,
            "crosslink/hub",
            sync.remote(),
            opts.keep_commits,
            opts.dry_run,
        )?;
        results.push(stats);
    }

    if !opts.hub_only {
        let km = KnowledgeManager::new(crosslink_dir)?;
        if km.is_initialized() {
            if !opts.dry_run {
                km.init_cache()?;
                km.sync()?;
            }

            let cache_dir = km.cache_path();
            let remote = crate::sync::read_tracker_remote(km.crosslink_dir());

            let stats = squash_branch(
                cache_dir,
                "crosslink/knowledge",
                &remote,
                opts.keep_commits,
                opts.dry_run,
            )?;
            results.push(stats);
        } else {
            tracing::info!("Knowledge branch not initialized, skipping.");
        }
    }

    if json {
        #[derive(Serialize)]
        struct PruneReport {
            branches: Vec<BranchStats>,
            stale_data_removed: Vec<String>,
            dry_run: bool,
        }
        let report = PruneReport {
            branches: results,
            stale_data_removed: stale_removed,
            dry_run: opts.dry_run,
        };
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    if opts.dry_run {
        println!("Prune plan (dry run):\n");
    }

    for stats in &results {
        if stats.commits_before == stats.commits_after {
            println!(
                "  {} — {} commit(s), nothing to prune",
                stats.branch, stats.commits_before
            );
        } else {
            let verb = if opts.dry_run {
                "would remove"
            } else {
                "removed"
            };
            println!(
                "  {} — {} → {} commit(s) ({} {})",
                stats.branch,
                stats.commits_before,
                stats.commits_after,
                stats.commits_before - stats.commits_after,
                verb,
            );
        }
    }

    if !stale_removed.is_empty() {
        println!(
            "\n  Stale data {}: {} file(s)/dir(s)",
            if opts.dry_run { "to remove" } else { "removed" },
            stale_removed.len()
        );
        for item in &stale_removed {
            println!("    {item}");
        }
    }

    if opts.dry_run {
        println!("\nRun with --force (without --dry-run) to proceed.");
    } else {
        println!("\nDone.");
    }

    Ok(())
}
