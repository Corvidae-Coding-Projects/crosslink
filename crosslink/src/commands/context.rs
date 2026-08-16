use anyhow::{Context, Result};
use std::fs;
use std::path::Path;

use crate::commands::init;
use crate::ContextCommands;

const LANGUAGE_MANIFESTS: &[(&str, &str, &str)] = &[
    ("Cargo.toml", "Rust", "rust.md"),
    ("package.json", "JavaScript", "javascript.md"),
    ("tsconfig.json", "TypeScript", "typescript.md"),
    ("pyproject.toml", "Python", "python.md"),
    ("requirements.txt", "Python", "python.md"),
    ("go.mod", "Go", "go.md"),
    ("pom.xml", "Java", "java.md"),
    ("build.gradle", "Java", "java.md"),
    ("Gemfile", "Ruby", "ruby.md"),
    ("composer.json", "PHP", "php.md"),
    ("Package.swift", "Swift", "swift.md"),
    ("CMakeLists.txt", "C/C++", "cpp.md"),
    ("Makefile", "C/C++", "c.md"),
    ("mix.exs", "Elixir", "elixir.md"),
    (".shellcheckrc", "Shell", "shell.md"),
];

const EXPECTED_HOOKS: &[&str] = &[
    "prompt-guard.py",
    "post-edit-check.py",
    "session-start.py",
    "pre-web-check.py",
    "work-check.py",
    "crosslink_config.py",
    "heartbeat.py",
    "hook_protocol.py",
];

const EXPECTED_COMMANDS: &[&str] = &[
    "workflow.md",
    "feature.md",
    "featree.md",
    "kickoff.md",
    "check.md",
    "commit.md",
    "preflight.md",
    "review.md",
    "audit.md",
];

const EXPECTED_RULES: &[&str] = &[
    "global.md",
    "project.md",
    "tracking-strict.md",
    "tracking-normal.md",
    "tracking-relaxed.md",
];

pub fn run(command: ContextCommands, crosslink_dir: &Path) -> Result<()> {
    match command {
        ContextCommands::Measure { verbose } => measure(crosslink_dir, verbose),
        ContextCommands::Check => {
            let project_root = crosslink_dir
                .parent()
                .context("Cannot determine project root")?;
            check(crosslink_dir, project_root);
            Ok(())
        }
    }
}

