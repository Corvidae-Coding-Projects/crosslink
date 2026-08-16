pub(crate) const HOOK_CONFIG_JSON: &str =
    include_str!("../../resources/crosslink/hook-config.json");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigType {
    Bool,
    Enum(&'static [&'static str]),
    String,
    StringArray,
    Integer,
    Map,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigGroup {
    Workflow,
    Security,
    Infrastructure,
    Agents,
    Sentinel,
}

impl ConfigGroup {
    pub const fn label(self) -> &'static str {
        match self {
            ConfigGroup::Workflow => "Workflow",
            ConfigGroup::Security => "Security",
            ConfigGroup::Infrastructure => "Infrastructure",
            ConfigGroup::Agents => "Agents",
            ConfigGroup::Sentinel => "Sentinel",
        }
    }

    pub const fn all() -> &'static [ConfigGroup] {
        &[
            ConfigGroup::Workflow,
            ConfigGroup::Security,
            ConfigGroup::Infrastructure,
            ConfigGroup::Agents,
            ConfigGroup::Sentinel,
        ]
    }
}

pub struct ConfigKey {
    pub key: &'static str,
    pub config_type: ConfigType,
    pub description: &'static str,
    pub group: ConfigGroup,
    pub hot_swappable: bool,
}

pub static REGISTRY: &[ConfigKey] = &[
    ConfigKey {
        key: "tracking_mode",
        config_type: ConfigType::Enum(&["strict", "normal", "relaxed"]),
        description: "How aggressively issue tracking is enforced before code changes",
        group: ConfigGroup::Workflow,
        hot_swappable: true,
    },
    ConfigKey {
        key: "intervention_tracking",
        config_type: ConfigType::Bool,
        description: "Log driver interventions for autonomy improvement",
        group: ConfigGroup::Agents,
        hot_swappable: true,
    },
    ConfigKey {
        key: "cpitd_auto_install",
        config_type: ConfigType::Bool,
        description: "Automatically install cpitd (context-provider) during init",
        group: ConfigGroup::Infrastructure,
        hot_swappable: false,
    },
    ConfigKey {
        key: "comment_discipline",
        config_type: ConfigType::Enum(&["encouraged", "required", "relaxed"]),
        description: "How strictly typed comments are enforced on issues",
        group: ConfigGroup::Workflow,
        hot_swappable: true,
    },
    ConfigKey {
        key: "kickoff_verification",
        config_type: ConfigType::Enum(&["local", "ci", "none"]),
        description: "Verification mode for agent kickoff tasks",
        group: ConfigGroup::Agents,
        hot_swappable: true,
    },
    ConfigKey {
        key: "agent.provider",
        config_type: ConfigType::Enum(&["claude", "codex", "custom"]),
        description: "Runtime agent protocol; installed integrations are configured separately",
        group: ConfigGroup::Agents,
        hot_swappable: true,
    },
    ConfigKey {
        key: "agent.binary",
        config_type: ConfigType::String,
        description: "Optional executable-path override for the selected provider",
        group: ConfigGroup::Agents,
        hot_swappable: true,
    },
    ConfigKey {
        key: "kickoff.allowed_tools",
        config_type: ConfigType::StringArray,
        description: "Extra Bash tool patterns appended to the kickoff agent's --allowedTools list",
        group: ConfigGroup::Agents,
        hot_swappable: true,
    },
    ConfigKey {
        key: "signing_enforcement",
        config_type: ConfigType::Enum(&["disabled", "audit", "enforced"]),
        description: "SSH signature verification level for coordination branch",
        group: ConfigGroup::Security,
        hot_swappable: false,
    },
    ConfigKey {
        key: "reminder_drift_threshold",
        config_type: ConfigType::Enum(&["0", "3", "5", "10", "15"]),
        description: "Prompts without crosslink usage before re-injecting reminder (0 = always)",
        group: ConfigGroup::Workflow,
        hot_swappable: true,
    },
    ConfigKey {
        key: "auto_steal_stale_locks",
        config_type: ConfigType::Enum(&["false", "2", "3", "5", "10"]),
        description: "Auto-steal stale locks after N * stale_timeout minutes (false = disabled)",
        group: ConfigGroup::Security,
        hot_swappable: true,
    },
    ConfigKey {
        key: "tracker_remote",
        config_type: ConfigType::String,
        description: "Git remote name for hub/knowledge branches (default: origin)",
        group: ConfigGroup::Infrastructure,
        hot_swappable: false,
    },
    ConfigKey {
        key: "blocked_git_commands",
        config_type: ConfigType::StringArray,
        description: "Git mutation commands blocked in all tracking modes",
        group: ConfigGroup::Infrastructure,
        hot_swappable: true,
    },
    ConfigKey {
        key: "gated_git_commands",
        config_type: ConfigType::StringArray,
        description: "Git commands allowed only with explicit user approval",
        group: ConfigGroup::Infrastructure,
        hot_swappable: true,
    },
    ConfigKey {
        key: "allowed_bash_prefixes",
        config_type: ConfigType::StringArray,
        description: "Bash commands that bypass the issue-required check",
        group: ConfigGroup::Infrastructure,
        hot_swappable: true,
    },
    ConfigKey {
        key: "external-cache-ttl",
        config_type: ConfigType::Integer,
        description: "TTL in seconds for cached external repo data (default: 300)",
        group: ConfigGroup::Infrastructure,
        hot_swappable: false,
    },
    ConfigKey {
        key: "external-url-ttl",
        config_type: ConfigType::Integer,
        description: "TTL in seconds for cached URL resolution results (default: 86400)",
        group: ConfigGroup::Infrastructure,
        hot_swappable: false,
    },
    ConfigKey {
        key: "repo-alias",
        config_type: ConfigType::Map,
        description: "Named aliases for external repositories (e.g. repo-alias.upstream)",
        group: ConfigGroup::Infrastructure,
        hot_swappable: false,
    },
    ConfigKey {
        key: "sentinel.enabled",
        config_type: ConfigType::Bool,
        description: "Enable the autonomous sentinel daemon",
        group: ConfigGroup::Sentinel,
        hot_swappable: true,
    },
    ConfigKey {
        key: "sentinel.interval_minutes",
        config_type: ConfigType::Integer,
        description: "Minutes between sentinel poll cycles (1-1440)",
        group: ConfigGroup::Sentinel,
        hot_swappable: true,
    },
    ConfigKey {
        key: "sentinel.max_concurrent_agents",
        config_type: ConfigType::Integer,
        description: "Maximum agents sentinel may run simultaneously (1-10)",
        group: ConfigGroup::Sentinel,
        hot_swappable: true,
    },
    ConfigKey {
        key: "sentinel.sources.github_labels.enabled",
        config_type: ConfigType::Bool,
        description: "Enable GitHub label polling source",
        group: ConfigGroup::Sentinel,
        hot_swappable: true,
    },
    ConfigKey {
        key: "sentinel.sources.github_labels.labels",
        config_type: ConfigType::StringArray,
        description: "GitHub labels that trigger sentinel dispatch (e.g. agent-todo: replicate)",
        group: ConfigGroup::Sentinel,
        hot_swappable: true,
    },
    ConfigKey {
        key: "sentinel.sources.cpitd.enabled",
        config_type: ConfigType::Bool,
        description: "Enable periodic cpitd clone-detection source (default off)",
        group: ConfigGroup::Sentinel,
        hot_swappable: true,
    },
    ConfigKey {
        key: "sentinel.sources.cpitd.interval_hours",
        config_type: ConfigType::Integer,
        description: "Hours between cpitd clone scans (default 168 = weekly)",
        group: ConfigGroup::Sentinel,
        hot_swappable: true,
    },
    ConfigKey {
        key: "sentinel.sources.cpitd.min_tokens",
        config_type: ConfigType::Integer,
        description: "Minimum token sequence length cpitd reports as a clone (default 50)",
        group: ConfigGroup::Sentinel,
        hot_swappable: true,
    },
    ConfigKey {
        key: "sentinel.default_agent.model",
        config_type: ConfigType::String,
        description: "Default model for sentinel-dispatched agents",
        group: ConfigGroup::Sentinel,
        hot_swappable: true,
    },
    ConfigKey {
        key: "sentinel.default_agent.timeout_minutes",
        config_type: ConfigType::Integer,
        description: "Default timeout in minutes for sentinel agents (5-480)",
        group: ConfigGroup::Sentinel,
        hot_swappable: true,
    },
    ConfigKey {
        key: "sentinel.default_agent.verify",
        config_type: ConfigType::Enum(&["local", "ci", "thorough"]),
        description: "Default verification level for sentinel agents",
        group: ConfigGroup::Sentinel,
        hot_swappable: true,
    },
    ConfigKey {
        key: "sentinel.escalation.enabled",
        config_type: ConfigType::Bool,
        description: "Enable automatic standard-to-advanced model escalation on failure",
        group: ConfigGroup::Sentinel,
        hot_swappable: true,
    },
    ConfigKey {
        key: "sentinel.escalation.model",
        config_type: ConfigType::String,
        description: "Model to escalate to on first-attempt failure",
        group: ConfigGroup::Sentinel,
        hot_swappable: true,
    },
    ConfigKey {
        key: "sentinel.escalation.cooldown_minutes",
        config_type: ConfigType::Integer,
        description: "Minutes to wait before retrying with escalated model (5-1440)",
        group: ConfigGroup::Sentinel,
        hot_swappable: true,
    },
    ConfigKey {
        key: "sentinel.escalation.max_attempts",
        config_type: ConfigType::Integer,
        description: "Maximum dispatch attempts per signal (1-5)",
        group: ConfigGroup::Sentinel,
        hot_swappable: true,
    },
    ConfigKey {
        key: "sentinel.escalation.timeout_multiplier_pct",
        config_type: ConfigType::Integer,
        description: "Timeout multiplier for escalation attempt as percentage (150 = 1.5x)",
        group: ConfigGroup::Sentinel,
        hot_swappable: true,
    },
    ConfigKey {
        key: "template_required_fields",
        config_type: ConfigType::Map,
        description: "Per-template required fields in issue descriptions (gh#658)",
        group: ConfigGroup::Workflow,
        hot_swappable: false,
    },
];

