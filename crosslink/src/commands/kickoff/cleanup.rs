use anyhow::Result;
use serde::Serialize;
use std::path::Path;
use std::process::Command;

use super::helpers::*;
use super::monitor::discover_agents;
use super::rollback::{
    host_locks_held_by, kept_worktree_remedy, outcome_message, worktree_agent_id, KickoffUndo,
    UndoOps, UndoOutcome, UndoStep,
};
use super::types::*;

#[derive(Debug, Clone, Copy, Default)]
pub struct CleanupOptions {
    pub dry_run: bool,
    pub force: bool,
    pub keep: usize,
    /// Keep each agent's branch even when it has no commits beyond its base.
    pub keep_branch: bool,
    pub json_output: bool,
}

pub fn cleanup(crosslink_dir: &Path, opts: &CleanupOptions) -> Result<()> {
    let CleanupOptions {
        dry_run,
        force,
        keep,
        keep_branch,
        json_output,
    } = *opts;
    let agents = discover_agents(crosslink_dir)?;
    if !dry_run {
        for agent in &agents {
            super::monitor::record_runtime_usage(
                crosslink_dir,
                Path::new(&agent.worktree),
                &agent.id,
            );
        }
    }

    let (active, removable): (Vec<_>, Vec<_>) = agents
        .into_iter()
        .map(|a| {
            let class = classify_agent(&a);
            (a, class)
        })
        .partition(|(_, class)| *class == CleanupClass::Active);

    let (mut to_clean, skipped_stale): (Vec<_>, Vec<_>) = if force {
        (removable, vec![])
    } else {
        removable
            .into_iter()
            .partition(|(_, class)| *class == CleanupClass::Done)
    };

    to_clean.sort_by(|a, b| a.0.worktree.cmp(&b.0.worktree));

    let to_clean = if keep > 0 && to_clean.len() > keep {
        to_clean[..to_clean.len() - keep].to_vec()
    } else if keep > 0 && to_clean.len() <= keep {
        vec![]
    } else {
        to_clean
    };

    if json_output {
        #[derive(Serialize)]
        struct CleanupPlan {
            to_clean: Vec<CleanupPlanEntry>,
            skipped_stale: Vec<CleanupPlanEntry>,
            active: Vec<CleanupPlanEntry>,
            dry_run: bool,
        }
        #[derive(Serialize)]
        struct CleanupPlanEntry {
            id: String,
            status: String,
            class: CleanupClass,
            worktree: String,
            session: Option<String>,
            docker: Option<String>,
        }
        let to_entry = |items: &[(AgentInfo, CleanupClass)]| -> Vec<CleanupPlanEntry> {
            items
                .iter()
                .map(|(a, c)| CleanupPlanEntry {
                    id: a.id.clone(),
                    status: a.status.clone(),
                    class: c.clone(),
                    worktree: a.worktree.clone(),
                    session: a.session.clone(),
                    docker: a.docker.clone(),
                })
                .collect()
        };
        let plan = CleanupPlan {
            to_clean: to_entry(&to_clean),
            skipped_stale: to_entry(&skipped_stale),
            active: to_entry(&active),
            dry_run,
        };
        println!("{}", serde_json::to_string_pretty(&plan)?);
        if dry_run {
            return Ok(());
        }
    }

    if to_clean.is_empty() && skipped_stale.is_empty() {
        if !json_output {
            println!("No agents to clean up.");
        }
        return Ok(());
    }

    if dry_run || !json_output {
        if !to_clean.is_empty() {
            println!("Cleanup candidates:\n");
            for (agent, class) in &to_clean {
                let class_label = match class {
                    CleanupClass::Done => "DONE  ",
                    CleanupClass::Stale => "STALE ",
                    CleanupClass::Active => "      ",
                };
                let wt_display = if agent.worktree.is_empty() {
                    "-".to_string()
                } else {
                    std::path::Path::new(&agent.worktree)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or(&agent.worktree)
                        .to_string()
                };
                let session_info = agent
                    .session
                    .as_deref()
                    .map_or_else(|| "tmux: exited".to_string(), |s| format!("tmux: {s}"));
                let docker_info = agent
                    .docker
                    .as_deref()
                    .map(|d| format!("  docker: {d}"))
                    .unwrap_or_default();
                println!(
                    "  {}  {:<40} worktree: {:<30} {}{}",
                    class_label, agent.id, wt_display, session_info, docker_info
                );
            }
        }

        if !skipped_stale.is_empty() {
            println!(
                "\n{} stale agent(s) skipped (use --force to include):",
                skipped_stale.len()
            );
            for (agent, _) in &skipped_stale {
                let wt_display = if agent.worktree.is_empty() {
                    "-".to_string()
                } else {
                    std::path::Path::new(&agent.worktree)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or(&agent.worktree)
                        .to_string()
                };
                println!("  STALE  {:<40} worktree: {}", agent.id, wt_display);
            }
        }

        if dry_run {
            let wt_count = to_clean
                .iter()
                .filter(|(a, _)| !a.worktree.is_empty())
                .count();
            let tmux_count = to_clean.iter().filter(|(a, _)| a.session.is_some()).count();
            let docker_count = to_clean.iter().filter(|(a, _)| a.docker.is_some()).count();
            println!();
            print!("Would remove {wt_count} worktree(s)");
            if tmux_count > 0 {
                print!(", kill {tmux_count} tmux session(s)");
            }
            if docker_count > 0 {
                print!(", remove {docker_count} container(s)");
            }
            println!(".");
            println!("Run without --dry-run to proceed.");
            return Ok(());
        }

        println!();
    }

    let mut results: Vec<CleanupResult> = Vec::new();

    for (agent, class) in &to_clean {
        let mut result = CleanupResult {
            id: agent.id.clone(),
            class: class.clone(),
            worktree_removed: false,
            tmux_killed: false,
            container_removed: false,
            locks_released: Vec::new(),
            branch_deleted: false,
            worktree_kept: false,
            warnings: Vec::new(),
            error: None,
        };

        if let Some(ref session_name) = agent.session {
            match Command::new("tmux")
                .args(["kill-session", "-t", session_name])
                .output()
            {
                Ok(o) if o.status.success() => {
                    result.tmux_killed = true;
                    if !json_output {
                        println!("  Killed tmux session: {session_name}");
                    }
                }
                Ok(o) => {
                    let stderr = String::from_utf8_lossy(&o.stderr);
                    tracing::warn!(
                        "failed to kill tmux session {}: {}",
                        session_name,
                        stderr.trim()
                    );
                }
                Err(e) => {
                    tracing::warn!("tmux error for {}: {}", session_name, e);
                }
            }
        }

        if let Some(ref container_name) = agent.docker {
            for runtime in &["docker", "podman"] {
                if command_available(runtime) {
                    if let Ok(o) = Command::new(runtime)
                        .args(["rm", "-f", container_name])
                        .output()
                    {
                        if o.status.success() {
                            result.container_removed = true;
                            if !json_output {
                                println!("  Removed {runtime} container: {container_name}");
                            }
                            break;
                        }
                    }
                }
            }
        }

        // With the agent's processes gone, release its locks and end its
        // session under its own identity, before the worktree goes. If a
        // release does not complete, the worktree (and the identity in it)
        // is kept so the lock can still be released.
        let worktree_path = Path::new(&agent.worktree);
        let worktree_present = !agent.worktree.is_empty() && worktree_path.exists();
        let repo_root = crosslink_dir.parent().unwrap_or(crosslink_dir);
        let mut ops = KickoffUndo {
            repo_root,
            worktree_dir: worktree_path,
            prior_pipeline: None,
        };
        let mut released = true;
        let mut branch_to_delete = None;
        if worktree_present {
            let held = worktree_agent_id(&worktree_path.join(".crosslink"))
                .map(|agent_id| host_locks_held_by(&repo_root.join(".crosslink"), &agent_id));
            released = release_agent_work(worktree_path, held, &mut ops, &mut result);
            if !keep_branch {
                branch_to_delete = branch_and_base(worktree_path);
            }
        }

        if !agent.worktree.is_empty() {
            if let Some(root) = crosslink_dir.parent() {
                let pipeline_status = match agent.status.as_str() {
                    "done" => "completed",
                    "failed" => "failed",
                    _ => "aborted",
                };
                let _ = super::pipeline::reconcile_completion_by_worktree(
                    root,
                    &agent.worktree,
                    pipeline_status,
                );
            }
        }

        if worktree_present {
            finish_worktree(
                repo_root,
                worktree_path,
                released,
                branch_to_delete,
                &mut ops,
                &mut result,
            );
            if result.worktree_removed && !json_output {
                let wt_display = worktree_path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(&agent.worktree);
                println!("  Removed worktree: {wt_display}");
            }
        }

        if !json_output {
            for issue in &result.locks_released {
                println!("  Released lock on #{issue}");
            }
            if result.branch_deleted {
                println!("  Deleted branch with no commits beyond its base");
            }
            for warning in &result.warnings {
                eprintln!("  Warning: {warning}");
            }
        }

        results.push(result);
    }

    if json_output {
        println!("{}", serde_json::to_string_pretty(&results)?);
    } else {
        let wt_removed = results.iter().filter(|r| r.worktree_removed).count();
        let tmux_killed = results.iter().filter(|r| r.tmux_killed).count();
        let containers_removed = results.iter().filter(|r| r.container_removed).count();
        let errors = results.iter().filter(|r| r.error.is_some()).count();

        println!();
        print!("Cleaned up {} agent(s)", results.len());
        if wt_removed > 0 {
            print!(": {wt_removed} worktree(s)");
        }
        if tmux_killed > 0 {
            print!(", {tmux_killed} tmux session(s)");
        }
        if containers_removed > 0 {
            print!(", {containers_removed} container(s)");
        }
        if errors > 0 {
            print!(" ({errors} error(s))");
        }
        println!(".");
    }

    let left_behind = results
        .iter()
        .filter(|r| r.error.is_some() || !r.warnings.is_empty())
        .count();
    if left_behind > 0 {
        anyhow::bail!(
            "cleanup left work behind for {left_behind} agent(s); see the warnings and errors above"
        );
    }
    Ok(())
}

