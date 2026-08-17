use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::agents::{
    build_invocation, render_shell_command, AgentProvider, ApprovalPolicy, ExecutionPolicy,
    InvocationRequest, OutputProtocol, ResolvedAgent, SandboxPosture,
};
use crate::identity::AgentConfig;

use super::helpers::*;
use super::types::*;

fn resolve_timeout_command(platform: &Platform) -> Result<&'static str> {
    if command_available("timeout") {
        return Ok("timeout");
    }
    if command_available("gtimeout") {
        return Ok("gtimeout");
    }
    bail!(
        "Neither `timeout` nor `gtimeout` found.\n{}",
        install_hint("timeout", platform)
    );
}

pub(super) fn read_sandbox_command(crosslink_dir: &Path) -> Option<String> {
    let config_path = crosslink_dir.join("hook-config.json");
    let content = std::fs::read_to_string(&config_path).ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&content).ok()?;
    parsed
        .get("sandbox")
        .and_then(|s| s.get("command"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
}

pub(super) fn read_watchdog_config(crosslink_dir: &Path) -> WatchdogConfig {
    let config_path = crosslink_dir.join("hook-config.json");
    let Ok(content) = std::fs::read_to_string(&config_path) else {
        return WatchdogConfig::default();
    };
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&content) else {
        return WatchdogConfig::default();
    };

    let Some(wd) = parsed.get("watchdog") else {
        return WatchdogConfig::default();
    };

    let mut cfg = WatchdogConfig::default();
    if let Some(v) = wd.get("enabled").and_then(serde_json::Value::as_bool) {
        cfg.enabled = v;
    }
    if let Some(v) = wd.get("staleness_secs").and_then(serde_json::Value::as_u64) {
        cfg.staleness_secs = v;
    }
    if let Some(v) = wd.get("max_nudges").and_then(serde_json::Value::as_u64) {
        cfg.max_nudges = u32::try_from(v).unwrap_or(u32::MAX);
    }
    if let Some(v) = wd
        .get("check_interval_secs")
        .and_then(serde_json::Value::as_u64)
    {
        cfg.check_interval_secs = v;
    }
    if let Some(v) = wd
        .get("grace_period_secs")
        .and_then(serde_json::Value::as_u64)
    {
        cfg.grace_period_secs = v;
    }
    cfg
}

pub(super) fn build_watchdog_script(
    session_name: &str,
    worktree_dir: &Path,
    cfg: &WatchdogConfig,
) -> String {
    format!(
        r#"NUDGES=0
sleep {grace}
while true; do
    sleep {interval}
    if [ -f "{worktree}/.kickoff-status" ]; then exit 0; fi
    if ! tmux has-session -t "{session}" 2>/dev/null; then exit 0; fi
    HB="{worktree}/.crosslink/.cache/last-heartbeat"
    if [ -f "$HB" ]; then
        LAST=$(stat -c %Y "$HB" 2>/dev/null || stat -f %m "$HB" 2>/dev/null)
        NOW=$(date +%s)
        AGE=$((NOW - LAST))
        if [ "$AGE" -gt {staleness} ]; then
            if [ "$NUDGES" -ge {max_nudges} ]; then exit 1; fi
            NUDGES=$((NUDGES + 1))
            tmux send-keys -t "{session}" "continue working, the task is not yet complete" Enter
        fi
    fi
done
"#,
        grace = cfg.grace_period_secs,
        interval = cfg.check_interval_secs,
        worktree = worktree_dir.display(),
        session = session_name,
        staleness = cfg.staleness_secs,
        max_nudges = cfg.max_nudges,
    )
}

pub(super) fn spawn_watchdog(
    session_name: &str,
    worktree_dir: &Path,
    cfg: &WatchdogConfig,
) -> Result<()> {
    let script = build_watchdog_script(session_name, worktree_dir, cfg);

    Command::new("bash")
        .args(["-c", &script])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("Failed to spawn watchdog process")?;

    Ok(())
}

#[cfg(test)]
pub(super) fn permission_flag(permission_mode: Option<&str>, skip_permissions: bool) -> String {
    match (permission_mode, skip_permissions) {
        (Some(mode), _) if !mode.is_empty() => {
            format!(
                " --permission-mode {}",
                crate::utils::shell_escape_arg(mode)
            )
        }
        (_, true) => " --dangerously-skip-permissions".to_string(),
        _ => String::new(),
    }
}

