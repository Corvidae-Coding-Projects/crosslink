use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

/// Agent protocol selected independently from the executable path.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentProvider {
    #[default]
    Claude,
    Codex,
    Custom,
}

impl AgentProvider {
    #[must_use]
    pub const fn default_binary(self) -> Option<&'static str> {
        match self {
            Self::Claude => Some("claude"),
            Self::Codex => Some("codex"),
            Self::Custom => None,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Custom => "custom",
        }
    }
}

impl fmt::Display for AgentProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for AgentProvider {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "claude" => Ok(Self::Claude),
            "codex" => Ok(Self::Codex),
            "custom" => Ok(Self::Custom),
            other => {
                bail!("Unknown agent provider '{other}'. Valid providers: claude, codex, custom")
            }
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProviderModels {
    pub default: Option<String>,
    pub standard: Option<String>,
    pub advanced: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderOptions {
    pub models: ProviderModels,
    pub sandbox: String,
    pub approval: String,
}

impl ProviderOptions {
    fn defaults(provider: AgentProvider) -> Self {
        match provider {
            AgentProvider::Claude => Self {
                models: ProviderModels {
                    default: Some("opus".to_string()),
                    standard: Some("sonnet".to_string()),
                    advanced: Some("opus".to_string()),
                },
                sandbox: "workspace-write".to_string(),
                approval: "interactive".to_string(),
            },
            AgentProvider::Codex => Self {
                models: ProviderModels::default(),
                sandbox: "workspace-write".to_string(),
                // Codex deliberately protects Git metadata even when the
                // worktree itself is writable. Automatic review permits the
                // narrowly elevated git add/commit operations required by the
                // kickoff contract without disabling the host sandbox.
                approval: "auto-review".to_string(),
            },
            AgentProvider::Custom => Self {
                models: ProviderModels::default(),
                sandbox: "workspace-write".to_string(),
                approval: "interactive".to_string(),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedAgent {
    pub provider: AgentProvider,
    pub binary: PathBuf,
    pub options: ProviderOptions,
    /// True when provider semantics were inferred from a legacy binary-only config.
    pub legacy_inferred: bool,
}

impl ResolvedAgent {
    /// Resolve semantic model tiers while allowing an explicit provider model.
    #[must_use]
    pub fn resolve_model(&self, requested: Option<&str>) -> Option<String> {
        match requested.map(str::trim).filter(|value| !value.is_empty()) {
            None | Some("default") => self.options.models.default.clone(),
            Some("standard") => self.options.models.standard.clone(),
            Some("advanced") => self.options.models.advanced.clone(),
            Some(model) => Some(model.to_string()),
        }
    }
}

fn read_json(path: &Path) -> Result<Option<serde_json::Value>> {
    if !path.exists() {
        return Ok(None);
    }
    let body = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    let value = serde_json::from_str(&body)
        .with_context(|| format!("Failed to parse {}", path.display()))?;
    Ok(Some(value))
}

fn agent_string<'a>(value: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    // `crosslink config set` exposes and writes nested settings as literal
    // dotted keys. Prefer that explicit CLI-set value when a canonical nested
    // value from `crosslink init` is also present in the same layer.
    let dotted = value
        .get(format!("agent.{key}"))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());

    dotted.or_else(|| {
        value
            .get("agent")
            .and_then(|agent| agent.get(key))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    })
}

fn provider_value<'a>(
    value: &'a serde_json::Value,
    provider: AgentProvider,
    key: &str,
) -> Option<&'a str> {
    value
        .get("agent")
        .and_then(|agent| agent.get("providers"))
        .and_then(|providers| providers.get(provider.as_str()))
        .and_then(|config| config.get(key))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn infer_provider(binary: &str) -> AgentProvider {
    let name = Path::new(binary)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(binary)
        .trim_end_matches(".exe")
        .to_ascii_lowercase();
    match name.as_str() {
        "claude" => AgentProvider::Claude,
        "codex" => AgentProvider::Codex,
        _ => AgentProvider::Custom,
    }
}

fn overlay_options(
    options: &mut ProviderOptions,
    layer: &serde_json::Value,
    provider: AgentProvider,
) {
    let fields = [
        ("default_model", &mut options.models.default),
        ("standard_model", &mut options.models.standard),
        ("advanced_model", &mut options.models.advanced),
    ];
    for (key, slot) in fields {
        if let Some(value) = provider_value(layer, provider, key) {
            *slot = Some(value.to_string());
        }
    }
    if let Some(value) = provider_value(layer, provider, "sandbox") {
        options.sandbox = value.to_string();
    }
    if let Some(value) = provider_value(layer, provider, "approval") {
        options.approval = value.to_string();
    }
}

/// Resolve agent settings with local > shared > legacy inference > Claude precedence.
pub fn resolve_agent(crosslink_dir: &Path) -> Result<ResolvedAgent> {
    let team = read_json(&crosslink_dir.join("hook-config.json"))?.unwrap_or_default();
    let local = read_json(&crosslink_dir.join("hook-config.local.json"))?;

    let explicit_provider = local
        .as_ref()
        .and_then(|value| agent_string(value, "provider"))
        .or_else(|| agent_string(&team, "provider"));
    let binary_override = local
        .as_ref()
        .and_then(|value| agent_string(value, "binary"))
        .or_else(|| agent_string(&team, "binary"));

    let (provider, legacy_inferred) = if let Some(value) = explicit_provider {
        (AgentProvider::from_str(value)?, false)
    } else if let Some(binary) = binary_override {
        (infer_provider(binary), true)
    } else {
        (AgentProvider::Claude, false)
    };

    let binary = binary_override
        .map(PathBuf::from)
        .or_else(|| provider.default_binary().map(PathBuf::from))
        .ok_or_else(|| {
            anyhow::anyhow!("agent.provider is 'custom' but agent.binary is not configured")
        })?;

    let mut options = ProviderOptions::defaults(provider);
    overlay_options(&mut options, &team, provider);
    if let Some(local) = &local {
        overlay_options(&mut options, local, provider);
    }

    Ok(ResolvedAgent {
        provider,
        binary,
        options,
        legacy_inferred,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write(dir: &Path, name: &str, value: serde_json::Value) {
        std::fs::write(dir.join(name), serde_json::to_vec_pretty(&value).unwrap()).unwrap();
    }

    #[test]
    fn provider_values_round_trip() {
        for provider in [
            AgentProvider::Claude,
            AgentProvider::Codex,
            AgentProvider::Custom,
        ] {
            let encoded = serde_json::to_string(&provider).unwrap();
            let decoded: AgentProvider = serde_json::from_str(&encoded).unwrap();
            assert_eq!(decoded, provider);
        }
    }

    #[test]
    fn defaults_to_claude() {
        let dir = tempdir().unwrap();
        let resolved = resolve_agent(dir.path()).unwrap();
        assert_eq!(resolved.provider, AgentProvider::Claude);
        assert_eq!(resolved.binary, PathBuf::from("claude"));
        assert!(!resolved.legacy_inferred);
    }

    #[test]
    fn codex_defaults_to_automatic_review_for_git_metadata_writes() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "hook-config.json",
            serde_json::json!({"agent":{"provider":"codex"}}),
        );

        let resolved = resolve_agent(dir.path()).unwrap();
        assert_eq!(resolved.provider, AgentProvider::Codex);
        assert_eq!(resolved.options.sandbox, "workspace-write");
        assert_eq!(resolved.options.approval, "auto-review");
    }

    #[test]
    fn local_provider_wins_but_binary_only_overrides_executable() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "hook-config.json",
            serde_json::json!({"agent":{"provider":"claude","binary":"team-wrapper"}}),
        );
        write(
            dir.path(),
            "hook-config.local.json",
            serde_json::json!({"agent":{"provider":"codex","binary":"/opt/codex-wrapper"}}),
        );
        let resolved = resolve_agent(dir.path()).unwrap();
        assert_eq!(resolved.provider, AgentProvider::Codex);
        assert_eq!(resolved.binary, PathBuf::from("/opt/codex-wrapper"));
    }

    #[test]
    fn dotted_local_provider_key_written_by_config_cli_is_resolved() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "hook-config.json",
            serde_json::json!({"agent":{"provider":"claude"}}),
        );
        write(
            dir.path(),
            "hook-config.local.json",
            serde_json::json!({"agent.provider":"codex"}),
        );

