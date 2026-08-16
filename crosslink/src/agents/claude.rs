use anyhow::{bail, Result};
use std::ffi::OsString;

use super::invocation::{
    common_env_remove, validate_nonempty, AgentInvocation, ApprovalPolicy, AuthRequirement,
    InvocationRequest, OutputProtocol, SandboxPosture, StdinSource,
};
use super::{AgentProvider, ResolvedAgent};

pub(super) fn build(
    agent: &ResolvedAgent,
    request: &InvocationRequest<'_>,
) -> Result<AgentInvocation> {
    let mut args = Vec::<OsString>::new();
    if request.policy.sandbox == SandboxPosture::ReadOnly {
        if request.policy.approval != ApprovalPolicy::Interactive {
            bail!(
                "Claude cannot preserve read-only planning and a non-interactive approval override simultaneously; remove the permission override"
            );
        }
        args.extend(["--permission-mode".into(), "plan".into()]);
    }
    match request.policy.approval {
        ApprovalPolicy::Interactive => {}
        ApprovalPolicy::Never => args.push("--dangerously-skip-permissions".into()),
        ApprovalPolicy::AutoReview => {
            args.extend(["--permission-mode".into(), "acceptEdits".into()]);
        }
        ApprovalPolicy::Automatic => {
            args.extend(["--permission-mode".into(), "auto".into()]);
        }
        ApprovalPolicy::DontAsk => {
            args.extend(["--permission-mode".into(), "dontAsk".into()]);
        }
    }
    if let Some(model) = validate_nonempty(request.model, "model")? {
        args.extend(["--model".into(), model.into()]);
    }
    if let Some(effort) = request.policy.effort.as_deref() {
        args.extend(["--effort".into(), effort.into()]);
    }
    if let Some(budget) = request.policy.monetary_budget_usd.as_deref() {
        args.extend(["--max-budget-usd".into(), budget.into()]);
    }
    if let Some(tools) = request.allowed_tools.filter(|value| !value.is_empty()) {
        args.extend(["--allowedTools".into(), tools.into()]);
    }
    if request.output != OutputProtocol::Interactive {
        args.push("-p".into());
    }
    if matches!(request.output, OutputProtocol::JsonLines) {
        args.extend([
            "--output-format".into(),
            "stream-json".into(),
            "--verbose".into(),
        ]);
    } else if matches!(
        request.output,
        OutputProtocol::FinalText | OutputProtocol::JsonSchema(_)
    ) {
        args.extend(["--output-format".into(), "json".into()]);
    }

    let mut env_set = Vec::new();
    if let Some(path) = request.claude_config_dir.filter(|value| !value.is_empty()) {
        env_set.push(("CLAUDE_CONFIG_DIR".into(), path.into()));
    }

    Ok(AgentInvocation {
        provider: AgentProvider::Claude,
        program: agent.binary.clone(),
        args,
        env_set,
        env_remove: common_env_remove(),
        cwd: request.cwd.to_path_buf(),
        stdin: StdinSource::PromptArgumentFile(request.prompt_file.to_path_buf()),
        output: request.output.clone(),
        timeout: request.policy.timeout,
        sandbox: request.policy.sandbox,
        auth: AuthRequirement::AccountLogin,
    })
}