#[cfg(test)]
pub(super) fn dial_flags(effort: Option<&str>, budget_usd: Option<&str>) -> String {
    use crate::utils::shell_escape_arg;
    use std::fmt::Write as _;

    let mut flags = String::new();
    if let Some(level) = effort.filter(|v| !v.is_empty()) {
        let _ = write!(flags, " --effort {}", shell_escape_arg(level));
    }
    if let Some(amount) = budget_usd.filter(|v| !v.is_empty()) {
        let _ = write!(flags, " --max-budget-usd {}", shell_escape_arg(amount));
    }
    flags
}

pub(crate) fn resolve_execution_policy(
    agent: &ResolvedAgent,
    permission_mode: Option<&str>,
    skip_permissions: bool,
    effort: Option<&str>,
    budget_usd: Option<&str>,
    timeout: Duration,
    sandbox_override: Option<SandboxPosture>,
) -> Result<ExecutionPolicy> {
    if skip_permissions && permission_mode.is_some_and(|mode| !mode.is_empty()) {
        bail!("--skip-permissions cannot be combined with --permission-mode");
    }
    let configured_sandbox = match agent.options.sandbox.as_str() {
        "read-only" => SandboxPosture::ReadOnly,
        "workspace-write" => SandboxPosture::WorkspaceWrite,
        other => bail!(
            "Unsupported configured agent sandbox '{other}'; expected read-only or workspace-write"
        ),
    };
    let mut sandbox = sandbox_override.unwrap_or(configured_sandbox);
    let approval = match permission_mode.filter(|mode| !mode.is_empty()) {
        Some("acceptEdits") => ApprovalPolicy::AutoReview,
        Some("auto") => ApprovalPolicy::Automatic,
        Some("bypassPermissions") => ApprovalPolicy::Never,
        Some("default") => ApprovalPolicy::Interactive,
        Some("dontAsk") => ApprovalPolicy::DontAsk,
        Some("plan") => {
            sandbox = SandboxPosture::ReadOnly;
            ApprovalPolicy::Interactive
        }
        Some(mode) => bail!(
            "Unsupported --permission-mode '{mode}'; expected acceptEdits, auto, bypassPermissions, default, dontAsk, or plan"
        ),
        None if skip_permissions => ApprovalPolicy::Never,
        None => match agent.options.approval.as_str() {
            "interactive" | "on-request" => ApprovalPolicy::Interactive,
            "never" => ApprovalPolicy::Never,
            "auto-review" | "acceptEdits" => ApprovalPolicy::AutoReview,
            "automatic" | "auto" => ApprovalPolicy::Automatic,
            "dontAsk" | "dont-ask" => ApprovalPolicy::DontAsk,
            other => bail!("Unsupported configured agent approval policy '{other}'"),
        },
    };
    Ok(ExecutionPolicy {
        approval,
        sandbox,
        effort: effort.filter(|value| !value.is_empty()).map(str::to_string),
        monetary_budget_usd: budget_usd
            .filter(|value| !value.is_empty())
            .map(str::to_string),
        timeout,
    })
}

