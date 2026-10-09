use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

use crate::application::{CommandService, QueryService, RepositoryService};
use crate::db::Database;

use super::helpers::*;
use super::launch::*;
use super::prompt::*;
use super::rollback::{
    host_locks_held_by, worktree_agent_id, worktree_daemon_is_live, KickoffRollback, KickoffUndo,
    Origin,
};
use super::types::*;

pub(crate) fn resolve_kickoff_issue(
    service: &(impl CommandService + QueryService),
    requested_issue: Option<i64>,
    description: &str,
) -> Result<i64> {
    if let Some(id) = requested_issue {
        if service.get_issue(id)?.is_none() {
            bail!("Issue {} not found", crate::utils::format_issue_id(id));
        }
        Ok(id)
    } else {
        service.create_issue_with_labels(
            description,
            Some("Created by crosslink kickoff"),
            "medium",
            &["feature".to_string()],
            None,
            None,
        )
    }
}

pub fn run(crosslink_dir: &Path, db: &Database, opts: &KickoffOpts) -> Result<String> {
    let preflight = if opts.dry_run {
        None
    } else {
        Some(preflight_check(
            &opts.container,
            &opts.verify,
            crosslink_dir,
            Some(opts.image),
        )?)
    };

    let root = repo_root()?;
    let validation_agent = crate::agents::resolve_agent(crosslink_dir)?;
    let mut validation_conventions = detect_conventions(&root);
    validation_conventions
        .allowed_tools
        .extend(read_kickoff_allowed_tools(crosslink_dir));
    let validation_tools = build_allowed_tools(&validation_conventions, &opts.verify);
    validate_agent_request(
        &validation_agent,
        &root,
        opts.model,
        &validation_tools,
        &opts.policy,
    )?;
    let base_slug = slugify(opts.description);
    let slug = if base_slug.is_empty() {
        rand_hex_suffix()
    } else {
        format!("{}-{}", base_slug, rand_hex_suffix())
    };

    let repo_id = crate::commands::init::read_repo_compact_id(crosslink_dir);
    let agent_compact = crate::utils::generate_compact_id();
    let compact_name = crate::utils::compose_compact_name(&repo_id, &agent_compact, &slug);
    crate::utils::validate_compact_name(&compact_name)?;

    let service = if opts.dry_run {
        RepositoryService::projection(db)
    } else {
        RepositoryService::for_kickoff(db, crosslink_dir)?
    };

    let issue_id = if opts.dry_run {
        if let Some(id) = opts.issue {
            if service.get_issue(id)?.is_none() {
                bail!("Issue {} not found", crate::utils::format_issue_id(id));
            }
            id
        } else {
            0
        }
    } else {
        resolve_kickoff_issue(&service, opts.issue, opts.description)?
    };
    if !opts.dry_run && opts.issue.is_none() && !opts.quiet {
        println!("Created issue #{issue_id}");
    }

    let (wt_slug, branch_name) = opts.branch.map_or_else(
        || (compact_name.clone(), format!("feature/{compact_name}")),
        |br| {
            let wt_slug = br.strip_prefix("feature/").unwrap_or(br);
            (wt_slug.to_string(), br.to_string())
        },
    );
    let worktree_dir = root.join(".worktrees").join(&wt_slug);

    let mut conventions = validation_conventions;
    conventions
        .allowed_tools
        .extend(read_kickoff_allowed_tools(crosslink_dir));

    let prompt = if crate::utils::read_no_template(crosslink_dir) {
        String::new()
    } else {
        let built = build_prompt(opts, issue_id, &branch_name, &conventions);
        match crate::utils::resolve_kickoff_template(crosslink_dir, opts.template) {
            Some(template) => {
                let allowed_tools = conventions.allowed_tools.join(",");
                let ctx = TemplateContext {
                    built_prompt: &built,
                    issue_id,
                    branch: &branch_name,
                    description: opts.description,
                    model: opts.model,
                    effort: opts.policy.effort.as_deref(),
                    doc_path: opts.doc_path,
                    allowed_tools: &allowed_tools,
                };
                interpolate_template(&template, &ctx)
            }
            None => built,
        }
    };

    if opts.dry_run {
        println!("{prompt}");
        println!("---");
        println!("Worktree: {}", worktree_dir.display());
        println!("Branch:   {branch_name}");
        println!("Agent:    {compact_name}");
        return Ok(compact_name);
    }

    let reused = worktree_dir.exists() && opts.branch.is_some();
    let (worktree_dir, branch_name) = if reused {
        (worktree_dir, branch_name)
    } else {
        create_worktree(&root, &wt_slug, None)?
    };

    // Everything below is undone if it fails before the agent starts.
    let origin = if reused {
        Origin::Reused
    } else {
        Origin::Created
    };
    let base_commit = if reused {
        None
    } else {
        worktree_head(&worktree_dir)
    };
    let mut rollback = KickoffRollback::new();
    rollback.record_worktree(worktree_dir.clone(), origin);
    rollback.record_branch(branch_name.clone(), origin, base_commit.clone());

    let launched = (|| -> Result<String> {
        std::fs::write(worktree_dir.join(".kickoff-slug"), &compact_name)
            .context("Failed to write .kickoff-slug sentinel")?;

        std::fs::write(worktree_dir.join("KICKOFF.md"), &prompt)
            .context("Failed to write KICKOFF.md")?;

        if let Some(doc) = opts.design_doc {
            if !doc.acceptance_criteria.is_empty() {
                let source = opts.doc_path.unwrap_or("unknown");
                let criteria_file = extract_criteria(doc, source);
                let json = serde_json::to_string_pretty(&criteria_file)
                    .context("Failed to serialize criteria")?;
                std::fs::write(worktree_dir.join(".kickoff-criteria.json"), &json)
                    .context("Failed to write .kickoff-criteria.json")?;
            }
        }

        {
            let mut metadata = KickoffMetadata::for_launch(opts, chrono::Utc::now().to_rfc3339());
            metadata.base_commit.clone_from(&base_commit);
            metadata.provider = Some(validation_agent.provider.to_string());
            metadata.model = validation_agent.resolve_model(Some(opts.model));
            let json = serde_json::to_string_pretty(&metadata)
                .context("Failed to serialize kickoff metadata")?;
            std::fs::write(worktree_dir.join(".kickoff-metadata.json"), &json)
                .context("Failed to write .kickoff-metadata.json")?;
        }

        let protected_doc_rel = resolve_worktree_relative_doc(opts.doc_path, &root);
        if let Some(rel) = protected_doc_rel.as_deref() {
            protect_design_doc(&worktree_dir, rel)?;
        }

        exclude_kickoff_files(&worktree_dir)?;

        record_activation(
            &mut rollback,
            reused,
            crosslink_dir,
            &worktree_dir,
            issue_id,
        );
        let agent_id =
            init_worktree_agent(&worktree_dir, crosslink_dir, &compact_name, Some(issue_id))?;

        if let Some(doc_path_str) = opts.doc_path {
            let doc_path = Path::new(doc_path_str);
            rollback.record_pipeline(
                doc_path.to_path_buf(),
                super::pipeline::read_pipeline_state(doc_path),
            );
            if let Err(e) = super::pipeline::mark_running(
                doc_path,
                &agent_id,
                &worktree_dir.to_string_lossy(),
                Some(issue_id),
            ) {
                tracing::warn!("could not record pipeline run row for {doc_path_str}: {e}");
            }
        }

        let preflight = preflight.context("preflight check was skipped unexpectedly")?;

        let allowed_tools = build_allowed_tools(&conventions, &opts.verify);

        match &opts.container {
            ContainerMode::None => {
                let mut session_name = tmux_session_name(&compact_name);
                if tmux_session_exists(&session_name) {
                    let suffix: u32 = rand_suffix();
                    session_name =
                        format!("{}-{}", &session_name[..session_name.len().min(58)], suffix);
                }

                rollback.record_tmux_session(session_name.clone());
                launch_local(
                    &preflight.agent,
                    &worktree_dir,
                    &session_name,
                    opts.model,
                    &allowed_tools,
                    preflight.timeout_cmd,
                    preflight.sandbox_command.as_deref(),
                    crosslink_dir,
                    &opts.policy,
                )?;
                rollback.disarm();

                let _ = std::fs::write(worktree_dir.join(".kickoff-session"), &session_name);

                if opts.quiet {
                    println!("{session_name}");
                } else {
                    println!("Feature agent launched.");
                    println!();
                    println!("  Worktree: {}", worktree_dir.display());
                    println!("  Branch:   {branch_name}");
                    println!("  Issue:    #{issue_id}");
                    println!("  Agent:    {agent_id}");
                    println!("  Session:  {session_name}");
                    println!("  Verify:   {:?}", opts.verify);
                    println!();
                    println!("  Approve trust:  tmux attach -t {session_name}");
                    println!("  Check status:   crosslink kickoff status {agent_id}");
                    if opts.verify == VerifyLevel::Ci || opts.verify == VerifyLevel::Thorough {
                        println!();
                        println!("  CI verification is enabled. The agent will push and open a draft PR after local tests pass.");
                    }
                }
            }
            mode @ (ContainerMode::Docker | ContainerMode::Podman) => {
                let runtime = if *mode == ContainerMode::Docker {
                    "docker"
                } else {
                    "podman"
                };
                rollback
                    .record_container(runtime.to_string(), format!("crosslink-agent-{agent_id}"));
                let container_id = launch_container(
                    mode,
                    &preflight.agent,
                    &worktree_dir,
                    &root,
                    opts.image,
                    &agent_id,
                    opts.model,
                    &allowed_tools,
                    opts.timeout,
                    protected_doc_rel.as_deref(),
                    &opts.policy,
                )?;
                rollback.disarm();

                if opts.quiet {
                    println!("{container_id}");
                } else {
                    println!("Feature agent launched in container.");
                    println!();
                    println!("  Worktree:    {}", worktree_dir.display());
                    println!("  Branch:      {branch_name}");
                    println!("  Issue:       #{issue_id}");
                    println!("  Agent:       {agent_id}");
                    println!(
                        "  Container:   {}",
                        &container_id[..12.min(container_id.len())]
                    );
                    println!("  Verify:      {:?}", opts.verify);
                    println!();
                    println!(
                        "  View logs:   {} logs -f {}",
                        runtime,
                        &container_id[..12.min(container_id.len())]
                    );
                    println!("  Check status: crosslink kickoff status {agent_id}");
                }
            }
        }

        Ok(compact_name.clone())
    })();

    launched.map_err(|error| with_undo_report(error, rollback, &root, &worktree_dir))
}