fn measure(crosslink_dir: &Path, verbose: bool) -> Result<()> {
    let project_root = crosslink_dir
        .parent()
        .context("Cannot determine project root")?;

    println!("Context injection measurement");
    println!("{}", "=".repeat(60));

    let rules_dir = crosslink_dir.join("rules");
    let mut total_rules: usize = 0;
    let mut active_rules: usize = 0;
    let mut dormant_rules: usize = 0;

    let active_langs = detect_active_languages(project_root);

    let rules_local_dir = crosslink_dir.join("rules.local");

    println!("\n## Rule files (.crosslink/rules/)");
    println!("{:<35} {:>8} {:>8}  STATUS", "FILE", "BYTES", "~TOKENS");
    println!("{}", "-".repeat(65));

    let local_overrides: std::collections::HashSet<String> = if rules_local_dir.is_dir() {
        fs::read_dir(&rules_local_dir)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(std::result::Result::ok)
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect()
    } else {
        std::collections::HashSet::new()
    };

    if rules_dir.is_dir() {
        let mut entries: Vec<_> = fs::read_dir(&rules_dir)
            .context("Failed to read rules directory")?
            .filter_map(std::result::Result::ok)
            .filter(|e| {
                e.path()
                    .extension()
                    .is_some_and(|ext| ext == "md" || ext == "txt")
            })
            .collect();
        entries.sort_by_key(std::fs::DirEntry::file_name);

        for entry in &entries {
            let path = entry.path();
            let filename = entry.file_name().to_string_lossy().to_string();

            let (size, suffix) = if local_overrides.contains(&filename) {
                let local_path = rules_local_dir.join(&filename);
                let s = fs::metadata(&local_path).map_or(0, |m| m.len() as usize);
                (s, " (local)")
            } else {
                let s = fs::metadata(&path).map_or(0, |m| m.len() as usize);
                (s, "")
            };
            let tokens = size / 4;
            total_rules += size;

            let is_active = is_rule_active(&filename, &active_langs);
            let status = if is_active {
                active_rules += size;
                "active"
            } else {
                dormant_rules += size;
                "dormant"
            };

            println!("{filename:<35} {size:>8} {tokens:>8}  {status}{suffix}");
        }
    }

    if rules_local_dir.is_dir() {
        let base_files: std::collections::HashSet<String> = if rules_dir.is_dir() {
            fs::read_dir(&rules_dir)
                .ok()
                .into_iter()
                .flatten()
                .filter_map(std::result::Result::ok)
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect()
        } else {
            std::collections::HashSet::new()
        };

        let mut local_entries: Vec<_> = fs::read_dir(&rules_local_dir)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(std::result::Result::ok)
            .filter(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                !base_files.contains(&name)
                    && e.path()
                        .extension()
                        .is_some_and(|ext| ext == "md" || ext == "txt")
            })
            .collect();
        local_entries.sort_by_key(std::fs::DirEntry::file_name);

        for entry in &local_entries {
            let path = entry.path();
            let filename = entry.file_name().to_string_lossy().to_string();
            let size = fs::metadata(&path).map_or(0, |m| m.len() as usize);
            let tokens = size / 4;
            total_rules += size;
            active_rules += size;

            println!("{filename:<35} {size:>8} {tokens:>8}  active (local)");
        }
    }

    println!();
    println!(
        "  Total rules:   {:>8} bytes ({} tokens)",
        total_rules,
        total_rules / 4
    );
    println!(
        "  Active rules:  {:>8} bytes ({} tokens)",
        active_rules,
        active_rules / 4
    );
    println!(
        "  Dormant rules: {:>8} bytes ({} tokens)",
        dormant_rules,
        dormant_rules / 4
    );

    println!("\n## Detected languages");
    if active_langs.is_empty() {
        println!("  (none detected)");
    } else {
        for lang in &active_langs {
            println!("  - {lang}");
        }
    }

    println!("\n## Project instructions");
    for name in ["AGENTS.md", "CLAUDE.md"] {
        let instruction_path = project_root.join(name);
        if instruction_path.is_file() {
            let size =
                fs::metadata(&instruction_path).map_or(0, |metadata| metadata.len() as usize);
            println!("  {name:<12} {size:>8} bytes ({} tokens)", size / 4);
        } else {
            println!("  {name:<12} (not found)");
        }
    }

    let mut total_skills: usize = 0;
    println!("\n## Skill files");
    for relative in [".claude/skills", ".agents/skills", ".claude/commands"] {
        let skill_root = project_root.join(relative);
        let mut root_total = 0;
        if skill_root.is_dir() {
            for entry in walk_markdown_files(&skill_root) {
                root_total += fs::metadata(entry).map_or(0, |metadata| metadata.len() as usize);
            }
            total_skills += root_total;
            println!(
                "  {relative:<20} {root_total:>8} bytes ({} tokens)",
                root_total / 4
            );
        } else {
            println!("  {relative:<20} (not found)");
        }
    }
    println!(
        "  {:<20} {:>8} bytes ({} tokens)",
        "total",
        total_skills,
        total_skills / 4
    );

    let tree_est: usize = 2000;
    let deps_est: usize = 1200;
    let wrapper_est: usize = 500;
    let full_guard = tree_est + deps_est + active_rules + wrapper_est;

    println!("\n## Estimated first-prompt injection");
    println!("  Project tree:    ~{tree_est:>6} bytes");
    println!("  Dependencies:    ~{deps_est:>6} bytes");
    println!("  Active rules:     {active_rules:>6} bytes");
    println!("  Wrapper/headers: ~{wrapper_est:>6} bytes");
    println!("  ─────────────────────────");
    println!(
        "  Total:           ~{:>6} bytes (~{} tokens)",
        full_guard,
        full_guard / 4
    );

    let condensed_est: usize = 500;
    println!("\n## Condensed reminder (subsequent prompts)");
    println!(
        "  Estimated:       ~{:>6} bytes (~{} tokens)",
        condensed_est,
        condensed_est / 4
    );

    println!("\n## Adaptive reminder savings (over 50 prompts)");
    let always_total = full_guard + condensed_est * 49;

    let adaptive_reminders = 49 / 5;
    let adaptive_total = full_guard + condensed_est * adaptive_reminders;
    let saved = always_total.saturating_sub(adaptive_total);
    println!(
        "  Always-inject:   ~{:>8} bytes ({} tokens)",
        always_total,
        always_total / 4
    );
    println!(
        "  Adaptive (t=5):  ~{:>8} bytes ({} tokens)",
        adaptive_total,
        adaptive_total / 4
    );
    println!(
        "  Saved:           ~{:>8} bytes ({} tokens, {:.0}%)",
        saved,
        saved / 4,
        if always_total > 0 {
            saved as f64 / always_total as f64 * 100.0
        } else {
            0.0
        }
    );

    if verbose {
        println!("\n## Hook config");
        let config_path = crosslink_dir.join("hook-config.json");
        if config_path.is_file() {
            let content =
                fs::read_to_string(&config_path).context("Failed to read hook-config.json")?;
            println!("{content}");
        } else {
            println!("  (not found)");
        }
    }

    Ok(())
}