#[allow(clippy::too_many_arguments)]
pub(super) fn validate_agent_request(
    agent: &ResolvedAgent,
    cwd: &Path,
    model: &str,
    allowed_tools: &str,
    policy: &ExecutionPolicy,
) -> Result<()> {
    let model = agent.resolve_model(Some(model));
    build_invocation(
        agent,
        &InvocationRequest {
            cwd,
            prompt_file: Path::new("KICKOFF.md"),
            model: model.as_deref(),
            allowed_tools: Some(allowed_tools),
            policy: policy.clone(),
            output: OutputProtocol::JsonLines,
            verified_hook_trust: false,
            claude_config_dir: None,
        },
    )?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn build_resolved_agent_command(
    agent: &ResolvedAgent,
    timeout_cmd: &str,
    model: &str,
    allowed_tools: &str,
    kickoff_file: &str,
    sandbox_command: Option<&str>,
    worktree_dir: &Path,
    policy: &ExecutionPolicy,
) -> Result<String> {
    let verified_hook_trust = agent.provider == AgentProvider::Codex
        && crate::commands::init::codex_hook_trust_ready(worktree_dir)?;
    if agent.provider == AgentProvider::Codex && !verified_hook_trust {
        eprintln!(
            "Codex hook definitions do not match Crosslink's init manifest; normal /hooks review is required."
        );
    }
    let model = agent.resolve_model(Some(model));
    let claude_config_dir = std::env::var("CLAUDE_CONFIG_DIR").ok();
    let grant_git_common_dir =
        agent.provider == AgentProvider::Codex && policy.sandbox == SandboxPosture::WorkspaceWrite;
    let mut invocation = build_invocation(
        agent,
        &InvocationRequest {
            cwd: worktree_dir,
            prompt_file: Path::new(kickoff_file),
            model: model.as_deref(),
            allowed_tools: Some(allowed_tools),
            policy: policy.clone(),
            output: OutputProtocol::JsonLines,
            verified_hook_trust,
            claude_config_dir: claude_config_dir.as_deref(),
        },
    )?;

    if grant_git_common_dir {
        if let Some(common_dir) = git_common_dir_outside_worktree(worktree_dir) {
            invocation
                .args
                .extend(["--add-dir".into(), common_dir.as_os_str().to_os_string()]);
        }
    }
    let mut launch = render_shell_command(&invocation, timeout_cmd);
    if let Some(command) = sandbox_command {
        let prefix = format!("{timeout_cmd} {}s ", policy.timeout.as_secs());
        let escaped_worktree = crate::utils::shell_escape_path(worktree_dir);
        let wrapper = command.replace("{{worktree}}", &escaped_worktree);
        launch = launch.replacen(&prefix, &format!("{prefix}{wrapper} "), 1);
    }
    let status_path = crate::utils::shell_escape_path(&worktree_dir.join(".kickoff-status"));
    let raw_log_path = crate::utils::shell_escape_path(
        &worktree_dir.join(".crosslink/runtime/agent-events.jsonl"),
    );
    Ok(format!(
        "set -o pipefail; {{ {launch}; }} 2>&1 | tee -a {raw_log_path}; CROSSLINK_AGENT_RC=${{PIPESTATUS[0]}}; if [ \"$CROSSLINK_AGENT_RC\" -eq 124 ]; then printf 'TIMEOUT\\n' > {status_path}; fi; exit \"$CROSSLINK_AGENT_RC\""
    ))
}

fn git_common_dir_outside_worktree(worktree_dir: &Path) -> Option<PathBuf> {
    let absolute = Command::new("git")
        .current_dir(worktree_dir)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .output()
        .ok()
        .filter(|output| output.status.success());
    let output = absolute.or_else(|| {
        Command::new("git")
            .current_dir(worktree_dir)
            .args(["rev-parse", "--git-common-dir"])
            .output()
            .ok()
            .filter(|output| output.status.success())
    })?;
    let raw = String::from_utf8(output.stdout).ok()?;
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let candidate = PathBuf::from(raw);
    let candidate = if candidate.is_absolute() {
        candidate
    } else {
        worktree_dir.join(candidate)
    };
    let common_dir = candidate.canonicalize().ok()?;
    let canonical_worktree = worktree_dir.canonicalize().ok()?;
    (!common_dir.starts_with(canonical_worktree)).then_some(common_dir)
}

#[allow(clippy::too_many_arguments)]
#[cfg(test)]
pub(super) fn build_agent_command(
    agent_binary: &str,
    timeout_cmd: &str,
    timeout_secs: u64,
    model: &str,
    allowed_tools: &str,
    kickoff_file: &str,
    sandbox_command: Option<&str>,
    worktree_dir: &Path,
    skip_permissions: bool,
    claude_config_dir: Option<&str>,
    permission_mode: Option<&str>,
    effort: Option<&str>,
    budget_usd: Option<&str>,
) -> String {
    use crate::utils::shell_escape_arg;

    let permission_flag_owned = permission_flag(permission_mode, skip_permissions);
    let skip_flag = permission_flag_owned.as_str();

    let env_assignment = if agent_binary == "claude" {
        claude_config_dir
            .filter(|v| !v.is_empty())
            .map(|v| format!("CLAUDE_CONFIG_DIR={} ", shell_escape_arg(v)))
            .unwrap_or_default()
    } else {
        String::new()
    };
    let escaped_model = shell_escape_arg(model);
    let escaped_tools = shell_escape_arg(allowed_tools);
    let escaped_kickoff = shell_escape_arg(kickoff_file);

    let dials = dial_flags(effort, budget_usd);
    let agent_cmd = if agent_binary == "claude" {
        format!(
            "env -u CLAUDECODE {env_assignment}{agent_binary}{skip_flag} --model {escaped_model}{dials} --allowedTools {escaped_tools} -- \"$(cat {escaped_kickoff})\""
        )
    } else {
        format!("env -u CLAUDECODE {agent_binary} < {escaped_kickoff}")
    };
    let launch = sandbox_command.map_or_else(
        || format!("{timeout_cmd} {timeout_secs}s {agent_cmd}"),
        |cmd| {
            let escaped_worktree = crate::utils::shell_escape_path(worktree_dir);
            let expanded = cmd.replace("{{worktree}}", &escaped_worktree);
            format!("{timeout_cmd} {timeout_secs}s {expanded} {agent_cmd}")
        },
    );

    let status_path = crate::utils::shell_escape_path(&worktree_dir.join(".kickoff-status"));
    format!("{launch}; if [ $? -eq 124 ]; then printf 'TIMEOUT\\n' > {status_path}; fi")
}

pub(super) fn preflight_check(
    container: &ContainerMode,
    verify: &VerifyLevel,
    crosslink_dir: &Path,
) -> Result<PreflightResult> {
    let platform = detect_platform();
    let agent = crate::agents::resolve_agent(crosslink_dir)?;
    let agent_binary = agent.binary.to_string_lossy().into_owned();
    let mut missing: Vec<String> = Vec::new();

    let timeout_cmd = match resolve_timeout_command(&platform) {
        Ok(cmd) => cmd,
        Err(e) => {
            missing.push(format!("{e}"));
            "timeout"
        }
    };

    if *container == ContainerMode::None {
        if cfg!(target_os = "windows") {
            bail!(
                "Local kickoff mode requires tmux, which is not available on Windows.\n\
                 Use `--container docker` for agent kickoff on Windows."
            );
        }
        if !command_available("tmux") {
            missing.push(install_hint("tmux", &platform));
        }
    }

    if *container == ContainerMode::None && !command_available(&agent_binary) {
        missing.push(install_hint(&agent_binary, &platform));
    }

    if (*verify == VerifyLevel::Ci || *verify == VerifyLevel::Thorough) && !command_available("gh")
    {
        missing.push(install_hint("gh", &platform));
    }

    match container {
        ContainerMode::Docker if !command_available("docker") => {
            missing.push(install_hint("docker", &platform));
        }
        ContainerMode::Podman if !command_available("podman") => {
            missing.push(install_hint("podman", &platform));
        }
        _ => {}
    }

    let sandbox_command = read_sandbox_command(crosslink_dir);
    if let Some(ref cmd) = sandbox_command {
        let binary = cmd.split_whitespace().next().unwrap_or(cmd);
        if !command_available(binary) {
            missing.push(format!(
                "`{binary}` (configured in hook-config.json sandbox.command) not found on PATH"
            ));
        }
    }

    if !missing.is_empty() {
        let header = format!(
            "Pre-flight check failed — {} missing command{}:\n",
            missing.len(),
            if missing.len() == 1 { "" } else { "s" }
        );
        let body = missing
            .iter()
            .enumerate()
            .map(|(i, msg)| format!("{}. {}", i + 1, msg))
            .collect::<Vec<_>>()
            .join("\n\n");
        bail!("{header}{body}");
    }

    if *container == ContainerMode::None {
        crate::agents::verify_account_login(&agent)?;
    }

    Ok(PreflightResult {
        timeout_cmd,
        sandbox_command,
        agent,
    })
}

pub(super) fn repo_root() -> Result<std::path::PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .context("Failed to run git rev-parse")?;
    if !output.status.success() {
        bail!("Not inside a git repository");
    }
    let toplevel = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let toplevel_path = std::path::PathBuf::from(&toplevel);

    Ok(crate::utils::resolve_main_repo_root(&toplevel_path).unwrap_or(toplevel_path))
}

