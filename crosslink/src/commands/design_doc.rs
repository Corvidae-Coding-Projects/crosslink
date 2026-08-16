use std::fmt::Write as _;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RequirementGroup {
    pub(crate) name: String,

    pub(crate) execution_hint: String,

    pub(crate) items: Vec<String>,
}

pub(crate) struct DesignDoc {
    pub(crate) title: String,
    pub(crate) summary: String,
    pub(crate) requirements: Vec<String>,

    pub(crate) requirement_groups: Vec<RequirementGroup>,
    pub(crate) acceptance_criteria: Vec<String>,
    pub(crate) architecture: String,
    pub(crate) open_questions: Vec<String>,
    pub(crate) out_of_scope: Vec<String>,
    pub(crate) unknown_sections: Vec<(String, String)>,
}

enum Section {
    Title,
    Summary,
    Requirements,
    AcceptanceCriteria,
    Architecture,
    OpenQuestions,
    OutOfScope,

    PhaseGroup(String),
    Unknown(String),
}

pub(crate) fn parse_design_doc(content: &str) -> DesignDoc {
    let mut doc = DesignDoc {
        title: String::new(),
        summary: String::new(),
        requirements: Vec::new(),
        requirement_groups: Vec::new(),
        acceptance_criteria: Vec::new(),
        architecture: String::new(),
        open_questions: Vec::new(),
        out_of_scope: Vec::new(),
        unknown_sections: Vec::new(),
    };

    let mut section = Section::Title;
    let mut current_block = String::new();
    let mut in_code_fence = false;

    for line in content.lines() {
        if line.starts_with("```") {
            in_code_fence = !in_code_fence;
            current_block.push_str(line);
            current_block.push('\n');
            continue;
        }

        if in_code_fence {
            current_block.push_str(line);
            current_block.push('\n');
            continue;
        }

        if let Some(rest) = line.strip_prefix("# ") {
            let rest = rest.trim();
            if !rest.starts_with('#') {
                doc.title = rest
                    .strip_prefix("Feature:")
                    .or_else(|| rest.strip_prefix("feature:"))
                    .map_or_else(|| rest.to_string(), |s| s.trim().to_string());
                section = Section::Title;
                current_block.clear();
                continue;
            }
        }

        if let Some(rest) = line.strip_prefix("## ") {
            flush_block(&mut doc, &section, &current_block);
            current_block.clear();

            let trimmed = rest.trim();
            let lowered = trimmed.to_lowercase();

            section = if lowered.starts_with("phase:")
                || lowered.starts_with("phase ")
                || lowered.starts_with("layer:")
                || lowered.starts_with("layer ")
            {
                Section::PhaseGroup(trimmed.to_string())
            } else {
                match lowered.as_str() {
                    "summary" => Section::Summary,
                    "requirements" => Section::Requirements,
                    "acceptance criteria" => Section::AcceptanceCriteria,
                    "architecture" => Section::Architecture,
                    "open questions" => Section::OpenQuestions,
                    "out of scope" => Section::OutOfScope,
                    other => Section::Unknown(other.to_string()),
                }
            };
            continue;
        }

        current_block.push_str(line);
        current_block.push('\n');
    }

    flush_block(&mut doc, &section, &current_block);

    doc
}

fn flush_block(doc: &mut DesignDoc, section: &Section, block: &str) {
    match section {
        Section::Title => {}
        Section::Summary => doc.summary = block.trim().to_string(),
        Section::Requirements => {
            let (flat, groups) = parse_requirements_block(block);
            doc.requirements = flat;
            doc.requirement_groups = groups;
        }
        Section::AcceptanceCriteria => doc.acceptance_criteria = parse_list_items(block),
        Section::Architecture => doc.architecture = block.trim().to_string(),
        Section::OpenQuestions => doc.open_questions = parse_list_items(block),
        Section::OutOfScope => doc.out_of_scope = parse_list_items(block),
        Section::PhaseGroup(header) => {
            let (name, hint) = parse_layer_header(header);
            let items = parse_list_items_collapsing_sub_bullets(block);
            doc.requirements.extend(items.clone());
            doc.requirement_groups.push(RequirementGroup {
                name,
                execution_hint: hint,
                items,
            });
        }
        Section::Unknown(name) => {
            let trimmed = block.trim();
            if !trimmed.is_empty() {
                doc.unknown_sections
                    .push((name.clone(), trimmed.to_string()));
            }
        }
    }
}