/// Undoes what a failed kickoff created and returns the original error with
/// the undo report after it, so the error reads first and callers that only
/// log the error (swarm, sentinel) keep the report and its remedies.
pub(super) fn with_undo_report(
    error: anyhow::Error,
    rollback: KickoffRollback,
    root: &Path,
    worktree_dir: &Path,
) -> anyhow::Error {
    match undo_failed_kickoff(rollback, root, worktree_dir) {
        Some(report) => anyhow::anyhow!("{error:#}\n{report}"),
        None => error,
    }
}

/// Undoes what a failed kickoff created; returns the report, if any step ran.
pub(super) fn undo_failed_kickoff(
    rollback: KickoffRollback,
    root: &Path,
    worktree_dir: &Path,
) -> Option<String> {
    let mut ops = KickoffUndo {
        repo_root: root,
        worktree_dir,
        prior_pipeline: rollback.prior_pipeline().cloned(),
    };
    rollback.unwind(&mut ops).render()
}

/// Records the agent, daemon and claim that activation is about to set up.
/// On a reused worktree, whatever already existed (the agent and its session,
/// a running daemon, a lock the agent already held) is recorded as such, so
/// undo leaves it as found.
pub(super) fn record_activation(
    rollback: &mut KickoffRollback,
    reused: bool,
    host_crosslink: &Path,
    worktree_dir: &Path,
    issue_id: i64,
) {
    let worktree_crosslink = worktree_dir.join(".crosslink");
    let existing_agent = if reused {
        worktree_agent_id(&worktree_crosslink)
    } else {
        None
    };
    let Some(agent_id) = existing_agent else {
        rollback.record_agent();
        rollback.record_claim(issue_id);
        return;
    };
    rollback.record_existing_agent(worktree_daemon_is_live(&worktree_crosslink));
    let already_held =
        host_locks_held_by(host_crosslink, &agent_id).is_ok_and(|held| held.contains(&issue_id));
    if already_held {
        rollback.record_existing_claim(issue_id);
    } else {
        rollback.record_claim(issue_id);
    }
}

