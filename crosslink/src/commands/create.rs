use anyhow::{bail, Context, Result};
use regex::Regex;

use crate::db::Database;
use crate::lock_check::{release_lock_best_effort, try_claim_lock, ClaimResult};
use crate::shared_writer::SharedWriter;
use crate::utils::format_issue_id;

const VALID_PRIORITIES: [&str; 4] = ["low", "medium", "high", "critical"];

pub struct Template {
    pub name: &'static str,
    pub priority: &'static str,
    pub label: &'static str,
    pub description_prefix: Option<&'static str>,
}

pub const TEMPLATES: &[Template] = &[
    Template {
        name: "bug",
        priority: "high",
        label: "bug",
        description_prefix: Some("Steps to reproduce:\n1. \n\nExpected: \nActual: "),
    },
    Template {
        name: "feature",
        priority: "medium",
        label: "feature",
        description_prefix: Some("Goal: \n\nAcceptance criteria:\n- "),
    },
    Template {
        name: "refactor",
        priority: "low",
        label: "refactor",
        description_prefix: Some("Current state: \n\nDesired state: \n\nReason: "),
    },
    Template {
        name: "research",
        priority: "low",
        label: "research",
        description_prefix: Some("Question: \n\nContext: \n\nFindings: "),
    },
    Template {
        name: "audit",
        priority: "high",
        label: "audit",
        description_prefix: Some("Scope: \n\nFiles to review: \n\nFindings: \n\nSeverity: "),
    },
    Template {
        name: "continuation",
        priority: "high",
        label: "continuation",
        description_prefix: Some("Previous session: \n\nCompleted: \n\nRemaining: \n\nBlockers: "),
    },
    Template {
        name: "investigation",
        priority: "medium",
        label: "investigation",
        description_prefix: Some(
            "Symptom: \n\nReproduction: \n\nHypotheses: \n\nRoot cause: \n\nFix: ",
        ),
    },
];

pub fn get_template(name: &str) -> Option<&'static Template> {
    TEMPLATES.iter().find(|t| t.name == name)
}

pub fn list_templates() -> Vec<&'static str> {
    TEMPLATES.iter().map(|t| t.name).collect()
}

pub fn validate_priority(priority: &str) -> bool {
    VALID_PRIORITIES.contains(&priority)
}

#[derive(Debug)]
pub struct RequiredFieldRule {
    pub field: String,

    pub pattern: Regex,

    pub min_chars: usize,
}

fn parse_required_fields(
    config: &serde_json::Value,
    template_name: &str,
) -> Result<Vec<RequiredFieldRule>> {
    let Some(map) = config
        .get("template_required_fields")
        .and_then(|v| v.as_object())
    else {
        return Ok(Vec::new());
    };

    let mut requested = Vec::new();
    for (tmpl, entries) in map {
        let arr = entries.as_array().ok_or_else(|| {
            anyhow::anyhow!("template_required_fields.{tmpl} must be an array of rule objects")
        })?;
        for (idx, entry) in arr.iter().enumerate() {
            let rule = parse_one_rule(tmpl, idx, entry)?;
            if tmpl == template_name {
                requested.push(rule);
            }
        }
    }
    Ok(requested)
}

fn parse_one_rule(tmpl: &str, idx: usize, entry: &serde_json::Value) -> Result<RequiredFieldRule> {
    let obj = entry.as_object().ok_or_else(|| {
        anyhow::anyhow!("template_required_fields.{tmpl}[{idx}] must be an object")
    })?;

    let field = obj
        .get("field")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            anyhow::anyhow!("template_required_fields.{tmpl}[{idx}] is missing string key 'field'")
        })?
        .to_string();

    let pattern_str = obj.get("pattern").and_then(|v| v.as_str()).ok_or_else(|| {
        anyhow::anyhow!(
            "template_required_fields.{tmpl}[{idx}] ('{field}') is missing string key 'pattern'"
        )
    })?;
    let pattern = Regex::new(pattern_str).with_context(|| {
        format!("Invalid regex in template_required_fields.{tmpl}[{idx}] ('{field}')")
    })?;

    let min_chars = match obj.get("min_chars") {
        None => 0,
        Some(v) => v.as_u64().ok_or_else(|| {
            anyhow::anyhow!(
                "template_required_fields.{tmpl}[{idx}] ('{field}') key 'min_chars' \
                 must be a non-negative integer"
            )
        })? as usize,
    };

    Ok(RequiredFieldRule {
        field,
        pattern,
        min_chars,
    })
}

