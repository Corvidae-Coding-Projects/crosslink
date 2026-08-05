#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use tempfile::tempdir;

use crosslink::db::Database;

#[derive(Arbitrary, Debug)]
struct CreateIssueInput {
    title: String,
    description: Option<String>,
    priority: String,
}

fuzz_target!(|input: CreateIssueInput| {
    let dir = match tempdir() {
        Ok(d) => d,
        Err(_) => return,
    };
    let db_path = dir.path().join("issues.db");

    let db = match Database::open(&db_path) {
        Ok(d) => d,
        Err(_) => return,
    };

    // Fuzz issue creation - should never panic
    let _ = db.create_issue(&input.title, input.description.as_deref(), &input.priority);

    // If creation succeeded, try other operations
    if let Ok(id) = db.create_issue(&input.title, input.description.as_deref(), &input.priority) {
        let _ = db.get_issue(id);
        let _ = db.close_issue(id);
        let _ = db.reopen_issue(id);
        let _ = db.list_issues(None, None, None);
    }
});
