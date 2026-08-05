#![no_main]

use libfuzzer_sys::fuzz_target;
use tempfile::tempdir;
use std::io::Write;

use crosslink::db::Database;

fuzz_target!(|data: &[u8]| {
    let dir = match tempdir() {
        Ok(d) => d,
        Err(_) => return,
    };
    let db_path = dir.path().join("issues.db");
    let import_path = dir.path().join("import.json");

    // Write fuzz data as import file
    let mut file = match std::fs::File::create(&import_path) {
        Ok(f) => f,
        Err(_) => return,
    };
    if file.write_all(data).is_err() {
        return;
    }
    drop(file);

    let db = match Database::open(&db_path) {
        Ok(d) => d,
        Err(_) => return,
    };

    // Try to parse the data as JSON and import
    // This tests robustness against malformed import files
    if let Ok(content) = std::fs::read_to_string(&import_path) {
        // Try parsing as our export format
        #[derive(serde::Deserialize)]
        struct ExportData {
            issues: Vec<serde_json::Value>,
        }

        if let Ok(export_data) = serde_json::from_str::<ExportData>(&content) {
            // Try to create issues from the parsed data
            for issue in export_data.issues {
                if let Some(title) = issue.get("title").and_then(|t| t.as_str()) {
                    let desc = issue.get("description").and_then(|d| d.as_str());
                    let priority = issue
                        .get("priority")
                        .and_then(|p| p.as_str())
                        .unwrap_or("medium");
                    let _ = db.create_issue(title, desc, priority);
                }
            }
        }
    }

    // Verify database is still functional after import attempt
    let _ = db.list_issues(None, None, None);
});
