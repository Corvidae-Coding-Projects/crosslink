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
            image,
        } => {
            let path = PathBuf::from(&worktree);
            start(
                &path,
                name.as_deref(),
                prompt.as_deref(),
                issue,
                memory.as_deref(),
                image.as_deref(),
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
            crate::ContainerAuthCommands::Login { provider, image }
            | crate::ContainerAuthCommands::Refresh { provider, image } => {
                auth_login(&provider, image.as_deref())
            }
            crate::ContainerAuthCommands::Status { provider, image } => {
                auth_status(&provider, image.as_deref())
            }
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

/// An account a container can log in to: the two agent providers, and GitHub
/// for publishing hub refs from inside the container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthProvider {
    Claude,
    Codex,
    Github,
}

impl std::str::FromStr for AuthProvider {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "claude" => Ok(Self::Claude),
            "codex" => Ok(Self::Codex),
            "github" => Ok(Self::Github),
            other => {
                bail!("Container account login supports claude, codex or github, not '{other}'")
            }
        }
    }
}

impl std::fmt::Display for AuthProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Github => "github",
        })
    }
}

impl AuthProvider {
    fn volume(self) -> String {
        format!("crosslink-auth-{self}-{}", auth_scope())
    }

    /// Where the provider's CLI keeps its session inside the container.
    pub(crate) const fn mount_path(self) -> &'static str {
        match self {
            Self::Claude => "/home/agent/.claude",
            Self::Codex => "/home/agent/.codex",
            Self::Github => "/home/agent/.config/gh",
        }
    }

    fn command(self, status: bool) -> Vec<&'static str> {
        match (self, status) {
            (Self::Claude, false) => vec!["claude", "auth", "login"],
            (Self::Claude, true) => vec!["claude", "auth", "status"],
            (Self::Codex, false) => vec!["codex", "login"],
            (Self::Codex, true) => vec!["codex", "login", "status"],
            (Self::Github, false) => vec!["gh", "auth", "login"],
            (Self::Github, true) => vec!["gh", "auth", "status"],
        }
    }

    const fn agent_provider_name(self) -> Option<&'static str> {
        match self {
            Self::Claude => Some("claude"),
            Self::Codex => Some("codex"),
            Self::Github => None,
        }
    }
}

pub(crate) fn credential_volume(provider: crate::agents::AgentProvider) -> Result<String> {
    match provider {
        crate::agents::AgentProvider::Claude => Ok(AuthProvider::Claude.volume()),
        crate::agents::AgentProvider::Codex => Ok(AuthProvider::Codex.volume()),
        crate::agents::AgentProvider::Custom => {
            bail!("Container account login covers the agent providers claude and codex; a custom agent binary has no login volume")
        }
    }
}

/// The GitHub login volume kickoff and `container start` mount when it exists.
pub(crate) fn github_credential_volume() -> String {
    AuthProvider::Github.volume()
}

/// Whether the container runtime already has a named volume, so a launch can
/// mount a GitHub login without creating an empty volume by accident.
pub(crate) fn volume_exists(runtime: &str, volume: &str) -> bool {
    Command::new(runtime)
        .args(["volume", "inspect", volume])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// `HOST_UID`/`HOST_GID` for the entrypoint's user remap, so the files a
/// login container writes into a volume are owned by the same user an agent
/// container runs as; agent launches mount the login read-only and cannot
/// fix ownership themselves. Windows has no uid to forward.
pub(crate) fn host_identity_args() -> Vec<String> {
    if cfg!(target_os = "windows") {
        return Vec::new();
    }
    let id = |flag: &str| {
        Command::new("id")
            .arg(flag)
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
    };
    match (id("-u"), id("-g")) {
        (Some(uid), Some(gid)) => vec![
            "-e".to_string(),
            format!("HOST_UID={uid}"),
            "-e".to_string(),
            format!("HOST_GID={gid}"),
        ],
        _ => Vec::new(),
    }
}

/// Environment variable the launchers set when the hub has a remote, so the
/// entrypoint refuses to start an agent that could not publish.
pub(crate) const REQUIRE_GIT_LOGIN_ENV: &str = "CROSSLINK_REQUIRE_GIT_LOGIN";

/// What the hub publishes to, as far as a container's credentials go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HubPublication {
    /// No hub, or a hub with no remote: the container publishes nothing.
    Local,
    /// An HTTPS remote, which the GitHub login authenticates through
    /// `gh auth setup-git`.
    Https { remote: String, host: String },
    /// A remote the container holds no credentials for.
    Unsupported { remote: String, transport: String },
}

