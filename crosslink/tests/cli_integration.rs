use std::process::Command;
use tempfile::tempdir;

fn run_crosslink(dir: &std::path::Path, args: &[&str]) -> (bool, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_crosslink"))
        .current_dir(dir)
        .args(args)
        .output()
        .expect("Failed to execute crosslink");

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    (output.status.success(), stdout, stderr)
}

fn run_crosslink_info(dir: &std::path::Path, args: &[&str]) -> (bool, String, String) {
    let mut full_args = vec!["--log-level", "info"];
    full_args.extend_from_slice(args);
    let output = Command::new(env!("CARGO_BIN_EXE_crosslink"))
        .current_dir(dir)
        .args(&full_args)
        .output()
        .expect("Failed to execute crosslink");

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    (output.status.success(), stdout, stderr)
}

fn test_dir() -> tempfile::TempDir {
    let dir = tempdir().unwrap();
    assert!(Command::new("git")
        .current_dir(dir.path())
        .args(["init"])
        .output()
        .expect("git init failed")
        .status
        .success());

    assert!(Command::new("git")
        .current_dir(dir.path())
        .args([
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@test",
            "commit",
            "--allow-empty",
            "-m",
            "init",
        ])
        .output()
        .expect("git commit failed")
        .status
        .success());
    dir
}

fn contains_issue_ref(text: &str, id: u32) -> bool {
    text.contains(&format!("#{id}")) || text.contains(&format!("L{id}"))
}

fn init_crosslink(dir: &std::path::Path) {
    let (success, _, stderr) = run_crosslink(dir, &["init"]);
    assert!(success, "Failed to init: {stderr}");
}

#[test]
fn test_init_creates_crosslink_directory() {
    let dir = test_dir();
    let (success, stdout, _) = run_crosslink(dir.path(), &["init"]);

    assert!(success);
    assert!(stdout.contains("Created") || stdout.contains("initialized"));
    assert!(dir.path().join(".crosslink").exists());
    assert!(dir.path().join(".crosslink").join("issues.db").exists());
}

#[test]
fn test_init_twice_warns() {
    let dir = test_dir();

    run_crosslink(dir.path(), &["init"]);
    let (success, stdout, _) = run_crosslink(dir.path(), &["init"]);

    assert!(success);
    assert!(stdout.contains("Already") || stdout.contains("already") || stdout.contains("exists"));
}

#[test]
fn test_create_issue() {
    let dir = test_dir();
    init_crosslink(dir.path());

    let (success, stdout, _) = run_crosslink(dir.path(), &["create", "Test issue"]);

    assert!(success);
    assert!(
        stdout.contains("Created issue") && contains_issue_ref(&stdout, 1),
        "Expected 'Created issue #1' in output, got: {stdout}"
    );
}

#[test]
fn test_create_issue_with_priority() {
    let dir = test_dir();
    init_crosslink(dir.path());

    let (success, _, _) =
        run_crosslink(dir.path(), &["create", "High priority issue", "-p", "high"]);

    assert!(success);

    let (_, list_out, _) = run_crosslink(dir.path(), &["list"]);
    assert!(list_out.contains("high"));
}

#[test]
fn test_create_issue_with_description() {
    let dir = test_dir();
    init_crosslink(dir.path());

    let (success, _, _) = run_crosslink(
        dir.path(),
        &[
            "create",
            "Issue with desc",
            "-d",
            "Detailed description here",
        ],
    );

    assert!(success);

    let (_, show_out, _) = run_crosslink(dir.path(), &["show", "1"]);
    assert!(show_out.contains("Detailed description"));
}

#[test]
fn test_create_subissue() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Parent issue"]);
    let (success, stdout, _) = run_crosslink(dir.path(), &["subissue", "1", "Child issue"]);

    assert!(success);
    assert!(
        stdout.contains("Created subissue") && contains_issue_ref(&stdout, 2),
        "Expected 'Created subissue #2' in output, got: {stdout}"
    );

    let (_, show_out, _) = run_crosslink(dir.path(), &["show", "1"]);
    assert!(show_out.contains("Child") || show_out.contains("subissue"));
}

#[test]
fn test_list_empty() {
    let dir = test_dir();
    init_crosslink(dir.path());

    let (success, stdout, _) = run_crosslink(dir.path(), &["list"]);

    assert!(success);
    assert!(
        stdout.contains("No issues found."),
        "Expected 'No issues found.' in output, got: {stdout}"
    );
}

#[test]
fn test_list_issues() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Issue 1"]);
    run_crosslink(dir.path(), &["create", "Issue 2"]);

    let (success, stdout, _) = run_crosslink(dir.path(), &["list"]);

    assert!(success);
    assert!(stdout.contains("Issue 1"));
    assert!(stdout.contains("Issue 2"));
}

#[test]
fn test_list_filter_by_status() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Open issue"]);
    run_crosslink(dir.path(), &["create", "Closed issue"]);
    run_crosslink(dir.path(), &["close", "2"]);

    let (_, open_list, _) = run_crosslink(dir.path(), &["list", "-s", "open"]);
    assert!(open_list.contains("Open issue"));
    assert!(!open_list.contains("Closed issue"));

    let (_, closed_list, _) = run_crosslink(dir.path(), &["list", "-s", "closed"]);
    assert!(closed_list.contains("Closed issue"));
    assert!(!closed_list.contains("Open issue"));
}

#[test]
fn test_list_filter_by_label() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Bug issue"]);
    run_crosslink(dir.path(), &["create", "Feature issue"]);
    run_crosslink(dir.path(), &["issue", "label", "1", "bug"]);
    run_crosslink(dir.path(), &["issue", "label", "2", "feature"]);

    let (_, bug_list, _) = run_crosslink(dir.path(), &["list", "-l", "bug"]);
    assert!(bug_list.contains("Bug issue"));
    assert!(!bug_list.contains("Feature issue"));
}

#[test]
fn test_show_issue() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Test issue", "-d", "Description"]);

    let (success, stdout, _) = run_crosslink(dir.path(), &["show", "1"]);

    assert!(success);
    assert!(stdout.contains("Test issue"));
    assert!(stdout.contains("Description"));
}

#[test]
fn test_show_nonexistent_issue() {
    let dir = test_dir();
    init_crosslink(dir.path());

    let (success, _, stderr) = run_crosslink(dir.path(), &["show", "999"]);

    assert!(!success || stderr.contains("not found") || stderr.contains("No issue"));
}

#[test]
fn test_update_issue_title() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Original title"]);
    let (success, _, _) = run_crosslink(
        dir.path(),
        &["issue", "update", "1", "--title", "Updated title"],
    );

    assert!(success);

    let (_, show_out, _) = run_crosslink(dir.path(), &["show", "1"]);
    assert!(show_out.contains("Updated title"));
}

#[test]
fn test_update_issue_priority() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Issue", "-p", "low"]);
    run_crosslink(dir.path(), &["issue", "update", "1", "-p", "critical"]);

    let (_, show_out, _) = run_crosslink(dir.path(), &["show", "1"]);
    assert!(show_out.contains("critical"));
}

#[test]
fn test_close_issue() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Test issue"]);
    let (success, stdout, stderr) = run_crosslink(dir.path(), &["close", "1"]);

    assert!(success, "close failed: stdout={stdout} stderr={stderr}");
    assert!(stdout.contains("Closed") || stdout.contains("closed"));

    let (_, show_out, _) = run_crosslink(dir.path(), &["show", "1"]);
    assert!(show_out.contains("closed"));
}

#[test]
fn test_reopen_issue() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Test issue"]);
    run_crosslink(dir.path(), &["close", "1"]);
    let (success, stdout, _) = run_crosslink(dir.path(), &["issue", "reopen", "1"]);

    assert!(success);
    assert!(stdout.contains("Reopened") || stdout.contains("reopen"));

    let (_, show_out, _) = run_crosslink(dir.path(), &["show", "1"]);
    assert!(show_out.contains("open"));
}

#[test]
fn test_delete_issue() {
    let dir = test_dir();
    init_crosslink(dir.path());

    let (_, create_out, _) = run_crosslink(dir.path(), &["create", "To delete"]);
    let (success, del_out, del_err) = run_crosslink(dir.path(), &["issue", "delete", "1", "-f"]);

    assert!(success, "delete failed: stdout={del_out} stderr={del_err}");

    let (_, list_out, list_err) = run_crosslink(dir.path(), &["list"]);
    assert!(
        !list_out.contains("To delete"),
        "Deleted issue still in list.\ncreate: {}\ndelete: {}\nlist: {}\nlist_err: {}",
        create_out.trim(),
        del_out.trim(),
        list_out.trim(),
        list_err.trim()
    );
}

#[test]
fn test_add_label() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Test issue"]);
    let (success, _, _) = run_crosslink(dir.path(), &["issue", "label", "1", "bug"]);

    assert!(success);

    let (_, show_out, _) = run_crosslink(dir.path(), &["show", "1"]);
    assert!(show_out.contains("bug"));
}

#[test]
fn test_remove_label() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Test issue"]);
    run_crosslink(dir.path(), &["issue", "label", "1", "bug"]);
    let (success, _, _) = run_crosslink(dir.path(), &["issue", "unlabel", "1", "bug"]);

    assert!(success);

    let (_, show_out, _) = run_crosslink(dir.path(), &["show", "1"]);
    assert!(!show_out.contains("bug") || show_out.contains("Labels: none"));
}

#[test]
fn test_add_comment() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Test issue"]);
    let (success, _, _) =
        run_crosslink(dir.path(), &["issue", "comment", "1", "This is a comment"]);

    assert!(success);

    let (_, show_out, _) = run_crosslink(dir.path(), &["show", "1"]);
    assert!(show_out.contains("This is a comment"));
}

#[test]
fn test_block_issue() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Blocked issue"]);
    run_crosslink(dir.path(), &["create", "Blocker issue"]);
    let (success, _, _) = run_crosslink(dir.path(), &["issue", "block", "1", "2"]);

    assert!(success);

    let (_, blocked_out, _) = run_crosslink(dir.path(), &["issue", "blocked"]);
    assert!(blocked_out.contains("Blocked issue"));
}

#[test]
fn test_unblock_issue() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Blocked issue"]);
    run_crosslink(dir.path(), &["create", "Blocker issue"]);
    run_crosslink(dir.path(), &["issue", "block", "1", "2"]);
    let (success, _, _) = run_crosslink(dir.path(), &["issue", "unblock", "1", "2"]);

    assert!(success);

    let (_, blocked_out, _) = run_crosslink(dir.path(), &["issue", "blocked"]);
    assert!(!blocked_out.contains("Blocked issue"));
}

#[test]
fn test_ready_issues() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Blocked issue"]);
    run_crosslink(dir.path(), &["create", "Blocker issue"]);
    run_crosslink(dir.path(), &["create", "Ready issue"]);
    run_crosslink(dir.path(), &["issue", "block", "1", "2"]);

    let (success, stdout, _) = run_crosslink(dir.path(), &["issue", "ready"]);

    assert!(success);
    assert!(stdout.contains("Ready issue"));
    assert!(stdout.contains("Blocker issue"));
    assert!(!stdout.contains("Blocked issue"));
}

#[test]
fn test_session_start() {
    let dir = test_dir();
    init_crosslink(dir.path());

    let (success, stdout, _) = run_crosslink(dir.path(), &["session", "start"]);

    assert!(success);
    assert!(stdout.contains("Session") || stdout.contains("started"));
}

#[test]
fn test_session_status() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["session", "start"]);
    let (success, stdout, _) = run_crosslink(dir.path(), &["session", "status"]);

    assert!(success);
    assert!(stdout.contains("Session") || stdout.contains("active"));
}

#[test]
fn test_session_work() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Working issue"]);
    run_crosslink(dir.path(), &["session", "start"]);
    let (success, stdout, _) = run_crosslink(dir.path(), &["session", "work", "1"]);

    assert!(success);
    assert!(stdout.contains("Working") || contains_issue_ref(&stdout, 1));
}

#[test]
fn test_session_end() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["session", "start"]);
    let (success, stdout, _) =
        run_crosslink(dir.path(), &["session", "end", "--notes", "Finished work"]);

    assert!(success);
    assert!(stdout.contains("ended") || stdout.contains("Session"));
}

#[test]
fn test_search_issues() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Authentication bug"]);
    run_crosslink(dir.path(), &["create", "Dark mode feature"]);
    run_crosslink(dir.path(), &["create", "Auth improvements"]);

    let (success, stdout, _) = run_crosslink(dir.path(), &["issue", "search", "auth"]);

    assert!(success);
    assert!(stdout.contains("Authentication") || stdout.contains("Auth"));
    assert!(!stdout.contains("Dark mode"));
}

