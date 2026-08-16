use anyhow::{bail, Context, Result};
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::agents::{
    build_invocation, render_shell_command, ApprovalPolicy, ExecutionPolicy, InvocationRequest,
    OutputProtocol, ResolvedAgent, SandboxPosture,
};

use crate::ContainerCommands;

pub fn run(command: ContainerCommands) -> Result<()> {
    match command {
        ContainerCommands::Build {
            force,
            tag,
            dockerfile,
        } => build(force, tag.as_deref(), dockerfile.as_deref()),
        ContainerCommands::Start {
            worktree,
            name,
            prompt,
            issue,
            memory,
        } => {
            let path = PathBuf::from(&worktree);
            start(
                &path,
                name.as_deref(),
                prompt.as_deref(),
                issue,
                memory.as_deref(),
            )
        }
        ContainerCommands::Ps => ps(),
        ContainerCommands::Logs { name, follow, tail } => logs(&name, follow, tail),
        ContainerCommands::Stop { name } => stop(&name),
        ContainerCommands::Rm { name } => rm(&name),
        ContainerCommands::Kill { name } => kill(&name),
        ContainerCommands::Shell { name } => shell(&name),
        ContainerCommands::Snapshot { name, tag } => snapshot(&name, tag.as_deref()),
        ContainerCommands::Auth { action } => match action {
            crate::ContainerAuthCommands::Login { provider }
            | crate::ContainerAuthCommands::Refresh { provider } => auth_login(&provider),
            crate::ContainerAuthCommands::Status { provider } => auth_status(&provider),
            crate::ContainerAuthCommands::Logout { provider, force } => {
                auth_logout(&provider, force)
            }
        },
    }
}

fn normalize_auth_scope(raw: &str) -> String {
    let normalized: String = raw
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '-'
            }
        })
        .collect();
    if normalized.is_empty() {
        "default".to_string()
    } else {
        normalized
    }
}

fn auth_scope() -> String {
    let raw = std::env::var("CROSSLINK_AUTH_SCOPE")
        .ok()
        .or_else(|| std::env::var("UID").ok())
        .or_else(|| {
            if cfg!(windows) {
                std::env::var("USERNAME").ok()
            } else {
                Command::new("id")
                    .arg("-u")
                    .output()
                    .ok()
                    .and_then(|output| {
                        output
                            .status
                            .success()
                            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
                    })
            }
        })
        .unwrap_or_else(|| "default".to_string());
    normalize_auth_scope(&raw)
}

pub(crate) fn credential_volume(provider: crate::agents::AgentProvider) -> Result<String> {
    match provider {
        crate::agents::AgentProvider::Claude | crate::agents::AgentProvider::Codex => {
            Ok(format!("crosslink-auth-{provider}-{}", auth_scope()))
        }
        crate::agents::AgentProvider::Custom => {
            bail!("Container account login supports only claude or codex")
        }
    }
}

fn auth_command(provider: &str, status: bool) -> Vec<&'static str> {
    match (provider, status) {
        ("claude", false) => vec!["claude", "auth", "login"],
        ("claude", true) => vec!["claude", "auth", "status"],
        ("codex", false) => vec!["codex", "login"],
        ("codex", true) => vec!["codex", "login", "status"],
        _ => unreachable!(),
    }
}

fn run_auth_container(provider: &str, status: bool) -> Result<()> {
    if !docker_available() {
        bail!("Docker is not available.");
    }
    let parsed_provider = provider.parse::<crate::agents::AgentProvider>()?;
    let volume = credential_volume(parsed_provider)?;
    let image = format!("{IMAGE_NAME}:{IMAGE_TAG}");
    let mut command = Command::new("docker");
    command.args(["run", "--rm"]);
    if !status {
        command.arg("-it");
    }
    command.args([
        "-v",
        &format!("{volume}:/home/agent/.{provider}"),
        "-e",
        &format!("CROSSLINK_AGENT_PROVIDER={provider}"),
        &image,
    ]);
    command.args(auth_command(provider, status));
    if status {
        let output = command
            .output()
            .context("Failed to inspect container account login")?;
        if output.status.success() {
            println!("{provider} container account is logged in (account details redacted).");
            return Ok(());
        }
        bail!("{provider} container account is not logged in; run `crosslink container auth login --provider {provider}`");
    }
    let result = command
        .status()
        .context("Failed to run container account login command")?;
    if !result.success() {
        bail!(
            "{provider} account {} failed",
            if status { "status" } else { "login" }
        );
    }
    Ok(())
}