pub fn find_registry_key(key: &str) -> Option<&'static ConfigKey> {
    if let Some(entry) = REGISTRY.iter().find(|k| k.key == key) {
        return Some(entry);
    }
    if let Some(dot_pos) = key.find('.') {
        let prefix = &key[..dot_pos];
        if let Some(entry) = REGISTRY.iter().find(|k| k.key == prefix) {
            if matches!(entry.config_type, ConfigType::Map) {
                return Some(entry);
            }
        }
    }
    None
}

pub fn value_at_path<'a>(
    value: &'a serde_json::Value,
    path: &str,
) -> Option<&'a serde_json::Value> {
    path.split('.')
        .try_fold(value, |current, segment| current.get(segment))
        .or_else(|| value.get(path))
}

pub const fn type_label(ct: ConfigType) -> &'static str {
    match ct {
        ConfigType::Bool => "bool",
        ConfigType::Enum(_) => "enum",
        ConfigType::String => "string",
        ConfigType::StringArray => "string[]",
        ConfigType::Integer => "integer",
        ConfigType::Map => "map",
    }
}

pub static PRESET_TEAM: &[(&str, &str)] = &[
    ("tracking_mode", "strict"),
    ("comment_discipline", "required"),
    ("auto_steal_stale_locks", "3"),
    ("kickoff_verification", "ci"),
    ("signing_enforcement", "enforced"),
];

