use super::harness::{assert_stdout_contains, SmokeHarness};

fn count_issues(h: &SmokeHarness, status: &str) -> usize {
    let result = h.run_ok(&["list", "-s", status, "--json"]);
    let parsed: serde_json::Value = serde_json::from_str(&result.stdout).unwrap_or_else(|e| {
        panic!(
            "failed to parse issue list JSON: {}\nstdout was:\n{}",
            e, result.stdout
        )
    });
    parsed
        .as_array()
        .map(|a| a.len())
        .unwrap_or_else(|| panic!("expected JSON array, got: {}", result.stdout))
}

fn assert_issue_count_flat(h: &SmokeHarness, status: &str, expected: usize) {
    let count = count_issues(h, status);
    assert_eq!(
        count, expected,
        "expected {expected} issues with status {status:?}, got {count}"
    );
}

#[test]
fn test_tui_help() {
    let h = SmokeHarness::new();
    let result = h.run_ok(&["tui", "--help"]);
    assert_stdout_contains(&result, "Open the terminal workspace overview");
    assert_stdout_contains(&result, "Usage:");
}

#[test]
fn test_roundtrip_create_show() {
    let h = SmokeHarness::new();
    let titles = [
        "Simple title",
        "Title with 'quotes' and \"double quotes\"",
        "Title with special chars: @#$%^&*()",
        "Unicode: cafe\u{0301} re\u{0301}sume\u{0301} nai\u{0308}ve",
        "Very long title that goes on and on and should still be stored correctly even when it contains many words and reaches a significant length",
    ];
    for (i, title) in titles.iter().enumerate() {
        h.run_ok(&["create", title]);
        let result = h.run_ok(&["show", &(i + 1).to_string()]);
        assert!(
            result.stdout_contains(title),
            "show for issue {} didn't contain title {:?}.\nGot: {}",
            i + 1,
            title,
            result.stdout
        );
    }
}

#[test]
fn test_roundtrip_label_list() {
    let h = SmokeHarness::new();

    let labels = ["bug", "feature", "docs", "ci", "refactor"];
    for (i, label) in labels.iter().enumerate() {
        h.run_ok(&["create", &format!("Issue for label {label}")]);
        h.run_ok(&["issue", "label", &(i + 1).to_string(), label]);
    }

    for label in &labels {
        let result = h.run_ok(&["list", "-l", label]);
        assert!(
            result.stdout_contains(&format!("Issue for label {label}")),
            "list with -l {} didn't contain expected issue.\nGot: {}",
            label,
            result.stdout
        );
    }

    let result = h.run_ok(&["list", "-l", "nonexistent"]);
    assert!(
        result.stdout_contains("No issues found"),
        "list with non-existent label should show no issues.\nGot: {}",
        result.stdout
    );
}

#[test]
fn test_roundtrip_comment_trail() {
    let h = SmokeHarness::new();
    h.run_ok(&["create", "Comment trail test"]);

    let comments = [
        ("Planning the approach", "plan"),
        ("Decided on strategy X", "decision"),
        ("Found a bottleneck", "observation"),
        ("Blocked on dependency", "blocker"),
        ("Resolved the bottleneck", "resolution"),
        ("Final outcome recorded", "result"),
    ];

    for (text, kind) in &comments {
        h.run_ok(&["issue", "comment", "1", text, "--kind", kind]);
    }

    let result = h.run_ok(&["workflow", "trail", "1"]);
    for (text, kind) in &comments {
        assert!(
            result.stdout_contains(text),
            "trail missing comment text {:?}.\nGot: {}",
            text,
            result.stdout
        );
        assert!(
            result.stdout_contains(&format!("[{kind}]")),
            "trail missing kind tag [{}].\nGot: {}",
            kind,
            result.stdout
        );
    }
}

