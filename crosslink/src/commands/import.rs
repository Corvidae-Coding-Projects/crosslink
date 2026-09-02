use anyhow::{Context, Result};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

use super::export::ExportData;
use crate::application::CommandService;
use crate::issue_file::IssueFile;
use crate::shared_writer::{ImportedCommentSpec, ImportedIssueSpec};
use crate::utils::format_issue_id;

const MAX_IMPORT_SIZE: u64 = 10 * 1024 * 1024;

#[cfg(test)]
use crate::db::Database;

pub fn run_json(service: &impl CommandService, input_path: &Path) -> Result<()> {
    let metadata = fs::metadata(input_path).context("Failed to read import file metadata")?;
    if metadata.len() > MAX_IMPORT_SIZE {
        anyhow::bail!(
            "Import file is {} bytes, exceeding the {} byte limit",
            metadata.len(),
            MAX_IMPORT_SIZE
        );
    }
    let content = fs::read_to_string(input_path).context("Failed to read import file")?;

    let (specs, format) = if let Ok(issue_files) = serde_json::from_str::<Vec<IssueFile>>(&content)
    {
        (
            issue_files
                .iter()
                .map(spec_from_issue_file)
                .collect::<Vec<_>>(),
            "IssueFile",
        )
    } else {
        let data: ExportData = serde_json::from_str(&content).context("Failed to parse JSON")?;
        (specs_from_legacy(&data), "legacy")
    };
    println!(
        "Importing {} issues from {} ({format} format)",
        specs.len(),
        input_path.display()
    );
    let assigned = service.import_issues(&specs)?;
    for (spec, (_, new_id)) in specs.iter().zip(&assigned) {
        println!("  Imported: {} {}", format_issue_id(*new_id), spec.title);
    }
    println!("Successfully imported {} issues", assigned.len());
    Ok(())
}

fn spec_from_issue_file(issue: &IssueFile) -> ImportedIssueSpec {
    ImportedIssueSpec {
        uuid: issue.uuid,
        title: issue.title.clone(),
        description: issue.description.clone(),
        priority: issue.priority.as_str().to_string(),
        parent_uuid: issue.parent_uuid,
        closed: issue.status == crate::models::IssueStatus::Closed,
        labels: issue.labels.clone(),
        comments: issue
            .comments
            .iter()
            .map(|c| ImportedCommentSpec {
                author: c.author.clone(),
                content: c.content.clone(),
                created_at: c.created_at,
                kind: c.kind.clone(),
            })
            .collect(),
        blockers: issue.blockers.clone(),
        display_id: None,
    }
}

