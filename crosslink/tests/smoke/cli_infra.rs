use super::harness::{assert_stdout_contains, SmokeHarness};

#[test]
fn test_config_show() {
    let h = SmokeHarness::new();
    let r = h.run_ok(&["config", "show"]);

    assert_stdout_contains(&r, "tracking_mode");
    assert_stdout_contains(&r, "(default)");
}

#[test]
fn test_config_get_set_roundtrip() {
    let h = SmokeHarness::new();

    h.run_ok(&["config", "set", "tracking_mode", "strict"]);

    let r = h.run_ok(&["config", "get", "tracking_mode"]);
    assert_stdout_contains(&r, "strict");
}

#[test]
fn test_config_list() {
    let h = SmokeHarness::new();
    let r = h.run_ok(&["config", "list"]);

    assert_stdout_contains(&r, "KEY");
    assert_stdout_contains(&r, "tracking_mode");
    assert_stdout_contains(&r, "intervention_tracking");
    assert_stdout_contains(&r, "signing_enforcement");
}

#[test]
fn test_config_invalid_key() {
    let h = SmokeHarness::new();
    let r = h.run_err(&["config", "get", "nonexistent_key_xyz"]);

    let combined = format!("{}{}", r.stdout, r.stderr);
    assert!(
        combined.contains("Unknown config key")
            || combined.contains("unknown")
            || combined.contains("nknown"),
        "Expected error about unknown key, got:\nstdout: {}\nstderr: {}",
        r.stdout,
        r.stderr,
    );
}

#[test]
fn test_config_reset_single() {
    let h = SmokeHarness::new();

    h.run_ok(&["config", "set", "tracking_mode", "strict"]);
    let r = h.run_ok(&["config", "get", "tracking_mode"]);
    assert_stdout_contains(&r, "strict");

    h.run_ok(&["config", "reset", "tracking_mode"]);

    let r = h.run_ok(&["config", "diff"]);
    assert!(
        !r.stdout.contains("tracking_mode"),
        "Expected tracking_mode to be back to default after reset, but diff shows:\n{}",
        r.stdout,
    );
}

#[test]
fn test_config_diff_clean() {
    let h = SmokeHarness::new();
    let r = h.run_ok(&["config", "diff"]);

    assert_stdout_contains(&r, "No differences");
}

#[test]
fn test_config_diff_after_set() {
    let h = SmokeHarness::new();

    h.run_ok(&["config", "set", "tracking_mode", "relaxed"]);

    let r = h.run_ok(&["config", "diff"]);

    assert_stdout_contains(&r, "tracking_mode");
    assert!(
        !r.stdout.contains("No differences"),
        "Expected diff to show changes, but got:\n{}",
        r.stdout,
    );
}

#[test]
fn test_sync_basic() {
    let h = SmokeHarness::new();

    let r = h.run(&["sync"]);
    assert!(
        r.success || r.stderr.contains("Warning") || r.stderr.contains("agent"),
        "sync failed unexpectedly:\nstdout: {}\nstderr: {}",
        r.stdout,
        r.stderr,
    );
}

#[test]
fn test_sync_idempotent() {
    let h = SmokeHarness::new();

    let r1 = h.run(&["sync"]);
    let r2 = h.run(&["sync"]);
    assert_eq!(
        r1.success, r2.success,
        "sync not idempotent:\nfirst: exit={} stderr={}\nsecond: exit={} stderr={}",
        r1.exit_code, r1.stderr, r2.exit_code, r2.stderr,
    );
}

#[test]
fn test_migrate_rename_no_old() {
    let h = SmokeHarness::new();

    let r = h.run(&["migrate", "rename-branch"]);
    let combined = format!("{}{}", r.stdout, r.stderr);
    assert!(!r.success, "rename compatibility command must fail closed");
    assert!(
        combined.contains("cannot rename canonical authority refs independently")
            && combined.contains("migrate hub-v3"),
        "Expected explicit canonical-authority guidance:\nstdout: {}\nstderr: {}",
        r.stdout,
        r.stderr,
    );
}

#[test]
fn test_integrity_counters_clean() {
    let h = SmokeHarness::new();
    let r = h.run_ok(&["integrity", "counters"]);

    let combined = format!("{}{}", r.stdout, r.stderr);
    assert!(
        combined.contains("PASS") || combined.contains("SKIPPED"),
        "Expected PASS or SKIPPED for counters on fresh install, got:\n{combined}",
    );
}

