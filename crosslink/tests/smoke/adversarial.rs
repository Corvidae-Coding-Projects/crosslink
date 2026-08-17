use std::process::Command;
use std::thread;

use super::harness::{assert_issue_count, assert_stdout_contains, SmokeHarness};

#[test]
fn test_boundary_title_exact_512() {
    let h = SmokeHarness::new();
    let title = "a".repeat(512);
    let result = h.run_ok(&["create", &title]);
    assert!(result.stdout.contains("Created issue"));

    let show = h.run_ok(&["show", "1"]);
    assert!(show.stdout.contains(&title));
}

#[test]
fn test_boundary_title_over_513() {
    let h = SmokeHarness::new();
    let title = "a".repeat(513);
    let result = h.run(&["create", &title]);

    if result.success {
        let show = h.run_ok(&["show", "1"]);
        assert!(show.stdout.contains(&title[..50]));
    } else {
        assert!(
            result.stderr.contains("exceeds") || result.stderr.contains("maximum length"),
            "Expected error about length, got stderr: {}",
            result.stderr
        );
    }
}

#[test]
fn test_boundary_title_null_bytes() {
    let h = SmokeHarness::new();

    let output = Command::new(&h.crosslink_bin)
        .current_dir(h.temp_dir.path())
        .args(["create", "test\x00null"])
        .output();

    match output {
        Ok(o) => {
            if o.status.success() {
                let list = h.run_ok(&["list", "-s", "all"]);
                assert!(list.success);
            }
        }
        Err(e) => {
            assert!(
                e.kind() == std::io::ErrorKind::InvalidInput,
                "Expected InvalidInput error for null byte, got: {:?}",
                e.kind()
            );
        }
    }
}

#[test]
fn test_boundary_label_exact_128() {
    let h = SmokeHarness::new();
    h.run_ok(&["create", "Label boundary test"]);

    let label = "a".repeat(128);
    h.run_ok(&["issue", "label", "1", &label]);

    let show = h.run_ok(&["show", "1"]);
    assert!(show.stdout.contains(&label));
}

#[test]
fn test_boundary_label_over_129() {
    let h = SmokeHarness::new();
    h.run_ok(&["create", "Label boundary test"]);

    let label = "a".repeat(129);
    let result = h.run(&["issue", "label", "1", &label]);

    if result.success {
        let show = h.run_ok(&["show", "1"]);
        assert!(show.stdout.contains(&label[..50]));
    } else {
        assert!(
            result.stderr.contains("exceeds") || result.stderr.contains("maximum length"),
            "Expected error about label length, got stderr: {}",
            result.stderr
        );
    }
}

#[test]
fn test_boundary_desc_exact_64k() {
    let h = SmokeHarness::new();
    let desc = "b".repeat(65_536);
    let result = h.run_ok(&["create", "Desc boundary test", "-d", &desc]);
    assert!(result.stdout.contains("Created issue"));
}

#[test]
fn test_boundary_desc_over_64k() {
    let h = SmokeHarness::new();
    let desc = "b".repeat(65_537);
    let result = h.run(&["create", "Desc boundary test", "-d", &desc]);

    if result.success {
        let show = h.run_ok(&["show", "1"]);
        assert!(show.stdout.contains("Desc boundary test"));
    } else {
        assert!(
            result.stderr.contains("exceeds") || result.stderr.contains("maximum length"),
            "Expected error about description length, got stderr: {}",
            result.stderr
        );
    }
}

#[test]
fn test_boundary_empty_title() {
    let h = SmokeHarness::new();
    let _result = h.run(&["create", ""]);

    let list = h.run_ok(&["list", "-s", "all"]);
    assert!(list.success);
}

#[test]
fn test_boundary_whitespace_title() {
    let h = SmokeHarness::new();
    let _result = h.run(&["create", "   "]);

    let list = h.run_ok(&["list", "-s", "all"]);
    assert!(list.success);
}

