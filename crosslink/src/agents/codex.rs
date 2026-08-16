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
    if request.policy.monetary_budget_usd.is_some() {
        bail!(
            "Codex normal-account sessions do not expose a per-run USD budget; remove --budget-usd or select Claude"
        );
    }

    let interactive = request.output == OutputProtocol::Interactive;
    let mut args = if interactive {
        Vec::<OsString>::new()
    } else {
        Vec::<OsString>::from(["exec".into(), "-".into()])
    };
    args.extend(["--cd".into(), request.cwd.as_os_str().to_os_string()]);
    match request.policy.sandbox {
        SandboxPosture::ReadOnly => args.extend(["--sandbox".into(), "read-only".into()]),
        SandboxPosture::WorkspaceWrite => {
            args.extend(["--sandbox".into(), "workspace-write".into()]);
        }
        SandboxPosture::ExternalIsolation => {
            args.push("--dangerously-bypass-approvals-and-sandbox".into());
        }
    }
    match request.policy.approval {
        ApprovalPolicy::Interactive if interactive => {
            args.extend(["--ask-for-approval".into(), "on-request".into()]);
        }
        ApprovalPolicy::Interactive => {
            args.extend(["--config".into(), "approval_policy=\"on-request\"".into()]);
        }
        ApprovalPolicy::Never | ApprovalPolicy::DontAsk if interactive => {
            args.extend(["--ask-for-approval".into(), "never".into()]);
        }
        ApprovalPolicy::Never | ApprovalPolicy::DontAsk => {
            args.extend(["--config".into(), "approval_policy=\"never\"".into()]);
        }
        ApprovalPolicy::AutoReview | ApprovalPolicy::Automatic => {
            args.push("--approve-for-me".into());
        }
    }
    if request.verified_hook_trust {
        args.push("--dangerously-bypass-hook-trust".into());
    }
    if let Some(model) = validate_nonempty(request.model, "model")? {
        args.extend(["--model".into(), model.into()]);
    }
    if let Some(effort) = request.policy.effort.as_deref() {
        args.extend([
            "--config".into(),
            format!("model_reasoning_effort=\"{effort}\"").into(),
        ]);
    }
    match &request.output {
        OutputProtocol::JsonLines => args.push("--json".into()),
        OutputProtocol::JsonSchema(path) => args.extend([
            "--output-schema".into(),
            path.as_os_str().to_os_string(),
            "--json".into(),
        ]),
        OutputProtocol::Interactive | OutputProtocol::FinalText => {}
    }

    Ok(AgentInvocation {
        provider: AgentProvider::Codex,
        program: agent.binary.clone(),
        args,
        env_set: Vec::new(),
        env_remove: common_env_remove(),
        cwd: request.cwd.to_path_buf(),
        stdin: if interactive {
            StdinSource::PromptArgumentFile(request.prompt_file.to_path_buf())
        } else {
            StdinSource::File(request.prompt_file.to_path_buf())
        },
        output: request.output.clone(),
        timeout: request.policy.timeout,
        sandbox: request.policy.sandbox,
        auth: AuthRequirement::AccountLogin,
    })
}