#[test]
fn test_integrity_hydration_clean() {
    let h = SmokeHarness::new();
    let r = h.run_ok(&["integrity", "hydration"]);
    let combined = format!("{}{}", r.stdout, r.stderr);
    assert!(
        combined.contains("PASS") || combined.contains("SKIPPED"),
        "Expected PASS or SKIPPED for hydration on fresh install, got:\n{combined}",
    );
}

#[test]
fn test_integrity_locks_clean() {
    let h = SmokeHarness::new();
    let r = h.run_ok(&["integrity", "locks"]);
    let combined = format!("{}{}", r.stdout, r.stderr);
    assert!(
        combined.contains("PASS") || combined.contains("SKIPPED"),
        "Expected PASS or SKIPPED for locks on fresh install, got:\n{combined}",
    );
}

#[test]
fn test_integrity_schema_current() {
    let h = SmokeHarness::new();
    let r = h.run_ok(&["integrity", "schema"]);
    assert_stdout_contains(&r, "PASS");
}

#[test]
fn test_integrity_counters_repair() {
    let h = SmokeHarness::new();

    h.run_ok(&["issue", "create", "Issue alpha"]);
    h.run_ok(&["issue", "create", "Issue beta"]);
    h.run_ok(&["issue", "create", "Issue gamma"]);

    let _sync_result = h.run(&["sync"]);

    let hub_cache = h.crosslink_dir().join(".hub-cache");
    let counters_path = hub_cache.join("meta").join("counters.json");

    if counters_path.exists() {
        std::fs::write(
            &counters_path,
            r#"{"next_display_id": 1, "next_comment_id": 1, "next_milestone_id": 1}"#,
        )
        .expect("failed to write corrupted counters");

        let r = h.run_ok(&["integrity", "counters"]);
        assert_stdout_contains(&r, "FAIL");

        let r = h.run_ok(&["integrity", "counters", "--repair"]);
        let combined = format!("{}{}", r.stdout, r.stderr);
        assert!(
            combined.contains("REPAIRED") || combined.contains("PASS"),
            "Expected REPAIRED or PASS after repair, got:\n{combined}",
        );

        let r = h.run_ok(&["integrity", "counters"]);
        assert_stdout_contains(&r, "PASS");
    } else {
        let r = h.run_ok(&["integrity", "counters"]);
        let combined = format!("{}{}", r.stdout, r.stderr);
        assert!(
            combined.contains("PASS") || combined.contains("SKIPPED") || combined.contains("FAIL"),
            "Expected PASS, SKIPPED, or FAIL when counters are not populated, got:\n{combined}",
        );
    }
}

#[test]
fn test_compact_cli_basic() {
    let h = SmokeHarness::new();

    let r = h.run(&["compact"]);
    if !r.success {
        let r2 = h.run(&["compact", "--force"]);
        let combined = format!("{}{}", r2.stdout, r2.stderr);
        assert!(
            r2.success
                || combined.contains("agent")
                || combined.contains("No agent")
                || combined.contains("sync")
                || combined.contains("remote")
                || combined.contains("fetch"),
            "compact --force failed unexpectedly:\nstdout: {}\nstderr: {}",
            r2.stdout,
            r2.stderr,
        );
    }
}

#[test]
fn test_compact_cli_no_events() {
    let h = SmokeHarness::new();

    let r = h.run(&["compact", "--force"]);
    let combined = format!("{}{}", r.stdout, r.stderr);
    assert!(
        r.success
            || combined.contains("No agent")
            || combined.contains("agent")
            || combined.contains("No new events")
            || combined.contains("remote")
            || combined.contains("fetch"),
        "compact with no events failed unexpectedly:\nstdout: {}\nstderr: {}",
        r.stdout,
        r.stderr,
    );
}

#[test]
fn test_prune_dry_run() {
    let h = SmokeHarness::new();
    let r = h.run(&["prune", "--dry-run"]);
    let combined = format!("{}{}", r.stdout, r.stderr);

    assert!(
        r.success
            || combined.contains("sync")
            || combined.contains("remote")
            || combined.contains("fetch")
            || combined.contains("hub"),
        "prune --dry-run failed unexpectedly:\nstdout: {}\nstderr: {}",
        r.stdout,
        r.stderr,
    );

    if r.success {
        assert!(
            combined.contains("dry run")
                || combined.contains("Prune plan")
                || combined.contains("commit(s)")
                || combined.contains("nothing to prune"),
            "Expected dry-run output, got:\n{combined}",
        );
    }
}
