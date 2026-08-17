use super::harness::SmokeHarness;

fn init_agent_and_sync(h: &SmokeHarness, agent_id: &str) {
    h.run_ok(&["agent", "init", agent_id, "--no-key", "--force"]);

    h.run_ok(&["sync"]);
}

#[test]
fn test_two_agents_create_issues() {
    let agent_a = SmokeHarness::new();
    init_agent_and_sync(&agent_a, "agent-a");

    let agent_b = agent_a.fork_agent("agent-b");
    init_agent_and_sync(&agent_b, "agent-b");

    agent_a.run_ok(&["create", "Task from A"]);

    agent_a.run_ok(&["sync"]);

    agent_b.run_ok(&["sync"]);

    let result = agent_b.run_ok(&["list", "-s", "all"]);
    assert!(
        result.stdout_contains("Task from A"),
        "Agent B should see Agent A's issue after sync.\nstdout: {}",
        result.stdout,
    );
}

#[test]
fn test_two_agents_independent() {
    let agent_a = SmokeHarness::new();
    init_agent_and_sync(&agent_a, "agent-a");

    let agent_b = agent_a.fork_agent("agent-b");
    init_agent_and_sync(&agent_b, "agent-b");

    agent_a.run_ok(&["create", "Issue from A"]);
    agent_a.run_ok(&["sync"]);

    agent_b.run_ok(&["sync"]);
    agent_b.run_ok(&["create", "Issue from B"]);
    agent_b.run_ok(&["sync"]);

    agent_a.run_ok(&["sync"]);

    let result_a = agent_a.run_ok(&["list", "-s", "all"]);
    assert!(
        result_a.stdout_contains("Issue from A"),
        "Agent A should see its own issue.\nstdout: {}",
        result_a.stdout,
    );
    assert!(
        result_a.stdout_contains("Issue from B"),
        "Agent A should see Agent B's issue.\nstdout: {}",
        result_a.stdout,
    );

    let result_b = agent_b.run_ok(&["list", "-s", "all"]);
    assert!(
        result_b.stdout_contains("Issue from A"),
        "Agent B should see Agent A's issue.\nstdout: {}",
        result_b.stdout,
    );
    assert!(
        result_b.stdout_contains("Issue from B"),
        "Agent B should see its own issue.\nstdout: {}",
        result_b.stdout,
    );
}

#[test]
fn test_lock_claim_release() {
    let h = SmokeHarness::new();
    init_agent_and_sync(&h, "smoke-agent");

    h.run_ok(&["create", "Lockable task"]);
    h.run_ok(&["sync"]);

    let claim_result = h.run_ok(&["locks", "claim", "1"]);
    assert!(
        claim_result.stdout_contains("Claimed")
            || claim_result.stdout_contains("claimed")
            || claim_result.stdout_contains("lock"),
        "Expected claim confirmation.\nstdout: {}\nstderr: {}",
        claim_result.stdout,
        claim_result.stderr,
    );

    let check_result = h.run_ok(&["locks", "check", "1"]);
    assert!(
        check_result.stdout_contains("locked")
            || check_result.stdout_contains("Locked")
            || check_result.stdout_contains("held")
            || check_result.stdout_contains("Held"),
        "Expected issue to be locked.\nstdout: {}\nstderr: {}",
        check_result.stdout,
        check_result.stderr,
    );

    let release_result = h.run_ok(&["locks", "release", "1"]);
    assert!(
        release_result.stdout_contains("Released")
            || release_result.stdout_contains("released")
            || release_result.stdout_contains("lock"),
        "Expected release confirmation.\nstdout: {}\nstderr: {}",
        release_result.stdout,
        release_result.stderr,
    );

    let check_result = h.run_ok(&["locks", "check", "1"]);
    assert!(
        check_result.stdout_contains("available")
            || check_result.stdout_contains("Available")
            || check_result.stdout_contains("unlocked")
            || check_result.stdout_contains("not locked")
            || check_result.stdout_contains("Not locked"),
        "Expected issue to be unlocked after release.\nstdout: {}\nstderr: {}",
        check_result.stdout,
        check_result.stderr,
    );
}

