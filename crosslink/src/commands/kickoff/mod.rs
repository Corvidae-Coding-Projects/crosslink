mod cleanup;
mod graph;
mod helpers;
mod launch;
mod monitor;
pub(crate) mod pipeline;
mod plan;
mod prompt;
mod run;
mod types;
mod wizard;

#[cfg(test)]
mod tests;

pub use types::{
    ContainerMode, KickoffOpts, KickoffReport, PlanOpts, ReportFormat, VerifyLevel,
    DEFAULT_AGENT_IMAGE, EFFORT_LEVELS,
};

pub use types::{parse_container_mode, parse_duration, parse_verify_level};

pub use cleanup::cleanup;
pub use graph::graph;
pub use monitor::{list, logs, report, report_all, status, stop};
pub use plan::{plan, show_plan};
pub use run::run;

pub(crate) use helpers::{
    command_available, detect_conventions, format_duration, slugify, tmux_session_name,
};
pub(crate) use launch::resolve_execution_policy;

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

use crate::db::Database;
use crate::shared_writer::SharedWriter;
use crate::KickoffCommands;

pub fn dispatch(
    command: KickoffCommands,
    crosslink_dir: &Path,
    db: &Database,
    writer: Option<&SharedWriter>,
    quiet: bool,
    json: bool,
) -> Result<()> {
    match command {
        KickoffCommands::Run {
            description,
            issue,
            container,
            verify,
            model,
            image,
            timeout,
            dry_run,
            branch,
            doc,
            skip_permissions,
            permission_mode,
            effort,
            budget_usd,
            template,
        } => {
            let parsed_doc = if let Some(ref path) = doc {
                let content = std::fs::read_to_string(path)
                    .with_context(|| format!("Failed to read design doc: {}", path.display()))?;
                let d = super::design_doc::parse_design_doc(&content);
                for warning in super::design_doc::validate_design_doc(&d) {
                    tracing::warn!("{}", warning);
                }
                Some(d)
            } else {
                None
            };
            let parsed_container = parse_container_mode(&container)?;
            let parsed_timeout = parse_duration(&timeout)?;
            let agent = crate::agents::resolve_agent(crosslink_dir)?;
            let policy = resolve_execution_policy(
                &agent,
                permission_mode.as_deref(),
                skip_permissions,
                effort.as_deref(),
                budget_usd.as_deref(),
                parsed_timeout,
                (parsed_container != ContainerMode::None)
                    .then_some(crate::agents::SandboxPosture::ExternalIsolation),
            )?;
            let opts = KickoffOpts {
                description: &description,
                issue,
                container: parsed_container,
                verify: parse_verify_level(&verify)?,
                model: &model,
                image: &image,
                timeout: parsed_timeout,
                dry_run,
                branch: branch.as_deref(),
                quiet,
                design_doc: parsed_doc.as_ref(),
                doc_path: doc.as_ref().map(|p| p.to_str().unwrap_or("unknown")),
                policy,
                template: template.as_deref(),
            };

            run(crosslink_dir, db, writer, &opts)?;
            Ok(())
        }
        KickoffCommands::Status { agent } => agent.as_ref().map_or_else(
            || pipeline_status_overview(crosslink_dir, json),
            |id| status(crosslink_dir, id),
        ),
        KickoffCommands::Logs { agent, lines } => logs(crosslink_dir, &agent, lines),
        KickoffCommands::Stop { agent, force } => stop(crosslink_dir, &agent, force),
        KickoffCommands::Plan {
            doc,
            issue,
            model,
            timeout,
            dry_run,
            skip_permissions,
            permission_mode,
            effort,
            budget_usd,
            template,
        } => {
            let content = std::fs::read_to_string(&doc)
                .with_context(|| format!("Failed to read design doc: {}", doc.display()))?;
            let design_doc = super::design_doc::parse_design_doc(&content);
            for warning in super::design_doc::validate_design_doc(&design_doc) {
                tracing::warn!("{}", warning);
            }
            let parsed_timeout = parse_duration(&timeout)?;
            let agent = crate::agents::resolve_agent(crosslink_dir)?;
            let policy = resolve_execution_policy(
                &agent,
                permission_mode.as_deref(),
                skip_permissions,
                effort.as_deref(),
                budget_usd.as_deref(),
                parsed_timeout,
                Some(crate::agents::SandboxPosture::ReadOnly),
            )?;
            let plan_opts = PlanOpts {
                doc: &design_doc,
                doc_path: Some(&doc),
                model: &model,
                timeout: parsed_timeout,
                dry_run,
                issue,
                quiet,
                policy,
                template: template.as_deref(),
            };
            plan(crosslink_dir, db, &plan_opts)
        }
        KickoffCommands::ShowPlan { agent } => show_plan(crosslink_dir, &agent),
        KickoffCommands::Report {
            agent,
            json: report_json,
            markdown,
            all,
        } => {
            let format = if report_json {
                ReportFormat::Json
            } else if markdown {
                ReportFormat::Markdown
            } else {
                ReportFormat::Table
            };
            if all {
                report_all(crosslink_dir, format)
            } else {
                let agent =
                    agent.ok_or_else(|| anyhow::anyhow!("Agent ID required (or use --all)"))?;
                report(crosslink_dir, &agent, format)
            }
        }
        KickoffCommands::List { status } => list(crosslink_dir, &status, json, quiet),
        KickoffCommands::Graph { all, no_pager: _ } => graph(crosslink_dir, all, json, quiet),
        KickoffCommands::Cleanup {
            dry_run,
            force,
            keep,
            json: cleanup_json,
        } => cleanup(crosslink_dir, dry_run, force, keep, cleanup_json),
        KickoffCommands::Launch {
            doc,
            plan: do_plan,
            run: do_run,
            verify,
            model,
            timeout,
            container,
            issue,
            dry_run,
            skip_permissions,
            permission_mode,
        } => dispatch_launch(
            crosslink_dir,
            db,
            writer,
            quiet,
            json,
            doc,
            do_plan,
            do_run,
            &verify,
            &model,
            &timeout,
            &container,
            issue,
            dry_run,
            skip_permissions,
            permission_mode.as_deref(),
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn dispatch_launch(
    crosslink_dir: &Path,
    db: &Database,
    writer: Option<&SharedWriter>,
    quiet: bool,
    _json: bool,
    doc: Option<PathBuf>,
    do_plan: bool,
    do_run: bool,
    verify: &str,
    model: &str,
    timeout: &str,
    container: &str,
    issue: Option<i64>,
    dry_run: bool,
    skip_permissions: bool,
    permission_mode: Option<&str>,
) -> Result<()> {
    if do_plan {
        let doc_path = doc.ok_or_else(|| {
            anyhow::anyhow!(
                "--plan requires a design document path: crosslink kickoff .design/foo.md --plan"
            )
        })?;
        let content = std::fs::read_to_string(&doc_path)
            .with_context(|| format!("Failed to read design doc: {}", doc_path.display()))?;
        let design_doc = super::design_doc::parse_design_doc(&content);
        for warning in super::design_doc::validate_design_doc(&design_doc) {
            eprintln!("Warning: {warning}");
        }
        let parsed_timeout = parse_duration(timeout)?;
        let agent = crate::agents::resolve_agent(crosslink_dir)?;
        let policy = resolve_execution_policy(
            &agent,
            permission_mode,
            skip_permissions,
            None,
            None,
            parsed_timeout,
            Some(crate::agents::SandboxPosture::ReadOnly),
        )?;
        let plan_opts = PlanOpts {
            doc: &design_doc,
            doc_path: Some(&doc_path),
            model,
            timeout: parsed_timeout,
            dry_run,
            issue,
            quiet,
            policy,
            template: None,
        };
        return plan(crosslink_dir, db, &plan_opts);
    }

    if do_run {
        let Some(ref doc_path) = doc else {
            bail!("--run requires a design document or description");
        };

        let content = std::fs::read_to_string(doc_path)
            .with_context(|| format!("Failed to read design doc: {}", doc_path.display()))?;
        let parsed = super::design_doc::parse_design_doc(&content);
        for warning in super::design_doc::validate_design_doc(&parsed) {
            eprintln!("Warning: {warning}");
        }

        let description = if parsed.title.is_empty() {
            doc_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("feature")
                .to_string()
        } else {
            parsed.title.clone()
        };
        let parsed_doc = Some(parsed);

        let parsed_container = parse_container_mode(container)?;
        let parsed_timeout = parse_duration(timeout)?;
        let agent = crate::agents::resolve_agent(crosslink_dir)?;
        let policy = resolve_execution_policy(
            &agent,
            permission_mode,
            skip_permissions,
            None,
            None,
            parsed_timeout,
            (parsed_container != ContainerMode::None)
                .then_some(crate::agents::SandboxPosture::ExternalIsolation),
        )?;
        let opts = KickoffOpts {
            description: &description,
            issue,
            container: parsed_container,
            verify: parse_verify_level(verify)?,
            model,
            image: types::DEFAULT_AGENT_IMAGE,
            timeout: parsed_timeout,
            dry_run,
            branch: None,
            quiet,
            design_doc: parsed_doc.as_ref(),
            doc_path: doc.as_ref().map(|p| p.to_str().unwrap_or("unknown")),
            policy,
            template: None,
        };

        run(crosslink_dir, db, writer, &opts)?;
        return Ok(());
    }

    let choices = if let Some(ref doc_path) = doc {
        wizard_with_preselected_doc(crosslink_dir, doc_path)?
    } else {
        wizard::launch_wizard(crosslink_dir)?
    };

    let Some(choices) = choices else {
        if !quiet {
            println!("Kickoff cancelled.");
        }
        return Ok(());
    };

    match choices.stage {
        wizard::WizardStage::Plan => {
            let doc_path = match &choices.source {
                wizard::WizardSource::DesignDoc(p) => p.clone(),
                wizard::WizardSource::QuickDescription(_) => {
                    bail!("Plan mode requires a design document");
                }
            };
            let config = choices.plan_config.unwrap_or_default();
            let content = std::fs::read_to_string(&doc_path)
                .with_context(|| format!("Failed to read design doc: {}", doc_path.display()))?;
            let design_doc = super::design_doc::parse_design_doc(&content);
            let parsed_timeout = parse_duration(&config.timeout)?;
            let agent = crate::agents::resolve_agent(crosslink_dir)?;
            let policy = resolve_execution_policy(
                &agent,
                None,
                false,
                None,
                None,
                parsed_timeout,
                Some(crate::agents::SandboxPosture::ReadOnly),
            )?;
            let plan_opts = PlanOpts {
                doc: &design_doc,
                doc_path: Some(&doc_path),
                model: &config.model,
                timeout: parsed_timeout,
                dry_run: false,
                issue,
                quiet,

                policy,
                template: None,
            };
            plan(crosslink_dir, db, &plan_opts)
        }
        wizard::WizardStage::Run => {
            let config = choices.run_config.unwrap_or_default();
            let (description, parsed_doc, doc_path_str) = match &choices.source {
                wizard::WizardSource::DesignDoc(p) => {
                    let content = std::fs::read_to_string(p)
                        .with_context(|| format!("Failed to read design doc: {}", p.display()))?;
                    let d = super::design_doc::parse_design_doc(&content);
                    let title = if d.title.is_empty() {
                        p.file_stem()
                            .and_then(|s| s.to_str())
                            .unwrap_or("feature")
                            .to_string()
                    } else {
                        d.title.clone()
                    };
                    let path_str = p.to_str().unwrap_or("unknown").to_string();
                    (title, Some(d), Some(path_str))
                }
                wizard::WizardSource::QuickDescription(desc) => (desc.clone(), None, None),
            };

            let parsed_container = parse_container_mode(&config.container)?;
            let parsed_timeout = parse_duration(&config.timeout)?;
            let agent = crate::agents::resolve_agent(crosslink_dir)?;
            let policy = resolve_execution_policy(
                &agent,
                None,
                false,
                None,
                None,
                parsed_timeout,
                (parsed_container != ContainerMode::None)
                    .then_some(crate::agents::SandboxPosture::ExternalIsolation),
            )?;
            let opts = KickoffOpts {
                description: &description,
                issue: config.issue,
                container: parsed_container,
                verify: parse_verify_level(&config.verify)?,
                model: &config.model,
                image: types::DEFAULT_AGENT_IMAGE,
                timeout: parsed_timeout,
                dry_run: false,
                branch: None,
                quiet,
                design_doc: parsed_doc.as_ref(),
                doc_path: doc_path_str.as_deref(),
                policy,
                template: None,
            };
            run(crosslink_dir, db, writer, &opts)?;
            Ok(())
        }
    }
}

fn wizard_with_preselected_doc(
    crosslink_dir: &Path,
    doc_path: &Path,
) -> Result<Option<wizard::WizardChoices>> {
    if !doc_path.exists() {
        bail!(
            "Design document not found: {}\nCreate one with: crosslink design \"feature description\"",
            doc_path.display()
        );
    }
    wizard::launch_wizard(crosslink_dir)
}

fn pipeline_status_overview(crosslink_dir: &Path, json: bool) -> Result<()> {
    let root = crosslink_dir
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Cannot determine repo root"))?;

    let mut states = pipeline::scan_pipeline_states(root);
    let agents = monitor::discover_agents(crosslink_dir).unwrap_or_default();

    let live_ids: Vec<String> = agents
        .iter()
        .filter(|a| a.session.is_some() || a.docker.is_some())
        .map(|a| a.id.clone())
        .collect();
    for (doc_path, state) in &mut states {
        pipeline::reconcile_runs_for_display(doc_path, state, &live_ids);
    }

    if states.is_empty() {
        println!("No pipeline state files found in .design/");
        println!("Create a design doc with: crosslink design \"feature description\"");
        return Ok(());
    }

    if json {
        let json_states: Vec<_> = states.iter().map(|(_, s)| s).collect();
        println!("{}", serde_json::to_string_pretty(&json_states)?);
        return Ok(());
    }

    println!(
        "{:<34} {:<12} {:<14} {:<8} RUN",
        "DESIGN DOC", "STAGE", "PLAN", "GAPS"
    );

    for (doc_path, state) in &states {
        let filename = doc_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown");

        let stage = &state.stage;

        let plan_display = state.plans.last().map_or_else(
            || "\u{2014}".to_string(),
            |plan| {
                let age = plan.completed_at.as_ref().map_or_else(String::new, |ts| {
                    chrono::DateTime::parse_from_rfc3339(ts).map_or_else(
                        |_| String::new(),
                        |dt| {
                            let elapsed = chrono::Utc::now()
                                .signed_duration_since(dt.with_timezone(&chrono::Utc));
                            let mins = elapsed.num_minutes();
                            if mins < 60 {
                                format!(" ({mins}m)")
                            } else {
                                format!(" ({}h)", mins / 60)
                            }
                        },
                    )
                });
                format!("{}{}", plan.status, age)
            },
        );

        let gaps_display = state.plans.last().map_or_else(
            || "\u{2014}".to_string(),
            |plan| {
                if plan.status == "done" {
                    format!("{}/{}", plan.blocking_gaps, plan.advisory_gaps)
                } else {
                    "\u{2014}".to_string()
                }
            },
        );

        let run_display = state.runs.last().map_or_else(
            || "\u{2014}".to_string(),
            |run| {
                let live = agents.iter().find(|a| a.id == run.agent_id);
                live.map_or_else(
                    || format!("{} ({})", run.agent_id, run.status),
                    |agent| {
                        if agent.session.is_some() {
                            format!("{} ({})", run.agent_id, agent.status)
                        } else {
                            format!("{} ({})", run.agent_id, run.status)
                        }
                    },
                )
            },
        );

        println!(
            "{:<34} {:<12} {:<14} {:<8} {}",
            helpers::truncate_str(filename, 33),
            stage,
            helpers::truncate_str(&plan_display, 13),
            gaps_display,
            helpers::truncate_str(&run_display, 40),
        );
    }

    Ok(())
}
