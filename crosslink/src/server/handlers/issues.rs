use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::Json,
};
use serde_json::{json, Value};

use crate::application::{CommandService, QueryService, RepositoryService};
use crate::server::{
    errors::{bad_request, internal_error, not_found},
    state::AppState,
    types::{
        AddBlockerRequest, AddLabelRequest, ApiError, CreateCommentRequest, CreateIssueRequest,
        CreateSubissueRequest, IssueDetail, IssueListQuery, IssueListResponse, IssueSummary,
        OkResponse, UpdateIssueRequest, WsIssueUpdatedEvent,
    },
    ws::WsEvent,
};

fn broadcast_issue_updated(state: &AppState, issue_id: i64, field: &str) {
    let _ = state.ws_tx.send(WsEvent::IssueUpdated(WsIssueUpdatedEvent {
        event_type: crate::server::types::WsEventType::IssueUpdated,
        issue_id,
        field: field.to_string(),
    }));
}

pub(crate) fn execute_create_issue_command(
    service: &impl CommandService,
    body: &CreateIssueRequest,
) -> anyhow::Result<i64> {
    let priority = body.priority.to_string();
    body.parent_id.map_or_else(
        || {
            service.create_issue(
                &body.title,
                body.description.as_deref(),
                &priority,
                None,
                None,
            )
        },
        |parent_id| {
            service.create_subissue(
                parent_id,
                &body.title,
                body.description.as_deref(),
                &priority,
            )
        },
    )
}

pub(crate) fn execute_update_issue_command(
    service: &impl CommandService,
    id: i64,
    body: UpdateIssueRequest,
) -> anyhow::Result<()> {
    let priority = body.priority.as_ref().map(std::string::ToString::to_string);
    service.update_issue(
        id,
        crate::application::OwnedIssueUpdate {
            title: body.title,
            description: body.description.map_or(
                crate::application::DescriptionChange::Unchanged,
                crate::application::DescriptionChange::Set,
            ),
            status: None,
            priority,
            scheduled_at: crate::application::DateTimeChange::Unchanged,
            due_at: crate::application::DateTimeChange::Unchanged,
        },
    )
}

pub(crate) fn execute_create_subissue_command(
    service: &impl CommandService,
    parent_id: i64,
    body: &CreateSubissueRequest,
) -> anyhow::Result<i64> {
    service.create_subissue(
        parent_id,
        &body.title,
        body.description.as_deref(),
        &body.priority.to_string(),
    )
}

pub(crate) fn execute_delete_issue_command(
    service: &impl CommandService,
    id: i64,
) -> anyhow::Result<()> {
    service.delete_issue(id)
}

pub(crate) fn execute_close_issue_command(
    service: &impl CommandService,
    id: i64,
) -> anyhow::Result<()> {
    service.close_issue(id)
}

pub(crate) fn execute_reopen_issue_command(
    service: &impl CommandService,
    id: i64,
) -> anyhow::Result<()> {
    service.reopen_issue(id)
}

pub(crate) fn execute_add_comment_command(
    service: &impl CommandService,
    id: i64,
    body: &CreateCommentRequest,
) -> anyhow::Result<i64> {
    if body.kind == crate::server::types::CommentKind::Intervention {
        let trigger = body
            .trigger_type
            .as_deref()
            .filter(|trigger| !trigger.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!("trigger_type is required when comment kind is 'intervention'")
            })?;
        service.add_intervention(
            id,
            &body.content,
            trigger,
            body.intervention_context.as_deref(),
            None,
        )
    } else {
        service.add_comment(id, &body.content, &body.kind.to_string())
    }
}

pub(crate) fn execute_add_label_command(
    service: &impl CommandService,
    id: i64,
    label: &str,
) -> anyhow::Result<bool> {
    service.add_label(id, label)
}

pub(crate) fn execute_remove_label_command(
    service: &impl CommandService,
    id: i64,
    label: &str,
) -> anyhow::Result<bool> {
    service.remove_label(id, label)
}

pub(crate) fn execute_add_blocker_command(
    service: &impl CommandService,
    id: i64,
    blocker_id: i64,
) -> anyhow::Result<bool> {
    service.add_dependency(id, blocker_id)
}

pub(crate) fn execute_remove_blocker_command(
    service: &impl CommandService,
    id: i64,
    blocker_id: i64,
) -> anyhow::Result<bool> {
    service.remove_dependency(id, blocker_id)
}

