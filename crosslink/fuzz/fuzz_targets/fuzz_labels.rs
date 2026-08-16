#![no_main]







use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use tempfile::tempdir;

use crosslink::db::Database;

#[derive(Arbitrary, Debug)]
struct LabelInput {
    labels: Vec<String>,
    remove_indices: Vec<u8>,
}

fuzz_target!(|input: LabelInput| {
    let dir = match tempdir() {
        Ok(d) => d,
        Err(_) => return,
    };
    let db_path = dir.path().join("issues.db");

    let db = match Database::open(&db_path) {
        Ok(d) => d,
        Err(_) => return,
    };

    let issue_id = match db.create_issue("Fuzz label test", None, "medium") {
        Ok(id) => id,
        Err(_) => return,
    };


    let labels: Vec<&String> = input.labels.iter().take(20).collect();
    for label in &labels {
        let _ = db.add_label(issue_id, label);
    }


    let _ = db.get_labels(issue_id);


    for idx in input.remove_indices.iter().take(10) {
        if !labels.is_empty() {
            let label = &labels[(*idx as usize) % labels.len()];
            let _ = db.remove_label(issue_id, label);
        }
    }


    let _ = db.get_labels(issue_id);


    if let Some(label) = labels.first() {
        let _ = db.add_label(issue_id, label);
        let _ = db.add_label(issue_id, label);
    }


    if let Some(label) = labels.first() {
        let _ = db.add_label(999999, label);
        let _ = db.remove_label(999999, label);
    }
    let _ = db.get_labels(999999);


    if let Some(label) = labels.first() {
        let label_str: &str = label;
        let _ = db.list_issues(None, Some(label_str), None);
    }
});
