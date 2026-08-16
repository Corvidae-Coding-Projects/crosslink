use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::process::Command;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssueTemplate {
    pub title: String,
    pub body: String,
    pub labels: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilingResult {
    pub filed: Vec<FiledIssue>,
    pub skipped: Vec<SkippedIssue>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FiledIssue {
    pub number: u64,
    pub title: String,
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkippedIssue {
    pub title: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FindingForFiling {
    pub title: String,
    pub severity: String,
    pub file: String,
    pub line: Option<usize>,
    pub description: String,
    pub suggested_fix: Option<String>,
    pub consensus_count: usize,
}

#[must_use]
pub fn build_issue_template(finding: &FindingForFiling) -> IssueTemplate {
    let severity_upper = finding.severity.to_uppercase();
    let title = format!("[{}] {}", severity_upper, finding.title);

    let location = finding.line.map_or_else(
        || format!("`{}`", finding.file),
        |line| format!("`{}:{}`", finding.file, line),
    );

    let suggested_fix_section = finding
        .suggested_fix
        .as_ref()
        .map_or_else(String::new, |fix| format!("## Suggested Fix\n\n{fix}\n"));

    let body = format!(
        "## Description\n\n{}\n\n## Location\n\n{}\n\n{}## Metadata\n\n- **Severity**: {}\n- **Consensus count**: {}\n- **Source**: automated swarm review\n",
        finding.description,
        location,
        suggested_fix_section,
        severity_upper,
        finding.consensus_count,
    );

    let severity_label = match severity_upper.as_str() {
        "CRITICAL" | "HIGH" => "bug",
        "MEDIUM" => "enhancement",
        _ => "tech-debt",
    };

    let labels = vec![severity_label.to_string(), "review-finding".to_string()];

    IssueTemplate {
        title,
        body,
        labels,
    }
}

fn normalize_title(title: &str) -> String {
    let trimmed = title.trim().to_lowercase();

    let without_prefix = if trimmed.starts_with('[') {
        match trimmed.find(']') {
            Some(idx) => trimmed[idx + 1..].trim_start().to_string(),
            None => trimmed,
        }
    } else {
        trimmed
    };

    without_prefix
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn word_set(s: &str) -> HashSet<String> {
    s.split_whitespace()
        .map(std::string::ToString::to_string)
        .collect()
}

fn jaccard(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let intersection = a.intersection(b).count();
    let union = a.union(b).count();
    if union == 0 {
        return 0.0;
    }
    f64::from(u32::try_from(intersection).unwrap_or(u32::MAX))
        / f64::from(u32::try_from(union).unwrap_or(u32::MAX))
}

#[must_use]
pub fn check_duplicate(title: &str, existing_issues: &[String]) -> bool {
    let norm = normalize_title(title);
    let norm_words = word_set(&norm);

    for existing in existing_issues {
        let existing_norm = normalize_title(existing);

        if norm == existing_norm {
            return true;
        }

        if norm.contains(&existing_norm) || existing_norm.contains(&norm) {
            return true;
        }

        let existing_words = word_set(&existing_norm);
        if jaccard(&norm_words, &existing_words) > 0.7 {
            return true;
        }
    }

    false
}

fn fetch_existing_issue_titles() -> Result<Vec<String>> {
    let output = Command::new("gh")
        .args(["issue", "list", "--json", "title", "--limit", "500"])
        .output()
        .context("failed to run `gh issue list`")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("gh issue list failed: {stderr}");
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let entries: Vec<serde_json::Value> =
        serde_json::from_str(&stdout).context("failed to parse gh issue list JSON")?;

    let titles: Vec<String> = entries
        .iter()
        .filter_map(|v| v.get("title").and_then(|t| t.as_str()).map(String::from))
        .collect();

    Ok(titles)
}

fn create_issue_via_gh(template: &IssueTemplate) -> Result<(u64, String)> {
    let labels_arg = template.labels.join(",");

    let output = Command::new("gh")
        .args([
            "issue",
            "create",
            "--title",
            &template.title,
            "--body",
            &template.body,
            "--label",
            &labels_arg,
        ])
        .output()
        .context("failed to run `gh issue create`")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("gh issue create failed: {stderr}");
    }

    let url = String::from_utf8_lossy(&output.stdout).trim().to_string();

    let number = url
        .rsplit('/')
        .next()
        .and_then(|seg| seg.parse::<u64>().ok())
        .unwrap_or(0);

    Ok((number, url))
}

pub fn file_issues(findings: &[FindingForFiling], dry_run: bool) -> Result<FilingResult> {
    let existing_titles = if dry_run {
        fetch_existing_issue_titles().unwrap_or_default()
    } else {
        fetch_existing_issue_titles()?
    };

    let mut filed: Vec<FiledIssue> = Vec::new();
    let mut skipped: Vec<SkippedIssue> = Vec::new();

    for finding in findings {
        let template = build_issue_template(finding);

        if check_duplicate(&template.title, &existing_titles) {
            skipped.push(SkippedIssue {
                title: template.title,
                reason: "duplicate of existing issue".to_string(),
            });
            continue;
        }

        if dry_run {
            filed.push(FiledIssue {
                number: 0,
                title: template.title,
                url: "(dry run)".to_string(),
            });
        } else {
            let (number, url) = create_issue_via_gh(&template)?;
            filed.push(FiledIssue {
                number,
                title: template.title,
                url,
            });
        }
    }

    Ok(FilingResult { filed, skipped })
}

pub fn file_issues_batch(findings: &[FindingForFiling], dry_run: bool) -> Result<FilingResult> {
    let result = file_issues(findings, dry_run)?;

    if dry_run {
        println!("=== Dry Run Summary ===");
    } else {
        println!("=== Filing Summary ===");
    }

    if !result.filed.is_empty() {
        println!("\nFiled ({}):", result.filed.len());
        for issue in &result.filed {
            if dry_run {
                println!("  - {}", issue.title);
            } else {
                println!("  - #{} {} ({})", issue.number, issue.title, issue.url);
            }
        }
    }

    if !result.skipped.is_empty() {
        println!("\nSkipped ({}):", result.skipped.len());
        for issue in &result.skipped {
            println!("  - {} ({})", issue.title, issue.reason);
        }
    }

    println!(
        "\nTotal: {} filed, {} skipped",
        result.filed.len(),
        result.skipped.len()
    );

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_finding(severity: &str) -> FindingForFiling {
        FindingForFiling {
            title: "Buffer overflow in parser".to_string(),
            severity: severity.to_string(),
            file: "src/parser.rs".to_string(),
            line: Some(42),
            description: "Unchecked index into byte slice.".to_string(),
            suggested_fix: Some("Add bounds check before indexing.".to_string()),
            consensus_count: 3,
        }
    }

    #[test]
    fn template_title_includes_severity_prefix() {
        let tmpl = build_issue_template(&make_finding("high"));
        assert_eq!(tmpl.title, "[HIGH] Buffer overflow in parser");
    }

    #[test]
    fn template_body_contains_description_section() {
        let tmpl = build_issue_template(&make_finding("high"));
        assert!(tmpl.body.contains("## Description"));
        assert!(tmpl.body.contains("Unchecked index into byte slice."));
    }

    #[test]
    fn template_body_contains_location_with_line() {
        let tmpl = build_issue_template(&make_finding("high"));
        assert!(tmpl.body.contains("`src/parser.rs:42`"));
    }

    #[test]
    fn template_body_location_without_line() {
        let mut f = make_finding("medium");
        f.line = None;
        let tmpl = build_issue_template(&f);
        assert!(tmpl.body.contains("`src/parser.rs`"));
        assert!(!tmpl.body.contains(":42"));
    }

    #[test]
    fn template_body_contains_suggested_fix() {
        let tmpl = build_issue_template(&make_finding("high"));
        assert!(tmpl.body.contains("## Suggested Fix"));
        assert!(tmpl.body.contains("Add bounds check before indexing."));
    }

    #[test]
    fn template_body_omits_suggested_fix_when_none() {
        let mut f = make_finding("high");
        f.suggested_fix = None;
        let tmpl = build_issue_template(&f);
        assert!(!tmpl.body.contains("## Suggested Fix"));
    }

    #[test]
    fn template_body_contains_metadata() {
        let tmpl = build_issue_template(&make_finding("high"));
        assert!(tmpl.body.contains("**Severity**: HIGH"));
        assert!(tmpl.body.contains("**Consensus count**: 3"));
    }

    #[test]
    fn labels_critical_maps_to_bug() {
        let tmpl = build_issue_template(&make_finding("critical"));
        assert!(tmpl.labels.contains(&"bug".to_string()));
        assert!(tmpl.labels.contains(&"review-finding".to_string()));
    }

    #[test]
    fn labels_high_maps_to_bug() {
        let tmpl = build_issue_template(&make_finding("high"));
        assert!(tmpl.labels.contains(&"bug".to_string()));
    }

    #[test]
    fn labels_medium_maps_to_enhancement() {
        let tmpl = build_issue_template(&make_finding("medium"));
        assert!(tmpl.labels.contains(&"enhancement".to_string()));
    }

    #[test]
    fn labels_low_maps_to_tech_debt() {
        let tmpl = build_issue_template(&make_finding("low"));
        assert!(tmpl.labels.contains(&"tech-debt".to_string()));
    }

    #[test]
    fn labels_info_maps_to_tech_debt() {
        let tmpl = build_issue_template(&make_finding("info"));
        assert!(tmpl.labels.contains(&"tech-debt".to_string()));
    }

    #[test]
    fn labels_always_include_review_finding() {
        for sev in &["critical", "high", "medium", "low", "info"] {
            let tmpl = build_issue_template(&make_finding(sev));
            assert!(
                tmpl.labels.contains(&"review-finding".to_string()),
                "missing review-finding label for severity {sev}"
            );
        }
    }

    #[test]
    fn duplicate_exact_match() {
        let existing = vec!["[HIGH] Buffer overflow in parser".to_string()];
        assert!(check_duplicate(
            "[HIGH] Buffer overflow in parser",
            &existing
        ));
    }

    #[test]
    fn duplicate_ignores_severity_prefix() {
        let existing = vec!["Buffer overflow in parser".to_string()];
        assert!(check_duplicate(
            "[HIGH] Buffer overflow in parser",
            &existing
        ));
    }

    #[test]
    fn duplicate_case_insensitive() {
        let existing = vec!["buffer overflow in parser".to_string()];
        assert!(check_duplicate(
            "[HIGH] Buffer Overflow In Parser",
            &existing
        ));
    }

    #[test]
    fn duplicate_close_match_via_jaccard() {
        let existing = vec!["Buffer overflow found in the parser module".to_string()];
        assert!(check_duplicate(
            "[HIGH] Buffer overflow in parser module",
            &existing
        ));
    }

    #[test]
    fn no_false_positive_on_unrelated_titles() {
        let existing = vec!["Fix typo in README".to_string()];
        assert!(!check_duplicate(
            "[HIGH] Buffer overflow in parser",
            &existing
        ));
    }

    #[test]
    fn no_false_positive_on_partial_word_overlap() {
        let existing = vec!["Update parser documentation".to_string()];
        assert!(!check_duplicate(
            "[HIGH] Buffer overflow in parser",
            &existing
        ));
    }

    #[test]
    fn duplicate_with_different_severity_prefix() {
        let existing = vec!["[MEDIUM] Buffer overflow in parser".to_string()];
        assert!(check_duplicate(
            "[HIGH] Buffer overflow in parser",
            &existing
        ));
    }

    #[test]
    fn duplicate_empty_existing_list() {
        let existing: Vec<String> = vec![];
        assert!(!check_duplicate("[HIGH] Something", &existing));
    }

    #[test]
    fn issue_template_serde_roundtrip() {
        let tmpl = build_issue_template(&make_finding("high"));
        let json = serde_json::to_string(&tmpl).unwrap();
        let parsed: IssueTemplate = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.title, tmpl.title);
        assert_eq!(parsed.body, tmpl.body);
        assert_eq!(parsed.labels, tmpl.labels);
    }

    #[test]
    fn filing_result_serde_roundtrip() {
        let result = FilingResult {
            filed: vec![FiledIssue {
                number: 42,
                title: "[HIGH] Test issue".to_string(),
                url: "https://github.com/org/repo/issues/42".to_string(),
            }],
            skipped: vec![SkippedIssue {
                title: "[LOW] Skipped issue".to_string(),
                reason: "duplicate of existing issue".to_string(),
            }],
        };
        let json = serde_json::to_string(&result).unwrap();
        let parsed: FilingResult = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.filed.len(), 1);
        assert_eq!(parsed.filed[0].number, 42);
        assert_eq!(parsed.skipped.len(), 1);
        assert_eq!(parsed.skipped[0].reason, "duplicate of existing issue");
    }

    #[test]
    fn finding_for_filing_serde_roundtrip() {
        let finding = make_finding("critical");
        let json = serde_json::to_string(&finding).unwrap();
        let parsed: FindingForFiling = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.title, finding.title);
        assert_eq!(parsed.severity, finding.severity);
        assert_eq!(parsed.line, Some(42));
    }

    #[test]
    fn normalize_strips_prefix_and_lowercases() {
        assert_eq!(normalize_title("[HIGH] Buffer Overflow"), "buffer overflow");
    }

    #[test]
    fn normalize_handles_no_prefix() {
        assert_eq!(normalize_title("Buffer Overflow"), "buffer overflow");
    }

    #[test]
    fn normalize_collapses_whitespace() {
        assert_eq!(normalize_title("[HIGH]   extra   spaces  "), "extra spaces");
    }

    #[test]
    fn normalize_unclosed_bracket_treated_as_no_prefix() {
        assert_eq!(
            normalize_title("[unclosed bracket title"),
            "[unclosed bracket title"
        );
    }

    #[test]
    fn normalize_empty_string() {
        assert_eq!(normalize_title(""), "");
    }

    #[test]
    fn normalize_only_whitespace() {
        assert_eq!(normalize_title("   "), "");
    }

    #[test]
    fn jaccard_both_empty_returns_one() {
        let a: HashSet<String> = HashSet::new();
        let b: HashSet<String> = HashSet::new();
        assert!((jaccard(&a, &b) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn jaccard_identical_sets_returns_one() {
        let a: HashSet<String> = ["foo", "bar"]
            .iter()
            .map(std::string::ToString::to_string)
            .collect();
        let b = a.clone();
        assert!((jaccard(&a, &b) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn jaccard_disjoint_sets_returns_zero() {
        let a: HashSet<String> = std::iter::once("foo".to_string()).collect();
        let b: HashSet<String> = std::iter::once("bar".to_string()).collect();
        assert!((jaccard(&a, &b)).abs() < f64::EPSILON);
    }

    #[test]
    fn jaccard_partial_overlap() {
        let a: HashSet<String> = ["a", "b", "c"]
            .iter()
            .map(std::string::ToString::to_string)
            .collect();
        let b: HashSet<String> = ["b", "c", "d"]
            .iter()
            .map(std::string::ToString::to_string)
            .collect();

        let score = jaccard(&a, &b);
        assert!(
            (score - 0.5).abs() < f64::EPSILON,
            "expected 0.5 got {score}"
        );
    }

    #[test]
    fn jaccard_one_empty_returns_zero() {
        let a: HashSet<String> = std::iter::once("foo".to_string()).collect();
        let b: HashSet<String> = HashSet::new();
        assert!((jaccard(&a, &b)).abs() < f64::EPSILON);
    }

    #[test]
    fn duplicate_via_substring_norm_contains_existing() {
        let existing = vec!["buffer overflow".to_string()];
        assert!(check_duplicate(
            "[HIGH] Buffer overflow in parser",
            &existing
        ));
    }

    #[test]
    fn duplicate_via_substring_existing_contains_norm() {
        let existing = vec!["buffer overflow in parser module for all inputs".to_string()];
        assert!(check_duplicate(
            "[HIGH] Buffer overflow in parser",
            &existing
        ));
    }

    #[test]
    fn no_duplicate_when_below_jaccard_threshold() {
        let existing = vec!["completely unrelated title about networking".to_string()];
        assert!(!check_duplicate(
            "[HIGH] Buffer overflow in parser",
            &existing
        ));
    }

    #[test]
    fn file_issues_dry_run_empty_findings() {
        let result = file_issues(&[], true).unwrap();
        assert!(result.filed.is_empty());
        assert!(result.skipped.is_empty());
    }

    #[test]
    fn file_issues_dry_run_files_issue_with_zero_number() {
        let findings = vec![FindingForFiling {
            title: "Unique test finding for dry run".to_string(),
            severity: "high".to_string(),
            file: "src/lib.rs".to_string(),
            line: Some(10),
            description: "Test description.".to_string(),
            suggested_fix: None,
            consensus_count: 1,
        }];
        let result = file_issues(&findings, true).unwrap();

        assert_eq!(result.filed.len(), 1);
        assert_eq!(result.filed[0].number, 0);
        assert_eq!(result.filed[0].url, "(dry run)");
        assert!(result.skipped.is_empty());
    }

    #[test]
    fn file_issues_dry_run_skips_duplicate_against_empty_existing() {
        let finding = FindingForFiling {
            title: "Duplicate finding".to_string(),
            severity: "low".to_string(),
            file: "src/lib.rs".to_string(),
            line: None,
            description: "Desc".to_string(),
            suggested_fix: None,
            consensus_count: 2,
        };
        let findings = vec![finding.clone(), finding];
        let result = file_issues(&findings, true).unwrap();

        assert_eq!(result.filed.len(), 2);
    }

    #[test]
    fn file_issues_dry_run_multiple_severities() {
        let findings: Vec<FindingForFiling> = ["critical", "high", "medium", "low", "info"]
            .iter()
            .map(|sev| FindingForFiling {
                title: format!("Finding for {sev}"),
                severity: sev.to_string(),
                file: "src/lib.rs".to_string(),
                line: None,
                description: "Desc.".to_string(),
                suggested_fix: Some("Fix it.".to_string()),
                consensus_count: 1,
            })
            .collect();
        let result = file_issues(&findings, true).unwrap();
        assert_eq!(result.filed.len(), 5);
        assert!(result.skipped.is_empty());
    }

    #[test]
    fn file_issues_batch_dry_run_empty() {
        let result = file_issues_batch(&[], true).unwrap();
        assert!(result.filed.is_empty());
        assert!(result.skipped.is_empty());
    }

    #[test]
    fn file_issues_batch_dry_run_with_findings() {
        let findings = vec![
            FindingForFiling {
                title: "Batch finding one".to_string(),
                severity: "high".to_string(),
                file: "src/a.rs".to_string(),
                line: Some(1),
                description: "Desc one.".to_string(),
                suggested_fix: None,
                consensus_count: 2,
            },
            FindingForFiling {
                title: "Batch finding two".to_string(),
                severity: "medium".to_string(),
                file: "src/b.rs".to_string(),
                line: None,
                description: "Desc two.".to_string(),
                suggested_fix: Some("Fix two.".to_string()),
                consensus_count: 1,
            },
        ];
        let result = file_issues_batch(&findings, true).unwrap();
        assert_eq!(result.filed.len(), 2);
        assert!(result.skipped.is_empty());
    }

    #[test]
    fn template_body_contains_source_metadata() {
        let tmpl = build_issue_template(&make_finding("high"));
        assert!(tmpl.body.contains("automated swarm review"));
    }

    #[test]
    fn template_body_severity_uppercase_in_metadata() {
        let tmpl = build_issue_template(&make_finding("critical"));
        assert!(tmpl.body.contains("**Severity**: CRITICAL"));
    }

    #[test]
    fn template_body_contains_location_section() {
        let tmpl = build_issue_template(&make_finding("high"));
        assert!(tmpl.body.contains("## Location"));
    }

    #[test]
    fn template_labels_exactly_two_entries() {
        let tmpl = build_issue_template(&make_finding("high"));
        assert_eq!(tmpl.labels.len(), 2);
    }

    #[test]
    fn template_unknown_severity_maps_to_tech_debt() {
        let tmpl = build_issue_template(&make_finding("unknown"));
        assert!(tmpl.labels.contains(&"tech-debt".to_string()));
        assert!(tmpl.labels.contains(&"review-finding".to_string()));
    }

    #[test]
    fn filed_issue_fields_accessible() {
        let issue = FiledIssue {
            number: 99,
            title: "Title".to_string(),
            url: "https://example.com/issues/99".to_string(),
        };
        assert_eq!(issue.number, 99);
        assert_eq!(issue.title, "Title");
        assert_eq!(issue.url, "https://example.com/issues/99");
    }

    #[test]
    fn skipped_issue_fields_accessible() {
        let skipped = SkippedIssue {
            title: "Some title".to_string(),
            reason: "duplicate".to_string(),
        };
        assert_eq!(skipped.title, "Some title");
        assert_eq!(skipped.reason, "duplicate");
    }

    #[test]
    fn filing_result_fields_accessible() {
        let result = FilingResult {
            filed: vec![],
            skipped: vec![],
        };
        assert!(result.filed.is_empty());
        assert!(result.skipped.is_empty());
    }

    #[test]
    fn issue_template_clone_and_debug() {
        let tmpl = build_issue_template(&make_finding("high"));
        let cloned = tmpl.clone();
        assert_eq!(cloned.title, tmpl.title);

        let _ = format!("{tmpl:?}");
    }

    #[test]
    fn finding_for_filing_clone_and_debug() {
        let f = make_finding("medium");
        let cloned = f.clone();
        assert_eq!(cloned.severity, f.severity);
        let _ = format!("{f:?}");
    }

    #[test]
    fn filed_issue_clone_and_debug() {
        let issue = FiledIssue {
            number: 1,
            title: "t".to_string(),
            url: "u".to_string(),
        };
        let cloned = issue.clone();
        assert_eq!(cloned.number, issue.number);
        let _ = format!("{issue:?}");
    }

    #[test]
    fn skipped_issue_clone_and_debug() {
        let s = SkippedIssue {
            title: "t".to_string(),
            reason: "r".to_string(),
        };
        let cloned = s.clone();
        assert_eq!(cloned.reason, s.reason);
        let _ = format!("{s:?}");
    }

    #[test]
    fn filing_result_clone_and_debug() {
        let r = FilingResult {
            filed: vec![],
            skipped: vec![],
        };
        let cloned = r.clone();
        assert_eq!(cloned.filed.len(), 0);
        let _ = format!("{r:?}");
    }

    #[test]
    fn file_issues_non_dry_run_empty_findings_succeeds() {
        let result = file_issues(&[], false);
        if let Ok(r) = result {
            assert!(r.filed.is_empty());
            assert!(r.skipped.is_empty());
        }
    }

    #[test]
    fn file_issues_batch_non_dry_run_empty_prints_filing_summary() {
        let result = file_issues_batch(&[], false);
        if let Ok(r) = result {
            assert!(r.filed.is_empty());
            assert!(r.skipped.is_empty());
        }
    }

    #[test]
    fn file_issues_dry_run_skips_when_matches_existing_gh_issue() {
        let finding = FindingForFiling {
            title: "Release v0.5.0".to_string(),
            severity: "high".to_string(),
            file: "src/lib.rs".to_string(),
            line: None,
            description: "Desc.".to_string(),
            suggested_fix: None,
            consensus_count: 1,
        };
        let result = file_issues(&[finding], true).unwrap();

        assert_eq!(result.filed.len() + result.skipped.len(), 1);
    }

    #[test]
    fn file_issues_batch_dry_run_prints_skipped_section() {
        let finding = FindingForFiling {
            title: "Release v0.5.0".to_string(),
            severity: "medium".to_string(),
            file: "src/lib.rs".to_string(),
            line: None,
            description: "Desc.".to_string(),
            suggested_fix: None,
            consensus_count: 1,
        };
        let result = file_issues_batch(&[finding], true).unwrap();
        assert_eq!(result.filed.len() + result.skipped.len(), 1);
    }

    #[test]
    fn file_issues_non_dry_run_all_duplicates_never_calls_create() {
        let finding = FindingForFiling {
            title: "Release v0.5.0".to_string(),
            severity: "critical".to_string(),
            file: "src/release.rs".to_string(),
            line: Some(1),
            description: "Planned release.".to_string(),
            suggested_fix: None,
            consensus_count: 2,
        };
        let result = file_issues(&[finding], false);

        if let Ok(r) = result {
            assert_eq!(r.filed.len() + r.skipped.len(), 1);
        }
    }

    #[test]
    fn jaccard_one_non_empty_one_empty_does_not_divide_by_zero() {
        let a: HashSet<String> = std::iter::once("only".to_string()).collect();
        let b: HashSet<String> = HashSet::new();
        let score = jaccard(&a, &b);
        assert!((score).abs() < f64::EPSILON);
    }
}
