use anyhow::{bail, Result};

use super::invocation::{
    common_env_remove, AgentInvocation, ApprovalPolicy, AuthRequirement, InvocationRequest,
    OutputProtocol, SandboxPosture, StdinSource,
};
use super::{AgentProvider, ResolvedAgent};

pub(super) fn build(
    agent: &ResolvedAgent,
    request: &InvocationRequest<'_>,
) -> Result<AgentInvocation> {
    if request.policy.effort.is_some()
        || request.policy.monetary_budget_usd.is_some()
        || request.policy.approval != ApprovalPolicy::Interactive
        || request.policy.sandbox != SandboxPosture::WorkspaceWrite
        || request.output != OutputProtocol::Interactive
    {
        bail!(
            "The custom provider supports only prompt-on-stdin interactive execution; provider-specific policy or structured output was requested"
        );
    }
    Ok(AgentInvocation {
        provider: AgentProvider::Custom,
        program: agent.binary.clone(),
        args: Vec::new(),
        env_set: Vec::new(),
        env_remove: common_env_remove(),
        cwd: request.cwd.to_path_buf(),
        stdin: StdinSource::File(request.prompt_file.to_path_buf()),
        output: request.output.clone(),
        timeout: request.policy.timeout,
        sandbox: request.policy.sandbox,
        auth: AuthRequirement::None,
    })
}
