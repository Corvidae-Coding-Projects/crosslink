use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::Command;

use crate::application::QueryService;
use crate::identity::AgentConfig;

use super::helpers::*;
use super::launch::*;
use super::prompt::{interpolate_template, TemplateContext};
use super::types::*;

pub(crate) fn build_allowed_tools_plan() -> String {
    let tools = vec![
        "Read",
        "Glob",
        "Grep",
        "WebSearch",
        "WebFetch",
        "Bash(git status *)",
        "Bash(git log *)",
        "Bash(git diff *)",
        "Bash(git show *)",
        "Bash(git branch *)",
        "Bash(ls *)",
        "Bash(cat *)",
        "Bash(head *)",
        "Bash(tail *)",
        "Bash(wc *)",
        "Bash(crosslink *)",
    ];
    tools.join(",")
}

pub(crate) fn build_plan_prompt(
    doc: &super::super::design_doc::DesignDoc,
    issue_id: Option<i64>,
    plan_copy_target: Option<&std::path::Path>,
) -> String {
    let issue_line = issue_id.map_or_else(String::new, |id| format!("- **Issue**: #{id}\n"));

    let mut prompt = format!(
        "# KICKOFF PLAN: Gap Analysis — {}\n\n\
         ## Context\n\n\
         {}- **Mode**: Read-only analysis (no code changes)\n\n",
        doc.title, issue_line,
    );

    prompt.push_str(&super::super::design_doc::build_design_doc_section(doc));

    if let Some(escalation) = super::super::design_doc::build_open_questions_escalation(doc) {
        prompt.push_str(&escalation);
    }

    prompt.push_str(
        r#"
## Analysis Instructions

You are in **read-only analysis mode**. Do NOT write or edit any code files. Your task is to
analyze the design document above against the existing codebase and produce a structured gap report.

### Steps

1. **Explore the codebase** — find files, patterns, and existing implementations relevant to
   each requirement in the design document.
2. **Assess each requirement** — for each one, determine:
   - Is it feasible with the current codebase?
   - What existing code supports or conflicts with it?
   - What information is missing?
3. **Address open questions** — attempt to answer each from codebase context (existing patterns,
   conventions, prior art).
4. **Identify conflicts** — flag any existing code that contradicts or complicates requirements.
5. **Estimate subtasks** — break the implementation into estimated subtasks with scope and risk.
6. **Write the gap report** — produce `.kickoff-plan.json` in the current directory.

### Output Format

Write a JSON file `.kickoff-plan.json` with exactly this structure:

```json
{
  "gaps": [
    {
      "section": "Requirements|Acceptance Criteria|Architecture|...",
      "item": "REQ-1 or null",
      "severity": "blocking|advisory",
      "detail": "description of the gap"
    }
  ],
  "assumptions": [
    {
      "about": "what this assumption relates to",
      "assumption": "what we're assuming"
    }
  ],
  "estimated_subtasks": [
    {
      "title": "subtask title",
      "scope": "~200 lines",
      "risk": "low|medium|high"
    }
  ],
  "conflicts": [
    {
      "file": "src/path/to/file.rs",
      "detail": "description of the conflict"
    }
  ]
}
```

### Final Steps

1. Write `.kickoff-plan.json` (valid JSON only)
"#,
    );

    if let Some(target) = plan_copy_target {
        use std::fmt::Write as _;
        let _ = writeln!(
            prompt,
            "2. Copy `.kickoff-plan.json` to `{}` so the plan is discoverable alongside the design doc",
            target.display()
        );
        prompt.push_str("3. Write the word `DONE` to `.kickoff-status`\n");
    } else {
        prompt.push_str("2. Write the word `DONE` to `.kickoff-status`\n");
    }

    prompt
}

