use super::harness::{assert_stdout_contains, SmokeHarness};

fn harness_local_only() -> SmokeHarness {
    let mut h = SmokeHarness::new_bare();

    let out = std::process::Command::new("git")
        .current_dir(h.temp_dir.path())
        .args(["init", "-b", "main"])
        .output()
        .expect("git init failed");
    assert!(out.status.success(), "git init failed");

    for args in [
        vec!["config", "user.email", "smoke@test.local"],
        vec!["config", "user.name", "Smoke Test"],
    ] {
        let out = std::process::Command::new("git")
            .current_dir(h.temp_dir.path())
            .args(&args)
            .output()
            .expect("git config failed");
        assert!(out.status.success(), "git config {args:?} failed");
    }

    std::fs::write(h.temp_dir.path().join("README.md"), "# smoke\n")
        .expect("failed to write README.md");
    let _ = std::process::Command::new("git")
        .current_dir(h.temp_dir.path())
        .args(["add", "README.md"])
        .output();
    let out = std::process::Command::new("git")
        .current_dir(h.temp_dir.path())
        .args(["commit", "-m", "initial", "--no-gpg-sign"])
        .output()
        .expect("git commit failed");
    assert!(out.status.success(), "initial git commit failed");

    h.run_ok(&["init", "--defaults", "--skip-cpitd", "--skip-signing"]);
    h.ensure_ready();

    h
}

fn extract_issue_id(stdout: &str) -> String {
    for line in stdout.lines() {
        if let Some(pos) = line.find('L') {
            let id_str: String = line[pos..]
                .chars()
                .take_while(|c| *c == 'L' || c.is_ascii_digit())
                .collect();
            if id_str.len() > 1 && id_str.starts_with('L') {
                return id_str;
            }
        }

        if let Some(pos) = line.find('#') {
            let id_str: String = line[pos + 1..]
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            if !id_str.is_empty() {
                return id_str;
            }
        }
    }
    panic!("Could not extract issue ID from output:\n{stdout}");
}

#[test]
fn test_cpitd_status_no_scan() {
    let h = SmokeHarness::new();
    let result = h.run_ok(&["cpitd", "status"]);

    assert!(
        result.stdout_contains("No open cpitd clone issues")
            || result.stdout_contains("0 open clone issue"),
        "expected 'no open cpitd clone issues' message, got stdout:\n{}",
        result.stdout,
    );
}

#[test]
fn test_cpitd_clear_idempotent() {
    let h = SmokeHarness::new();

    let result = h.run_ok(&["cpitd", "clear"]);
    assert!(
        result.stdout_contains("No open cpitd clone issues to close")
            || result.stdout_contains("Closed 0"),
        "expected idempotent clear message, got stdout:\n{}",
        result.stdout,
    );

    h.run_ok(&["cpitd", "clear"]);
}

#[test]
fn test_cpitd_scan_dry_run() {
    let h = SmokeHarness::new();

    let src_dir = h.temp_dir.path().join("src");
    std::fs::create_dir_all(&src_dir).expect("failed to create src dir");
    std::fs::write(
        src_dir.join("example.rs"),
        r#"
fn hello() {
    println!("Hello, world!");
}

fn goodbye() {
    println!("Goodbye, world!");
}
"#,
    )
    .expect("failed to write example.rs");

    let result = h.run(&["cpitd", "scan", "src", "--dry-run"]);

    if result.success {
        let status = h.run_ok(&["cpitd", "status"]);
        assert!(
            status.stdout_contains("No open cpitd clone issues"),
            "dry-run should not create issues, but cpitd status shows:\n{}",
            status.stdout,
        );
    } else {
        let combined = format!("{}{}", result.stdout, result.stderr);
        assert!(
            combined.contains("cpitd")
                && (combined.contains("not installed")
                    || combined.contains("not found")
                    || combined.contains("pip install")),
            "expected cpitd installation hint on failure, got:\nstdout: {}\nstderr: {}",
            result.stdout,
            result.stderr,
        );
    }
}

#[test]
fn test_workflow_diff_clean() {
    let h = SmokeHarness::new();

    let result = h.run_ok(&["workflow", "diff"]);

    let combined = format!("{}{}", result.stdout, result.stderr);
    assert!(
        combined.contains("Tracking Mode")
            || combined.contains("Rules")
            || combined.contains("Hooks")
            || combined.contains("matches default"),
        "expected workflow diff section headers, got:\nstdout: {}\nstderr: {}",
        result.stdout,
        result.stderr,
    );
}