/// Releases the agent's locks and ends its session under its own identity.
/// `held` is the host's view of the locks the worktree agent holds (`None`
/// when the worktree has no agent). Returns whether every lock was released;
/// anything left is a warning with its remedy.
fn release_agent_work(
    worktree: &Path,
    held: Option<anyhow::Result<Vec<i64>>>,
    ops: &mut impl UndoOps,
    result: &mut CleanupResult,
) -> bool {
    let held = match held {
        None => return true,
        Some(Ok(held)) => held,
        Some(Err(error)) => {
            result.warnings.push(format!(
                "could not read which locks the agent holds: {error:#}; {}",
                kept_worktree_remedy(worktree, None)
            ));
            return false;
        }
    };
    if held.is_empty() {
        return true;
    }
    let mut released = true;
    for issue_id in held {
        let step = UndoStep::ReleaseLock { issue_id };
        let outcome = ops.run(&step);
        if outcome == UndoOutcome::Done {
            result.locks_released.push(issue_id);
        } else {
            released &= outcome.is_clean();
            if let Some(message) = outcome_message(&step, &outcome) {
                result.warnings.push(message);
            }
        }
    }
    let step = UndoStep::EndSession;
    let outcome = ops.run(&step);
    if let Some(message) = outcome_message(&step, &outcome) {
        result.warnings.push(message);
    }
    released
}

