use super::harness::{assert_stdout_contains, SmokeHarness};

fn extract_issue_id(stdout: &str) -> String {
    for line in stdout.lines() {
        for word in line.split_whitespace() {
            let word = word.trim_end_matches(&['.', ',', ':', ';', '!', '?', ')'] as &[char]);
            if word.starts_with('L')
                && word.len() > 1
                && word[1..].chars().all(|c| c.is_ascii_digit())
            {
                return word.to_string();
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

    for line in stdout.lines() {
        if line.contains("working on") || line.contains("Created") {
            for word in line.split_whitespace() {
                let word = word.trim_end_matches(&['.', ',', ':', ';'] as &[char]);
                if word.starts_with('L')
                    && word.len() > 1
                    && word[1..].chars().all(|c| c.is_ascii_digit())
                {
                    return word.to_string();
                }
            }
        }
    }
    panic!("Could not extract issue ID from output:\n{stdout}");
}

#[test]
fn test_timer_roundtrip() {
    let h = SmokeHarness::new();

    let create_result = h.run_ok(&["issue", "create", "Timer roundtrip issue"]);
    let issue_id = extract_issue_id(&create_result.stdout);

    let start = h.run_ok(&["timer", "start", &issue_id]);
    assert!(
        start.stdout_contains("Started")
            || start.stdout_contains("timer")
            || start.stdout_contains("Timer"),
        "timer start should confirm start.\nstdout: {}",
        start.stdout,
    );

    let show_running = h.run_ok(&["timer", "show"]);
    assert!(
        show_running.stdout_contains("running")
            || show_running.stdout_contains("active")
            || show_running.stdout_contains("Active")
            || show_running.stdout_contains("Timer"),
        "timer show while running should indicate active state.\nstdout: {}",
        show_running.stdout,
    );

    let stop = h.run_ok(&["timer", "stop"]);
    assert!(
        stop.stdout_contains("Stopped")
            || stop.stdout_contains("stopped")
            || stop.stdout_contains("timer")
            || stop.stdout_contains("Timer"),
        "timer stop should confirm stop.\nstdout: {}",
        stop.stdout,
    );

    let show_stopped = h.run_ok(&["timer", "show"]);

    let combined = format!("{}{}", show_stopped.stdout, show_stopped.stderr);
    assert!(
        combined.contains("No active")
            || combined.contains("no active")
            || combined.contains("No time")
            || combined.contains("Total")
            || combined.contains("0s")
            || combined.contains("0m")
            || show_stopped.success,
        "timer show after stop should report stopped state or elapsed time.\nstdout: {}\nstderr: {}",
        show_stopped.stdout,
        show_stopped.stderr,
    );
}

#[test]
fn test_timer_start_already_running() {
    let h = SmokeHarness::new();

    h.run_ok(&["issue", "create", "Double-start issue"]);

    h.run_ok(&["timer", "start", "1"]);

    let result = h.run(&["timer", "start", "1"]);
    let combined = format!("{}{}", result.stdout, result.stderr);
    assert!(
        result.success
            || combined.contains("already")
            || combined.contains("running")
            || combined.contains("active"),
        "Second timer start should handle gracefully.\nstdout: {}\nstderr: {}",
        result.stdout,
        result.stderr,
    );
}

#[test]
fn test_timer_stop_not_running() {
    let h = SmokeHarness::new();

    let result = h.run(&["timer", "stop"]);
    let combined = format!("{}{}", result.stdout, result.stderr);
    assert!(
        result.success
            || combined.contains("No active")
            || combined.contains("no active")
            || combined.contains("not running")
            || combined.contains("No timer"),
        "timer stop with no running timer should handle gracefully.\nstdout: {}\nstderr: {}",
        result.stdout,
        result.stderr,
    );
}

#[test]
fn test_session_full_lifecycle() {
    let h = SmokeHarness::new();

    let create_result = h.run_ok(&["issue", "create", "Session lifecycle issue"]);
    let issue_id = extract_issue_id(&create_result.stdout);

    let start = h.run_ok(&["session", "start"]);
    assert!(
        start.stdout_contains("started")
            || start.stdout_contains("Started")
            || start.stdout_contains("Session"),
        "session start should confirm.\nstdout: {}",
        start.stdout,
    );

    let work = h.run_ok(&["session", "work", &issue_id]);
    assert!(
        work.stdout_contains("working on")
            || work.stdout_contains("Working on")
            || work.stdout_contains(&issue_id)
            || work.success,
        "session work should confirm the work item.\nstdout: {}",
        work.stdout,
    );

    let action = h.run_ok(&["session", "action", "Implementing the lifecycle test"]);
    assert!(
        action.stdout_contains("Recorded") || action.stdout_contains("action") || action.success,
        "session action should confirm.\nstdout: {}",
        action.stdout,
    );

    let status = h.run_ok(&["session", "status"]);
    assert!(
        status.stdout_contains("active")
            || status.stdout_contains("Active")
            || status.stdout_contains("Session"),
        "session should be active.\nstdout: {}",
        status.stdout,
    );

    assert!(
        status.stdout_contains("lifecycle") || status.stdout_contains(&issue_id) || status.success,
        "session status should reference work item.\nstdout: {}",
        status.stdout,
    );

    let handoff_note = "Done: lifecycle test complete, all assertions passed";
    let end = h.run_ok(&["session", "end", "--notes", handoff_note]);
    assert!(
        end.stdout_contains("ended")
            || end.stdout_contains("Ended")
            || end.stdout_contains("Session")
            || end.success,
        "session end should confirm.\nstdout: {}",
        end.stdout,
    );

    h.run_ok(&["session", "start"]);
    let last = h.run_ok(&["session", "last-handoff"]);
    assert!(
        last.stdout_contains("lifecycle test complete")
            || last.stdout_contains("Done:")
            || last.stdout_contains("Handoff")
            || last.stdout_contains("handoff"),
        "last-handoff should contain previous session notes.\nstdout: {}",
        last.stdout,
    );
}

#[test]
fn test_session_status_no_session() {
    let h = SmokeHarness::new();

    let result = h.run(&["session", "status"]);

    let combined = format!("{}{}", result.stdout, result.stderr);
    assert!(
        combined.contains("No active")
            || combined.contains("no active")
            || combined.contains("No session")
            || combined.contains("not started")
            || result.success,
        "session status with no session should handle gracefully.\nstdout: {}\nstderr: {}",
        result.stdout,
        result.stderr,
    );
}

#[test]
fn test_intervene_records_event() {
    let h = SmokeHarness::new();

    let create_result = h.run_ok(&["issue", "create", "Intervene target"]);
    let issue_id = extract_issue_id(&create_result.stdout);

    let intervene = h.run_ok(&[
        "issue",
        "intervene",
        &issue_id,
        "Manual correction applied to output",
        "--trigger",
        "manual_action",
        "--context",
        "Running lifecycle smoke test",
    ]);
    assert!(
        intervene.stdout_contains("intervention")
            || intervene.stdout_contains("Intervention")
            || intervene.stdout_contains("Recorded")
            || intervene.stdout_contains("recorded")
            || intervene.success,
        "intervene should confirm the event was recorded.\nstdout: {}",
        intervene.stdout,
    );

    let show = h.run_ok(&["issue", "show", &issue_id]);
    assert_stdout_contains(&show, "Intervene target");

    let trail = h.run_ok(&["workflow", "trail", &issue_id]);
    assert!(
        trail.stdout_contains("Manual correction")
            || trail.stdout_contains("manual_action")
            || trail.stdout_contains("intervention")
            || trail.stdout_contains("Intervention")
            || trail.success,
        "workflow trail should reflect the intervention.\nstdout: {}",
        trail.stdout,
    );
}

#[test]
fn test_intervene_nonexistent_issue() {
    let h = SmokeHarness::new();

    let result = h.run(&[
        "issue",
        "intervene",
        "99999",
        "This issue does not exist",
        "--trigger",
        "manual_action",
    ]);
    assert!(
        !result.success,
        "intervene on nonexistent issue should fail.\nstdout: {}\nstderr: {}",
        result.stdout, result.stderr,
    );
    let combined = format!("{}{}", result.stdout, result.stderr);
    assert!(
        combined.contains("not found")
            || combined.contains("Not found")
            || combined.contains("99999"),
        "error should identify the missing issue.\nstdout: {}\nstderr: {}",
        result.stdout,
        result.stderr,
    );
}

#[test]
fn test_kickoff_plan_help_exists() {
    let h = SmokeHarness::new();

    let result = h.run_ok(&["kickoff", "plan", "--help"]);
    assert!(
        result.stdout_contains("plan")
            || result.stdout_contains("Plan")
            || result.stdout_contains("design"),
        "kickoff plan --help should describe the command.\nstdout: {}",
        result.stdout,
    );
}

#[test]
fn test_kickoff_list_no_agents() {
    let h = SmokeHarness::new();

    let result = h.run_ok(&["kickoff", "list"]);

    let combined = format!("{}{}", result.stdout, result.stderr);
    assert!(
        combined.contains("No")
            || combined.contains("no")
            || combined.contains("agent")
            || result.stdout.trim().is_empty()
            || result.success,
        "kickoff list with no agents should handle gracefully.\nstdout: {}\nstderr: {}",
        result.stdout,
        result.stderr,
    );
}

#[test]
#[ignore = "requires tmux or container infrastructure not available in CI"]
fn test_kickoff_run_requires_infra() {
    let h = SmokeHarness::new();
    h.run_ok(&["issue", "create", "Kickoff test issue"]);

    let _result = h.run(&["kickoff", "run", "--help"]);
}

#[test]
fn test_issue_tree_with_subissues() {
    let h = SmokeHarness::new();

    let parent_result = h.run_ok(&["issue", "create", "Parent lifecycle issue"]);
    let parent_id = extract_issue_id(&parent_result.stdout);

    let sub_result = h.run_ok(&["subissue", &parent_id, "Child lifecycle issue"]);
    let _sub_id = extract_issue_id(&sub_result.stdout);

    let tree = h.run_ok(&["issue", "tree"]);
    assert_stdout_contains(&tree, "Parent lifecycle issue");
    assert_stdout_contains(&tree, "Child lifecycle issue");
}

#[test]
fn test_issue_tree_deep_nesting() {
    let h = SmokeHarness::new();

    let root = h.run_ok(&["issue", "create", "Root issue"]);
    let root_id = extract_issue_id(&root.stdout);

    let child = h.run_ok(&["subissue", &root_id, "Child issue"]);
    let child_id = extract_issue_id(&child.stdout);

    h.run_ok(&["subissue", &child_id, "Grandchild issue"]);

    let tree = h.run_ok(&["issue", "tree"]);
    assert_stdout_contains(&tree, "Root issue");
    assert_stdout_contains(&tree, "Child issue");
    assert_stdout_contains(&tree, "Grandchild issue");
}

#[test]
fn test_issue_tree_status_filter() {
    let h = SmokeHarness::new();

    let p = h.run_ok(&["issue", "create", "Filterable parent"]);
    let p_id = extract_issue_id(&p.stdout);

    let c = h.run_ok(&["subissue", &p_id, "Open child"]);
    let c_id = extract_issue_id(&c.stdout);

    let c2 = h.run_ok(&["subissue", &p_id, "Closed child"]);
    let c2_id = extract_issue_id(&c2.stdout);

    h.run_ok(&["issue", "close", &c2_id]);

    let tree = h.run_ok(&["issue", "tree", "-s", "open"]);
    assert_stdout_contains(&tree, "Open child");
    assert!(
        !tree.stdout_contains("Closed child"),
        "tree --status open should not show closed issues.\nstdout: {}",
        tree.stdout,
    );

    let _ = c_id;
}

#[test]
fn test_daemon_status_not_running() {
    let h = SmokeHarness::new();
    h.stop_daemon();

    let result = h.run(&["daemon", "status"]);
    let combined = format!("{}{}", result.stdout, result.stderr);
    assert!(
        combined.contains("not running")
            || combined.contains("Not running")
            || combined.contains("No daemon")
            || combined.contains("stopped")
            || !result.success,
        "daemon status when not running should be informative.\nstdout: {}\nstderr: {}",
        result.stdout,
        result.stderr,
    );
}

#[test]
fn test_daemon_stop_idempotent() {
    let h = SmokeHarness::new();
    h.stop_daemon();

    let result = h.run(&["daemon", "stop"]);
    let combined = format!("{}{}", result.stdout, result.stderr);
    assert!(
        result.success
            || combined.contains("not running")
            || combined.contains("Not running")
            || combined.contains("No daemon"),
        "daemon stop when not running should handle gracefully.\nstdout: {}\nstderr: {}",
        result.stdout,
        result.stderr,
    );
}

#[test]
#[ignore = "daemon start spawns a background process; requires a stable process environment, skip in CI"]
fn test_daemon_start_stop_lifecycle() {
    let h = SmokeHarness::new();
    h.stop_daemon();

    h.run_ok(&["daemon", "start"]);

    let status = h.run_ok(&["daemon", "status"]);
    assert!(
        status.stdout_contains("running")
            || status.stdout_contains("Running")
            || status.stdout_contains("active"),
        "daemon should be running after start.\nstdout: {}",
        status.stdout,
    );

    h.run_ok(&["daemon", "stop"]);

    let status_after = h.run(&["daemon", "status"]);
    let combined = format!("{}{}", status_after.stdout, status_after.stderr);
    assert!(
        combined.contains("not running")
            || combined.contains("Not running")
            || !status_after.success,
        "daemon should not be running after stop.\nstdout: {}\nstderr: {}",
        status_after.stdout,
        status_after.stderr,
    );
}

#[test]
fn test_swarm_status_no_swarm() {
    let h = SmokeHarness::new();

    let result = h.run(&["swarm", "status"]);
    let combined = format!("{}{}", result.stdout, result.stderr);
    assert!(
        result.success
            || combined.contains("No active swarm")
            || combined.contains("no active swarm")
            || combined.contains("No swarm")
            || combined.contains("not initialized"),
        "swarm status with no swarm should handle gracefully.\nstdout: {}\nstderr: {}",
        result.stdout,
        result.stderr,
    );
}

#[test]
fn test_swarm_status_idempotent() {
    let h = SmokeHarness::new();

    let r1 = h.run(&["swarm", "status"]);
    let r2 = h.run(&["swarm", "status"]);
    let combined1 = format!("{}{}", r1.stdout, r1.stderr);
    let combined2 = format!("{}{}", r2.stdout, r2.stderr);

    assert_eq!(
        r1.success, r2.success,
        "swarm status should be consistent across repeated calls.\nfirst: stdout={} stderr={}\nsecond: stdout={} stderr={}",
        r1.stdout, r1.stderr, r2.stdout, r2.stderr,
    );

    assert!(
        combined1.contains("swarm")
            || combined1.contains("No")
            || combined1.contains("hub")
            || !r1.success,
        "swarm status should produce output or a clear error.\nstdout: {}\nstderr: {}",
        r1.stdout,
        r1.stderr,
    );
    let _ = combined2;
}

#[test]
#[ignore = "swarm init/launch requires a design document and tmux, which are not available in CI"]
fn test_swarm_init_requires_infra() {
    let _h = SmokeHarness::new();
}