#[test]
fn test_command_without_init() {
    let dir = test_dir();

    let (success, stdout, stderr) = run_crosslink(dir.path(), &["list"]);

    if !success {
        assert!(
            stderr.contains("Not a crosslink repository") || stderr.contains("crosslink init"),
            "Error should mention missing repo, got stderr: {stderr}"
        );
    } else {
        assert!(
            stdout.contains("No issues") || stdout.contains("#"),
            "Should show valid list output, got: {stdout}"
        );
    }
}

#[test]
fn test_invalid_priority() {
    let dir = test_dir();
    init_crosslink(dir.path());

    let (success, _, stderr) = run_crosslink(dir.path(), &["create", "Issue", "-p", "invalid"]);

    assert!(!success, "Creating issue with invalid priority should fail");
    assert!(
        stderr.contains("Invalid") || stderr.contains("priority"),
        "Error should mention invalid priority, got stderr: {stderr}"
    );
}

#[test]
fn test_sql_injection_in_title_cli() {
    let dir = test_dir();
    init_crosslink(dir.path());

    let malicious = "'; DROP TABLE issues; --";
    let (success, _, _) = run_crosslink(dir.path(), &["create", malicious]);

    assert!(success);

    let (success2, stdout, _) = run_crosslink(dir.path(), &["list"]);
    assert!(success2);
    assert!(stdout.contains(malicious));
}

#[test]
fn test_special_characters_in_fields() {
    let dir = test_dir();
    init_crosslink(dir.path());

    let special = "Test <>&\"'\\n\\t issue";
    let (success, _, _) = run_crosslink(dir.path(), &["create", special]);

    assert!(success);

    let (_, show_out, _) = run_crosslink(dir.path(), &["show", "1"]);
    assert!(show_out.contains("Test"));
}

#[test]
fn test_unicode_in_cli() {
    let dir = test_dir();
    init_crosslink(dir.path());

    let unicode = "测试问题 🐛 émoji";
    let (success, _, _) = run_crosslink(dir.path(), &["create", unicode]);

    assert!(success);

    let (_, show_out, _) = run_crosslink(dir.path(), &["show", "1"]);
    assert!(show_out.contains("测试") || show_out.contains("🐛"));
}

#[test]
fn test_archive_closed_issue() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Issue to archive"]);
    run_crosslink(dir.path(), &["close", "1"]);
    let (success, stdout, _) = run_crosslink(dir.path(), &["archive", "add", "1"]);

    assert!(success);
    assert!(stdout.contains("Archived") || stdout.contains("archived"));
}

#[test]
fn test_archive_open_issue_fails() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Open issue"]);
    let (success, stdout, stderr) = run_crosslink(dir.path(), &["archive", "add", "1"]);

    assert!(
        !success
            || stderr.contains("closed")
            || stderr.contains("open")
            || stdout.contains("not closed")
            || stdout.contains("Cannot")
    );
}

#[test]
fn test_archive_list() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Issue to archive"]);
    run_crosslink(dir.path(), &["create", "Open issue"]);
    run_crosslink(dir.path(), &["close", "1"]);
    run_crosslink(dir.path(), &["archive", "add", "1"]);

    let (success, stdout, _) = run_crosslink(dir.path(), &["archive", "list"]);

    assert!(success);
    assert!(stdout.contains("Issue to archive") || contains_issue_ref(&stdout, 1));
}

#[test]
fn test_unarchive_issue() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Issue to archive"]);
    run_crosslink(dir.path(), &["close", "1"]);
    run_crosslink(dir.path(), &["archive", "add", "1"]);
    let (success, stdout, _) = run_crosslink(dir.path(), &["archive", "remove", "1"]);

    assert!(success);
    assert!(
        stdout.contains("Unarchived")
            || stdout.contains("restored")
            || stdout.contains("removed")
            || stdout.contains("Restored")
    );

    let (_, closed_list, _) = run_crosslink(dir.path(), &["list", "-s", "closed"]);
    assert!(closed_list.contains("Issue to archive"));
}

#[test]
fn test_milestone_create() {
    let dir = test_dir();
    init_crosslink(dir.path());

    let (success, stdout, _) = run_crosslink(
        dir.path(),
        &["milestone", "create", "v1.0", "-d", "First release"],
    );

    assert!(success);
    assert!(
        stdout.contains("v1.0") || contains_issue_ref(&stdout, 1) || stdout.contains("Created")
    );
}

#[test]
fn test_milestone_list() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["milestone", "create", "v1.0"]);
    run_crosslink(dir.path(), &["milestone", "create", "v2.0"]);

    let (success, stdout, _) = run_crosslink(dir.path(), &["milestone", "list"]);

    assert!(success);
    assert!(stdout.contains("v1.0"));
    assert!(stdout.contains("v2.0"));
}

#[test]
fn test_milestone_show() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(
        dir.path(),
        &["milestone", "create", "v1.0", "-d", "First release"],
    );

    let (success, stdout, _) = run_crosslink(dir.path(), &["milestone", "show", "1"]);

    assert!(success);
    assert!(stdout.contains("v1.0"));
    assert!(stdout.contains("First release") || stdout.contains("description"));
}

#[test]
fn test_milestone_add_issues() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["milestone", "create", "v1.0"]);
    run_crosslink(dir.path(), &["create", "Feature 1"]);
    run_crosslink(dir.path(), &["create", "Feature 2"]);

    let (success, _, _) = run_crosslink(dir.path(), &["milestone", "add", "1", "1", "2"]);

    assert!(success);

    let (_, show_out, _) = run_crosslink(dir.path(), &["milestone", "show", "1"]);
    assert!(show_out.contains("Feature 1") || contains_issue_ref(&show_out, 1));
}

#[test]
fn test_milestone_close() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["milestone", "create", "v1.0"]);
    let (success, stdout, _) = run_crosslink(dir.path(), &["milestone", "close", "1"]);

    assert!(success);
    assert!(stdout.contains("Closed") || stdout.contains("closed"));
}

#[test]
fn test_milestone_delete() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["milestone", "create", "v1.0"]);
    let (success, _, _) = run_crosslink(dir.path(), &["milestone", "delete", "1"]);

    assert!(success);

    let (_, list_out, _) = run_crosslink(dir.path(), &["milestone", "list", "-s", "all"]);
    assert!(!list_out.contains("v1.0") || list_out.contains("No milestones"));
}

#[test]
fn test_timer_start() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Issue to time"]);
    let (success, stdout, _) = run_crosslink(dir.path(), &["start", "1"]);

    assert!(success);
    assert!(
        stdout.contains("Started") || stdout.contains("timer") || contains_issue_ref(&stdout, 1)
    );
}

#[test]
fn test_timer_stop() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Issue to time"]);
    run_crosslink(dir.path(), &["start", "1"]);
    let (success, stdout, _) = run_crosslink(dir.path(), &["stop"]);

    assert!(success);
    assert!(stdout.contains("Stopped") || stdout.contains("stopped") || stdout.contains("timer"));
}

#[test]
fn test_timer_status() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Issue to time"]);
    run_crosslink(dir.path(), &["start", "1"]);

    let (success, stdout, _) = run_crosslink(dir.path(), &["timer", "show"]);

    assert!(success);
    assert!(
        contains_issue_ref(&stdout, 1)
            || stdout.contains("Issue to time")
            || stdout.contains("running")
    );
}

#[test]
fn test_timer_status_no_timer() {
    let dir = test_dir();
    init_crosslink(dir.path());

    let (success, stdout, _) = run_crosslink(dir.path(), &["timer", "show"]);

    assert!(success);
    assert!(
        stdout.contains("No timer running"),
        "Expected 'No timer running' message, got: {stdout}"
    );
}

#[test]
fn test_relate_issues() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Issue 1"]);
    run_crosslink(dir.path(), &["create", "Issue 2"]);

    let (success, _, _) = run_crosslink(dir.path(), &["issue", "relate", "1", "2"]);

    assert!(success);
}

#[test]
fn test_related_list() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Issue 1"]);
    run_crosslink(dir.path(), &["create", "Issue 2"]);
    run_crosslink(dir.path(), &["issue", "relate", "1", "2"]);

    let (success, stdout, _) = run_crosslink(dir.path(), &["issue", "related", "1"]);

    assert!(success);
    assert!(stdout.contains("Issue 2") || contains_issue_ref(&stdout, 2));
}

#[test]
fn test_unrelate_issues() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Issue 1"]);
    run_crosslink(dir.path(), &["create", "Issue 2"]);
    run_crosslink(dir.path(), &["issue", "relate", "1", "2"]);
    let (success, _, _) = run_crosslink(dir.path(), &["issue", "unrelate", "1", "2"]);

    assert!(success);

    let (_, related_out, _) = run_crosslink(dir.path(), &["issue", "related", "1"]);
    assert!(!related_out.contains("Issue 2") || related_out.contains("No related"));
}

#[test]
fn test_tree_command() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Parent issue"]);
    run_crosslink(dir.path(), &["subissue", "1", "Child issue"]);

    let (success, stdout, _) = run_crosslink(dir.path(), &["issue", "tree"]);

    assert!(success);
    assert!(stdout.contains("Parent issue"));
    assert!(stdout.contains("Child issue"));
}

#[test]
fn test_tree_with_status_filter() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Open parent"]);
    run_crosslink(dir.path(), &["create", "Closed parent"]);
    run_crosslink(dir.path(), &["close", "2"]);

    let (success, stdout, _) = run_crosslink(dir.path(), &["issue", "tree", "-s", "open"]);

    assert!(success);
    assert!(stdout.contains("Open parent"));

    assert!(!stdout.contains("Closed parent"));
}

#[test]
fn test_next_command() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Low priority", "-p", "low"]);
    run_crosslink(dir.path(), &["create", "High priority", "-p", "high"]);
    run_crosslink(
        dir.path(),
        &["create", "Critical priority", "-p", "critical"],
    );

    let (success, stdout, _) = run_crosslink(dir.path(), &["issue", "next"]);

    assert!(success);

    assert!(
        stdout.contains("Critical priority")
            || stdout.contains("High priority")
            || contains_issue_ref(&stdout, 3)
            || contains_issue_ref(&stdout, 2)
    );
}

#[test]
fn test_next_no_issues() {
    let dir = test_dir();
    init_crosslink(dir.path());

    let (success, stdout, _) = run_crosslink(dir.path(), &["issue", "next"]);

    assert!(success);
    assert!(
        stdout.contains("No issues ready to work on"),
        "Expected 'No issues ready to work on' message, got: {stdout}"
    );
}

#[test]
fn test_export_json() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Issue 1"]);
    run_crosslink(dir.path(), &["create", "Issue 2"]);

    let export_path = dir.path().join("export.json");
    let (success, _, _) = run_crosslink(
        dir.path(),
        &["export", "-o", export_path.to_str().unwrap(), "-f", "json"],
    );

    assert!(success);
    assert!(export_path.exists());

    let content = std::fs::read_to_string(&export_path).unwrap();
    assert!(content.contains("Issue 1"));
    assert!(content.contains("Issue 2"));
}

#[test]
fn test_export_markdown() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Issue 1", "-d", "Description 1"]);

    let export_path = dir.path().join("export.md");
    let (success, _, _) = run_crosslink(
        dir.path(),
        &[
            "export",
            "-o",
            export_path.to_str().unwrap(),
            "-f",
            "markdown",
        ],
    );

    assert!(success);
    assert!(export_path.exists());

    let content = std::fs::read_to_string(&export_path).unwrap();
    assert!(
        content.contains("Issue 1"),
        "Exported markdown should contain issue title, got: {}",
        &content[..content.len().min(200)]
    );
}

#[test]
fn test_import_json() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Exported Issue"]);
    let export_path = dir.path().join("export.json");
    run_crosslink(
        dir.path(),
        &["export", "-o", export_path.to_str().unwrap(), "-f", "json"],
    );

    let dir2 = test_dir();
    init_crosslink(dir2.path());

    let (success, _, _) = run_crosslink(dir2.path(), &["import", export_path.to_str().unwrap()]);

    assert!(success);

    let (_, list_out, _) = run_crosslink(dir2.path(), &["list", "-s", "all"]);
    assert!(list_out.contains("Exported Issue") || contains_issue_ref(&list_out, 1));
}

#[test]
fn test_tested_command() {
    let dir = test_dir();
    init_crosslink(dir.path());

    let (success, stdout, _) = run_crosslink(dir.path(), &["issue", "tested"]);

    assert!(success);
    assert!(
        stdout.contains("Marked tests as run"),
        "Expected 'Marked tests as run' in output, got: {stdout}"
    );
}

#[test]
fn test_create_with_template() {
    let dir = test_dir();
    init_crosslink(dir.path());

    let (success, _, _) = run_crosslink(dir.path(), &["create", "Bug report", "-t", "bug"]);

    assert!(success);

    let (_, show_out, _) = run_crosslink(dir.path(), &["show", "1"]);
    assert!(show_out.contains("Bug report"));
}