/// The commit a freshly created worktree's branch points at.
pub(super) fn worktree_head(worktree_dir: &Path) -> Option<String> {
    std::process::Command::new("git")
        .current_dir(worktree_dir)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn resolve_worktree_relative_doc(doc_path: Option<&str>, repo_root: &Path) -> Option<PathBuf> {
    let raw = doc_path?;
    let candidate = Path::new(raw);
    let absolute = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(candidate)
    };
    let canonical = absolute.canonicalize().ok()?;
    let canonical_root = repo_root.canonicalize().ok()?;
    canonical
        .strip_prefix(&canonical_root)
        .ok()
        .map(Path::to_path_buf)
}

fn protect_design_doc(worktree_dir: &Path, rel: &Path) -> Result<()> {
    let worktree_doc = worktree_dir.join(rel);
    if !worktree_doc.is_file() {
        return Ok(());
    }

    let content = std::fs::read_to_string(&worktree_doc)
        .with_context(|| format!("Failed to read design doc at {}", worktree_doc.display()))?;
    let doc_hash = super::pipeline::compute_doc_hash(&content);

    let breadcrumb = KickoffDocBreadcrumb {
        rel_path: rel.to_string_lossy().into_owned(),
        doc_hash,
    };
    let json = serde_json::to_string_pretty(&breadcrumb)
        .context("Failed to serialize kickoff doc breadcrumb")?;
    std::fs::write(worktree_dir.join(".kickoff-doc.json"), json)
        .context("Failed to write .kickoff-doc.json")?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&worktree_doc, std::fs::Permissions::from_mode(0o444));
    }

    Ok(())
}