fn auth_login(provider: &str) -> Result<()> {
    run_auth_container(provider, false)
}

fn auth_status(provider: &str) -> Result<()> {
    run_auth_container(provider, true)
}

fn auth_logout(provider: &str, force: bool) -> Result<()> {
    let parsed_provider = provider.parse::<crate::agents::AgentProvider>()?;
    let volume = credential_volume(parsed_provider)?;
    if !force {
        if !io::stdin().is_terminal() {
            bail!("Refusing to remove credential volume {volume} without confirmation; rerun with --force");
        }
        print!("Remove {provider} account credentials from volume {volume}? [y/N] ");
        io::stdout().flush().ok();
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        if !answer.trim().eq_ignore_ascii_case("y") {
            println!("Credentials preserved.");
            return Ok(());
        }
    }
    let result = Command::new("docker")
        .args(["volume", "rm", &volume])
        .status()
        .context("Failed to remove credential volume")?;
    if !result.success() {
        bail!("Could not remove credential volume {volume}");
    }
    println!("Removed {provider} container account credentials ({volume}).");
    Ok(())
}

const IMAGE_NAME: &str = "ghcr.io/corvidae-coding-projects/crosslink-agent";

const IMAGE_TAG: &str = "latest";

const BUILD_DEFAULT_TAG: &str = "local";
const CONTAINER_PREFIX: &str = "crosslink-task-";
const LABEL_AGENT: &str = "crosslink-agent=true";

const DOCKERFILE: &str = include_str!("../../resources/container/Dockerfile");
const ENTRYPOINT: &str = include_str!("../../resources/container/entrypoint.sh");