#[test]
fn test_create_all_priorities() {
    let dir = test_dir();
    init_crosslink(dir.path());

    for priority in &["low", "medium", "high", "critical"] {
        let (success, _, _) = run_crosslink(
            dir.path(),
            &["create", &format!("{priority} issue"), "-p", priority],
        );
        assert!(success, "Failed to create {priority} priority issue");
    }

    let (_, list_out, _) = run_crosslink(dir.path(), &["list"]);
    assert!(list_out.contains("low"));
    assert!(list_out.contains("medium"));
    assert!(list_out.contains("high"));
    assert!(list_out.contains("critical"));
}

#[test]
fn test_subissue_with_priority() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Parent"]);
    let (success, _, _) = run_crosslink(dir.path(), &["subissue", "1", "Child", "-p", "critical"]);

    assert!(success);

    let (_, show_out, _) = run_crosslink(dir.path(), &["show", "2"]);
    assert!(show_out.contains("critical"));
}

#[test]
fn test_subissue_with_description() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Parent"]);
    let (success, _, _) = run_crosslink(
        dir.path(),
        &["subissue", "1", "Child", "-d", "Child description"],
    );

    assert!(success);

    let (_, show_out, _) = run_crosslink(dir.path(), &["show", "2"]);
    assert!(show_out.contains("Child description"));
}

#[test]
fn test_delete_nonexistent_issue() {
    let dir = test_dir();
    init_crosslink(dir.path());

    let (success, _, stderr) = run_crosslink(dir.path(), &["issue", "delete", "999", "-f"]);

    assert!(!success || stderr.contains("not found") || stderr.contains("No issue"));
}

#[test]
fn test_delete_with_subissues() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Parent"]);
    run_crosslink(dir.path(), &["subissue", "1", "Child"]);

    let (success, _, _) = run_crosslink(dir.path(), &["issue", "delete", "1", "-f"]);

    assert!(success);

    let (_, list_out, _) = run_crosslink(dir.path(), &["list", "-s", "all"]);
    assert!(!list_out.contains("Parent"));
    assert!(!list_out.contains("Child"));
}

#[test]
fn test_session_work_nonexistent_issue() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["session", "start"]);
    let (success, _, stderr) = run_crosslink(dir.path(), &["session", "work", "999"]);

    assert!(!success || stderr.contains("not found") || stderr.contains("No issue"));
}

#[test]
fn test_session_end_without_start() {
    let dir = test_dir();
    init_crosslink(dir.path());

    let (success, stdout, stderr) = run_crosslink(dir.path(), &["session", "end"]);

    assert!(
        !success || stdout.contains("No active") || stderr.contains("No active"),
        "Ending without starting should fail or report no active session, got stdout: {stdout}, stderr: {stderr}"
    );
}

#[test]
fn test_session_status_without_session() {
    let dir = test_dir();
    init_crosslink(dir.path());

    let (success, stdout, _) = run_crosslink(dir.path(), &["session", "status"]);

    assert!(success);
    assert!(
        stdout.contains("No active session"),
        "Expected 'No active session' message, got: {stdout}"
    );
}

#[test]
fn test_session_multiple_starts() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["session", "start"]);
    let (success, stdout, _) = run_crosslink(dir.path(), &["session", "start"]);

    assert!(success);
    assert!(stdout.contains("already") || stdout.contains("Session") || stdout.contains("started"));
}

#[test]
fn test_next_with_blocked_issues() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Blocked issue", "-p", "critical"]);
    run_crosslink(dir.path(), &["create", "Blocker issue", "-p", "low"]);
    run_crosslink(dir.path(), &["issue", "block", "1", "2"]);

    let (success, stdout, _) = run_crosslink(dir.path(), &["issue", "next"]);

    assert!(success);

    assert!(
        stdout.contains("Blocker issue"),
        "Next should recommend the unblocked blocker, got: {stdout}"
    );
    assert!(
        !stdout.contains("Next: #1"),
        "Next should not recommend the blocked issue as top pick"
    );
}

#[test]
fn test_next_all_closed() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Issue 1"]);
    run_crosslink(dir.path(), &["close", "1"]);

    let (success, stdout, _) = run_crosslink(dir.path(), &["issue", "next"]);

    assert!(success);
    assert!(
        stdout.contains("No issues ready to work on"),
        "Expected 'No issues ready to work on' message, got: {stdout}"
    );
}

#[test]
fn test_archive_older_days() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Old issue"]);
    run_crosslink(dir.path(), &["close", "1"]);

    let (success, _, _) = run_crosslink(dir.path(), &["archive", "older", "0"]);

    assert!(success);
}

#[test]
fn test_archive_already_archived() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Issue"]);
    run_crosslink(dir.path(), &["close", "1"]);
    run_crosslink(dir.path(), &["archive", "add", "1"]);

    let (success, stdout, stderr) = run_crosslink(dir.path(), &["archive", "add", "1"]);

    assert!(
        stdout.contains("already") || stderr.contains("already") || !success,
        "Archiving twice should indicate already archived, got stdout: {stdout}, stderr: {stderr}"
    );
}

#[test]
fn test_milestone_remove_issue() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["milestone", "create", "v1.0"]);
    run_crosslink(dir.path(), &["create", "Feature"]);
    run_crosslink(dir.path(), &["milestone", "add", "1", "1"]);

    let (success, _, _) = run_crosslink(dir.path(), &["milestone", "remove", "1", "1"]);

    assert!(success);

    let (_, show_out, _) = run_crosslink(dir.path(), &["milestone", "show", "1"]);
    assert!(!show_out.contains("Feature") || show_out.contains("No issues"));
}

#[test]
fn test_milestone_show_nonexistent() {
    let dir = test_dir();
    init_crosslink(dir.path());

    let (success, _, stderr) = run_crosslink(dir.path(), &["milestone", "show", "999"]);

    assert!(!success || stderr.contains("not found") || stderr.contains("No milestone"));
}

#[test]
fn test_milestone_list_closed() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["milestone", "create", "v1.0"]);
    run_crosslink(dir.path(), &["milestone", "create", "v2.0"]);
    run_crosslink(dir.path(), &["milestone", "close", "1"]);

    let (success, stdout, _) = run_crosslink(dir.path(), &["milestone", "list", "-s", "closed"]);

    assert!(success);
    assert!(stdout.contains("v1.0"));
    assert!(!stdout.contains("v2.0"));
}

#[test]
fn test_list_filter_by_priority() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Low issue", "-p", "low"]);
    run_crosslink(dir.path(), &["create", "High issue", "-p", "high"]);

    let (success, stdout, _) = run_crosslink(dir.path(), &["list", "-p", "high"]);

    assert!(success);
    assert!(stdout.contains("High issue"));
    assert!(!stdout.contains("Low issue"));
}

#[test]
fn test_list_all_statuses() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Open issue"]);
    run_crosslink(dir.path(), &["create", "Closed issue"]);
    run_crosslink(dir.path(), &["close", "2"]);

    let (success, stdout, _) = run_crosslink(dir.path(), &["list", "-s", "all"]);

    assert!(success);
    assert!(stdout.contains("Open issue"));
    assert!(stdout.contains("Closed issue"));
}

#[test]
fn test_update_description() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Issue"]);
    let (success, _, _) = run_crosslink(
        dir.path(),
        &["issue", "update", "1", "-d", "New description"],
    );

    assert!(success);

    let (_, show_out, _) = run_crosslink(dir.path(), &["show", "1"]);
    assert!(show_out.contains("New description"));
}

#[test]
fn test_update_nonexistent() {
    let dir = test_dir();
    init_crosslink(dir.path());

    let (success, _, stderr) =
        run_crosslink(dir.path(), &["issue", "update", "999", "--title", "New"]);

    assert!(!success || stderr.contains("not found") || stderr.contains("No issue"));
}

#[test]
fn test_show_with_labels() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Issue"]);
    run_crosslink(dir.path(), &["issue", "label", "1", "bug"]);
    run_crosslink(dir.path(), &["issue", "label", "1", "urgent"]);

    let (success, stdout, _) = run_crosslink(dir.path(), &["show", "1"]);

    assert!(success);
    assert!(stdout.contains("bug"));
    assert!(stdout.contains("urgent"));
}

#[test]
fn test_show_with_comments() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Issue"]);
    run_crosslink(dir.path(), &["issue", "comment", "1", "First comment"]);
    run_crosslink(dir.path(), &["issue", "comment", "1", "Second comment"]);

    let (success, stdout, _) = run_crosslink(dir.path(), &["show", "1"]);

    assert!(success);
    assert!(stdout.contains("First comment"));
    assert!(stdout.contains("Second comment"));
}

#[test]
fn test_show_with_blockers() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Blocked"]);
    run_crosslink(dir.path(), &["create", "Blocker"]);
    run_crosslink(dir.path(), &["issue", "block", "1", "2"]);

    let (success, stdout, _) = run_crosslink(dir.path(), &["show", "1"]);

    assert!(success);
    assert!(
        stdout.contains("Blocker") || contains_issue_ref(&stdout, 2) || stdout.contains("blocked")
    );
}

#[test]
fn test_search_no_results() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Test issue"]);

    let (success, stdout, _) = run_crosslink(dir.path(), &["issue", "search", "nonexistent"]);

    assert!(success);
    assert!(
        stdout.contains("No issues found matching"),
        "Expected 'No issues found matching' message, got: {stdout}"
    );
}

#[test]
fn test_search_in_description() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(
        dir.path(),
        &["create", "Generic title", "-d", "specific_keyword_here"],
    );

    let (success, stdout, _) = run_crosslink(dir.path(), &["issue", "search", "specific_keyword"]);

    assert!(success);
    assert!(stdout.contains("Generic title") || contains_issue_ref(&stdout, 1));
}

#[test]
fn test_init_force_update() {
    let dir = test_dir();

    run_crosslink(dir.path(), &["init"]);
    let (success, stdout, _) = run_crosslink(dir.path(), &["init", "--force"]);

    assert!(success);
    assert!(
        stdout.contains("Updated")
            || stdout.contains("updated")
            || stdout.contains("Created")
            || stdout.contains("initialized")
    );
}

#[test]
fn test_full_issue_lifecycle() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Lifecycle test", "-p", "high"]);

    run_crosslink(dir.path(), &["issue", "label", "1", "feature"]);

    run_crosslink(dir.path(), &["issue", "comment", "1", "Working on this"]);

    run_crosslink(dir.path(), &["issue", "update", "1", "-p", "critical"]);

    run_crosslink(dir.path(), &["close", "1"]);

    let (success, stdout, _) = run_crosslink(dir.path(), &["show", "1"]);
    assert!(success);
    assert!(stdout.contains("Lifecycle test"));
    assert!(stdout.contains("critical"));
    assert!(stdout.contains("feature"));
    assert!(stdout.contains("Working on this"));
    assert!(stdout.contains("closed"));
}

#[test]
fn test_dependency_chain() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Final task"]);
    run_crosslink(dir.path(), &["create", "Middle task"]);
    run_crosslink(dir.path(), &["create", "First task"]);

    run_crosslink(dir.path(), &["issue", "block", "1", "2"]);
    run_crosslink(dir.path(), &["issue", "block", "2", "3"]);

    let (success, stdout, _) = run_crosslink(dir.path(), &["issue", "ready"]);
    assert!(success);
    assert!(stdout.contains("First task") || contains_issue_ref(&stdout, 3));
    assert!(!stdout.contains("Final task"));
    assert!(!stdout.contains("Middle task"));

    run_crosslink(dir.path(), &["close", "3"]);
    let (_, stdout, _) = run_crosslink(dir.path(), &["issue", "ready"]);
    assert!(stdout.contains("Middle task") || contains_issue_ref(&stdout, 2));
}

#[test]
fn test_next_with_multiple_ready_issues() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Low prio task", "-p", "low"]);
    run_crosslink(dir.path(), &["create", "Medium prio task", "-p", "medium"]);
    run_crosslink(dir.path(), &["create", "High prio task", "-p", "high"]);
    run_crosslink(dir.path(), &["create", "Critical task", "-p", "critical"]);

    let (success, stdout, _) = run_crosslink(dir.path(), &["issue", "next"]);

    assert!(success);

    assert!(stdout.contains("Critical") || contains_issue_ref(&stdout, 4));

    assert!(stdout.contains("Also ready") || stdout.contains("ready"));
}

#[test]
fn test_next_with_description() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(
        dir.path(),
        &[
            "create",
            "Task with description",
            "-p",
            "high",
            "-d",
            "This is a detailed description for the task",
        ],
    );

    let (success, stdout, _) = run_crosslink(dir.path(), &["issue", "next"]);

    assert!(success);
    assert!(stdout.contains("description") || stdout.contains("Task with description"));
}