pub async fn list_issues(
    State(state): State<AppState>,
    Query(params): Query<IssueListQuery>,
) -> Result<Json<IssueListResponse>, (StatusCode, Json<ApiError>)> {
    let db = state.db().await;
    let service = RepositoryService::projection(&db);

    let issues = if let Some(ref search) = params.search {
        let mut results = service
            .search_issues(search)
            .map_err(|e| internal_error("Failed to search issues", e))?;

        if let Some(ref status) = params.status {
            if status != "all" {
                if let Ok(s) = status.parse::<crate::models::IssueStatus>() {
                    results.retain(|i| i.status == s);
                }
            }
        }
        if let Some(ref label) = params.label {
            let ids_with_label: Vec<i64> = results
                .iter()
                .filter_map(|i| {
                    service
                        .get_labels(i.id)
                        .ok()
                        .filter(|labels| labels.contains(label))
                        .map(|_| i.id)
                })
                .collect();
            results.retain(|i| ids_with_label.contains(&i.id));
        }
        if let Some(ref priority) = params.priority {
            if let Ok(p) = priority.parse::<crate::models::Priority>() {
                results.retain(|i| i.priority == p);
            }
        }
        if let Some(parent_id) = params.parent_id {
            results.retain(|i| i.parent_id == Some(parent_id));
        }
        results
    } else {
        let mut results = service
            .list_issues(
                params.status.as_deref(),
                params.label.as_deref(),
                params.priority.as_deref(),
            )
            .map_err(|e| internal_error("Failed to list issues", e))?;

        if let Some(parent_id) = params.parent_id {
            results.retain(|i| i.parent_id == Some(parent_id));
        }
        results
    };

    let issue_ids: Vec<i64> = issues.iter().map(|i| i.id).collect();
    let labels_map = service.get_labels_batch(&issue_ids).unwrap_or_default();
    let blocker_counts = service
        .get_blocker_counts_batch(&issue_ids)
        .unwrap_or_default();
    drop(db);

    let mut items: Vec<IssueSummary> = Vec::with_capacity(issues.len());
    for issue in issues {
        let labels = labels_map.get(&issue.id).cloned().unwrap_or_default();
        let blocker_count = blocker_counts.get(&issue.id).copied().unwrap_or(0);
        items.push(IssueSummary {
            issue,
            labels,
            blocker_count,
        });
    }

    let total = items.len();
    Ok(Json(IssueListResponse { items, total }))
}

pub async fn create_issue(
    State(state): State<AppState>,
    Json(body): Json<CreateIssueRequest>,
) -> Result<Json<Value>, (StatusCode, Json<ApiError>)> {
    let db = state.db().await;
    let service = RepositoryService::new(&db, &state.crosslink_dir)
        .map_err(|error| internal_error("Shared authority is unavailable", error))?;

    let id =
        execute_create_issue_command(&service, &body).map_err(|e| bad_request(e.to_string()))?;

    let issue = service
        .get_issue(id)
        .map_err(|e| internal_error("Failed to retrieve created issue", e))?
        .ok_or_else(|| internal_error("Issue was created but not found", "unexpected state"))?;
    drop(db);

    broadcast_issue_updated(&state, id, "created");
    Ok(Json(json!(issue)))
}

pub async fn list_blocked(
    State(state): State<AppState>,
) -> Result<Json<Value>, (StatusCode, Json<ApiError>)> {
    let db = state.db().await;
    let service = RepositoryService::projection(&db);

    let issues = service
        .list_blocked_issues()
        .map_err(|e| internal_error("Failed to list blocked issues", e))?;
    drop(db);

    let total = issues.len();
    Ok(Json(json!({ "items": issues, "total": total })))
}

pub async fn list_ready(
    State(state): State<AppState>,
) -> Result<Json<Value>, (StatusCode, Json<ApiError>)> {
    let db = state.db().await;
    let service = RepositoryService::projection(&db);

    let issues = service
        .list_ready_issues()
        .map_err(|e| internal_error("Failed to list ready issues", e))?;
    drop(db);

    let total = issues.len();
    Ok(Json(json!({ "items": issues, "total": total })))
}