#[test]
fn test_boundary_priority_invalid() {
    let h = SmokeHarness::new();
    let result = h.run_err(&["create", "Priority test", "-p", "hgih"]);
    assert!(
        result.stderr.contains("Invalid priority")
            || result.stderr.contains("invalid")
            || result.stderr.contains("hgih"),
        "Expected error about invalid priority, got stderr: {}",
        result.stderr
    );
}

#[test]
fn test_boundary_priority_case() {
    let h = SmokeHarness::new();

    let result = h.run_err(&["create", "Priority case test", "-p", "High"]);
    assert!(
        result.stderr.contains("Invalid priority")
            || result.stderr.contains("invalid")
            || result.stderr.contains("High"),
        "Expected error about invalid priority, got stderr: {}",
        result.stderr
    );
}

#[test]
fn test_boundary_status_invalid() {
    let h = SmokeHarness::new();
    h.run_ok(&["create", "Test issue"]);

    let result = h.run(&["list", "-s", "bogus"]);
    if result.success {
        assert!(
            !result.stdout.contains("Test issue"),
            "Invalid status should not match real issues"
        );

        assert_issue_count(&h, "all", 1);
    } else {
        assert!(
            result.stderr.contains("Invalid status")
                || result.stderr.contains("invalid")
                || result.stderr.contains("bogus"),
            "Expected error about invalid status, got stderr: {}",
            result.stderr
        );
    }
}

#[test]
fn test_inject_sql_title() {
    let h = SmokeHarness::new();
    let payload = "'; DROP TABLE issues; --";
    h.run_ok(&["create", payload]);

    let show = h.run_ok(&["show", "1"]);
    assert_stdout_contains(&show, payload);

    h.run_ok(&["create", "Normal issue after injection"]);

    assert_issue_count(&h, "all", 2);
}

#[test]
fn test_inject_sql_search() {
    let h = SmokeHarness::new();
    h.run_ok(&["create", "Findable issue"]);
    h.run_ok(&["create", "Another issue"]);

    let _result = h.run_ok(&["issue", "search", "% OR 1=1 --"]);

    assert_issue_count(&h, "all", 2);
}

#[test]
fn test_inject_sql_label() {
    let h = SmokeHarness::new();
    h.run_ok(&["create", "Label injection test"]);

    let payload = "'; DELETE FROM labels; --";
    h.run_ok(&["issue", "label", "1", payload]);

    let show = h.run_ok(&["show", "1"]);
    assert_stdout_contains(&show, payload);

    h.run_ok(&["issue", "label", "1", "safe-label"]);
    let show2 = h.run_ok(&["show", "1"]);
    assert_stdout_contains(&show2, "safe-label");
}

#[test]
fn test_inject_path_slug() {
    let h = SmokeHarness::new();

    let result = h.run(&["knowledge", "add", "../../../etc/passwd"]);

    if result.success {
        let bad_path = h.temp_dir.path().join("../../../etc/passwd.md");
        assert!(
            !bad_path.exists(),
            "Path traversal should not create files outside knowledge directory"
        );
    } else {
        assert!(
            result.stderr.contains("path separator")
                || result.stderr.contains("Invalid")
                || result.stderr.contains("..")
                || result.stderr.contains("outside"),
            "Expected path traversal rejection error, got stderr: {}",
            result.stderr
        );
    }
}

#[test]
fn test_inject_shell_title() {
    let h = SmokeHarness::new();
    let payload = "Issue with $(whoami) and `id` and $HOME";
    h.run_ok(&["create", payload]);

    let show = h.run_ok(&["show", "1"]);

    assert_stdout_contains(&show, "$(whoami)");
    assert_stdout_contains(&show, "`id`");
    assert_stdout_contains(&show, "$HOME");
}

#[test]
fn test_inject_shell_comment() {
    let h = SmokeHarness::new();
    h.run_ok(&["create", "Shell comment test"]);

    let payload = "Running $(rm -rf /) and `cat /etc/shadow` for $USER";
    h.run_ok(&["issue", "comment", "1", payload, "--kind", "observation"]);

    let show = h.run_ok(&["show", "1"]);
    assert_stdout_contains(&show, "$(rm -rf /)");
}