pub fn plan(crosslink_dir: &Path, db: &impl QueryService, opts: &PlanOpts) -> Result<()> {
    if cfg!(target_os = "windows") && !opts.dry_run {
        bail!(
            "Plan mode requires tmux, which is not available on Windows.\n\
             Use `--container docker` for agent kickoff on Windows."
        );
    }

    let preflight = if opts.dry_run {
        None
    } else {
        Some(preflight_check(
            &ContainerMode::None,
            &VerifyLevel::Local,
            crosslink_dir,
        )?)
    };

    let root = repo_root()?;
    let validation_agent = crate::agents::resolve_agent(crosslink_dir)?;
    let validation_tools = build_allowed_tools_plan();
    validate_agent_request(
        &validation_agent,
        &root,
        opts.model,
        &validation_tools,
        &opts.policy,
    )?;
    let title_slug = if opts.doc.title.is_empty() {
        "analysis".to_string()
    } else {
        slugify(&opts.doc.title)
    };
    let slug = format!("plan-{}-{}", title_slug, rand_hex_suffix());

    let issue_id = if let Some(id) = opts.issue {
        if db.get_issue(id)?.is_none() {
            bail!("Issue {} not found", crate::utils::format_issue_id(id));
        }
        Some(id)
    } else {
        None
    };

    let plan_copy_target = opts.doc_path.map(super::pipeline::plan_path_for_doc);
    let built = build_plan_prompt(opts.doc, issue_id, plan_copy_target.as_deref());
    let prompt = match crate::utils::resolve_kickoff_template(crosslink_dir, opts.template) {
        Some(template) => {
            let ctx = TemplateContext {
                built_prompt: &built,
                issue_id: issue_id.unwrap_or(0),
                branch: "",
                description: "",
                model: opts.model,
                effort: opts.policy.effort.as_deref(),
                doc_path: opts.doc_path.and_then(|p| p.to_str()),
                allowed_tools: "",
            };
            interpolate_template(&template, &ctx)
        }
        None => built,
    };

    if opts.dry_run {
        let parent_id =
            AgentConfig::load(crosslink_dir)?.map_or_else(|| "driver".to_string(), |c| c.agent_id);
        let agent_id = format!("{parent_id}--{slug}");
        println!("{prompt}");
        println!("---");
        println!(
            "Worktree: {}",
            root.join(".worktrees").join(&slug).display()
        );
        println!("Branch:   feature/{slug}");
        println!("Agent:    {agent_id}");
        return Ok(());
    }

    let (worktree_dir, branch_name) = create_worktree(&root, &slug, None)?;

    std::fs::write(worktree_dir.join(".kickoff-slug"), &slug)
        .context("Failed to write .kickoff-slug sentinel")?;

    std::fs::write(worktree_dir.join("PLAN_KICKOFF.md"), &prompt)
        .context("Failed to write PLAN_KICKOFF.md")?;
    std::fs::create_dir_all(worktree_dir.join(".crosslink/runtime"))
        .context("Failed to create provider runtime log directory")?;
    let metadata = KickoffMetadata {
        started_at: chrono::Utc::now().to_rfc3339(),
        timeout_secs: opts.timeout.as_secs(),
        provider: Some(validation_agent.provider.to_string()),
        model: validation_agent.resolve_model(Some(opts.model)),
        effort: opts.policy.effort.clone(),
        budget_usd: opts.policy.monetary_budget_usd.clone(),
    };
    std::fs::write(
        worktree_dir.join(".kickoff-metadata.json"),
        serde_json::to_vec_pretty(&metadata)?,
    )
    .context("Failed to write plan provider metadata")?;

    exclude_kickoff_files(&worktree_dir)?;

    if let Some(doc_path) = opts.doc_path {
        let _ = super::pipeline::mark_planning(
            doc_path,
            &format!("driver--{slug}"),
            &worktree_dir.to_string_lossy(),
        );
    }

    let agent_id = init_worktree_agent(&worktree_dir, crosslink_dir, &slug, issue_id)?;

    let preflight = preflight.context("preflight check was skipped unexpectedly")?;

    let allowed_tools = build_allowed_tools_plan();
    let mut session_name = tmux_session_name(&slug);
    if tmux_session_exists(&session_name) {
        let suffix = rand_suffix();
        session_name = format!("{}-{}", &session_name[..session_name.len().min(44)], suffix);
    }

    let cmd = build_resolved_agent_command(
        &preflight.agent,
        preflight.timeout_cmd,
        opts.model,
        &allowed_tools,
        "PLAN_KICKOFF.md",
        preflight.sandbox_command.as_deref(),
        &worktree_dir,
        &opts.policy,
    )?;

    let output = Command::new("tmux")
        .args([
            "new-session",
            "-d",
            "-s",
            &session_name,
            "-c",
            &worktree_dir.to_string_lossy(),
        ])
        .output()
        .context("Failed to create tmux session")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("Failed to create tmux session: {}", stderr.trim());
    }

    let output = Command::new("tmux")
        .args(["send-keys", "-t", &session_name, &cmd, "Enter"])
        .output()
        .context("Failed to send command to tmux session")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("Failed to send keys to tmux: {}", stderr.trim());
    }

    let _ = std::fs::write(worktree_dir.join(".kickoff-session"), &session_name);

    let watchdog_cfg = read_watchdog_config(crosslink_dir);
    if watchdog_cfg.enabled {
        if let Err(e) = spawn_watchdog(&session_name, &worktree_dir, &watchdog_cfg) {
            tracing::warn!("failed to spawn watchdog: {}", e);
        }
    }

    if opts.quiet {
        println!("{session_name}");
    } else {
        println!("Plan analysis agent launched (read-only mode).");
        println!();
        println!("  Worktree: {}", worktree_dir.display());
        println!("  Branch:   {branch_name}");
        if let Some(id) = issue_id {
            println!("  Issue:    #{id}");
        }
        println!("  Agent:    {agent_id}");
        println!("  Session:  {session_name}");
        println!();
        println!("  Approve trust:  tmux attach -t {session_name}");
        println!("  Check status:   crosslink kickoff status {agent_id}");
        println!("  View report:    crosslink kickoff show-plan {agent_id}");
    }

    Ok(())
}

pub fn show_plan(crosslink_dir: &Path, agent: &str) -> Result<()> {
    let root = crosslink_dir
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Cannot determine repo root"))?;

    let slug = agent
        .strip_prefix("feature/")
        .or_else(|| agent.strip_prefix("feat-"))
        .unwrap_or(agent);
    let wt_slug = slug.rsplit("--").next().unwrap_or(slug);

    let worktree_dir = root.join(".worktrees").join(wt_slug);
    if !worktree_dir.exists() {
        bail!(
            "No worktree found for '{}'. Checked: {}",
            agent,
            worktree_dir.display()
        );
    }

    let plan_file = worktree_dir.join(".kickoff-plan.json");
    if !plan_file.exists() {
        let status_file = worktree_dir.join(".kickoff-status");
        let status = if status_file.exists() {
            std::fs::read_to_string(&status_file)
                .unwrap_or_default()
                .trim()
                .to_string()
        } else {
            "still running".to_string()
        };
        bail!("No gap report found yet for '{agent}'. Agent status: {status}");
    }

    let content =
        std::fs::read_to_string(&plan_file).context("Failed to read .kickoff-plan.json")?;

    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&content) {
        println!(
            "{}",
            serde_json::to_string_pretty(&parsed).unwrap_or(content)
        );
    } else {
        print!("{content}");
    }

    Ok(())
}