#[test]
fn test_workflow_trail_basic() {
    let h = harness_local_only();

    let create_result = h.run_ok(&["issue", "create", "Trail test issue"]);
    let issue_id = extract_issue_id(&create_result.stdout);

    h.run_ok(&[
        "issue",
        "comment",
        &issue_id,
        "Planning the approach",
        "--kind",
        "plan",
    ]);
    h.run_ok(&[
        "issue",
        "comment",
        &issue_id,
        "Decided to use method A",
        "--kind",
        "decision",
    ]);
    h.run_ok(&[
        "issue",
        "comment",
        &issue_id,
        "Tests all pass",
        "--kind",
        "result",
    ]);

    let trail = h.run_ok(&["workflow", "trail", &issue_id]);
    assert!(
        trail.stdout_contains("Planning the approach"),
        "trail should contain plan comment, got:\n{}",
        trail.stdout,
    );
    assert!(
        trail.stdout_contains("Decided to use method A"),
        "trail should contain decision comment, got:\n{}",
        trail.stdout,
    );
    assert!(
        trail.stdout_contains("Tests all pass"),
        "trail should contain result comment, got:\n{}",
        trail.stdout,
    );
}

#[test]
fn test_workflow_trail_kind_filter() {
    let h = harness_local_only();

    let create_result = h.run_ok(&["issue", "create", "Filter test issue"]);
    let issue_id = extract_issue_id(&create_result.stdout);

    h.run_ok(&[
        "issue",
        "comment",
        &issue_id,
        "Plan: do the thing",
        "--kind",
        "plan",
    ]);
    h.run_ok(&[
        "issue",
        "comment",
        &issue_id,
        "Note: something happened",
        "--kind",
        "note",
    ]);
    h.run_ok(&[
        "issue",
        "comment",
        &issue_id,
        "Decision: chose X",
        "--kind",
        "decision",
    ]);

    let trail = h.run_ok(&["workflow", "trail", &issue_id, "--kind", "plan"]);
    assert!(
        trail.stdout_contains("Plan: do the thing"),
        "trail --kind plan should include plan comment, got:\n{}",
        trail.stdout,
    );
    assert!(
        !trail.stdout_contains("Note: something happened"),
        "trail --kind plan should exclude note comment, got:\n{}",
        trail.stdout,
    );
    assert!(
        !trail.stdout_contains("Decision: chose X"),
        "trail --kind plan should exclude decision comment, got:\n{}",
        trail.stdout,
    );
}

#[test]
fn test_workflow_trail_nonexistent() {
    let h = SmokeHarness::new();

    let result = h.run_err(&["workflow", "trail", "99999"]);
    let combined = format!("{}{}", result.stdout, result.stderr);
    assert!(
        combined.contains("not found")
            || combined.contains("No issue")
            || combined.contains("99999"),
        "expected error about nonexistent issue, got:\nstdout: {}\nstderr: {}",
        result.stdout,
        result.stderr,
    );
}

#[test]
fn test_workflow_trail_empty() {
    let h = harness_local_only();

    let create_result = h.run_ok(&["issue", "create", "Empty trail issue"]);
    let issue_id = extract_issue_id(&create_result.stdout);

    let trail = h.run_ok(&["workflow", "trail", &issue_id]);
    assert!(
        trail.stdout_contains("No comments found"),
        "trail for issue with no comments should say 'No comments found', got:\n{}",
        trail.stdout,
    );
}

#[test]
fn test_context_measure_basic() {
    let h = SmokeHarness::new();
    let result = h.run_ok(&["context", "measure"]);

    assert_stdout_contains(&result, "Context injection measurement");
    assert!(
        result.stdout_contains("Rule files") || result.stdout_contains("rules"),
        "context measure should mention rule files, got:\n{}",
        result.stdout,
    );
    assert!(
        result.stdout_contains("BYTES")
            || result.stdout_contains("bytes")
            || result.stdout_contains("tokens"),
        "context measure should mention sizes, got:\n{}",
        result.stdout,
    );
}

#[test]
fn test_context_check_clean() {
    let h = SmokeHarness::new();

    let result = h.run_ok(&["context", "check"]);
    assert!(
        result.stdout_contains("All checks passed") || result.stdout_contains("OK"),
        "context check on fresh init should pass, got:\n{}",
        result.stdout,
    );
}

#[test]
fn test_style_show_none() {
    let h = SmokeHarness::new();
    let result = h.run_ok(&["style", "show"]);

    assert!(
        result.stdout_contains("No house style configured")
            || result.stdout_contains("not configured"),
        "style show with no config should be informative, got:\n{}",
        result.stdout,
    );
}

#[test]
fn test_style_unset_idempotent() {
    let h = SmokeHarness::new();

    let result = h.run_ok(&["style", "unset"]);
    assert!(
        result.stdout_contains("No house style configured")
            || result.stdout_contains("Nothing to do")
            || result.stdout_contains("removed"),
        "style unset with no config should be informative, got:\n{}",
        result.stdout,
    );

    h.run_ok(&["style", "unset"]);
}
