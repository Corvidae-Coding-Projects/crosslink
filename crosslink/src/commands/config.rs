use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::fs;
use std::io::IsTerminal;
use std::path::Path;

use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{self, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Padding, Paragraph},
    Frame, TerminalOptions, Viewport,
};

use crate::commands::config_registry::WalkthroughCore;
pub(crate) use crate::commands::config_registry::HOOK_CONFIG_JSON;
pub use crate::commands::config_registry::{
    find_registry_key, type_label, value_at_path, ConfigGroup, ConfigType, PRESET_SOLO,
    PRESET_TEAM, REGISTRY,
};
use crate::ConfigCommands;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Default,
    Team,
    Local,
}

impl Source {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Team => "team",
            Self::Local => "local",
        }
    }
}

pub struct ResolvedConfig {
    pub merged: serde_json::Value,
    pub provenance: HashMap<String, Source>,
    pub team: serde_json::Value,
    pub local: Option<serde_json::Value>,
}

fn set_value_at_path(
    root: &mut serde_json::Value,
    path: &str,
    value: serde_json::Value,
) -> Result<()> {
    let segments: Vec<&str> = path.split('.').collect();
    if segments.iter().any(|segment| segment.is_empty()) {
        bail!("Invalid config key: {path}");
    }
    if segments.len() > 1 {
        if let Some(object) = root.as_object_mut() {
            object.remove(path);
        }
    }
    let (last, parents) = segments
        .split_last()
        .ok_or_else(|| anyhow::anyhow!("Invalid config key: {path}"))?;
    let mut current = root;
    for segment in parents {
        let object = current
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("Config path {path} crosses a non-object value"))?;
        current = object
            .entry((*segment).to_string())
            .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    }
    let object = current
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("Config path {path} crosses a non-object value"))?;
    object.insert((*last).to_string(), value);
    Ok(())
}

fn value_at_path_mut<'a>(
    value: &'a mut serde_json::Value,
    path: &str,
) -> Option<&'a mut serde_json::Value> {
    let mut current = value;
    for segment in path.split('.') {
        current = current.as_object_mut()?.get_mut(segment)?;
    }
    Some(current)
}

fn normalize_layer(value: &mut serde_json::Value) -> Result<()> {
    let dotted_keys: Vec<String> = value
        .as_object()
        .into_iter()
        .flat_map(|object| object.keys())
        .filter(|key| key.contains('.') && find_registry_key(key).is_some())
        .cloned()
        .collect();
    for key in dotted_keys {
        let dotted_value = value
            .as_object_mut()
            .and_then(|object| object.remove(&key))
            .ok_or_else(|| anyhow::anyhow!("Config key disappeared while normalizing: {key}"))?;
        set_value_at_path(value, &key, dotted_value)?;
    }
    Ok(())
}

fn deep_merge(target: &mut serde_json::Value, overlay: &serde_json::Value, path: &str) {
    if !path.is_empty()
        && find_registry_key(path).is_some_and(|entry| matches!(entry.config_type, ConfigType::Map))
    {
        *target = overlay.clone();
        return;
    }
    if let (Some(target), Some(overlay)) = (target.as_object_mut(), overlay.as_object()) {
        for (key, value) in overlay {
            let child_path = if path.is_empty() {
                key.clone()
            } else {
                format!("{path}.{key}")
            };
            if let Some(existing) = target.get_mut(key) {
                deep_merge(existing, value, &child_path);
            } else {
                target.insert(key.clone(), value.clone());
            }
        }
    } else {
        *target = overlay.clone();
    }
}