pub fn docker_available() -> bool {
    Command::new("docker")
        .args(["info"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn find_crosslink_binary() -> Result<PathBuf> {
    std::env::current_exe().context("Could not determine crosslink binary path")
}

fn file_hash(path: &Path) -> Result<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;

    let mut buf = vec![0u8; 65536];
    let n = file.read(&mut buf)?;
    buf.truncate(n);

    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in &buf {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    Ok(format!("{hash:016x}"))
}

fn resolve_repo_root() -> Result<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .context("Failed to run git rev-parse")?;
    if !output.status.success() {
        bail!("Not in a git repository");
    }
    let path = String::from_utf8(output.stdout)?.trim().to_string();
    Ok(PathBuf::from(path))
}

fn resolve_git_common_dir() -> Result<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--git-common-dir"])
        .output()
        .context("Failed to run git rev-parse --git-common-dir")?;
    if !output.status.success() {
        bail!("Not in a git repository");
    }
    let path_str = String::from_utf8(output.stdout)?.trim().to_string();
    let path = PathBuf::from(&path_str);

    if path.is_absolute() {
        Ok(path)
    } else {
        let cwd = std::env::current_dir()?;
        Ok(cwd.join(path).canonicalize()?)
    }
}

fn detect_host_memory_gb() -> Option<u64> {
    if let Ok(content) = std::fs::read_to_string("/proc/meminfo") {
        for line in content.lines() {
            if line.starts_with("MemTotal:") {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 {
                    if let Ok(kb) = parts[1].parse::<u64>() {
                        return Some(kb / 1024 / 1024);
                    }
                }
            }
        }
    }

    let output = Command::new("sysctl")
        .args(["-n", "hw.memsize"])
        .output()
        .ok()?;
    if output.status.success() {
        let bytes_str = String::from_utf8(output.stdout).ok()?.trim().to_string();
        let bytes: u64 = bytes_str.parse().ok()?;
        return Some(bytes / 1024 / 1024 / 1024);
    }
    None
}

fn compute_memory_limit(config_override: Option<&str>) -> String {
    if let Some(val) = config_override {
        if val != "auto" {
            return val.to_string();
        }
    }
    detect_host_memory_gb().map_or_else(
        || "8g".to_string(),
        |host_gb| {
            let container_gb = if host_gb > 6 {
                host_gb - 2
            } else {
                4.max(host_gb)
            };
            format!("{container_gb}g")
        },
    )
}

fn get_image_hash() -> Option<String> {
    let output = Command::new("docker")
        .args([
            "inspect",
            "--format",
            "{{index .Config.Labels \"crosslink-binary-hash\"}}",
            &format!("{IMAGE_NAME}:{IMAGE_TAG}"),
        ])
        .output()
        .ok()?;
    if output.status.success() {
        let hash = String::from_utf8(output.stdout).ok()?.trim().to_string();
        if !hash.is_empty() && hash != "<no value>" {
            return Some(hash);
        }
    }
    None
}

fn check_staleness() {
    let Ok(binary_hash) = find_crosslink_binary().and_then(|p| file_hash(&p)) else {
        return;
    };
    if let Some(image_hash) = get_image_hash() {
        if image_hash != binary_hash {
            tracing::warn!(
                "container image {IMAGE_NAME}:{IMAGE_TAG} is stale relative to your installed crosslink binary. \
                 Pull the latest published image (`docker pull {IMAGE_NAME}:{IMAGE_TAG}`) or rebuild locally (`just build-image` or `crosslink container build`)."
            );
        }
    }
}

struct BuildDirCleanup(PathBuf);
impl Drop for BuildDirCleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn build(force: bool, tag: Option<&str>, dockerfile: Option<&str>) -> Result<()> {
    if !docker_available() {
        bail!("Docker is not available. Install Docker and ensure the daemon is running.");
    }

    let tag = tag.unwrap_or(BUILD_DEFAULT_TAG);
    let image = format!("{IMAGE_NAME}:{tag}");

    let build_path =
        std::env::temp_dir().join(format!("crosslink-container-build-{}", std::process::id()));
    std::fs::create_dir_all(&build_path).context("Failed to create temp build directory")?;

    let _cleanup = BuildDirCleanup(build_path.clone());

    let dockerfile_content = if let Some(path) = dockerfile {
        std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read custom Dockerfile: {path}"))?
    } else {
        DOCKERFILE.to_string()
    };
    std::fs::write(build_path.join("Dockerfile"), &dockerfile_content)?;

    std::fs::write(build_path.join("entrypoint.sh"), ENTRYPOINT)?;

    let docker_arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => bail!(
            "unsupported host architecture `{other}` for `crosslink container build`; \
             build the image via CI (.github/workflows/container-image.yml) or `just build-image`"
        ),
    };
    if !cfg!(target_os = "linux") {
        bail!(
            "`crosslink container build` packages the installed crosslink binary, which must \
             be a Linux binary to run in the agent image — but this host is `{}`. Build on a \
             Linux host, or use the CI workflow (.github/workflows/container-image.yml) or \
             `just build-image`, which cross-compile a static musl binary.",
            std::env::consts::OS
        );
    }

    let binary = find_crosslink_binary()?;
    let staged_binary = format!("crosslink-{docker_arch}");
    std::fs::copy(&binary, build_path.join(&staged_binary))
        .context("Failed to copy crosslink binary to build context")?;

    let binary_hash = file_hash(&binary).unwrap_or_else(|_| "unknown".to_string());

    println!("Building container image: {image}");

    let mut cmd = Command::new("docker");
    cmd.args(["build", "-t", &image]);

    cmd.args(["--build-arg", &format!("TARGETARCH={docker_arch}")]);
    cmd.args(["--label", LABEL_AGENT]);
    cmd.args(["--label", &format!("crosslink-binary-hash={binary_hash}")]);
    if force {
        cmd.arg("--no-cache");
    }
    cmd.arg(".");
    cmd.current_dir(build_path);

    let status = cmd.status().context("Failed to run docker build")?;
    if !status.success() {
        bail!("Docker build failed");
    }

    println!("Image built successfully: {image}");
    println!("Binary hash: {binary_hash}");
    Ok(())
}