pub static PRESET_SOLO: &[(&str, &str)] = &[
    ("tracking_mode", "relaxed"),
    ("comment_discipline", "encouraged"),
    ("auto_steal_stale_locks", "false"),
    ("kickoff_verification", "local"),
    ("signing_enforcement", "disabled"),
];

use std::collections::HashMap;

pub struct WalkthroughCore {
    pub screen: usize,

    pub preset_selected: usize,

    pub group_selections: Vec<Vec<usize>>,

    pub group_names: Vec<&'static str>,

    pub group_keys: Vec<Vec<usize>>,

    pub group_cursor: usize,

    pub extra_screens: usize,
    pub finished: bool,
    pub cancelled: bool,
}

impl WalkthroughCore {
    pub fn new(current_config: &serde_json::Value, extra_screens: usize) -> Self {
        let groups = ConfigGroup::all();
        let mut group_names = Vec::new();
        let mut group_keys: Vec<Vec<usize>> = Vec::new();
        let mut group_selections: Vec<Vec<usize>> = Vec::new();

        for group in groups {
            let mut keys_in_group = Vec::new();
            let mut selections = Vec::new();

            for (idx, entry) in REGISTRY.iter().enumerate() {
                if entry.group != *group {
                    continue;
                }

                if matches!(
                    entry.config_type,
                    ConfigType::StringArray | ConfigType::Map | ConfigType::Integer
                ) {
                    continue;
                }
                keys_in_group.push(idx);
                let current_val = value_at_path(current_config, entry.key);
                let sel = match entry.config_type {
                    ConfigType::Bool => {
                        let val = current_val
                            .and_then(serde_json::Value::as_bool)
                            .unwrap_or(false);
                        usize::from(!val)
                    }
                    ConfigType::Enum(options) => {
                        let val = current_val.and_then(|v| v.as_str()).unwrap_or("");
                        options.iter().position(|o| *o == val).unwrap_or(0)
                    }
                    _ => 0,
                };
                selections.push(sel);
            }

            if !keys_in_group.is_empty() {
                group_names.push(group.label());
                group_keys.push(keys_in_group);
                group_selections.push(selections);
            }
        }

        Self {
            screen: 0,
            preset_selected: 2,
            group_selections,
            group_names,
            group_keys,
            group_cursor: 0,
            extra_screens,
            finished: false,
            cancelled: false,
        }
    }