fn parse_requirements_block(block: &str) -> (Vec<String>, Vec<RequirementGroup>) {
    let mut groups: Vec<RequirementGroup> = Vec::new();
    let mut current_group: Option<RequirementGroup> = None;
    let mut current_chunk = String::new();
    let mut has_layer_headers = false;

    for line in block.lines() {
        if let Some(rest) = line.strip_prefix("### ") {
            let rest = rest.trim();

            let is_layer = rest.starts_with("Layer ")
                || rest.starts_with("Phase ")
                || rest.starts_with("layer ")
                || rest.starts_with("phase ");
            if is_layer {
                has_layer_headers = true;

                if let Some(mut group) = current_group.take() {
                    group.items = parse_list_items_collapsing_sub_bullets(&current_chunk);
                    groups.push(group);
                }
                current_chunk.clear();

                let (name, hint) = parse_layer_header(rest);
                current_group = Some(RequirementGroup {
                    name,
                    execution_hint: hint,
                    items: Vec::new(),
                });
                continue;
            }
        }
        current_chunk.push_str(line);
        current_chunk.push('\n');
    }

    if let Some(mut group) = current_group.take() {
        group.items = parse_list_items_collapsing_sub_bullets(&current_chunk);
        groups.push(group);
    }

    let flat = if has_layer_headers {
        groups.iter().flat_map(|g| g.items.clone()).collect()
    } else {
        parse_list_items_collapsing_sub_bullets(block)
    };

    let groups = if has_layer_headers {
        groups
    } else {
        Vec::new()
    };
    (flat, groups)
}

fn parse_layer_header(header: &str) -> (String, String) {
    let after_prefix = header.find(':').map_or(header, |i| header[i + 1..].trim());

    let (name, hint) = after_prefix.find('(').map_or_else(
        || (after_prefix.to_string(), String::new()),
        |paren_start| {
            let name = after_prefix[..paren_start].trim().to_string();
            let paren_content = after_prefix[paren_start + 1..].trim_end_matches(')').trim();
            let hint = if paren_content.starts_with("parallel") {
                "parallel".to_string()
            } else if paren_content.starts_with("sequential") {
                "sequential".to_string()
            } else {
                paren_content.to_string()
            };
            (name, hint)
        },
    );

    (name, hint)
}

fn parse_list_items_collapsing_sub_bullets(block: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut current_item: Option<String> = None;

    for line in block.lines() {
        if line.trim().is_empty() {
            continue;
        }

        let indent = line.len() - line.trim_start().len();
        let trimmed = line.trim();

        if indent >= 2 {
            if let Some(text) = strip_list_prefix(trimmed) {
                if let Some(ref mut item) = current_item {
                    item.push_str("; ");
                    item.push_str(text);
                } else {
                    current_item = Some(text.to_string());
                }
            } else if let Some(ref mut item) = current_item {
                item.push(' ');
                item.push_str(trimmed);
            }
        } else if let Some(text) = strip_list_prefix(trimmed) {
            if let Some(prev) = current_item.take() {
                items.push(prev);
            }
            current_item = Some(text.to_string());
        } else if let Some(ref mut item) = current_item {
            item.push(' ');
            item.push_str(trimmed);
        }
    }

    if let Some(item) = current_item {
        items.push(item);
    }

    items
}

fn parse_list_items(block: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut current_item: Option<String> = None;

    for line in block.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let content = strip_list_prefix(trimmed);
        if let Some(text) = content {
            if let Some(prev) = current_item.take() {
                items.push(prev);
            }
            current_item = Some(text.to_string());
        } else if let Some(ref mut item) = current_item {
            item.push(' ');
            item.push_str(trimmed);
        }
    }

    if let Some(item) = current_item {
        items.push(item);
    }

    items
}

fn strip_list_prefix(line: &str) -> Option<&str> {
    for prefix in &["- [x] ", "- [X] ", "- [ ] ", "* [x] ", "* [X] ", "* [ ] "] {
        if let Some(rest) = line.strip_prefix(prefix) {
            return Some(rest);
        }
    }

    if let Some(rest) = line.strip_prefix("- ") {
        return Some(rest);
    }
    if let Some(rest) = line.strip_prefix("* ") {
        return Some(rest);
    }
    None
}

