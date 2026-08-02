// E-ana tablet — kickoff run: main entry point for `crosslink kickoff run`
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

use crate::db::Database;
use crate::shared_writer::SharedWriter;

use super::helpers::*;
use super::launch::*;
use super::prompt::*;
use super::types::*;

/// Main entry point: `crosslink kickoff run`.
///
/// Returns the compact name (`<repo>-<agent>-<slug>`) used as the agent ID,
/// branch suffix, and tmux session name.
pub fn run(
    crosslink_dir: &Path,
    db: &Database,
    writer: Option<&SharedWriter>,
    opts: &KickoffOpts,
) -> Result<String> {
    // 1. Pre-flight: validate all required external commands are present
    let preflight = if opts.dry_run {
        None
    } else {
        Some(preflight_check(
            &opts.container,
            &opts.verify,
            crosslink_dir,
        )?)
    };

    let root = repo_root()?;
    let base_slug = slugify(opts.description);
    let slug = if base_slug.is_empty() {
        rand_hex_suffix()
    } else {
        format!("{}-{}", base_slug, rand_hex_suffix())
    };

    // Generate compact identifiers for structured naming
    let repo_id = crate::commands::init::read_repo_compact_id(crosslink_dir);
    let agent_compact = crate::utils::generate_compact_id();
    let compact_name = crate::utils::compose_compact_name(&repo_id, &agent_compact, &slug);
    crate::utils::validate_compact_name(&compact_name)?;

    // 2. Create or find the issue
    let issue_id = if let Some(id) = opts.issue {
        // Verify the issue exists
        if db.get_issue(id)?.is_none() {
            bail!("Issue {} not found", crate::utils::format_issue_id(id));
        }
        id
    } else {
        // Create a new issue directly
        let id = if let Some(w) = writer {
            w.create_issue(
                db,
                opts.description,
                Some("Created by crosslink kickoff"),
                "medium",
                None,
                None,
            )?
        } else {
            db.create_issue(
                opts.description,
                Some("Created by crosslink kickoff"),
                "medium",
            )?
        };
        let label_err = writer.map_or_else(
            || db.add_label(id, "feature").err(),
            |w| w.add_label(db, id, "feature").err(),
        );
        if let Some(e) = label_err {
            tracing::warn!("could not label issue #{id} with 'feature': {e}");
        }
        if !opts.quiet {
            println!("Created issue #{id}");
        }
        id
    };

    // 3. Resolve the worktree slug and branch name up front. Creation is
    //    deferred until after the dry-run gate below so a dry run is
    //    side-effect-free (gh#19) — these are the exact names
    //    create_worktree derives.
    let (wt_slug, branch_name) = opts.branch.map_or_else(
        || (compact_name.clone(), format!("feature/{compact_name}")),
        |br| {
            let wt_slug = br.strip_prefix("feature/").unwrap_or(br);
            (wt_slug.to_string(), br.to_string())
        },
    );
    let worktree_dir = root.join(".worktrees").join(&wt_slug);

    // 4. Detect project conventions, then extend with any explicit additions
    //    from `hook-config.json`'s `kickoff.allowed_tools` array so projects
    //    can teach the kickoff agent about tools detection doesn't pick up
    //    automatically. See GH#584.
    let mut conventions = detect_conventions(&root);
    conventions
        .allowed_tools
        .extend(read_kickoff_allowed_tools(crosslink_dir));

    // 5. Build the prompt — check for custom template or no_template first
    let prompt = if crate::utils::read_no_template(crosslink_dir) {
        String::new()
    } else if let Some(custom) = crate::utils::read_kickoff_template(crosslink_dir) {
        custom
    } else {
        build_prompt(opts, issue_id, &branch_name, &conventions)
    };

    // Dry run: print the prompt and the would-be worktree/branch/agent, then
    // exit BEFORE creating the worktree, branch, or any sentinel file (gh#19).
    if opts.dry_run {
        println!("{prompt}");
        println!("---");
        println!("Worktree: {}", worktree_dir.display());
        println!("Branch:   {branch_name}");
        println!("Agent:    {compact_name}");
        return Ok(compact_name);
    }

    // 6. Create worktree and feature branch (or reuse the existing worktree
    //    when --branch points at one), then write the kickoff files into it.
    let (worktree_dir, branch_name) = if worktree_dir.exists() && opts.branch.is_some() {
        (worktree_dir, branch_name)
    } else {
        create_worktree(&root, &wt_slug, None)?
    };

    // Write slug sentinel so other commands can identify this worktree
    std::fs::write(worktree_dir.join(".kickoff-slug"), &compact_name)
        .context("Failed to write .kickoff-slug sentinel")?;

    // 6a. Write KICKOFF.md to worktree
    std::fs::write(worktree_dir.join("KICKOFF.md"), &prompt)
        .context("Failed to write KICKOFF.md")?;

    // 6b. Extract and write criteria if design doc has acceptance criteria
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

    // 6c. Write launch metadata (timeout + start time) for status tracking,
    //     plus the dispatch dials (model/effort/budget) so the run record
    //     captures what reasoning and spend posture the agent was given
    //     (gh#61). Dials absent from the launch stay absent from the JSON.
    {
        let metadata = KickoffMetadata::for_launch(opts, chrono::Utc::now().to_rfc3339());
        let json = serde_json::to_string_pretty(&metadata)
            .context("Failed to serialize kickoff metadata")?;
        std::fs::write(worktree_dir.join(".kickoff-metadata.json"), &json)
            .context("Failed to write .kickoff-metadata.json")?;
    }

    // 6d. Protect the canonical design doc passed via `--doc` from agent edits.
    //     Writes a `.kickoff-doc.json` breadcrumb (consumed by post-run
    //     validation in monitor::report) and applies chmod 0444 so even
    //     non-container kickoffs flag accidental rewrites. The container
    //     mode adds a read-only overlay mount on top. See GH#580.
    let protected_doc_rel = resolve_worktree_relative_doc(opts.doc_path, &root);
    if let Some(rel) = protected_doc_rel.as_deref() {
        protect_design_doc(&worktree_dir, rel)?;
    }

    // 7. Exclude kickoff files from git
    exclude_kickoff_files(&worktree_dir)?;

    // 8. Initialize crosslink + agent in worktree (only for real launches)
    let agent_id = init_worktree_agent(&worktree_dir, crosslink_dir, &compact_name)?;

    // 8b. Record the pipeline run row now that the worktree and agent identity
    //     both exist — this is past the launch's point of no return, so the row
    //     carries the real agent_id and worktree path instead of the legacy
    //     "pending" placeholders (GH#614). Best-effort: a pipeline-state write
    //     failure must not abort an otherwise-successful launch.
    if let Some(doc_path_str) = opts.doc_path {
        let doc_path = Path::new(doc_path_str);
        if let Err(e) = super::pipeline::mark_running(
            doc_path,
            &agent_id,
            &worktree_dir.to_string_lossy(),
            Some(issue_id),
        ) {
            tracing::warn!("could not record pipeline run row for {doc_path_str}: {e}");
        }
    }

    // preflight is guaranteed Some after the dry-run early return above
    let preflight = preflight.context("preflight check was skipped unexpectedly")?;

    // 9. Launch the agent
    let allowed_tools = build_allowed_tools(&conventions, &opts.verify);

    match &opts.container {
        ContainerMode::None => {
            let mut session_name = tmux_session_name(&compact_name);
            if tmux_session_exists(&session_name) {
                // Append random suffix
                let suffix: u32 = rand_suffix();
                session_name =
                    format!("{}-{}", &session_name[..session_name.len().min(58)], suffix);
            }

            launch_local(
                &opts.agent_binary,
                &worktree_dir,
                &session_name,
                opts.model,
                &allowed_tools,
                opts.timeout,
                preflight.timeout_cmd,
                preflight.sandbox_command.as_deref(),
                crosslink_dir,
                opts.skip_permissions,
                opts.permission_mode,
                opts.effort,
                opts.budget_usd,
            )?;

            // Persist the actual session name so kickoff list can find it
            let _ = std::fs::write(worktree_dir.join(".kickoff-session"), &session_name);

            // 10. Report
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
            let container_id = launch_container(
                mode,
                &opts.agent_binary,
                &worktree_dir,
                &root,
                opts.image,
                &agent_id,
                opts.model,
                &allowed_tools,
                opts.timeout,
                protected_doc_rel.as_deref(),
                opts.skip_permissions,
                opts.permission_mode,
                opts.effort,
                opts.budget_usd,
            )?;

            if opts.quiet {
                println!("{container_id}");
            } else {
                let runtime = if *mode == ContainerMode::Docker {
                    "docker"
                } else {
                    "podman"
                };
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

    Ok(compact_name)
}

/// Resolve a `--doc <path>` CLI argument to a path relative to the repo root.
///
/// Returns `None` when the doc lies outside the repo or cannot be canonicalized
/// (e.g. the user passed a path that doesn't exist on disk yet). The container
/// `:ro` mount and the breadcrumb both need the worktree-relative form because
/// the worktree mirrors the repo's directory structure.
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

/// Stage the design doc as a protected canonical input inside the worktree.
///
/// Writes `.kickoff-doc.json` (so post-run validation can detect drift) and
/// applies chmod 0444 to the doc itself. Both steps are best-effort: if the
/// worktree doesn't carry the doc yet — e.g. fresh design that wasn't
/// committed — there's nothing to protect and we return Ok(()).
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

    // chmod 0444 is advisory — a determined agent can flip it back — but it
    // pairs with the KICKOFF.md instruction and the post-run hash check to
    // make accidental rewrites loud rather than silent.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&worktree_doc, std::fs::Permissions::from_mode(0o444));
    }

    Ok(())
}