fn merge_layer(target: &mut serde_json::Value, layer: &serde_json::Value, apply_additions: bool) {
    let mut regular = layer.clone();
    let additions: Vec<(String, serde_json::Value)> = if apply_additions {
        regular
            .as_object_mut()
            .map(|object| {
                let mut additions = Vec::new();
                object.retain(|key, value| {
                    if key.starts_with('+') {
                        additions.push((key.clone(), value.clone()));
                        false
                    } else {
                        true
                    }
                });
                additions
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    deep_merge(target, &regular, "");
    for (key, value) in additions {
        let path = key.trim_start_matches('+');
        if let (Some(existing), Some(extension)) =
            (value_at_path_mut(target, path), value.as_array())
        {
            if let Some(existing) = existing.as_array_mut() {
                for item in extension {
                    if !existing.contains(item) {
                        existing.push(item.clone());
                    }
                }
            }
        }
    }
}

pub fn read_config_layered(crosslink_dir: &Path) -> Result<ResolvedConfig> {
    let defaults = read_defaults()?;
    let team_path = crosslink_dir.join("hook-config.json");
    let local_path = crosslink_dir.join("hook-config.local.json");

    let mut team: serde_json::Value = if team_path.exists() {
        let content =
            fs::read_to_string(&team_path).context("Failed to read .crosslink/hook-config.json")?;
        serde_json::from_str(&content).context("Failed to parse hook-config.json")?
    } else {
        defaults.clone()
    };

    normalize_layer(&mut team)?;

    let mut local: Option<serde_json::Value> = if local_path.exists() {
        let content = fs::read_to_string(&local_path)
            .context("Failed to read .crosslink/hook-config.local.json")?;
        Some(serde_json::from_str(&content).context("Failed to parse hook-config.local.json")?)
    } else {
        None
    };

    if let Some(local) = &mut local {
        normalize_layer(local)?;
    }

    let mut merged = defaults.clone();
    let mut provenance: HashMap<String, Source> = HashMap::new();

    for entry in REGISTRY {
        provenance.insert(entry.key.to_string(), Source::Default);
    }

    merge_layer(&mut merged, &team, false);
    for entry in REGISTRY {
        if let Some(team_value) = value_at_path(&team, entry.key) {
            if Some(team_value) != value_at_path(&defaults, entry.key) {
                provenance.insert(entry.key.to_string(), Source::Team);
            }
        }
    }

    if let Some(ref local_val) = local {
        merge_layer(&mut merged, local_val, true);
        for entry in REGISTRY {
            let additive = format!("+{}", entry.key);
            if value_at_path(local_val, entry.key).is_some() || local_val.get(additive).is_some() {
                provenance.insert(entry.key.to_string(), Source::Local);
            }
        }
    }

    Ok(ResolvedConfig {
        merged,
        provenance,
        team,
        local,
    })
}

fn read_config(crosslink_dir: &Path) -> Result<serde_json::Value> {
    let resolved = read_config_layered(crosslink_dir)?;
    Ok(resolved.merged)
}

fn read_team_config(crosslink_dir: &Path) -> Result<serde_json::Value> {
    let path = crosslink_dir.join("hook-config.json");
    let content =
        fs::read_to_string(&path).context("Failed to read .crosslink/hook-config.json")?;
    let mut value = serde_json::from_str(&content).context("Failed to parse hook-config.json")?;
    normalize_layer(&mut value)?;
    Ok(value)
}

fn read_local_config(crosslink_dir: &Path) -> Result<Option<serde_json::Value>> {
    let path = crosslink_dir.join("hook-config.local.json");
    if !path.exists() {
        return Ok(None);
    }
    let content =
        fs::read_to_string(&path).context("Failed to read .crosslink/hook-config.local.json")?;
    let mut val =
        serde_json::from_str(&content).context("Failed to parse hook-config.local.json")?;
    normalize_layer(&mut val)?;
    Ok(Some(val))
}

#[derive(Debug, Clone, Copy)]
pub enum WriteScope {
    Team,
    Local,
}

fn write_config(crosslink_dir: &Path, config: &serde_json::Value) -> Result<()> {
    write_config_scoped(crosslink_dir, config, WriteScope::Team)
}

pub fn write_config_scoped(
    crosslink_dir: &Path,
    config: &serde_json::Value,
    scope: WriteScope,
) -> Result<()> {
    let filename = match scope {
        WriteScope::Team => "hook-config.json",
        WriteScope::Local => "hook-config.local.json",
    };
    let path = crosslink_dir.join(filename);
    let pretty = serde_json::to_string_pretty(config).context("Failed to serialize config")?;
    fs::write(&path, format!("{pretty}\n")).context(format!("Failed to write {filename}"))
}

fn read_defaults() -> Result<serde_json::Value> {
    serde_json::from_str(HOOK_CONFIG_JSON).context("embedded hook-config.json is invalid")
}

pub fn format_value(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Array(arr) => {
            let items: Vec<String> = arr
                .iter()
                .filter_map(|v| v.as_str().map(std::string::ToString::to_string))
                .collect();
            items.join(", ")
        }
        other => other.to_string(),
    }
}

fn apply_preset(crosslink_dir: &Path, preset: &[(&str, &str)]) -> Result<()> {
    let mut config = read_team_config(crosslink_dir)?;
    for (key, value) in preset {
        let entry = find_registry_key(key)
            .ok_or_else(|| anyhow::anyhow!("Preset references unknown key: {key}"))?;
        let json_val = match entry.config_type {
            ConfigType::Bool => match *value {
                "true" => serde_json::Value::Bool(true),
                "false" => serde_json::Value::Bool(false),
                _ => serde_json::Value::String(value.to_string()),
            },
            _ => serde_json::Value::String(value.to_string()),
        };
        set_value_at_path(&mut config, key, json_val)?;
    }
    write_config(crosslink_dir, &config)?;
    Ok(())
}

pub fn run(command: ConfigCommands, crosslink_dir: &Path) -> Result<()> {
    match command {
        ConfigCommands::Show => show(crosslink_dir),
        ConfigCommands::Get { key } => get(crosslink_dir, &key),
        ConfigCommands::Set {
            key,
            value,
            add,
            remove,
            local,
        } => set(
            crosslink_dir,
            &key,
            value.as_deref(),
            add.as_deref(),
            remove.as_deref(),
            local,
        ),
        ConfigCommands::List => list(),
        ConfigCommands::Reset { key } => reset(crosslink_dir, key.as_deref()),
        ConfigCommands::Diff => diff(crosslink_dir),
    }
}

pub fn run_bare(crosslink_dir: &Path, preset: Option<&str>) -> Result<()> {
    if let Some(name) = preset {
        match name {
            "team" => {
                apply_preset(crosslink_dir, PRESET_TEAM)?;
                println!("Applied 'team' preset.");
                show(crosslink_dir)?;
            }
            "solo" => {
                apply_preset(crosslink_dir, PRESET_SOLO)?;
                println!("Applied 'solo' preset.");
                show(crosslink_dir)?;
            }
            _ => bail!("Unknown preset: \"{name}\". Valid presets: team, solo"),
        }
        return Ok(());
    }

    if std::io::stdout().is_terminal() {
        interactive_walkthrough(crosslink_dir)
    } else {
        show(crosslink_dir)
    }
}

fn show(crosslink_dir: &Path) -> Result<()> {
    let resolved = read_config_layered(crosslink_dir)?;
    let defaults = read_defaults()?;

    for entry in REGISTRY {
        let current = value_at_path(&resolved.merged, entry.key);
        let source = resolved
            .provenance
            .get(entry.key)
            .copied()
            .unwrap_or(Source::Default);
        let current_display = current.map_or_else(|| "(unset)".into(), format_value);

        let team_val = value_at_path(&resolved.team, entry.key);
        let additive = format!("+{}", entry.key);
        let local_val = resolved
            .local
            .as_ref()
            .and_then(|local| value_at_path(local, entry.key).or_else(|| local.get(&additive)));
        let has_override = source == Source::Local && team_val.is_some() && local_val.is_some();

        if matches!(entry.config_type, ConfigType::StringArray) {
            println!(
                "{} ({}) {}:",
                entry.key,
                source.label(),
                type_label(entry.config_type)
            );
            if let Some(serde_json::Value::Array(arr)) = current {
                for item in arr {
                    if let Some(s) = item.as_str() {
                        println!("  - {s}");
                    }
                }
            }
        } else if matches!(entry.config_type, ConfigType::Map) {
            println!(
                "{} ({}) {}:",
                entry.key,
                source.label(),
                type_label(entry.config_type)
            );
            if let Some(serde_json::Value::Object(map)) = current {
                for (k, v) in map {
                    println!("  {}.{} = {}", entry.key, k, format_value(v));
                }
            }
        } else if has_override {
            let team_str = team_val.map(format_value).unwrap_or_default();
            println!(
                "{} = {} (local — overrides: {})",
                entry.key, current_display, team_str
            );
        } else {
            println!("{} = {} ({})", entry.key, current_display, source.label());
        }
    }

    if let Some(ref local_val) = resolved.local {
        if let Some(local_obj) = local_val.as_object() {
            for k in local_obj.keys() {
                let base_key = k.strip_prefix('+').unwrap_or(k);
                if find_registry_key(base_key).is_none()
                    && !defaults.as_object().is_some_and(|d| d.contains_key(k))
                {}
            }
        }
    }

    Ok(())
}

fn get(crosslink_dir: &Path, key: &str) -> Result<()> {
    if find_registry_key(key).is_none() {
        bail!("Unknown config key: \"{key}\". Run `crosslink config list` to see available keys.");
    }

    let config = read_config(crosslink_dir)?;

    match value_at_path(&config, key) {
        Some(serde_json::Value::Array(arr)) => {
            for item in arr {
                if let Some(s) = item.as_str() {
                    println!("{s}");
                }
            }
        }
        Some(serde_json::Value::Object(map)) => {
            for (k, v) in map {
                println!("{key}.{k} = {}", format_value(v));
            }
        }
        Some(v) => println!("{}", format_value(v)),
        None => println!("(unset)"),
    }
    Ok(())
}

fn set(
    crosslink_dir: &Path,
    key: &str,
    value: Option<&str>,
    add: Option<&str>,
    remove: Option<&str>,
    local: bool,
) -> Result<()> {
    let entry = find_registry_key(key).ok_or_else(|| {
        anyhow::anyhow!(
            "Unknown config key: \"{key}\". Run `crosslink config list` to see available keys."
        )
    })?;

    let scope = if local {
        WriteScope::Local
    } else {
        WriteScope::Team
    };

    let mut config = if local {
        read_local_config(crosslink_dir)?.unwrap_or_else(|| serde_json::json!({}))
    } else {
        read_team_config(crosslink_dir)?
    };

    match entry.config_type {
        ConfigType::Bool => {
            let val = value
                .ok_or_else(|| anyhow::anyhow!("Usage: crosslink config set {key} <true|false>"))?;
            match val {
                "true" => set_value_at_path(&mut config, key, serde_json::Value::Bool(true))?,
                "false" => set_value_at_path(&mut config, key, serde_json::Value::Bool(false))?,
                _ => {
                    bail!("Invalid value for {key}: expected \"true\" or \"false\", got \"{val}\"")
                }
            }
            write_config_scoped(crosslink_dir, &config, scope)?;
            println!("{key} = {val}");
        }
        ConfigType::Enum(valid) => {
            let val = value.ok_or_else(|| {
                anyhow::anyhow!("Usage: crosslink config set {key} <{}>", valid.join("|"))
            })?;
            if !valid.contains(&val) {
                bail!(
                    "Invalid value for {key}: \"{val}\". Valid values: {}",
                    valid.join(", ")
                );
            }
            set_value_at_path(&mut config, key, serde_json::Value::String(val.to_string()))?;
            write_config_scoped(crosslink_dir, &config, scope)?;
            println!("{key} = {val}");
        }
        ConfigType::String => {
            let val = value
                .ok_or_else(|| anyhow::anyhow!("Usage: crosslink config set {key} <value>"))?;
            set_value_at_path(&mut config, key, serde_json::Value::String(val.to_string()))?;
            write_config_scoped(crosslink_dir, &config, scope)?;
            println!("{key} = {val}");
        }
        ConfigType::Integer => {
            let val = value
                .ok_or_else(|| anyhow::anyhow!("Usage: crosslink config set {key} <number>"))?;
            let _: u64 = val.parse().map_err(|_| {
                anyhow::anyhow!(
                    "Invalid value for {key}: expected a non-negative integer, got \"{val}\""
                )
            })?;
            set_value_at_path(&mut config, key, serde_json::Value::String(val.to_string()))?;
            write_config_scoped(crosslink_dir, &config, scope)?;
            println!("{key} = {val}");
        }
        ConfigType::Map => {
            if let Some(dot_pos) = key.find('.') {
                let namespace = &key[..dot_pos];
                let subkey = &key[dot_pos + 1..];
                if subkey.is_empty() {
                    bail!("Usage: crosslink config set {namespace}.<name> <value>");
                }
                let val = value
                    .ok_or_else(|| anyhow::anyhow!("Usage: crosslink config set {key} <value>"))?;
                set_value_at_path(&mut config, key, serde_json::Value::String(val.to_string()))?;
                write_config_scoped(crosslink_dir, &config, scope)?;
                println!("{key} = {val}");
            } else {
                bail!("Usage: crosslink config set {key}.<name> <value>");
            }
        }
        ConfigType::StringArray => {
            if let Some(item) = add {
                let arr = value_at_path_mut(&mut config, key).and_then(|v| v.as_array_mut());
                if let Some(arr) = arr {
                    let already = arr.iter().any(|v| v.as_str() == Some(item));
                    if already {
                        println!("\"{item}\" already in {key}");
                    } else {
                        arr.push(serde_json::Value::String(item.to_string()));
                        write_config_scoped(crosslink_dir, &config, scope)?;
                        println!("Added \"{item}\" to {key}");
                    }
                } else {
                    set_value_at_path(
                        &mut config,
                        key,
                        serde_json::Value::Array(vec![serde_json::Value::String(item.to_string())]),
                    )?;
                    write_config_scoped(crosslink_dir, &config, scope)?;
                    println!("Added \"{item}\" to {key}");
                }
            } else if let Some(item) = remove {
                let arr = value_at_path_mut(&mut config, key)
                    .and_then(serde_json::Value::as_array_mut)
                    .ok_or_else(|| anyhow::anyhow!("{key} is not an array in config"))?;
                let before = arr.len();
                arr.retain(|v| v.as_str() != Some(item));
                if arr.len() == before {
                    println!("\"{item}\" not found in {key}");
                } else {
                    write_config_scoped(crosslink_dir, &config, scope)?;
                    println!("Removed \"{item}\" from {key}");
                }
            } else if let Some(val) = value {
                let items: Vec<serde_json::Value> = val
                    .split(',')
                    .map(|s| serde_json::Value::String(s.trim().to_string()))
                    .collect();
                set_value_at_path(&mut config, key, serde_json::Value::Array(items))?;
                write_config_scoped(crosslink_dir, &config, scope)?;
                println!("Set {key} to {val}");
            } else {
                bail!(
                    "Usage: crosslink config set {key} \"val1,val2,...\" or --add/--remove \"val\""
                );
            }
        }
    }
    Ok(())
}

fn list() -> Result<()> {
    let defaults = read_defaults()?;

    println!("{:<28} {:<10} {:<16} DESCRIPTION", "KEY", "TYPE", "GROUP");
    let sep = "-".repeat(90);
    println!("{sep}");

    for entry in REGISTRY {
        let default_str = value_at_path(&defaults, entry.key).map_or_else(
            || "(none)".into(),
            |v| match v {
                serde_json::Value::Array(a) => format!("[{} items]", a.len()),
                other => format_value(other),
            },
        );

        let hot = if entry.hot_swappable { " [hot]" } else { "" };

        println!(
            "{:<28} {:<10} {:<16} {} (default: {}){}",
            entry.key,
            type_label(entry.config_type),
            entry.group.label(),
            entry.description,
            default_str,
            hot,
        );
    }
    Ok(())
}

fn reset(crosslink_dir: &Path, key: Option<&str>) -> Result<()> {
    let defaults = read_defaults()?;

    if let Some(key) = key {
        if find_registry_key(key).is_none() {
            bail!(
                "Unknown config key: \"{key}\". Run `crosslink config list` to see available keys."
            );
        }
        let default_val = value_at_path(&defaults, key)
            .ok_or_else(|| anyhow::anyhow!("No default found for {key}"))?;
        let mut config = read_team_config(crosslink_dir)?;
        set_value_at_path(&mut config, key, default_val.clone())?;
        write_config(crosslink_dir, &config)?;
        println!("Reset {key} to default: {}", format_value(default_val));
    } else {
        write_config(crosslink_dir, &defaults)?;
        println!("Reset all config to defaults.");
    }
    Ok(())
}

fn diff(crosslink_dir: &Path) -> Result<()> {
    let resolved = read_config_layered(crosslink_dir)?;
    let defaults = read_defaults()?;
    let mut any_diff = false;

    for entry in REGISTRY {
        let current = value_at_path(&resolved.merged, entry.key);
        let default = value_at_path(&defaults, entry.key);
        let team_val = value_at_path(&resolved.team, entry.key);
        let additive = format!("+{}", entry.key);
        let local_val = resolved
            .local
            .as_ref()
            .and_then(|local| value_at_path(local, entry.key).or_else(|| local.get(&additive)));

        if current != default || local_val.is_some() {
            any_diff = true;
            let def_str = default.map_or_else(|| "(unset)".into(), format_value);
            let team_str = team_val.map_or_else(|| "(unset)".into(), format_value);

            if matches!(entry.config_type, ConfigType::StringArray) {
                println!("{} (modified):", entry.key);
                let cur_arr: Vec<&str> = current
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
                    .unwrap_or_default();
                let def_arr: Vec<&str> = default
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
                    .unwrap_or_default();

                for item in &cur_arr {
                    if !def_arr.contains(item) {
                        println!("  + {item}");
                    }
                }
                for item in &def_arr {
                    if !cur_arr.contains(item) {
                        println!("  - {item}");
                    }
                }
            } else if local_val.is_some() && team_val.is_some() {
                let local_str = local_val.map(format_value).unwrap_or_default();
                println!(
                    "{}: default: {}  team: {}  local: {}",
                    entry.key, def_str, team_str, local_str
                );
            } else {
                let cur_str = current.map_or_else(|| "(unset)".into(), format_value);
                println!("{}: {} (default: {})", entry.key, cur_str, def_str);
            }
        }
    }

    if !any_diff {
        println!("No differences from defaults.");
    }
    Ok(())
}

type WalkthroughApp = WalkthroughCore;

fn new_walkthrough_app(current_config: &serde_json::Value) -> WalkthroughApp {
    WalkthroughCore::new(current_config, 0)
}

fn draw_config_walkthrough(frame: &mut Frame, app: &WalkthroughApp) {
    let area = frame.area();

    let outer = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(Line::from(vec![
            Span::raw(" "),
            Span::styled(
                "crosslink",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" config ", Style::default().fg(Color::DarkGray)),
        ]))
        .padding(Padding::new(2, 2, 1, 1));

    let inner = outer.inner(area);
    frame.render_widget(outer, area);

    let total = app.total_screens();
    let progress_spans: Vec<Span> = (0..total)
        .map(|i| match i.cmp(&app.screen) {
            std::cmp::Ordering::Less => {
                Span::styled(" \u{25cf} ", Style::default().fg(Color::Green))
            }
            std::cmp::Ordering::Equal => Span::styled(
                " \u{25cf} ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            std::cmp::Ordering::Greater => {
                Span::styled(" \u{25cb} ", Style::default().fg(Color::DarkGray))
            }
        })
        .collect();

    if app.is_preset_screen() {
        draw_preset_screen(frame, app, inner, progress_spans);
    } else if app.is_confirm_screen() {
        draw_confirm_screen(frame, app, inner, progress_spans);
    } else if let Some(gi) = app.current_group_idx() {
        draw_group_screen(frame, app, gi, inner, progress_spans);
    }
}

fn draw_preset_screen(
    frame: &mut Frame,
    app: &WalkthroughApp,
    area: Rect,
    progress_spans: Vec<Span>,
) {
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .split(area);

    frame.render_widget(Paragraph::new(Line::from(progress_spans)), chunks[0]);

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "Quick-start presets",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ))),
        chunks[2],
    );

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "Choose a preset or configure each setting individually",
            Style::default().fg(Color::DarkGray),
        ))),
        chunks[3],
    );

    let presets = [
        ("Team", "strict tracking, CI verification, signing enforced"),
        ("Solo", "relaxed tracking, local verification, no signing"),
        ("Custom", "configure each setting individually"),
    ];

    let items: Vec<ListItem> = presets
        .iter()
        .enumerate()
        .map(|(i, (label, desc))| {
            let (marker, style) = if i == app.preset_selected {
                (
                    "\u{276f} ",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                ("  ", Style::default().fg(Color::Gray))
            };
            ListItem::new(Line::from(vec![
                Span::styled(marker, style),
                Span::styled(*label, style),
                Span::raw("  "),
                Span::styled(*desc, Style::default().fg(Color::DarkGray)),
            ]))
        })
        .collect();

    let list = List::new(items);
    let mut state = ListState::default().with_selected(Some(app.preset_selected));
    frame.render_stateful_widget(list, chunks[5], &mut state);

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "\u{2191}\u{2193} select  Enter confirm  Esc cancel",
            Style::default().fg(Color::DarkGray),
        ))),
        chunks[6],
    );
}