        let resolved = resolve_agent(dir.path()).unwrap();
        assert_eq!(resolved.provider, AgentProvider::Codex);
        assert_eq!(resolved.binary, PathBuf::from("codex"));
        assert!(!resolved.legacy_inferred);
    }

    #[test]
    fn dotted_team_provider_key_overrides_nested_init_default() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "hook-config.json",
            serde_json::json!({
                "agent": {"provider": "claude"},
                "agent.provider": "codex"
            }),
        );

        let resolved = resolve_agent(dir.path()).unwrap();
        assert_eq!(resolved.provider, AgentProvider::Codex);
        assert_eq!(resolved.binary, PathBuf::from("codex"));
        assert!(!resolved.legacy_inferred);
    }

    #[test]
    fn legacy_binary_infers_known_and_custom_protocols() {
        for (binary, expected) in [
            ("claude", AgentProvider::Claude),
            ("/opt/bin/codex", AgentProvider::Codex),
            ("my-agent", AgentProvider::Custom),
        ] {
            let dir = tempdir().unwrap();
            write(
                dir.path(),
                "hook-config.json",
                serde_json::json!({"agent":{"binary":binary}}),
            );
            let resolved = resolve_agent(dir.path()).unwrap();
            assert_eq!(resolved.provider, expected);
            assert!(resolved.legacy_inferred);
        }
    }

    #[test]
    fn custom_requires_binary() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "hook-config.json",
            serde_json::json!({"agent":{"provider":"custom"}}),
        );
        let error = resolve_agent(dir.path()).unwrap_err().to_string();
        assert!(error.contains("agent.binary"));
    }

    #[test]
    fn local_provider_options_overlay_shared_values() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "hook-config.json",
            serde_json::json!({"agent":{"provider":"codex","providers":{"codex":{"default_model":"gpt-team","sandbox":"read-only"}}}}),
        );
        write(
            dir.path(),
            "hook-config.local.json",
            serde_json::json!({"agent":{"providers":{"codex":{"default_model":"gpt-local","approval":"on-request"}}}}),
        );
        let resolved = resolve_agent(dir.path()).unwrap();
        assert_eq!(
            resolved.options.models.default.as_deref(),
            Some("gpt-local")
        );
        assert_eq!(resolved.options.sandbox, "read-only");
        assert_eq!(resolved.options.approval, "on-request");
    }
}
