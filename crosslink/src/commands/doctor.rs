use anyhow::Result;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::agents::{resolve_agent, AgentCapabilities, AgentProvider};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct IntegrationStatus {
    claude_settings: bool,
    claude_mcp: bool,
    claude_skills: bool,
    codex_hooks: bool,
    codex_mcp: bool,
    codex_skills: bool,
    agents_instructions: bool,
    shared_hooks: bool,
    shared_mcp: bool,
}

#[derive(Debug, Serialize)]
struct DoctorReport {
    provider: AgentProvider,
    capabilities: AgentCapabilities,
    binary: String,
    binary_version: Option<String>,
    binary_available: bool,
    login_status: String,
    integrations: IntegrationStatus,
    plugin_present: bool,
    hook_trust_ready: bool,
    container_credential_volume: Option<String>,
    container_credentials_present: Option<bool>,
    warnings: Vec<String>,
}

fn account_home() -> Option<PathBuf> {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .map(|path| path.join(".codex"))
        })
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .map(PathBuf::from)
                .map(|path| path.join(".codex"))
        })
}

fn command_status(binary: &Path, args: &[&str]) -> Option<bool> {
    Command::new(binary)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()
        .map(|status| status.success())
}

fn binary_version(binary: &Path) -> Option<String> {
    let output = Command::new(binary).arg("--version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
}

fn login_status(provider: AgentProvider, binary: &Path) -> String {
    let args: &[&str] = match provider {
        AgentProvider::Claude => &["auth", "status"],
        AgentProvider::Codex => &["login", "status"],
        AgentProvider::Custom => return "not-applicable".to_string(),
    };
    match command_status(binary, args) {
        Some(true) => "logged-in",
        Some(false) => "not-logged-in",
        None => "unknown",
    }
    .to_string()
}

fn codex_plugin_present(binary: &Path) -> bool {
    if let Some(home) = account_home() {
        for relative in ["plugins/crosslink-codex", "plugins/cache/crosslink-codex"] {
            if home.join(relative).exists() {
                return true;
            }
        }
    }
    Command::new(binary)
        .args(["plugin", "list"])
        .stdin(Stdio::null())
        .output()
        .ok()
        .filter(|output| output.status.success())
        .is_some_and(|output| {
            String::from_utf8_lossy(&output.stdout)
                .to_ascii_lowercase()
                .contains("crosslink-codex")
        })
}

fn file_contains_all(path: &Path, needles: &[&str]) -> bool {
    std::fs::read_to_string(path)
        .is_ok_and(|content| needles.iter().all(|needle| content.contains(needle)))
}

fn integration_warnings(
    provider: AgentProvider,
    integrations: &IntegrationStatus,
    plugin_present: bool,
    hook_trust_ready: bool,
) -> Vec<String> {
    let mut warnings = Vec::new();
    let missing = match provider {
        AgentProvider::Claude => [
            (!integrations.claude_settings, ".claude/settings.json"),
            (!integrations.claude_mcp, ".mcp.json Crosslink servers"),
            (!integrations.claude_skills, ".claude/skills"),
            (!integrations.shared_hooks, "shared hook scripts"),
            (!integrations.shared_mcp, "shared MCP scripts"),
        ]
        .into_iter()
        .filter_map(|(is_missing, name)| is_missing.then_some(name))
        .collect::<Vec<_>>(),
        AgentProvider::Codex => [
            (!integrations.codex_hooks, ".codex/hooks.json"),
            (
                !integrations.codex_mcp,
                ".codex/config.toml Crosslink servers",
            ),
            (!integrations.codex_skills, ".agents/skills"),
            (!integrations.agents_instructions, "AGENTS.md"),
            (!integrations.shared_hooks, "shared hook scripts"),
            (!integrations.shared_mcp, "shared MCP scripts"),
        ]
        .into_iter()
        .filter_map(|(is_missing, name)| is_missing.then_some(name))
        .collect::<Vec<_>>(),
        AgentProvider::Custom => Vec::new(),
    };
    if !missing.is_empty() {
        warnings.push(format!(
            "{} integration is incomplete (missing {}); run `crosslink init --update --agent-integration {}`",
            provider,
            missing.join(", "),
            provider
        ));
    }
    if provider == AgentProvider::Codex && integrations.codex_hooks && !hook_trust_ready {
        warnings.push(
            "Codex hook definitions are untrusted or differ from the init manifest; review them with `/hooks`"
                .to_string(),
        );
    }
    if provider == AgentProvider::Codex && plugin_present && integrations.codex_hooks {
        warnings.push(
            "Codex project and plugin hooks are both enabled; Crosslink event deduplication will prevent duplicate effects"
                .to_string(),
        );
    }
    warnings
}

pub fn run(crosslink_dir: &Path, json: bool) -> Result<()> {
    let project_root = crosslink_dir.parent().unwrap_or(crosslink_dir);
    let agent = resolve_agent(crosslink_dir)?;
    let version = binary_version(&agent.binary);
    let binary_available = version.is_some();
    let login = if binary_available {
        login_status(agent.provider, &agent.binary)
    } else {
        "binary-missing".to_string()
    };
    let volume = crate::commands::container::credential_volume(agent.provider).ok();
    let volume_present = volume
        .as_ref()
        .and_then(|name| command_status(Path::new("docker"), &["volume", "inspect", name]));
    let integrations = IntegrationStatus {
        claude_settings: project_root.join(".claude/settings.json").is_file(),
        claude_mcp: file_contains_all(
            &project_root.join(".mcp.json"),
            &["crosslink-knowledge", "crosslink-agent-prompt"],
        ),
        claude_skills: project_root.join(".claude/skills").is_dir(),
        codex_hooks: project_root.join(".codex/hooks.json").is_file(),
        codex_mcp: file_contains_all(
            &project_root.join(".codex/config.toml"),
            &["crosslink-knowledge", "crosslink-agent-prompt"],
        ),
        codex_skills: project_root.join(".agents/skills").is_dir(),
        agents_instructions: project_root.join("AGENTS.md").is_file(),
        shared_hooks: ["hook_protocol.py", "work-check.py", "session-start.py"]
            .iter()
            .all(|name| {
                crosslink_dir
                    .join("integrations/hooks")
                    .join(name)
                    .is_file()
            }),
        shared_mcp: ["knowledge-server.py", "agent-prompt-server.py"]
            .iter()
            .all(|name| crosslink_dir.join("integrations/mcp").join(name).is_file()),
    };
    let plugin_present =
        agent.provider == AgentProvider::Codex && codex_plugin_present(&agent.binary);
    let hook_trust_ready =
        crate::commands::init::codex_hook_trust_ready(project_root).unwrap_or(false);
    let mut warnings = Vec::new();
    if !binary_available {
        warnings.push(format!(
            "configured {} binary is not executable",
            agent.provider
        ));
    }
    if agent.provider == AgentProvider::Custom {
        warnings.push(
            "custom providers support interactive prompt-on-stdin only; autonomous, structured-output, effort, budget, and container options are unsupported"
                .to_string(),
        );
    }
    if agent.provider != AgentProvider::Custom && login != "logged-in" {
        warnings.push(format!(
            "{} normal-account login is not ready; run `{}`",
            agent.provider,
            if agent.provider == AgentProvider::Codex {
                "codex login"
            } else {
                "claude auth login"
            }
        ));
    }
    warnings.extend(integration_warnings(
        agent.provider,
        &integrations,
        plugin_present,
        hook_trust_ready,
    ));
    let report = DoctorReport {
        provider: agent.provider,
        capabilities: agent.provider.capabilities(),
        binary: agent.binary.display().to_string(),
        binary_version: version,
        binary_available,
        login_status: login,
        integrations,
        plugin_present,
        hook_trust_ready,
        container_credential_volume: volume,
        container_credentials_present: volume_present,
        warnings,
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("Crosslink provider diagnostics");
        println!("  provider:             {}", report.provider);
        println!("  binary:               {}", report.binary);
        println!(
            "  version:              {}",
            report.binary_version.as_deref().unwrap_or("unavailable")
        );
        println!("  account login:         {}", report.login_status);
        println!(
            "  Codex plugin:          {}",
            if report.plugin_present {
                "present"
            } else {
                "not detected"
            }
        );
        println!(
            "  Codex hook trust:      {}",
            if report.hook_trust_ready {
                "ready"
            } else {
                "review required"
            }
        );
        if let Some(volume) = &report.container_credential_volume {
            let state =
                report
                    .container_credentials_present
                    .map_or("docker unavailable", |present| {
                        if present {
                            "present"
                        } else {
                            "missing"
                        }
                    });
            println!("  container credentials: {volume} ({state})");
        }
        println!("  integrations:");
        println!(
            "    Claude: settings={} mcp={} skills={}",
            report.integrations.claude_settings,
            report.integrations.claude_mcp,
            report.integrations.claude_skills
        );
        println!(
            "    Codex:  hooks={} mcp={} skills={} AGENTS.md={}",
            report.integrations.codex_hooks,
            report.integrations.codex_mcp,
            report.integrations.codex_skills,
            report.integrations.agents_instructions
        );
        println!(
            "    Shared: hooks={} mcp={}",
            report.integrations.shared_hooks, report.integrations.shared_mcp
        );
        for warning in &report.warnings {
            println!("  warning: {warning}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn complete_integrations() -> IntegrationStatus {
        IntegrationStatus {
            claude_settings: true,
            claude_mcp: true,
            claude_skills: true,
            codex_hooks: true,
            codex_mcp: true,
            codex_skills: true,
            agents_instructions: true,
            shared_hooks: true,
            shared_mcp: true,
        }
    }

    #[test]
    fn diagnostics_distinguish_incomplete_untrusted_and_duplicate_codex_assets() {
        let mut integrations = complete_integrations();
        integrations.codex_mcp = false;
        let warnings = integration_warnings(AgentProvider::Codex, &integrations, true, false);
        assert!(warnings
            .iter()
            .any(|warning| warning.contains("incomplete")));
        assert!(warnings.iter().any(|warning| warning.contains("/hooks")));
        assert!(warnings
            .iter()
            .any(|warning| warning.contains("both enabled")));
        assert!(warnings.iter().all(|warning| !warning.contains("token")));
    }

    #[test]
    fn diagnostics_are_clean_for_complete_trusted_integrations() {
        assert!(
            integration_warnings(AgentProvider::Codex, &complete_integrations(), false, true,)
                .is_empty()
        );
        assert!(integration_warnings(
            AgentProvider::Claude,
            &complete_integrations(),
            false,
            false,
        )
        .is_empty());
    }
}