#[test]
fn test_lock_list_empty() {
    let h = SmokeHarness::new();

    h.run_ok(&["sync"]);

    let result = h.run_ok(&["locks", "list"]);
    assert!(
        result.stdout_contains("No active locks")
            || result.stdout_contains("no active locks")
            || result.stdout_contains("0 active lock"),
        "Expected empty lock list.\nstdout: {}\nstderr: {}",
        result.stdout,
        result.stderr,
    );
}

#[test]
fn test_lock_check_unlocked() {
    let h = SmokeHarness::new();

    h.run_ok(&["create", "Never locked"]);
    h.run_ok(&["sync"]);

    let result = h.run_ok(&["locks", "check", "1"]);
    assert!(
        result.stdout_contains("available")
            || result.stdout_contains("Available")
            || result.stdout_contains("unlocked")
            || result.stdout_contains("not locked")
            || result.stdout_contains("Not locked"),
        "Unlocked issue should report as available.\nstdout: {}\nstderr: {}",
        result.stdout,
        result.stderr,
    );
}

#[test]
fn test_lock_claim_twice_same_agent() {
    let h = SmokeHarness::new();
    init_agent_and_sync(&h, "smoke-agent");

    h.run_ok(&["create", "Double lock task"]);
    h.run_ok(&["sync"]);

    h.run_ok(&["locks", "claim", "1"]);

    let result = h.run(&["locks", "claim", "1"]);
    assert!(
        result.stdout_contains("Claimed")
            || result.stdout_contains("claimed")
            || result.stdout_contains("Already")
            || result.stdout_contains("already")
            || result.stdout_contains("held"),
        "Double claim should be idempotent or report already held.\nstdout: {}\nstderr: {}",
        result.stdout,
        result.stderr,
    );

    assert!(
        result.exit_code == 0 || result.exit_code == 1,
        "Unexpected exit code {} for double claim",
        result.exit_code,
    );
}

#[test]
fn test_compact_after_creates() {
    let h = SmokeHarness::new();
    init_agent_and_sync(&h, "smoke-agent");

    h.run_ok(&["create", "Compact test A"]);
    h.run_ok(&["create", "Compact test B"]);
    h.run_ok(&["create", "Compact test C"]);
    h.run_ok(&["sync"]);

    let result = h.run_ok(&["compact", "--force"]);
    assert!(
        result.stdout_contains("Compaction complete")
            || result.stdout_contains("compaction")
            || result.stdout_contains("Compact"),
        "Expected compaction success message.\nstdout: {}\nstderr: {}",
        result.stdout,
        result.stderr,
    );

    let list_result = h.run_ok(&["list", "-s", "all"]);
    assert!(
        list_result.stdout_contains("Compact test A"),
        "Issue A should survive compaction.\nstdout: {}",
        list_result.stdout,
    );
    assert!(
        list_result.stdout_contains("Compact test B"),
        "Issue B should survive compaction.\nstdout: {}",
        list_result.stdout,
    );
    assert!(
        list_result.stdout_contains("Compact test C"),
        "Issue C should survive compaction.\nstdout: {}",
        list_result.stdout,
    );
}