pub fn start(
    worktree_path: &Path,
    name: Option<&str>,
    prompt_file: Option<&str>,
    issue_id: Option<i64>,
    memory: Option<&str>,
) -> Result<()> {
    if !docker_available() {
        bail!("Docker is not available. Install Docker and ensure the daemon is running.");
    }

    check_staleness();

    let worktree_abs = std::fs::canonicalize(worktree_path)
        .with_context(|| format!("Worktree not found: {}", worktree_path.display()))?;

    let worktree_slug = worktree_abs
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");
    let container_name = name.map_or_else(
        || format!("{CONTAINER_PREFIX}{worktree_slug}"),
        ToString::to_string,
    );

    let git_common_dir = resolve_git_common_dir()?;
    let repo_root = resolve_repo_root()?;
    let hub_cache = repo_root.join(".crosslink").join(".hub-cache");

    let prompt_path = prompt_file.map_or_else(|| worktree_abs.join("KICKOFF.md"), PathBuf::from);
    if !prompt_path.exists() {
        bail!(
            "Prompt file not found: {}. Write a KICKOFF.md in the worktree first.",
            prompt_path.display()
        );
    }
    let prompt_abs = std::fs::canonicalize(&prompt_path)
        .with_context(|| format!("Could not resolve prompt file {}", prompt_path.display()))?;

    let resolved = crate::agents::resolve_agent(&worktree_abs.join(".crosslink"))?;
    if !resolved.provider.capabilities().container {
        bail!("{} does not support container execution", resolved.provider);
    }
    let container_agent = ResolvedAgent {
        provider: resolved.provider,
        binary: resolved
            .provider
            .default_binary()
            .map_or_else(|| resolved.binary.clone(), PathBuf::from),
        options: resolved.options.clone(),
        legacy_inferred: resolved.legacy_inferred,
    };

    let credentials = credential_volume(resolved.provider)?;
    let container_workspace = PathBuf::from(format!("/workspaces/{worktree_slug}"));
    let container_prompt = prompt_abs.strip_prefix(&worktree_abs).map_or_else(
        |_| PathBuf::from("/tmp/crosslink-prompt.md"),
        |relative| container_workspace.join(relative),
    );
    let timeout = Duration::from_secs(3600);
    let model = container_agent.resolve_model(Some("standard"));
    let invocation = build_invocation(
        &container_agent,
        &InvocationRequest {
            cwd: &container_workspace,
            prompt_file: &container_prompt,
            model: model.as_deref(),
            allowed_tools: None,
            policy: ExecutionPolicy {
                approval: ApprovalPolicy::Never,
                sandbox: SandboxPosture::ExternalIsolation,
                effort: None,
                monetary_budget_usd: None,
                timeout,
            },
            output: OutputProtocol::JsonLines,
            verified_hook_trust: resolved.provider == crate::agents::AgentProvider::Codex
                && crate::commands::init::codex_hook_trust_ready(&worktree_abs)?,
            claude_config_dir: None,
        },
    )?;
    let agent_command = render_shell_command(&invocation, "timeout");

    let memory_limit = compute_memory_limit(memory);

    let agent_id = format!("container--{worktree_slug}");

    let image = format!("{IMAGE_NAME}:{IMAGE_TAG}");

    println!("Starting task container: {container_name}");
    println!("  Worktree: {}", worktree_abs.display());
    println!("  Memory:   {memory_limit}");
    println!("  Agent:    {agent_id}");
    println!("  Provider: {}", resolved.provider);

    let mut cmd = Command::new("docker");
    cmd.args(["run", "-d"]);
    cmd.args(["--name", &container_name]);
    cmd.args(["--label", LABEL_AGENT]);
    cmd.args(["--label", &format!("crosslink-task={worktree_slug}")]);
    if let Some(id) = issue_id {
        cmd.args(["--label", &format!("crosslink-issue={id}")]);
    }
    cmd.args(["--memory", &memory_limit]);

    cmd.args([
        "-v",
        &format!("{}:/workspaces/{}", worktree_abs.display(), worktree_slug),
    ]);
    if prompt_abs.strip_prefix(&worktree_abs).is_err() {
        let target = PathBuf::from("/tmp/crosslink-prompt.md");
        cmd.args([
            "-v",
            &format!("{}:{}:ro", prompt_abs.display(), target.display()),
        ]);
    }

    cmd.args(["-v", &format!("{}:/repo/.git:rw", git_common_dir.display())]);

    let dot_git_path = worktree_abs.join(".git");
    if dot_git_path.is_file() {
        let fixup_dir = worktree_abs.join(".crosslink").join("container-git-fixup");
        std::fs::create_dir_all(&fixup_dir).context("Failed to create git fixup dir")?;

        let container_workspace = format!("/workspaces/{worktree_slug}");
        let container_gitdir = format!("/repo/.git/worktrees/{worktree_slug}");

        let override_dot_git = fixup_dir.join("dot-git");
        std::fs::write(&override_dot_git, format!("gitdir: {container_gitdir}\n"))?;

        let override_gitdir = fixup_dir.join("gitdir");
        std::fs::write(&override_gitdir, format!("{container_workspace}/.git\n"))?;

        cmd.args([
            "-v",
            &format!(
                "{}:{}/.git:ro",
                override_dot_git.display(),
                container_workspace
            ),
        ]);
        cmd.args([
            "-v",
            &format!(
                "{}:{}/gitdir:ro",
                override_gitdir.display(),
                container_gitdir
            ),
        ]);
    }

    if hub_cache.exists() {
        cmd.args([
            "-v",
            &format!("{}:/repo/.crosslink/.hub-cache:rw", hub_cache.display()),
        ]);
    }

    cmd.args([
        "-v",
        &format!("{credentials}:/home/agent/.{}", resolved.provider),
    ]);

    cmd.args(["-e", &format!("AGENT_ID={agent_id}")]);
    cmd.args([
        "-e",
        &format!("CROSSLINK_AGENT_PROVIDER={}", resolved.provider),
    ]);

    if let Ok(uid_output) = Command::new("id").arg("-u").output() {
        if uid_output.status.success() {
            let uid = String::from_utf8_lossy(&uid_output.stdout)
                .trim()
                .to_string();
            cmd.args(["-e", &format!("HOST_UID={uid}")]);
        }
    }
    if let Ok(gid_output) = Command::new("id").arg("-g").output() {
        if gid_output.status.success() {
            let gid = String::from_utf8_lossy(&gid_output.stdout)
                .trim()
                .to_string();
            cmd.args(["-e", &format!("HOST_GID={gid}")]);
        }
    }

    cmd.arg(&image);
    let workspace_arg = crate::utils::shell_escape_arg(&container_workspace.to_string_lossy());
    let runtime_dir = crate::utils::shell_escape_arg(
        &container_workspace
            .join(".crosslink/runtime")
            .to_string_lossy(),
    );
    let raw_log = crate::utils::shell_escape_arg(
        &container_workspace
            .join(".crosslink/runtime/agent-events.jsonl")
            .to_string_lossy(),
    );
    let status_file = crate::utils::shell_escape_arg(
        &container_workspace
            .join(".kickoff-status")
            .to_string_lossy(),
    );
    cmd.args([
        "bash",
        "-o",
        "pipefail",
        "-c",
        &format!(
            "cd {workspace_arg} && mkdir -p {runtime_dir} && \
             {agent_command} 2>&1 | tee -a {raw_log}; \
             code=${{PIPESTATUS[0]}}; \
             if [ \"$code\" -eq 124 ]; then printf 'TIMEOUT\\n' > {status_file}; \
             elif [ \"$code\" -ne 0 ]; then printf 'FAILED\\n' > {status_file}; \
             elif [ ! -s {status_file} ]; then printf 'DONE\\n' > {status_file}; fi; \
             exit \"$code\""
        ),
    ]);

    let output = cmd.output().context("Failed to start container")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("Failed to start container: {}", stderr.trim());
    }

    let container_id = String::from_utf8_lossy(&output.stdout).trim().to_string();
    println!(
        "  Container ID: {}...",
        &container_id[..12.min(container_id.len())]
    );

    let id_file = worktree_abs.join(".crosslink").join("container-id");
    if let Some(parent) = id_file.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(&id_file, &container_id).ok();

    println!();
    println!("Task container started.");
    println!("  Check status: crosslink container ps");
    println!("  View logs:    crosslink container logs {container_name}");
    println!("  Shell in:     crosslink container shell {container_name}");
    println!("  Stop:         crosslink container stop {container_name}");

    Ok(())
}