pub(crate) fn validate_design_doc(doc: &DesignDoc) -> Vec<String> {
    let mut warnings = Vec::new();
    if doc.summary.is_empty() {
        warnings.push("Design doc has no ## Summary section".to_string());
    }
    if doc.requirements.is_empty() {
        warnings.push("Design doc has no ## Requirements section".to_string());
    }
    if doc.acceptance_criteria.is_empty() {
        warnings.push("Design doc has no ## Acceptance Criteria section".to_string());
    }
    warnings
}

pub(crate) fn build_design_doc_section(doc: &DesignDoc) -> String {
    let mut out = String::from("\n## Design Specification\n\n");

    if !doc.summary.is_empty() {
        out.push_str("### Summary\n\n");
        out.push_str(&doc.summary);
        out.push_str("\n\n");
    }

    if !doc.requirements.is_empty() {
        out.push_str("### Requirements\n\n");
        for req in &doc.requirements {
            let _ = writeln!(out, "- {req}");
        }
        out.push('\n');
    }

    if !doc.acceptance_criteria.is_empty() {
        out.push_str("### Acceptance Criteria\n\n");
        for ac in &doc.acceptance_criteria {
            let _ = writeln!(out, "- [ ] {ac}");
        }
        out.push('\n');
    }

    if !doc.architecture.is_empty() {
        out.push_str("### Architecture\n\n");
        out.push_str(&doc.architecture);
        out.push_str("\n\n");
    }

    if !doc.out_of_scope.is_empty() {
        out.push_str("### Out of Scope\n\n");
        for item in &doc.out_of_scope {
            let _ = writeln!(out, "- {item}");
        }
        out.push('\n');
    }

    for (name, body) in &doc.unknown_sections {
        let _ = writeln!(out, "### {name}\n");
        out.push_str(body);
        out.push_str("\n\n");
    }

    out
}