    pub const fn total_screens(&self) -> usize {
        1 + self.group_names.len() + self.extra_screens + 1
    }

    pub const fn is_preset_screen(&self) -> bool {
        self.screen == 0
    }

    pub const fn is_confirm_screen(&self) -> bool {
        self.screen == self.total_screens() - 1
    }

    pub const fn extra_screen_idx(&self) -> Option<usize> {
        if self.extra_screens == 0 {
            return None;
        }
        let first_extra = 1 + self.group_names.len();
        if self.screen >= first_extra && self.screen < first_extra + self.extra_screens {
            Some(self.screen - first_extra)
        } else {
            None
        }
    }

    pub const fn current_group_idx(&self) -> Option<usize> {
        if self.screen >= 1 && self.screen < 1 + self.group_names.len() {
            Some(self.screen - 1)
        } else {
            None
        }
    }

    pub fn options_for_key(registry_idx: usize) -> Vec<&'static str> {
        let entry = &REGISTRY[registry_idx];
        match entry.config_type {
            ConfigType::Bool => vec!["true", "false"],
            ConfigType::Enum(opts) => opts.to_vec(),
            ConfigType::String => vec!["(text)"],
            _ => vec![],
        }
    }

    pub const fn move_up(&mut self) {
        if self.is_preset_screen() {
            self.preset_selected = self.preset_selected.saturating_sub(1);
        } else if let Some(gi) = self.current_group_idx() {
            self.group_cursor = self.group_cursor.saturating_sub(1);
            let _ = gi;
        }
    }

    pub fn move_down(&mut self) {
        if self.is_preset_screen() {
            if self.preset_selected < 2 {
                self.preset_selected += 1;
            }
        } else if let Some(gi) = self.current_group_idx() {
            let max = self.group_keys[gi].len().saturating_sub(1);
            if self.group_cursor < max {
                self.group_cursor += 1;
            }
        }
    }

    pub fn cycle_value(&mut self) {
        if let Some(gi) = self.current_group_idx() {
            if self.group_cursor < self.group_keys[gi].len() {
                let reg_idx = self.group_keys[gi][self.group_cursor];
                let options = Self::options_for_key(reg_idx);
                if !options.is_empty() {
                    let current = self.group_selections[gi][self.group_cursor];
                    self.group_selections[gi][self.group_cursor] = (current + 1) % options.len();
                }
            }
        }
    }

    pub fn confirm(&mut self) {
        if self.is_confirm_screen() {
            self.finished = true;
        } else if self.is_preset_screen() {
            if self.preset_selected < 2 {
                self.apply_preset_selections();

                self.screen = 1 + self.group_names.len();
            } else {
                self.screen = 1;
            }
            self.group_cursor = 0;
        } else {
            self.screen += 1;
            self.group_cursor = 0;
        }
    }

    pub const fn go_back(&mut self) {
        if self.screen > 0 {
            let first_extra = 1 + self.group_names.len();
            if self.screen == first_extra && self.preset_selected < 2 {
                self.screen = 0;
            } else {
                self.screen -= 1;
            }
            self.group_cursor = 0;
        }
    }

    pub fn apply_preset_selections(&mut self) {
        let preset = if self.preset_selected == 0 {
            PRESET_TEAM
        } else {
            PRESET_SOLO
        };
        for (key, value) in preset {
            for (gi, keys) in self.group_keys.iter().enumerate() {
                for (ki, &reg_idx) in keys.iter().enumerate() {
                    if REGISTRY[reg_idx].key == *key {
                        let options = Self::options_for_key(reg_idx);
                        if let Some(pos) = options.iter().position(|o| o == value) {
                            self.group_selections[gi][ki] = pos;
                        }
                    }
                }
            }
        }
    }

    pub fn build_config(&self) -> HashMap<String, serde_json::Value> {
        let mut result = HashMap::new();
        for (gi, keys) in self.group_keys.iter().enumerate() {
            for (ki, &reg_idx) in keys.iter().enumerate() {
                let entry = &REGISTRY[reg_idx];
                let options = Self::options_for_key(reg_idx);
                let selected = self.group_selections[gi][ki];
                if selected < options.len() {
                    let val_str = options[selected];
                    let val = match entry.config_type {
                        ConfigType::Bool => match val_str {
                            "true" => serde_json::Value::Bool(true),
                            _ => serde_json::Value::Bool(false),
                        },

                        ConfigType::String => continue,
                        _ => serde_json::Value::String(val_str.to_string()),
                    };
                    result.insert(entry.key.to_string(), val);
                }
            }
        }
        result
    }
}

#[cfg(test)]
mod walkthrough_core_tests {
    use super::*;

    #[test]
    fn build_config_never_emits_string_typed_keys_with_text_placeholder() {
        let empty = serde_json::json!({});
        let core = WalkthroughCore::new(&empty, 0);
        let built = core.build_config();

        let string_keys: Vec<&str> = REGISTRY
            .iter()
            .filter(|e| matches!(e.config_type, ConfigType::String))
            .map(|e| e.key)
            .collect();
        assert!(
            !string_keys.is_empty(),
            "registry must contain at least one ConfigType::String key for this test to be meaningful"
        );
        for key in &string_keys {
            assert!(
                !built.contains_key(*key),
                "GH#739: build_config emitted a value for ConfigType::String key '{key}' \
                 (would overwrite the template default with the UI placeholder '(text)')"
            );
        }

        for (k, v) in &built {
            assert_ne!(
                v.as_str(),
                Some("(text)"),
                "GH#739: build_config emitted the '(text)' UI placeholder as the value for '{k}'"
            );
        }
    }
}