#[test]
fn test_roundtrip_export_import() {
    let h = SmokeHarness::new();

    for i in 1..=5 {
        h.run_ok(&["create", &format!("Export issue {i}"), "-p", "medium"]);
        h.run_ok(&["issue", "label", &i.to_string(), "test-label"]);
        h.run_ok(&[
            "issue",
            "comment",
            &i.to_string(),
            &format!("Comment on {i}"),
        ]);
    }

    let export_path = h.temp_dir.path().join("export.json");
    h.run_ok(&["export", "-f", "json", "-o", export_path.to_str().unwrap()]);

    assert!(export_path.exists(), "export file was not created");
    let content = std::fs::read_to_string(&export_path).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&content)
        .unwrap_or_else(|e| panic!("export JSON is invalid: {e}\ncontent: {content}"));
    assert_eq!(
        parsed.as_array().map(|a| a.len()),
        Some(5),
        "expected 5 issues in export"
    );

    let h2 = SmokeHarness::new();
    let import_path = h2.temp_dir.path().join("import.json");
    std::fs::copy(&export_path, &import_path).unwrap();
    h2.run_ok(&["import", import_path.to_str().unwrap()]);

    assert_issue_count_flat(&h2, "all", 5);

    let list_result = h2.run_ok(&["list", "-s", "all"]);
    for i in 1..=5 {
        assert!(
            list_result.stdout_contains(&format!("Export issue {i}")),
            "imported issue {} missing from list.\nGot: {}",
            i,
            list_result.stdout
        );
    }
}

#[test]
fn test_roundtrip_milestone_issues() {
    let h = SmokeHarness::new();
    h.run_ok(&["sync"]);

    h.run_ok(&["milestone", "create", "v1.0-test"]);

    let issue_titles = [
        "Milestone issue alpha",
        "Milestone issue beta",
        "Milestone issue gamma",
    ];
    for (i, title) in issue_titles.iter().enumerate() {
        h.run_ok(&["create", title]);
        h.run_ok(&["milestone", "add", "1", &(i + 1).to_string()]);
    }

    let result = h.run_ok(&["milestone", "show", "1"]);
    assert_stdout_contains(&result, "v1.0-test");
    for title in &issue_titles {
        assert!(
            result.stdout_contains(title),
            "milestone show missing issue {:?}.\nGot: {}",
            title,
            result.stdout
        );
    }
    assert_stdout_contains(&result, "0/3");
}

#[test]
fn test_regression_empty_description() {
    let h = SmokeHarness::new();

    let result = h.run_ok(&["create", "Empty desc issue", "-d", ""]);
    assert_stdout_contains(&result, "Created issue #1");

    let show = h.run_ok(&["show", "1"]);
    assert_stdout_contains(&show, "Empty desc issue");
}

#[test]
fn test_regression_single_char_label() {
    let h = SmokeHarness::new();
    h.run_ok(&["create", "Single char label test"]);

    h.run_ok(&["issue", "label", "1", "a"]);

    let result = h.run_ok(&["show", "1"]);
    assert_stdout_contains(&result, "a");

    let list_result = h.run_ok(&["list", "-l", "a"]);
    assert_stdout_contains(&list_result, "Single char label test");
}

#[test]
fn test_regression_large_id_show() {
    let h = SmokeHarness::new();

    let result = h.run_err(&["show", "1000"]);
    assert!(
        result.stderr_contains("not found") || result.stdout_contains("not found"),
        "expected 'not found' error for non-existent ID 1000.\nstdout: {}\nstderr: {}",
        result.stdout,
        result.stderr
    );
}

#[test]
fn test_regression_special_search() {
    let h = SmokeHarness::new();
    h.run_ok(&["create", "Normal issue one"]);
    h.run_ok(&["create", "Normal issue two"]);

    let result = h.run(&["issue", "search", "%"]);

    assert!(
        result.success,
        "search with '%' should not cause a crash.\nstderr: {}",
        result.stderr
    );
}