fn detect_active_languages(project_root: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let mut seen = std::collections::HashSet::new();

    let mut check_dirs = vec![project_root.to_path_buf()];
    if let Ok(entries) = fs::read_dir(project_root) {
        for entry in entries.filter_map(std::result::Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                let name = entry.file_name().to_string_lossy().to_string();
                if !name.starts_with('.') {
                    check_dirs.push(path);
                }
            }
        }
    }

    for dir in &check_dirs {
        for &(manifest, lang, _rule_file) in LANGUAGE_MANIFESTS {
            if dir.join(manifest).exists() && seen.insert(lang.to_string()) {
                found.push(lang.to_string());
            }
        }
    }

    if !seen.contains("Shell") {
        let shell_dirs = [
            project_root.to_path_buf(),
            project_root.join("scripts"),
            project_root.join("bin"),
        ];
        'shell_scan: for dir in &shell_dirs {
            if let Ok(entries) = fs::read_dir(dir) {
                for entry in entries.filter_map(std::result::Result::ok) {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if std::path::Path::new(&name).extension().is_some_and(|ext| {
                        ext.eq_ignore_ascii_case("sh") || ext.eq_ignore_ascii_case("bash")
                    }) {
                        seen.insert("Shell".to_string());
                        found.push("Shell".to_string());
                        break 'shell_scan;
                    }
                }
            }
        }
    }

    found
}

fn is_rule_active(filename: &str, active_langs: &[String]) -> bool {
    if matches!(
        filename,
        "global.md"
            | "project.md"
            | "tracking-strict.md"
            | "tracking-normal.md"
            | "tracking-relaxed.md"
            | "external-content.md"
            | "knowledge.md"
            | "web.md"
    ) {
        return true;
    }

    for &(_, lang, rule_file) in LANGUAGE_MANIFESTS {
        if filename == rule_file && active_langs.iter().any(|l| l == lang) {
            return true;
        }
    }

    false
}

fn walk_markdown_files(root: &Path) -> Vec<std::path::PathBuf> {
    let mut pending = vec![root.to_path_buf()];
    let mut files = Vec::new();
    while let Some(directory) = pending.pop() {
        if let Ok(entries) = fs::read_dir(directory) {
            for entry in entries.filter_map(std::result::Result::ok) {
                if entry.path().is_dir() {
                    pending.push(entry.path());
                } else if entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "md")
                {
                    files.push(entry.path());
                }
            }
        }
    }
    files
}

fn check(crosslink_dir: &Path, project_root: &Path) {
    let mut problems = 0;

    println!("Crosslink deployment check");
    println!("{}", "=".repeat(40));

    println!("\n## Rule files");
    let rules_dir = crosslink_dir.join("rules");
    for &name in EXPECTED_RULES {
        let path = rules_dir.join(name);
        if path.is_file() {
            println!("  OK  {name}");
        } else {
            println!("  MISSING  {name}");
            problems += 1;
        }
    }

    for &(rule_name, _content) in init::RULE_FILES {
        let path = rules_dir.join(rule_name);
        if path.is_file() {
        } else {
            println!("  MISSING  {rule_name}");
            problems += 1;
        }
    }

    println!("\n## Hook files");
    let hooks_dir = crosslink_dir.join("integrations/hooks");
    for &name in EXPECTED_HOOKS {
        let path = hooks_dir.join(name);
        if path.is_file() {
            println!("  OK  {name}");
        } else {
            println!("  MISSING  {name}");
            problems += 1;
        }
    }

    println!("\n## Command files");
    let commands_dir = project_root.join(".claude/commands");
    for &name in EXPECTED_COMMANDS {
        let path = commands_dir.join(name);
        if path.is_file() {
            println!("  OK  {name}");
        } else {
            println!("  MISSING  {name}");
            problems += 1;
        }
    }

    println!("\n## Provider integrations");
    for relative in [
        ".claude/settings.json",
        ".mcp.json",
        ".codex/hooks.json",
        ".codex/config.toml",
        "AGENTS.md",
    ] {
        if project_root.join(relative).is_file() {
            println!("  OK  {relative}");
        } else {
            println!("  MISSING  {relative}");
            problems += 1;
        }
    }

    println!("\n## Configuration");
    let config_path = crosslink_dir.join("hook-config.json");
    if config_path.is_file() {
        match fs::read_to_string(&config_path) {
            Ok(content) => match serde_json::from_str::<serde_json::Value>(&content) {
                Ok(_) => println!("  OK  hook-config.json (valid JSON)"),
                Err(e) => {
                    println!("  INVALID  hook-config.json: {e}");
                    problems += 1;
                }
            },
            Err(e) => {
                println!("  ERROR  hook-config.json: {e}");
                problems += 1;
            }
        }
    } else {
        println!("  MISSING  hook-config.json");
        problems += 1;
    }

    println!();
    if problems == 0 {
        println!("All checks passed.");
    } else {
        println!("{problems} problem(s) found. Run `crosslink init --force` to repair.");
        std::process::exit(1);
    }
}