#[test]
fn test_next_with_subissue_progress() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Parent task", "-p", "high"]);
    run_crosslink(dir.path(), &["subissue", "1", "Sub 1"]);
    run_crosslink(dir.path(), &["subissue", "1", "Sub 2"]);
    run_crosslink(dir.path(), &["subissue", "1", "Sub 3"]);

    run_crosslink(dir.path(), &["close", "2"]);

    let (success, stdout, _) = run_crosslink(dir.path(), &["issue", "next"]);

    assert!(success);

    assert!(
        stdout.contains("Progress")
            || stdout.contains("1/3")
            || stdout.contains("subissue")
            || stdout.contains("Parent task")
    );
}

#[test]
fn test_next_only_subissues_ready() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Blocker"]);
    run_crosslink(dir.path(), &["create", "Parent"]);
    run_crosslink(dir.path(), &["issue", "block", "2", "1"]);

    run_crosslink(dir.path(), &["subissue", "2", "Subissue"]);

    run_crosslink(dir.path(), &["close", "1"]);

    let (success, stdout, _) = run_crosslink(dir.path(), &["issue", "next"]);

    assert!(success);

    assert!(
        stdout.contains("Next")
            || contains_issue_ref(&stdout, 2)
            || contains_issue_ref(&stdout, 3)
            || stdout.contains("Parent")
            || stdout.contains("Subissue")
    );
}

#[test]
fn test_import_with_parent_relationships() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Parent issue"]);
    run_crosslink(dir.path(), &["subissue", "1", "Child issue"]);

    let export_path = dir.path().join("export.json");
    run_crosslink(
        dir.path(),
        &["export", "-o", export_path.to_str().unwrap(), "-f", "json"],
    );

    let dir2 = test_dir();
    init_crosslink(dir2.path());

    std::fs::copy(&export_path, dir2.path().join("import.json")).unwrap();

    let import_path = dir2.path().join("import.json");
    let (success, stdout, _) =
        run_crosslink(dir2.path(), &["import", import_path.to_str().unwrap()]);

    assert!(success);
    assert!(stdout.contains("Imported") || stdout.contains("import"));

    let (_, tree_out, _) = run_crosslink(dir2.path(), &["issue", "tree"]);
    assert!(tree_out.contains("Parent") && tree_out.contains("Child"));
}

#[test]
fn test_import_with_labels_and_comments() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Labeled issue"]);
    run_crosslink(dir.path(), &["issue", "label", "1", "bug"]);
    run_crosslink(dir.path(), &["issue", "label", "1", "urgent"]);
    run_crosslink(dir.path(), &["issue", "comment", "1", "First comment"]);
    run_crosslink(dir.path(), &["close", "1"]);

    let export_path = dir.path().join("export.json");
    run_crosslink(
        dir.path(),
        &["export", "-o", export_path.to_str().unwrap(), "-f", "json"],
    );

    let dir2 = test_dir();
    init_crosslink(dir2.path());

    std::fs::copy(&export_path, dir2.path().join("import.json")).unwrap();

    let import_path = dir2.path().join("import.json");
    let (success, _, _) = run_crosslink(dir2.path(), &["import", import_path.to_str().unwrap()]);

    assert!(success);

    let (_, show_out, _) = run_crosslink(dir2.path(), &["show", "1"]);
    assert!(show_out.contains("bug") || show_out.contains("Label"));
    assert!(show_out.contains("closed") || show_out.contains("Closed"));
}

#[test]
fn test_session_start_shows_handoff_notes() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["session", "start"]);
    run_crosslink(
        dir.path(),
        &["session", "end", "--notes", "Remember to check auth module"],
    );

    let (success, stdout, _) = run_crosslink(dir.path(), &["session", "start"]);

    assert!(success);
    assert!(
        stdout.contains("Remember to check auth module")
            || stdout.contains("Handoff")
            || stdout.contains("Previous")
            || stdout.contains("notes")
    );
}

#[test]
fn test_session_status_with_active_issue() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Active task"]);
    run_crosslink(dir.path(), &["session", "start"]);
    run_crosslink(dir.path(), &["session", "work", "1"]);

    let (success, stdout, _) = run_crosslink(dir.path(), &["session", "status"]);

    assert!(success);
    assert!(
        stdout.contains("Active task")
            || contains_issue_ref(&stdout, 1)
            || stdout.contains("Working")
    );
}

#[test]
fn test_template_with_priority_override() {
    let dir = test_dir();
    init_crosslink(dir.path());

    let (success, stdout, _) = run_crosslink(
        dir.path(),
        &["create", "Critical bug", "-t", "bug", "-p", "critical"],
    );

    assert!(success);
    assert!(contains_issue_ref(&stdout, 1));

    let (_, show_out, _) = run_crosslink(dir.path(), &["show", "1"]);
    assert!(show_out.contains("critical"));
}

#[test]
fn test_template_with_user_description() {
    let dir = test_dir();
    init_crosslink(dir.path());

    let (success, stdout, _) = run_crosslink(
        dir.path(),
        &[
            "create",
            "Bug with details",
            "-t",
            "bug",
            "-d",
            "User provided details here",
        ],
    );

    assert!(success);
    assert!(contains_issue_ref(&stdout, 1));

    let (_, show_out, _) = run_crosslink(dir.path(), &["show", "1"]);

    assert!(show_out.contains("User provided details") || show_out.contains("Steps to reproduce"));
}

#[test]
fn test_subissue_invalid_parent() {
    let dir = test_dir();
    init_crosslink(dir.path());

    let (success, _, stderr) = run_crosslink(dir.path(), &["subissue", "999", "Orphan"]);

    assert!(!success);
    assert!(stderr.contains("not found") || stderr.contains("999") || stderr.contains("Parent"));
}

#[test]
fn test_related_issues_display() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Issue A"]);
    run_crosslink(dir.path(), &["create", "Issue B"]);
    run_crosslink(dir.path(), &["create", "Issue C"]);

    run_crosslink(dir.path(), &["issue", "relate", "1", "2"]);
    run_crosslink(dir.path(), &["issue", "relate", "1", "3"]);

    let (success, stdout, _) = run_crosslink(dir.path(), &["issue", "related", "1"]);

    assert!(success);
    assert!(stdout.contains("Issue B") || contains_issue_ref(&stdout, 2));
    assert!(stdout.contains("Issue C") || contains_issue_ref(&stdout, 3));
}

#[test]
fn test_multiple_labels() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Multi-label issue"]);
    run_crosslink(dir.path(), &["issue", "label", "1", "bug"]);
    run_crosslink(dir.path(), &["issue", "label", "1", "urgent"]);
    run_crosslink(dir.path(), &["issue", "label", "1", "frontend"]);

    let (success, stdout, _) = run_crosslink(dir.path(), &["show", "1"]);

    assert!(success);
    assert!(stdout.contains("bug"));
    assert!(stdout.contains("urgent"));
    assert!(stdout.contains("frontend"));
}

#[test]
fn test_export_markdown_format() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Issue for markdown"]);
    run_crosslink(dir.path(), &["issue", "label", "1", "test"]);
    run_crosslink(dir.path(), &["issue", "comment", "1", "Test comment"]);

    let export_path = dir.path().join("export.md");
    let (success, stdout, _stderr) = run_crosslink(
        dir.path(),
        &[
            "export",
            "-o",
            export_path.to_str().unwrap(),
            "-f",
            "markdown",
        ],
    );

    assert!(success);
    assert!(
        stdout.contains("Exported"),
        "Expected 'Exported' in stdout, got: {stdout}"
    );

    let content = std::fs::read_to_string(&export_path).unwrap();
    assert!(
        content.contains("Issue for markdown"),
        "Exported markdown should contain issue title, got: {}",
        &content[..content.len().min(200)]
    );
}

#[test]
fn test_archive_older_no_matches() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "New issue"]);
    run_crosslink(dir.path(), &["close", "1"]);

    let (success, stdout, _) = run_crosslink(dir.path(), &["archive", "older", "30"]);

    assert!(success);
    assert!(
        stdout.contains("No issues to archive") || stdout.contains("Archived 0"),
        "Should report no issues to archive, got: {stdout}"
    );
}

#[test]
fn test_relate_nonexistent_first_issue() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Existing"]);

    let (success, _, stderr) = run_crosslink(dir.path(), &["issue", "relate", "999", "1"]);

    assert!(!success);
    assert!(stderr.contains("not found") || stderr.contains("999"));
}

#[test]
fn test_relate_nonexistent_second_issue() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Existing"]);

    let (success, _, stderr) = run_crosslink(dir.path(), &["issue", "relate", "1", "999"]);

    assert!(!success);
    assert!(stderr.contains("not found") || stderr.contains("999"));
}

#[test]
fn test_relate_already_related() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Issue A"]);
    run_crosslink(dir.path(), &["create", "Issue B"]);
    run_crosslink(dir.path(), &["issue", "relate", "1", "2"]);

    let (success, stdout, _) = run_crosslink(dir.path(), &["issue", "relate", "1", "2"]);

    assert!(success);
    assert!(stdout.contains("already") || stdout.contains("related"));
}

#[test]
fn test_unrelate_no_relation() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Issue A"]);
    run_crosslink(dir.path(), &["create", "Issue B"]);

    let (success, stdout, _) = run_crosslink(dir.path(), &["issue", "unrelate", "1", "2"]);

    assert!(success);
    assert!(
        stdout.contains("No relation found"),
        "Expected 'No relation found' message, got: {stdout}"
    );
}

#[test]
fn test_related_no_relations() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Solo issue"]);

    let (success, stdout, _) = run_crosslink(dir.path(), &["issue", "related", "1"]);

    assert!(success);
    assert!(
        stdout.contains("No related issues"),
        "Expected 'No related issues' message, got: {stdout}"
    );
}

#[test]
fn test_related_nonexistent_issue() {
    let dir = test_dir();
    init_crosslink(dir.path());

    let (success, _, stderr) = run_crosslink(dir.path(), &["issue", "related", "999"]);

    assert!(!success);
    assert!(stderr.contains("not found") || stderr.contains("999"));
}

#[test]
fn test_label_nonexistent_issue() {
    let dir = test_dir();
    init_crosslink(dir.path());

    let (success, _, stderr) = run_crosslink(dir.path(), &["issue", "label", "999", "bug"]);

    assert!(!success);
    assert!(stderr.contains("not found") || stderr.contains("999"));
}

#[test]
fn test_label_already_exists() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Issue"]);
    run_crosslink(dir.path(), &["issue", "label", "1", "bug"]);

    let (success, stdout, _) = run_crosslink(dir.path(), &["issue", "label", "1", "bug"]);

    assert!(success);
    assert!(stdout.contains("already") || stdout.contains("exists"));
}

#[test]
fn test_unlabel_nonexistent_issue() {
    let dir = test_dir();
    init_crosslink(dir.path());

    let (success, _, stderr) = run_crosslink(dir.path(), &["issue", "unlabel", "999", "bug"]);

    assert!(!success);
    assert!(stderr.contains("not found") || stderr.contains("999"));
}

#[test]
fn test_unlabel_nonexistent_label() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Issue"]);

    let (success, stdout, _) = run_crosslink(dir.path(), &["issue", "unlabel", "1", "nonexistent"]);

    assert!(success);
    assert!(
        stdout.contains("not found"),
        "Expected 'not found' message for non-existent label, got: {stdout}"
    );
}

#[test]
fn test_create_invalid_priority() {
    let dir = test_dir();
    init_crosslink(dir.path());

    let (success, _, stderr) = run_crosslink(dir.path(), &["create", "Issue", "-p", "invalid"]);

    assert!(!success);
    assert!(
        stderr.contains("Invalid") || stderr.contains("priority") || stderr.contains("invalid")
    );
}

#[test]
fn test_create_unknown_template() {
    let dir = test_dir();
    init_crosslink(dir.path());

    let (success, _, stderr) = run_crosslink(dir.path(), &["create", "Issue", "-t", "unknown"]);

    assert!(!success);
    assert!(
        stderr.contains("Unknown") || stderr.contains("template") || stderr.contains("unknown")
    );
}

#[test]
fn test_block_self() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Issue"]);

    let (success, _, stderr) = run_crosslink(dir.path(), &["issue", "block", "1", "1"]);

    assert!(!success, "Blocking an issue by itself should fail");
    assert!(
        stderr.contains("cannot block itself"),
        "Error should mention self-blocking, got stderr: {stderr}"
    );
}

#[test]
fn test_block_nonexistent_issue() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Issue"]);

    let (success, _, stderr) = run_crosslink(dir.path(), &["issue", "block", "1", "999"]);

    assert!(!success);
    assert!(stderr.contains("not found") || stderr.contains("999"));
}

