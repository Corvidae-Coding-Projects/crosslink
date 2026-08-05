//! Unit tests for `hydrate_from_state` (754a PASS 2 — hub-version-routed
//! operation). These live in the lib test tree because they only need the
//! library surface (`Database`, `CheckpointState`, `hydration`). The full
//! end-to-end V3 lifecycle / two-writer / lock / fetch / heartbeat / request
//! tests live in the bin test tree (`commands::hub_v3_operation_tests`) because
//! they drive the `migrate hub-v3` command, which is bin-only.

#![cfg(test)]

use std::path::Path;

use crate::db::Database;

#[test]
fn hydrate_from_state_empty_is_data_loss_guard() {
    // An empty state must NOT clear a populated SQLite (data-loss guard).
    let db = Database::open(Path::new(":memory:")).unwrap();
    let id = db.create_issue("keep me", None, "medium").unwrap();
    let empty = crate::checkpoint::CheckpointState::default();
    let stats = crate::hydration::hydrate_from_state(&empty, &db).unwrap();
    assert_eq!(stats.issues, 0, "empty state hydrates nothing");
    assert!(
        db.get_issue(id).unwrap().is_some(),
        "empty state must not wipe existing SQLite issues"
    );
}

#[test]
fn hydrate_from_state_preserves_sqlite_only_issue() {
    // #443 analogue: a direct-SQLite issue (created_by NULL) absent from the
    // reduced state survives hydration.
    let db = Database::open(Path::new(":memory:")).unwrap();
    let kept = db.create_issue("sqlite only", None, "low").unwrap();

    let mut state = crate::checkpoint::CheckpointState::default();
    let uuid = uuid::Uuid::new_v4();
    state.display_id_map.insert(uuid, 1);
    state
        .issues
        .insert(uuid, sample_compact_issue(uuid, 1, "from state"));

    crate::hydration::hydrate_from_state(&state, &db).unwrap();
    assert!(
        db.get_issue(kept).unwrap().is_some(),
        "SQLite-only issue must be preserved across hydrate_from_state"
    );
    assert!(
        db.get_issue(1).unwrap().is_some(),
        "state issue must be hydrated"
    );
}

#[test]
fn hydrate_from_state_maps_issue_children() {
    // A state issue with a label, comment, blocker, and milestone link
    // hydrates each child table row.
    let db = Database::open(Path::new(":memory:")).unwrap();

    let mut state = crate::checkpoint::CheckpointState::default();

    // Milestone (id 7) referenced by the issue.
    let ms_uuid = uuid::Uuid::new_v4();
    state.milestones.insert(
        ms_uuid,
        crate::checkpoint::CompactMilestone {
            uuid: ms_uuid,
            display_id: Some(7),
            name: "m1".to_string(),
            description: None,
            status: crate::models::IssueStatus::Open,
            created_at: chrono::Utc::now(),
            closed_at: None,
        },
    );

    // Blocker issue (id 2).
    let blocker_uuid = uuid::Uuid::new_v4();
    state.display_id_map.insert(blocker_uuid, 2);
    state.issues.insert(
        blocker_uuid,
        sample_compact_issue(blocker_uuid, 2, "blocker"),
    );

    // Main issue (id 1) with a label, comment, blocker, and milestone link.
    let uuid = uuid::Uuid::new_v4();
    state.display_id_map.insert(uuid, 1);
    let mut issue = sample_compact_issue(uuid, 1, "main");
    issue.labels.insert("bug".to_string());
    issue.blockers.insert(blocker_uuid);
    issue.milestone_uuid = Some(ms_uuid);
    let comment_uuid = uuid::Uuid::new_v4();
    issue.comments.insert(
        comment_uuid,
        crate::checkpoint::CompactComment {
            display_id: Some(5),
            author: "alpha".to_string(),
            content: "hello".to_string(),
            created_at: chrono::Utc::now(),
            kind: "note".to_string(),
            trigger_type: None,
            intervention_context: None,
            driver_key_fingerprint: None,
            signed_by: None,
            signature: None,
        },
    );
    state.issues.insert(uuid, issue);

    let stats = crate::hydration::hydrate_from_state(&state, &db).unwrap();
    assert_eq!(stats.issues, 2);
    assert_eq!(stats.milestones, 1);
    assert_eq!(stats.comments, 1);
    assert_eq!(stats.dependencies, 1);

    assert!(db.get_labels(1).unwrap().iter().any(|l| l == "bug"));
    assert!(!db.get_comments(1).unwrap().is_empty());
}