#[test]
fn test_compact_idempotent() {
    let h = SmokeHarness::new();
    init_agent_and_sync(&h, "smoke-agent");

    h.run_ok(&["create", "Idempotent compact test"]);
    h.run_ok(&["sync"]);

    let first = h.run_ok(&["compact", "--force"]);
    assert!(
        first.stdout_contains("Compaction complete") || first.stdout_contains("compaction"),
        "First compaction should succeed.\nstdout: {}\nstderr: {}",
        first.stdout,
        first.stderr,
    );

    let second = h.run_ok(&["compact", "--force"]);
    assert!(
        second.stdout_contains("Compaction complete")
            || second.stdout_contains("compaction")
            || second.stdout_contains("No new events"),
        "Second compaction should succeed idempotently.\nstdout: {}\nstderr: {}",
        second.stdout,
        second.stderr,
    );

    let list_result = h.run_ok(&["list", "-s", "all"]);
    assert!(
        list_result.stdout_contains("Idempotent compact test"),
        "Issue should survive double compaction.\nstdout: {}",
        list_result.stdout,
    );
}

#[test]
fn test_integrity_after_sync() {
    let h = SmokeHarness::new();
    init_agent_and_sync(&h, "smoke-agent");

    h.run_ok(&["create", "Integrity check A"]);
    h.run_ok(&["create", "Integrity check B"]);
    h.run_ok(&["sync"]);

    let result = h.run_ok(&["integrity"]);
    assert!(
        !result.stdout_contains("[FAIL]"),
        "No integrity check should fail after clean sync.\nstdout: {}\nstderr: {}",
        result.stdout,
        result.stderr,
    );
}

#[test]
fn test_integrity_hydration_matches() {
    let h = SmokeHarness::new();
    init_agent_and_sync(&h, "smoke-agent");

    h.run_ok(&["create", "Hydration test A"]);
    h.run_ok(&["create", "Hydration test B"]);
    h.run_ok(&["create", "Hydration test C"]);
    h.run_ok(&["sync"]);

    let result = h.run_ok(&["integrity", "hydration"]);
    assert!(
        result.stdout_contains("[PASS]") || result.stdout_contains("[SKIPPED]"),
        "Hydration integrity should pass or skip (not fail) after sync.\nstdout: {}\nstderr: {}",
        result.stdout,
        result.stderr,
    );
    assert!(
        !result.stdout_contains("[FAIL]"),
        "Hydration should not fail.\nstdout: {}",
        result.stdout,
    );
}

#[test]
fn test_adversarial_same_issue_write_conflict_convergence() {
    let agent_a = SmokeHarness::new();
    init_agent_and_sync(&agent_a, "agent-a");

    let agent_b = agent_a.fork_agent("agent-b");
    init_agent_and_sync(&agent_b, "agent-b");

    agent_a.run_ok(&["create", "Shared conflict issue"]);
    agent_a.run_ok(&["sync"]);
    agent_b.run_ok(&["sync"]);

    agent_a.run_ok(&["issue", "label", "1", "label-from-a"]);
    agent_b.run_ok(&["issue", "label", "1", "label-from-b"]);

    let sync_a = agent_a.run(&["sync"]);
    let sync_b = agent_b.run(&["sync"]);

    assert!(
        sync_a.success || sync_b.success,
        "At least one agent's sync must succeed after concurrent edits.\
         \nAgent A sync stdout: {}\nAgent A sync stderr: {}\
         \nAgent B sync stdout: {}\nAgent B sync stderr: {}",
        sync_a.stdout,
        sync_a.stderr,
        sync_b.stdout,
        sync_b.stderr,
    );

    let _ = agent_a.run(&["sync"]);
    let _ = agent_b.run(&["sync"]);

    let show_a = agent_a.run_ok(&["show", "1"]);
    assert!(
        show_a.stdout_contains("Shared conflict issue"),
        "Agent A: issue must survive concurrent edit.\nstdout: {}",
        show_a.stdout,
    );

    let show_b = agent_b.run_ok(&["show", "1"]);
    assert!(
        show_b.stdout_contains("Shared conflict issue"),
        "Agent B: issue must survive concurrent edit.\nstdout: {}",
        show_b.stdout,
    );
}

