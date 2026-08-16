use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};
use serde::Serialize;

use super::helpers::truncate_str;
use super::monitor::discover_agents;
use super::types::AgentInfo;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
enum Annotation {
    Tmux(String),
    Docker(String),
    Status(String),
    Orphan,
}

impl std::fmt::Display for Annotation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Annotation::Tmux(s) => write!(f, "tmux: {s}"),
            Annotation::Docker(s) => write!(f, "docker: {s}"),
            Annotation::Status(s) => write!(f, "{s}"),
            Annotation::Orphan => write!(f, "orphan"),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct BranchNode {
    branch_name: String,
    fork_point: String,
    base_branch: String,
    tip_commit: String,
    intermediate_count: usize,
    annotation: Annotation,

    merged: bool,
}

#[derive(Debug, Serialize)]
struct GraphJson {
    base_branches: Vec<String>,
    kickoff_branches: Vec<BranchJsonEntry>,
}

#[derive(Debug, Serialize)]
struct BranchJsonEntry {
    branch: String,
    base: String,
    intermediate_commits: usize,
    annotation: serde_json::Value,
    merged: bool,
}

pub fn graph(crosslink_dir: &Path, all: bool, json: bool, quiet: bool) -> Result<()> {
    let term_width = crossterm::terminal::size().map_or(80, |(w, _)| w as usize);

    let agents = discover_agents(crosslink_dir).unwrap_or_default();
    let base_branches = discover_base_branches();

    let mut nodes: Vec<BranchNode> = Vec::new();

    for agent in &agents {
        if !all
            && matches!(
                agent.status.as_str(),
                "done" | "stopped" | "timed-out" | "failed"
            )
        {
            continue;
        }

        let branch = agent_branch_name(agent);
        let Some(branch) = branch else { continue };

        if !ref_exists(&branch) {
            continue;
        }

        let annotation = agent_annotation(agent);
        if let Some(node) = build_branch_node(&branch, &base_branches, annotation) {
            nodes.push(node);
        }
    }

    if all {
        let orphans = find_orphan_branches(&agents)?;
        for orphan_branch in orphans {
            if let Some(node) =
                build_branch_node(&orphan_branch, &base_branches, Annotation::Orphan)
            {
                if !nodes.iter().any(|n| n.branch_name == orphan_branch) {
                    nodes.push(node);
                }
            }
        }
    }

    if json {
        return output_json(&base_branches, &nodes);
    }

    if quiet {
        for node in &nodes {
            println!("{}", node.branch_name);
        }
        return Ok(());
    }

    render_ascii(&base_branches, &nodes, term_width);
    Ok(())
}

fn discover_base_branches() -> Vec<String> {
    let mut bases = Vec::new();
    for name in &["develop", "main"] {
        if ref_exists(name) {
            bases.push(name.to_string());
        }
    }

    if bases.is_empty() {
        bases.push("HEAD".to_string());
    }
    bases
}

fn ref_exists(name: &str) -> bool {
    Command::new("git")
        .args(["rev-parse", "--verify", "--quiet", name])
        .output()
        .is_ok_and(|o| o.status.success())
}

fn agent_branch_name(agent: &AgentInfo) -> Option<String> {
    if agent.worktree.is_empty() {
        return None;
    }

    let output = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "--symbolic-full-name", "HEAD"])
        .current_dir(&agent.worktree)
        .output()
        .ok()?;
    if output.status.success() {
        let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if branch == "HEAD" {
            let dir_name = Path::new(&agent.worktree)
                .file_name()?
                .to_str()?
                .to_string();
            Some(format!("feature/{dir_name}"))
        } else {
            Some(branch)
        }
    } else {
        None
    }
}

fn agent_annotation(agent: &AgentInfo) -> Annotation {
    agent.session.as_ref().map_or_else(
        || {
            agent.docker.as_ref().map_or_else(
                || Annotation::Status(agent.status.clone()),
                |container| Annotation::Docker(container.clone()),
            )
        },
        |session| Annotation::Tmux(session.clone()),
    )
}

fn build_branch_node(
    branch: &str,
    base_branches: &[String],
    annotation: Annotation,
) -> Option<BranchNode> {
    let tip = git_rev_parse(branch)?;

    let mut best: Option<(String, String, usize)> = None;

    for base in base_branches {
        if base == "HEAD" {
            if let Some(fork) = git_merge_base("HEAD", branch) {
                let count = git_rev_list_count(&fork, branch).unwrap_or(0);
                if best.as_ref().is_none_or(|b| count < b.2) {
                    best = Some((base.clone(), fork, count));
                }
            }
            continue;
        }
        if let Some(fork) = git_merge_base(base, branch) {
            let count = git_rev_list_count(&fork, branch).unwrap_or(0);
            if best.as_ref().is_none_or(|b| count < b.2) {
                best = Some((base.clone(), fork, count));
            }
        }
    }

    let Some((base_branch, fork_point, intermediate_count)) = best else {
        eprintln!("warning: cannot determine fork point for '{branch}', skipping");
        return None;
    };

    let merged = git_is_ancestor(&tip, &base_branch);

    Some(BranchNode {
        branch_name: branch.to_string(),
        fork_point,
        base_branch,
        tip_commit: tip,
        intermediate_count,
        annotation,
        merged,
    })
}