#[cfg(test)]
mod local_kickoff_tests {
    use super::*;
    use std::process::Command;

    fn git(path: &Path, args: &[&str]) {
        let output = Command::new("git")
            .current_dir(path)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn local_kickoff_bootstraps_hub_and_promotes_existing_issue() {
        let repo = tempfile::tempdir().unwrap();
        git(repo.path(), &["init", "-b", "main"]);
        git(repo.path(), &["config", "user.email", "test@example.com"]);
        git(repo.path(), &["config", "user.name", "Test"]);
        git(repo.path(), &["commit", "--allow-empty", "-m", "init"]);

        let crosslink_dir = repo.path().join(".crosslink");
        std::fs::create_dir_all(&crosslink_dir).unwrap();
        crate::identity::AgentConfig::init(&crosslink_dir, "local-driver", None).unwrap();
        let db = Database::open(&crosslink_dir.join("issues.db")).unwrap();
        let existing = db
            .create_issue("existing local issue", None, "medium")
            .unwrap();

        let service = RepositoryService::for_kickoff(&db, &crosslink_dir).unwrap();
        assert!(crate::sync::SyncManager::new(&crosslink_dir)
            .unwrap()
            .hub_mode()
            .is_v3());
        assert_eq!(
            db.get_issue(existing).unwrap().unwrap().title,
            "existing local issue"
        );

        let created = service
            .create_issue("kickoff issue", None, "medium", None, None)
            .unwrap();
        assert_eq!(
            db.get_issue(created).unwrap().unwrap().title,
            "kickoff issue"
        );

        let state = crate::compaction::reduce(
            &crate::hub_source::RefHubSource::new(
                crate::sync::SyncManager::new(&crosslink_dir)
                    .unwrap()
                    .cache_path(),
            )
            .unwrap(),
        )
        .unwrap()
        .state;
        let titles: std::collections::HashSet<&str> = state
            .issues
            .values()
            .map(|issue| issue.title.as_str())
            .collect();
        assert!(titles.contains("existing local issue"));
        assert!(titles.contains("kickoff issue"));
    }
}