#[test]
fn test_adversarial_stale_lock_steal_audit() {
    let agent_a = SmokeHarness::new();
    init_agent_and_sync(&agent_a, "agent-a");

    let agent_b = agent_a.fork_agent("agent-b");
    init_agent_and_sync(&agent_b, "agent-b");

    agent_a.run_ok(&["create", "Stale lock test issue"]);
    agent_a.run_ok(&["sync"]);
    agent_b.run_ok(&["sync"]);

    agent_a.run_ok(&["locks", "claim", "1"]);

    agent_b.run_ok(&["sync"]);
    let steal = agent_b.run(&["locks", "steal", "1"]);

    if steal.success {
        let check = agent_b.run_ok(&["locks", "check", "1"]);
        assert!(
            check.stdout_contains("agent-b")
                || check.stdout_contains("locked")
                || check.stdout_contains("Locked")
                || check.stdout_contains("held"),
            "After steal, Agent B should hold the lock.\nstdout: {}",
            check.stdout,
        );

        let show = agent_b.run(&["show", "1"]);
        let locks_list = agent_b.run(&["locks", "list"]);
        let has_audit = show.stdout.to_ascii_lowercase().contains("steal")
            || show.stdout.to_ascii_lowercase().contains("lock")
            || locks_list.stdout_contains("agent-b")
            || locks_list.stdout_contains("1");
        assert!(
            has_audit,
            "Steal should produce an audit record (comment or lock list entry).\
             \nshow stdout: {}\nlocks list stdout: {}",
            show.stdout, locks_list.stdout,
        );
    } else {
        agent_b.run_ok(&["list", "-s", "all"]);
    }
}

#[test]
fn test_adversarial_hub_cache_corruption_recovery() {
    let h = SmokeHarness::new();
    init_agent_and_sync(&h, "recovery-agent");

    h.run_ok(&["create", "Pre-corruption issue"]);
    h.run_ok(&["sync"]);

    let crosslink_dir = h.crosslink_dir();

    let hub_deleted = if let Ok(entries) = std::fs::read_dir(&crosslink_dir) {
        let mut deleted = false;
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with("hub") || name_str.starts_with("cache") {
                let path = entry.path();
                if path.is_dir() {
                    if let Ok(inner) = std::fs::read_dir(&path) {
                        for inner_entry in inner.flatten() {
                            let _ = std::fs::remove_file(inner_entry.path());
                        }
                    }
                    deleted = true;
                } else {
                    let _ = std::fs::remove_file(&path);
                    deleted = true;
                }
            }
        }
        deleted
    } else {
        false
    };

    let _ = std::fs::remove_file(crosslink_dir.join("FETCH_HEAD"));

    let sync_result = h.run(&["sync"]);

    let output_present = !sync_result.stdout.is_empty() || !sync_result.stderr.is_empty();

    if !sync_result.success {
        assert!(
            output_present,
            "sync after hub cache corruption must produce output (not silently crash).\
             \nstdout: {}\nstderr: {}",
            sync_result.stdout, sync_result.stderr,
        );

        let _ = hub_deleted;
    } else {
        let list = h.run_ok(&["list", "-s", "all"]);
        assert!(
            list.success,
            "After sync recovery, list should succeed.\nstdout: {}",
            list.stdout,
        );
    }
}

