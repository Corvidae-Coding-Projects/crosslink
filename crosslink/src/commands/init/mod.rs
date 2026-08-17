mod manifest;
mod merge;
mod python;
mod signing;
mod walkthrough;

use anyhow::{Context, Result};
use crossterm::style::Stylize;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::Path;

use crate::db::Database;
use merge::{
    agents_managed_hash, write_agents_md_merged, write_codex_config_merged,
    write_codex_hooks_merged, write_mcp_json_merged, write_root_gitignore,
    write_settings_json_merged,
};
pub use python::detect_python_prefix;
use python::{install_cpitd, CpitdResult};
use signing::setup_driver_signing;
use walkthrough::{apply_tui_choices, run_tui_walkthrough, setup_shell_alias};

const PYTHON_PREFIX_PLACEHOLDER: &str = "__PYTHON_PREFIX__";

const SETTINGS_JSON: &str = include_str!("../../../resources/providers/claude/settings.json");
const CODEX_HOOKS_JSON: &str = include_str!("../../../resources/providers/codex/hooks.json");
const CODEX_CONFIG_TOML: &str = include_str!("../../../resources/providers/codex/config.toml");
const AGENTS_MD: &str = include_str!("../../../resources/agent/instructions/crosslink-agents.md");
pub(crate) const PROMPT_GUARD_PY: &str =
    include_str!("../../../resources/agent/hooks/prompt-guard.py");
pub(crate) const POST_EDIT_CHECK_PY: &str =
    include_str!("../../../resources/agent/hooks/post-edit-check.py");
pub(crate) const SESSION_START_PY: &str =
    include_str!("../../../resources/agent/hooks/session-start.py");
pub(crate) const PRE_WEB_CHECK_PY: &str =
    include_str!("../../../resources/agent/hooks/pre-web-check.py");
pub(crate) const WORK_CHECK_PY: &str = include_str!("../../../resources/agent/hooks/work-check.py");
pub(crate) const CROSSLINK_CONFIG_PY: &str =
    include_str!("../../../resources/agent/hooks/crosslink_config.py");
pub(crate) const HEARTBEAT_PY: &str = include_str!("../../../resources/agent/hooks/heartbeat.py");
pub(crate) const HOOK_PROTOCOL_PY: &str =
    include_str!("../../../resources/agent/hooks/hook_protocol.py");

const KNOWLEDGE_SERVER_PY: &str = include_str!("../../../resources/agent/mcp/knowledge-server.py");
const AGENT_PROMPT_SERVER_PY: &str =
    include_str!("../../../resources/agent/mcp/agent-prompt-server.py");
const MCP_JSON: &str = include_str!("../../../resources/mcp.json");

include!(concat!(env!("OUT_DIR"), "/commands_gen.rs"));

pub(crate) use crate::commands::config_registry::HOOK_CONFIG_JSON;

include!(concat!(env!("OUT_DIR"), "/rules_gen.rs"));

include!(concat!(env!("OUT_DIR"), "/skills_gen.rs"));

use crate::commands::config_registry::{ConfigType, REGISTRY};
use std::collections::HashMap;

struct TuiChoices {
    values: HashMap<String, serde_json::Value>,
    install_alias: bool,
}

impl Default for TuiChoices {
    fn default() -> Self {
        let mut values = HashMap::new();

        let defaults: serde_json::Value =
            serde_json::from_str(HOOK_CONFIG_JSON).unwrap_or_default();
        for entry in REGISTRY {
            if matches!(
                entry.config_type,
                ConfigType::StringArray | ConfigType::Map | ConfigType::Integer
            ) {
                continue;
            }
            if let Some(v) = defaults.get(entry.key) {
                values.insert(entry.key.to_string(), v.clone());
            }
        }
        Self {
            values,
            install_alias: false,
        }
    }
}

struct InitUI {
    is_tty: bool,
}

impl InitUI {
    fn new() -> Self {
        Self {
            is_tty: io::stdout().is_terminal(),
        }
    }

    fn banner(&self) {
        if self.is_tty {
            println!();
            println!("  {} {}", "crosslink".bold().cyan(), "init".dark_grey());
            println!("  {}", "─".repeat(40).dark_grey());
            println!();
        }
    }

    fn step_start(&self, label: &str) {
        if self.is_tty {
            print!("  {} {}... ", "●".cyan(), label);
            io::stdout().flush().ok();
        } else {
            print!("{label}... ");
        }
    }

    fn step_ok(&self, detail: Option<&str>) {
        if self.is_tty {
            match detail {
                Some(d) => println!("{} {}", "✓".green(), d.dark_grey()),
                None => println!("{}", "✓".green()),
            }
        } else {
            match detail {
                Some(d) => println!("done ({d})"),
                None => println!("done"),
            }
        }
    }

    fn step_created(&self, what: &str) {
        if self.is_tty {
            println!(
                "  {} {} {}",
                "✓".green(),
                "created".green(),
                what.dark_grey()
            );
        } else {
            println!("Created {what}");
        }
    }

    fn step_skip(&self, msg: &str) {
        if self.is_tty {
            println!("  {} {}", "–".dark_grey(), msg.dark_grey());
        } else {
            println!("{msg}");
        }
    }

    fn warn(&self, msg: &str) {
        if self.is_tty {
            println!("  {} {}", "⚠".yellow(), msg.yellow());
        } else {
            println!("Warning: {msg}");
        }
    }

    fn detail(&self, msg: &str) {
        if self.is_tty {
            println!("    {}", msg.dark_grey());
        } else {
            println!("  {msg}");
        }
    }

    fn success(&self) {
        if self.is_tty {
            println!();
            println!(
                "  {} {}",
                "✓".green().bold(),
                "Crosslink initialized successfully!".bold()
            );
            println!();
            println!(
                "  {} {} {}",
                "next".dark_grey(),
                "→".cyan(),
                "crosslink session start".white()
            );
            println!(
                "       {} {}",
                "→".cyan(),
                "crosslink create \"Task\"".white()
            );
            println!();
        } else {
            println!("Crosslink initialized successfully!");
            println!();
            println!("Crosslink tracks issues, comments, and sessions in .crosslink/issues.db.");
            println!("AI agents use it to coordinate work across sessions and worktrees.");
            println!();
            println!("Quick start:");
            println!("  crosslink create \"Task\"     # Create an issue");
            println!("  crosslink list              # See all issues");
            println!("  crosslink session start     # Start a work session");
            println!();
            println!("Multi-agent features (agents, signing, locks, containers) are optional");
            println!("and only needed when multiple AI agents collaborate on the same repo.");
        }
    }
}

pub struct InitOpts<'a> {
    pub force: bool,
    pub update: bool,
    pub dry_run: bool,
    pub no_prompt: bool,
    pub python_prefix: Option<&'a str>,
    pub skip_cpitd: bool,
    pub skip_signing: bool,
    pub signing_key: Option<&'a str>,
    pub reconfigure: bool,
    pub defaults: bool,
    pub integrations: IntegrationSelection,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum IntegrationSelection {
    Claude,
    Codex,
    #[default]
    Both,
}

impl IntegrationSelection {
    const fn includes_claude(self) -> bool {
        matches!(self, Self::Claude | Self::Both)
    }

    const fn includes_codex(self) -> bool {
        matches!(self, Self::Codex | Self::Both)
    }
}

impl std::str::FromStr for IntegrationSelection {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "claude" => Ok(Self::Claude),
            "codex" => Ok(Self::Codex),
            "both" => Ok(Self::Both),
            _ => anyhow::bail!("integration must be one of: claude, codex, both"),
        }
    }
}

fn managed_projection(relative_path: &str, raw: &str) -> Result<String> {
    match relative_path {
        ".claude/settings.json" => {
            let value: serde_json::Value = serde_json::from_str(raw)?;
            let defaults: serde_json::Value = serde_json::from_str(SETTINGS_JSON)?;
            let required_tools: Vec<&str> = defaults
                .get("allowedTools")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(serde_json::Value::as_str)
                .collect();
            let tools: Vec<serde_json::Value> = value
                .get("allowedTools")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(serde_json::Value::as_str)
                .filter(|tool| required_tools.contains(tool))
                .map(|tool| serde_json::Value::String(tool.to_string()))
                .collect();
            Ok(serde_json::to_string(&serde_json::json!({
                "allowedTools": tools,
                "enableAllProjectMcpServers": value.get("enableAllProjectMcpServers"),
                "hooks": value.get("hooks"),
            }))?)
        }
        ".mcp.json" => {
            let value: serde_json::Value = serde_json::from_str(raw)?;
            let defaults: serde_json::Value = serde_json::from_str(MCP_JSON)?;
            let current = value
                .get("mcpServers")
                .and_then(serde_json::Value::as_object);
            let mut managed = serde_json::Map::new();
            if let Some(default_servers) = defaults
                .get("mcpServers")
                .and_then(serde_json::Value::as_object)
            {
                for name in default_servers.keys() {
                    if let Some(server) = current.and_then(|servers| servers.get(name)) {
                        managed.insert(name.clone(), server.clone());
                    }
                }
            }
            Ok(serde_json::to_string(&serde_json::json!({
                "mcpServers": managed,
            }))?)
        }
        ".codex/hooks.json" => {
            let value: serde_json::Value = serde_json::from_str(raw)?;
            let mut projected = serde_json::Map::new();
            if let Some(hooks) = value.get("hooks").and_then(serde_json::Value::as_object) {
                for (event, groups) in hooks {
                    let managed: Vec<_> = groups
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|group| {
                            group
                                .get("description")
                                .and_then(serde_json::Value::as_str)
                                .is_some_and(|description| description.starts_with("crosslink:"))
                        })
                        .cloned()
                        .collect();
                    if !managed.is_empty() {
                        projected.insert(event.clone(), serde_json::Value::Array(managed));
                    }
                }
            }
            Ok(serde_json::to_string(&projected)?)
        }
        ".codex/config.toml" => {
            let document = raw.parse::<toml_edit::DocumentMut>()?;
            let mut output = String::new();
            if let Some(servers) = document
                .get("mcp_servers")
                .and_then(toml_edit::Item::as_table_like)
            {
                for name in ["crosslink-agent-prompt", "crosslink-knowledge"] {
                    if let Some(item) = servers.get(name) {
                        output.push_str(name);
                        output.push('=');
                        output.push_str(&item.to_string());
                        output.push('\n');
                    }
                }
            }
            Ok(output)
        }
        "AGENTS.md" => Ok(raw.trim().to_string()),
        _ => Ok(raw.to_string()),
    }
}

fn managed_current_hash(relative_path: &str, path: &Path) -> Result<Option<String>> {
    if relative_path == "AGENTS.md" {
        return agents_managed_hash(path);
    }
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("Failed to read {}", path.display()))
        }
    };
    Ok(Some(manifest::sha256_hex(&managed_projection(
        relative_path,
        &raw,
    )?)))
}