#[test]
fn test_session_status_deleted_issue() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "To delete"]);
    run_crosslink(dir.path(), &["session", "start"]);
    run_crosslink(dir.path(), &["session", "work", "1"]);
    run_crosslink(dir.path(), &["issue", "delete", "1", "-f"]);

    let (success, stdout, _) = run_crosslink(dir.path(), &["session", "status"]);

    assert!(success);

    assert!(
        stdout.contains("not found")
            || contains_issue_ref(&stdout, 1)
            || stdout.contains("Session")
    );
}

#[test]
fn test_show_with_related_issues() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Main issue"]);
    run_crosslink(dir.path(), &["create", "Related issue"]);
    run_crosslink(dir.path(), &["issue", "relate", "1", "2"]);

    let (success, stdout, _) = run_crosslink(dir.path(), &["show", "1"]);

    assert!(success);
    assert!(
        stdout.contains("Related")
            || contains_issue_ref(&stdout, 2)
            || stdout.contains("Main issue")
    );
}

#[test]
fn test_milestone_add_nonexistent_issue() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["milestone", "create", "v1.0"]);

    let (success, stdout, _) = run_crosslink(dir.path(), &["milestone", "add", "1", "999"]);

    assert!(success);
    assert!(
        stdout.contains("not found")
            || stdout.contains("999")
            || stdout.contains("Warning")
            || stdout.contains("skipping")
    );
}

#[test]
fn test_milestone_delete_nonexistent() {
    let dir = test_dir();
    init_crosslink(dir.path());

    let (success, stdout, _) = run_crosslink(dir.path(), &["milestone", "delete", "999"]);

    assert!(success);
    assert!(stdout.contains("not found") || stdout.contains("999"));
}

#[test]
fn test_stress_very_long_title() {
    let dir = test_dir();
    init_crosslink(dir.path());

    let long_title = "A".repeat(512);
    let (success, stdout, _) = run_crosslink(dir.path(), &["create", &long_title]);

    assert!(success);
    assert!(contains_issue_ref(&stdout, 1));

    let (success, _, _) = run_crosslink(dir.path(), &["list"]);
    assert!(success);

    let (success, _, _) = run_crosslink(dir.path(), &["show", "1"]);
    assert!(success);

    let too_long_title = "A".repeat(513);
    let (success, _, stderr) = run_crosslink(dir.path(), &["create", &too_long_title]);
    assert!(!success, "Should reject title exceeding 512 chars");
    assert!(stderr.contains("512"));
}

#[test]
fn test_stress_very_long_description() {
    let dir = test_dir();
    init_crosslink(dir.path());

    let long_desc = "B".repeat(5000);
    let (success, _, _) =
        run_crosslink(dir.path(), &["create", "Long desc issue", "-d", &long_desc]);

    assert!(success);

    let (success, stdout, _) = run_crosslink(dir.path(), &["show", "1"]);
    assert!(success);

    assert!(stdout.contains("BBBB"));
}

#[test]
fn test_stress_many_issues() {
    let dir = test_dir();
    init_crosslink(dir.path());

    for i in 0..100 {
        let title = format!("Issue number {i}");
        let (success, _, _) = run_crosslink(dir.path(), &["create", &title]);
        assert!(success, "Failed to create issue {i}");
    }

    let (success, stdout, _) = run_crosslink(dir.path(), &["list"]);
    assert!(success);
    assert!(stdout.contains("Issue number 99"));

    let (success, stdout, _) = run_crosslink(dir.path(), &["issue", "search", "number 50"]);
    assert!(success);
    assert!(stdout.contains("50"));
}

#[test]
fn test_stress_deep_nesting() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Level 0"]);

    for i in 1..=20 {
        let parent_id = i.to_string();
        let title = format!("Level {i}");
        let (success, _, _) = run_crosslink(dir.path(), &["subissue", &parent_id, &title]);
        assert!(success, "Failed to create subissue at level {i}");
    }

    let (success, stdout, _) = run_crosslink(dir.path(), &["issue", "tree"]);
    assert!(success);
    assert!(stdout.contains("Level 20"));
}

#[test]
fn test_security_sql_injection_title() {
    let dir = test_dir();
    init_crosslink(dir.path());

    let malicious_titles = [
        "'; DROP TABLE issues; --",
        "\" OR 1=1 --",
        "Robert'); DROP TABLE issues;--",
        "1; DELETE FROM issues WHERE 1=1; --",
        "' UNION SELECT * FROM sqlite_master --",
    ];

    for title in malicious_titles {
        let (success, _, _) = run_crosslink(dir.path(), &["create", title]);
        assert!(success, "Failed to create issue with title: {title}");
    }

    let (success, stdout, _) = run_crosslink(dir.path(), &["list"]);
    assert!(success);
    assert!(stdout.contains("DROP TABLE"));
}

#[test]
fn test_security_sql_injection_search() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Normal issue"]);

    let malicious_searches = [
        "' OR '1'='1",
        "'; DROP TABLE issues; --",
        "\" OR \"\"=\"",
        "%' OR 1=1 --",
    ];

    for query in malicious_searches {
        let (success, _, _) = run_crosslink(dir.path(), &["issue", "search", query]);

        assert!(success, "Search crashed with query: {query}");
    }

    let (success, stdout, _) = run_crosslink(dir.path(), &["list"]);
    assert!(success);
    assert!(stdout.contains("Normal issue"));
}

#[test]
fn test_security_path_traversal_export() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Test issue"]);

    let traversal_paths = [
        "../../../tmp/evil.json",
        "..\\..\\..\\tmp\\evil.json",
        "/etc/passwd",
        "C:\\Windows\\System32\\evil.json",
    ];

    for path in traversal_paths {
        let (_, _, _) = run_crosslink(dir.path(), &["export", "-o", path, "-f", "json"]);
    }
}

#[test]
fn test_security_null_bytes() {
    let dir = test_dir();
    init_crosslink(dir.path());

    let (success, _, _) = run_crosslink(dir.path(), &["create", "Test with special: \t\r"]);
    assert!(success);

    let (success, _, _) = run_crosslink(dir.path(), &["list"]);
    assert!(success);
}

#[test]
fn test_security_control_characters() {
    let dir = test_dir();
    init_crosslink(dir.path());

    let control_inputs = [
        "Line1\nLine2",
        "Tab\there",
        "Return\rhere",
        "Bell\x07sound",
        "Escape\x1b[31mred",
    ];

    for input in control_inputs {
        let (success, _, _) = run_crosslink(dir.path(), &["create", input]);
        assert!(success, "Failed with input containing control chars");
    }

    let (success, _, _) = run_crosslink(dir.path(), &["list"]);
    assert!(success);
}

#[test]
fn test_edge_empty_strings() {
    let dir = test_dir();
    init_crosslink(dir.path());

    let (success, stdout, _) = run_crosslink(dir.path(), &["create", ""]);
    if success {
        assert!(
            stdout.contains("Created issue"),
            "If success, should show created message, got: {stdout}"
        );
    }

    run_crosslink(dir.path(), &["create", "Issue"]);
    let (_, _, _) = run_crosslink(dir.path(), &["issue", "comment", "1", ""]);

    let (_, _, _) = run_crosslink(dir.path(), &["issue", "label", "1", ""]);
}

#[test]
fn test_edge_large_ids() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Test"]);

    let (success, _, stderr) = run_crosslink(dir.path(), &["show", "9223372036854775807"]);
    assert!(!success, "Show with non-existent large ID should fail");
    assert!(
        stderr.contains("not found"),
        "Error should say not found, got: {stderr}"
    );

    let (success, _, _) = run_crosslink(dir.path(), &["show", "99999999999999999999999"]);
    assert!(!success, "Show with overflow ID should fail");

    let (success, _, _) = run_crosslink(dir.path(), &["show", "-1"]);
    assert!(!success, "Show with negative ID should fail");
}

#[test]
fn test_stress_rapid_operations() {
    let dir = test_dir();
    init_crosslink(dir.path());

    for i in 0..20 {
        let title = format!("Rapid issue {i}");
        run_crosslink(dir.path(), &["create", &title]);
        let id = (i + 1).to_string();
        run_crosslink(dir.path(), &["close", &id]);
        run_crosslink(dir.path(), &["issue", "reopen", &id]);
        run_crosslink(dir.path(), &["issue", "comment", &id, "Rapid comment"]);
        run_crosslink(dir.path(), &["issue", "label", &id, "rapid"]);
    }

    let (success, stdout, _) = run_crosslink(dir.path(), &["list"]);
    assert!(success);
    assert!(stdout.contains("Rapid issue 19"));
}

#[test]
fn test_integrity_export_import_roundtrip() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(
        dir.path(),
        &["create", "Parent", "-p", "high", "-d", "Parent desc"],
    );
    run_crosslink(dir.path(), &["subissue", "1", "Child"]);
    run_crosslink(dir.path(), &["issue", "label", "1", "important"]);
    run_crosslink(dir.path(), &["issue", "comment", "1", "Test comment"]);

    let export_path = dir.path().join("backup.json");
    let (success, _, _) = run_crosslink(
        dir.path(),
        &["export", "-o", export_path.to_str().unwrap(), "-f", "json"],
    );
    assert!(success);

    let dir2 = test_dir();
    init_crosslink(dir2.path());
    std::fs::copy(&export_path, dir2.path().join("backup.json")).unwrap();

    let (success, _, _) = run_crosslink(
        dir2.path(),
        &["import", dir2.path().join("backup.json").to_str().unwrap()],
    );
    assert!(success);

    let (success, stdout, _) = run_crosslink(dir2.path(), &["show", "1"]);
    assert!(success);
    assert!(stdout.contains("Parent"));

    let (success, stdout, _) = run_crosslink(dir2.path(), &["list"]);
    assert!(success);
    assert!(stdout.contains("Child") || contains_issue_ref(&stdout, 2));
}

#[test]
fn test_unicode_arrows_in_title() {
    let dir = test_dir();
    init_crosslink(dir.path());

    let (success, _, _) = run_crosslink(
        dir.path(),
        &["create", "Add keyboard shortcuts for swiping (← →)"],
    );
    assert!(success);

    let (success, stdout, _) = run_crosslink(dir.path(), &["list"]);
    assert!(success);
    assert!(stdout.contains("←") || stdout.contains("..."));
}

#[test]
fn test_unicode_variety_in_titles() {
    let dir = test_dir();
    init_crosslink(dir.path());

    let unicode_titles = [
        "日本語タイトル",
        "中文标题测试",
        "Тест на русском языке",
        "العربية اختبار",
        "🎉 Emoji celebration 🎊🎈",
        "Mixed: Hello 世界 مرحبا мир 🌍",
        "Math: ∑∏∫∂ √∞ ≈≠≤≥",
        "Arrows: ← → ↑ ↓ ↔ ↕ ⇐ ⇒",
        "Currency: $ € £ ¥ ₹ ₽ ₿",
        "Box: ─│┌┐└┘├┤┬┴┼",
    ];

    for (i, title) in unicode_titles.iter().enumerate() {
        let (success, _, _) = run_crosslink(dir.path(), &["create", title]);
        assert!(success, "Failed to create issue with title: {title}");

        let id = (i + 1).to_string();
        let (success, _, _) = run_crosslink(dir.path(), &["show", &id]);
        assert!(
            success,
            "Failed to show issue #{} with title: {}",
            i + 1,
            title
        );
    }

    let (success, _, _) = run_crosslink(dir.path(), &["list"]);
    assert!(success);
}

#[test]
fn test_unicode_in_descriptions_and_comments() {
    let dir = test_dir();
    init_crosslink(dir.path());

    let (success, _, _) = run_crosslink(
        dir.path(),
        &[
            "create",
            "Unicode test",
            "-d",
            "Description with 日本語 and émojis 🚀",
        ],
    );
    assert!(success);

    let (success, _, _) = run_crosslink(
        dir.path(),
        &["issue", "comment", "1", "Comment: ← back, → forward, ↑ up"],
    );
    assert!(success);

    let (success, stdout, _) = run_crosslink(dir.path(), &["show", "1"]);
    assert!(success);
    assert!(
        stdout.contains("日本語"),
        "Show output should contain the Unicode description text, got: {stdout}"
    );
}

#[test]
fn test_unicode_search() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "日本語のテスト"]);
    run_crosslink(dir.path(), &["create", "Test with arrows ← →"]);
    run_crosslink(dir.path(), &["create", "Emoji test 🎉"]);

    let (success, _, _) = run_crosslink(dir.path(), &["issue", "search", "日本"]);
    assert!(success);

    let (success, _, _) = run_crosslink(dir.path(), &["issue", "search", "🎉"]);
    assert!(success);

    let (success, _, _) = run_crosslink(dir.path(), &["issue", "search", "←"]);
    assert!(success);
}