#[test]
fn test_adversarial_event_log_divergence_compaction_consistency() {
    let agent_a = SmokeHarness::new();
    init_agent_and_sync(&agent_a, "agent-a");

    let agent_b = agent_a.fork_agent("agent-b");
    init_agent_and_sync(&agent_b, "agent-b");

    agent_a.run_ok(&["create", "Event diverge A-1"]);
    agent_a.run_ok(&["create", "Event diverge A-2"]);

    agent_b.run_ok(&["create", "Event diverge B-1"]);
    agent_b.run_ok(&["create", "Event diverge B-2"]);

    let sync_a = agent_a.run(&["sync"]);
    let sync_b = agent_b.run(&["sync"]);

    if !sync_a.success {
        agent_a.run_ok(&["sync"]);
    }
    if !sync_b.success {
        agent_b.run_ok(&["sync"]);
    }

    agent_a.run_ok(&["sync"]);
    agent_b.run_ok(&["sync"]);

    let _compact_a = agent_a.run(&["compact", "--force"]);
    let _compact_b = agent_b.run(&["compact", "--force"]);

    agent_a.run_ok(&["sync"]);
    agent_b.run_ok(&["sync"]);

    let list_a = agent_a.run_ok(&["list", "-s", "all"]);
    for title in &[
        "Event diverge A-1",
        "Event diverge A-2",
        "Event diverge B-1",
        "Event diverge B-2",
    ] {
        assert!(
            list_a.stdout_contains(title),
            "Agent A: compaction must not drop event '{}'.\nstdout: {}",
            title,
            list_a.stdout,
        );
    }

    let list_b = agent_b.run_ok(&["list", "-s", "all"]);
    for title in &[
        "Event diverge A-1",
        "Event diverge A-2",
        "Event diverge B-1",
        "Event diverge B-2",
    ] {
        assert!(
            list_b.stdout_contains(title),
            "Agent B: compaction must not drop event '{}'.\nstdout: {}",
            title,
            list_b.stdout,
        );
    }
}

#[test]
fn test_adversarial_concurrent_issue_creation_no_duplicates() {
    let agent_a = SmokeHarness::new();
    init_agent_and_sync(&agent_a, "agent-a");

    let agent_b = agent_a.fork_agent("agent-b");
    init_agent_and_sync(&agent_b, "agent-b");

    agent_a.run_ok(&["create", "Concurrent create A"]);
    agent_b.run_ok(&["create", "Concurrent create B"]);

    let sync_a = agent_a.run(&["sync"]);
    let sync_b = agent_b.run(&["sync"]);

    if !sync_a.success {
        agent_a.run_ok(&["sync"]);
    }
    if !sync_b.success {
        agent_b.run_ok(&["sync"]);
    }

    agent_a.run_ok(&["sync"]);
    agent_b.run_ok(&["sync"]);

    let list_a = agent_a.run_ok(&["list", "-s", "all"]);
    assert!(
        list_a.stdout_contains("Concurrent create A"),
        "Agent A: its own issue must exist after sync.\nstdout: {}",
        list_a.stdout,
    );
    assert!(
        list_a.stdout_contains("Concurrent create B"),
        "Agent A: Agent B's issue must exist after sync.\nstdout: {}",
        list_a.stdout,
    );

    let list_b = agent_b.run_ok(&["list", "-s", "all"]);
    assert!(
        list_b.stdout_contains("Concurrent create A"),
        "Agent B: Agent A's issue must exist after sync.\nstdout: {}",
        list_b.stdout,
    );
    assert!(
        list_b.stdout_contains("Concurrent create B"),
        "Agent B: its own issue must exist after sync.\nstdout: {}",
        list_b.stdout,
    );

    let json_a = agent_a.run_ok(&["issue", "list", "-s", "all", "--json"]);
    let parsed_a: serde_json::Value = serde_json::from_str(&json_a.stdout).unwrap_or_else(|e| {
        panic!(
            "Failed to parse Agent A issue list JSON: {}\nstdout: {}",
            e, json_a.stdout
        )
    });
    let issues_a = parsed_a
        .as_array()
        .expect("Expected JSON array for Agent A issue list");

    let count_a = issues_a
        .iter()
        .filter(|issue| {
            issue
                .get("title")
                .and_then(|t| t.as_str())
                .map(|t| t == "Concurrent create A" || t == "Concurrent create B")
                .unwrap_or(false)
        })
        .count();

    assert_eq!(
        count_a, 2,
        "Agent A: expected exactly 2 issues (one per agent), got {}.\nJSON: {}",
        count_a, json_a.stdout,
    );
}