pub fn load_required_fields(
    crosslink_dir: &std::path::Path,
    template_name: &str,
) -> Result<Vec<RequiredFieldRule>> {
    let resolved = crate::commands::config::read_config_layered(crosslink_dir)?;
    parse_required_fields(&resolved.merged, template_name)
}

pub fn validate_required_fields(
    rules: &[RequiredFieldRule],
    description: Option<&str>,
) -> Result<()> {
    let desc = description.unwrap_or("");
    for rule in rules {
        let Some(caps) = rule.pattern.captures(desc) else {
            bail!(
                "Issue description is missing required field '{}' (must match /{}/). \
                 Add it with -d, or pass --force to bypass.",
                rule.field,
                rule.pattern.as_str()
            );
        };

        let content = caps
            .get(1)
            .or_else(|| caps.get(0))
            .map_or("", |m| m.as_str());
        let len = content.chars().count();
        if len < rule.min_chars {
            bail!(
                "Required field '{}' is too short: {} characters, needs at least {}. \
                 Pass --force to bypass.",
                rule.field,
                len,
                rule.min_chars
            );
        }
    }
    Ok(())
}

fn enforce_required_fields(
    opts: &CreateOpts<'_>,
    template: Option<&str>,
    description: Option<&str>,
) -> Result<()> {
    if opts.force {
        return Ok(());
    }
    let (Some(tmpl_name), Some(dir)) = (template, opts.crosslink_dir) else {
        return Ok(());
    };
    let rules = load_required_fields(dir, tmpl_name)?;
    validate_required_fields(&rules, description)
}

struct AppliedTemplate {
    priority: String,
    description: Option<String>,
    label: Option<&'static str>,
}

fn apply_template(
    template: Option<&str>,
    priority: &str,
    description: Option<&str>,
) -> Result<AppliedTemplate> {
    let Some(tmpl_name) = template else {
        return Ok(AppliedTemplate {
            priority: priority.to_string(),
            description: description.map(ToString::to_string),
            label: None,
        });
    };

    let tmpl = get_template(tmpl_name).ok_or_else(|| {
        anyhow::anyhow!(
            "Unknown template '{}'. Available: {}",
            tmpl_name,
            list_templates().join(", ")
        )
    })?;

    let priority = if priority == "medium" {
        tmpl.priority
    } else {
        priority
    };

    let desc = match (tmpl.description_prefix, description) {
        (Some(prefix), Some(user_desc)) => Some(format!("{prefix}\n\n{user_desc}")),
        (Some(prefix), None) => Some(prefix.to_string()),
        (None, user_desc) => user_desc.map(ToString::to_string),
    };

    Ok(AppliedTemplate {
        priority: priority.to_string(),
        description: desc,
        label: Some(tmpl.label),
    })
}

fn auto_claim_and_set_work(
    db: &Database,
    id: i64,
    title: &str,
    crosslink_dir: Option<&std::path::Path>,
    quiet: bool,
) -> Result<()> {
    let mut freshly_claimed = false;

    if let Some(dir) = crosslink_dir {
        crate::lock_check::enforce_lock(dir, id, db)?;

        match try_claim_lock(dir, id, None) {
            Ok(ClaimResult::Claimed) => {
                freshly_claimed = true;
                if !quiet {
                    println!("Auto-claimed lock on issue {}", format_issue_id(id));
                }
            }
            Ok(ClaimResult::AlreadyHeld | ClaimResult::NotConfigured) => {}
            Ok(ClaimResult::Contended { winner_agent_id }) => {
                tracing::warn!(
                    "Lock on {} won by '{}'",
                    format_issue_id(id),
                    winner_agent_id
                );
            }
            Err(e) => tracing::warn!("Could not auto-claim lock: {}", e),
        }
    }

    let agent_id = crosslink_dir.and_then(|dir| {
        crate::identity::AgentConfig::load(dir)
            .ok()
            .flatten()
            .map(|a| a.agent_id)
    });
    if let Ok(Some(session)) = db.get_current_session_for_agent(agent_id.as_deref()) {
        if let Err(e) = db.set_session_issue(session.id, id) {
            if freshly_claimed {
                if let Some(dir) = crosslink_dir {
                    release_lock_best_effort(dir, id);
                }
            }
            return Err(e);
        }

        if let Some(dir) = crosslink_dir {
            crate::commands::session::write_active_issue_sentinel(dir, id);
        }
        if !quiet {
            println!("Now working on: {} {}", format_issue_id(id), title);
        }
    } else if !quiet {
        tracing::warn!("--work specified but no active session");
    }

    Ok(())
}