pub(super) fn create_worktree(
    repo_root: &Path,
    slug: &str,
    base_branch: Option<&str>,
) -> Result<(std::path::PathBuf, String)> {
    let branch_name = format!("feature/{slug}");
    let worktree_dir = repo_root.join(".worktrees").join(slug);

    let canonical_root = repo_root
        .canonicalize()
        .unwrap_or_else(|_| repo_root.to_path_buf());
    for forbidden in [".crosslink", ".git"] {
        let forbidden_dir = canonical_root.join(forbidden);
        if let Ok(canonical_wt) = worktree_dir.canonicalize() {
            if canonical_wt.starts_with(&forbidden_dir) {
                bail!(
                    "Worktree path {} would land inside {}/. \
                     This usually means repo_root resolved to an internal directory. \
                     Please run this command from the main repository root.",
                    worktree_dir.display(),
                    forbidden
                );
            }
        }
    }

    if worktree_dir.exists() {
        bail!(
            "Worktree already exists at {}. Remove it first or use --branch to target an existing branch.",
            worktree_dir.display()
        );
    }

    let base = base_branch.unwrap_or("HEAD");

    let branch_exists = Command::new("git")
        .current_dir(repo_root)
        .args(["rev-parse", "--verify", &branch_name])
        .output()
        .is_ok_and(|o| o.status.success());

    if branch_exists {
        let wt_output = Command::new("git")
            .current_dir(repo_root)
            .args(["worktree", "list", "--porcelain"])
            .output()
            .context("Failed to list worktrees")?;
        let wt_list = String::from_utf8_lossy(&wt_output.stdout);
        let has_active_worktree = wt_list
            .lines()
            .any(|line| line.starts_with("branch ") && line.ends_with(&branch_name));

        if has_active_worktree {
            bail!(
                "Branch '{branch_name}' already exists and has an active worktree. \
                 Clean up the worktree first with: git worktree remove <path>"
            );
        }

        let is_merged = Command::new("git")
            .current_dir(repo_root)
            .args(["merge-base", "--is-ancestor", &branch_name, base])
            .output()
            .is_ok_and(|o| o.status.success());

        if is_merged {
            tracing::info!(
                "branch '{}' exists from a prior phase and is fully merged, recreating",
                branch_name
            );
            let delete_output = Command::new("git")
                .current_dir(repo_root)
                .args(["branch", "-d", &branch_name])
                .output()
                .context("Failed to delete merged branch")?;
            if !delete_output.status.success() {
                let stderr = String::from_utf8_lossy(&delete_output.stderr);
                bail!(
                    "Branch '{}' is merged but could not be deleted: {}",
                    branch_name,
                    stderr.trim()
                );
            }
        } else {
            bail!(
                "Branch '{branch_name}' already exists and has unmerged changes. \
                 Either merge it first, delete it manually with \
                 `git branch -D {branch_name}`, or use a different slug."
            );
        }
    }

    let output = Command::new("git")
        .current_dir(repo_root)
        .args(["worktree", "add", "-b", &branch_name])
        .arg(&worktree_dir)
        .arg(base)
        .output()
        .context("Failed to create git worktree")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("Failed to create worktree: {}", stderr.trim());
    }

    Ok((worktree_dir, branch_name))
}