#[test]
fn test_unicode_long_string_truncation() {
    let dir = test_dir();
    init_crosslink(dir.path());

    let long_arrows = "←".repeat(60);
    let (success, _, _) = run_crosslink(dir.path(), &["create", &format!("Long: {long_arrows}")]);
    assert!(success);

    let (success, stdout, _) = run_crosslink(dir.path(), &["list"]);
    assert!(success);
    assert!(stdout.contains("...") || stdout.contains("Long:"));

    let mixed = "a←b→c↑d↓e🎉f".repeat(10);
    let (success, _, _) = run_crosslink(dir.path(), &["create", &mixed]);
    assert!(success);

    let (success, _, _) = run_crosslink(dir.path(), &["list"]);
    assert!(success);
}

#[test]
fn test_unicode_in_dependencies() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "ブロッカー (blocker) ←"]);
    run_crosslink(dir.path(), &["create", "待機中 (waiting) →"]);
    run_crosslink(dir.path(), &["issue", "block", "2", "1"]);

    let (success, _, _) = run_crosslink(dir.path(), &["issue", "blocked"]);
    assert!(success);

    let (success, _, _) = run_crosslink(dir.path(), &["issue", "ready"]);
    assert!(success);
}

#[test]
fn test_unicode_export_import_roundtrip() {
    let dir = test_dir();
    init_crosslink(dir.path());

    let unicode_title = "Test: 日本語 ← → 🎉";
    let unicode_desc = "Description: 中文 العربية Русский";

    run_crosslink(dir.path(), &["create", unicode_title, "-d", unicode_desc]);
    run_crosslink(dir.path(), &["issue", "comment", "1", "コメント (comment)"]);

    let export_path = dir.path().join("unicode_backup.json");
    let (success, _, _) = run_crosslink(
        dir.path(),
        &["export", "-o", export_path.to_str().unwrap(), "-f", "json"],
    );
    assert!(success);

    let dir2 = test_dir();
    init_crosslink(dir2.path());
    std::fs::copy(&export_path, dir2.path().join("unicode_backup.json")).unwrap();

    let (success, _, _) = run_crosslink(
        dir2.path(),
        &[
            "import",
            dir2.path().join("unicode_backup.json").to_str().unwrap(),
        ],
    );
    assert!(success);

    let (success, stdout, _) = run_crosslink(dir2.path(), &["show", "1"]);
    assert!(success);
    assert!(
        stdout.contains("日本語") || stdout.contains("Test:"),
        "Unicode should be preserved in export/import"
    );
}

#[test]
fn test_unicode_special_characters() {
    let dir = test_dir();
    init_crosslink(dir.path());

    let (success, _, _) = run_crosslink(
        dir.path(),
        &["create", "Test\u{200B}with\u{200B}zero\u{200B}width"],
    );
    assert!(success);

    let (success, _, _) = run_crosslink(
        dir.path(),
        &["create", "Test \u{202E}desrever\u{202C} normal"],
    );
    assert!(success);

    let (success, _, _) = run_crosslink(dir.path(), &["create", "Café résumé naïve"]);
    assert!(success);

    let (success, _, _) = run_crosslink(dir.path(), &["list"]);
    assert!(success);
}

#[test]
fn test_integrity_all_checks() {
    let dir = test_dir();
    init_crosslink(dir.path());
    let (success, stdout, _) = run_crosslink(dir.path(), &["integrity"]);
    assert!(success, "integrity command failed");
    assert!(stdout.contains("schema"));
    assert!(stdout.contains("Integrity:"));
}

#[test]
fn test_integrity_schema_pass() {
    let dir = test_dir();
    init_crosslink(dir.path());
    let (success, stdout, _) = run_crosslink(dir.path(), &["integrity", "schema"]);
    assert!(success);
    assert!(stdout.contains("PASS"));
}

#[test]
fn test_integrity_counters_skipped_without_sync() {
    let dir = test_dir();
    init_crosslink(dir.path());
    let (success, stdout, _) = run_crosslink(dir.path(), &["integrity", "counters"]);
    assert!(success);
    assert!(stdout.contains("SKIPPED"));
}

#[test]
fn test_integrity_locks_skipped_without_sync() {
    let dir = test_dir();
    init_crosslink(dir.path());
    let (success, stdout, _) = run_crosslink(dir.path(), &["integrity", "locks"]);
    assert!(success);
    assert!(stdout.contains("SKIPPED"));
}

#[test]
fn test_integrity_hydration_skipped_without_sync() {
    let dir = test_dir();
    init_crosslink(dir.path());
    let (success, stdout, _) = run_crosslink(dir.path(), &["integrity", "hydration"]);
    assert!(success);
    assert!(stdout.contains("SKIPPED"));
}

fn init_git_and_crosslink(dir: &std::path::Path) {
    let output = Command::new("git")
        .current_dir(dir)
        .args(["init", "-b", "main"])
        .output()
        .expect("Failed to init git repo");
    assert!(output.status.success(), "git init failed");

    let _ = Command::new("git")
        .current_dir(dir)
        .args(["config", "user.email", "test@test.com"])
        .output();
    let _ = Command::new("git")
        .current_dir(dir)
        .args(["config", "user.name", "Test"])
        .output();

    std::fs::write(dir.join("README.md"), "# test\n").unwrap();
    let _ = Command::new("git")
        .current_dir(dir)
        .args(["add", "README.md"])
        .output();
    let _ = Command::new("git")
        .current_dir(dir)
        .args(["commit", "-m", "initial", "--no-gpg-sign"])
        .output();

    init_crosslink(dir);
}

#[test]
fn test_kickoff_dry_run_prints_prompt_and_metadata() {
    let dir = test_dir();
    init_git_and_crosslink(dir.path());

    let (success, stdout, stderr) = run_crosslink(
        dir.path(),
        &["kickoff", "run", "--dry-run", "add batch retry logic"],
    );

    assert!(success, "kickoff --dry-run failed: stderr={stderr}");

    assert!(
        stdout.contains("KICKOFF: add batch retry logic"),
        "Missing KICKOFF header in output: {stdout}"
    );
    assert!(stdout.contains("Feature Description"));
    assert!(stdout.contains("add batch retry logic"));
    assert!(stdout.contains("Final Steps"));
    assert!(stdout.contains("crosslink session"));

    assert!(stdout.contains("Worktree:"));
    assert!(stdout.contains("Branch:"));
    assert!(stdout.contains("Agent:"));
}

#[test]
fn test_kickoff_run_template_interpolates_prompt() {
    let dir = test_dir();
    init_git_and_crosslink(dir.path());

    let template_path = dir.path().join("tmpl.md");
    std::fs::write(
        &template_path,
        "TEMPLATE-START desc={{description}} model={{model}} MARKER\n{{built_prompt}}\nTEMPLATE-END",
    )
    .unwrap();

    let (success, stdout, stderr) = run_crosslink(
        dir.path(),
        &[
            "kickoff",
            "run",
            "--dry-run",
            "--model",
            "sonnet",
            "--template",
            template_path.to_str().unwrap(),
            "add retry logic",
        ],
    );
    assert!(success, "kickoff run --template failed: stderr={stderr}");

    assert!(
        stdout.contains("TEMPLATE-START desc=add retry logic model=sonnet MARKER"),
        "template placeholders not interpolated: {stdout}"
    );
    assert!(
        stdout.contains("TEMPLATE-END"),
        "template tail missing: {stdout}"
    );

    assert!(
        stdout.contains("KICKOFF: add retry logic"),
        "built_prompt placeholder not expanded to the built prompt: {stdout}"
    );

    assert!(
        !stdout.contains("description}}") && !stdout.contains("built_prompt}}"),
        "raw template tokens leaked into output: {stdout}"
    );
}

#[test]
fn test_kickoff_dry_run_is_side_effect_free_and_prints_prompt() {
    let dir = test_dir();
    init_git_and_crosslink(dir.path());

    let (success, stdout, _) = run_crosslink(
        dir.path(),
        &["kickoff", "run", "--dry-run", "test file creation"],
    );
    assert!(success);

    let worktree_line = stdout
        .lines()
        .find(|l| l.starts_with("Worktree:"))
        .expect("No Worktree line in output");
    let worktree_path = worktree_line.trim_start_matches("Worktree:").trim();
    assert!(
        !std::path::Path::new(worktree_path).exists(),
        "dry run must not create the worktree, but {worktree_path} exists"
    );

    assert!(stdout.contains("test file creation"));
    assert!(
        stdout.contains("Verify agent setup"),
        "printed prompt should include agent verification step"
    );
    assert!(
        stdout.contains("crosslink agent status"),
        "printed prompt should instruct agent to check identity"
    );
    assert!(
        stdout.contains("Sync periodically"),
        "printed prompt should instruct agent to sync during work"
    );
    assert!(
        stdout.contains("Final sync"),
        "printed prompt should instruct agent to sync before ending"
    );
}

#[test]
fn test_kickoff_dry_run_does_not_launch_agent() {
    let dir = test_dir();
    init_git_and_crosslink(dir.path());

    let (success, stdout, _) = run_crosslink(
        dir.path(),
        &["kickoff", "run", "--dry-run", "no launch test"],
    );
    assert!(success);

    assert!(!stdout.contains("Feature agent launched"));
    assert!(!stdout.contains("tmux"));
    assert!(!stdout.contains("Approve trust"));
}

#[test]
fn test_knowledge_search_with_tag_filter() {
    let dir = test_dir();
    init_git_and_crosslink(dir.path());

    let (s1, _, _) = run_crosslink(
        dir.path(),
        &[
            "knowledge",
            "add",
            "design-alpha",
            "--title",
            "Alpha Design",
            "--tag",
            "design-doc",
            "--content",
            "Alpha design content with searchword",
        ],
    );
    assert!(s1, "Failed to add knowledge page alpha");

    let (s2, _, _) = run_crosslink(
        dir.path(),
        &[
            "knowledge",
            "add",
            "notes-beta",
            "--title",
            "Beta Notes",
            "--tag",
            "meeting-notes",
            "--content",
            "Beta meeting content with searchword",
        ],
    );
    assert!(s2, "Failed to add knowledge page beta");

    let (success, stdout, _) = run_crosslink(
        dir.path(),
        &["knowledge", "search", "searchword", "--tag", "design-doc"],
    );
    assert!(success);
    assert!(
        stdout.contains("design-alpha") || stdout.contains("Alpha"),
        "Tag-filtered search should find design-alpha, got: {stdout}"
    );
    assert!(
        !stdout.contains("notes-beta"),
        "Tag-filtered search should NOT find notes-beta"
    );
}

#[test]
fn test_knowledge_import_dry_run() {
    let dir = test_dir();
    init_git_and_crosslink(dir.path());

    let fixtures = dir.path().join("import-fixtures");
    std::fs::create_dir_all(&fixtures).unwrap();
    std::fs::write(fixtures.join("doc-one.md"), "# Doc One\n\nContent one.\n").unwrap();
    std::fs::write(fixtures.join("doc-two.md"), "# Doc Two\n\nContent two.\n").unwrap();

    let (success, stdout, _) = run_crosslink(
        dir.path(),
        &[
            "knowledge",
            "import",
            fixtures.to_str().unwrap(),
            "--dry-run",
        ],
    );
    assert!(success);

    assert!(
        stdout.contains("doc-one") || stdout.contains("import"),
        "Dry run should list files: {stdout}"
    );
    assert!(
        stdout.contains("doc-two") || stdout.contains("import"),
        "Dry run should list both files: {stdout}"
    );

    let (_, list_out, _) = run_crosslink(dir.path(), &["knowledge", "list"]);
    assert!(
        !list_out.contains("doc-one"),
        "Dry run should not actually import pages"
    );
}

#[test]
fn test_init_deploys_mcp_knowledge_server_integration() {
    let dir = test_dir();
    init_crosslink(dir.path());

    assert!(
        dir.path()
            .join(".crosslink/integrations/mcp/knowledge-server.py")
            .exists(),
        "provider-neutral knowledge-server.py not deployed"
    );

    let mcp_path = dir.path().join(".mcp.json");
    assert!(mcp_path.exists(), ".mcp.json not created");

    let mcp_content = std::fs::read_to_string(&mcp_path).unwrap();
    assert!(
        !mcp_content.contains("crosslink-safe-fetch"),
        ".mcp.json retained the retired downloader"
    );
    assert!(
        mcp_content.contains("crosslink-knowledge"),
        ".mcp.json missing crosslink-knowledge"
    );
}

#[test]
fn test_init_deploys_skill_files_integration() {
    let dir = test_dir();
    init_crosslink(dir.path());

    let commands_dir = dir.path().join(".claude/commands");
    assert!(
        commands_dir.join("maintain.md").exists(),
        "maintain.md not deployed"
    );
    assert!(
        commands_dir.join("design.md").exists(),
        "design.md not deployed"
    );

    let (success, _, _) = run_crosslink(dir.path(), &["init", "--force"]);
    assert!(success, "Force init failed");
    assert!(commands_dir.join("maintain.md").exists());
    assert!(commands_dir.join("design.md").exists());
}

