#![no_main]






use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use tempfile::tempdir;

use crosslink::db::Database;

#[derive(Arbitrary, Debug)]
struct UpdateInput {
    initial_title: String,
    initial_description: Option<String>,
    initial_priority: String,
    new_title: Option<String>,
    new_description: Option<String>,
    new_priority: Option<String>,
    num_updates: u8,
}

fuzz_target!(|input: UpdateInput| {
    let dir = match tempdir() {
        Ok(d) => d,
        Err(_) => return,
    };
    let db_path = dir.path().join("issues.db");

    let db = match Database::open(&db_path) {
        Ok(d) => d,
        Err(_) => return,
    };


    let issue_id = match db.create_issue(
        &input.initial_title,
        input.initial_description.as_deref(),
        &input.initial_priority,
    ) {
        Ok(id) => id,
        Err(_) => return,
    };


    let num = (input.num_updates % 10).max(1);
    for _ in 0..num {
        let _ = db.update_issue(
            issue_id,
            input.new_title.as_deref(),
            input.new_description.as_deref(),
            input.new_priority.as_deref(),
        );
    }


    let _ = db.get_issue(issue_id);


    let _ = db.update_issue(issue_id, None, None, None);


    let _ = db.update_issue(999999, Some("new title"), None, None);


    let _ = db.close_issue(issue_id);
    let _ = db.update_issue(
        issue_id,
        input.new_title.as_deref(),
        input.new_description.as_deref(),
        input.new_priority.as_deref(),
    );


    let _ = db.get_issue(issue_id);
    let _ = db.list_issues(None, None, None);
});
