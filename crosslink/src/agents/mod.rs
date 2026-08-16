//! Provider-aware agent configuration, invocation, and event normalization.

mod claude;
mod codex;
mod config;
mod custom;
mod events;
mod invocation;

#[allow(unused_imports)]
pub use config::{resolve_agent, AgentProvider, ProviderModels, ProviderOptions, ResolvedAgent};
#[allow(unused_imports)]
pub use events::{
    parse_jsonl_event, runtime_provider, runtime_snapshot, RuntimeEvent, RuntimeEventKind,
    RuntimeSnapshot, RuntimeUsage,
};
#[allow(unused_imports)]
pub use invocation::{
    build_invocation, execute_foreground, render_shell_command, verify_account_login,
    AgentCapabilities, AgentInvocation, ApprovalPolicy, AuthRequirement, ExecutionPolicy,
    InvocationRequest, OutputProtocol, SandboxPosture, StdinSource,
};