#[test]
fn test_init_deploys_claude_skills() {
    let dir = test_dir();
    init_crosslink(dir.path());

    let skills_dir = dir.path().join(".claude/skills");
    assert!(skills_dir.is_dir(), ".claude/skills/ directory not created");

    for skill in [
        "architect",
        "commit",
        "crosslink-guide",
        "kickoff",
        "rust-quality",
        "workflow",
    ] {
        let skill_md = skills_dir.join(skill).join("SKILL.md");
        assert!(skill_md.exists(), "skill not deployed: {}/SKILL.md", skill);
        let content = std::fs::read_to_string(&skill_md).unwrap();
        assert!(
            !content.is_empty(),
            "deployed skill {}/SKILL.md is empty",
            skill
        );
    }

    let gpu_skill = skills_dir.join("rust-gpu-discipline");
    assert!(gpu_skill.join("SKILL.md").exists());
    assert!(gpu_skill.join("anti-patterns.md").exists());
    assert!(gpu_skill.join("ferrotorch-stack.md").exists());
    assert!(gpu_skill.join("verification-script.md").exists());

    let (success, _, _) = run_crosslink(dir.path(), &["init", "--force"]);
    assert!(success, "Force init failed");
    assert!(skills_dir.join("architect/SKILL.md").exists());
}

fn setup_repo_with_remote() -> (tempfile::TempDir, tempfile::TempDir) {
    let remote_dir = tempdir().unwrap();
    let work_dir = tempdir().unwrap();

    let out = Command::new("git")
        .current_dir(remote_dir.path())
        .args(["init", "--bare", "-b", "main"])
        .output()
        .unwrap();
    assert!(out.status.success(), "git init --bare failed");

    let out = Command::new("git")
        .current_dir(work_dir.path())
        .args(["init", "-b", "main"])
        .output()
        .unwrap();
    assert!(out.status.success(), "git init failed");

    for args in [
        vec!["config", "user.email", "test@test.com"],
        vec!["config", "user.name", "Test"],
        vec![
            "remote",
            "add",
            "origin",
            remote_dir.path().to_str().unwrap(),
        ],
    ] {
        let _ = Command::new("git")
            .current_dir(work_dir.path())
            .args(&args)
            .output();
    }

    std::fs::write(work_dir.path().join("README.md"), "# test\n").unwrap();
    let _ = Command::new("git")
        .current_dir(work_dir.path())
        .args(["add", "README.md"])
        .output();
    let _ = Command::new("git")
        .current_dir(work_dir.path())
        .args(["commit", "-m", "initial", "--no-gpg-sign"])
        .output();
    let out = Command::new("git")
        .current_dir(work_dir.path())
        .args(["push", "-u", "origin", "main"])
        .output()
        .unwrap();
    assert!(out.status.success(), "initial push failed");

    init_crosslink(work_dir.path());

    (work_dir, remote_dir)
}

#[test]
fn test_hub_sync_idempotent() {
    let (work_dir, _remote_dir) = setup_repo_with_remote();

    let (s1, out1, err1) = run_crosslink(work_dir.path(), &["sync"]);
    assert!(s1, "First sync failed: stdout={out1} stderr={err1}");

    let (s2, out2, err2) = run_crosslink(work_dir.path(), &["sync"]);
    assert!(
        s2,
        "Second sync failed (not idempotent): stdout={out2} stderr={err2}"
    );
}

#[test]
fn test_hub_sync_recovery_from_dirty_cache() {
    let (work_dir, _remote_dir) = setup_repo_with_remote();

    let (s, _, err) = run_crosslink(work_dir.path(), &["sync"]);
    assert!(s, "Initial sync failed: {err}");

    let hub_cache = work_dir.path().join(".crosslink/.hub-cache");
    if hub_cache.exists() {
        std::fs::write(hub_cache.join("dirty-test-file.txt"), "dirty\n").ok();
    }

    let (s2, _, err2) = run_crosslink(work_dir.path(), &["sync"]);
    assert!(s2, "Sync after dirty cache should recover: stderr={err2}");
}

#[test]
fn test_offline_sync_does_not_panic() {
    let (work_dir, _remote_dir) = setup_repo_with_remote();

    let (s, _, err) = run_crosslink(work_dir.path(), &["sync"]);
    assert!(s, "Initial sync failed: {err}");

    let _ = Command::new("git")
        .current_dir(work_dir.path())
        .args([
            "remote",
            "set-url",
            "origin",
            "/nonexistent/remote/path/that/does/not/exist",
        ])
        .output();

    let (_, stdout, stderr) = run_crosslink(work_dir.path(), &["sync"]);
    let combined = format!("{stdout}{stderr}");

    assert!(
        !combined.contains("panicked"),
        "Sync with offline remote should not panic: {combined}"
    );
}

#[test]
fn test_issue_create_canonical() {
    let dir = test_dir();
    init_crosslink(dir.path());

    let (success, stdout, stderr) =
        run_crosslink(dir.path(), &["issue", "create", "Canonical test"]);

    assert!(success, "issue create failed: {stderr}");
    assert!(
        stdout.contains("Created issue") && contains_issue_ref(&stdout, 1),
        "Expected 'Created issue #1', got: {stdout}"
    );

    assert!(
        !stderr.contains("hint:"),
        "Canonical path should not emit hint, got stderr: {stderr}"
    );
}

#[test]
fn test_issue_create_with_parent() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["issue", "create", "Parent"]);
    let (success, stdout, stderr) =
        run_crosslink(dir.path(), &["issue", "create", "Child", "--parent", "1"]);

    assert!(success, "issue create --parent failed: {stderr}");
    assert!(
        stdout.contains("Created subissue") && contains_issue_ref(&stdout, 2),
        "Expected subissue creation, got: {stdout}"
    );
}

#[test]
fn test_issue_quick_canonical() {
    let dir = test_dir();
    init_crosslink(dir.path());

    let (success, stdout, stderr) = run_crosslink(
        dir.path(),
        &[
            "issue",
            "quick",
            "Quick test",
            "-p",
            "high",
            "-l",
            "feature",
        ],
    );

    assert!(success, "issue quick failed: {stderr}");
    assert!(
        stdout.contains("Created issue") && contains_issue_ref(&stdout, 1),
        "Expected issue creation, got: {stdout}"
    );
}

#[test]
fn test_issue_list_canonical() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["issue", "create", "Test issue"]);
    let (success, stdout, stderr) = run_crosslink(dir.path(), &["issue", "list"]);

    assert!(success, "issue list failed: {stderr}");
    assert!(stdout.contains("Test issue"));
    assert!(!stderr.contains("hint:"));
}

#[test]
fn test_issue_show_canonical() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["issue", "create", "Show me"]);
    let (success, stdout, stderr) = run_crosslink(dir.path(), &["issue", "show", "1"]);

    assert!(success, "issue show failed: {stderr}");
    assert!(stdout.contains("Show me"));
    assert!(!stderr.contains("hint:"));
}

#[test]
fn test_issue_close_canonical() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["issue", "create", "Close me"]);
    let (success, stdout, stderr) = run_crosslink(dir.path(), &["issue", "close", "1"]);

    assert!(success, "issue close failed: {stderr}");
    assert!(stdout.contains("Closed") || stdout.contains("closed"));
    assert!(!stderr.contains("hint:"));
}

#[test]
fn test_issue_update_canonical() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["issue", "create", "Original"]);
    let (success, _, stderr) =
        run_crosslink(dir.path(), &["issue", "update", "1", "--title", "Updated"]);

    assert!(success, "issue update failed: {stderr}");

    let (_, show_out, _) = run_crosslink(dir.path(), &["issue", "show", "1"]);
    assert!(show_out.contains("Updated"));
}

#[test]
fn test_issue_reopen_canonical() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["issue", "create", "Reopen me"]);
    run_crosslink(dir.path(), &["issue", "close", "1"]);
    let (success, stdout, stderr) = run_crosslink(dir.path(), &["issue", "reopen", "1"]);

    assert!(success, "issue reopen failed: {stderr}");
    assert!(stdout.contains("Reopened") || stdout.contains("reopen"));
}

#[test]
fn test_issue_delete_canonical() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["issue", "create", "Delete me"]);
    let (success, _, stderr) = run_crosslink(dir.path(), &["issue", "delete", "1", "-f"]);

    assert!(success, "issue delete failed: {stderr}");

    let (_, list_out, _) = run_crosslink(dir.path(), &["issue", "list"]);
    assert!(!list_out.contains("Delete me"));
}

#[test]
fn test_issue_comment_canonical() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["issue", "create", "Comment target"]);
    let (success, _, stderr) = run_crosslink(
        dir.path(),
        &["issue", "comment", "1", "A canonical comment"],
    );

    assert!(success, "issue comment failed: {stderr}");

    let (_, show_out, _) = run_crosslink(dir.path(), &["issue", "show", "1"]);
    assert!(show_out.contains("A canonical comment"));
}

#[test]
fn test_issue_label_canonical() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["issue", "create", "Label target"]);
    let (success, _, stderr) = run_crosslink(dir.path(), &["issue", "label", "1", "bugfix"]);

    assert!(success, "issue label failed: {stderr}");

    let (_, show_out, _) = run_crosslink(dir.path(), &["issue", "show", "1"]);
    assert!(show_out.contains("bugfix"));
}

#[test]
fn test_issue_search_canonical() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["issue", "create", "Findable needle"]);
    run_crosslink(dir.path(), &["issue", "create", "Unrelated haystack"]);

    let (success, stdout, _) = run_crosslink(dir.path(), &["issue", "search", "needle"]);

    assert!(success);
    assert!(stdout.contains("Findable needle"));
    assert!(!stdout.contains("haystack"));
}

#[test]
fn test_issue_block_canonical() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["issue", "create", "Blocked"]);
    run_crosslink(dir.path(), &["issue", "create", "Blocker"]);
    let (success, _, stderr) = run_crosslink(dir.path(), &["issue", "block", "1", "2"]);

    assert!(success, "issue block failed: {stderr}");

    let (_, blocked_out, _) = run_crosslink(dir.path(), &["issue", "blocked"]);
    assert!(blocked_out.contains("Blocked"));
}

#[test]
fn test_issue_tree_canonical() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["issue", "create", "Tree parent"]);
    run_crosslink(
        dir.path(),
        &["issue", "create", "Tree child", "--parent", "1"],
    );

    let (success, stdout, _) = run_crosslink(dir.path(), &["issue", "tree"]);

    assert!(success);
    assert!(stdout.contains("Tree parent"));
    assert!(stdout.contains("Tree child"));
}

#[test]
fn test_issue_next_canonical() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["issue", "create", "Low prio", "-p", "low"]);
    run_crosslink(dir.path(), &["issue", "create", "High prio", "-p", "high"]);

    let (success, stdout, _) = run_crosslink(dir.path(), &["issue", "next"]);

    assert!(success);
    assert!(stdout.contains("High prio") || contains_issue_ref(&stdout, 2));
}

#[test]
fn test_timer_start_canonical() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["issue", "create", "Timed issue"]);
    let (success, stdout, stderr) = run_crosslink(dir.path(), &["timer", "start", "1"]);

    assert!(success, "timer start failed: {stderr}");
    assert!(
        stdout.contains("Started") || stdout.contains("timer") || contains_issue_ref(&stdout, 1)
    );
    assert!(!stderr.contains("hint:"));
}

#[test]
fn test_timer_stop_canonical() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["issue", "create", "Timed issue"]);
    run_crosslink(dir.path(), &["timer", "start", "1"]);
    let (success, stdout, stderr) = run_crosslink(dir.path(), &["timer", "stop"]);

    assert!(success, "timer stop failed: {stderr}");
    assert!(stdout.contains("Stopped") || stdout.contains("stopped") || stdout.contains("timer"));
    assert!(!stderr.contains("hint:"));
}

#[test]
fn test_timer_show_canonical() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["issue", "create", "Timed issue"]);
    run_crosslink(dir.path(), &["timer", "start", "1"]);

    let (success, stdout, _) = run_crosslink(dir.path(), &["timer", "show"]);

    assert!(success);
    assert!(
        contains_issue_ref(&stdout, 1)
            || stdout.contains("Timed issue")
            || stdout.contains("running")
    );
}

#[test]
fn test_new_alias_emits_hint() {
    let dir = test_dir();
    init_crosslink(dir.path());

    let (success, stdout, stderr) = run_crosslink_info(dir.path(), &["new", "Aliased issue"]);

    assert!(success, "new alias failed: {stderr}");
    assert!(stdout.contains("Created issue") && contains_issue_ref(&stdout, 1));
    assert!(
        stderr.contains("hint:") && stderr.contains("issue create"),
        "Expected hint about 'issue create', got stderr: {stderr}"
    );
}