/// Stops the worktree's daemon, then removes the worktree and deletes its
/// branch, unless a lock release did not complete: then both are kept, with
/// a remedy that names the kept path.
fn finish_worktree(
    repo_root: &Path,
    worktree: &Path,
    released: bool,
    branch_to_delete: Option<(String, String)>,
    ops: &mut impl UndoOps,
    result: &mut CleanupResult,
) {
    let step = UndoStep::StopDaemon;
    let outcome = ops.run(&step);
    if let Some(message) = outcome_message(&step, &outcome) {
        result.warnings.push(message);
    }
    if !released {
        result.worktree_kept = true;
        result.warnings.push(kept_worktree_remedy(worktree, None));
        return;
    }
    match Command::new("git")
        .current_dir(repo_root)
        .args(["worktree", "remove", "--force"])
        .arg(worktree)
        .output()
    {
        Ok(o) if o.status.success() => {
            result.worktree_removed = true;
            if let Some((name, base_commit)) = branch_to_delete {
                let step = UndoStep::DeleteBranch { name, base_commit };
                let outcome = ops.run(&step);
                if outcome == UndoOutcome::Done {
                    result.branch_deleted = true;
                } else if let Some(message) = outcome_message(&step, &outcome) {
                    result.warnings.push(message);
                }
            }
        }
        Ok(o) => {
            let msg = format!(
                "git worktree remove failed: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            );
            tracing::warn!("{}", msg);
            result.error = Some(msg);
        }
        Err(e) => {
            let msg = format!("git worktree remove error: {e}");
            tracing::warn!("{}", msg);
            result.error = Some(msg);
        }
    }
}

/// The worktree's branch and its recorded base, when kickoff recorded one.
fn branch_and_base(worktree: &Path) -> Option<(String, String)> {
    let metadata: KickoffMetadata =
        serde_json::from_slice(&std::fs::read(worktree.join(".kickoff-metadata.json")).ok()?)
            .ok()?;
    let base_commit = metadata.base_commit?;
    let branch = Command::new("git")
        .current_dir(worktree)
        .args(["branch", "--show-current"])
        .output()
        .ok()
        .filter(|output| output.status.success())?;
    let branch = String::from_utf8_lossy(&branch.stdout).trim().to_string();
    (!branch.is_empty()).then_some((branch, base_commit))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::kickoff::rollback::RecordingOps;

    fn git(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .expect("git runs");
        assert!(output.status.success(), "git {args:?}");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn write_metadata(worktree: &Path, base_commit: Option<&str>) {
        let metadata = serde_json::json!({
            "started_at": "2026-10-08T00:00:00Z",
            "timeout_secs": 3600,
            "base_commit": base_commit,
        });
        std::fs::write(
            worktree.join(".kickoff-metadata.json"),
            serde_json::to_vec(&metadata).expect("json"),
        )
        .expect("metadata");
    }

    /// AC-11 (cleanup): the branch rule needs a recorded base; metadata
    /// written before `base_commit` existed keeps the branch.
    #[test]
    fn cleanup_deletes_a_branch_only_with_a_recorded_base() {
        let dir = tempfile::tempdir().expect("tempdir");
        let worktree = dir.path();
        git(worktree, &["init", "-q", "-b", "feature/x"]);
        git(worktree, &["config", "user.email", "test@test.local"]);
        git(worktree, &["config", "user.name", "Test"]);
        git(worktree, &["config", "commit.gpgsign", "false"]);
        git(worktree, &["commit", "-q", "--allow-empty", "-m", "init"]);
        let head = git(worktree, &["rev-parse", "HEAD"]);

        write_metadata(worktree, Some(&head));
        assert_eq!(
            branch_and_base(worktree),
            Some(("feature/x".to_string(), head))
        );

        write_metadata(worktree, None);
        assert_eq!(branch_and_base(worktree), None);
    }

    #[test]
    fn cleanup_leaves_locks_alone_without_an_agent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut result = empty_result();
        let mut ops = RecordingOps::default();
        assert!(release_agent_work(dir.path(), None, &mut ops, &mut result));
        assert!(ops.calls.is_empty());
        assert!(result.locks_released.is_empty());
        assert!(result.warnings.is_empty());
    }

    fn empty_result() -> CleanupResult {
        CleanupResult {
            id: "agent".to_string(),
            class: CleanupClass::Stale,
            worktree_removed: false,
            tmux_killed: false,
            container_removed: false,
            locks_released: Vec::new(),
            branch_deleted: false,
            worktree_kept: false,
            warnings: Vec::new(),
            error: None,
        }
    }

    /// A repository with one commit and a worktree on a fresh branch.
    fn repo_with_worktree() -> (
        tempfile::TempDir,
        std::path::PathBuf,
        std::path::PathBuf,
        String,
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("repo");
        std::fs::create_dir_all(&root).expect("repo dir");
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.email", "test@test.local"]);
        git(&root, &["config", "user.name", "Test"]);
        git(&root, &["config", "commit.gpgsign", "false"]);
        git(&root, &["commit", "-q", "--allow-empty", "-m", "init"]);
        let worktree = dir.path().join("wt");
        git(
            &root,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature/x",
                worktree.to_str().expect("utf-8"),
                "HEAD",
            ],
        );
        let base = git(&worktree, &["rev-parse", "HEAD"]);
        (dir, root, worktree, base)
    }

    /// The field scenario on gh#110 (2026-10-09T20:43Z): the worktree's
    /// daemon never becomes ready, so the release fails. Cleanup stops the
    /// daemon but keeps the worktree and branch, and the remedy names a path
    /// that still exists.
    #[test]
    fn cleanup_keeps_the_worktree_when_a_release_does_not_complete() {
        let (_dir, root, worktree, base) = repo_with_worktree();
        let mut ops = RecordingOps {
            outcomes: vec![(
                UndoStep::ReleaseLock { issue_id: 7 },
                UndoOutcome::Failed {
                    error: "the worktree daemon did not become ready".to_string(),
                    remedy: "r".to_string(),
                },
            )],
            ..RecordingOps::default()
        };
        let mut result = empty_result();
        let released = release_agent_work(&worktree, Some(Ok(vec![7])), &mut ops, &mut result);
        assert!(!released);
        finish_worktree(
            &root,
            &worktree,
            released,
            Some(("feature/x".to_string(), base)),
            &mut ops,
            &mut result,
        );
        assert!(ops.calls.contains(&UndoStep::StopDaemon));
        assert!(worktree.exists(), "the worktree was removed");
        assert!(!git(&root, &["branch", "--list", "feature/x"]).is_empty());
        assert!(result.worktree_kept);
        assert!(!result.worktree_removed);
        let shown = worktree.display().to_string();
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.contains(&shown) && w.contains("kept")),
            "{:?}",
            result.warnings
        );
    }

    /// When every release completes, the worktree is removed and the
    /// unchanged branch deleted.
    #[test]
    fn cleanup_removes_the_worktree_when_releases_complete() {
        let (_dir, root, worktree, base) = repo_with_worktree();
        let mut ops = RecordingOps::default();
        let mut result = empty_result();
        let released = release_agent_work(&worktree, Some(Ok(vec![7])), &mut ops, &mut result);
        assert!(released);
        assert_eq!(result.locks_released, vec![7]);
        finish_worktree(
            &root,
            &worktree,
            released,
            Some(("feature/x".to_string(), base.clone())),
            &mut ops,
            &mut result,
        );
        assert!(!worktree.exists());
        assert!(result.worktree_removed);
        assert!(ops.calls.contains(&UndoStep::DeleteBranch {
            name: "feature/x".to_string(),
            base_commit: base,
        }));
    }
}