pub(crate) fn build_open_questions_escalation(doc: &DesignDoc) -> Option<String> {
    if doc.open_questions.is_empty() {
        return None;
    }

    let mut out = String::from(
        "\n## Open Questions — Escalation Required\n\n\
         The design document contains unresolved questions. **Before implementing anything \
         affected by these questions**, escalate to the user:\n\n",
    );

    for (i, q) in doc.open_questions.iter().enumerate() {
        let _ = writeln!(out, "{}. {}", i + 1, q);
    }

    out.push_str(
        "\n**Action**: For each question that affects your implementation, add a comment:\n\
         `crosslink comment <issue_id> \"Blocker: <question>\" --kind blocker`\n\
         Then proceed with the parts of the feature that are NOT blocked by these questions.\n",
    );

    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_empty_input() {
        let doc = parse_design_doc("");
        assert!(doc.title.is_empty());
        assert!(doc.summary.is_empty());
        assert!(doc.requirements.is_empty());
        assert!(doc.acceptance_criteria.is_empty());
        assert!(doc.architecture.is_empty());
        assert!(doc.open_questions.is_empty());
        assert!(doc.out_of_scope.is_empty());
        assert!(doc.unknown_sections.is_empty());
    }

    #[test]
    fn test_parse_title_plain() {
        let doc = parse_design_doc("# My Great Feature\n");
        assert_eq!(doc.title, "My Great Feature");
    }

    #[test]
    fn test_parse_title_with_feature_prefix() {
        let doc = parse_design_doc("# Feature: User Authentication\n");
        assert_eq!(doc.title, "User Authentication");
    }

    #[test]
    fn test_parse_title_feature_prefix_lowercase() {
        let doc = parse_design_doc("# feature: lower case prefix\n");
        assert_eq!(doc.title, "lower case prefix");
    }

    #[test]
    fn test_parse_summary() {
        let input = "# Title\n\n## Summary\n\nThis is a summary\nwith multiple lines.\n";
        let doc = parse_design_doc(input);
        assert_eq!(doc.summary, "This is a summary\nwith multiple lines.");
    }

    #[test]
    fn test_parse_requirements_dash() {
        let input = "## Requirements\n- REQ-1: First\n- REQ-2: Second\n";
        let doc = parse_design_doc(input);
        assert_eq!(doc.requirements, vec!["REQ-1: First", "REQ-2: Second"]);
    }

    #[test]
    fn test_parse_requirements_asterisk() {
        let input = "## Requirements\n* First requirement\n* Second requirement\n";
        let doc = parse_design_doc(input);
        assert_eq!(
            doc.requirements,
            vec!["First requirement", "Second requirement"]
        );
    }

    #[test]
    fn test_parse_acceptance_criteria_checkboxes() {
        let input = "## Acceptance Criteria\n- [ ] AC-1: Not done\n- [x] AC-2: Already done\n";
        let doc = parse_design_doc(input);
        assert_eq!(
            doc.acceptance_criteria,
            vec!["AC-1: Not done", "AC-2: Already done"]
        );
    }

    #[test]
    fn test_parse_architecture() {
        let input = "## Architecture\n\nUse a layered approach.\n\nDatabase -> Service -> API\n";
        let doc = parse_design_doc(input);
        assert_eq!(
            doc.architecture,
            "Use a layered approach.\n\nDatabase -> Service -> API"
        );
    }

    #[test]
    fn test_parse_open_questions() {
        let input = "## Open Questions\n- Q1: Should we use Redis?\n- Q2: What about auth?\n";
        let doc = parse_design_doc(input);
        assert_eq!(
            doc.open_questions,
            vec!["Q1: Should we use Redis?", "Q2: What about auth?"]
        );
    }

    #[test]
    fn test_parse_out_of_scope() {
        let input = "## Out of Scope\n- Not doing X\n- Not doing Y\n";
        let doc = parse_design_doc(input);
        assert_eq!(doc.out_of_scope, vec!["Not doing X", "Not doing Y"]);
    }

    #[test]
    fn test_parse_unknown_sections() {
        let input = "## References\n\nSee RFC 1234.\n";
        let doc = parse_design_doc(input);
        assert_eq!(doc.unknown_sections.len(), 1);
        assert_eq!(doc.unknown_sections[0].0, "references");
        assert_eq!(doc.unknown_sections[0].1, "See RFC 1234.");
    }

    #[test]
    fn test_parse_h2_phase_headers_documented_form() {
        let input = "## Phase: Database Migrations\n- Add user table\n- Add session table\n\n## Phase: API Endpoints (parallel)\n- Registration endpoint\n- Login endpoint\n";
        let doc = parse_design_doc(input);
        assert_eq!(doc.requirement_groups.len(), 2);
        assert_eq!(doc.requirement_groups[0].name, "Database Migrations");
        assert_eq!(
            doc.requirement_groups[0].items,
            vec!["Add user table", "Add session table"]
        );
        assert_eq!(doc.requirement_groups[1].name, "API Endpoints");
        assert_eq!(doc.requirement_groups[1].execution_hint, "parallel");

        assert_eq!(doc.requirements.len(), 4);
        assert!(doc.unknown_sections.is_empty());
    }

    #[test]
    fn test_parse_h2_layer_numbered_and_no_false_positive() {
        let input = "## Layer 1: Foundation (sequential)\n- Base schema\n\n## Phased Rollout\n\nProse about rollout.\n";
        let doc = parse_design_doc(input);
        assert_eq!(doc.requirement_groups.len(), 1);
        assert_eq!(doc.requirement_groups[0].name, "Foundation");
        assert_eq!(doc.requirement_groups[0].execution_hint, "sequential");
        assert_eq!(doc.unknown_sections.len(), 1);
        assert_eq!(doc.unknown_sections[0].0, "phased rollout");
    }

    #[test]
    fn test_parse_case_insensitive_headings() {
        let input = "## SUMMARY\n\nUpper case heading.\n\n## REQUIREMENTS\n- Item one\n";
        let doc = parse_design_doc(input);
        assert_eq!(doc.summary, "Upper case heading.");
        assert_eq!(doc.requirements, vec!["Item one"]);
    }

    #[test]
    fn test_parse_mixed_bullet_styles() {
        let input = "## Requirements\n- Dash item\n* Star item\n- [ ] Checkbox item\n";
        let doc = parse_design_doc(input);
        assert_eq!(
            doc.requirements,
            vec!["Dash item", "Star item", "Checkbox item"]
        );
    }

    #[test]
    fn test_parse_full_document() {
        let input = "\
# Feature: Batch Retry Logic

## Summary

Add retry logic for batch operations.

## Requirements
- REQ-1: Retry up to 3 times
- REQ-2: Exponential backoff

## Acceptance Criteria
- [ ] AC-1: Retries work
- [x] AC-2: Logs show attempts

## Architecture

Use a middleware pattern.

## Open Questions
- Q1: Max retry count?

## Out of Scope
- Not handling network partitions
";
        let doc = parse_design_doc(input);
        assert_eq!(doc.title, "Batch Retry Logic");
        assert_eq!(doc.summary, "Add retry logic for batch operations.");
        assert_eq!(doc.requirements.len(), 2);
        assert_eq!(doc.acceptance_criteria.len(), 2);
        assert_eq!(doc.architecture, "Use a middleware pattern.");
        assert_eq!(doc.open_questions.len(), 1);
        assert_eq!(doc.out_of_scope.len(), 1);
    }

    #[test]
    fn test_parse_multiline_list_item() {
        let input = "## Requirements\n- First requirement\n  which continues here\n- Second\n";
        let doc = parse_design_doc(input);
        assert_eq!(doc.requirements.len(), 2);
        assert_eq!(
            doc.requirements[0],
            "First requirement which continues here"
        );
        assert_eq!(doc.requirements[1], "Second");
    }

    #[test]
    fn test_parse_uppercase_x_checkbox() {
        let input = "## Acceptance Criteria\n- [X] Done with uppercase X\n";
        let doc = parse_design_doc(input);
        assert_eq!(doc.acceptance_criteria, vec!["Done with uppercase X"]);
    }

    #[test]
    fn test_validate_complete_doc() {
        let doc = DesignDoc {
            title: "Test".to_string(),
            summary: "A summary".to_string(),
            requirements: vec!["REQ-1".to_string()],
            requirement_groups: Vec::new(),
            acceptance_criteria: vec!["AC-1".to_string()],
            architecture: String::new(),
            open_questions: Vec::new(),
            out_of_scope: Vec::new(),
            unknown_sections: Vec::new(),
        };
        assert!(validate_design_doc(&doc).is_empty());
    }

    #[test]
    fn test_validate_missing_all() {
        let doc = parse_design_doc("# Just a title\n");
        let warnings = validate_design_doc(&doc);
        assert_eq!(warnings.len(), 3);
        assert!(warnings[0].contains("Summary"));
        assert!(warnings[1].contains("Requirements"));
        assert!(warnings[2].contains("Acceptance Criteria"));
    }

    #[test]
    fn test_validate_partial_missing() {
        let doc = DesignDoc {
            title: "Test".to_string(),
            summary: "Present".to_string(),
            requirements: Vec::new(),
            requirement_groups: Vec::new(),
            acceptance_criteria: vec!["AC-1".to_string()],
            architecture: String::new(),
            open_questions: Vec::new(),
            out_of_scope: Vec::new(),
            unknown_sections: Vec::new(),
        };
        let warnings = validate_design_doc(&doc);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("Requirements"));
    }

    #[test]
    fn test_build_section_full() {
        let doc = DesignDoc {
            title: "Test".to_string(),
            summary: "A summary".to_string(),
            requirements: vec!["REQ-1: Do thing".to_string()],
            requirement_groups: Vec::new(),
            acceptance_criteria: vec!["AC-1: Verify thing".to_string()],
            architecture: "Layered".to_string(),
            open_questions: Vec::new(),
            out_of_scope: vec!["Not X".to_string()],
            unknown_sections: Vec::new(),
        };
        let section = build_design_doc_section(&doc);
        assert!(section.contains("## Design Specification"));
        assert!(section.contains("### Summary"));
        assert!(section.contains("A summary"));
        assert!(section.contains("### Requirements"));
        assert!(section.contains("- REQ-1: Do thing"));
        assert!(section.contains("### Acceptance Criteria"));
        assert!(section.contains("- [ ] AC-1: Verify thing"));
        assert!(section.contains("### Architecture"));
        assert!(section.contains("Layered"));
        assert!(section.contains("### Out of Scope"));
        assert!(section.contains("- Not X"));
    }

    #[test]
    fn test_build_section_empty_doc() {
        let doc = parse_design_doc("");
        let section = build_design_doc_section(&doc);
        assert!(section.contains("## Design Specification"));
        assert!(!section.contains("### Summary"));
        assert!(!section.contains("### Requirements"));
    }

    #[test]
    fn test_build_section_includes_unknown_sections() {
        let doc = DesignDoc {
            title: String::new(),
            summary: String::new(),
            requirements: Vec::new(),
            requirement_groups: Vec::new(),
            acceptance_criteria: Vec::new(),
            architecture: String::new(),
            open_questions: Vec::new(),
            out_of_scope: Vec::new(),
            unknown_sections: vec![("notes".to_string(), "Some notes here.".to_string())],
        };
        let section = build_design_doc_section(&doc);
        assert!(section.contains("### notes"));
        assert!(section.contains("Some notes here."));
    }

    #[test]
    fn test_escalation_none_when_no_questions() {
        let doc = parse_design_doc("");
        assert!(build_open_questions_escalation(&doc).is_none());
    }

    #[test]
    fn test_escalation_present_with_questions() {
        let doc = DesignDoc {
            title: String::new(),
            summary: String::new(),
            requirements: Vec::new(),
            requirement_groups: Vec::new(),
            acceptance_criteria: Vec::new(),
            architecture: String::new(),
            open_questions: vec![
                "Q1: Should we use Redis?".to_string(),
                "Q2: Auth strategy?".to_string(),
            ],
            out_of_scope: Vec::new(),
            unknown_sections: Vec::new(),
        };
        let escalation = build_open_questions_escalation(&doc).unwrap();
        assert!(escalation.contains("Open Questions"));
        assert!(escalation.contains("Escalation Required"));
        assert!(escalation.contains("1. Q1: Should we use Redis?"));
        assert!(escalation.contains("2. Q2: Auth strategy?"));
        assert!(escalation.contains("crosslink comment"));
        assert!(escalation.contains("blocker"));
    }

    #[test]
    fn test_strip_list_prefix_dash() {
        assert_eq!(strip_list_prefix("- hello"), Some("hello"));
    }

    #[test]
    fn test_strip_list_prefix_asterisk() {
        assert_eq!(strip_list_prefix("* hello"), Some("hello"));
    }

    #[test]
    fn test_strip_list_prefix_checkbox_unchecked() {
        assert_eq!(strip_list_prefix("- [ ] todo"), Some("todo"));
    }

    #[test]
    fn test_strip_list_prefix_checkbox_checked() {
        assert_eq!(strip_list_prefix("- [x] done"), Some("done"));
    }

    #[test]
    fn test_strip_list_prefix_no_prefix() {
        assert_eq!(strip_list_prefix("plain text"), None);
    }

    #[test]
    fn test_parse_h1_inside_code_fence_ignored() {
        let input = "\
# Real Title

## Summary

Some summary.

```bash
# This is a shell comment, not a heading
echo hello
```
";
        let doc = parse_design_doc(input);
        assert_eq!(doc.title, "Real Title");
        assert_eq!(
            doc.summary,
            "Some summary.\n\n```bash\n# This is a shell comment, not a heading\necho hello\n```"
        );
    }

    #[test]
    fn test_parse_h2_inside_code_fence_ignored() {
        let input = "\
# Real Title

## Requirements
- REQ-1: First

## Summary

Some summary.

```markdown
## This is not a section switch
```

Still in summary.
";
        let doc = parse_design_doc(input);
        assert_eq!(doc.title, "Real Title");
        assert_eq!(doc.requirements, vec!["REQ-1: First"]);

        assert!(doc.summary.contains("## This is not a section switch"));
        assert!(doc.summary.contains("Still in summary."));
    }

    #[test]
    fn test_parse_multiple_code_fences() {
        let input = "\
# Real Title

## Summary

Before code.

```
# Not a title
## Not a section
```

After first fence.

```python
# Python comment
```

Still in summary.
";
        let doc = parse_design_doc(input);
        assert_eq!(doc.title, "Real Title");
        assert!(doc.summary.contains("Before code."));
        assert!(doc.summary.contains("# Not a title"));
        assert!(doc.summary.contains("After first fence."));
        assert!(doc.summary.contains("# Python comment"));
        assert!(doc.summary.contains("Still in summary."));
    }

    #[test]
    fn test_parse_code_fence_does_not_affect_content_after() {
        let input = "\
# Real Title

## Summary

Summary text.

```
# shell comment
```

## Requirements
- REQ-1: After the fence
";
        let doc = parse_design_doc(input);
        assert_eq!(doc.title, "Real Title");
        assert_eq!(doc.requirements, vec!["REQ-1: After the fence"]);
    }

    #[test]
    fn test_parse_layer_headers_creates_groups() {
        let input = "\
# Feature: Secrets

## Requirements

### Layer 1: Foundation (sequential — everything depends on these)
- REQ-1: SecretBackend trait
- REQ-2: Extension traits

### Layer 2: Backends (parallel — each agent independent)
- REQ-3: EnvBackend
- REQ-4: FileBackend

### Layer 3: Delivery (sequential — depends on Layer 2)
- REQ-5: Container delivery
- REQ-6: E2E test
";
        let doc = parse_design_doc(input);
        assert_eq!(doc.requirement_groups.len(), 3);

        assert_eq!(doc.requirement_groups[0].name, "Foundation");
        assert_eq!(doc.requirement_groups[0].execution_hint, "sequential");
        assert_eq!(doc.requirement_groups[0].items.len(), 2);

        assert_eq!(doc.requirement_groups[1].name, "Backends");
        assert_eq!(doc.requirement_groups[1].execution_hint, "parallel");
        assert_eq!(doc.requirement_groups[1].items.len(), 2);

        assert_eq!(doc.requirement_groups[2].name, "Delivery");
        assert_eq!(doc.requirement_groups[2].execution_hint, "sequential");
        assert_eq!(doc.requirement_groups[2].items.len(), 2);

        assert_eq!(doc.requirements.len(), 6);
    }

    #[test]
    fn test_parse_no_layer_headers_no_groups() {
        let input = "\
## Requirements
- REQ-1: First
- REQ-2: Second
";
        let doc = parse_design_doc(input);
        assert!(doc.requirement_groups.is_empty());
        assert_eq!(doc.requirements.len(), 2);
    }

    #[test]
    fn test_parse_layer_header_no_hint() {
        let input = "\
## Requirements

### Layer 1: Foundation
- REQ-1: Thing
";
        let doc = parse_design_doc(input);
        assert_eq!(doc.requirement_groups.len(), 1);
        assert_eq!(doc.requirement_groups[0].name, "Foundation");
        assert_eq!(doc.requirement_groups[0].execution_hint, "");
    }

    #[test]
    fn test_parse_phase_header_variant() {
        let input = "\
## Requirements

### Phase 1: Setup
- REQ-1: Init
";
        let doc = parse_design_doc(input);
        assert_eq!(doc.requirement_groups.len(), 1);
        assert_eq!(doc.requirement_groups[0].name, "Setup");
    }

    #[test]
    fn test_sub_bullets_collapsed_into_parent() {
        let input = "\
## Requirements
- REQ-1: Error enum
  - SecretNotProvided
  - SecretNotFound
  - BackendError
- REQ-2: Config section
";
        let doc = parse_design_doc(input);
        assert_eq!(doc.requirements.len(), 2);
        assert!(doc.requirements[0].contains("SecretNotProvided"));
        assert!(doc.requirements[0].contains("SecretNotFound"));
        assert!(doc.requirements[0].contains("BackendError"));
        assert_eq!(doc.requirements[1], "REQ-2: Config section");
    }

    #[test]
    fn test_sub_bullets_in_layer_groups() {
        let input = "\
## Requirements

### Layer 1: Foundation (sequential)
- REQ-1: Error enum
  - SecretNotProvided
  - SecretNotFound
- REQ-2: Config
";
        let doc = parse_design_doc(input);
        assert_eq!(doc.requirement_groups[0].items.len(), 2);
        assert!(doc.requirement_groups[0].items[0].contains("SecretNotProvided"));
    }

    #[test]
    fn test_parse_layer_header_details() {
        let (name, hint) =
            parse_layer_header("Layer 1: Foundation (sequential — everything depends on these)");
        assert_eq!(name, "Foundation");
        assert_eq!(hint, "sequential");
    }

    #[test]
    fn test_parse_layer_header_parallel() {
        let (name, hint) =
            parse_layer_header("Phase 2: Backends + Integration (parallel — each independent)");
        assert_eq!(name, "Backends + Integration");
        assert_eq!(hint, "parallel");
    }

    #[test]
    fn test_parse_layer_header_no_parens() {
        let (name, hint) = parse_layer_header("Layer 3: End-to-end delivery");
        assert_eq!(name, "End-to-end delivery");
        assert_eq!(hint, "");
    }
}