pub(super) fn init_worktree_agent(
    worktree_dir: &Path,
    crosslink_dir: &Path,
    compact_name: &str,
    issue_id: Option<i64>,
) -> Result<String> {
    let output = Command::new("crosslink")
        .current_dir(worktree_dir)
        .args(["init", "--skip-signing", "--defaults"])
        .output()
        .context("Failed to run crosslink init in worktree")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::warn!("crosslink init in worktree: {}", stderr.trim());
    }

    let agent_id = compact_name.to_string();

    let wt_crosslink = worktree_dir.join(".crosslink");
    if wt_crosslink.exists() && AgentConfig::load(&wt_crosslink)?.is_none() {
        if let Err(e) = super::super::agent::init(
            &wt_crosslink,
            &agent_id,
            Some(&format!("Kickoff agent for: {compact_name}")),
            false,
            false,
            crate::identity::AgentRole::Agent,
        ) {
            tracing::warn!("could not initialize agent identity in worktree: {e} — agent will work without its own identity");
        }

        if let Err(e) = super::super::trust::approve(crosslink_dir, &agent_id) {
            tracing::warn!(
                "could not auto-approve agent '{}': {e} — run `crosslink trust approve {}` manually",
                agent_id, agent_id
            );
        }
    }

    let output = Command::new("crosslink")
        .current_dir(worktree_dir)
        .args(["sync"])
        .output();

    if let Ok(o) = output {
        if !o.status.success() {
            tracing::warn!("crosslink sync in worktree returned non-zero");
        }
    }

    if let Some(issue_id) = issue_id {
        let start = Command::new("crosslink")
            .current_dir(worktree_dir)
            .args(["session", "start"])
            .output()
            .context("Failed to start kickoff worktree session")?;
        if !start.status.success() {
            bail!(
                "Failed to start kickoff worktree session: {}",
                String::from_utf8_lossy(&start.stderr).trim()
            );
        }
        let issue = issue_id.to_string();
        let work = Command::new("crosslink")
            .current_dir(worktree_dir)
            .args(["session", "work", &issue])
            .output()
            .context("Failed to activate kickoff issue in worktree")?;
        if !work.status.success() {
            bail!(
                "Failed to activate kickoff issue #{} in worktree: {}",
                issue_id,
                String::from_utf8_lossy(&work.stderr).trim()
            );
        }
    }

    Ok(agent_id)
}

