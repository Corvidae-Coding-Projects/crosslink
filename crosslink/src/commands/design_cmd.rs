use anyhow::{Context, Result};
use std::path::Path;
use std::time::Duration;

use crate::agents::{
    build_invocation, execute_foreground, resolve_agent, ApprovalPolicy, ExecutionPolicy,
    InvocationRequest, OutputProtocol, SandboxPosture,
};

pub fn run(
    crosslink_dir: &Path,
    description: Option<&str>,
    issue: Option<i64>,
    gh_issue: Option<i64>,
    continue_slug: Option<&str>,
) -> Result<()> {
    if std::env::var("CLAUDE_CODE").is_ok() || std::env::var("CLAUDECODE").is_ok() {
        eprintln!("Already inside Claude Code — use the installed `design` skill (`/design`).");
        std::process::exit(1);
    }
    if ["CODEX_THREAD_ID", "CODEX_SESSION_ID"]
        .iter()
        .any(|name| std::env::var_os(name).is_some())
    {
        eprintln!("Already inside Codex — ask it to use the installed `design` skill.");
        std::process::exit(1);
    }

    let agent = resolve_agent(crosslink_dir)?;

    let mut args_parts = Vec::new();

    if let Some(slug) = continue_slug {
        args_parts.push(format!("--continue {slug}"));
    } else if let Some(desc) = description {
        args_parts.push(format!("\"{desc}\""));
    }

    if let Some(id) = issue {
        args_parts.push(format!("--issue {id}"));
    }
    if let Some(id) = gh_issue {
        args_parts.push(format!("--gh-issue {id}"));
    }

    let arguments = args_parts.join(" ");

    let skill_prompt = include_str!("../../resources/agent/skills/design/SKILL.md");

    let prompt_body = strip_frontmatter(skill_prompt);

    let full_prompt = if arguments.is_empty() {
        prompt_body.to_string()
    } else {
        format!("ARGUMENTS: {arguments}\n\n{prompt_body}")
    };

    let mut prompt = tempfile::Builder::new()
        .prefix("crosslink-design-")
        .suffix(".md")
        .tempfile_in(std::env::temp_dir())
        .context("Failed to create the design prompt")?;
    use std::io::Write as _;
    prompt
        .write_all(full_prompt.as_bytes())
        .context("Failed to write the design prompt")?;

    let model = agent.resolve_model(None);
    let invocation = build_invocation(
        &agent,
        &InvocationRequest {
            cwd: crosslink_dir.parent().unwrap_or(crosslink_dir),
            prompt_file: prompt.path(),
            model: model.as_deref(),
            allowed_tools: None,
            policy: ExecutionPolicy {
                approval: ApprovalPolicy::Interactive,
                sandbox: SandboxPosture::WorkspaceWrite,
                effort: None,
                monetary_budget_usd: None,
                timeout: Duration::from_secs(24 * 60 * 60),
            },
            output: OutputProtocol::Interactive,
            verified_hook_trust: false,
            claude_config_dir: std::env::var("CLAUDE_CONFIG_DIR").ok().as_deref(),
        },
    )?;
    let status = execute_foreground(&invocation)?;

    if !status.success() {
        let code = status.code().unwrap_or(1);
        std::process::exit(code);
    }

    Ok(())
}

fn strip_frontmatter(content: &str) -> &str {
    if !content.starts_with("---") {
        return content;
    }

    content[3..].find("\n---").map_or(content, |end| {
        let after_frontmatter = &content[3 + end + 4..];
        after_frontmatter.trim_start_matches('\n')
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_frontmatter_with_frontmatter() {
        let input = "---\nallowed-tools: Read\ndescription: test\n---\n\n## Context\nBody here";
        let result = strip_frontmatter(input);
        assert!(result.starts_with("## Context"));
    }

    #[test]
    fn test_strip_frontmatter_without_frontmatter() {
        let input = "## Context\nBody here";
        let result = strip_frontmatter(input);
        assert_eq!(result, input);
    }

    #[test]
    fn test_strip_frontmatter_empty() {
        let result = strip_frontmatter("");
        assert_eq!(result, "");
    }
}