#[test]
fn hydrate_from_state_remaps_local_only_issue_colliding_with_hub_display_id() {
    // GH#5: a local-only issue (e.g. from a legacy `import`) occupying id 1
    // must not silently REPLACE the hub issue the reduction assigned display
    // id 1 — the hub issue keeps the id, the local-only issue is remapped to
    // a negative local id, and both survive with their children.
    let db = Database::open(Path::new(":memory:")).unwrap();
    let local = db.create_issue("local only", None, "low").unwrap();
    assert_eq!(
        local, 1,
        "test precondition: local-only issue occupies id 1"
    );
    db.add_label(local, "keep").unwrap();
    db.add_comment(local, "local note", "note").unwrap();

    let mut state = crate::checkpoint::CheckpointState::default();
    let uuid = uuid::Uuid::new_v4();
    state.display_id_map.insert(uuid, 1);
    state
        .issues
        .insert(uuid, sample_compact_issue(uuid, 1, "from hub"));

    crate::hydration::hydrate_from_state(&state, &db).unwrap();

    let hub = db.get_issue(1).unwrap().expect("hub issue at id 1");
    assert_eq!(hub.title, "from hub", "hub issue must not be shadowed");

    let issues = db.list_issues(Some("all"), None, None).unwrap();
    let local_row = issues
        .iter()
        .find(|i| i.title == "local only")
        .expect("colliding local-only issue must survive hydration");
    assert!(
        local_row.id < 0,
        "colliding local-only issue is remapped to a negative id, got {}",
        local_row.id
    );
    assert!(
        db.get_labels(local_row.id)
            .unwrap()
            .iter()
            .any(|l| l == "keep"),
        "labels follow the remapped issue"
    );
    assert!(
        !db.get_comments(local_row.id).unwrap().is_empty(),
        "comments follow the remapped issue"
    );
}

#[test]
fn hydrate_from_state_remap_is_stable_across_repeated_hydrations() {
    // After the first pass remaps a colliding local-only issue to a negative
    // id, subsequent hydrations must keep both issues without error.
    let db = Database::open(Path::new(":memory:")).unwrap();
    db.create_issue("local only", None, "low").unwrap();

    let mut state = crate::checkpoint::CheckpointState::default();
    let uuid = uuid::Uuid::new_v4();
    state.display_id_map.insert(uuid, 1);
    state
        .issues
        .insert(uuid, sample_compact_issue(uuid, 1, "from hub"));

    crate::hydration::hydrate_from_state(&state, &db).unwrap();
    crate::hydration::hydrate_from_state(&state, &db).unwrap();

    let issues = db.list_issues(Some("all"), None, None).unwrap();
    assert_eq!(issues.len(), 2, "both issues survive repeated hydration");
    assert_eq!(
        db.get_issue(1).unwrap().expect("hub issue").title,
        "from hub"
    );
}

fn sample_compact_issue(
    uuid: uuid::Uuid,
    display_id: i64,
    title: &str,
) -> crate::checkpoint::CompactIssue {
    crate::checkpoint::CompactIssue {
        uuid,
        display_id: Some(display_id),
        title: title.to_string(),
        description: None,
        status: crate::models::IssueStatus::Open,
        priority: crate::models::Priority::Medium,
        parent_uuid: None,
        created_by: "alpha".to_string(),
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        closed_at: None,
        scheduled_at: None,
        due_at: None,
        labels: Default::default(),
        blockers: Default::default(),
        related: Default::default(),
        milestone_uuid: None,
        comments: Default::default(),
        time_entries: Default::default(),
    }
}

#[test]
fn hydrate_from_state_rekeys_preserved_comment_colliding_with_frozen_comment_id() {
    // GH#11: a preserved sqlite-only issue carries a comment at a positive
    // v2-era id; the state hydrates a hub comment at the SAME frozen display
    // id. insert_hydrated_comment is a plain INSERT, so before the re-keying
    // this aborted all of hydrate_from_state with
    // SQLITE_CONSTRAINT_PRIMARYKEY (error 1555) - the post-migrate sync
    // failure. Both comments must survive.
    let db = Database::open(Path::new(":memory:")).unwrap();
    let local = db.create_issue("local with comment", None, "low").unwrap();
    let local_comment = db.add_comment(local, "local words", "note").unwrap();
    assert!(local_comment > 0, "v2-era comment ids are positive");

    let mut state = crate::checkpoint::CheckpointState::default();
    let uuid = uuid::Uuid::new_v4();
    let hub_id = local + 1; // no issue-id collision: isolate the comment one
    state.display_id_map.insert(uuid, hub_id);
    let mut issue = sample_compact_issue(uuid, hub_id, "hub issue");
    issue.comments.insert(
        uuid::Uuid::new_v4(),
        crate::checkpoint::CompactComment {
            display_id: Some(local_comment),
            author: "alpha".to_string(),
            content: "hub words".to_string(),
            created_at: chrono::Utc::now(),
            kind: "note".to_string(),
            trigger_type: None,
            intervention_context: None,
            driver_key_fingerprint: None,
            signed_by: None,
            signature: None,
        },
    );
    state.issues.insert(uuid, issue);

    let stats = crate::hydration::hydrate_from_state(&state, &db).unwrap();
    assert_eq!(stats.issues, 2, "hub issue plus preserved local issue");

    let hub_comments = db.get_comments(hub_id).unwrap();
    assert_eq!(hub_comments.len(), 1);
    assert_eq!(hub_comments[0].content, "hub words");
    assert_eq!(
        hub_comments[0].id, local_comment,
        "hub comment keeps its frozen display id"
    );

    let local_comments = db.get_comments(local).unwrap();
    assert_eq!(local_comments.len(), 1, "preserved comment survives");
    assert_eq!(local_comments[0].content, "local words");
    assert!(
        local_comments[0].id < 0,
        "preserved comment re-keyed to a negative local id"
    );
}