pub async fn get_issue(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<IssueDetail>, (StatusCode, Json<ApiError>)> {
    let db = state.db().await;
    let service = RepositoryService::projection(&db);

    let issue = service
        .get_issue(id)
        .map_err(|e| internal_error("Failed to fetch issue", e))?
        .ok_or_else(|| not_found(format!("Issue #{id} not found")))?;

    let labels = service
        .get_labels(id)
        .map_err(|e| internal_error("Failed to fetch labels", e))?;
    let comments = service
        .get_comments(id)
        .map_err(|e| internal_error("Failed to fetch comments", e))?;
    let blockers = service
        .get_blockers(id)
        .map_err(|e| internal_error("Failed to fetch blockers", e))?;
    let blocking = service
        .get_blocking(id)
        .map_err(|e| internal_error("Failed to fetch blocking", e))?;
    let subissues = service
        .get_subissues(id)
        .map_err(|e| internal_error("Failed to fetch subissues", e))?;

    let milestone = service.get_issue_milestone(id).ok().flatten().map(|m| {
        crate::server::types::MilestoneSummary {
            id: m.id,
            name: m.name,
            status: m.status,
        }
    });
    drop(db);

    Ok(Json(IssueDetail {
        issue,
        labels,
        comments,
        blockers,
        blocking,
        subissues,
        milestone,
    }))
}

pub async fn update_issue(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<UpdateIssueRequest>,
) -> Result<Json<Value>, (StatusCode, Json<ApiError>)> {
    let db = state.db().await;
    let service = RepositoryService::new(&db, &state.crosslink_dir)
        .map_err(|error| internal_error("Shared authority is unavailable", error))?;

    service
        .get_issue(id)
        .map_err(|e| internal_error("Failed to fetch issue", e))?
        .ok_or_else(|| not_found(format!("Issue #{id} not found")))?;

    execute_update_issue_command(&service, id, body).map_err(|e| bad_request(e.to_string()))?;

    let issue = service
        .get_issue(id)
        .map_err(|e| internal_error("Failed to refetch updated issue", e))?
        .ok_or_else(|| internal_error("Issue disappeared after update", "unexpected state"))?;
    drop(db);

    broadcast_issue_updated(&state, id, "updated");
    Ok(Json(json!(issue)))
}

pub async fn delete_issue(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<OkResponse>, (StatusCode, Json<ApiError>)> {
    let db = state.db().await;
    let service = RepositoryService::new(&db, &state.crosslink_dir)
        .map_err(|error| internal_error("Shared authority is unavailable", error))?;

    service
        .get_issue(id)
        .map_err(|e| internal_error("Failed to fetch issue", e))?
        .ok_or_else(|| not_found(format!("Issue #{id} not found")))?;

    execute_delete_issue_command(&service, id)
        .map_err(|e| internal_error("Failed to delete issue", e))?;
    drop(db);

    broadcast_issue_updated(&state, id, "deleted");
    Ok(Json(OkResponse { ok: true }))
}

pub async fn close_issue(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, (StatusCode, Json<ApiError>)> {
    let db = state.db().await;
    let service = RepositoryService::new(&db, &state.crosslink_dir)
        .map_err(|error| internal_error("Shared authority is unavailable", error))?;

    service
        .get_issue(id)
        .map_err(|e| internal_error("Failed to fetch issue", e))?
        .ok_or_else(|| not_found(format!("Issue #{id} not found")))?;

    execute_close_issue_command(&service, id)
        .map_err(|e| internal_error("Failed to close issue", e))?;

    let issue = service
        .get_issue(id)
        .map_err(|e| internal_error("Failed to refetch closed issue", e))?
        .ok_or_else(|| internal_error("Issue disappeared after close", "unexpected state"))?;
    drop(db);

    broadcast_issue_updated(&state, id, "status");
    Ok(Json(json!(issue)))
}

pub async fn reopen_issue(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, (StatusCode, Json<ApiError>)> {
    let db = state.db().await;
    let service = RepositoryService::new(&db, &state.crosslink_dir)
        .map_err(|error| internal_error("Shared authority is unavailable", error))?;

    service
        .get_issue(id)
        .map_err(|e| internal_error("Failed to fetch issue", e))?
        .ok_or_else(|| not_found(format!("Issue #{id} not found")))?;

    execute_reopen_issue_command(&service, id)
        .map_err(|e| internal_error("Failed to reopen issue", e))?;

    let issue = service
        .get_issue(id)
        .map_err(|e| internal_error("Failed to refetch reopened issue", e))?
        .ok_or_else(|| internal_error("Issue disappeared after reopen", "unexpected state"))?;
    drop(db);

    broadcast_issue_updated(&state, id, "status");
    Ok(Json(json!(issue)))
}

pub async fn create_subissue(
    State(state): State<AppState>,
    Path(parent_id): Path<i64>,
    Json(body): Json<CreateSubissueRequest>,
) -> Result<Json<Value>, (StatusCode, Json<ApiError>)> {
    let db = state.db().await;
    let service = RepositoryService::new(&db, &state.crosslink_dir)
        .map_err(|error| internal_error("Shared authority is unavailable", error))?;

    service
        .get_issue(parent_id)
        .map_err(|e| internal_error("Failed to fetch parent issue", e))?
        .ok_or_else(|| not_found(format!("Parent issue #{parent_id} not found")))?;

    let child_id = execute_create_subissue_command(&service, parent_id, &body)
        .map_err(|e| bad_request(e.to_string()))?;

    let child = service
        .get_issue(child_id)
        .map_err(|e| internal_error("Failed to retrieve created subissue", e))?
        .ok_or_else(|| internal_error("Subissue was created but not found", "unexpected state"))?;
    drop(db);

    broadcast_issue_updated(&state, parent_id, "subissues");
    broadcast_issue_updated(&state, child_id, "created");
    Ok(Json(json!(child)))
}

pub async fn list_comments(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, (StatusCode, Json<ApiError>)> {
    let db = state.db().await;
    let service = RepositoryService::projection(&db);

    service
        .get_issue(id)
        .map_err(|e| internal_error("Failed to fetch issue", e))?
        .ok_or_else(|| not_found(format!("Issue #{id} not found")))?;

    let comments = service
        .get_comments(id)
        .map_err(|e| internal_error("Failed to fetch comments", e))?;
    drop(db);

    let total = comments.len();
    Ok(Json(json!({ "items": comments, "total": total })))
}

pub async fn add_comment(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<CreateCommentRequest>,
) -> Result<Json<Value>, (StatusCode, Json<ApiError>)> {
    let db = state.db().await;
    let service = RepositoryService::new(&db, &state.crosslink_dir)
        .map_err(|error| internal_error("Shared authority is unavailable", error))?;

    service
        .get_issue(id)
        .map_err(|e| internal_error("Failed to fetch issue", e))?
        .ok_or_else(|| not_found(format!("Issue #{id} not found")))?;

    let comment_id =
        execute_add_comment_command(&service, id, &body).map_err(|e| bad_request(e.to_string()))?;

    let comments = service
        .get_comments(id)
        .map_err(|e| internal_error("Failed to fetch comments after add", e))?;

    let comment = comments
        .into_iter()
        .find(|c| c.id == comment_id)
        .ok_or_else(|| internal_error("Comment was stored but not found", "unexpected state"))?;
    drop(db);

    broadcast_issue_updated(&state, id, "comments");
    Ok(Json(json!(comment)))
}

pub async fn add_label(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<AddLabelRequest>,
) -> Result<Json<OkResponse>, (StatusCode, Json<ApiError>)> {
    let db = state.db().await;
    let service = RepositoryService::new(&db, &state.crosslink_dir)
        .map_err(|error| internal_error("Shared authority is unavailable", error))?;

    service
        .get_issue(id)
        .map_err(|e| internal_error("Failed to fetch issue", e))?
        .ok_or_else(|| not_found(format!("Issue #{id} not found")))?;

    execute_add_label_command(&service, id, &body.label).map_err(|e| bad_request(e.to_string()))?;
    drop(db);

    broadcast_issue_updated(&state, id, "labels");
    Ok(Json(OkResponse { ok: true }))
}

pub async fn remove_label(
    State(state): State<AppState>,
    Path((id, label)): Path<(i64, String)>,
) -> Result<Json<OkResponse>, (StatusCode, Json<ApiError>)> {
    let db = state.db().await;
    let service = RepositoryService::new(&db, &state.crosslink_dir)
        .map_err(|error| internal_error("Shared authority is unavailable", error))?;

    service
        .get_issue(id)
        .map_err(|e| internal_error("Failed to fetch issue", e))?
        .ok_or_else(|| not_found(format!("Issue #{id} not found")))?;

    let removed = execute_remove_label_command(&service, id, &label)
        .map_err(|e| internal_error("Failed to remove label", e))?;
    drop(db);

    if !removed {
        return Err(not_found(format!(
            "Label '{label}' not found on issue #{id}"
        )));
    }

    broadcast_issue_updated(&state, id, "labels");
    Ok(Json(OkResponse { ok: true }))
}

pub async fn add_blocker(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<AddBlockerRequest>,
) -> Result<Json<OkResponse>, (StatusCode, Json<ApiError>)> {
    let db = state.db().await;
    let service = RepositoryService::new(&db, &state.crosslink_dir)
        .map_err(|error| internal_error("Shared authority is unavailable", error))?;

    service
        .get_issue(id)
        .map_err(|e| internal_error("Failed to fetch issue", e))?
        .ok_or_else(|| not_found(format!("Issue #{id} not found")))?;

    service
        .get_issue(body.blocker_id)
        .map_err(|e| internal_error("Failed to fetch blocker issue", e))?
        .ok_or_else(|| not_found(format!("Blocker issue #{} not found", body.blocker_id)))?;

    execute_add_blocker_command(&service, id, body.blocker_id)
        .map_err(|e| bad_request(e.to_string()))?;
    drop(db);

    broadcast_issue_updated(&state, id, "blockers");
    Ok(Json(OkResponse { ok: true }))
}

pub async fn remove_blocker(
    State(state): State<AppState>,
    Path((id, blocker_id)): Path<(i64, i64)>,
) -> Result<Json<OkResponse>, (StatusCode, Json<ApiError>)> {
    let db = state.db().await;
    let service = RepositoryService::new(&db, &state.crosslink_dir)
        .map_err(|error| internal_error("Shared authority is unavailable", error))?;

    service
        .get_issue(id)
        .map_err(|e| internal_error("Failed to fetch issue", e))?
        .ok_or_else(|| not_found(format!("Issue #{id} not found")))?;

    let removed = execute_remove_blocker_command(&service, id, blocker_id)
        .map_err(|e| internal_error("Failed to remove dependency", e))?;
    drop(db);

    if !removed {
        return Err(not_found(format!(
            "Issue #{blocker_id} is not a blocker of issue #{id}"
        )));
    }

    broadcast_issue_updated(&state, id, "blockers");
    Ok(Json(OkResponse { ok: true }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;
    use crate::server::state::AppState;
    use axum::{body::Body, http::Request};
    use tempfile::tempdir;
    use tower::util::ServiceExt;

    fn test_state() -> (AppState, tempfile::TempDir) {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = Database::open(&db_path).unwrap();
        let state = AppState::new(db, dir.path().join(".crosslink"));
        (state, dir)
    }

    #[test]
    fn test_broadcast_issue_updated_no_receivers() {
        let (state, _dir) = test_state();

        broadcast_issue_updated(&state, 1, "status");
    }

    #[test]
    fn test_broadcast_issue_updated_with_receiver() {
        let (state, _dir) = test_state();
        let mut rx = state.ws_tx.subscribe();
        broadcast_issue_updated(&state, 42, "labels");
        let event = rx.try_recv().unwrap();
        if let WsEvent::IssueUpdated(e) = event {
            assert_eq!(e.issue_id, 42);
            assert_eq!(e.field, "labels");
        } else {
            panic!("expected IssueUpdated event");
        }
    }

    fn build_router(state: AppState) -> axum::Router {
        use axum::routing::{delete, get, post};
        axum::Router::new()
            .route("/issues", get(list_issues).post(create_issue))
            .route("/issues/blocked", get(list_blocked))
            .route("/issues/ready", get(list_ready))
            .route(
                "/issues/{id}",
                get(get_issue).patch(update_issue).delete(delete_issue),
            )
            .route("/issues/{id}/close", post(close_issue))
            .route("/issues/{id}/reopen", post(reopen_issue))
            .route("/issues/{id}/subissue", post(create_subissue))
            .route(
                "/issues/{id}/comments",
                get(list_comments).post(add_comment),
            )
            .route("/issues/{id}/labels", post(add_label))
            .route("/issues/{id}/labels/{label}", delete(remove_label))
            .route("/issues/{id}/block", post(add_blocker))
            .route("/issues/{id}/block/{blocker_id}", delete(remove_blocker))
            .with_state(state)
    }

    #[tokio::test]
    async fn test_list_issues_empty() {
        let (state, _dir) = test_state();
        let app = build_router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/issues")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["total"], 0);
        assert!(json["items"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_create_and_get_issue() {
        let (state, _dir) = test_state();
        let app = build_router(state);

        let create_body = r#"{"title": "Test issue", "priority": "high"}"#;
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/issues")
                    .header("content-type", "application/json")
                    .body(Body::from(create_body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let created: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let id = created["id"].as_i64().unwrap();

        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/issues/{id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let detail: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(detail["id"], id);
        assert_eq!(detail["title"], "Test issue");
        assert_eq!(detail["labels"].as_array().unwrap().len(), 0);
        assert_eq!(detail["comments"].as_array().unwrap().len(), 0);
        assert_eq!(detail["subissues"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn test_get_issue_not_found() {
        let (state, _dir) = test_state();
        let app = build_router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/issues/9999")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_update_issue() {
        let (state, _dir) = test_state();
        let id = {
            let db = state.db.lock().await;
            db.create_issue("Original", None, "medium").unwrap()
        };

        let app = build_router(state);
        let patch_body = r#"{"title": "Updated", "priority": "low"}"#;
        let response = app
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri(format!("/issues/{id}"))
                    .header("content-type", "application/json")
                    .body(Body::from(patch_body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let updated: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(updated["title"], "Updated");
        assert_eq!(updated["priority"], "low");
    }

    #[tokio::test]
    async fn test_close_and_reopen_issue() {
        let (state, _dir) = test_state();
        let id = {
            let db = state.db.lock().await;
            db.create_issue("Close me", None, "medium").unwrap()
        };

        let app = build_router(state);

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/issues/{id}/close"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let closed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(closed["status"], "closed");

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/issues/{id}/reopen"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let reopened: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(reopened["status"], "open");
    }

    #[tokio::test]
    async fn test_delete_issue() {
        let (state, _dir) = test_state();
        let id = {
            let db = state.db.lock().await;
            db.create_issue("Delete me", None, "low").unwrap()
        };

        let app = build_router(state);
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/issues/{id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/issues/{id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_subissue() {
        let (state, _dir) = test_state();
        let parent_id = {
            let db = state.db.lock().await;
            db.create_issue("Parent", None, "high").unwrap()
        };

        let app = build_router(state);
        let body = r#"{"title": "Child issue", "priority": "medium"}"#;
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/issues/{parent_id}/subissue"))
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let resp_body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let child: serde_json::Value = serde_json::from_slice(&resp_body).unwrap();
        assert_eq!(child["parent_id"], parent_id);
        assert_eq!(child["title"], "Child issue");

        let child_id = child["id"].as_i64().unwrap();
        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/issues/{parent_id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let resp_body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let detail: serde_json::Value = serde_json::from_slice(&resp_body).unwrap();
        let has_child = detail["subissues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["id"].as_i64() == Some(child_id));
        assert!(has_child);
    }

    #[tokio::test]
    async fn test_comments() {
        let (state, _dir) = test_state();
        let id = {
            let db = state.db.lock().await;
            db.create_issue("Comment test", None, "medium").unwrap()
        };

        let app = build_router(state);

        let body = r#"{"content": "Hello world", "kind": "note"}"#;
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/issues/{id}/comments"))
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let resp_body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let comment: serde_json::Value = serde_json::from_slice(&resp_body).unwrap();
        assert_eq!(comment["content"], "Hello world");
        assert_eq!(comment["kind"], "note");

        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/issues/{id}/comments"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let resp_body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let list: serde_json::Value = serde_json::from_slice(&resp_body).unwrap();
        assert_eq!(list["total"], 1);
    }

    #[tokio::test]
    async fn test_labels() {
        let (state, _dir) = test_state();
        let id = {
            let db = state.db.lock().await;
            db.create_issue("Label test", None, "medium").unwrap()
        };

        let app = build_router(state);

        let body = r#"{"label": "bug"}"#;
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/issues/{id}/labels"))
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/issues/{id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let resp_body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let detail: serde_json::Value = serde_json::from_slice(&resp_body).unwrap();
        assert!(detail["labels"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l == "bug"));

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/issues/{id}/labels/bug"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/issues/{id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let resp_body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let detail: serde_json::Value = serde_json::from_slice(&resp_body).unwrap();
        assert!(detail["labels"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_blockers() {
        let (state, _dir) = test_state();
        let (blocked_id, blocker_id) = {
            let db = state.db.lock().await;
            let b1 = db.create_issue("Blocked", None, "medium").unwrap();
            let b2 = db.create_issue("Blocker", None, "high").unwrap();
            (b1, b2)
        };

        let app = build_router(state);

        let body = format!(r#"{{"blocker_id": {blocker_id}}}"#);
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/issues/{blocked_id}/block"))
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/issues/{blocked_id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let resp_body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let detail: serde_json::Value = serde_json::from_slice(&resp_body).unwrap();
        assert!(detail["blockers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b == blocker_id));

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/issues/{blocked_id}/block/{blocker_id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/issues/blocked")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let resp_body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let blocked_list: serde_json::Value = serde_json::from_slice(&resp_body).unwrap();
        assert_eq!(blocked_list["total"], 0);
    }

    #[tokio::test]
    async fn test_list_issues_filter_by_status() {
        let (state, _dir) = test_state();
        let (id1, id2) = {
            let db = state.db.lock().await;
            let a = db.create_issue("Open issue", None, "medium").unwrap();
            let b = db.create_issue("Closed issue", None, "medium").unwrap();
            db.close_issue(b).unwrap();
            (a, b)
        };

        let app = build_router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/issues?status=open")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let resp_body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let list: serde_json::Value = serde_json::from_slice(&resp_body).unwrap();
        let ids: Vec<i64> = list["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["id"].as_i64().unwrap())
            .collect();
        assert!(ids.contains(&id1));
        assert!(!ids.contains(&id2));
    }

    #[tokio::test]
    async fn test_list_ready_issues() {
        let (state, _dir) = test_state();
        let (ready_id, blocked_id, blocker_id) = {
            let db = state.db.lock().await;
            let r = db.create_issue("Ready", None, "medium").unwrap();
            let bd = db.create_issue("Blocked", None, "medium").unwrap();
            let bl = db.create_issue("Blocker", None, "high").unwrap();
            db.add_dependency(bd, bl).unwrap();
            (r, bd, bl)
        };

        let app = build_router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/issues/ready")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let resp_body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let list: serde_json::Value = serde_json::from_slice(&resp_body).unwrap();
        let ids: Vec<i64> = list["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["id"].as_i64().unwrap())
            .collect();
        assert!(ids.contains(&ready_id));
        assert!(ids.contains(&blocker_id));
        assert!(!ids.contains(&blocked_id));
    }

    #[tokio::test]
    async fn test_create_issue_with_parent_id() {
        let (state, _dir) = test_state();
        let parent_id = {
            let db = state.db.lock().await;
            db.create_issue("Parent via create", None, "medium")
                .unwrap()
        };

        let app = build_router(state);
        let body = format!(
            r#"{{"title": "Child via create", "priority": "low", "parent_id": {parent_id}}}"#
        );
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/issues")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let resp_body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let created: serde_json::Value = serde_json::from_slice(&resp_body).unwrap();
        assert_eq!(created["parent_id"], parent_id);
        assert_eq!(created["title"], "Child via create");
    }

    #[tokio::test]
    async fn test_list_issues_filter_by_label() {
        let (state, _dir) = test_state();
        {
            let db = state.db.lock().await;
            let id1 = db.create_issue("Has bug label", None, "medium").unwrap();
            let _id2 = db.create_issue("No label", None, "medium").unwrap();
            db.add_label(id1, "bug").unwrap();
        };

        let app = build_router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/issues?label=bug")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let resp_body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let list: serde_json::Value = serde_json::from_slice(&resp_body).unwrap();
        assert_eq!(list["total"], 1);
        assert_eq!(list["items"][0]["title"], "Has bug label");
    }

    #[tokio::test]
    async fn test_list_issues_filter_by_priority() {
        let (state, _dir) = test_state();
        {
            let db = state.db.lock().await;
            db.create_issue("High prio", None, "high").unwrap();
            db.create_issue("Low prio", None, "low").unwrap();
        };

        let app = build_router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/issues?priority=high")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let resp_body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let list: serde_json::Value = serde_json::from_slice(&resp_body).unwrap();
        assert_eq!(list["total"], 1);
        assert_eq!(list["items"][0]["title"], "High prio");
    }

    #[tokio::test]
    async fn test_list_issues_search() {
        let (state, _dir) = test_state();
        {
            let db = state.db.lock().await;
            db.create_issue("Fix authentication bug", None, "high")
                .unwrap();
            db.create_issue("Add new feature", None, "medium").unwrap();
        };

        let app = build_router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/issues?search=authentication")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let resp_body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let list: serde_json::Value = serde_json::from_slice(&resp_body).unwrap();
        assert_eq!(list["total"], 1);
        assert_eq!(list["items"][0]["title"], "Fix authentication bug");
    }

    #[tokio::test]
    async fn test_delete_issue_not_found() {
        let (state, _dir) = test_state();
        let app = build_router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/issues/9999")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_close_issue_not_found() {
        let (state, _dir) = test_state();
        let app = build_router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/issues/9999/close")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_reopen_issue_not_found() {
        let (state, _dir) = test_state();
        let app = build_router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/issues/9999/reopen")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_update_issue_not_found() {
        let (state, _dir) = test_state();
        let app = build_router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri("/issues/9999")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"title": "nope"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_add_comment_issue_not_found() {
        let (state, _dir) = test_state();
        let app = build_router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/issues/9999/comments")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"content": "hello", "kind": "note"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_list_comments_issue_not_found() {
        let (state, _dir) = test_state();
        let app = build_router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/issues/9999/comments")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_add_label_issue_not_found() {
        let (state, _dir) = test_state();
        let app = build_router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/issues/9999/labels")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"label": "bug"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_remove_label_not_found() {
        let (state, _dir) = test_state();
        let id = {
            let db = state.db.lock().await;
            db.create_issue("No labels", None, "medium").unwrap()
        };
        let app = build_router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/issues/{id}/labels/nonexistent"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_add_blocker_issue_not_found() {
        let (state, _dir) = test_state();
        let app = build_router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/issues/9999/block")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"blocker_id": 1}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_add_blocker_blocker_not_found() {
        let (state, _dir) = test_state();
        let id = {
            let db = state.db.lock().await;
            db.create_issue("Exists", None, "medium").unwrap()
        };
        let app = build_router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/issues/{id}/block"))
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"blocker_id": 9999}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_remove_blocker_not_a_blocker() {
        let (state, _dir) = test_state();
        let (id, other_id) = {
            let db = state.db.lock().await;
            let a = db.create_issue("Issue A", None, "medium").unwrap();
            let b = db.create_issue("Issue B", None, "medium").unwrap();
            (a, b)
        };
        let app = build_router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/issues/{id}/block/{other_id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_create_subissue_parent_not_found() {
        let (state, _dir) = test_state();
        let app = build_router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/issues/9999/subissue")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"title": "Child", "priority": "medium"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_remove_label_issue_not_found() {
        let (state, _dir) = test_state();
        let app = build_router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/issues/9999/labels/bug")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_remove_blocker_issue_not_found() {
        let (state, _dir) = test_state();
        let app = build_router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/issues/9999/block/1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_list_issues_filter_by_parent_id() {
        let (state, _dir) = test_state();
        let (parent_id, child_id) = {
            let db = state.db.lock().await;
            let p = db.create_issue("Parent issue", None, "high").unwrap();
            let c = db
                .create_subissue(p, "Child issue", None, "medium")
                .unwrap();
            (p, c)
        };

        let app = build_router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/issues?parent_id={parent_id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let resp_body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let list: serde_json::Value = serde_json::from_slice(&resp_body).unwrap();
        assert_eq!(list["total"], 1);
        assert_eq!(list["items"][0]["id"], child_id);
    }

    #[tokio::test]
    async fn test_list_issues_search_with_priority_filter() {
        let (state, _dir) = test_state();
        {
            let db = state.db.lock().await;
            db.create_issue("Widget high", None, "high").unwrap();
            db.create_issue("Widget low", None, "low").unwrap();
        };

        let app = build_router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/issues?search=Widget&priority=high")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let resp_body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let list: serde_json::Value = serde_json::from_slice(&resp_body).unwrap();
        assert_eq!(list["total"], 1);
        assert_eq!(list["items"][0]["title"], "Widget high");
    }

    #[tokio::test]
    async fn test_list_issues_search_with_label_filter() {
        let (state, _dir) = test_state();
        {
            let db = state.db.lock().await;
            let id1 = db.create_issue("Gadget with bug", None, "medium").unwrap();
            db.create_issue("Gadget no label", None, "medium").unwrap();
            db.add_label(id1, "bug").unwrap();
        };

        let app = build_router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/issues?search=Gadget&label=bug")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let resp_body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let list: serde_json::Value = serde_json::from_slice(&resp_body).unwrap();
        assert_eq!(list["total"], 1);
        assert_eq!(list["items"][0]["title"], "Gadget with bug");
    }

    #[tokio::test]
    async fn test_list_issues_search_with_parent_id_filter() {
        let (state, _dir) = test_state();
        let parent_id = {
            let db = state.db.lock().await;
            let p = db.create_issue("Parent task", None, "high").unwrap();
            db.create_subissue(p, "Gizmo sub-task", None, "medium")
                .unwrap();

            db.create_issue("Gizmo standalone", None, "medium").unwrap();
            p
        };

        let app = build_router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/issues?search=Gizmo&parent_id={parent_id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let resp_body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let list: serde_json::Value = serde_json::from_slice(&resp_body).unwrap();
        assert_eq!(list["total"], 1);
        assert_eq!(list["items"][0]["title"], "Gizmo sub-task");
    }

    #[tokio::test]
    async fn test_list_issues_search_status_all() {
        let (state, _dir) = test_state();
        {
            let db = state.db.lock().await;
            let id1 = db.create_issue("Sprocket open", None, "medium").unwrap();
            let id2 = db.create_issue("Sprocket closed", None, "medium").unwrap();
            db.close_issue(id2).unwrap();
            let _ = id1;
        };

        let app = build_router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/issues?search=Sprocket&status=all")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let resp_body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let list: serde_json::Value = serde_json::from_slice(&resp_body).unwrap();

        assert_eq!(list["total"], 2);
    }

    #[tokio::test]
    async fn test_add_intervention_comment() {
        let (state, _dir) = test_state();
        let id = {
            let db = state.db.lock().await;
            db.create_issue("Intervention test", None, "medium")
                .unwrap()
        };

        let app = build_router(state);
        let body = serde_json::json!({
            "content": "Manual intervention needed",
            "kind": "intervention",
            "trigger_type": "user_request",
            "intervention_context": "CI pipeline failed"
        });
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/issues/{id}/comments"))
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let resp_body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let comment: serde_json::Value = serde_json::from_slice(&resp_body).unwrap();
        assert_eq!(comment["content"], "Manual intervention needed");
        assert_eq!(comment["kind"], "intervention");
    }

    #[tokio::test]
    async fn test_list_issues_search_with_status_filter() {
        let (state, _dir) = test_state();
        {
            let db = state.db.lock().await;
            let id1 = db.create_issue("Auth bug open", None, "high").unwrap();
            let id2 = db.create_issue("Auth bug closed", None, "high").unwrap();
            db.close_issue(id2).unwrap();
            let _ = (id1, id2);
        };

        let app = build_router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/issues?search=Auth+bug&status=open")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let resp_body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let list: serde_json::Value = serde_json::from_slice(&resp_body).unwrap();
        assert_eq!(list["total"], 1);
        assert_eq!(list["items"][0]["title"], "Auth bug open");
    }

    #[test]
    fn test_helper_functions_directly() {
        let (status, json) = crate::server::errors::internal_error("ctx", "detail");
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(json.error, "ctx");
        assert_eq!(json.detail.as_deref(), Some("detail"));

        let (status, json) = crate::server::errors::not_found("gone");
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(json.error, "not found");
        assert_eq!(json.detail.as_deref(), Some("gone"));

        let (status, json) = crate::server::errors::bad_request("invalid input");
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(json.error, "bad request");
        assert_eq!(json.detail.as_deref(), Some("invalid input"));
    }

    #[tokio::test]
    async fn test_get_issue_with_milestone() {
        let (state, _dir) = test_state();
        let (issue_id, milestone_id) = {
            let db = state.db.lock().await;
            let iid = db.create_issue("Has milestone", None, "medium").unwrap();
            let mid = db.create_milestone("Sprint 1", None).unwrap();
            db.add_issue_to_milestone(mid, iid).unwrap();
            (iid, mid)
        };

        let app = build_router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/issues/{issue_id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let resp_body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let detail: serde_json::Value = serde_json::from_slice(&resp_body).unwrap();

        assert!(!detail["milestone"].is_null());
        assert_eq!(detail["milestone"]["id"], milestone_id);
        assert_eq!(detail["milestone"]["name"], "Sprint 1");
    }

    #[tokio::test]
    async fn test_update_issue_priority_only() {
        let (state, _dir) = test_state();
        let id = {
            let db = state.db.lock().await;
            db.create_issue("Priority update", None, "low").unwrap()
        };

        let app = build_router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri(format!("/issues/{id}"))
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"priority": "high"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let resp_body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let updated: serde_json::Value = serde_json::from_slice(&resp_body).unwrap();
        assert_eq!(updated["priority"], "high");
        assert_eq!(updated["title"], "Priority update");
    }

    #[tokio::test]
    async fn test_list_issues_no_label_match_in_search() {
        let (state, _dir) = test_state();
        {
            let db = state.db.lock().await;

            let id = db.create_issue("Widget thing", None, "medium").unwrap();
            db.add_label(id, "enhancement").unwrap();
        };

        let app = build_router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/issues?search=Widget&label=bug")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let resp_body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let list: serde_json::Value = serde_json::from_slice(&resp_body).unwrap();

        assert_eq!(list["total"], 0);
    }
}