#[test]
fn test_regression_subissue_chain() {
    let h = SmokeHarness::new();

    h.run_ok(&["create", "Chain parent"]);
    h.run_ok(&["issue", "create", "Chain child", "--parent", "1"]);
    h.run_ok(&["issue", "create", "Chain grandchild", "--parent", "2"]);

    let result = h.run_ok(&["issue", "tree"]);
    assert_stdout_contains(&result, "Chain parent");
    assert_stdout_contains(&result, "Chain child");
    assert_stdout_contains(&result, "Chain grandchild");

    let lines: Vec<&str> = result.stdout.lines().collect();
    let parent_line = lines.iter().position(|l| l.contains("Chain parent"));
    let child_line = lines
        .iter()
        .position(|l| l.contains("Chain child") && !l.contains("Chain parent"));
    let grandchild_line = lines.iter().position(|l| l.contains("Chain grandchild"));

    assert!(parent_line.is_some(), "parent not found in tree");
    assert!(child_line.is_some(), "child not found in tree");
    assert!(grandchild_line.is_some(), "grandchild not found in tree");

    assert!(
        parent_line.unwrap() < child_line.unwrap(),
        "parent should appear before child in tree"
    );
    assert!(
        child_line.unwrap() < grandchild_line.unwrap(),
        "child should appear before grandchild in tree"
    );
}

#[test]
fn test_scale_50_issues() {
    let h = SmokeHarness::new();
    for i in 1..=50 {
        h.run_ok(&["create", &format!("Scale test issue {i}")]);
    }
    assert_issue_count_flat(&h, "all", 50);

    let result = h.run_ok(&["list", "-s", "all"]);
    assert_stdout_contains(&result, "Scale test issue 1");
    assert_stdout_contains(&result, "Scale test issue 25");
    assert_stdout_contains(&result, "Scale test issue 50");
}

#[test]
fn test_scale_many_labels() {
    let h = SmokeHarness::new();
    h.run_ok(&["create", "Many labels issue"]);

    let labels: Vec<String> = (1..=20).map(|i| format!("label-{i}")).collect();
    for label in &labels {
        h.run_ok(&["issue", "label", "1", label]);
    }

    let result = h.run_ok(&["show", "1"]);
    for label in &labels {
        assert!(
            result.stdout_contains(label),
            "show missing label {:?}.\nGot: {}",
            label,
            result.stdout
        );
    }
}

#[test]
fn test_scale_deep_subissues_10() {
    let h = SmokeHarness::new();

    h.run_ok(&["create", "Depth level 1"]);
    for depth in 2..=10 {
        h.run_ok(&[
            "issue",
            "create",
            &format!("Depth level {depth}"),
            "--parent",
            &(depth - 1).to_string(),
        ]);
    }

    let result = h.run_ok(&["issue", "tree"]);
    for depth in 1..=10 {
        assert!(
            result.stdout_contains(&format!("Depth level {depth}")),
            "tree missing depth level {}.\nGot: {}",
            depth,
            result.stdout
        );
    }

    let lines: Vec<&str> = result
        .stdout
        .lines()
        .filter(|l| l.contains("Depth level"))
        .collect();
    assert_eq!(
        lines.len(),
        10,
        "expected 10 depth levels in tree, got {}",
        lines.len()
    );

    for i in 1..lines.len() {
        let prev_indent = lines[i - 1].len() - lines[i - 1].trim_start().len();
        let curr_indent = lines[i].len() - lines[i].trim_start().len();
        assert!(
            curr_indent > prev_indent,
            "depth level {} not more indented than level {}.\nline {}: {:?}\nline {}: {:?}",
            i + 1,
            i,
            i,
            lines[i - 1],
            i + 1,
            lines[i]
        );
    }
}

#[test]
fn test_scale_comments_20() {
    let h = SmokeHarness::new();

    let kinds = [
        "note",
        "plan",
        "decision",
        "observation",
        "blocker",
        "resolution",
        "result",
    ];
    for i in 1..=20 {
        let kind = kinds[(i - 1) % kinds.len()];
        h.run_ok(&["create", &format!("Issue for comment {i}")]);
        h.run_ok(&[
            "issue",
            "comment",
            &i.to_string(),
            &format!("Comment number {i} of twenty"),
            "--kind",
            kind,
        ]);
    }

    for i in 1..=20 {
        let result = h.run_ok(&["workflow", "trail", &i.to_string()]);
        assert!(
            result.stdout_contains(&format!("Comment number {i} of twenty")),
            "trail for issue {} missing comment.\nGot: {}",
            i,
            result.stdout
        );
    }
}