pub(super) fn exclude_kickoff_files(worktree_dir: &Path) -> Result<()> {
    let output = Command::new("git")
        .current_dir(worktree_dir)
        .args(["rev-parse", "--git-common-dir"])
        .output()
        .context("Failed to get git common dir")?;

    let common_dir = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let exclude_path = std::path::PathBuf::from(&common_dir).join("info/exclude");

    if let Some(parent) = exclude_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }

    let existing = std::fs::read_to_string(&exclude_path).unwrap_or_default();
    let additions = missing_exclude_patterns(&existing);

    if !additions.is_empty() {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&exclude_path)
            .context("Failed to open git exclude file")?;
        for pattern in additions {
            writeln!(file, "{pattern}")?;
        }
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn launch_local(
    agent: &ResolvedAgent,
    worktree_dir: &Path,
    session_name: &str,
    model: &str,
    allowed_tools: &str,
    timeout_cmd: &str,
    sandbox_command: Option<&str>,
    crosslink_dir: &Path,
    policy: &ExecutionPolicy,
) -> Result<()> {
    std::fs::create_dir_all(worktree_dir.join(".crosslink/runtime"))
        .context("Failed to create provider runtime log directory")?;

    let output = Command::new("tmux")
        .args([
            "new-session",
            "-d",
            "-s",
            session_name,
            "-c",
            &worktree_dir.to_string_lossy(),
        ])
        .output()
        .context("Failed to create tmux session")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("Failed to create tmux session: {}", stderr.trim());
    }

    let cmd = build_resolved_agent_command(
        agent,
        timeout_cmd,
        model,
        allowed_tools,
        "KICKOFF.md",
        sandbox_command,
        worktree_dir,
        policy,
    )?;

    std::fs::write(worktree_dir.join(".kickoff-status"), "LAUNCHING\n")
        .context("Failed to write initial .kickoff-status")?;

    let output = Command::new("tmux")
        .args(["send-keys", "-t", session_name, &cmd, "Enter"])
        .output()
        .context("Failed to send command to tmux session")?;

    if !output.status.success() {
        let _ = std::fs::write(worktree_dir.join(".kickoff-status"), "FAILED\n");
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("Failed to send keys to tmux: {}", stderr.trim());
    }

    let _ = std::fs::write(worktree_dir.join(".kickoff-status"), "RUNNING\n");

    let watchdog_cfg = read_watchdog_config(crosslink_dir);
    if watchdog_cfg.enabled {
        if let Err(e) = spawn_watchdog(session_name, worktree_dir, &watchdog_cfg) {
            tracing::warn!("failed to spawn watchdog: {}", e);
        }
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn launch_container(
    runtime: &ContainerMode,
    agent: &ResolvedAgent,
    worktree_dir: &Path,
    host_repo_root: &Path,
    image: &str,
    agent_id: &str,
    model: &str,
    allowed_tools: &str,
    timeout: Duration,
    protected_doc_rel: Option<&Path>,
    policy: &ExecutionPolicy,
) -> Result<String> {
    std::fs::create_dir_all(worktree_dir.join(".crosslink/runtime"))
        .context("Failed to create provider runtime log directory")?;
    let runtime_cmd = match runtime {
        ContainerMode::Docker => "docker",
        ContainerMode::Podman => "podman",
        ContainerMode::None => unreachable!(),
    };

    if !command_available(runtime_cmd) {
        bail!("{runtime_cmd} is not installed. Install it or use --container none for local mode.");
    }

    let timeout_secs = timeout.as_secs();
    let container_name = format!("crosslink-agent-{agent_id}");

    let uid_gid = if cfg!(target_os = "windows") {
        None
    } else {
        let uid = Command::new("id").arg("-u").output().map_or_else(
            |_| "1000".to_string(),
            |o| String::from_utf8_lossy(&o.stdout).trim().to_string(),
        );
        let gid = Command::new("id").arg("-g").output().map_or_else(
            |_| "1000".to_string(),
            |o| String::from_utf8_lossy(&o.stdout).trim().to_string(),
        );
        Some((uid, gid))
    };

    let mut args = vec![
        "run".to_string(),
        "-d".to_string(),
        "--name".to_string(),
        container_name,
        "--stop-timeout".to_string(),
        format!("{}", timeout_secs),
        "-v".to_string(),
        format!("{}:/workspaces/repo", worktree_dir.to_string_lossy()),
        "-e".to_string(),
        format!("AGENT_ID={}", agent_id),
    ];

    let credential_volume = crate::commands::container::credential_volume(agent.provider)?;
    args.extend([
        "-v".to_string(),
        format!("{credential_volume}:/home/agent/.{}", agent.provider),
        "-e".to_string(),
        format!("CROSSLINK_AGENT_PROVIDER={}", agent.provider),
        "-e".to_string(),
        "CROSSLINK_REQUIRE_LOGIN=1".to_string(),
    ]);

    let host_git_dir = host_repo_root.join(".git");
    if host_git_dir.exists() {
        let git_path = host_git_dir.to_string_lossy();
        args.push("-v".to_string());
        args.push(format!("{git_path}:{git_path}:rw"));
    }

    if let Some((uid, gid)) = &uid_gid {
        args.extend([
            "-e".to_string(),
            format!("HOST_UID={uid}"),
            "-e".to_string(),
            format!("HOST_GID={gid}"),
        ]);
    }

    if let Some(rel) = protected_doc_rel {
        let host_doc = worktree_dir.join(rel);
        if host_doc.is_file() {
            let container_path = format!("/workspaces/repo/{}", rel.display());
            args.push("-v".to_string());
            args.push(format!(
                "{}:{}:ro",
                host_doc.to_string_lossy(),
                container_path
            ));
        }
    }

    let container_agent = ResolvedAgent {
        provider: agent.provider,
        binary: agent
            .provider
            .default_binary()
            .map_or_else(|| agent.binary.clone(), std::path::PathBuf::from),
        options: agent.options.clone(),
        legacy_inferred: agent.legacy_inferred,
    };
    let command = build_resolved_agent_command(
        &container_agent,
        "timeout",
        model,
        allowed_tools,
        "KICKOFF.md",
        None,
        Path::new("/workspaces/repo"),
        policy,
    )?;
    args.push(image.to_string());
    args.push("bash".to_string());
    args.push("-c".to_string());
    args.push(format!("cd /workspaces/repo && {command}"));

    let output = Command::new(runtime_cmd)
        .args(&args)
        .output()
        .with_context(|| format!("Failed to launch {runtime_cmd} container"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(format_container_launch_error(runtime_cmd, image, &stderr));
    }

    let container_id = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(container_id)
}

const AGENT_IMAGE_PACKAGE_URL: &str =
    "https://github.com/Corvidae-Coding-Projects/crosslink/pkgs/container/crosslink-agent";

fn format_container_launch_error(runtime_cmd: &str, image: &str, stderr: &str) -> String {
    let trimmed = stderr.trim();
    let lowered = trimmed.to_ascii_lowercase();
    let pull_failure = ["not found", "denied", "manifest unknown", "no such image"]
        .iter()
        .any(|needle| lowered.contains(needle));

    if pull_failure {
        format!(
            "{runtime_cmd} container launch failed: {trimmed}\n\n\
             Hint: the image `{image}` could not be pulled. Either:\n  \
               * Build it locally:  just build-image       (tags as :local)\n  \
               * Or pick a published tag from {AGENT_IMAGE_PACKAGE_URL}\n  \
                 and pass it via `--image ghcr.io/corvidae-coding-projects/crosslink-agent:<tag>`."
        )
    } else {
        format!("{runtime_cmd} container launch failed: {trimmed}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linked_worktree_resolves_external_git_common_dir() {
        let repo = tempfile::tempdir().unwrap();
        Command::new("git")
            .current_dir(repo.path())
            .args(["init", "-q"])
            .status()
            .unwrap();
        Command::new("git")
            .current_dir(repo.path())
            .args(["config", "user.name", "Crosslink Test"])
            .status()
            .unwrap();
        Command::new("git")
            .current_dir(repo.path())
            .args(["config", "user.email", "test@example.invalid"])
            .status()
            .unwrap();
        Command::new("git")
            .current_dir(repo.path())
            .args(["commit", "--allow-empty", "-qm", "seed"])
            .status()
            .unwrap();
        let worktree = repo.path().join("worktree");
        let created = Command::new("git")
            .current_dir(repo.path())
            .args(["worktree", "add", "-qb", "smoke"])
            .arg(&worktree)
            .status()
            .unwrap();
        assert!(created.success());

        assert_eq!(
            git_common_dir_outside_worktree(&worktree),
            Some(repo.path().join(".git").canonicalize().unwrap())
        );
        assert_eq!(git_common_dir_outside_worktree(repo.path()), None);
    }

    #[test]
    fn pull_failure_not_found_yields_hint() {
        let stderr = "Unable to find image 'ghcr.io/corvidae-coding-projects/crosslink-agent:latest' locally\nError response from daemon: manifest unknown";
        let msg = format_container_launch_error(
            "docker",
            "ghcr.io/corvidae-coding-projects/crosslink-agent:latest",
            stderr,
        );
        assert!(msg.contains("docker container launch failed"));
        assert!(msg.contains("Hint:"));
        assert!(msg.contains("just build-image"));
        assert!(msg.contains(AGENT_IMAGE_PACKAGE_URL));
        assert!(msg.contains("ghcr.io/corvidae-coding-projects/crosslink-agent:latest"));
    }

    #[test]
    fn pull_failure_denied_yields_hint() {
        let stderr = "Error response from daemon: pull access denied for some/image, repository does not exist or may require 'docker login'";
        let msg = format_container_launch_error(
            "podman",
            "ghcr.io/corvidae-coding-projects/crosslink-agent:nightly",
            stderr,
        );
        assert!(msg.contains("podman container launch failed"));
        assert!(msg.contains("Hint:"));
        assert!(msg.contains("just build-image"));
    }

    #[test]
    fn pull_failure_no_such_image_yields_hint() {
        let stderr =
            "Error: No such image: ghcr.io/corvidae-coding-projects/crosslink-agent:does-not-exist";
        let msg = format_container_launch_error(
            "docker",
            "ghcr.io/corvidae-coding-projects/crosslink-agent:does-not-exist",
            stderr,
        );
        assert!(msg.contains("Hint:"));
    }

    #[test]
    fn non_pull_failure_omits_hint() {
        let stderr = "docker: Error response from daemon: invalid mount config for type \"bind\": bind source path does not exist";
        let msg = format_container_launch_error(
            "docker",
            "ghcr.io/corvidae-coding-projects/crosslink-agent:latest",
            stderr,
        );
        assert!(msg.contains("docker container launch failed"));
        assert!(
            !msg.contains("Hint:"),
            "non-pull errors must not get the build-image hint (would misdirect): {msg}"
        );
        assert!(!msg.contains("just build-image"));
    }

    #[test]
    fn pull_failure_is_case_insensitive() {
        let stderr = "Error: NOT FOUND";
        let msg = format_container_launch_error("docker", "image:tag", stderr);
        assert!(msg.contains("Hint:"));
    }
}