#[test]
fn test_issues_alias_emits_hint() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["issue", "create", "Visible issue"]);
    let (success, stdout, stderr) = run_crosslink_info(dir.path(), &["issues"]);

    assert!(success, "issues alias failed: {stderr}");
    assert!(stdout.contains("Visible issue"));
    assert!(
        stderr.contains("hint:") && stderr.contains("issue list"),
        "Expected hint about 'issue list', got stderr: {stderr}"
    );
}

#[test]
fn test_issues_list_alias_emits_hint() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["issue", "create", "Listed issue"]);
    let (success, stdout, stderr) =
        run_crosslink_info(dir.path(), &["issues", "list", "-s", "open"]);

    assert!(success, "issues list alias failed: {stderr}");
    assert!(stdout.contains("Listed issue"));
    assert!(
        stderr.contains("hint:"),
        "Expected hint on stderr, got: {stderr}"
    );
}

#[test]
fn test_subissue_alias_emits_hint() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["issue", "create", "Parent"]);
    let (success, stdout, stderr) =
        run_crosslink_info(dir.path(), &["subissue", "1", "Child via alias"]);

    assert!(success, "subissue alias failed: {stderr}");
    assert!(stdout.contains("Created subissue") && contains_issue_ref(&stdout, 2));
    assert!(
        stderr.contains("hint:") && stderr.contains("issue create --parent"),
        "Expected hint about 'issue create --parent', got stderr: {stderr}"
    );
}

#[test]
fn test_start_alias_emits_hint() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["issue", "create", "Timer target"]);
    let (success, stdout, stderr) = run_crosslink_info(dir.path(), &["start", "1"]);

    assert!(success, "start alias failed: {stderr}");
    assert!(
        stdout.contains("Started") || stdout.contains("timer") || contains_issue_ref(&stdout, 1)
    );
    assert!(
        stderr.contains("hint:") && stderr.contains("timer start"),
        "Expected hint about 'timer start', got stderr: {stderr}"
    );
}

#[test]
fn test_stop_alias_emits_hint() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["issue", "create", "Timer target"]);
    run_crosslink(dir.path(), &["timer", "start", "1"]);
    let (success, stdout, stderr) = run_crosslink_info(dir.path(), &["stop"]);

    assert!(success, "stop alias failed: {stderr}");
    assert!(stdout.contains("Stopped") || stdout.contains("stopped") || stdout.contains("timer"));
    assert!(
        stderr.contains("hint:") && stderr.contains("timer stop"),
        "Expected hint about 'timer stop', got stderr: {stderr}"
    );
}

#[test]
fn test_top_level_create_no_hint() {
    let dir = test_dir();
    init_crosslink(dir.path());

    let (success, stdout, stderr) = run_crosslink(dir.path(), &["create", "Shortcut issue"]);

    assert!(success);
    assert!(stdout.contains("Created issue") && contains_issue_ref(&stdout, 1));
    assert!(
        !stderr.contains("hint:"),
        "Top-level 'create' shortcut should not emit hint, got stderr: {stderr}"
    );
}

#[test]
fn test_top_level_list_no_hint() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Visible"]);
    let (_, _, stderr) = run_crosslink(dir.path(), &["list"]);

    assert!(
        !stderr.contains("hint:"),
        "Top-level 'list' shortcut should not emit hint, got stderr: {stderr}"
    );
}

#[test]
fn test_top_level_show_no_hint() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Visible"]);
    let (_, _, stderr) = run_crosslink(dir.path(), &["show", "1"]);

    assert!(
        !stderr.contains("hint:"),
        "Top-level 'show' shortcut should not emit hint, got stderr: {stderr}"
    );
}

#[test]
fn test_top_level_close_no_hint() {
    let dir = test_dir();
    init_crosslink(dir.path());

    run_crosslink(dir.path(), &["create", "Close me"]);
    let (_, _, stderr) = run_crosslink(dir.path(), &["close", "1"]);

    assert!(
        !stderr.contains("hint:"),
        "Top-level 'close' shortcut should not emit hint, got stderr: {stderr}"
    );
}

#[test]
fn test_top_level_quick_no_hint() {
    let dir = test_dir();
    init_crosslink(dir.path());

    let (success, stdout, stderr) =
        run_crosslink(dir.path(), &["quick", "Quick shortcut", "-p", "high"]);

    assert!(success);
    assert!(stdout.contains("Created issue") && contains_issue_ref(&stdout, 1));
    assert!(
        !stderr.contains("hint:"),
        "Top-level 'quick' shortcut should not emit hint, got stderr: {stderr}"
    );
}

#[test]
fn test_dry_run_flag_accepted() {
    let dir = test_dir();
    init_crosslink(dir.path());

    let (_, _, stderr) = run_crosslink(dir.path(), &["style", "sync", "--dry-run"]);

    assert!(
        !stderr.contains("unexpected argument"),
        "--dry-run flag should be accepted, got stderr: {stderr}"
    );
}

#[test]
fn test_sentinel_run_disabled() {
    let dir = test_dir();
    init_crosslink(dir.path());

    let config_path = dir.path().join(".crosslink").join("hook-config.json");
    let config = serde_json::json!({
        "sentinel": { "enabled": false }
    });
    std::fs::write(&config_path, serde_json::to_string_pretty(&config).unwrap()).unwrap();

    let (success, stdout, _) = run_crosslink(dir.path(), &["sentinel", "run"]);
    assert!(success);
    assert!(
        stdout.contains("sentinel is disabled"),
        "Expected 'sentinel is disabled', got: {stdout}"
    );
}

#[test]
fn test_sentinel_run_dry_run() {
    let dir = test_dir();
    init_crosslink(dir.path());

    let config_path = dir.path().join(".crosslink").join("hook-config.json");
    let config = serde_json::json!({
        "sentinel": { "enabled": true }
    });
    std::fs::write(&config_path, serde_json::to_string_pretty(&config).unwrap()).unwrap();

    let (success, stdout, _) = run_crosslink(dir.path(), &["sentinel", "run", "--dry-run"]);
    assert!(success);
    assert!(
        stdout.contains("sentinel dry-run"),
        "Expected dry-run output, got: {stdout}"
    );
    assert!(stdout.contains("github-labels"));
    assert!(stdout.contains("max concurrent agents"));
    assert!(stdout.contains("default model"));
}

#[test]
fn test_sentinel_history_empty() {
    let dir = test_dir();
    init_crosslink(dir.path());

    let (success, stdout, _) = run_crosslink(dir.path(), &["sentinel", "history"]);
    assert!(success);
    assert!(
        stdout.contains("No sentinel runs recorded"),
        "Expected empty history message, got: {stdout}"
    );
}

#[test]
fn test_sentinel_history_json_empty() {
    let dir = test_dir();
    init_crosslink(dir.path());

    let (success, stdout, _) = run_crosslink(dir.path(), &["sentinel", "history", "--json"]);
    assert!(success);
    assert_eq!(stdout.trim(), "[]");
}

#[test]
fn test_sentinel_status_not_running() {
    let dir = test_dir();
    init_crosslink(dir.path());

    let (success, stdout, _) = run_crosslink(dir.path(), &["sentinel", "status"]);
    assert!(success);
    assert!(
        stdout.contains("Sentinel not running"),
        "Expected not running status, got: {stdout}"
    );
    assert!(stdout.contains("In-flight: 0"));
}

#[test]
fn test_sentinel_stop_not_running() {
    let dir = test_dir();
    init_crosslink(dir.path());

    let (success, stdout, _) = run_crosslink(dir.path(), &["sentinel", "stop"]);
    assert!(success);
    assert!(
        stdout.contains("not running"),
        "Expected not running, got: {stdout}"
    );
}

#[test]
fn test_sentinel_schema_migration() {
    let dir = test_dir();
    init_crosslink(dir.path());

    let db_path = dir.path().join(".crosslink").join("issues.db");
    let db = rusqlite::Connection::open(&db_path).unwrap();

    let has_runs: bool = db
        .query_row(
            "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name='sentinel_runs'",
            [],
            |row| row.get(0),
        )
        .unwrap_or(false);
    assert!(has_runs, "sentinel_runs table should exist");

    let has_dispatches: bool = db
        .query_row(
            "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name='sentinel_dispatches'",
            [],
            |row| row.get(0),
        )
        .unwrap_or(false);
    assert!(has_dispatches, "sentinel_dispatches table should exist");

    let version: i32 = db
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert!(
        version >= 16,
        "Schema version should be >= 16 (sentinel migration), got {version}"
    );
}

#[test]
fn test_kickoff_plan_permission_flags_are_exposed_and_conflict() {
    let dir = test_dir();
    init_git_and_crosslink(dir.path());
    let (success, _out, err) = run_crosslink(
        dir.path(),
        &[
            "kickoff",
            "plan",
            "does-not-matter.md",
            "--skip-permissions",
            "--permission-mode",
            "auto",
        ],
    );
    assert!(
        !success,
        "conflicting --skip-permissions + --permission-mode must be rejected"
    );
    let e = err.to_lowercase();
    assert!(
        e.contains("cannot be used with") || e.contains("conflict"),
        "expected a clap conflict error naming the two flags, got: {err}"
    );
}

#[test]
fn test_kickoff_run_rejects_unknown_effort_level() {
    let dir = test_dir();
    init_git_and_crosslink(dir.path());
    let (success, _out, err) = run_crosslink(
        dir.path(),
        &["kickoff", "run", "some feature", "--effort", "bogus"],
    );
    assert!(!success, "an unknown --effort level must be rejected");
    let e = err.to_lowercase();
    assert!(
        e.contains("invalid value") || e.contains("possible values"),
        "expected a clap value error, got: {err}"
    );
    for level in ["low", "medium", "high", "xhigh", "max"] {
        assert!(
            err.contains(level),
            "the error must name the allowed level '{level}', got: {err}"
        );
    }
}

#[test]
fn test_kickoff_plan_rejects_unknown_effort_level() {
    let dir = test_dir();
    init_git_and_crosslink(dir.path());
    let (success, _out, err) = run_crosslink(
        dir.path(),
        &["kickoff", "plan", "does-not-matter.md", "--effort", "bogus"],
    );
    assert!(!success, "an unknown --effort level must be rejected");
    let e = err.to_lowercase();
    assert!(
        e.contains("invalid value") || e.contains("possible values"),
        "expected a clap value error, got: {err}"
    );
}

#[test]
fn test_kickoff_plan_accepts_valid_effort_level() {
    let dir = test_dir();
    init_git_and_crosslink(dir.path());
    let (_success, _out, err) = run_crosslink(
        dir.path(),
        &["kickoff", "plan", "does-not-matter.md", "--effort", "xhigh"],
    );
    let e = err.to_lowercase();
    assert!(
        !e.contains("invalid value"),
        "'xhigh' is a valid effort level, got: {err}"
    );
}

#[cfg(unix)]
#[test]
fn test_design_custom_provider_receives_skill_prompt_and_propagates_exit_code() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = test_dir();
    init_crosslink(dir.path());
    let capture = dir.path().join("design-prompt.txt");
    let stub = dir.path().join("design-agent");
    std::fs::write(
        &stub,
        format!("#!/bin/sh\ncat > '{}'\nexit 37\n", capture.display()),
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&stub).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&stub, permissions).unwrap();

    let config_path = dir.path().join(".crosslink/hook-config.json");
    let mut config: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
    config["agent"] = serde_json::json!({
        "provider": "custom",
        "binary": stub,
    });
    std::fs::write(
        &config_path,
        format!("{}\n", serde_json::to_string_pretty(&config).unwrap()),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_crosslink"))
        .current_dir(dir.path())
        .env_remove("CLAUDE_CODE")
        .env_remove("CLAUDECODE")
        .env_remove("CODEX_THREAD_ID")
        .env_remove("CODEX_SESSION_ID")
        .args(["design", "provider-neutral feature"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(37));
    let prompt = std::fs::read_to_string(capture).unwrap();
    assert!(prompt.contains("ARGUMENTS: \"provider-neutral feature\""));
    assert!(prompt.contains("# Feature design"));
}

#[test]
fn test_design_detects_both_agent_environments() {
    let dir = test_dir();
    init_crosslink(dir.path());
    for (name, expected) in [
        ("CLAUDE_CODE", "Already inside Claude Code"),
        ("CODEX_THREAD_ID", "Already inside Codex"),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_crosslink"))
            .current_dir(dir.path())
            .env_remove("CLAUDE_CODE")
            .env_remove("CLAUDECODE")
            .env_remove("CODEX_THREAD_ID")
            .env_remove("CODEX_SESSION_ID")
            .env(name, "fixture")
            .args(["design", "feature"])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&output.stderr).contains(expected));
    }
}