/// Classify a remote URL by the transport git would use for it. The URL
/// itself is never surfaced: it may carry a token in its userinfo.
fn classify_remote_url(remote: &str, url: &str) -> HubPublication {
    let url = url.trim();
    if let Some(rest) = url.strip_prefix("https://") {
        let authority = rest.split('/').next().unwrap_or_default();
        let host = authority.rsplit('@').next().unwrap_or_default();
        let host = match host.rsplit_once(':') {
            Some((name, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => {
                name
            }
            _ => host,
        };
        return HubPublication::Https {
            remote: remote.to_string(),
            host: host.to_string(),
        };
    }
    let transport = if let Some((scheme, _)) = url.split_once("://") {
        scheme.to_string()
    } else if url.starts_with('/') || url.starts_with('.') || url.starts_with('~') {
        "a local path".to_string()
    } else if let Some((before, _)) = url.split_once(':') {
        if before.len() == 1 || before.contains('/') || before.contains('\\') {
            "a local path".to_string()
        } else {
            "ssh".to_string()
        }
    } else {
        "a local path".to_string()
    };
    HubPublication::Unsupported {
        remote: remote.to_string(),
        transport,
    }
}

/// The hub's publication target, from the first initialized hub among the
/// given `.crosslink` directories (a worktree resolves to its main repository).
pub(crate) fn hub_publication(crosslink_dirs: &[&Path]) -> HubPublication {
    let Some(sync) = crosslink_dirs
        .iter()
        .filter_map(|dir| crate::sync::SyncManager::new(dir).ok())
        .find(crate::sync::SyncManager::is_initialized)
    else {
        return HubPublication::Local;
    };
    sync.remote_url().map_or(HubPublication::Local, |url| {
        classify_remote_url(sync.remote(), &url)
    })
}

fn missing_github_login_message(runtime: &str, remote: &str, host: &str, volume: &str) -> String {
    let mut message = format!(
        "the hub publishes to remote '{remote}' on {host}, which needs a GitHub login inside the container; run `crosslink container auth login --provider github` first"
    );
    if runtime != "docker" {
        use std::fmt::Write as _;
        let _ = write!(
            message,
            " (that command keeps the login in a docker volume; this launch uses {runtime}, whose volume store is separate, so create `{volume}` with {runtime} and run `gh auth login` inside a container that mounts it at {})",
            AuthProvider::Github.mount_path()
        );
    }
    message
}

/// Whether the container runtime answers at all, so a missing volume is not
/// reported as a missing login when the daemon or machine is simply down.
fn runtime_reachable(runtime: &str) -> bool {
    Command::new(runtime)
        .arg("info")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Whether the login volume holds a gh session file, probed with the agent
/// image's `test` binary (its entrypoint bypassed, so no user remap and no
/// network). `None` when the probe itself could not run.
fn login_volume_has_session(runtime: &str, volume: &str, image: &str) -> Option<bool> {
    let mount = AuthProvider::Github.mount_path();
    let status = Command::new(runtime)
        .args([
            "run",
            "--rm",
            "--entrypoint",
            "test",
            "-v",
            &format!("{volume}:{mount}:ro"),
            image,
            "-s",
            &format!("{mount}/hosts.yml"),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .ok()?;
    match status.code() {
        Some(0) => Some(true),
        Some(1) => Some(false),
        _ => None,
    }
}

/// Check, before a launch has side effects, that the hub's publication needs
/// are met: the login volume to mount when the hub has an HTTPS remote, `None`
/// when the hub publishes nothing, and an error naming the missing login or
/// the unsupported transport otherwise. With an `image`, the volume is also
/// probed for a session file, so a login that was interrupted after creating
/// the volume is caught here rather than by the entrypoint after launch.
pub(crate) fn github_login_preflight(
    runtime: &str,
    crosslink_dirs: &[&Path],
    image: Option<&str>,
) -> Result<Option<String>> {
    match hub_publication(crosslink_dirs) {
        HubPublication::Local => Ok(None),
        HubPublication::Unsupported { remote, transport } => bail!(
            "the hub publishes to remote '{remote}' over {transport}, which the container cannot authenticate: the GitHub login covers HTTPS remotes only. Point the hub remote at an HTTPS URL (`git remote set-url {remote} https://github.com/<owner>/<repo>.git`), or run kickoff with `--container none`"
        ),
        HubPublication::Https { remote, host } => {
            let volume = github_credential_volume();
            if !volume_exists(runtime, &volume) {
                if !runtime_reachable(runtime) {
                    bail!(
                        "{runtime} is installed but not reachable (is its daemon or machine running?); start it and retry"
                    );
                }
                bail!(missing_github_login_message(runtime, &remote, &host, &volume));
            }
            if let Some(image) = image {
                if login_volume_has_session(runtime, &volume, image) == Some(false) {
                    bail!(
                        "the GitHub login volume {volume} holds no gh session (an interrupted login leaves an empty volume); run `crosslink container auth login --provider github` again"
                    );
                }
            }
            Ok(Some(volume))
        }
    }
}

/// Bring a floating tag (`:nightly`, `:latest`) up to date before a launch.
/// When the registry cannot be reached, an existing local copy is used with a
/// warning, so an offline launch keeps working; without a local copy the error
/// names the reason. Pinned tags and digests are left to the runtime, which
/// pulls them once.
pub(crate) fn refresh_floating_image(runtime: &str, image: &str) -> Result<()> {
    if !crate::commands::kickoff::is_floating_image(image) {
        return Ok(());
    }
    let pull = Command::new(runtime)
        .args(["pull", "--quiet", image])
        .output()
        .with_context(|| format!("Failed to run {runtime} pull"))?;
    if pull.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&pull.stderr);
    let reason = stderr
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("no error output")
        .to_string();
    let cached = Command::new(runtime)
        .args(["image", "inspect", image])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    if cached {
        eprintln!(
            "warning: could not refresh {image} ({reason}); using the local copy, which may be out of date"
        );
        return Ok(());
    }
    bail!(
        "could not pull {image}: {reason}\nCheck network and registry access, or pass --image with an image you have locally (`just build-image` tags :local)."
    )
}

/// Pick the image: an explicit `--image` wins, then `CROSSLINK_CONTAINER_IMAGE`,
/// then this build's default (the image matching the CLI's own version).
/// Blank values count as unset. See `kickoff::resolve_agent_image_from`.
#[cfg(test)]
fn resolve_auth_image(explicit: Option<&str>, env_value: Option<&str>) -> String {
    crate::commands::kickoff::resolve_agent_image_from(explicit, env_value)
        .map_or_else(|_| DEFAULT_IMAGE.to_string(), |(image, _)| image)
}

/// The image a container command uses, and where it came from.
fn auth_image(explicit: Option<&str>) -> Result<String> {
    crate::commands::kickoff::resolve_agent_image(explicit).map(|(image, _)| image)
}

fn run_auth_container(provider: &str, status: bool, image: Option<&str>) -> Result<()> {
    if !docker_available() {
        bail!("Docker is not available.");
    }
    let parsed_provider = provider.parse::<AuthProvider>()?;
    let volume = parsed_provider.volume();
    let image = auth_image(image)?;
    let image_hint = if image == DEFAULT_IMAGE {
        String::new()
    } else {
        format!(" --image {image}")
    };
    // A status check must not create the volume as a side effect: docker
    // creates a missing named volume on `-v`, and an empty volume would then
    // pass for a login at launch time.
    if status && !volume_exists("docker", &volume) {
        bail!("{provider} container account has no login volume ({volume}); run `crosslink container auth login --provider {provider}{image_hint}`");
    }
    if !status && !io::stdin().is_terminal() {
        bail!("{provider} container account login is interactive; run `crosslink container auth login --provider {provider}{image_hint}` from a terminal");
    }
    let mut command = Command::new("docker");
    command.args(["run", "--rm"]);
    if !status {
        command.arg("-it");
    }
    command.args(["-v", &format!("{volume}:{}", parsed_provider.mount_path())]);
    command.args(host_identity_args());
    if let Some(agent_provider) = parsed_provider.agent_provider_name() {
        command.args(["-e", &format!("CROSSLINK_AGENT_PROVIDER={agent_provider}")]);
    }
    refresh_floating_image("docker", &image)?;
    command.arg(&image);
    command.args(parsed_provider.command(status));
    if status {
        let output = command
            .output()
            .context("Failed to inspect container account login")?;
        if output.status.success() {
            println!("{provider} container account is logged in (account details redacted).");
            return Ok(());
        }
        bail!("{provider} container account is not logged in; run `crosslink container auth login --provider {provider}{image_hint}`");
    }
    let result = command
        .status()
        .context("Failed to run container account login command")?;
    if !result.success() {
        bail!("{provider} account login failed");
    }
    if parsed_provider == AuthProvider::Github {
        println!(
            "GitHub login stored in volume {volume}. Agent containers that publish to an HTTPS hub remote mount it read-only, and the agent can read the token, so prefer a fine-grained token limited to the hub repository (see the container guide)."
        );
    }
    Ok(())
}

fn auth_login(provider: &str, image: Option<&str>) -> Result<()> {
    run_auth_container(provider, false, image)
}

fn auth_status(provider: &str, image: Option<&str>) -> Result<()> {
    run_auth_container(provider, true, image)
}

fn auth_logout(provider: &str, force: bool) -> Result<()> {
    let parsed_provider = provider.parse::<AuthProvider>()?;
    let volume = parsed_provider.volume();
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
    if parsed_provider == AuthProvider::Github {
        println!(
            "The GitHub token itself stays valid until you revoke it at https://github.com/settings/applications (GitHub CLI)."
        );
    }
    Ok(())
}

/// Docker arguments that give a container the GitHub login it needs to
/// publish hub refs: nothing when the hub publishes nothing; the login volume,
/// read-only, plus the entrypoint's requirement when the hub has an HTTPS
/// remote; and an error naming what is missing otherwise. Kickoff runs the
/// same check in preflight, before it creates an issue or a worktree.
pub(crate) fn github_login_args(
    runtime: &str,
    host_crosslink_dir: &Path,
    worktree_crosslink_dir: &Path,
) -> Result<Vec<String>> {
    let Some(volume) =
        github_login_preflight(runtime, &[worktree_crosslink_dir, host_crosslink_dir], None)?
    else {
        return Ok(Vec::new());
    };
    Ok(vec![
        "-v".to_string(),
        format!("{volume}:{}:ro", AuthProvider::Github.mount_path()),
        "-e".to_string(),
        format!("{REQUIRE_GIT_LOGIN_ENV}=1"),
    ])
}

const IMAGE_NAME: &str = crate::commands::kickoff::AGENT_IMAGE_REPOSITORY;

/// This build's default image: the published image matching the CLI's own
/// version (`:<version>` for releases, `:nightly` for development builds).
const DEFAULT_IMAGE: &str = crate::commands::kickoff::DEFAULT_AGENT_IMAGE;

const BUILD_DEFAULT_TAG: &str = "local";
const CONTAINER_PREFIX: &str = "crosslink-task-";
const LABEL_AGENT: &str = "crosslink-agent=true";

const DOCKERFILE: &str = include_str!("../../resources/container/Dockerfile");
pub(crate) const ENTRYPOINT: &str = include_str!("../../resources/container/entrypoint.sh");

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

/// The image label that records which crosslink an agent image contains: the
/// exact `crosslink --version` of its binary. CI and `container build` set it.
pub(crate) const IMAGE_VERSION_LABEL: &str = "org.opencontainers.image.version";

/// This CLI's version as `crosslink --version` reports it (`+<commit>` for git
/// builds).
pub(crate) fn cli_version() -> &'static str {
    option_env!("CROSSLINK_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"))
}

/// How the crosslink inside an image relates to this CLI's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ImageVersionMatch {
    /// Same version and build.
    Same,
    /// Same release version, built from a different commit: the normal case
    /// for a development build against `:nightly`.
    SameReleaseDifferentBuild,
    /// A different version: the container and the CLI write the same hub, and
    /// their formats may disagree.
    Different,
}

/// Compare two `crosslink --version` strings (`<semver>[+<build>]`).
pub(crate) fn compare_crosslink_versions(cli: &str, image: &str) -> ImageVersionMatch {
    let core = |version: &str| {
        version
            .split_once('+')
            .map_or(version, |(core, _)| core)
            .to_string()
    };
    if cli == image {
        ImageVersionMatch::Same
    } else if core(cli) == core(image) {
        ImageVersionMatch::SameReleaseDifferentBuild
    } else {
        ImageVersionMatch::Different
    }
}

/// Compare the crosslink an image contains with this CLI's, using the image's
/// version label, and say so when they differ. Nothing is printed when the
/// image is not available locally yet or carries no label (images built before
/// the label existed).
pub(crate) fn check_image_version(runtime: &str, image: &str) -> Option<ImageVersionMatch> {
    let output = Command::new(runtime)
        .args([
            "image",
            "inspect",
            "--format",
            &format!("{{{{ index .Config.Labels \"{IMAGE_VERSION_LABEL}\" }}}}"),
            image,
        ])
        .output()
        .ok()
        .filter(|output| output.status.success())?;
    let image_version = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if image_version.is_empty() || image_version == "<no value>" {
        tracing::debug!(
            "{image} carries no {IMAGE_VERSION_LABEL} label; skipping the version check"
        );
        return None;
    }
    let verdict = compare_crosslink_versions(cli_version(), &image_version);
    match verdict {
        ImageVersionMatch::Same => {}
        ImageVersionMatch::SameReleaseDifferentBuild => tracing::info!(
            "{image} contains crosslink {image_version}; this CLI is {}",
            cli_version()
        ),
        ImageVersionMatch::Different => eprintln!(
            "warning: {image} contains crosslink {image_version}, but this CLI is {}. Both write the same hub; \
             use the image matching this build, or pass --image with one built from your code (`just build-image`).",
            cli_version()
        ),
    }
    Some(verdict)
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

    println!("Building container image: {image}");

    let mut cmd = Command::new("docker");
    cmd.args(["build", "-t", &image]);

    cmd.args(["--build-arg", &format!("TARGETARCH={docker_arch}")]);
    cmd.args(["--label", LABEL_AGENT]);
    cmd.args([
        "--label",
        &format!("{IMAGE_VERSION_LABEL}={}", cli_version()),
    ]);
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
    println!("crosslink version: {}", cli_version());
    Ok(())
}

pub fn start(
    worktree_path: &Path,
    name: Option<&str>,
    prompt_file: Option<&str>,
    issue_id: Option<i64>,
    memory: Option<&str>,
    image: Option<&str>,
) -> Result<()> {
    if !docker_available() {
        bail!("Docker is not available. Install Docker and ensure the daemon is running.");
    }
    let (image, image_source) = crate::commands::kickoff::resolve_agent_image(image)?;

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

    refresh_floating_image("docker", &image)?;
    check_image_version("docker", &image);

    println!("Starting task container: {container_name}");
    println!("  Worktree: {}", worktree_abs.display());
    println!("  Memory:   {memory_limit}");
    println!("  Agent:    {agent_id}");
    println!("  Provider: {}", resolved.provider);
    println!("  Image:    {image} (from {image_source})");

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
    cmd.args(["-e", "CROSSLINK_REQUIRE_LOGIN=1"]);
    for arg in github_login_args(
        "docker",
        &repo_root.join(".crosslink"),
        &worktree_abs.join(".crosslink"),
    )? {
        cmd.arg(arg);
    }

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
    fn auth_image_resolution_prefers_flag_then_env_then_default() {
        let default = DEFAULT_IMAGE.to_string();
        assert_eq!(resolve_auth_image(None, None), default);
        assert_eq!(resolve_auth_image(Some("  "), Some("")), default);
        assert_eq!(
            resolve_auth_image(None, Some("ghcr.io/fork/crosslink-agent:nightly")),
            "ghcr.io/fork/crosslink-agent:nightly"
        );
        assert_eq!(
            resolve_auth_image(
                Some("local/crosslink-agent:dev"),
                Some("ghcr.io/fork/crosslink-agent:nightly")
            ),
            "local/crosslink-agent:dev"
        );
    }

    #[test]
    fn image_name_is_ghcr_namespaced() {
        // Fork builds may override the repository at build time.
        if std::env::var("CROSSLINK_AGENT_IMAGE_REPOSITORY").is_err() {
            assert_eq!(
                IMAGE_NAME,
                "ghcr.io/corvidae-coding-projects/crosslink-agent"
            );
        }
    }

    #[test]
    fn build_default_tag_is_distinct_from_lookup_tag() {
        assert_eq!(BUILD_DEFAULT_TAG, "local");
        assert_ne!(
            BUILD_DEFAULT_TAG, crate::commands::kickoff::AGENT_IMAGE_TAG,
            "BUILD_DEFAULT_TAG and crate::commands::kickoff::AGENT_IMAGE_TAG must differ — otherwise `crosslink container build` \
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

    #[test]
    fn container_entrypoint_requires_normal_account_login_when_requested() {
        assert!(ENTRYPOINT.contains("CROSSLINK_REQUIRE_LOGIN"));
        assert!(ENTRYPOINT.contains("claude auth status"));
        assert!(ENTRYPOINT.contains("codex login status"));
        assert!(!ENTRYPOINT.contains("API_KEY"));
    }

    #[test]
    fn github_login_is_a_provider_with_its_own_volume_and_mount() {
        let github: AuthProvider = "github".parse().unwrap();
        assert_eq!(github, AuthProvider::Github);
        assert!("gitlab".parse::<AuthProvider>().is_err());
        assert!(github_credential_volume().starts_with("crosslink-auth-github-"));
        assert_ne!(
            github_credential_volume(),
            credential_volume(crate::agents::AgentProvider::Claude).unwrap()
        );
        assert_eq!(github.mount_path(), "/home/agent/.config/gh");
        assert_eq!(github.command(false), ["gh", "auth", "login"]);
        assert_eq!(github.command(true), ["gh", "auth", "status"]);
        assert_eq!(github.agent_provider_name(), None);
        assert_eq!(AuthProvider::Codex.agent_provider_name(), Some("codex"));
        assert_eq!(
            AuthProvider::Claude.mount_path(),
            format!("/home/agent/.{}", crate::agents::AgentProvider::Claude)
        );
    }

    #[test]
    fn container_entrypoint_and_image_carry_the_github_login() {
        assert!(ENTRYPOINT.contains(REQUIRE_GIT_LOGIN_ENV));
        assert!(ENTRYPOINT.contains("hosts.yml"));
        assert!(ENTRYPOINT.contains("gh auth status"));
        assert!(ENTRYPOINT.contains("gh auth setup-git"));
        assert!(ENTRYPOINT.contains("container auth login --provider github"));
        assert!(DOCKERFILE.contains("cli.github.com/packages"));
        assert!(!ENTRYPOINT.contains("GH_TOKEN"));
    }

    #[test]
    fn remote_transports_are_classified_without_exposing_the_url() {
        for (url, host) in [
            ("https://github.com/example/hub.git", "github.com"),
            (
                "https://user:gho_secret@github.com/example/hub.git",
                "github.com",
            ),
            ("https://ghe.example.com:8443/org/hub", "ghe.example.com"),
        ] {
            assert_eq!(
                classify_remote_url("origin", url),
                HubPublication::Https {
                    remote: "origin".to_string(),
                    host: host.to_string()
                },
                "{url}"
            );
        }
        for (url, transport) in [
            ("git@github.com:example/hub.git", "ssh"),
            ("ssh://git@github.com/example/hub.git", "ssh"),
            ("git://github.com/example/hub.git", "git"),
            ("http://github.com/example/hub.git", "http"),
            ("file:///srv/hub.git", "file"),
            ("/srv/hub.git", "a local path"),
            ("../hub.git", "a local path"),
            ("C:\\hubs\\hub.git", "a local path"),
        ] {
            assert_eq!(
                classify_remote_url("origin", url),
                HubPublication::Unsupported {
                    remote: "origin".to_string(),
                    transport: transport.to_string()
                },
                "{url}"
            );
        }
    }

    /// A repository whose hub cache exists and whose tracker remote has the
    /// given URL; git never contacts it.
    fn hub_with_remote(url: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        for args in [vec!["init", "-q"], vec!["remote", "add", "origin", url]] {
            let status = Command::new("git")
                .current_dir(dir.path())
                .args(&args)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?}");
        }
        let crosslink = dir.path().join(".crosslink");
        std::fs::create_dir_all(&crosslink).unwrap();
        std::fs::write(crosslink.join("hook-config.json"), r#"{"remote":"origin"}"#).unwrap();
        let cache = crate::sync::SyncManager::new(&crosslink)
            .unwrap()
            .cache_path()
            .to_path_buf();
        std::fs::create_dir_all(cache).unwrap();
        (dir, crosslink)
    }

    #[test]
    fn github_login_preflight_needs_nothing_without_a_hub() {
        // No hub cache at all: nothing to publish, nothing required, nothing
        // mounted, and the runtime is never asked.
        let bare = tempfile::tempdir().unwrap();
        let bare_crosslink = bare.path().join(".crosslink");
        std::fs::create_dir_all(&bare_crosslink).unwrap();
        assert_eq!(
            github_login_preflight("definitely-not-a-runtime", &[&bare_crosslink], None).unwrap(),
            None
        );
        assert!(
            github_login_args("definitely-not-a-runtime", &bare_crosslink, &bare_crosslink)
                .unwrap()
                .is_empty()
        );
        // The login command is docker-only; any other runtime is told so.
        let docker = missing_github_login_message("docker", "origin", "github.com", "vol");
        assert!(!docker.contains("volume store is separate"), "{docker}");
        let podman = missing_github_login_message("podman", "origin", "github.com", "vol");
        assert!(podman.contains("volume store is separate"), "{podman}");
        assert!(podman.contains("create `vol` with podman"), "{podman}");
    }

    #[test]
    fn image_versions_compare_on_release_and_build() {
        use ImageVersionMatch::{Different, Same, SameReleaseDifferentBuild};
        let cases = [
            ("0.10.0", "0.10.0", Same),
            ("0.10.0+abc1234", "0.10.0+abc1234", Same),
            (
                "0.10.0+abc1234",
                "0.10.0+def5678",
                SameReleaseDifferentBuild,
            ),
            (
                "0.10.0+abc1234-dirty",
                "0.10.0+abc1234",
                SameReleaseDifferentBuild,
            ),
            ("0.10.0", "0.10.0+def5678", SameReleaseDifferentBuild),
            ("0.10.0", "0.9.0", Different),
            ("0.10.0-beta.1+abc", "0.10.0+abc", Different),
        ];
        for (cli, image, expected) in cases {
            assert_eq!(
                compare_crosslink_versions(cli, image),
                expected,
                "{cli} vs {image}"
            );
        }
    }

    /// A stand-in runtime whose `image inspect` prints the given label value.
    #[cfg(unix)]
    fn fake_inspect_runtime(dir: &Path, label: Option<&str>) -> String {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(format!(
            "inspect-{}",
            label.unwrap_or("missing").replace(['+', '.'], "_")
        ));
        let body = label.map_or_else(
            || "exit 1".to_string(),
            |value| format!("printf '%s\\n' '{value}'"),
        );
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.to_string_lossy().into_owned()
    }

    #[cfg(unix)]
    #[test]
    fn launch_reads_the_images_version_label() {
        let dir = tempfile::tempdir().unwrap();
        let image = format!("{IMAGE_NAME}:nightly");
        assert_eq!(
            check_image_version(
                &fake_inspect_runtime(dir.path(), Some(cli_version())),
                &image
            ),
            Some(ImageVersionMatch::Same)
        );
        assert_eq!(
            check_image_version(&fake_inspect_runtime(dir.path(), Some("0.0.1")), &image),
            Some(ImageVersionMatch::Different)
        );
        // No label (older images) or no local image: nothing to compare.
        assert_eq!(
            check_image_version(
                &fake_inspect_runtime(dir.path(), Some("<no value>")),
                &image
            ),
            None
        );
        assert_eq!(
            check_image_version(&fake_inspect_runtime(dir.path(), None), &image),
            None
        );
    }

    /// A stand-in runtime for the refresh: `pull` and `image inspect` exit
    /// with the given codes, and every call is appended to `calls.log`.
    #[cfg(unix)]
    fn fake_pull_runtime(dir: &Path, pull_rc: i32, inspect_rc: i32) -> String {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(format!("pull-{pull_rc}{inspect_rc}"));
        let log = dir.join("calls.log");
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\necho \"$*\" >> '{}'\ncase \"$1\" in\n  pull) echo 'dial tcp: lookup ghcr.io: no such host' >&2; exit {pull_rc} ;;\n  image) exit {inspect_rc} ;;\nesac\nexit 1\n",
                log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.to_string_lossy().into_owned()
    }

    #[cfg(unix)]
    #[test]
    fn floating_images_are_refreshed_and_fall_back_to_a_local_copy_offline() {
        let dir = tempfile::tempdir().unwrap();
        let nightly = format!("{IMAGE_NAME}:nightly");
        // Registry reachable: refreshed.
        refresh_floating_image(&fake_pull_runtime(dir.path(), 0, 1), &nightly).unwrap();
        // Registry unreachable, local copy present: launch continues.
        refresh_floating_image(&fake_pull_runtime(dir.path(), 1, 0), &nightly).unwrap();
        // Unreachable and nothing local: the error carries the runtime's reason.
        let error = refresh_floating_image(&fake_pull_runtime(dir.path(), 1, 1), &nightly)
            .unwrap_err()
            .to_string();
        assert!(error.contains("no such host"), "{error}");
        assert!(error.contains("--image"), "{error}");
        // A pinned tag is never pulled here.
        let calls_before = std::fs::read_to_string(dir.path().join("calls.log")).unwrap();
        refresh_floating_image(
            &fake_pull_runtime(dir.path(), 1, 1),
            &format!("{IMAGE_NAME}:0.10.0"),
        )
        .unwrap();
        let calls_after = std::fs::read_to_string(dir.path().join("calls.log")).unwrap();
        assert_eq!(calls_before, calls_after);
        assert!(
            calls_after.contains(&format!("pull --quiet {nightly}")),
            "{calls_after}"
        );
    }

    /// A stand-in container runtime: `info`, `volume inspect` and `run` exit
    /// with the given codes, so each preflight branch can be reached without
    /// a real daemon.
    #[cfg(unix)]
    fn fake_runtime(dir: &Path, info_rc: i32, volume_rc: i32, run_rc: i32) -> String {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(format!("runtime-{info_rc}{volume_rc}{run_rc}"));
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\ncase \"$1\" in\n  info) exit {info_rc} ;;\n  volume) exit {volume_rc} ;;\n  run) exit {run_rc} ;;\nesac\nexit 1\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.to_string_lossy().into_owned()
    }

    #[cfg(unix)]
    #[test]
    fn github_login_preflight_follows_the_hub_transport_and_the_login_volume() {
        let scripts = tempfile::tempdir().unwrap();
        let down = fake_runtime(scripts.path(), 1, 1, 1);
        let no_volume = fake_runtime(scripts.path(), 0, 1, 1);
        let empty_volume = fake_runtime(scripts.path(), 0, 0, 1);
        let logged_in = fake_runtime(scripts.path(), 0, 0, 0);

        // HTTPS remote: a login volume is required.
        let (_https, crosslink) = hub_with_remote("https://github.com/example/hub.git");
        let unreachable = github_login_preflight(&down, &[&crosslink], None)
            .unwrap_err()
            .to_string();
        assert!(unreachable.contains("not reachable"), "{unreachable}");
        let missing = github_login_preflight(&no_volume, &[&crosslink], None)
            .unwrap_err()
            .to_string();
        assert!(
            missing.contains("needs a GitHub login inside the container"),
            "{missing}"
        );
        assert!(missing.contains("github.com"), "{missing}");
        assert!(!missing.contains("example/hub"), "{missing}");
        // Without an image to probe with, a present volume is enough; with
        // one, an empty volume is caught before the launch.
        assert!(github_login_preflight(&empty_volume, &[&crosslink], None)
            .unwrap()
            .is_some());
        let empty = github_login_preflight(&empty_volume, &[&crosslink], Some("agent:test"))
            .unwrap_err()
            .to_string();
        assert!(empty.contains("holds no gh session"), "{empty}");
        assert!(
            github_login_preflight(&logged_in, &[&crosslink], Some("agent:test"))
                .unwrap()
                .is_some()
        );
        let args = github_login_args(&logged_in, &crosslink, &crosslink).unwrap();
        assert_eq!(args[0], "-v");
        assert!(args[1].starts_with("crosslink-auth-github-"), "{}", args[1]);
        assert!(
            args[1].ends_with(":/home/agent/.config/gh:ro"),
            "{}",
            args[1]
        );
        assert_eq!(args[2], "-e");
        assert_eq!(args[3], format!("{REQUIRE_GIT_LOGIN_ENV}=1"));
        assert_eq!(args.len(), 4);

        // SSH remote: refused before any launch, naming the transport, never the URL.
        let (_ssh, crosslink) = hub_with_remote("git@github.com:example/hub.git");
        let refused = github_login_preflight(&logged_in, &[&crosslink], None)
            .unwrap_err()
            .to_string();
        assert!(refused.contains("over ssh"), "{refused}");
        assert!(!refused.contains("example/hub"), "{refused}");
    }
}
