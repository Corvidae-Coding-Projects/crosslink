#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use tempfile::tempdir;

use crosslink::db::Database;

#[derive(Arbitrary, Debug, Clone)]
enum DependencyOp {
    CreateIssue { title: String },
    AddDependency { blocked_idx: usize, blocker_idx: usize },
    RemoveDependency { blocked_idx: usize, blocker_idx: usize },
    CloseIssue { idx: usize },
    ReopenIssue { idx: usize },
    CheckReady,
    CheckBlocked,
}

#[derive(Arbitrary, Debug)]
struct DependencyGraphInput {
    ops: Vec<DependencyOp>,
}

fuzz_target!(|input: DependencyGraphInput| {
    let dir = match tempdir() {
        Ok(d) => d,
        Err(_) => return,
    };
    let db_path = dir.path().join("issues.db");

    let db = match Database::open(&db_path) {
        Ok(d) => d,
        Err(_) => return,
    };

    // Track created issue IDs
    let mut issue_ids: Vec<i64> = Vec::new();

    for op in input.ops.iter().take(100) {
        // Limit operations to prevent timeout
        match op {
            DependencyOp::CreateIssue { title } => {
                if let Ok(id) = db.create_issue(title, None, "medium") {
                    issue_ids.push(id);
                }
            }
            DependencyOp::AddDependency {
                blocked_idx,
                blocker_idx,
            } => {
                if issue_ids.len() >= 2 {
                    let blocked = issue_ids[*blocked_idx % issue_ids.len()];
                    let blocker = issue_ids[*blocker_idx % issue_ids.len()];
                    // This should never panic, even with cycles or self-blocks
                    let _ = db.add_dependency(blocked, blocker);
                }
            }
            DependencyOp::RemoveDependency {
                blocked_idx,
                blocker_idx,
            } => {
                if issue_ids.len() >= 2 {
                    let blocked = issue_ids[*blocked_idx % issue_ids.len()];
                    let blocker = issue_ids[*blocker_idx % issue_ids.len()];
                    let _ = db.remove_dependency(blocked, blocker);
                }
            }
            DependencyOp::CloseIssue { idx } => {
                if !issue_ids.is_empty() {
                    let id = issue_ids[*idx % issue_ids.len()];
                    let _ = db.close_issue(id);
                }
            }
            DependencyOp::ReopenIssue { idx } => {
                if !issue_ids.is_empty() {
                    let id = issue_ids[*idx % issue_ids.len()];
                    let _ = db.reopen_issue(id);
                }
            }
            DependencyOp::CheckReady => {
                // Should never panic or hang
                let _ = db.list_ready_issues();
            }
            DependencyOp::CheckBlocked => {
                // Should never panic or hang
                let _ = db.list_blocked_issues();
            }
        }
    }

    // Final verification - these should never panic
    let _ = db.list_ready_issues();
    let _ = db.list_blocked_issues();
    let _ = db.list_issues(None, None, None);
});