#[test]
fn test_unicode_emoji_title() {
    let h = SmokeHarness::new();

    let title =
        "Fix rendering of \u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466} emoji";
    h.run_ok(&["create", title]);

    let show = h.run_ok(&["show", "1"]);
    assert_stdout_contains(
        &show,
        "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}",
    );
}

#[test]
fn test_unicode_rtl_title() {
    let h = SmokeHarness::new();

    let title = "\u{0645}\u{0631}\u{062D}\u{0628}\u{0627} \u{0628}\u{0627}\u{0644}\u{0639}\u{0627}\u{0644}\u{0645}";
    h.run_ok(&["create", title]);

    let show = h.run_ok(&["show", "1"]);
    assert_stdout_contains(&show, title);
}

#[test]
fn test_unicode_mixed_scripts() {
    let h = SmokeHarness::new();

    let title = "Hello \u{041F}\u{0440}\u{0438}\u{0432}\u{0435}\u{0442} \u{4F60}\u{597D} \u{0928}\u{092E}\u{0938}\u{094D}\u{0924}\u{0947}";
    h.run_ok(&["create", title]);

    let show = h.run_ok(&["show", "1"]);
    assert_stdout_contains(&show, title);
}

#[cfg(unix)]
#[test]
fn test_corrupt_db_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let h = SmokeHarness::new();

    let perms = std::fs::Permissions::from_mode(0o444);
    std::fs::set_permissions(h.db_path(), perms).unwrap();

    let result = h.run(&["create", "Should fail"]);
    if result.success {
        assert!(
            result.stderr.contains("hydration failed") || result.stderr.contains("readonly"),
            "Create succeeded on read-only DB without hydration warning.\nstderr: {}",
            result.stderr
        );
    }

    let perms = std::fs::Permissions::from_mode(0o644);
    std::fs::set_permissions(h.db_path(), perms).unwrap();
}

#[test]
fn test_corrupt_missing_db() {
    let h = SmokeHarness::new();

    std::fs::remove_file(h.db_path()).unwrap();

    let result = h.run(&["list"]);

    if result.success {
        let list = h.run_ok(&["list", "-s", "all", "--json"]);
        let parsed: serde_json::Value = serde_json::from_str(&list.stdout).unwrap_or_else(|e| {
            panic!(
                "Failed to parse JSON after DB recreation: {}\nstdout: {}",
                e, list.stdout
            )
        });
        assert!(
            parsed.as_array().map(|a| a.is_empty()).unwrap_or(false),
            "Expected empty array after DB recreation, got: {}",
            list.stdout
        );
    }
}

#[test]
fn test_concurrent_creates_5() {
    let h = SmokeHarness::new();
    let bin = h.crosslink_bin.clone();
    let dir = h.temp_dir.path().to_path_buf();

    let handles: Vec<_> = (0..5)
        .map(|i| {
            let bin = bin.clone();
            let dir = dir.clone();
            thread::spawn(move || {
                let output = Command::new(&bin)
                    .current_dir(&dir)
                    .args(["create", &format!("Concurrent issue {i}")])
                    .output()
                    .expect("failed to execute crosslink");
                output.status.success()
            })
        })
        .collect();

    let mut successes = 0u32;
    for handle in handles {
        if handle.join().expect("thread panicked") {
            successes += 1;
        }
    }

    assert!(
        successes >= 1,
        "At least one concurrent create should succeed, got 0",
    );

    let result = h.run_ok(&["issue", "list", "-s", "all", "--json"]);
    let parsed: serde_json::Value =
        serde_json::from_str(&result.stdout).expect("failed to parse issue list JSON");
    let count = parsed.as_array().map(|a| a.len()).unwrap_or(0);
    assert!(
        count >= 1,
        "DB should have at least 1 issue after concurrent creates, got {count}",
    );
}