fn find_orphan_branches(agents: &[AgentInfo]) -> Result<Vec<String>> {
    let output = Command::new("git")
        .args([
            "for-each-ref",
            "--format=%(refname:short)",
            "refs/heads/feature/",
        ])
        .output()
        .context("Failed to list feature branches")?;

    if !output.status.success() {
        return Ok(Vec::new());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let agent_branches: Vec<String> = agents
        .iter()
        .filter_map(|a| {
            let name = agent_branch_name(a)?;
            Some(name)
        })
        .collect();

    let mut orphans = Vec::new();
    for line in stdout.lines() {
        let branch = line.trim();
        if branch.is_empty() {
            continue;
        }

        if agent_branches.iter().any(|ab| ab == branch) {
            continue;
        }
        orphans.push(branch.to_string());
    }

    Ok(orphans)
}

fn git_rev_parse(refname: &str) -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", refname])
        .output()
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        None
    }
}

fn git_merge_base(a: &str, b: &str) -> Option<String> {
    let output = Command::new("git")
        .args(["merge-base", a, b])
        .output()
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        None
    }
}

fn git_is_ancestor(commit: &str, branch: &str) -> bool {
    Command::new("git")
        .args(["merge-base", "--is-ancestor", commit, branch])
        .output()
        .is_ok_and(|o| o.status.success())
}

fn git_rev_list_count(from: &str, to: &str) -> Option<usize> {
    let range = format!("{from}..{to}");
    let output = Command::new("git")
        .args(["rev-list", "--count", &range])
        .output()
        .ok()?;
    if output.status.success() {
        String::from_utf8_lossy(&output.stdout).trim().parse().ok()
    } else {
        None
    }
}

fn output_json(base_branches: &[String], nodes: &[BranchNode]) -> Result<()> {
    let entries: Vec<BranchJsonEntry> = nodes
        .iter()
        .map(|n| {
            let annotation_value = match &n.annotation {
                Annotation::Tmux(s) => serde_json::json!({ "tmux": s }),
                Annotation::Docker(s) => serde_json::json!({ "docker": s }),
                Annotation::Status(s) => serde_json::json!({ "status": s }),
                Annotation::Orphan => serde_json::json!({ "status": "orphan" }),
            };
            BranchJsonEntry {
                branch: n.branch_name.clone(),
                base: n.base_branch.clone(),
                intermediate_commits: n.intermediate_count,
                annotation: annotation_value,
                merged: n.merged,
            }
        })
        .collect();

    let graph = GraphJson {
        base_branches: base_branches.to_vec(),
        kickoff_branches: entries,
    };

    println!("{}", serde_json::to_string_pretty(&graph)?);
    Ok(())
}

fn render_ascii(base_branches: &[String], nodes: &[BranchNode], term_width: usize) {
    if nodes.is_empty() {
        for (i, base) in base_branches.iter().enumerate() {
            println!("  * {base}");
            if i < base_branches.len() - 1 {
                println!("  |");
            }
        }
        return;
    }

    let mut by_base: std::collections::HashMap<String, Vec<&BranchNode>> =
        std::collections::HashMap::new();
    for node in nodes {
        by_base
            .entry(node.base_branch.clone())
            .or_default()
            .push(node);
    }

    for branches in by_base.values_mut() {
        branches.sort_by_key(|b| std::cmp::Reverse(b.intermediate_count));
    }

    let label_max = if term_width > 8 { term_width - 8 } else { 72 };

    for (i, base) in base_branches.iter().enumerate() {
        if let Some(branches) = by_base.get(base) {
            for branch in branches {
                if branch.merged {
                    println!("  |\\");
                }

                for _ in 0..branch.intermediate_count {
                    println!("  | *");
                }

                let label = format!("{}", branch.annotation);
                let merged_tag = if branch.merged { " ✓merged" } else { "" };
                let tip_line = format!("{} [{}]{}", branch.branch_name, label, merged_tag);
                let tip_display = truncate_str(&tip_line, label_max);
                println!("  | * {tip_display}");

                println!("  |/");
            }
        }

        println!("  * {base}");

        if i < base_branches.len() - 1 {
            println!("  |");
        }
    }
}
