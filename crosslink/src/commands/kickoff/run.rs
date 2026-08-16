use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

use crate::db::Database;
use crate::shared_writer::SharedWriter;

use super::helpers::*;
use super::launch::*;
use super::prompt::*;
use super::types::*;

pub fn run(
    crosslink_dir: &Path,
    db: &Database,
    writer: Option<&SharedWriter>,
    opts: &KickoffOpts,
) -> Result<String> {
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

    let local_writer = if writer.is_none() && !opts.dry_run {
        prepare_local_kickoff_writer(crosslink_dir, db)?
    } else {
        None
    };
    let writer = writer.or(local_writer.as_ref());

    let issue_id = if let Some(id) = opts.issue {
        if db.get_issue(id)?.is_none() {
            bail!("Issue {} not found", crate::utils::format_issue_id(id));
        }
        id
    } else {
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

    let (worktree_dir, branch_name) = if worktree_dir.exists() && opts.branch.is_some() {
        (worktree_dir, branch_name)
    } else {
        create_worktree(&root, &wt_slug, None)?
    };

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

    let agent_id =
        init_worktree_agent(&worktree_dir, crosslink_dir, &compact_name, Some(issue_id))?;

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

fn prepare_local_kickoff_writer(
    crosslink_dir: &Path,
    db: &Database,
) -> Result<Option<SharedWriter>> {
    if crate::identity::AgentConfig::load(crosslink_dir)?.is_none() {
        return Ok(None);
    }

    let sync = crate::sync::SyncManager::new(crosslink_dir)?;
    if sync.remote_exists() {
        return Ok(None);
    }
    if !sync.is_initialized() {
        sync.init_cache()
            .context("Failed to initialize the local coordination hub for kickoff")?;
    }
    if !sync.hub_mode().is_v3() {
        bail!("Local-only kickoff requires a v3 coordination hub");
    }

    crate::commands::migrate::promote_sqlite_to_v3(crosslink_dir, db, &sync)
        .context("Failed to share local issues before kickoff")?;
    SharedWriter::new(crosslink_dir)
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

        let writer = prepare_local_kickoff_writer(&crosslink_dir, &db)
            .unwrap()
            .expect("local kickoff writer");
        assert!(crate::sync::SyncManager::new(&crosslink_dir)
            .unwrap()
            .hub_mode()
            .is_v3());
        assert_eq!(
            db.get_issue(existing).unwrap().unwrap().title,
            "existing local issue"
        );

        let created = writer
            .create_issue(&db, "kickoff issue", None, "medium", None, None)
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