fn draw_group_screen(
    frame: &mut Frame,
    app: &WalkthroughApp,
    group_idx: usize,
    area: Rect,
    progress_spans: Vec<Span>,
) {
    let keys = &app.group_keys[group_idx];

    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(keys.len() as u16 + 1),
        Constraint::Length(2),
        Constraint::Length(1),
    ])
    .split(area);

    frame.render_widget(Paragraph::new(Line::from(progress_spans)), chunks[0]);

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            app.group_names[group_idx],
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ))),
        chunks[2],
    );

    let items: Vec<ListItem> = keys
        .iter()
        .enumerate()
        .map(|(ki, &reg_idx)| {
            let entry = &REGISTRY[reg_idx];
            let options = WalkthroughCore::options_for_key(reg_idx);
            let selected = app.group_selections[group_idx][ki];
            let val_str = if selected < options.len() {
                options[selected]
            } else {
                "?"
            };

            let (marker, style) = if ki == app.group_cursor {
                (
                    "\u{276f} ",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                ("  ", Style::default().fg(Color::Gray))
            };

            ListItem::new(Line::from(vec![
                Span::styled(marker, style),
                Span::styled(format!("{:<30}", entry.key), style),
                Span::styled(
                    format!("[{val_str}]"),
                    if ki == app.group_cursor {
                        Style::default().fg(Color::Yellow)
                    } else {
                        Style::default().fg(Color::DarkGray)
                    },
                ),
            ]))
        })
        .collect();

    let list = List::new(items);
    let mut state = ListState::default().with_selected(Some(app.group_cursor));
    frame.render_stateful_widget(list, chunks[4], &mut state);

    if app.group_cursor < keys.len() {
        let reg_idx = keys[app.group_cursor];
        let entry = &REGISTRY[reg_idx];
        let options = WalkthroughCore::options_for_key(reg_idx);
        let valid = if options.len() > 1 {
            format!("  Valid: {}", options.join(", "))
        } else {
            String::new()
        };
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    format!("  {}", entry.description),
                    Style::default().fg(Color::DarkGray),
                )),
                Line::from(Span::styled(valid, Style::default().fg(Color::DarkGray))),
            ]),
            chunks[5],
        );
    }

    let help = if app.screen > 1 {
        "\u{2191}\u{2193} navigate  Enter/\u{2192}/\u{2190} cycle value  Enter next  Backspace back  Esc cancel"
    } else {
        "\u{2191}\u{2193} navigate  Enter/\u{2192}/\u{2190} cycle value  Enter next  Esc cancel"
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            help,
            Style::default().fg(Color::DarkGray),
        ))),
        chunks[6],
    );
}