pub struct CreateOpts<'a> {
    pub labels: &'a [String],
    pub work: bool,
    pub quiet: bool,

    pub crosslink_dir: Option<&'a std::path::Path>,

    pub defer_id: bool,

    pub force: bool,
}

#[allow(clippy::too_many_arguments)]
pub fn run(
    db: &Database,
    writer: Option<&SharedWriter>,
    title: &str,
    description: Option<&str>,
    priority: &str,
    template: Option<&str>,
    scheduled_at: Option<chrono::DateTime<chrono::Utc>>,
    due_at: Option<chrono::DateTime<chrono::Utc>>,
    opts: &CreateOpts<'_>,
) -> Result<()> {
    if let (Some(s), Some(d)) = (scheduled_at, due_at) {
        if s > d {
            eprintln!(
                "Warning: --scheduled ({}) is after --due ({}). Proceeding anyway.",
                s.format("%Y-%m-%d"),
                d.format("%Y-%m-%d")
            );
        }
    }

    let AppliedTemplate {
        priority: final_priority,
        description: final_description,
        label: template_label,
    } = apply_template(template, priority, description)?;

    enforce_required_fields(opts, template, final_description.as_deref())?;

    if !validate_priority(&final_priority) {
        bail!(
            "Invalid priority '{}'. Must be one of: {}",
            final_priority,
            VALID_PRIORITIES.join(", ")
        );
    }

    let id = if let Some(w) = writer {
        let id = w.create_issue(
            db,
            title,
            final_description.as_deref(),
            &final_priority,
            scheduled_at,
            due_at,
        )?;

        if let Some(lbl) = template_label {
            w.add_label(db, id, lbl)?;
        }

        for lbl in opts.labels {
            w.add_label(db, id, lbl)?;
        }

        id
    } else {
        if scheduled_at.is_some() || due_at.is_some() {
            bail!(
                "Scheduling dates require the shared-writer path. \
                 Run `crosslink agent init <id>` first to enable it."
            );
        }

        db.transaction(|| {
            let id = db.create_issue(title, final_description.as_deref(), &final_priority)?;

            if let Some(lbl) = template_label {
                db.add_label(id, lbl)?;
            }

            for lbl in opts.labels {
                db.add_label(id, lbl)?;
            }

            Ok(id)
        })?
    };

    if opts.defer_id && !opts.quiet {
        println!(
            "Created issue {} (display ID deferred — assigned on next compaction)",
            format_issue_id(id)
        );
    } else if opts.quiet {
        println!("{id}");
    } else {
        println!("Created issue {}", format_issue_id(id));
        if let Some(tmpl) = template {
            println!("  Applied template: {tmpl}");
        }
    }

    if opts.work {
        auto_claim_and_set_work(db, id, title, opts.crosslink_dir, opts.quiet)?;
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn run_subissue(
    db: &Database,
    writer: Option<&SharedWriter>,
    parent_id: i64,
    title: &str,
    description: Option<&str>,
    priority: &str,
    template: Option<&str>,
    opts: &CreateOpts<'_>,
) -> Result<()> {
    let AppliedTemplate {
        priority: final_priority,
        description: final_description,
        label: template_label,
    } = apply_template(template, priority, description)?;

    enforce_required_fields(opts, template, final_description.as_deref())?;

    if !validate_priority(&final_priority) {
        bail!(
            "Invalid priority '{}'. Must be one of: {}",
            final_priority,
            VALID_PRIORITIES.join(", ")
        );
    }

    let parent = db.get_issue(parent_id)?;
    if parent.is_none() {
        bail!("Parent issue {} not found", format_issue_id(parent_id));
    }

    let id = if let Some(w) = writer {
        let id = w.create_subissue(
            db,
            parent_id,
            title,
            final_description.as_deref(),
            &final_priority,
        )?;

        if let Some(lbl) = template_label {
            w.add_label(db, id, lbl)?;
        }

        for lbl in opts.labels {
            w.add_label(db, id, lbl)?;
        }

        id
    } else {
        db.transaction(|| {
            let id = db.create_subissue(
                parent_id,
                title,
                final_description.as_deref(),
                &final_priority,
            )?;

            if let Some(lbl) = template_label {
                db.add_label(id, lbl)?;
            }

            for lbl in opts.labels {
                db.add_label(id, lbl)?;
            }

            Ok(id)
        })?
    };

    if opts.quiet {
        println!("{id}");
    } else {
        println!(
            "Created subissue {} under {}",
            format_issue_id(id),
            format_issue_id(parent_id)
        );
    }

    if opts.work {
        auto_claim_and_set_work(db, id, title, opts.crosslink_dir, opts.quiet)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn test_validate_priority_valid() {
        assert!(validate_priority("low"));
        assert!(validate_priority("medium"));
        assert!(validate_priority("high"));
        assert!(validate_priority("critical"));
    }

    #[test]
    fn test_validate_priority_invalid() {
        assert!(!validate_priority(""));
        assert!(!validate_priority("urgent"));
        assert!(!validate_priority("LOW"));
        assert!(!validate_priority("MEDIUM"));
        assert!(!validate_priority("High"));
        assert!(!validate_priority("CRITICAL"));
        assert!(!validate_priority(" medium"));
        assert!(!validate_priority("medium "));
        assert!(!validate_priority("medium\n"));
    }

    #[test]
    fn test_validate_priority_malicious() {
        assert!(!validate_priority("'; DROP TABLE issues; --"));
        assert!(!validate_priority("high\0medium"));
        assert!(!validate_priority("medium; DELETE FROM issues"));
        assert!(!validate_priority("<script>alert('xss')</script>"));
    }

    #[test]
    fn test_get_template_exists() {
        let bug = get_template("bug");
        assert!(bug.is_some());
        let template = bug.unwrap();
        assert_eq!(template.name, "bug");
        assert_eq!(template.priority, "high");
        assert_eq!(template.label, "bug");
        assert!(template.description_prefix.is_some());
    }

    #[test]
    fn test_get_template_not_found() {
        assert!(get_template("nonexistent").is_none());
        assert!(get_template("").is_none());
        assert!(get_template("Bug").is_none());
        assert!(get_template("BUG").is_none());
    }

    #[test]
    fn test_list_templates() {
        let templates = list_templates();
        assert!(templates.contains(&"bug"));
        assert!(templates.contains(&"feature"));
        assert!(templates.contains(&"refactor"));
        assert!(templates.contains(&"research"));
        assert!(templates.contains(&"audit"));
        assert!(templates.contains(&"continuation"));
        assert!(templates.contains(&"investigation"));
        assert_eq!(templates.len(), 7);
    }

    #[test]
    fn test_template_fields() {
        for template in TEMPLATES {
            assert!(!template.name.is_empty());
            assert!(validate_priority(template.priority));
            assert!(!template.label.is_empty());
        }
    }

    #[test]
    fn test_template_bug_description_prefix() {
        let template = get_template("bug").unwrap();
        let prefix = template.description_prefix.unwrap();
        assert!(prefix.contains("Steps to reproduce"));
        assert!(prefix.contains("Expected"));
        assert!(prefix.contains("Actual"));
    }

    #[test]
    fn test_template_feature_description_prefix() {
        let template = get_template("feature").unwrap();
        let prefix = template.description_prefix.unwrap();
        assert!(prefix.contains("Goal"));
        assert!(prefix.contains("Acceptance criteria"));
    }

    proptest! {
        #[test]
        fn prop_invalid_priorities_never_validate(
            priority in "[a-zA-Z]{1,20}"
                .prop_filter("Exclude valid priorities", |s| {
                    !["low", "medium", "high", "critical"].contains(&s.as_str())
                })
        ) {
            prop_assert!(!validate_priority(&priority));
        }

        #[test]
        fn prop_unknown_template_returns_none(name in "[a-zA-Z]{5,20}"
            .prop_filter("Exclude known templates", |s| {
                !["bug", "feature", "refactor", "research", "audit", "continuation", "investigation"].contains(&s.as_str())
            })
        ) {
            prop_assert!(get_template(&name).is_none());
        }
    }

    fn setup_test_db() -> (crate::db::Database, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = crate::db::Database::open(&db_path).unwrap();
        (db, dir)
    }

    #[test]
    fn test_run_creates_issue() {
        let (db, _dir) = setup_test_db();
        let opts = CreateOpts {
            labels: &[],
            work: false,
            quiet: false,
            crosslink_dir: None,
            defer_id: false,
            force: false,
        };
        run(
            &db,
            None,
            "Test issue",
            None,
            "medium",
            None,
            None,
            None,
            &opts,
        )
        .unwrap();
        let issues = db.list_issues(Some("all"), None, None).unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].title, "Test issue");
    }

    #[test]
    fn test_run_with_template_applies_label() {
        let (db, _dir) = setup_test_db();
        let opts = CreateOpts {
            labels: &[],
            work: false,
            quiet: false,
            crosslink_dir: None,
            defer_id: false,
            force: false,
        };
        run(
            &db,
            None,
            "A bug",
            None,
            "medium",
            Some("bug"),
            None,
            None,
            &opts,
        )
        .unwrap();
        let issues = db.list_issues(Some("all"), None, None).unwrap();
        assert_eq!(issues.len(), 1);
        let labels = db.get_labels(issues[0].id).unwrap();
        assert!(labels.contains(&"bug".to_string()));
    }

    #[test]
    fn test_run_with_user_labels() {
        let (db, _dir) = setup_test_db();
        let labels = vec!["urgent".to_string(), "backend".to_string()];
        let opts = CreateOpts {
            labels: &labels,
            work: false,
            quiet: false,
            crosslink_dir: None,
            defer_id: false,
            force: false,
        };
        run(
            &db,
            None,
            "Labeled issue",
            None,
            "high",
            None,
            None,
            None,
            &opts,
        )
        .unwrap();
        let issues = db.list_issues(Some("all"), None, None).unwrap();
        let issue_labels = db.get_labels(issues[0].id).unwrap();
        assert_eq!(issue_labels.len(), 2);
        assert!(issue_labels.contains(&"urgent".to_string()));
        assert!(issue_labels.contains(&"backend".to_string()));
    }

    #[test]
    fn test_run_subissue_creates_child() {
        let (db, _dir) = setup_test_db();
        let parent_id = db.create_issue("Parent", None, "high").unwrap();
        let opts = CreateOpts {
            labels: &[],
            work: false,
            quiet: false,
            crosslink_dir: None,
            defer_id: false,
            force: false,
        };
        run_subissue(
            &db,
            None,
            parent_id,
            "Child task",
            None,
            "medium",
            None,
            &opts,
        )
        .unwrap();
        let subs = db.get_subissues(parent_id).unwrap();
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].title, "Child task");
    }

    #[test]
    fn test_run_invalid_priority_fails() {
        let (db, _dir) = setup_test_db();
        let opts = CreateOpts {
            labels: &[],
            work: false,
            quiet: false,
            crosslink_dir: None,
            defer_id: false,
            force: false,
        };
        let result = run(
            &db,
            None,
            "Bad priority",
            None,
            "urgent",
            None,
            None,
            None,
            &opts,
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Invalid priority"));
    }

    fn rule(field: &str, pattern: &str, min_chars: usize) -> RequiredFieldRule {
        RequiredFieldRule {
            field: field.to_string(),
            pattern: Regex::new(pattern).unwrap(),
            min_chars,
        }
    }

    #[test]
    fn test_validate_required_fields_no_rules_is_ok() {
        assert!(validate_required_fields(&[], None).is_ok());
        assert!(validate_required_fields(&[], Some("anything")).is_ok());
    }

    #[test]
    fn test_validate_required_fields_missing_field_fails() {
        let rules = vec![rule("Rationale", r"(?i)\brationale\b", 0)];
        let err = validate_required_fields(&rules, Some("no keyword here"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("missing required field 'Rationale'"), "{err}");
    }

    #[test]
    fn test_validate_required_fields_min_chars_measures_capture_group() {
        let rules = vec![rule("Rationale", r"(?is)rationale:\s*(.+)", 20)];

        let err = validate_required_fields(&rules, Some("Rationale: tiny"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("too short"), "{err}");

        assert!(validate_required_fields(
            &rules,
            Some("Rationale: this explanation is comfortably over twenty characters long")
        )
        .is_ok());
    }

    #[test]
    fn test_validate_required_fields_min_chars_measures_full_match_without_group() {
        let rules = vec![rule("Body", r"(?s).+", 10)];
        assert!(validate_required_fields(&rules, Some("short")).is_err());
        assert!(validate_required_fields(&rules, Some("this is long enough")).is_ok());
    }

    #[test]
    fn test_parse_required_fields_compiles_and_filters_by_template() {
        let config = serde_json::json!({
            "template_required_fields": {
                "research": [
                    { "field": "Rationale", "pattern": r"(?i)rationale", "min_chars": 200 }
                ],
                "audit": [
                    { "field": "Scope", "pattern": r"(?i)scope" }
                ]
            }
        });
        let research = parse_required_fields(&config, "research").unwrap();
        assert_eq!(research.len(), 1);
        assert_eq!(research[0].field, "Rationale");
        assert_eq!(research[0].min_chars, 200);

        let audit = parse_required_fields(&config, "audit").unwrap();
        assert_eq!(audit[0].min_chars, 0);

        assert!(parse_required_fields(&config, "feature")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn test_parse_required_fields_bad_regex_fails_at_load() {
        let config = serde_json::json!({
            "template_required_fields": {
                "research": [ { "field": "Bad", "pattern": "(unclosed" } ]
            }
        });
        let err = parse_required_fields(&config, "feature")
            .unwrap_err()
            .to_string();
        assert!(err.contains("Invalid regex"), "{err}");
    }

    #[test]
    fn test_parse_required_fields_missing_pattern_fails() {
        let config = serde_json::json!({
            "template_required_fields": {
                "research": [ { "field": "Rationale", "min_chars": 50 } ]
            }
        });
        let err = parse_required_fields(&config, "research")
            .unwrap_err()
            .to_string();
        assert!(err.contains("missing string key 'pattern'"), "{err}");
    }

    fn write_team_config(dir: &std::path::Path, json: &str) {
        std::fs::write(dir.join("hook-config.json"), json).unwrap();
    }

    fn validating_opts(dir: &std::path::Path, force: bool) -> CreateOpts<'_> {
        CreateOpts {
            labels: &[],
            work: false,
            quiet: true,
            crosslink_dir: Some(dir),
            defer_id: false,
            force,
        }
    }

    const RESEARCH_RULE: &str = r#"{
        "template_required_fields": {
            "research": [
                { "field": "Rationale", "pattern": "(?is)rationale:\\s*(.+)", "min_chars": 20 }
            ]
        }
    }"#;

    #[test]
    fn test_create_research_succeeds_with_required_fields() {
        let (db, dir) = setup_test_db();
        write_team_config(dir.path(), RESEARCH_RULE);
        let opts = validating_opts(dir.path(), false);
        run(
            &db,
            None,
            "Investigate",
            Some("Rationale: we need this to understand the failure mode in detail"),
            "medium",
            Some("research"),
            None,
            None,
            &opts,
        )
        .unwrap();
        assert_eq!(db.list_issues(Some("all"), None, None).unwrap().len(), 1);
    }

    #[test]
    fn test_create_research_fails_without_description() {
        let (db, dir) = setup_test_db();
        write_team_config(dir.path(), RESEARCH_RULE);
        let opts = validating_opts(dir.path(), false);
        let err = run(
            &db,
            None,
            "Investigate",
            None,
            "medium",
            Some("research"),
            None,
            None,
            &opts,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("missing required field 'Rationale'"), "{err}");

        assert_eq!(db.list_issues(Some("all"), None, None).unwrap().len(), 0);
    }

    #[test]
    fn test_create_research_fails_with_short_description() {
        let (db, dir) = setup_test_db();
        write_team_config(dir.path(), RESEARCH_RULE);
        let opts = validating_opts(dir.path(), false);
        let err = run(
            &db,
            None,
            "Investigate",
            Some("Rationale: tiny"),
            "medium",
            Some("research"),
            None,
            None,
            &opts,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("too short"), "{err}");
    }

    #[test]
    fn test_create_research_force_override_succeeds() {
        let (db, dir) = setup_test_db();
        write_team_config(dir.path(), RESEARCH_RULE);
        let opts = validating_opts(dir.path(), true);
        run(
            &db,
            None,
            "Investigate",
            Some("Rationale: tiny"),
            "medium",
            Some("research"),
            None,
            None,
            &opts,
        )
        .unwrap();
        assert_eq!(db.list_issues(Some("all"), None, None).unwrap().len(), 1);
    }

    #[test]
    fn test_create_research_local_config_override_relaxes_check() {
        let (db, dir) = setup_test_db();
        write_team_config(dir.path(), RESEARCH_RULE);

        std::fs::write(
            dir.path().join("hook-config.local.json"),
            r#"{ "template_required_fields": {} }"#,
        )
        .unwrap();
        let opts = validating_opts(dir.path(), false);

        run(
            &db,
            None,
            "Investigate",
            None,
            "medium",
            Some("research"),
            None,
            None,
            &opts,
        )
        .unwrap();
        assert_eq!(db.list_issues(Some("all"), None, None).unwrap().len(), 1);
    }

    #[test]
    fn test_create_feature_succeeds_without_required_fields() {
        let (db, dir) = setup_test_db();
        write_team_config(dir.path(), RESEARCH_RULE);
        let opts = validating_opts(dir.path(), false);
        run(
            &db,
            None,
            "A feature",
            None,
            "medium",
            Some("feature"),
            None,
            None,
            &opts,
        )
        .unwrap();
        assert_eq!(db.list_issues(Some("all"), None, None).unwrap().len(), 1);
    }

    #[test]
    fn test_subissue_enforces_template_required_fields() {
        let (db, dir) = setup_test_db();
        write_team_config(dir.path(), RESEARCH_RULE);
        let parent_id = db.create_issue("Parent", None, "high").unwrap();
        let opts = validating_opts(dir.path(), false);

        let err = run_subissue(
            &db,
            None,
            parent_id,
            "Child",
            Some("Rationale: tiny"),
            "medium",
            Some("research"),
            &opts,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("too short"), "{err}");
        assert!(db.get_subissues(parent_id).unwrap().is_empty());

        run_subissue(
            &db,
            None,
            parent_id,
            "Child",
            Some("Rationale: a thoroughly detailed explanation of the child task"),
            "medium",
            Some("research"),
            &opts,
        )
        .unwrap();
        assert_eq!(db.get_subissues(parent_id).unwrap().len(), 1);
    }

    #[test]
    fn test_quick_command_enforces_template_required_fields() {
        let (db, dir) = setup_test_db();
        write_team_config(dir.path(), RESEARCH_RULE);
        let opts = CreateOpts {
            labels: &[],
            work: true,
            quiet: true,
            crosslink_dir: Some(dir.path()),
            defer_id: false,
            force: false,
        };
        let err = run(
            &db,
            None,
            "Quick research",
            None,
            "medium",
            Some("research"),
            None,
            None,
            &opts,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("missing required field 'Rationale'"), "{err}");
        assert_eq!(db.list_issues(Some("all"), None, None).unwrap().len(), 0);
    }
}