pub fn ps() -> Result<()> {
    if !docker_available() {
        bail!("Docker is not available.");
    }

    let output = Command::new("docker")
        .args([
            "ps",
            "-a",
            "--filter",
            &format!("label={LABEL_AGENT}"),
            "--format",
            "table {{.Names}}\t{{.Status}}\t{{.Label \"crosslink-task\"}}\t{{.Label \"crosslink-issue\"}}",
        ])
        .output()
        .context("Failed to list containers")?;

    if !output.status.success() {
        bail!("Failed to list containers");
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    if stdout.trim().is_empty() || stdout.lines().count() <= 1 {
        println!("No crosslink task containers found.");
    } else {
        print!("{stdout}");
    }
    Ok(())
}

pub fn logs(name: &str, follow: bool, tail: Option<u32>) -> Result<()> {
    if !docker_available() {
        bail!("Docker is not available.");
    }

    let mut cmd = Command::new("docker");
    cmd.args(["logs"]);
    if follow {
        cmd.arg("--follow");
    }
    let tail_str = tail.unwrap_or(100).to_string();
    cmd.args(["--tail", &tail_str]);
    cmd.arg(name);

    let status = cmd.status().context("Failed to read container logs")?;
    if !status.success() {
        bail!("Failed to read logs for container '{name}'. Does it exist?");
    }
    Ok(())
}

pub fn stop(name: &str) -> Result<()> {
    if !docker_available() {
        bail!("Docker is not available.");
    }

    println!("Stopping container: {name}");
    let status = Command::new("docker")
        .args(["stop", name])
        .status()
        .context("Failed to stop container")?;

    if !status.success() {
        bail!("Failed to stop container '{name}'");
    }
    println!("Container stopped.");
    Ok(())
}

pub fn rm(name: &str) -> Result<()> {
    if !docker_available() {
        bail!("Docker is not available.");
    }

    println!("Removing container: {name}");
    let status = Command::new("docker")
        .args(["rm", name])
        .status()
        .context("Failed to remove container")?;

    if !status.success() {
        bail!("Failed to remove container '{name}'");
    }
    println!("Container removed.");
    Ok(())
}

pub fn kill(name: &str) -> Result<()> {
    if !docker_available() {
        bail!("Docker is not available.");
    }

    println!("Stopping and removing container: {name}");

    let _ = Command::new("docker").args(["stop", name]).status();
    let status = Command::new("docker")
        .args(["rm", "-f", name])
        .status()
        .context("Failed to remove container")?;

    if !status.success() {
        bail!("Failed to remove container '{name}'");
    }
    println!("Container removed.");
    Ok(())
}

pub fn shell(name: &str) -> Result<()> {
    if !docker_available() {
        bail!("Docker is not available.");
    }

    let status = Command::new("docker")
        .args(["exec", "-it", name, "/bin/bash"])
        .status()
        .context("Failed to exec into container")?;

    if !status.success() {
        bail!("Shell exited with error");
    }
    Ok(())
}

pub fn snapshot(name: &str, tag: Option<&str>) -> Result<()> {
    if !docker_available() {
        bail!("Docker is not available.");
    }

    let tag = tag.unwrap_or("cached");
    let image = format!("{IMAGE_NAME}:{tag}");

    println!("Snapshotting container '{name}' as '{image}'...");
    let status = Command::new("docker")
        .args(["commit", name, &image])
        .status()
        .context("Failed to snapshot container")?;

    if !status.success() {
        bail!("Failed to snapshot container '{name}'");
    }
    println!("Snapshot saved: {image}");
    println!("Use with: crosslink container start --image {image}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_name_is_ghcr_namespaced() {
        assert_eq!(
            IMAGE_NAME,
            "ghcr.io/corvidae-coding-projects/crosslink-agent"
        );
        assert_eq!(
            IMAGE_NAME,
            crate::commands::kickoff::DEFAULT_AGENT_IMAGE
                .rsplit_once(':')
                .map_or(IMAGE_NAME, |(name, _)| name),
            "container.rs IMAGE_NAME diverged from kickoff DEFAULT_AGENT_IMAGE — \
             re-opens the GH#576 compose-failure between `crosslink container build` \
             and `crosslink kickoff run --container …`"
        );
    }

    #[test]
    fn build_default_tag_is_distinct_from_lookup_tag() {
        assert_eq!(BUILD_DEFAULT_TAG, "local");
        assert_ne!(
            BUILD_DEFAULT_TAG, IMAGE_TAG,
            "BUILD_DEFAULT_TAG and IMAGE_TAG must differ — otherwise `crosslink container build` \
             clobbers the published `:latest` users pulled from GHCR"
        );
    }

    #[test]
    fn provider_credentials_are_isolated_and_scope_is_volume_safe() {
        assert_eq!(normalize_auth_scope("user/name:42"), "user-name-42");
        assert_eq!(normalize_auth_scope("***"), "---");
        let claude = credential_volume(crate::agents::AgentProvider::Claude).unwrap();
        let codex = credential_volume(crate::agents::AgentProvider::Codex).unwrap();
        assert!(claude.starts_with("crosslink-auth-claude-"));
        assert!(codex.starts_with("crosslink-auth-codex-"));
        assert_ne!(claude, codex);
        assert!(credential_volume(crate::agents::AgentProvider::Custom).is_err());
    }
}