fn specs_from_legacy(data: &ExportData) -> Vec<ImportedIssueSpec> {
    let old_id_to_uuid: HashMap<i64, uuid::Uuid> = data
        .issues
        .iter()
        .map(|i| (i.id, uuid::Uuid::new_v4()))
        .collect();

    data.issues
        .iter()
        .map(|issue| ImportedIssueSpec {
            uuid: old_id_to_uuid[&issue.id],
            title: issue.title.clone(),
            description: issue.description.clone(),
            priority: issue.priority.clone(),
            parent_uuid: issue
                .parent_id
                .and_then(|pid| old_id_to_uuid.get(&pid).copied()),
            closed: issue.status == "closed",
            labels: issue.labels.clone(),
            comments: issue
                .comments
                .iter()
                .map(|c| ImportedCommentSpec {
                    author: "import".to_string(),
                    content: c.content.clone(),
                    created_at: c
                        .created_at
                        .parse::<chrono::DateTime<chrono::Utc>>()
                        .unwrap_or_else(|_| chrono::Utc::now()),
                    kind: "note".to_string(),
                })
                .collect(),
            blockers: Vec::new(),
            display_id: None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::export::{ExportData, ExportedIssue};
    use super::*;
    use chrono::Utc;
    use proptest::prelude::*;

    fn setup_test_db() -> (Database, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = Database::open(&db_path).unwrap();
        (db, dir)
    }

    fn create_test_export(issues: Vec<ExportedIssue>) -> String {
        let data = ExportData {
            version: 1,
            exported_at: "2024-01-01T00:00:00Z".to_string(),
            issues,
        };
        serde_json::to_string_pretty(&data).unwrap()
    }

    fn make_issue(id: i64, title: &str, parent_id: Option<i64>, status: &str) -> ExportedIssue {
        ExportedIssue {
            id,
            title: title.to_string(),
            description: None,
            status: status.to_string(),
            priority: "medium".to_string(),
            parent_id,
            labels: vec![],
            comments: vec![],
            created_at: "2024-01-01T00:00:00Z".to_string(),
            updated_at: "2024-01-01T00:00:00Z".to_string(),
            closed_at: None,
        }
    }

    #[test]
    fn test_import_single_issue() {
        let (db, dir) = setup_test_db();
        let json = create_test_export(vec![make_issue(1, "Test issue", None, "open")]);
        let import_path = dir.path().join("import.json");
        fs::write(&import_path, json).unwrap();
        let result = run_json(&db, &import_path);
        assert!(result.is_ok());
        let issues = db.list_issues(Some("all"), None, None).unwrap();
        assert_eq!(issues.len(), 1);
    }

    #[test]
    fn test_import_multiple_issues() {
        let (db, dir) = setup_test_db();
        let json = create_test_export(vec![
            make_issue(1, "Issue 1", None, "open"),
            make_issue(2, "Issue 2", None, "open"),
        ]);
        let import_path = dir.path().join("import.json");
        fs::write(&import_path, json).unwrap();
        run_json(&db, &import_path).unwrap();
        let issues = db.list_issues(Some("all"), None, None).unwrap();
        assert_eq!(issues.len(), 2);
    }

    #[test]
    fn test_import_closed_issue() {
        let (db, dir) = setup_test_db();
        let json = create_test_export(vec![make_issue(1, "Closed", None, "closed")]);
        let import_path = dir.path().join("import.json");
        fs::write(&import_path, json).unwrap();
        run_json(&db, &import_path).unwrap();
        let issues = db.list_issues(Some("closed"), None, None).unwrap();
        assert_eq!(issues.len(), 1);
    }

    #[test]
    fn test_import_with_labels() {
        let (db, dir) = setup_test_db();
        let mut issue = make_issue(1, "Labeled", None, "open");
        issue.labels = vec!["bug".to_string()];
        let json = create_test_export(vec![issue]);
        let import_path = dir.path().join("import.json");
        fs::write(&import_path, json).unwrap();
        run_json(&db, &import_path).unwrap();
        let issues = db.list_issues(Some("all"), None, None).unwrap();
        let labels = db.get_labels(issues[0].id).unwrap();
        assert!(labels.contains(&"bug".to_string()));
    }

    #[test]
    fn test_import_invalid_json() {
        let (db, dir) = setup_test_db();
        let import_path = dir.path().join("invalid.json");
        fs::write(&import_path, "not valid json").unwrap();
        let result = run_json(&db, &import_path);
        assert!(result.is_err());
    }

    #[test]
    fn test_import_missing_file() {
        let (db, dir) = setup_test_db();
        let import_path = dir.path().join("nonexistent.json");
        let result = run_json(&db, &import_path);
        assert!(result.is_err());
    }

    #[test]
    fn test_import_empty_issues() {
        let (db, dir) = setup_test_db();
        let json = create_test_export(vec![]);
        let import_path = dir.path().join("import.json");
        fs::write(&import_path, json).unwrap();
        let result = run_json(&db, &import_path);
        assert!(result.is_ok());
    }

    #[test]
    fn test_import_issue_file_format() {
        let (db, dir) = setup_test_db();
        let issue = IssueFile {
            uuid: uuid::Uuid::new_v4(),
            display_id: Some(1),
            title: "New format issue".to_string(),
            description: Some("Imported from IssueFile".to_string()),
            status: crate::models::IssueStatus::Open,
            priority: crate::models::Priority::High,
            parent_uuid: None,
            created_by: "test".to_string(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            closed_at: None,
            scheduled_at: None,
            due_at: None,
            labels: vec!["feature".to_string()],
            comments: vec![],
            blockers: vec![],
            related: vec![],
            milestone_uuid: None,
            time_entries: vec![],
        };
        let json = serde_json::to_string_pretty(&vec![issue]).unwrap();
        let import_path = dir.path().join("import.json");
        fs::write(&import_path, &json).unwrap();
        run_json(&db, &import_path).unwrap();
        let issues = db.list_issues(Some("all"), None, None).unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].title, "New format issue");
        let labels = db.get_labels(issues[0].id).unwrap();
        assert!(labels.contains(&"feature".to_string()));
    }

    proptest! {
        #[test]
        fn prop_import_never_panics(title in "[a-zA-Z0-9 ]{1,50}") {
            let (db, dir) = setup_test_db();
            let json = create_test_export(vec![make_issue(1, &title, None, "open")]);
            let import_path = dir.path().join("import.json");
            fs::write(&import_path, json).unwrap();
            let result = run_json(&db, &import_path);
            prop_assert!(result.is_ok());
        }
    }
}