pub(crate) fn codex_hook_trust_ready(project_root: &Path) -> Result<bool> {
    let crosslink_dir = project_root.join(".crosslink");
    let Some(manifest) = manifest::read_manifest(&crosslink_dir) else {
        return Ok(false);
    };
    let mut paths = vec![".codex/hooks.json".to_string()];
    paths.extend(
        manifest
            .files
            .keys()
            .filter(|path| path.starts_with(".crosslink/integrations/hooks/"))
            .cloned(),
    );
    if paths.len() < 2 {
        return Ok(false);
    }

    let Ok(hook_raw) = fs::read_to_string(project_root.join(".codex/hooks.json")) else {
        return Ok(false);
    };
    let hook_value: serde_json::Value = match serde_json::from_str(&hook_raw) {
        Ok(value) => value,
        Err(_) => return Ok(false),
    };
    let Some(root) = hook_value.as_object() else {
        return Ok(false);
    };
    if root.len() != 1 || !root.contains_key("hooks") {
        return Ok(false);
    }
    let all_managed = root
        .get("hooks")
        .and_then(serde_json::Value::as_object)
        .into_iter()
        .flat_map(|events| events.values())
        .flat_map(|groups| groups.as_array().into_iter().flatten())
        .all(|group| {
            group
                .get("description")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|description| description.starts_with("crosslink:"))
        });
    if !all_managed {
        return Ok(false);
    }
    for relative_path in paths {
        let Some(entry) = manifest.files.get(&relative_path) else {
            return Ok(false);
        };
        let current = managed_current_hash(&relative_path, &project_root.join(&relative_path))?;
        if current.as_deref() != Some(entry.sha256.as_str()) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn write_managed_file(
    root: &Path,
    relative_path: &str,
    content: &str,
    python_prefix: &str,
) -> Result<()> {
    let path = root.join(relative_path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    match relative_path {
        ".claude/settings.json" => write_settings_json_merged(&path, python_prefix),
        ".mcp.json" => write_mcp_json_merged(&path).map(|_| ()),
        ".codex/hooks.json" => write_codex_hooks_merged(&path, python_prefix),
        ".codex/config.toml" => write_codex_config_merged(&path),
        "AGENTS.md" => write_agents_md_merged(&path),
        _ => fs::write(&path, content).with_context(|| format!("Failed to write {relative_path}")),
    }
}

fn ensure_repo_compact_id(crosslink_dir: &Path) -> Result<()> {
    let id_path = crosslink_dir.join("repo-id");
    if id_path.exists() {
        return Ok(());
    }
    let id = crate::utils::generate_compact_id();
    fs::write(&id_path, &id).context("Failed to write repo-id")?;
    Ok(())
}

pub fn read_repo_compact_id(crosslink_dir: &Path) -> String {
    let id_path = crosslink_dir.join("repo-id");
    if let Ok(id) = fs::read_to_string(&id_path) {
        let trimmed = id.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }

    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    crosslink_dir.hash(&mut hasher);
    crate::utils::base62_encode_4(hasher.finish())
}

fn init_agent_identity(crosslink_dir: &Path, agent_id: &str) -> Result<()> {
    let mut config = crate::identity::AgentConfig::init(crosslink_dir, agent_id, None)?;

    let keys_dir = crate::signing::host_crosslink_dir(crosslink_dir).join("keys");
    match crate::signing::generate_agent_key(&keys_dir, agent_id, &config.machine_id) {
        Ok(keypair) => {
            config.ssh_key_path = Some(format!("keys/{agent_id}_ed25519"));
            config.ssh_fingerprint = Some(keypair.fingerprint);
            config.ssh_public_key = Some(keypair.public_key);

            let path = crosslink_dir.join("agent.json");
            let json = serde_json::to_string_pretty(&config)?;
            fs::write(&path, json)?;
        }
        Err(e) => {
            tracing::warn!("Could not generate agent SSH key: {e}");
        }
    }

    Ok(())
}

fn populate_tracker_remote(config_path: &Path, project_root: &Path) -> Result<()> {
    if !config_path.exists() {
        return Ok(());
    }

    let raw = fs::read_to_string(config_path)?;
    let mut config: serde_json::Value = serde_json::from_str(&raw)?;

    let current = config
        .get("tracker_remote")
        .and_then(|v| v.as_str())
        .map(String::from);

    if let Some(v) = &current {
        if v != "origin" && v != "(text)" {
            return Ok(());
        }
    }

    let detected = match detect_git_remotes(project_root).as_slice() {
        [] => None,
        [only] => Some(only.clone()),
        many if many.iter().any(|r| r == "origin") => Some("origin".to_string()),
        many => Some(many[0].clone()),
    };

    let new_value = match (current.as_deref(), detected.as_deref()) {
        (Some("origin"), None | Some("origin")) => return Ok(()),

        (Some("origin"), Some(other)) => other.to_string(),

        (Some("(text)") | None, Some(d)) => d.to_string(),
        (Some("(text)") | None, None) => "origin".to_string(),

        (Some(_), _) => return Ok(()),
    };

    if let serde_json::Value::Object(map) = &mut config {
        map.insert(
            "tracker_remote".to_string(),
            serde_json::Value::String(new_value),
        );
    }

    let output = serde_json::to_string_pretty(&config)?;
    fs::write(config_path, format!("{output}\n"))?;
    Ok(())
}

fn detect_git_remotes(project_root: &Path) -> Vec<String> {
    let output = std::process::Command::new("git")
        .current_dir(project_root)
        .args(["remote"])
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let mut remotes: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();
    remotes.sort();
    remotes
}

fn populate_agent_tool_commands(config_path: &Path, project_root: &Path) -> Result<()> {
    if !config_path.exists() {
        return Ok(());
    }

    let raw = fs::read_to_string(config_path)?;
    let mut config: serde_json::Value = serde_json::from_str(&raw)?;

    let Some(serde_json::Value::Object(overrides)) = config.get_mut("agent_overrides") else {
        return Ok(());
    };

    let lint_empty = overrides
        .get("agent_lint_commands")
        .and_then(|v| v.as_array())
        .is_none_or(Vec::is_empty);
    let test_empty = overrides
        .get("agent_test_commands")
        .and_then(|v| v.as_array())
        .is_none_or(Vec::is_empty);

    if !lint_empty && !test_empty {
        return Ok(());
    }

    let conv = super::kickoff::detect_conventions(project_root);

    let changed = if lint_empty && !conv.lint_commands.is_empty() {
        overrides.insert(
            "agent_lint_commands".to_string(),
            serde_json::json!(conv.lint_commands),
        );
        true
    } else {
        false
    };

    let changed = if test_empty {
        conv.test_command.as_ref().map_or(changed, |test_cmd| {
            overrides.insert(
                "agent_test_commands".to_string(),
                serde_json::json!([test_cmd]),
            );
            true
        })
    } else {
        changed
    };

    if changed {
        let output = serde_json::to_string_pretty(&config)?;
        fs::write(config_path, format!("{output}\n"))?;
    }

    Ok(())
}

fn managed_files(
    python_prefix: &str,
    integrations: IntegrationSelection,
) -> Result<Vec<(String, String)>> {
    let mut files: Vec<(String, String)> = vec![
        (
            ".crosslink/integrations/hooks/prompt-guard.py".into(),
            PROMPT_GUARD_PY.into(),
        ),
        (
            ".crosslink/integrations/hooks/post-edit-check.py".into(),
            POST_EDIT_CHECK_PY.into(),
        ),
        (
            ".crosslink/integrations/hooks/session-start.py".into(),
            SESSION_START_PY.into(),
        ),
        (
            ".crosslink/integrations/hooks/pre-web-check.py".into(),
            PRE_WEB_CHECK_PY.into(),
        ),
        (
            ".crosslink/integrations/hooks/work-check.py".into(),
            WORK_CHECK_PY.into(),
        ),
        (
            ".crosslink/integrations/hooks/crosslink_config.py".into(),
            CROSSLINK_CONFIG_PY.into(),
        ),
        (
            ".crosslink/integrations/hooks/heartbeat.py".into(),
            HEARTBEAT_PY.into(),
        ),
        (
            ".crosslink/integrations/hooks/hook_protocol.py".into(),
            HOOK_PROTOCOL_PY.into(),
        ),
        (
            ".crosslink/integrations/mcp/knowledge-server.py".into(),
            KNOWLEDGE_SERVER_PY.into(),
        ),
        (
            ".crosslink/integrations/mcp/agent-prompt-server.py".into(),
            AGENT_PROMPT_SERVER_PY.into(),
        ),
    ];

    if integrations.includes_claude() {
        for (filename, content) in COMMAND_FILES {
            files.push((format!(".claude/commands/{filename}"), content.to_string()));
        }
    }

    for (filename, content) in RULE_FILES {
        files.push((format!(".crosslink/rules/{filename}"), content.to_string()));
    }

    for (rel_path, content) in SKILL_FILES {
        if integrations.includes_claude() {
            files.push((format!(".claude/skills/{rel_path}"), content.to_string()));
        }
        if integrations.includes_codex() {
            files.push((format!(".agents/skills/{rel_path}"), content.to_string()));
        }
    }

    if integrations.includes_claude() {
        let settings_template = SETTINGS_JSON.replace(PYTHON_PREFIX_PLACEHOLDER, python_prefix);
        files.push((
            ".claude/settings.json".into(),
            managed_projection(".claude/settings.json", &settings_template)?,
        ));
        files.push((
            ".mcp.json".into(),
            managed_projection(".mcp.json", MCP_JSON)?,
        ));
    }
    if integrations.includes_codex() {
        let hooks_template = CODEX_HOOKS_JSON.replace(PYTHON_PREFIX_PLACEHOLDER, python_prefix);
        files.push((
            ".codex/hooks.json".into(),
            managed_projection(".codex/hooks.json", &hooks_template)?,
        ));
        files.push((
            ".codex/config.toml".into(),
            managed_projection(".codex/config.toml", CODEX_CONFIG_TOML)?,
        ));
        files.push(("AGENTS.md".into(), AGENTS_MD.trim_end().into()));
    }

    Ok(files)
}

fn run_update(path: &Path, opts: &InitOpts<'_>) -> Result<()> {
    use manifest::{
        build_manifest, classify_update, read_manifest, sha256_hex, write_manifest, UpdateAction,
    };

    let crosslink_dir = path.join(".crosslink");
    let ui = InitUI::new();

    if !crosslink_dir.exists() {
        anyhow::bail!(
            "Project not initialized. Run `crosslink init` first, then use `--update` for upgrades."
        );
    }

    let prefix = opts.python_prefix.map_or_else(
        || detect_python_prefix(path),
        std::string::ToString::to_string,
    );

    let template_files = managed_files(&prefix, opts.integrations)?;
    let old_manifest = read_manifest(&crosslink_dir);

    let manifest_missing = old_manifest.is_none();
    if manifest_missing {
        ui.warn(
            "No init-manifest.json found — treating all managed files as potentially modified.",
        );
        ui.detail("This is expected on first upgrade from a pre-manifest crosslink version.");
        ui.detail("Use `crosslink init --force` instead to overwrite all managed files.");
        println!();
    }

    ui.banner();

    let old_files = old_manifest
        .as_ref()
        .map(|m| &m.files)
        .cloned()
        .unwrap_or_default();

    let mut auto_updated: Vec<String> = Vec::new();
    let mut up_to_date: Vec<String> = Vec::new();
    let mut template_unchanged: Vec<String> = Vec::new();
    let mut conflicts: Vec<String> = Vec::new();
    let mut deleted: Vec<String> = Vec::new();
    let mut new_files: Vec<String> = Vec::new();

    for (rel_path, template_content) in &template_files {
        let abs_path = path.join(rel_path);
        let new_template_hash = sha256_hex(template_content);

        match old_files.get(rel_path) {
            Some(entry) => {
                let current_hash = managed_current_hash(rel_path, &abs_path)?;
                let action =
                    classify_update(&entry.sha256, current_hash.as_deref(), &new_template_hash);

                match action {
                    UpdateAction::UpToDate => up_to_date.push(rel_path.clone()),
                    UpdateAction::AutoUpdate => auto_updated.push(rel_path.clone()),
                    UpdateAction::TemplateUnchanged => {
                        template_unchanged.push(rel_path.clone());
                    }
                    UpdateAction::Conflict => conflicts.push(rel_path.clone()),
                    UpdateAction::Deleted => deleted.push(rel_path.clone()),
                    UpdateAction::NewFile => unreachable!(),
                }
            }
            None => {
                new_files.push(rel_path.clone());
            }
        }
    }

    let all_current: std::collections::HashSet<String> =
        managed_files(&prefix, IntegrationSelection::Both)?
            .into_iter()
            .map(|(relative_path, _)| relative_path)
            .collect();
    let mut retired_untouched = Vec::new();
    let mut retired_modified = Vec::new();
    for (relative_path, old_entry) in &old_files {
        if all_current.contains(relative_path) {
            continue;
        }
        let current_hash = manifest::sha256_file(&path.join(relative_path))?;
        if current_hash.as_deref() == Some(old_entry.sha256.as_str()) {
            retired_untouched.push(relative_path.clone());
        } else if current_hash.is_some() {
            retired_modified.push(relative_path.clone());
        }
    }

    let total_changes = auto_updated.len() + new_files.len();
    let has_issues = !conflicts.is_empty() || !deleted.is_empty();

    if total_changes == 0 && !has_issues {
        ui.step_skip("All managed files are up to date.");
    }

    if !auto_updated.is_empty() {
        ui.step_start(&format!(
            "{} file{} to auto-update",
            auto_updated.len(),
            if auto_updated.len() == 1 { "" } else { "s" }
        ));
        println!();
        for f in &auto_updated {
            ui.detail(f);
        }
    }

    if !new_files.is_empty() {
        ui.step_start(&format!(
            "{} new file{} to create",
            new_files.len(),
            if new_files.len() == 1 { "" } else { "s" }
        ));
        println!();
        for f in &new_files {
            ui.detail(f);
        }
    }

    if !conflicts.is_empty() {
        ui.warn(&format!(
            "{} file{} modified by both user and template — {}",
            conflicts.len(),
            if conflicts.len() == 1 { "" } else { "s" },
            if opts.no_prompt {
                "skipping (--no-prompt)"
            } else {
                "will prompt"
            }
        ));
        for f in &conflicts {
            ui.detail(f);
        }
    }

    for f in &deleted {
        ui.detail(&format!(
            "{f} — deleted by user, skipping (will not recreate)"
        ));
    }
    for file in &retired_untouched {
        ui.detail(&format!("{file} — retired managed asset to remove"));
    }
    for file in &retired_modified {
        ui.warn(&format!(
            "Retired managed file {file} has user changes and will be preserved"
        ));
    }

    if !template_unchanged.is_empty() || !up_to_date.is_empty() {
        let skip_count = template_unchanged.len() + up_to_date.len();
        ui.step_skip(&format!(
            "{skip_count} file{} already up to date",
            if skip_count == 1 { "" } else { "s" }
        ));
    }

    if opts.dry_run {
        println!();
        ui.detail("Dry run — no files were modified.");
        return Ok(());
    }

    let template_map: std::collections::HashMap<&str, &str> = template_files
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    for rel_path in &auto_updated {
        let content = template_map[rel_path.as_str()];

        write_managed_file(path, rel_path, content, &prefix)?;
    }

    for rel_path in &new_files {
        let content = template_map[rel_path.as_str()];

        write_managed_file(path, rel_path, content, &prefix)?;
    }

    let mut conflict_accepted: Vec<String> = Vec::new();
    if !opts.no_prompt {
        let is_tty = io::stdin().is_terminal();
        for rel_path in &conflicts {
            if !is_tty {
                ui.detail(&format!("Skipping {rel_path} (non-interactive terminal)"));
                continue;
            }

            print!("  Overwrite {rel_path} with new template? (user changes will be lost) [y/N] ");
            io::stdout().flush().ok();

            let mut answer = String::new();
            io::stdin().read_line(&mut answer).ok();

            if answer.trim().eq_ignore_ascii_case("y") {
                let content = template_map[rel_path.as_str()];
                write_managed_file(path, rel_path, content, &prefix)?;
                conflict_accepted.push(rel_path.clone());
            } else {
                ui.detail(&format!("Keeping user version of {rel_path}"));
            }
        }
    }

    let new_full_manifest = build_manifest(&template_files);
    let mut final_manifest = new_full_manifest;

    for rel_path in &conflicts {
        if !conflict_accepted.contains(rel_path) {
            if let Some(old_entry) = old_files.get(rel_path) {
                final_manifest
                    .files
                    .insert(rel_path.clone(), old_entry.clone());
            }
        }
    }

    for rel_path in &deleted {
        final_manifest.files.remove(rel_path);
    }

    for rel_path in &template_unchanged {
        if let Some(old_entry) = old_files.get(rel_path) {
            final_manifest
                .files
                .insert(rel_path.clone(), old_entry.clone());
        }
    }

    for (relative_path, entry) in &old_files {
        if all_current.contains(relative_path) && !final_manifest.files.contains_key(relative_path)
        {
            final_manifest
                .files
                .insert(relative_path.clone(), entry.clone());
        }
    }
    for relative_path in &retired_untouched {
        fs::remove_file(path.join(relative_path))
            .with_context(|| format!("Failed to retire managed file {relative_path}"))?;
        final_manifest.files.remove(relative_path);
    }

    write_manifest(&crosslink_dir, &final_manifest)?;

    let total_written = auto_updated.len() + new_files.len() + conflict_accepted.len();
    if total_written > 0 {
        ui.step_ok(Some(&format!(
            "{total_written} file{} updated",
            if total_written == 1 { "" } else { "s" }
        )));
    }

    Ok(())
}

pub fn run(path: &Path, opts: &InitOpts<'_>) -> Result<()> {
    if opts.update {
        return run_update(path, opts);
    }

    let force = opts.force;
    let python_prefix = opts.python_prefix;
    let skip_cpitd = opts.skip_cpitd;
    let skip_signing = opts.skip_signing;
    let signing_key = opts.signing_key;
    let reconfigure = opts.reconfigure;
    let defaults = opts.defaults;
    let crosslink_dir = path.join(".crosslink");
    let prefix = python_prefix.map_or_else(
        || detect_python_prefix(path),
        std::string::ToString::to_string,
    );

    let ui = InitUI::new();

    let git_dir = path.join(".git");
    if !git_dir.exists() {
        anyhow::bail!(
            "No git repository found at {}.\n\
             Run `git init` and create an initial commit before running `crosslink init`.",
            path.display()
        );
    }
    let has_commits = std::process::Command::new("git")
        .current_dir(path)
        .args(["rev-parse", "HEAD"])
        .output()
        .is_ok_and(|o| o.status.success());
    if !has_commits {
        anyhow::bail!(
            "Git repository has no commits.\n\
             Create an initial commit before running `crosslink init`:\n\
             \n  git add .\n  git commit -m \"Initial commit\""
        );
    }

    let crosslink_exists = crosslink_dir.exists();
    let requested_files = managed_files(&prefix, opts.integrations)?;
    let integrations_complete = requested_files.iter().all(|(relative_path, _)| {
        let target = path.join(relative_path);
        if relative_path == "AGENTS.md" {
            return fs::read_to_string(target)
                .is_ok_and(|content| content.contains(merge::AGENTS_SECTION_START));
        }
        target.is_file()
    });

    if crosslink_exists && integrations_complete && !force && !reconfigure {
        ui.step_skip("Already initialized");
        ui.detail("Use --update for a manifest-safe asset upgrade.");
        ui.detail("Use --reconfigure to re-run the setup walkthrough.");
        return Ok(());
    }

    let config_path = crosslink_dir.join("hook-config.json");
    let config_exists = config_path.exists();
    let should_run_tui = !defaults && (!config_exists || force || reconfigure);

    let tui_result = if should_run_tui {
        let base_config: serde_json::Value = if config_exists && reconfigure {
            let raw = fs::read_to_string(&config_path)
                .context("Failed to read existing hook-config.json")?;
            serde_json::from_str(&raw).context("hook-config.json contains invalid JSON")?
        } else {
            serde_json::from_str(HOOK_CONFIG_JSON)
                .context("Embedded hook-config.json is invalid")?
        };

        let existing_ref = if config_exists {
            Some(&base_config as &serde_json::Value)
        } else {
            None
        };
        let choices = run_tui_walkthrough(existing_ref)?;
        Some((base_config, choices))
    } else {
        None
    };

    if opts.dry_run {
        let ui = InitUI::new();
        ui.banner();

        let files = managed_files(&prefix, opts.integrations)?;

        let mut would_write: Vec<&str> = Vec::new();
        let mut would_create: Vec<&str> = Vec::new();
        for (rel_path, _) in &files {
            if path.join(rel_path).exists() {
                would_write.push(rel_path);
            } else {
                would_create.push(rel_path);
            }
        }

        let extra = [".crosslink/hook-config.json", ".gitignore"];
        for f in &extra {
            if path.join(f).exists() {
                would_write.push(f);
            } else {
                would_create.push(f);
            }
        }

        if !would_write.is_empty() {
            ui.step_start(&format!(
                "{} file{} to overwrite",
                would_write.len(),
                if would_write.len() == 1 { "" } else { "s" }
            ));
            println!();
            for f in &would_write {
                ui.detail(f);
            }
        }
        if !would_create.is_empty() {
            ui.step_start(&format!(
                "{} new file{} to create",
                would_create.len(),
                if would_create.len() == 1 { "" } else { "s" }
            ));
            println!();
            for f in &would_create {
                ui.detail(f);
            }
        }

        println!();
        ui.detail("Dry run — no files were modified.");
        return Ok(());
    }

    ui.banner();

    let rules_dir = crosslink_dir.join("rules");

    if !crosslink_exists {
        ui.step_start("Initializing database");
        fs::create_dir_all(&crosslink_dir).context("Failed to create .crosslink directory")?;
        let db_path = crosslink_dir.join("issues.db");
        Database::open(&db_path)?;
        ui.step_ok(None);
    }

    let tui_choices = match tui_result {
        Some((mut config, choices)) => {
            apply_tui_choices(&mut config, &choices)?;
            let output = serde_json::to_string_pretty(&config)
                .context("Failed to serialize hook-config.json")?;
            fs::write(&config_path, format!("{output}\n"))
                .context("Failed to write hook-config.json")?;
            ui.step_created("hook-config.json");
            Some(choices)
        }
        None if !config_exists || force => {
            fs::write(&config_path, HOOK_CONFIG_JSON)
                .context("Failed to write hook-config.json")?;
            ui.step_created("hook-config.json");
            None
        }
        None => None,
    };

    ensure_repo_compact_id(&crosslink_dir)?;

    populate_agent_tool_commands(&config_path, path)?;

    populate_tracker_remote(&config_path, path)?;

    let crosslink_gitignore = crosslink_dir.join(".gitignore");
    if !crosslink_gitignore.exists() || force {
        fs::write(
            &crosslink_gitignore,
            "agent.json\n\
             repo-id\n\
             .hub-cache/\n\
             .knowledge-cache/\n\
             keys/\n\
             integrations/\n\
             runtime/\n\
             \n\
             hook-config.local.json\n\
             rules.local/\n\
             \n\
             .active-issue\n\
             .last-hydrated-ref\n\
             .promoted-uuids\n\
             promotion-log.json\n\
             hub-v3-shadow-stats.json\n\
             sentinel.log\n",
        )
        .context("Failed to write .crosslink/.gitignore")?;
    }

    ui.step_start("Configuring .gitignore");
    write_root_gitignore(path).context("Failed to update root .gitignore")?;
    ui.step_ok(None);

    let rules_exist = rules_dir.exists();
    if !rules_exist || force {
        ui.step_start("Deploying rules");
        fs::create_dir_all(&rules_dir).context("Failed to create .crosslink/rules directory")?;

        for (filename, content) in RULE_FILES {
            fs::write(rules_dir.join(filename), content)
                .with_context(|| format!("Failed to write {filename}"))?;
        }

        if force && rules_exist {
            ui.step_ok(Some("updated"));
        } else {
            ui.step_ok(Some(&format!("{} files", RULE_FILES.len())));
        }
    }

    let rules_local_dir = crosslink_dir.join("rules.local");
    if !rules_local_dir.exists() {
        fs::create_dir_all(&rules_local_dir)
            .context("Failed to create .crosslink/rules.local directory")?;
    }

    ui.step_start("Setting up agent integrations");
    for (relative_path, content) in &requested_files {
        let target = path.join(relative_path);
        let merge_aware = matches!(
            relative_path.as_str(),
            ".claude/settings.json"
                | ".mcp.json"
                | ".codex/hooks.json"
                | ".codex/config.toml"
                | "AGENTS.md"
        );
        if force || merge_aware || !target.exists() {
            write_managed_file(path, relative_path, content, &prefix)?;
        }
    }
    ui.step_ok(Some(match opts.integrations {
        IntegrationSelection::Claude => "Claude",
        IntegrationSelection::Codex => "Codex",
        IntegrationSelection::Both => "Claude + Codex",
    }));

    {
        let manifest_files: Vec<_> = managed_files(&prefix, IntegrationSelection::Both)?
            .into_iter()
            .filter(|(relative_path, _)| path.join(relative_path).exists())
            .collect();
        let m = manifest::build_manifest(&manifest_files);
        manifest::write_manifest(&crosslink_dir, &m)
            .context("Failed to write init-manifest.json")?;
    }

    if !skip_cpitd {
        ui.step_start("Checking cpitd");
        match install_cpitd(&prefix) {
            Ok(CpitdResult::InstalledFromPypi) => ui.step_ok(Some("installed")),
            Ok(CpitdResult::InstalledFromSource) => ui.step_ok(Some("installed from source")),
            Ok(CpitdResult::AlreadyInstalled) => ui.step_ok(Some("already installed")),
            Err(e) => {
                println!();
                ui.warn(&format!("Could not auto-install cpitd: {e}"));
                if e.externally_managed {
                    ui.detail(
                        "This Python is externally managed (PEP 668), so `pip install` is blocked.",
                    );
                    ui.detail("To install cpitd, pick one of:");
                    ui.detail("  1. Install pipx, then cpitd into its own isolated env:");
                    ui.detail("       apt install pipx   (or: brew install pipx)");
                    ui.detail("       pipx install cpitd");
                    ui.detail("  2. Create a virtualenv and install there:");
                    ui.detail("       python3 -m venv .venv && . .venv/bin/activate");
                    ui.detail("       pip install cpitd");
                    ui.detail("  3. Re-run init without cpitd: crosslink init --skip-cpitd");
                } else {
                    ui.detail(
                        "To install cpitd: pipx install cpitd  (or in a venv: pip install cpitd)",
                    );
                    ui.detail("Or re-run init without it: crosslink init --skip-cpitd");
                }
                ui.detail(
                    "cpitd is OPTIONAL: only `crosslink cpitd scan` and the sentinel cpitd source use it; everything else works without it.",
                );
            }
        }
    }

    if !skip_signing {
        setup_driver_signing(path, signing_key, &ui)?;
    }

    if crate::identity::AgentConfig::load(&crosslink_dir)?.is_none() {
        let agent_id = crate::utils::generate_compact_id();
        ui.step_start("Initializing agent identity");
        match init_agent_identity(&crosslink_dir, &agent_id) {
            Ok(()) => ui.step_ok(Some(&agent_id)),
            Err(e) => {
                println!();
                ui.warn(&format!("Could not auto-initialize agent: {e}"));
                ui.detail("Run `crosslink agent init <id>` manually to enable signing.");
            }
        }
    }

    if let Some(ref choices) = tui_choices {
        setup_shell_alias(&ui, choices);
    }

    ui.success();

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use merge::{GITIGNORE_SECTION_END, GITIGNORE_SECTION_START};
    use tempfile::tempdir;

    fn test_dir() -> tempfile::TempDir {
        let dir = tempdir().unwrap();
        let init = std::process::Command::new("git")
            .current_dir(dir.path())
            .args(["init"])
            .output()
            .expect("git init failed");
        assert!(init.status.success(), "git init failed");

        let commit = std::process::Command::new("git")
            .current_dir(dir.path())
            .args([
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@test",
                "commit",
                "--allow-empty",
                "-m",
                "init",
            ])
            .output()
            .expect("git commit failed");
        assert!(commit.status.success(), "git commit --allow-empty failed");
        dir
    }

    fn test_opts(force: bool) -> InitOpts<'static> {
        InitOpts {
            integrations: IntegrationSelection::Both,
            force,
            update: false,
            dry_run: false,
            no_prompt: false,
            python_prefix: None,
            skip_cpitd: true,
            skip_signing: true,
            signing_key: None,
            reconfigure: false,
            defaults: true,
        }
    }

    #[test]
    fn test_run_fresh_init() {
        let dir = test_dir();
        let result = run(dir.path(), &test_opts(false));
        assert!(result.is_ok());

        assert!(dir.path().join(".crosslink").exists());
        assert!(dir.path().join(".crosslink/rules").exists());
        assert!(dir.path().join(".crosslink/issues.db").exists());
        assert!(dir.path().join(".claude").exists());
        assert!(dir.path().join(".crosslink/integrations/hooks").exists());
        assert!(dir.path().join(".crosslink/integrations/mcp").exists());
        assert!(dir.path().join(".codex/hooks.json").exists());
        assert!(dir.path().join(".codex/config.toml").exists());
        assert!(dir.path().join(".agents/skills").exists());
        assert!(dir.path().join("AGENTS.md").exists());
        assert!(dir.path().join(".crosslink/hook-config.json").exists());
    }

    #[test]
    fn test_integration_selection_and_missing_integration_fill_are_idempotent() {
        for (selection, claude, codex) in [
            (IntegrationSelection::Claude, true, false),
            (IntegrationSelection::Codex, false, true),
            (IntegrationSelection::Both, true, true),
        ] {
            let dir = test_dir();
            let mut opts = test_opts(false);
            opts.integrations = selection;
            run(dir.path(), &opts).unwrap();
            assert_eq!(dir.path().join(".claude/settings.json").is_file(), claude);
            assert_eq!(dir.path().join(".codex/hooks.json").is_file(), codex);
            assert_eq!(dir.path().join(".agents/skills").is_dir(), codex);
            run(dir.path(), &opts).unwrap();
        }

        let dir = test_dir();
        run(dir.path(), &test_opts(false)).unwrap();
        fs::remove_file(dir.path().join(".codex/hooks.json")).unwrap();
        let original_config = fs::read(dir.path().join(".crosslink/hook-config.json")).unwrap();
        run(dir.path(), &test_opts(false)).unwrap();
        assert!(dir.path().join(".codex/hooks.json").is_file());
        assert_eq!(
            fs::read(dir.path().join(".crosslink/hook-config.json")).unwrap(),
            original_config
        );
    }

    #[test]
    fn test_codex_toml_merge_preserves_user_content_and_invalid_input_byte_for_byte() {
        let dir = test_dir();
        fs::create_dir_all(dir.path().join(".codex")).unwrap();
        let customized = "# user comment\nmodel = \"gpt-custom\"\n\n[profiles.careful]\napproval_policy = \"on-request\"\n\n[mcp_servers.user-server]\ncommand = \"user-mcp\"\n";
        fs::write(dir.path().join(".codex/config.toml"), customized).unwrap();
        run(dir.path(), &test_opts(false)).unwrap();
        let merged = fs::read_to_string(dir.path().join(".codex/config.toml")).unwrap();
        assert!(merged.contains("# user comment"));
        assert!(merged.contains("[profiles.careful]"));
        assert!(merged.contains("[mcp_servers.user-server]"));
        assert!(merged.contains("[mcp_servers.crosslink-knowledge]"));
        assert!(merged.contains("command = \"python3\""));

        let invalid = b"[broken\nexact bytes\xff".to_vec();
        fs::write(dir.path().join(".codex/config.toml"), &invalid).unwrap();
        let error = run(dir.path(), &test_opts(true)).unwrap_err().to_string();
        assert!(error.contains(".codex/config.toml") || error.contains("Codex config"));
        assert_eq!(
            fs::read(dir.path().join(".codex/config.toml")).unwrap(),
            invalid
        );
    }

    #[test]
    fn test_agents_managed_block_is_unique_and_preserves_user_text() {
        let dir = test_dir();
        let user_before = "# User instructions\nkeep-before\n\n";
        let user_after = "\n\nkeep-after\n";
        fs::write(
            dir.path().join("AGENTS.md"),
            format!(
                "{user_before}{}\nstale\n{}\n{}\nduplicate\n{}{user_after}",
                merge::AGENTS_SECTION_START,
                merge::AGENTS_SECTION_END,
                merge::AGENTS_SECTION_START,
                merge::AGENTS_SECTION_END,
            ),
        )
        .unwrap();
        run(dir.path(), &test_opts(false)).unwrap();
        let body = fs::read_to_string(dir.path().join("AGENTS.md")).unwrap();
        assert_eq!(body.matches(merge::AGENTS_SECTION_START).count(), 1);
        assert_eq!(body.matches(merge::AGENTS_SECTION_END).count(), 1);
        assert!(body.starts_with(user_before));
        assert!(body.ends_with(user_after));
    }

    #[test]
    fn test_codex_hook_trust_requires_manifest_matching_hooks_and_scripts() {
        let dir = test_dir();
        run(dir.path(), &test_opts(false)).unwrap();
        assert!(codex_hook_trust_ready(dir.path()).unwrap());
        let hooks = dir.path().join(".codex/hooks.json");
        let mut body = fs::read_to_string(&hooks).unwrap();
        body = body.replacen("crosslink:work-check", "crosslink:work-checK", 1);
        fs::write(&hooks, body).unwrap();
        assert!(!codex_hook_trust_ready(dir.path()).unwrap());
    }

    #[test]
    fn test_run_creates_hook_files() {
        let dir = test_dir();
        run(dir.path(), &test_opts(false)).unwrap();

        assert!(dir.path().join(".claude/settings.json").exists());
        assert!(dir
            .path()
            .join(".crosslink/integrations/hooks/prompt-guard.py")
            .exists());
        assert!(dir
            .path()
            .join(".crosslink/integrations/hooks/post-edit-check.py")
            .exists());
        assert!(dir
            .path()
            .join(".crosslink/integrations/hooks/session-start.py")
            .exists());
        assert!(dir
            .path()
            .join(".crosslink/integrations/hooks/pre-web-check.py")
            .exists());
        assert!(dir
            .path()
            .join(".crosslink/integrations/hooks/work-check.py")
            .exists());
        assert!(dir
            .path()
            .join(".crosslink/integrations/hooks/crosslink_config.py")
            .exists());
        assert!(dir
            .path()
            .join(".crosslink/integrations/hooks/hook_protocol.py")
            .exists());
        assert!(dir
            .path()
            .join(".crosslink/integrations/mcp/knowledge-server.py")
            .exists());
        assert!(dir
            .path()
            .join(".crosslink/integrations/mcp/agent-prompt-server.py")
            .exists());
        assert!(dir.path().join(".mcp.json").exists());
    }

    #[test]
    fn test_run_creates_rule_files() {
        let dir = test_dir();
        run(dir.path(), &test_opts(false)).unwrap();

        let rules_dir = dir.path().join(".crosslink/rules");
        assert!(rules_dir.join("global.md").exists());
        assert!(rules_dir.join("project.md").exists());
        assert!(rules_dir.join("rust.md").exists());
        assert!(rules_dir.join("python.md").exists());
        assert!(rules_dir.join("javascript.md").exists());
        assert!(rules_dir.join("typescript.md").exists());
        assert!(rules_dir.join("tracking-strict.md").exists());
        assert!(rules_dir.join("tracking-normal.md").exists());
        assert!(rules_dir.join("tracking-relaxed.md").exists());
    }

    #[test]
    fn test_run_already_initialized_no_force() {
        let dir = test_dir();

        run(dir.path(), &test_opts(false)).unwrap();

        let result = run(dir.path(), &test_opts(false));
        assert!(result.is_ok());
    }

    #[test]
    fn test_run_force_update() {
        let dir = test_dir();

        run(dir.path(), &test_opts(false)).unwrap();

        let hook_path = dir
            .path()
            .join(".crosslink/integrations/hooks/prompt-guard.py");
        fs::write(&hook_path, "# modified").unwrap();

        run(dir.path(), &test_opts(true)).unwrap();

        let content = fs::read_to_string(&hook_path).unwrap();
        assert_ne!(content, "# modified");
        assert!(content.contains("python") || content.contains("def") || content.len() > 20);
    }

    fn embedded_mcp_keys() -> Vec<String> {
        let embedded: serde_json::Value = serde_json::from_str(MCP_JSON).unwrap();
        embedded["mcpServers"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect()
    }

    #[test]
    fn test_force_init_preserves_existing_mcp_servers() {
        let dir = test_dir();
        run(dir.path(), &test_opts(false)).unwrap();

        let mcp_path = dir.path().join(".mcp.json");
        let mut content: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&mcp_path).unwrap()).unwrap();
        content["mcpServers"]["my-custom-server"] = serde_json::json!({
            "command": "node",
            "args": ["my-server.js"]
        });
        fs::write(&mcp_path, serde_json::to_string_pretty(&content).unwrap()).unwrap();

        run(dir.path(), &test_opts(true)).unwrap();

        let result: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&mcp_path).unwrap()).unwrap();
        let servers = result["mcpServers"].as_object().unwrap();

        for key in embedded_mcp_keys() {
            assert!(
                servers.contains_key(&key),
                "embedded key \"{key}\" should exist"
            );
        }
        assert!(
            servers.contains_key("my-custom-server"),
            "custom server should be preserved"
        );
        assert_eq!(
            servers["my-custom-server"]["command"].as_str().unwrap(),
            "node"
        );
    }

    #[test]
    fn test_force_init_returns_warnings_for_overwritten_keys() {
        let dir = test_dir();
        run(dir.path(), &test_opts(false)).unwrap();

        let mcp_path = dir.path().join(".mcp.json");
        let warnings = write_mcp_json_merged(&mcp_path).unwrap();

        let expected_keys = embedded_mcp_keys();
        assert_eq!(
            warnings.len(),
            expected_keys.len(),
            "should warn once per embedded key"
        );
        for key in &expected_keys {
            assert!(
                warnings.iter().any(|w| w.contains(key)),
                "should warn about overwriting \"{key}\""
            );
        }
    }

    #[test]
    fn test_write_mcp_json_merged_creates_fresh_file() {
        let dir = test_dir();
        let mcp_path = dir.path().join(".mcp.json");

        assert!(!mcp_path.exists());

        let warnings = write_mcp_json_merged(&mcp_path).unwrap();
        assert!(
            warnings.is_empty(),
            "fresh creation should produce no warnings"
        );

        let content: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&mcp_path).unwrap()).unwrap();
        let servers = content["mcpServers"].as_object().unwrap();

        let expected_keys = embedded_mcp_keys();
        assert_eq!(servers.len(), expected_keys.len());
        for key in &expected_keys {
            assert!(
                servers.contains_key(key),
                "fresh file should contain \"{key}\""
            );
        }
    }

    #[test]
    fn test_force_init_fails_on_malformed_mcp_json() {
        let dir = test_dir();
        run(dir.path(), &test_opts(false)).unwrap();

        let mcp_path = dir.path().join(".mcp.json");
        fs::write(&mcp_path, "not json {{{").unwrap();

        let result = run(dir.path(), &test_opts(true));
        assert!(result.is_err());
        let err = format!("{:#}", result.unwrap_err());
        assert!(
            err.contains("invalid JSON"),
            "Error should mention invalid JSON, got: {err}"
        );

        let content = fs::read_to_string(&mcp_path).unwrap();
        assert_eq!(content, "not json {{{");
    }

    #[test]
    fn test_force_init_fails_on_non_object_mcp_json() {
        let dir = test_dir();
        run(dir.path(), &test_opts(false)).unwrap();

        let mcp_path = dir.path().join(".mcp.json");
        fs::write(&mcp_path, "[1, 2, 3]").unwrap();

        let result = run(dir.path(), &test_opts(true));
        assert!(result.is_err());
        let err = format!("{:#}", result.unwrap_err());
        assert!(
            err.contains("not a JSON object"),
            "Error should mention not a JSON object, got: {err}"
        );

        let content = fs::read_to_string(&mcp_path).unwrap();
        assert_eq!(content, "[1, 2, 3]");
    }

    #[test]
    fn test_force_init_handles_empty_mcp_json_file() {
        let dir = test_dir();
        run(dir.path(), &test_opts(false)).unwrap();

        let mcp_path = dir.path().join(".mcp.json");
        fs::write(&mcp_path, "").unwrap();

        let result = run(dir.path(), &test_opts(true));
        assert!(result.is_err());
        let err = format!("{:#}", result.unwrap_err());
        assert!(
            err.contains("invalid JSON"),
            "Error should mention invalid JSON, got: {err}"
        );
    }

    #[test]
    fn test_force_init_fails_on_non_object_mcp_servers_value() {
        let dir = test_dir();
        run(dir.path(), &test_opts(false)).unwrap();

        let mcp_path = dir.path().join(".mcp.json");
        fs::write(&mcp_path, r#"{"mcpServers": "banana"}"#).unwrap();

        let result = run(dir.path(), &test_opts(true));
        assert!(result.is_err());
        let err = format!("{:#}", result.unwrap_err());
        assert!(
            err.contains("non-object mcpServers"),
            "Error should mention non-object mcpServers, got: {err}"
        );

        let content = fs::read_to_string(&mcp_path).unwrap();
        assert_eq!(content, r#"{"mcpServers": "banana"}"#);
    }

    #[test]
    fn test_init_merges_into_mcp_json_without_mcp_servers_key() {
        let dir = test_dir();
        run(dir.path(), &test_opts(false)).unwrap();

        let mcp_path = dir.path().join(".mcp.json");
        fs::write(&mcp_path, r#"{"someOtherKey": true}"#).unwrap();

        run(dir.path(), &test_opts(true)).unwrap();

        let content = fs::read_to_string(&mcp_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed["someOtherKey"], true);
        assert!(parsed["mcpServers"]["crosslink-knowledge"].is_object());
        assert!(parsed["mcpServers"]["crosslink-knowledge"].is_object());
    }

    #[test]
    fn test_run_partial_init_crosslink_only() {
        let dir = test_dir();

        fs::create_dir_all(dir.path().join(".crosslink")).unwrap();

        let result = run(dir.path(), &test_opts(false));
        assert!(result.is_ok());

        assert!(dir.path().join(".claude").exists());
    }

    #[test]
    fn test_run_partial_init_claude_only() {
        let dir = test_dir();

        fs::create_dir_all(dir.path().join(".claude")).unwrap();

        let result = run(dir.path(), &test_opts(false));
        assert!(result.is_ok());

        assert!(dir.path().join(".crosslink").exists());
    }

    #[test]
    fn test_run_database_usable() {
        let dir = test_dir();
        run(dir.path(), &test_opts(false)).unwrap();

        let db_path = dir.path().join(".crosslink/issues.db");
        let db = Database::open(&db_path).unwrap();

        let id = db.create_issue("Test issue", None, "medium").unwrap();
        assert!(id > 0);
    }

    #[test]
    fn test_run_rule_files_remain_present_and_zero_bytes() {
        let dir = test_dir();
        run(dir.path(), &test_opts(false)).unwrap();

        let rules_dir = dir.path().join(".crosslink/rules");

        let global = fs::read_to_string(rules_dir.join("global.md")).unwrap();
        assert!(global.is_empty());

        let rust = fs::read_to_string(rules_dir.join("rust.md")).unwrap();
        assert!(rust.is_empty());
    }

    #[test]
    fn test_run_force_updates_rules() {
        let dir = test_dir();
        run(dir.path(), &test_opts(false)).unwrap();

        let rule_path = dir.path().join(".crosslink/rules/global.md");
        fs::write(&rule_path, "# modified rule").unwrap();

        run(dir.path(), &test_opts(true)).unwrap();

        let content = fs::read_to_string(&rule_path).unwrap();
        assert_ne!(content, "# modified rule");
    }

    #[test]
    fn test_run_idempotent_with_force() {
        let dir = test_dir();

        for _ in 0..3 {
            let result = run(dir.path(), &test_opts(true));
            assert!(result.is_ok());
        }

        assert!(dir.path().join(".crosslink/issues.db").exists());
        assert!(dir.path().join(".claude/settings.json").exists());
    }

    #[test]
    #[allow(clippy::const_is_empty)]
    fn test_embedded_constants_not_empty() {
        assert!(!SETTINGS_JSON.is_empty());
        assert!(!PROMPT_GUARD_PY.is_empty());
        assert!(!POST_EDIT_CHECK_PY.is_empty());
        assert!(!SESSION_START_PY.is_empty());
        assert!(!PRE_WEB_CHECK_PY.is_empty());
        assert!(!WORK_CHECK_PY.is_empty());
        assert!(!CROSSLINK_CONFIG_PY.is_empty());
        assert!(!HEARTBEAT_PY.is_empty());
        assert!(!KNOWLEDGE_SERVER_PY.is_empty());
        assert!(!AGENT_PROMPT_SERVER_PY.is_empty());
        assert!(!MCP_JSON.is_empty());

        assert!(
            COMMAND_FILES.len() >= 11,
            "Expected at least 11 command files, found {}",
            COMMAND_FILES.len()
        );
        for (filename, content) in COMMAND_FILES {
            assert!(!content.is_empty(), "Command file {filename} is empty");
        }
        assert!(!HOOK_CONFIG_JSON.is_empty());
        assert!(RULE_TRACKING_STRICT.is_empty());
        assert!(RULE_TRACKING_NORMAL.is_empty());
        assert!(RULE_TRACKING_RELAXED.is_empty());
        assert!(RULE_GLOBAL.is_empty());
        assert!(RULE_RUST.is_empty());
    }

    #[test]
    fn test_rule_files_count() {
        assert!(RULE_FILES.len() >= 20);

        for (name, content) in RULE_FILES {
            assert!(!name.is_empty(), "Rule file name should not be empty");
            assert!(content.is_empty(), "Rule file {name} should be empty");
        }
    }

    #[test]
    fn test_gitignore_includes_local_config() {
        let dir = test_dir();
        run(dir.path(), &test_opts(false)).unwrap();

        let content = fs::read_to_string(dir.path().join(".crosslink/.gitignore")).unwrap();
        assert!(content.contains("agent.json"));
        assert!(content.contains(".hub-cache/"));
        assert!(content.contains("hook-config.local.json"));
    }

    #[test]
    fn test_detect_python_prefix_default() {
        let dir = test_dir();
        assert_eq!(detect_python_prefix(dir.path()), "python3");
    }

    #[test]
    fn test_detect_python_prefix_uv_lock() {
        let dir = test_dir();
        fs::write(dir.path().join("uv.lock"), "").unwrap();
        assert_eq!(detect_python_prefix(dir.path()), "uv run python3");
    }

    #[test]
    fn test_detect_python_prefix_uv_pyproject() {
        let dir = test_dir();
        fs::write(
            dir.path().join("pyproject.toml"),
            "[project]\nname = \"foo\"\n\n[tool.uv]\ndev-dependencies = []\n",
        )
        .unwrap();
        assert_eq!(detect_python_prefix(dir.path()), "uv run python3");
    }

    #[test]
    fn test_detect_python_prefix_poetry_lock() {
        let dir = test_dir();
        fs::write(dir.path().join("poetry.lock"), "").unwrap();
        assert_eq!(detect_python_prefix(dir.path()), "poetry run python3");
    }

    #[test]
    fn test_detect_python_prefix_poetry_pyproject() {
        let dir = test_dir();
        fs::write(
            dir.path().join("pyproject.toml"),
            "[project]\nname = \"foo\"\n\n[tool.poetry]\nname = \"foo\"\n",
        )
        .unwrap();
        assert_eq!(detect_python_prefix(dir.path()), "poetry run python3");
    }

    #[test]
    fn test_detect_python_prefix_venv() {
        let dir = test_dir();
        fs::create_dir(dir.path().join(".venv")).unwrap();
        let expected = if cfg!(target_os = "windows") {
            ".venv\\Scripts\\python.exe"
        } else {
            ".venv/bin/python3"
        };
        assert_eq!(detect_python_prefix(dir.path()), expected);
    }

    #[test]
    fn test_detect_python_prefix_pipenv() {
        let dir = test_dir();
        fs::write(dir.path().join("Pipfile"), "").unwrap();
        assert_eq!(detect_python_prefix(dir.path()), "pipenv run python3");
    }

    #[test]
    fn test_detect_python_prefix_pipenv_lock() {
        let dir = test_dir();
        fs::write(dir.path().join("Pipfile.lock"), "{}").unwrap();
        assert_eq!(detect_python_prefix(dir.path()), "pipenv run python3");
    }

    #[test]
    fn test_detect_python_prefix_uv_beats_poetry() {
        let dir = test_dir();

        fs::write(dir.path().join("uv.lock"), "").unwrap();
        fs::write(dir.path().join("poetry.lock"), "").unwrap();
        assert_eq!(detect_python_prefix(dir.path()), "uv run python3");
    }

    #[test]
    fn test_detect_python_prefix_poetry_beats_venv() {
        let dir = test_dir();
        fs::write(dir.path().join("poetry.lock"), "").unwrap();
        fs::create_dir(dir.path().join(".venv")).unwrap();
        assert_eq!(detect_python_prefix(dir.path()), "poetry run python3");
    }

    #[test]
    fn test_detect_python_prefix_venv_beats_pipenv() {
        let dir = test_dir();
        fs::create_dir(dir.path().join(".venv")).unwrap();
        fs::write(dir.path().join("Pipfile"), "").unwrap();
        let expected = if cfg!(target_os = "windows") {
            ".venv\\Scripts\\python.exe"
        } else {
            ".venv/bin/python3"
        };
        assert_eq!(detect_python_prefix(dir.path()), expected);
    }

    #[test]
    fn test_detect_python_prefix_pyproject_without_tools_is_default() {
        let dir = test_dir();

        fs::write(
            dir.path().join("pyproject.toml"),
            "[project]\nname = \"foo\"\nversion = \"1.0\"\n",
        )
        .unwrap();
        assert_eq!(detect_python_prefix(dir.path()), "python3");
    }

    #[test]
    fn test_settings_json_default_uses_python3() {
        let dir = test_dir();
        run(dir.path(), &test_opts(false)).unwrap();

        let content = fs::read_to_string(dir.path().join(".claude/settings.json")).unwrap();
        assert!(
            content.contains("python3"),
            "Default init should use python3 in settings.json"
        );
        assert!(
            !content.contains(PYTHON_PREFIX_PLACEHOLDER),
            "Placeholder should be replaced"
        );
    }

    #[test]
    fn test_settings_json_uv_project() {
        let dir = test_dir();
        fs::write(dir.path().join("uv.lock"), "").unwrap();
        run(dir.path(), &test_opts(false)).unwrap();

        let content = fs::read_to_string(dir.path().join(".claude/settings.json")).unwrap();
        assert!(
            content.contains("uv run python3"),
            "uv project should use 'uv run python3' in settings.json"
        );
    }

    #[test]
    fn test_settings_json_cli_override() {
        let dir = test_dir();
        fs::write(dir.path().join("uv.lock"), "").unwrap();

        run(
            dir.path(),
            &InitOpts {
                python_prefix: Some("custom-python"),
                ..test_opts(false)
            },
        )
        .unwrap();

        let content = fs::read_to_string(dir.path().join(".claude/settings.json")).unwrap();
        assert!(
            content.contains("custom-python"),
            "CLI override should be used in settings.json"
        );
        assert!(
            !content.contains("uv run python3"),
            "Auto-detected prefix should not appear when overridden"
        );
    }

    #[test]
    fn test_settings_json_produces_valid_json() {
        let dir = test_dir();
        fs::write(dir.path().join("uv.lock"), "").unwrap();
        run(dir.path(), &test_opts(false)).unwrap();

        let content = fs::read_to_string(dir.path().join(".claude/settings.json")).unwrap();
        let parsed: Result<serde_json::Value, _> = serde_json::from_str(&content);
        assert!(
            parsed.is_ok(),
            "Settings JSON should be valid after templating"
        );
    }

    #[test]
    fn test_force_re_detects_toolchain() {
        let dir = test_dir();

        run(dir.path(), &test_opts(false)).unwrap();
        let content = fs::read_to_string(dir.path().join(".claude/settings.json")).unwrap();
        assert!(content.contains("python3 \\\"$HOOK\\\""));

        fs::write(dir.path().join("uv.lock"), "").unwrap();
        run(dir.path(), &test_opts(true)).unwrap();
        let content = fs::read_to_string(dir.path().join(".claude/settings.json")).unwrap();
        assert!(
            content.contains("uv run python3"),
            "Force re-init should re-detect toolchain"
        );
    }

    fn embedded_allowed_tools() -> Vec<String> {
        let template: serde_json::Value = serde_json::from_str(SETTINGS_JSON).unwrap();
        template
            .get("allowedTools")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn test_settings_json_includes_allowed_tools() {
        let dir = test_dir();
        run(dir.path(), &test_opts(false)).unwrap();

        let content = fs::read_to_string(dir.path().join(".claude/settings.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        let tools = parsed["allowedTools"]
            .as_array()
            .expect("allowedTools should be an array");

        for expected in embedded_allowed_tools() {
            assert!(
                tools.iter().any(|v| v.as_str() == Some(&expected)),
                "allowedTools should contain \"{expected}\""
            );
        }
    }

    #[test]
    fn test_settings_json_includes_tmux_and_worktree_permissions() {
        let dir = test_dir();
        run(dir.path(), &test_opts(false)).unwrap();

        let content = fs::read_to_string(dir.path().join(".claude/settings.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        let tools: Vec<&str> = parsed["allowedTools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();

        assert!(
            tools.contains(&"Bash(tmux *)"),
            "allowedTools should include tmux permission"
        );
        assert!(
            tools.contains(&"Bash(git worktree *)"),
            "allowedTools should include git worktree permission"
        );
    }

    #[test]
    fn test_force_init_preserves_user_allowed_tools() {
        let dir = test_dir();
        run(dir.path(), &test_opts(false)).unwrap();

        let settings_path = dir.path().join(".claude/settings.json");
        let mut content: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
        content["allowedTools"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::Value::String("Bash(my-custom-tool *)".into()));
        fs::write(
            &settings_path,
            serde_json::to_string_pretty(&content).unwrap(),
        )
        .unwrap();

        run(dir.path(), &test_opts(true)).unwrap();

        let result: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
        let tools: Vec<&str> = result["allowedTools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();

        for expected in embedded_allowed_tools() {
            assert!(
                tools.contains(&expected.as_str()),
                "embedded tool \"{expected}\" should be preserved after force re-init"
            );
        }
        assert!(
            tools.contains(&"Bash(my-custom-tool *)"),
            "custom allowedTools entry should be preserved after force re-init"
        );
    }

    #[test]
    fn test_force_init_no_duplicate_allowed_tools() {
        let dir = test_dir();
        run(dir.path(), &test_opts(false)).unwrap();

        run(dir.path(), &test_opts(true)).unwrap();
        run(dir.path(), &test_opts(true)).unwrap();

        let settings_path = dir.path().join(".claude/settings.json");
        let content: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
        let tools: Vec<&str> = content["allowedTools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();

        for expected in embedded_allowed_tools() {
            let count = tools.iter().filter(|&&t| t == expected.as_str()).count();
            assert_eq!(
                count, 1,
                "\"{expected}\" should appear exactly once, found {count}"
            );
        }
    }

    #[test]
    fn test_settings_json_merge_fails_on_malformed_json() {
        let dir = test_dir();
        run(dir.path(), &test_opts(false)).unwrap();

        let settings_path = dir.path().join(".claude/settings.json");
        fs::write(&settings_path, "not json {{{").unwrap();

        let result = run(dir.path(), &test_opts(true));
        assert!(result.is_err());
        let err = format!("{:#}", result.unwrap_err());
        assert!(
            err.contains("invalid JSON"),
            "Error should mention invalid JSON, got: {err}"
        );

        let content = fs::read_to_string(&settings_path).unwrap();
        assert_eq!(content, "not json {{{");
    }

    #[test]
    fn test_settings_json_merge_fails_on_non_object() {
        let dir = test_dir();
        run(dir.path(), &test_opts(false)).unwrap();

        let settings_path = dir.path().join(".claude/settings.json");
        fs::write(&settings_path, "[1, 2, 3]").unwrap();

        let result = run(dir.path(), &test_opts(true));
        assert!(result.is_err());
        let err = format!("{:#}", result.unwrap_err());
        assert!(
            err.contains("not a JSON object"),
            "Error should mention not a JSON object, got: {err}"
        );
    }

    #[test]
    fn test_settings_json_merge_creates_fresh_file() {
        let dir = test_dir();
        let settings_path = dir.path().join(".claude/settings.json");
        fs::create_dir_all(dir.path().join(".claude")).unwrap();

        assert!(!settings_path.exists());

        write_settings_json_merged(&settings_path, "python3").unwrap();

        let content: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
        let tools: Vec<&str> = content["allowedTools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();

        for expected in embedded_allowed_tools() {
            assert!(
                tools.contains(&expected.as_str()),
                "fresh file should contain \"{expected}\""
            );
        }
    }

    #[test]
    fn test_init_creates_root_gitignore() {
        let dir = test_dir();
        run(dir.path(), &test_opts(false)).unwrap();

        let content = fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        assert!(content.contains(GITIGNORE_SECTION_START));
        assert!(content.contains(GITIGNORE_SECTION_END));
        assert!(content.contains(".crosslink/issues.db"));
        assert!(content.contains(".crosslink/agent.json"));
        assert!(content.contains(".crosslink/session.json"));
        assert!(content.contains(".crosslink/daemon.pid"));
        assert!(content.contains(".crosslink/keys/"));
        assert!(content.contains(".crosslink/.hub-cache/"));
        assert!(content.contains(".crosslink/hook-config.local.json"));
        assert!(content.contains(".worktrees/"));
        assert!(content.contains(".claude/commands/"));
        assert!(content.contains(".claude/skills/"));
        assert!(content.contains(".agents/skills/"));
        assert!(content.contains(".codex/hooks.json"));
    }

    #[test]
    fn test_root_gitignore_idempotent() {
        let dir = test_dir();
        run(dir.path(), &test_opts(false)).unwrap();

        let first = fs::read_to_string(dir.path().join(".gitignore")).unwrap();

        run(dir.path(), &test_opts(true)).unwrap();
        let second = fs::read_to_string(dir.path().join(".gitignore")).unwrap();

        assert_eq!(
            first, second,
            "Re-init should not duplicate gitignore entries"
        );
    }

    #[test]
    fn test_root_gitignore_preserves_user_entries() {
        let dir = test_dir();

        fs::write(dir.path().join(".gitignore"), "/target/\n*.log\n").unwrap();

        run(dir.path(), &test_opts(false)).unwrap();

        let content = fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        assert!(
            content.contains("/target/"),
            "User entries before managed section should be preserved"
        );
        assert!(
            content.contains("*.log"),
            "User entries before managed section should be preserved"
        );
        assert!(content.contains(GITIGNORE_SECTION_START));
        assert!(content.contains(".crosslink/issues.db"));
    }

    #[test]
    fn test_root_gitignore_preserves_entries_around_managed_section() {
        let dir = test_dir();

        run(dir.path(), &test_opts(false)).unwrap();

        let content = fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        let new_content =
            format!("# My custom rules\n/build/\n\n{content}\n# Trailing rules\n*.tmp\n");
        fs::write(dir.path().join(".gitignore"), new_content).unwrap();

        run(dir.path(), &test_opts(true)).unwrap();

        let result = fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        assert!(
            result.contains("/build/"),
            "Pre-section user entries preserved"
        );
        assert!(
            result.contains("*.tmp"),
            "Post-section user entries preserved"
        );
        assert!(
            result.contains(".crosslink/issues.db"),
            "Managed entries present"
        );

        assert_eq!(
            result.matches(GITIGNORE_SECTION_START).count(),
            1,
            "Should have exactly one managed section start marker"
        );
        assert_eq!(
            result.matches(GITIGNORE_SECTION_END).count(),
            1,
            "Should have exactly one managed section end marker"
        );
    }

    #[test]
    fn test_root_gitignore_uses_comment_free_managed_boundaries() {
        let dir = test_dir();
        run(dir.path(), &test_opts(false)).unwrap();

        let content = fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        assert!(content.contains(GITIGNORE_SECTION_START));
        assert!(content.contains(GITIGNORE_SECTION_END));
        assert!(content.contains(".crosslink/hook-config.local.json"));
        assert!(!content
            .lines()
            .any(|line| line.trim_start().starts_with('#')));
    }

    #[test]
    fn test_write_root_gitignore_fresh() {
        let dir = test_dir();
        write_root_gitignore(dir.path()).unwrap();

        let content = fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        assert!(content.starts_with(GITIGNORE_SECTION_START));
        assert!(content.contains(GITIGNORE_SECTION_END));
        assert!(content.contains(".crosslink/issues.db"));
    }

    #[test]
    fn test_write_root_gitignore_replaces_section() {
        let dir = test_dir();

        write_root_gitignore(dir.path()).unwrap();
        write_root_gitignore(dir.path()).unwrap();

        let content = fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        assert_eq!(
            content.matches(GITIGNORE_SECTION_START).count(),
            1,
            "Should have exactly one start marker after double write"
        );
    }

    #[test]
    fn test_crosslink_inner_gitignore_includes_integrations() {
        let dir = test_dir();
        run(dir.path(), &test_opts(false)).unwrap();

        let content = fs::read_to_string(dir.path().join(".crosslink/.gitignore")).unwrap();
        assert!(content.contains("integrations/"));
        assert!(!content
            .lines()
            .any(|line| line.trim_start().starts_with('#')));
    }

    #[test]
    fn test_crosslink_inner_gitignore_ignores_runtime_state() {
        let dir = test_dir();
        run(dir.path(), &test_opts(false)).unwrap();

        let inner = fs::read_to_string(dir.path().join(".crosslink/.gitignore")).unwrap();
        for entry in [
            ".active-issue",
            ".last-hydrated-ref",
            ".promoted-uuids",
            "promotion-log.json",
            "hub-v3-shadow-stats.json",
            "sentinel.log",
        ] {
            assert!(
                inner.contains(entry),
                "inner .crosslink/.gitignore missing runtime entry: {entry}"
            );
        }

        let root = fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        for entry in [
            ".crosslink/.last-hydrated-ref",
            ".crosslink/.promoted-uuids",
            ".crosslink/promotion-log.json",
            ".crosslink/hub-v3-shadow-stats.json",
            ".crosslink/sentinel.log",
        ] {
            assert!(
                root.contains(entry),
                "root .gitignore managed section missing runtime entry: {entry}"
            );
        }
    }

    #[test]
    fn test_init_deploys_skill_files() {
        let dir = test_dir();
        run(dir.path(), &test_opts(false)).unwrap();

        let commands_dir = dir.path().join(".claude/commands");
        assert!(
            commands_dir.join("maintain.md").exists(),
            "maintain.md skill not deployed"
        );
        assert!(
            commands_dir.join("design.md").exists(),
            "design.md skill not deployed"
        );

        let maintain = fs::read_to_string(commands_dir.join("maintain.md")).unwrap();
        assert!(!maintain.is_empty(), "maintain.md is empty");
        let design = fs::read_to_string(commands_dir.join("design.md")).unwrap();
        assert!(!design.is_empty(), "design.md is empty");
    }

    #[test]
    fn test_init_deploys_mcp_servers() {
        let dir = test_dir();
        run(dir.path(), &test_opts(false)).unwrap();

        let mcp_dir = dir.path().join(".crosslink/integrations/mcp");
        assert!(
            mcp_dir.join("knowledge-server.py").exists(),
            "knowledge-server.py not deployed"
        );
        assert!(
            mcp_dir.join("agent-prompt-server.py").exists(),
            "agent-prompt-server.py not deployed"
        );

        let mcp_content = fs::read_to_string(dir.path().join(".mcp.json")).unwrap();
        let mcp: serde_json::Value = serde_json::from_str(&mcp_content).unwrap();
        let servers = mcp["mcpServers"].as_object().unwrap();
        assert!(
            servers.contains_key("crosslink-knowledge"),
            ".mcp.json missing crosslink-knowledge"
        );
        assert!(
            servers.contains_key("crosslink-agent-prompt"),
            ".mcp.json missing crosslink-agent-prompt"
        );
        for (name, server) in servers {
            assert_eq!(
                server["command"], "python3",
                "{name} must use the Python runtime already required by Crosslink hooks"
            );
            assert_eq!(server["args"].as_array().map(Vec::len), Some(1));
        }

        let codex_content = fs::read_to_string(dir.path().join(".codex/config.toml")).unwrap();
        let codex: toml::Value = toml::from_str(&codex_content).unwrap();
        for name in ["crosslink-knowledge", "crosslink-agent-prompt"] {
            let server = &codex["mcp_servers"][name];
            assert_eq!(server["command"].as_str(), Some("python3"));
            assert_eq!(server["args"].as_array().map(Vec::len), Some(1));
        }
    }

    #[test]
    fn test_force_init_deploys_skill_files() {
        let dir = test_dir();

        run(dir.path(), &test_opts(false)).unwrap();

        let commands_dir = dir.path().join(".claude/commands");
        fs::remove_file(commands_dir.join("maintain.md")).unwrap();
        fs::remove_file(commands_dir.join("design.md")).unwrap();
        assert!(!commands_dir.join("maintain.md").exists());
        assert!(!commands_dir.join("design.md").exists());

        run(dir.path(), &test_opts(true)).unwrap();
        assert!(commands_dir.join("maintain.md").exists());
        assert!(commands_dir.join("design.md").exists());
    }

    fn update_opts() -> InitOpts<'static> {
        InitOpts {
            integrations: IntegrationSelection::Both,
            force: false,
            update: true,
            dry_run: false,
            no_prompt: true,
            python_prefix: None,
            skip_cpitd: true,
            skip_signing: true,
            signing_key: None,
            reconfigure: false,
            defaults: true,
        }
    }

    fn dry_run_opts() -> InitOpts<'static> {
        InitOpts {
            dry_run: true,
            ..update_opts()
        }
    }

    #[test]
    fn test_init_writes_manifest() {
        let dir = test_dir();
        run(dir.path(), &test_opts(false)).unwrap();

        let manifest_path = dir.path().join(".crosslink/init-manifest.json");
        assert!(manifest_path.exists(), "Manifest should be created on init");

        let content = fs::read_to_string(&manifest_path).unwrap();
        let manifest: serde_json::Value = serde_json::from_str(&content).unwrap();

        assert!(manifest.get("crosslink_version").is_some());
        assert!(manifest.get("initialized_at").is_some());
        assert!(manifest.get("files").is_some());

        let files = manifest["files"].as_object().unwrap();
        assert!(
            files.contains_key(".crosslink/integrations/hooks/prompt-guard.py"),
            "Manifest should track hook files"
        );
        assert!(
            files.contains_key(".claude/settings.json"),
            "Manifest should track settings.json"
        );
        assert!(
            files.contains_key(".crosslink/integrations/mcp/knowledge-server.py"),
            "Manifest should track knowledge MCP server"
        );
        assert!(
            files.contains_key(".crosslink/integrations/mcp/agent-prompt-server.py"),
            "Manifest should track agent-prompt MCP server"
        );
        assert!(files.contains_key(".mcp.json"));
        assert!(files.contains_key(".codex/hooks.json"));
        assert!(files.contains_key(".codex/config.toml"));
        assert!(files.contains_key("AGENTS.md"));
    }

    #[test]
    fn test_force_init_updates_manifest() {
        let dir = test_dir();
        run(dir.path(), &test_opts(false)).unwrap();

        let manifest_path = dir.path().join(".crosslink/init-manifest.json");
        let first = fs::read_to_string(&manifest_path).unwrap();

        run(dir.path(), &test_opts(true)).unwrap();

        let second = fs::read_to_string(&manifest_path).unwrap();

        let m1: serde_json::Value = serde_json::from_str(&first).unwrap();
        let m2: serde_json::Value = serde_json::from_str(&second).unwrap();
        assert_eq!(
            m1["files"], m2["files"],
            "File hashes should be identical across force re-inits"
        );
    }

    #[test]
    fn test_update_no_changes_needed() {
        let dir = test_dir();

        run(dir.path(), &test_opts(false)).unwrap();

        let result = run(dir.path(), &update_opts());
        assert!(result.is_ok());

        let content = fs::read_to_string(
            dir.path()
                .join(".crosslink/integrations/hooks/prompt-guard.py"),
        )
        .unwrap();
        assert!(content.contains("python") || content.contains("def") || content.len() > 20);
    }

    #[test]
    fn test_update_fails_without_init() {
        let dir = test_dir();

        let result = run(dir.path(), &update_opts());
        assert!(result.is_err());
        let err = format!("{:#}", result.unwrap_err());
        assert!(
            err.contains("not initialized"),
            "Should mention not initialized, got: {err}"
        );
    }

    #[test]
    fn test_update_preserves_user_modified_hook() {
        let dir = test_dir();
        run(dir.path(), &test_opts(false)).unwrap();

        let hook_path = dir
            .path()
            .join(".crosslink/integrations/hooks/prompt-guard.py");
        fs::write(&hook_path, "# user customization\nprint('hello')").unwrap();

        run(dir.path(), &update_opts()).unwrap();

        let content = fs::read_to_string(&hook_path).unwrap();
        assert_eq!(
            content, "# user customization\nprint('hello')",
            "User-modified hook should not be overwritten"
        );
    }

    #[test]
    fn test_update_skips_deleted_files() {
        let dir = test_dir();
        run(dir.path(), &test_opts(false)).unwrap();

        let hook_path = dir
            .path()
            .join(".crosslink/integrations/hooks/heartbeat.py");
        fs::remove_file(&hook_path).unwrap();

        run(dir.path(), &update_opts()).unwrap();

        assert!(
            !hook_path.exists(),
            "Deleted file should not be recreated by --update"
        );
    }

    #[test]
    fn test_update_dry_run_makes_no_changes() {
        let dir = test_dir();
        run(dir.path(), &test_opts(false)).unwrap();

        let hook_path = dir
            .path()
            .join(".crosslink/integrations/hooks/prompt-guard.py");
        let original = fs::read_to_string(&hook_path).unwrap();
        fs::write(&hook_path, "# modified").unwrap();

        let manifest_before =
            fs::read_to_string(dir.path().join(".crosslink/init-manifest.json")).unwrap();

        run(dir.path(), &dry_run_opts()).unwrap();

        let content = fs::read_to_string(&hook_path).unwrap();
        assert_eq!(content, "# modified", "Dry run should not modify files");

        let manifest_after =
            fs::read_to_string(dir.path().join(".crosslink/init-manifest.json")).unwrap();
        assert_eq!(
            manifest_before, manifest_after,
            "Dry run should not update manifest"
        );

        fs::write(&hook_path, original).unwrap();
    }

    #[test]
    fn test_force_dry_run_makes_no_changes() {
        let dir = test_dir();
        run(dir.path(), &test_opts(false)).unwrap();

        let hook_path = dir
            .path()
            .join(".crosslink/integrations/hooks/prompt-guard.py");
        fs::write(&hook_path, "# user-modified").unwrap();

        let manifest_before =
            fs::read_to_string(dir.path().join(".crosslink/init-manifest.json")).unwrap();

        let dry_force = InitOpts {
            force: true,
            dry_run: true,
            ..test_opts(true)
        };
        run(dir.path(), &dry_force).unwrap();

        let content = fs::read_to_string(&hook_path).unwrap();
        assert_eq!(
            content, "# user-modified",
            "Force dry-run should not overwrite files"
        );

        let manifest_after =
            fs::read_to_string(dir.path().join(".crosslink/init-manifest.json")).unwrap();
        assert_eq!(
            manifest_before, manifest_after,
            "Force dry-run should not update manifest"
        );
    }

    #[test]
    fn test_update_preserves_user_allowed_tools() {
        let dir = test_dir();
        run(dir.path(), &test_opts(false)).unwrap();

        let settings_path = dir.path().join(".claude/settings.json");
        let mut content: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
        content["allowedTools"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::Value::String("Bash(my-tool *)".into()));
        fs::write(
            &settings_path,
            serde_json::to_string_pretty(&content).unwrap(),
        )
        .unwrap();

        run(dir.path(), &update_opts()).unwrap();

        let result: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
        let has_custom_tool = result["allowedTools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .any(|t| t == "Bash(my-tool *)");
        assert!(
            has_custom_tool,
            "Custom allowedTools entry should survive --update"
        );
    }

    #[test]
    fn test_update_without_manifest_warns() {
        let dir = test_dir();
        run(dir.path(), &test_opts(false)).unwrap();

        fs::remove_file(dir.path().join(".crosslink/init-manifest.json")).unwrap();

        let result = run(dir.path(), &update_opts());
        assert!(result.is_ok());
    }

    #[test]
    fn test_gitignore_includes_manifest() {
        let dir = test_dir();
        run(dir.path(), &test_opts(false)).unwrap();

        let content = fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        assert!(
            content.contains("init-manifest.json"),
            "Gitignore should include init-manifest.json"
        );
    }

    #[test]
    fn test_manifest_tracks_all_managed_files() {
        let dir = test_dir();
        run(dir.path(), &test_opts(false)).unwrap();

        let manifest_content =
            fs::read_to_string(dir.path().join(".crosslink/init-manifest.json")).unwrap();
        let manifest: serde_json::Value = serde_json::from_str(&manifest_content).unwrap();
        let files = manifest["files"].as_object().unwrap();

        let hook_files = [
            "prompt-guard.py",
            "post-edit-check.py",
            "session-start.py",
            "pre-web-check.py",
            "work-check.py",
            "crosslink_config.py",
            "heartbeat.py",
        ];
        for hook in &hook_files {
            let key = format!(".crosslink/integrations/hooks/{hook}");
            assert!(files.contains_key(&key), "Manifest should track {key}");
        }

        assert!(files.contains_key(".crosslink/integrations/mcp/knowledge-server.py"));
        assert!(files.contains_key(".crosslink/integrations/mcp/agent-prompt-server.py"));
        assert!(files.contains_key(".mcp.json"));
        assert!(files.contains_key(".codex/hooks.json"));
        assert!(files.contains_key(".codex/config.toml"));
        assert!(files.contains_key("AGENTS.md"));

        assert!(files.contains_key(".claude/settings.json"));

        assert!(files.contains_key(".crosslink/rules/global.md"));
        assert!(files.contains_key(".crosslink/rules/rust.md"));
    }

    #[test]
    fn test_manifest_hashes_match_file_content() {
        use manifest::sha256_hex;

        let dir = test_dir();
        run(dir.path(), &test_opts(false)).unwrap();

        let manifest_content =
            fs::read_to_string(dir.path().join(".crosslink/init-manifest.json")).unwrap();
        let manifest: manifest::InitManifest = serde_json::from_str(&manifest_content).unwrap();

        let hook_path = dir
            .path()
            .join(".crosslink/integrations/hooks/prompt-guard.py");
        let on_disk = fs::read_to_string(&hook_path).unwrap();
        let expected_hash = sha256_hex(&on_disk);

        assert_eq!(
            manifest.files[".crosslink/integrations/hooks/prompt-guard.py"].sha256, expected_hash,
            "Manifest hash should match on-disk file for non-merged files"
        );
    }

    fn write_minimal_hook_config(dir: &Path, body: &str) -> std::path::PathBuf {
        let crosslink_dir = dir.join(".crosslink");
        fs::create_dir_all(&crosslink_dir).unwrap();
        let path = crosslink_dir.join("hook-config.json");
        fs::write(&path, body).unwrap();
        path
    }

    fn add_remote(repo: &Path, name: &str, url: &str) {
        let out = std::process::Command::new("git")
            .current_dir(repo)
            .args(["remote", "add", name, url])
            .output()
            .expect("git remote add failed");
        assert!(
            out.status.success(),
            "git remote add {name}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn test_detect_git_remotes_empty_when_no_remotes() {
        let dir = test_dir();
        let remotes = detect_git_remotes(dir.path());
        assert!(
            remotes.is_empty(),
            "fresh repo with no remotes should yield empty list; got {remotes:?}"
        );
    }

    #[test]
    fn test_detect_git_remotes_single() {
        let dir = test_dir();
        add_remote(dir.path(), "origin", "https://github.com/me/repo.git");
        assert_eq!(detect_git_remotes(dir.path()), vec!["origin".to_string()]);
    }

    #[test]
    fn test_detect_git_remotes_sorted() {
        let dir = test_dir();
        add_remote(dir.path(), "upstream", "https://github.com/up/r.git");
        add_remote(dir.path(), "fork", "https://github.com/me/r.git");
        add_remote(dir.path(), "origin", "https://github.com/me/r.git");

        assert_eq!(
            detect_git_remotes(dir.path()),
            vec![
                "fork".to_string(),
                "origin".to_string(),
                "upstream".to_string(),
            ]
        );
    }

    #[test]
    fn test_populate_tracker_remote_writes_single_remote() {
        let dir = test_dir();
        add_remote(dir.path(), "upstream", "https://example.com/r.git");
        let cfg = write_minimal_hook_config(dir.path(), "{}");

        populate_tracker_remote(&cfg, dir.path()).unwrap();

        let after: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(
            after.get("tracker_remote").and_then(|v| v.as_str()),
            Some("upstream"),
            "single remote should be selected unconditionally"
        );
    }

    #[test]
    fn test_populate_tracker_remote_prefers_origin_when_multiple() {
        let dir = test_dir();
        add_remote(dir.path(), "fork", "https://github.com/me/r.git");
        add_remote(dir.path(), "origin", "https://github.com/me/r.git");
        add_remote(dir.path(), "upstream", "https://github.com/up/r.git");
        let cfg = write_minimal_hook_config(dir.path(), "{}");

        populate_tracker_remote(&cfg, dir.path()).unwrap();

        let after: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(
            after.get("tracker_remote").and_then(|v| v.as_str()),
            Some("origin")
        );
    }

    #[test]
    fn test_populate_tracker_remote_falls_back_alphabetically_without_origin() {
        let dir = test_dir();
        add_remote(dir.path(), "upstream", "https://github.com/up/r.git");
        add_remote(dir.path(), "fork", "https://github.com/me/r.git");
        let cfg = write_minimal_hook_config(dir.path(), "{}");

        populate_tracker_remote(&cfg, dir.path()).unwrap();

        let after: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(
            after.get("tracker_remote").and_then(|v| v.as_str()),
            Some("fork"),
            "without origin, the alphabetically-first remote wins"
        );
    }

    #[test]
    fn test_populate_tracker_remote_writes_origin_when_no_remotes() {
        let dir = test_dir();
        let cfg = write_minimal_hook_config(dir.path(), "{}");

        populate_tracker_remote(&cfg, dir.path()).unwrap();

        let after: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(
            after.get("tracker_remote").and_then(|v| v.as_str()),
            Some("origin")
        );
    }

    #[test]
    fn test_populate_tracker_remote_upgrades_default_when_repo_has_non_origin_only() {
        let dir = test_dir();
        add_remote(dir.path(), "upstream", "https://example.com/r.git");
        let cfg = write_minimal_hook_config(dir.path(), r#"{"tracker_remote": "origin"}"#);

        populate_tracker_remote(&cfg, dir.path()).unwrap();

        let after: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(
            after.get("tracker_remote").and_then(|v| v.as_str()),
            Some("upstream"),
            "default 'origin' should be upgraded when the repo has a different real remote"
        );
    }

    #[test]
    fn test_populate_tracker_remote_byte_equal_noop_when_default_matches_reality() {
        let dir = test_dir();

        let cfg = write_minimal_hook_config(
            dir.path(),
            "{\n  \"a_key\": 1,\n  \"tracker_remote\": \"origin\",\n  \"z_key\": 2\n}",
        );
        let before = fs::read_to_string(&cfg).unwrap();
        populate_tracker_remote(&cfg, dir.path()).unwrap();
        let after = fs::read_to_string(&cfg).unwrap();
        assert_eq!(
            before, after,
            "file must be byte-identical when value matches detection"
        );

        add_remote(dir.path(), "origin", "https://example.com/r.git");
        populate_tracker_remote(&cfg, dir.path()).unwrap();
        let after_b = fs::read_to_string(&cfg).unwrap();
        assert_eq!(
            before, after_b,
            "file must stay byte-identical when current 'origin' matches detected 'origin'"
        );
    }

    #[test]
    fn test_populate_tracker_remote_preserves_manual_value() {
        let dir = test_dir();
        add_remote(dir.path(), "origin", "https://github.com/me/r.git");
        let cfg = write_minimal_hook_config(dir.path(), r#"{"tracker_remote": "custom-name"}"#);

        populate_tracker_remote(&cfg, dir.path()).unwrap();

        let after: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(
            after.get("tracker_remote").and_then(|v| v.as_str()),
            Some("custom-name"),
            "manually-set tracker_remote must survive populate (idempotency)"
        );
    }

    #[test]
    fn test_populate_tracker_remote_noop_when_config_missing() {
        let dir = test_dir();
        let cfg = dir.path().join(".crosslink/hook-config.json");

        populate_tracker_remote(&cfg, dir.path()).unwrap();
        assert!(!cfg.exists(), "should not create config when missing");
    }

    #[test]
    fn test_populate_tracker_remote_repairs_text_placeholder_with_detected() {
        let dir = test_dir();
        add_remote(dir.path(), "origin", "https://github.com/me/r.git");
        let cfg = write_minimal_hook_config(dir.path(), r#"{"tracker_remote": "(text)"}"#);

        populate_tracker_remote(&cfg, dir.path()).unwrap();

        let after: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(
            after.get("tracker_remote").and_then(|v| v.as_str()),
            Some("origin"),
            "corrupt '(text)' placeholder must be repaired to the detected remote"
        );
    }

    #[test]
    fn test_populate_tracker_remote_repairs_text_placeholder_without_remotes() {
        let dir = test_dir();
        let cfg = write_minimal_hook_config(dir.path(), r#"{"tracker_remote": "(text)"}"#);

        populate_tracker_remote(&cfg, dir.path()).unwrap();

        let after: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(
            after.get("tracker_remote").and_then(|v| v.as_str()),
            Some("origin"),
            "corrupt '(text)' must fall back to 'origin' when no remotes are detected"
        );
    }

    #[test]
    fn test_populate_tracker_remote_repairs_text_placeholder_to_non_origin() {
        let dir = test_dir();
        add_remote(dir.path(), "upstream", "https://example.com/r.git");
        let cfg = write_minimal_hook_config(dir.path(), r#"{"tracker_remote": "(text)"}"#);

        populate_tracker_remote(&cfg, dir.path()).unwrap();

        let after: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(
            after.get("tracker_remote").and_then(|v| v.as_str()),
            Some("upstream"),
            "corrupt '(text)' must upgrade to the only non-origin remote"
        );
    }
}
