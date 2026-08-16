use anyhow::Result;
use std::fmt::Write;
use std::path::{Path, PathBuf};

use crate::utils::resolve_main_repo_root;

pub const KNOWLEDGE_CACHE_DIR: &str = ".knowledge-cache";

pub const KNOWLEDGE_BRANCH: &str = "crosslink/knowledge";

pub struct KnowledgeManager {
    pub(super) crosslink_dir: PathBuf,

    pub(super) cache_dir: PathBuf,

    pub(super) repo_root: PathBuf,

    pub(super) remote: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageFrontmatter {
    pub title: String,
    pub tags: Vec<String>,
    pub sources: Vec<Source>,
    pub contributors: Vec<String>,
    pub created: String,
    pub updated: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub url: String,
    pub title: String,
    pub accessed_at: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PageInfo {
    pub slug: String,
    pub frontmatter: PageFrontmatter,
}

#[derive(Debug, Clone)]
pub struct SearchMatch {
    pub slug: String,
    pub line_number: usize,

    pub context_lines: Vec<(usize, String)>,
}

#[derive(Debug, Default)]
pub struct SyncOutcome {
    pub resolved_conflicts: Vec<String>,
}

#[must_use]
pub fn has_conflict_markers(content: &str) -> bool {
    #[derive(PartialEq)]
    enum ConflictScan {
        Ours,
        Separator,
        Theirs,
    }
    let mut state = ConflictScan::Ours;
    for line in content.lines() {
        match state {
            ConflictScan::Ours => {
                if line.starts_with("<<<<<<<") {
                    state = ConflictScan::Separator;
                }
            }
            ConflictScan::Separator => {
                if line.starts_with("=======") {
                    state = ConflictScan::Theirs;
                }
            }
            ConflictScan::Theirs => {
                if line.starts_with(">>>>>>>") {
                    return true;
                }
            }
        }
    }
    false
}

#[must_use]
pub fn resolve_accept_both(content: &str) -> String {
    enum ConflictState {
        Outside,

        InOurs,

        InTheirs,
    }

    let mut result = String::new();
    let mut state = ConflictState::Outside;
    let mut ours = String::new();
    let mut theirs = String::new();

    for line in content.lines() {
        match state {
            ConflictState::Outside => {
                if line.starts_with("<<<<<<<") {
                    state = ConflictState::InOurs;
                    ours.clear();
                    theirs.clear();
                } else {
                    result.push_str(line);
                    result.push('\n');
                }
            }
            ConflictState::InOurs => {
                if line.starts_with("=======") {
                    state = ConflictState::InTheirs;
                } else {
                    ours.push_str(line);
                    ours.push('\n');
                }
            }
            ConflictState::InTheirs => {
                if line.starts_with(">>>>>>>") {
                    state = ConflictState::Outside;

                    result.push_str(
                        "<!-- MERGE CONFLICT: Both versions kept. Cleanup recommended. -->\n",
                    );
                    result.push_str("---\n");
                    result.push_str(&ours);
                    result.push_str("---\n");
                    result.push_str(&theirs);
                } else {
                    theirs.push_str(line);
                    theirs.push('\n');
                }
            }
        }
    }

    if !matches!(state, ConflictState::Outside) {
        if !ours.is_empty() {
            result.push_str(&ours);
        }
        if !theirs.is_empty() {
            result.push_str(&theirs);
        }
    }

    result
}

impl KnowledgeManager {
    pub fn new(crosslink_dir: &Path) -> Result<Self> {
        let remote = crate::sync::read_tracker_remote(crosslink_dir);
        Self::with_remote(crosslink_dir, remote)
    }

    pub fn with_remote(crosslink_dir: &Path, remote: String) -> Result<Self> {
        let local_repo_root = crosslink_dir
            .parent()
            .ok_or_else(|| anyhow::anyhow!("Cannot determine repo root from .crosslink dir"))?
            .to_path_buf();

        let repo_root =
            resolve_main_repo_root(&local_repo_root).unwrap_or_else(|| local_repo_root.clone());

        let cache_dir = repo_root.join(".crosslink").join(KNOWLEDGE_CACHE_DIR);

        Ok(Self {
            crosslink_dir: crosslink_dir.to_path_buf(),
            cache_dir,
            repo_root,
            remote,
        })
    }

    #[must_use]
    pub fn is_initialized(&self) -> bool {
        self.cache_dir.exists()
    }

    #[must_use]
    pub fn crosslink_dir(&self) -> &Path {
        &self.crosslink_dir
    }

    #[must_use]
    pub fn cache_path(&self) -> &Path {
        &self.cache_dir
    }

    pub(super) fn cache_path_str(&self) -> String {
        self.cache_dir.to_string_lossy().to_string()
    }
}

enum ParseState {
    TopLevel,
    InTags,
    InSources,
    InContributors,
    InSourceItem,
}

fn apply_source_kv(source: &mut Source, key: &str, value: &str) {
    match key {
        "url" => source.url = unquote(value),
        "title" => source.title = unquote(value),
        "accessed_at" => source.accessed_at = Some(unquote(value)),
        _ => {}
    }
}

fn parse_source_list_item(source: &mut Source, trimmed: &str) {
    let after_dash = trimmed.strip_prefix("- ").unwrap_or("");
    if let Some((k, v)) = after_dash.split_once(": ") {
        apply_source_kv(source, k.trim(), v.trim());
    }
}

struct FrontmatterBuilder {
    title: String,
    tags: Vec<String>,
    sources: Vec<Source>,
    contributors: Vec<String>,
    created: String,
    updated: String,
    state: ParseState,
    current_source: Source,
}

impl FrontmatterBuilder {
    const fn new() -> Self {
        Self {
            title: String::new(),
            tags: Vec::new(),
            sources: Vec::new(),
            contributors: Vec::new(),
            created: String::new(),
            updated: String::new(),
            state: ParseState::TopLevel,
            current_source: Source {
                url: String::new(),
                title: String::new(),
                accessed_at: None,
            },
        }
    }

    fn flush_current_source(&mut self) {
        if !self.current_source.url.is_empty() || !self.current_source.title.is_empty() {
            self.sources.push(self.current_source.clone());
            self.current_source = Source {
                url: String::new(),
                title: String::new(),
                accessed_at: None,
            };
        }
    }

    fn handle_top_level_kv(&mut self, trimmed: &str) -> Option<()> {
        if matches!(self.state, ParseState::InSourceItem) {
            self.flush_current_source();
        }
        let (key, value) = split_kv_or_bare(trimmed)?;
        match key {
            "title" => {
                self.title = unquote(value);
                self.state = ParseState::TopLevel;
            }
            "tags" => self.parse_inline_or_begin_list(value, FieldKind::Tags),
            "sources" => {
                if value == "[]" {
                    self.sources = Vec::new();
                    self.state = ParseState::TopLevel;
                } else {
                    self.state = ParseState::InSources;
                }
            }
            "contributors" => self.parse_inline_or_begin_list(value, FieldKind::Contributors),
            "created" => {
                self.created = unquote(value);
                self.state = ParseState::TopLevel;
            }
            "updated" => {
                self.updated = unquote(value);
                self.state = ParseState::TopLevel;
            }
            _ => self.state = ParseState::TopLevel,
        }
        Some(())
    }

    fn parse_inline_or_begin_list(&mut self, value: &str, kind: FieldKind) {
        if let Some(inline) = parse_inline_array(value) {
            match kind {
                FieldKind::Tags => self.tags = inline,
                FieldKind::Contributors => self.contributors = inline,
            }
            self.state = ParseState::TopLevel;
        } else if value.is_empty() || value == "[]" {
            match kind {
                FieldKind::Tags => {
                    self.tags = Vec::new();
                    self.state = if value == "[]" {
                        ParseState::TopLevel
                    } else {
                        ParseState::InTags
                    };
                }
                FieldKind::Contributors => {
                    self.contributors = Vec::new();
                    self.state = if value == "[]" {
                        ParseState::TopLevel
                    } else {
                        ParseState::InContributors
                    };
                }
            }
        } else {
            self.state = ParseState::TopLevel;
        }
    }

    fn handle_nested_line(&mut self, trimmed: &str, is_list_item: bool, is_nested_key: bool) {
        match self.state {
            ParseState::InTags => {
                if is_list_item {
                    self.tags
                        .push(unquote(trimmed.strip_prefix("- ").unwrap_or(trimmed)));
                }
            }
            ParseState::InContributors => {
                if is_list_item {
                    self.contributors
                        .push(unquote(trimmed.strip_prefix("- ").unwrap_or(trimmed)));
                }
            }
            ParseState::InSources => {
                if is_list_item {
                    self.current_source = Source {
                        url: String::new(),
                        title: String::new(),
                        accessed_at: None,
                    };
                    parse_source_list_item(&mut self.current_source, trimmed);
                    self.state = ParseState::InSourceItem;
                }
            }
            ParseState::InSourceItem => {
                if is_list_item {
                    self.flush_current_source();
                    self.current_source = Source {
                        url: String::new(),
                        title: String::new(),
                        accessed_at: None,
                    };
                    parse_source_list_item(&mut self.current_source, trimmed);
                } else if is_nested_key {
                    if let Some((k, v)) = trimmed.split_once(": ") {
                        apply_source_kv(&mut self.current_source, k.trim(), v.trim());
                    }
                }
            }
            ParseState::TopLevel => {}
        }
    }

    fn build(mut self) -> PageFrontmatter {
        self.flush_current_source();
        PageFrontmatter {
            title: self.title,
            tags: self.tags,
            sources: self.sources,
            contributors: self.contributors,
            created: self.created,
            updated: self.updated,
        }
    }
}

#[derive(Clone, Copy)]
enum FieldKind {
    Tags,
    Contributors,
}

#[must_use]
pub fn parse_frontmatter(content: &str) -> Option<PageFrontmatter> {
    let content = if content.contains("\r\n") {
        std::borrow::Cow::Owned(content.replace("\r\n", "\n"))
    } else {
        std::borrow::Cow::Borrowed(content)
    };
    let content = content.trim_start();
    if !content.starts_with("---") {
        return None;
    }

    let after_first = &content[3..];
    let after_first = after_first.trim_start_matches(['\r', '\n']);
    let end_idx = after_first.find("\n---")?;
    let yaml_block = &after_first[..end_idx];

    let mut builder = FrontmatterBuilder::new();

    for line in yaml_block.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let is_top_level = !line.starts_with(' ') && !line.starts_with('\t');
        let is_list_item = trimmed.starts_with("- ");
        let is_nested_key = line.starts_with("    ") && !is_list_item && trimmed.contains(": ");
        let is_top_level_kv = is_top_level && (trimmed.contains(": ") || trimmed.ends_with(':'));

        if is_top_level_kv {
            builder.handle_top_level_kv(trimmed)?;
        } else {
            builder.handle_nested_line(trimmed, is_list_item, is_nested_key);
        }
    }

    Some(builder.build())
}

pub(super) fn yaml_escape(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

#[must_use]
pub fn serialize_frontmatter(fm: &PageFrontmatter) -> String {
    let mut out = String::from("---\n");

    let _ = writeln!(out, "title: {}", yaml_escape(&fm.title));

    if fm.tags.is_empty() {
        out.push_str("tags: []\n");
    } else {
        let escaped_tags: Vec<String> = fm.tags.iter().map(|t| yaml_escape(t)).collect();
        let _ = writeln!(out, "tags: [{}]", escaped_tags.join(", "));
    }

    if fm.sources.is_empty() {
        out.push_str("sources: []\n");
    } else {
        out.push_str("sources:\n");
        for src in &fm.sources {
            let _ = writeln!(out, "  - url: {}", yaml_escape(&src.url));
            let _ = writeln!(out, "    title: {}", yaml_escape(&src.title));
            if let Some(ref accessed) = src.accessed_at {
                let _ = writeln!(out, "    accessed_at: {}", yaml_escape(accessed));
            }
        }
    }

    if fm.contributors.is_empty() {
        out.push_str("contributors: []\n");
    } else {
        let escaped_contribs: Vec<String> =
            fm.contributors.iter().map(|c| yaml_escape(c)).collect();
        let _ = writeln!(out, "contributors: [{}]", escaped_contribs.join(", "));
    }

    let _ = writeln!(out, "created: {}", fm.created);
    let _ = writeln!(out, "updated: {}", fm.updated);
    out.push_str("---\n");

    out
}

pub(super) fn split_kv_or_bare(line: &str) -> Option<(&str, &str)> {
    line.find(": ").map_or_else(
        || line.strip_suffix(':').map(|stripped| (stripped.trim(), "")),
        |idx| {
            let key = line[..idx].trim();
            let value = line[idx + 2..].trim();
            Some((key, value))
        },
    )
}

pub(super) fn parse_inline_array(value: &str) -> Option<Vec<String>> {
    let trimmed = value.trim();
    if trimmed.starts_with('[') && trimmed.ends_with(']') {
        let inner = &trimmed[1..trimmed.len() - 1];
        if inner.trim().is_empty() {
            return Some(Vec::new());
        }
        let items: Vec<String> = split_yaml_array_items(inner)
            .iter()
            .map(|s| unquote(s.trim()))
            .collect();
        Some(items)
    } else {
        None
    }
}

fn split_yaml_array_items(s: &str) -> Vec<&str> {
    let mut items = Vec::new();
    let mut start = 0;
    let mut in_quotes = false;
    let mut escaped = false;

    for (i, ch) in s.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if in_quotes => escaped = true,
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => {
                items.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    items.push(&s[start..]);
    items
}

pub(super) fn unquote(s: &str) -> String {
    let s = s.trim();
    if s.starts_with('"') && s.ends_with('"') {
        s[1..s.len() - 1]
            .replace("\\\"", "\"")
            .replace("\\\\", "\\")
    } else if s.starts_with('\'') && s.ends_with('\'') {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}
