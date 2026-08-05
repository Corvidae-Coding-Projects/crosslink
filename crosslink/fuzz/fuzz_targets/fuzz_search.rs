#![no_main]

use libfuzzer_sys::fuzz_target;
use tempfile::tempdir;

use crosslink::db::Database;

fuzz_target!(|query: String| {
    let dir = match tempdir() {
        Ok(d) => d,
        Err(_) => return,
    };
    let db_path = dir.path().join("issues.db");

    let db = match Database::open(&db_path) {
        Ok(d) => d,
        Err(_) => return,
    };

    // Create some test data
    let _ = db.create_issue("Test issue one", Some("Description here"), "medium");
    let _ = db.create_issue("Another test", None, "high");
    let _ = db.create_issue("Third issue", Some("More content"), "low");

    // Fuzz search - should never panic, even with malicious SQL
    let _ = db.search_issues(&query);
});