fn draw_confirm_screen(
    frame: &mut Frame,
    app: &WalkthroughApp,
    area: Rect,
    progress_spans: Vec<Span>,
) {
    let total_keys: usize = app.group_keys.iter().map(Vec::len).sum();

    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(total_keys as u16 + app.group_names.len() as u16 + 2),
        Constraint::Length(1),
    ])
    .split(area);

    frame.render_widget(Paragraph::new(Line::from(progress_spans)), chunks[0]);

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "Review your choices",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ))),
        chunks[2],
    );

    let mut lines: Vec<Line> = Vec::new();
    for (gi, keys) in app.group_keys.iter().enumerate() {
        lines.push(Line::from(Span::styled(
            format!("  {}", app.group_names[gi]),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )));
        for (ki, &reg_idx) in keys.iter().enumerate() {
            let entry = &REGISTRY[reg_idx];
            let options = WalkthroughCore::options_for_key(reg_idx);
            let selected = app.group_selections[gi][ki];
            let val_str = if selected < options.len() {
                options[selected]
            } else {
                "?"
            };
            lines.push(Line::from(vec![
                Span::styled("    \u{2713} ", Style::default().fg(Color::Green)),
                Span::styled(
                    format!("{}: ", entry.key),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(
                    val_str,
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
            ]));
        }
    }

    frame.render_widget(Paragraph::new(lines), chunks[4]);

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "Enter apply  Backspace go back  Esc cancel",
            Style::default().fg(Color::DarkGray),
        ))),
        chunks[5],
    );
}

