use anyhow::{bail, Result};
use serde::Serialize;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::{claude, codex, custom, ResolvedAgent};

/// Provider features that Crosslink may request before any launch side effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct AgentCapabilities {
    pub structured_output: bool,
    pub jsonl_events: bool,
    pub reasoning_effort: bool,
    pub monetary_budget: bool,
    pub interactive: bool,
    pub account_auth: bool,
    pub verified_hook_trust_bypass: bool,
    pub container: bool,
}

impl super::AgentProvider {
    #[must_use]
    pub const fn capabilities(self) -> AgentCapabilities {
        match self {
            Self::Claude => AgentCapabilities {
                structured_output: true,
                jsonl_events: true,
                reasoning_effort: true,
                monetary_budget: true,
                interactive: true,
                account_auth: true,
                verified_hook_trust_bypass: false,
                container: true,
            },
            Self::Codex => AgentCapabilities {
                structured_output: true,
                jsonl_events: true,
                reasoning_effort: true,
                monetary_budget: false,
                interactive: true,
                account_auth: true,
                verified_hook_trust_bypass: true,
                container: true,
            },
            Self::Custom => AgentCapabilities {
                structured_output: false,
                jsonl_events: false,
                reasoning_effort: false,
                monetary_budget: false,
                interactive: true,
                account_auth: false,
                verified_hook_trust_bypass: false,
                container: false,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalPolicy {
    Interactive,
    Never,
    AutoReview,
    Automatic,
    DontAsk,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxPosture {
    ReadOnly,
    WorkspaceWrite,
    ExternalIsolation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionPolicy {
    pub approval: ApprovalPolicy,
    pub sandbox: SandboxPosture,
    pub effort: Option<String>,
    pub monetary_budget_usd: Option<String>,
    pub timeout: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StdinSource {
    #[allow(dead_code)]
    None,
    File(PathBuf),
    PromptArgumentFile(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputProtocol {
    Interactive,
    JsonLines,
    FinalText,
    JsonSchema(PathBuf),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthRequirement {
    AccountLogin,
    None,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentInvocation {
    pub provider: super::AgentProvider,
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub env_set: Vec<(OsString, OsString)>,
    pub env_remove: Vec<OsString>,
    pub cwd: PathBuf,
    pub stdin: StdinSource,
    pub output: OutputProtocol,
    pub timeout: Duration,
    pub sandbox: SandboxPosture,
    pub auth: AuthRequirement,
}

pub struct InvocationRequest<'a> {
    pub cwd: &'a Path,
    pub prompt_file: &'a Path,
    pub model: Option<&'a str>,
    pub allowed_tools: Option<&'a str>,
    pub policy: ExecutionPolicy,
    pub output: OutputProtocol,
    pub verified_hook_trust: bool,
    pub claude_config_dir: Option<&'a str>,
}

pub fn build_invocation(
    agent: &ResolvedAgent,
    request: &InvocationRequest<'_>,
) -> Result<AgentInvocation> {
    let capabilities = agent.provider.capabilities();
    if matches!(request.output, OutputProtocol::JsonSchema(_)) && !capabilities.structured_output {
        bail!("{} does not support structured output", agent.provider);
    }
    if request.output == OutputProtocol::JsonLines && !capabilities.jsonl_events {
        bail!("{} does not support JSONL event output", agent.provider);
    }
    if request.output == OutputProtocol::Interactive && !capabilities.interactive {
        bail!("{} does not support interactive execution", agent.provider);
    }
    if request.policy.effort.is_some() && !capabilities.reasoning_effort {
        bail!(
            "{} does not support reasoning-effort overrides",
            agent.provider
        );
    }
    if request.policy.monetary_budget_usd.is_some() && !capabilities.monetary_budget {
        bail!("{} does not support per-run USD budgets", agent.provider);
    }
    if request.verified_hook_trust && !capabilities.verified_hook_trust_bypass {
        bail!(
            "{} does not support verified hook-trust bypass",
            agent.provider
        );
    }
    match agent.provider {
        super::AgentProvider::Claude => claude::build(agent, request),
        super::AgentProvider::Codex => codex::build(agent, request),
        super::AgentProvider::Custom => custom::build(agent, request),
    }
}

/// Execute an invocation without introducing a shell, preserving terminal IO.
pub fn execute_foreground(invocation: &AgentInvocation) -> Result<std::process::ExitStatus> {
    use anyhow::Context as _;
    use std::process::{Command, Stdio};

    let mut command = Command::new(&invocation.program);
    command.args(&invocation.args).current_dir(&invocation.cwd);
    for name in &invocation.env_remove {
        command.env_remove(name);
    }
    for (name, value) in &invocation.env_set {
        command.env(name, value);
    }
    match &invocation.stdin {
        StdinSource::None => {
            command.stdin(Stdio::inherit());
        }
        StdinSource::File(path) => {
            let input = std::fs::File::open(path)
                .with_context(|| format!("Failed to open prompt input {}", path.display()))?;
            command.stdin(Stdio::from(input));
        }
        StdinSource::PromptArgumentFile(path) => {
            let prompt = std::fs::read_to_string(path)
                .with_context(|| format!("Failed to read prompt input {}", path.display()))?;
            command.arg("--").arg(prompt).stdin(Stdio::inherit());
        }
    }
    command.stdout(Stdio::inherit()).stderr(Stdio::inherit());
    command
        .status()
        .with_context(|| format!("Failed to launch {}", invocation.provider))
}

/// Verify that a known provider has a usable normal-account session without
/// reading or printing account details. Custom providers own their auth flow.
pub fn verify_account_login(agent: &ResolvedAgent) -> Result<()> {
    use std::process::{Command, Stdio};

    if !agent.provider.capabilities().account_auth {
        return Ok(());
    }
    let args: &[&str] = match agent.provider {
        super::AgentProvider::Claude => &["auth", "status"],
        super::AgentProvider::Codex => &["login", "status"],
        super::AgentProvider::Custom => unreachable!("custom account auth is capability-gated"),
    };
    let status = Command::new(&agent.binary)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|error| {
            anyhow::anyhow!(
                "Failed to inspect {} account login using {}: {error}",
                agent.provider,
                agent.binary.display()
            )
        })?;
    if !status.success() {
        bail!(
            "{} normal-account login is not ready; run `{}`",
            agent.provider,
            match agent.provider {
                super::AgentProvider::Claude => "claude auth login",
                super::AgentProvider::Codex => "codex login",
                super::AgentProvider::Custom => unreachable!(),
            }
        );
    }
    Ok(())
}

fn os_to_string(value: &std::ffi::OsStr) -> String {
    value.to_string_lossy().into_owned()
}

/// Render a structured invocation only at a tmux/container shell boundary.
#[must_use]
pub fn render_shell_command(invocation: &AgentInvocation, timeout_command: &str) -> String {
    use crate::utils::shell_escape_arg;
    use std::fmt::Write as _;

    let mut command = format!(
        "{} {}s env",
        shell_escape_arg(timeout_command),
        invocation.timeout.as_secs()
    );
    for name in &invocation.env_remove {
        let _ = write!(command, " -u {}", shell_escape_arg(&os_to_string(name)));
    }
    for (name, value) in &invocation.env_set {
        let assignment = format!("{}={}", os_to_string(name), os_to_string(value));
        let _ = write!(command, " {}", shell_escape_arg(&assignment));
    }
    let _ = write!(
        command,
        " {}",
        crate::utils::shell_escape_path(&invocation.program)
    );
    for arg in &invocation.args {
        let _ = write!(command, " {}", shell_escape_arg(&os_to_string(arg)));
    }
    match &invocation.stdin {
        StdinSource::None => {}
        StdinSource::File(path) => {
            let _ = write!(command, " < {}", crate::utils::shell_escape_path(path));
        }
        StdinSource::PromptArgumentFile(path) => {
            let _ = write!(
                command,
                " -- \"$(cat {})\"",
                crate::utils::shell_escape_path(path)
            );
        }
    }
    command
}

pub(super) fn common_env_remove() -> Vec<OsString> {
    [
        "CLAUDECODE",
        "ANTHROPIC_API_KEY",
        "OPENAI_API_KEY",
        "CODEX_API_KEY",
        "CLAUDE_CODE_OAUTH_TOKEN",
        "CODEX_ACCESS_TOKEN",
    ]
    .into_iter()
    .map(OsString::from)
    .collect()
}

pub(super) fn validate_nonempty(value: Option<&str>, option: &str) -> Result<Option<String>> {
    match value {
        Some(value) if value.trim().is_empty() => bail!("{option} cannot be empty"),
        Some(value) => Ok(Some(value.to_string())),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::{AgentProvider, ProviderModels, ProviderOptions};

    fn agent(provider: AgentProvider, binary: &str) -> ResolvedAgent {
        ResolvedAgent {
            provider,
            binary: binary.into(),
            options: ProviderOptions {
                models: ProviderModels::default(),
                sandbox: "workspace-write".into(),
                approval: "never".into(),
            },
            legacy_inferred: false,
        }
    }

    fn request<'a>(cwd: &'a Path, prompt: &'a Path) -> InvocationRequest<'a> {
        InvocationRequest {
            cwd,
            prompt_file: prompt,
            model: Some("model with spaces"),
            allowed_tools: Some("Bash,Write"),
            policy: ExecutionPolicy {
                approval: ApprovalPolicy::Never,
                sandbox: SandboxPosture::WorkspaceWrite,
                effort: Some("high".into()),
                monetary_budget_usd: None,
                timeout: Duration::from_secs(90),
            },
            output: OutputProtocol::JsonLines,
            verified_hook_trust: false,
            claude_config_dir: None,
        }
    }

    #[test]
    fn codex_invocation_is_structured_and_shell_safe() {
        let cwd = Path::new("/tmp/a repo;touch nope");
        let prompt = Path::new("/tmp/a repo/KICKOFF.md");
        let invocation = build_invocation(
            &agent(AgentProvider::Codex, "/opt/codex wrapper"),
            &request(cwd, prompt),
        )
        .unwrap();
        assert_eq!(invocation.program, PathBuf::from("/opt/codex wrapper"));
        assert!(invocation.args.iter().any(|arg| arg == "exec"));
        assert!(invocation.args.iter().any(|arg| arg == "--json"));
        let rendered = render_shell_command(&invocation, "timeout");
        assert!(rendered.contains("'/opt/codex wrapper'"));
        assert!(rendered.contains("'/tmp/a repo;touch nope'"));
        assert!(!rendered.contains(";touch nope' --"));
    }

    #[test]
    fn codex_rejects_a_fake_usd_budget() {
        let cwd = Path::new("/tmp/repo");
        let prompt = Path::new("/tmp/repo/KICKOFF.md");
        let mut req = request(cwd, prompt);
        req.policy.monetary_budget_usd = Some("5".into());
        let error = build_invocation(&agent(AgentProvider::Codex, "codex"), &req)
            .unwrap_err()
            .to_string();
        assert!(error.contains("USD budget"));
    }

    fn arg_strings(invocation: &AgentInvocation) -> Vec<String> {
        invocation
            .args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn claude_invocation_golden_covers_the_complete_contract() {
        let cwd = Path::new("/tmp/project");
        let prompt = Path::new("/tmp/project/KICKOFF.md");
        let invocation = build_invocation(
            &agent(AgentProvider::Claude, "/opt/claude"),
            &request(cwd, prompt),
        )
        .unwrap();
        assert_eq!(invocation.program, PathBuf::from("/opt/claude"));
        assert_eq!(
            arg_strings(&invocation),
            [
                "--dangerously-skip-permissions",
                "--model",
                "model with spaces",
                "--effort",
                "high",
                "--allowedTools",
                "Bash,Write",
                "-p",
                "--output-format",
                "stream-json",
                "--verbose",
            ]
        );
        assert_eq!(invocation.cwd, cwd);
        assert_eq!(
            invocation.stdin,
            StdinSource::PromptArgumentFile(prompt.into())
        );
        assert_eq!(invocation.output, OutputProtocol::JsonLines);
        assert_eq!(invocation.timeout, Duration::from_secs(90));
        assert_eq!(invocation.sandbox, SandboxPosture::WorkspaceWrite);
        assert_eq!(invocation.auth, AuthRequirement::AccountLogin);
        assert!(invocation.env_set.is_empty());
        assert!(invocation
            .env_remove
            .iter()
            .any(|name| name == "OPENAI_API_KEY"));
    }

    #[test]
    fn codex_invocation_golden_covers_exec_stdin_and_policy() {
        let cwd = Path::new("/tmp/project");
        let prompt = Path::new("/tmp/project/KICKOFF.md");
        let mut req = request(cwd, prompt);
        req.verified_hook_trust = true;
        let invocation = build_invocation(&agent(AgentProvider::Codex, "codex"), &req).unwrap();
        assert_eq!(
            arg_strings(&invocation),
            [
                "exec",
                "-",
                "--cd",
                "/tmp/project",
                "--sandbox",
                "workspace-write",
                "--config",
                "approval_policy=\"never\"",
                "--dangerously-bypass-hook-trust",
                "--model",
                "model with spaces",
                "--config",
                "model_reasoning_effort=\"high\"",
                "--json",
            ]
        );
        assert_eq!(invocation.stdin, StdinSource::File(prompt.into()));
        assert_eq!(invocation.output, OutputProtocol::JsonLines);
        assert_eq!(invocation.auth, AuthRequirement::AccountLogin);
        assert_eq!(invocation.sandbox, SandboxPosture::WorkspaceWrite);
    }

    #[test]
    fn codex_omits_account_default_model_and_maps_isolation() {
        let cwd = Path::new("/tmp/project");
        let prompt = Path::new("/tmp/project/KICKOFF.md");
        let mut req = request(cwd, prompt);
        req.model = None;
        req.policy.sandbox = SandboxPosture::ExternalIsolation;
        req.policy.approval = ApprovalPolicy::AutoReview;
        req.policy.effort = None;
        let invocation = build_invocation(&agent(AgentProvider::Codex, "codex"), &req).unwrap();
        let args = arg_strings(&invocation);
        assert!(args.contains(&"--dangerously-bypass-approvals-and-sandbox".to_string()));
        assert!(!args.contains(&"--approve-for-me".to_string()));
        assert!(!args.contains(&"--model".to_string()));
        assert!(!args
            .iter()
            .any(|arg| arg.contains("model_reasoning_effort")));
    }

    #[test]
    fn codex_auto_review_uses_its_implicit_workspace_sandbox() {
        let cwd = Path::new("/tmp/project");
        let prompt = Path::new("/tmp/project/KICKOFF.md");
        let mut req = request(cwd, prompt);
        req.policy.approval = ApprovalPolicy::AutoReview;

        let invocation = build_invocation(&agent(AgentProvider::Codex, "codex"), &req).unwrap();
        let args = arg_strings(&invocation);
        assert!(args.contains(&"--approve-for-me".to_string()));
        assert!(!args.contains(&"--sandbox".to_string()));
        assert!(!args.contains(&"approval_policy=\"never\"".to_string()));
        assert_eq!(invocation.sandbox, SandboxPosture::WorkspaceWrite);
    }

    #[test]
    fn claude_read_only_is_explicit_and_conflicts_fail_closed() {
        let cwd = Path::new("/tmp/project");
        let prompt = Path::new("/tmp/project/KICKOFF.md");
        let mut req = request(cwd, prompt);
        req.output = OutputProtocol::FinalText;
        req.policy.sandbox = SandboxPosture::ReadOnly;
        req.policy.approval = ApprovalPolicy::Interactive;
        let invocation = build_invocation(&agent(AgentProvider::Claude, "claude"), &req).unwrap();
        assert!(arg_strings(&invocation)
            .windows(2)
            .any(|pair| pair == ["--permission-mode", "plan"]));

        req.policy.approval = ApprovalPolicy::Never;
        let error = build_invocation(&agent(AgentProvider::Claude, "claude"), &req)
            .unwrap_err()
            .to_string();
        assert!(error.contains("cannot preserve read-only"));
    }

    #[test]
    fn custom_provider_golden_and_capability_errors_are_explicit() {
        let cwd = Path::new("/tmp/project");
        let prompt = Path::new("/tmp/project/KICKOFF.md");
        let mut req = request(cwd, prompt);
        req.model = None;
        req.allowed_tools = None;
        req.output = OutputProtocol::Interactive;
        req.policy.approval = ApprovalPolicy::Interactive;
        req.policy.effort = None;
        let invocation = build_invocation(&agent(AgentProvider::Custom, "my-agent"), &req).unwrap();
        assert_eq!(invocation.program, PathBuf::from("my-agent"));
        assert!(invocation.args.is_empty());
        assert_eq!(invocation.stdin, StdinSource::File(prompt.into()));
        assert_eq!(invocation.auth, AuthRequirement::None);

        req.output = OutputProtocol::JsonLines;
        let error = build_invocation(&agent(AgentProvider::Custom, "my-agent"), &req)
            .unwrap_err()
            .to_string();
        assert!(error.contains("JSONL event output"));
    }

    #[test]
    fn provider_capability_matrix_is_explicit() {
        let claude = AgentProvider::Claude.capabilities();
        assert!(claude.structured_output);
        assert!(claude.jsonl_events);
        assert!(claude.monetary_budget);
        assert!(claude.container);
        assert!(!claude.verified_hook_trust_bypass);

        let codex = AgentProvider::Codex.capabilities();
        assert!(codex.structured_output);
        assert!(codex.jsonl_events);
        assert!(codex.reasoning_effort);
        assert!(codex.verified_hook_trust_bypass);
        assert!(!codex.monetary_budget);

        let custom = AgentProvider::Custom.capabilities();
        assert!(custom.interactive);
        assert!(!custom.account_auth);
        assert!(!custom.container);
        assert!(!custom.structured_output);
    }
}