fn interactive_walkthrough(crosslink_dir: &Path) -> Result<()> {
    let resolved = read_config_layered(crosslink_dir)?;
    let mut app = new_walkthrough_app(&resolved.merged);

    const WALKTHROUGH_HEIGHT: u16 = 24;
    enable_raw_mode().context("Failed to enable raw mode")?;
    let stdout = std::io::stdout();
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut terminal = ratatui::Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(WALKTHROUGH_HEIGHT),
        },
    )
    .context("Failed to create terminal")?;

    let result = (|| -> Result<()> {
        loop {
            terminal.draw(|f| draw_config_walkthrough(f, &app))?;

            if let Event::Key(key) = event::read().context("Failed to read terminal event")? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match key.code {
                    KeyCode::Up | KeyCode::Char('k') if !app.is_confirm_screen() => app.move_up(),
                    KeyCode::Down | KeyCode::Char('j') if !app.is_confirm_screen() => {
                        app.move_down();
                    }
                    KeyCode::Right if !app.is_preset_screen() && !app.is_confirm_screen() => {
                        app.cycle_value();
                    }
                    KeyCode::Left if !app.is_preset_screen() && !app.is_confirm_screen() => {
                        if let Some(gi) = app.current_group_idx() {
                            if app.group_cursor < app.group_keys[gi].len() {
                                let reg_idx = app.group_keys[gi][app.group_cursor];
                                let options = WalkthroughCore::options_for_key(reg_idx);
                                if !options.is_empty() {
                                    let current = app.group_selections[gi][app.group_cursor];
                                    app.group_selections[gi][app.group_cursor] = if current == 0 {
                                        options.len() - 1
                                    } else {
                                        current - 1
                                    };
                                }
                            }
                        }
                    }
                    KeyCode::Enter | KeyCode::Char(' ') => {
                        if !app.is_preset_screen() && !app.is_confirm_screen() {
                            if let Some(gi) = app.current_group_idx() {
                                if app.group_cursor + 1 < app.group_keys[gi].len() {
                                    app.group_cursor += 1;
                                } else {
                                    app.screen += 1;
                                    app.group_cursor = 0;
                                }
                            }
                        } else {
                            app.confirm();
                        }
                        if app.finished {
                            break;
                        }
                    }
                    KeyCode::Tab if !app.is_confirm_screen() => {
                        if app.is_preset_screen() {
                            app.confirm();
                        } else {
                            app.screen += 1;
                            app.group_cursor = 0;
                        }
                    }
                    KeyCode::Backspace => app.go_back(),
                    KeyCode::Esc | KeyCode::Char('q') => {
                        app.cancelled = true;
                        break;
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    })();

    {
        let area = terminal.get_frame().area();
        let backend = terminal.backend_mut();
        for row in area.y..area.y + area.height {
            execute!(
                backend,
                cursor::MoveTo(0, row),
                terminal::Clear(terminal::ClearType::CurrentLine)
            )
            .ok();
        }
        execute!(backend, cursor::MoveTo(0, area.y)).ok();
    }

    disable_raw_mode().ok();
    terminal.show_cursor().ok();

    result?;

    if app.cancelled {
        bail!("Config walkthrough cancelled");
    }

    let choices = app.build_config();
    let mut config = read_team_config(crosslink_dir)?;
    for (key, value) in choices {
        set_value_at_path(&mut config, &key, value)?;
    }
    write_config(crosslink_dir, &config)?;
    println!("Configuration saved.");
    show(crosslink_dir)?;
    Ok(())
}

pub fn detect_alias_status() -> (bool, String) {
    let shell = std::env::var("SHELL").unwrap_or_default();
    let home = std::env::var("HOME").unwrap_or_default();

    let (config_file, alias_line) = if shell.ends_with("fish") {
        (
            format!("{home}/.config/fish/config.fish"),
            "abbr -a xl crosslink",
        )
    } else if shell.ends_with("zsh") {
        (format!("{home}/.zshrc"), "alias xl='crosslink'")
    } else if shell.ends_with("bash") {
        let bashrc = format!("{home}/.bashrc");
        (bashrc, "alias xl='crosslink'")
    } else {
        return (false, String::new());
    };

    let path = std::path::Path::new(&config_file);
    if !path.exists() {
        return (false, config_file);
    }

    match fs::read_to_string(path) {
        Ok(content) => {
            let installed = content.lines().any(|line| line.trim() == alias_line);
            (installed, config_file)
        }
        Err(_) => (false, config_file),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn configured_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("hook-config.json"), HOOK_CONFIG_JSON).unwrap();
        dir
    }

    #[test]
    fn set_provider_updates_nested_agent_object() {
        let dir = configured_dir();
        set(
            dir.path(),
            "agent.provider",
            Some("codex"),
            None,
            None,
            false,
        )
        .unwrap();

        let value: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.path().join("hook-config.json")).unwrap())
                .unwrap();
        assert_eq!(
            value_at_path(&value, "agent.provider"),
            Some(&serde_json::json!("codex"))
        );
        assert!(value.get("agent.provider").is_none());
        assert!(value_at_path(&value, "agent.providers.codex").is_some());
    }

    #[test]
    fn local_nested_override_preserves_provider_options() {
        let dir = configured_dir();
        fs::write(
            dir.path().join("hook-config.local.json"),
            r#"{"agent":{"provider":"codex"}}"#,
        )
        .unwrap();

        let resolved = read_config_layered(dir.path()).unwrap();
        assert_eq!(
            value_at_path(&resolved.merged, "agent.provider"),
            Some(&serde_json::json!("codex"))
        );
        assert!(value_at_path(&resolved.merged, "agent.providers.claude").is_some());
        assert!(value_at_path(&resolved.merged, "agent.providers.codex").is_some());
        assert_eq!(
            resolved.provenance.get("agent.provider"),
            Some(&Source::Local)
        );
    }

    #[test]
    fn set_provider_migrates_flat_dotted_output() {
        let dir = configured_dir();
        let path = dir.path().join("hook-config.json");
        let mut value: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        value["agent.provider"] = serde_json::json!("codex");
        fs::write(&path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();

        set(
            dir.path(),
            "agent.provider",
            Some("codex"),
            None,
            None,
            false,
        )
        .unwrap();

        let migrated: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(
            value_at_path(&migrated, "agent.provider"),
            Some(&serde_json::json!("codex"))
        );
        assert!(migrated.get("agent.provider").is_none());
    }
}
