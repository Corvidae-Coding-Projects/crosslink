use axum::{extract::State, http::StatusCode, response::Json};

use crate::server::{
    errors::internal_error,
    state::AppState,
    types::{ApiError, SyncActionResponse, SyncStatusResponse},
};
use crate::sync::SyncManager;

fn fetch_head_mtime(cache_path: &std::path::Path) -> Option<chrono::DateTime<chrono::Utc>> {
    let out = std::process::Command::new("git")
        .current_dir(cache_path)
        .args(["rev-parse", "--git-path", "FETCH_HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let rel = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if rel.is_empty() {
        return None;
    }
    let path = std::path::Path::new(&rel);
    let resolved = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cache_path.join(path)
    };
    std::fs::metadata(&resolved)
        .ok()?
        .modified()
        .ok()
        .map(chrono::DateTime::<chrono::Utc>::from)
}

fn sync_manager(state: &AppState) -> Result<SyncManager, (StatusCode, Json<ApiError>)> {
    SyncManager::new(&state.crosslink_dir)
        .map_err(|e| internal_error("Failed to initialize SyncManager", e))
}

pub async fn sync_status(
    State(state): State<AppState>,
) -> Result<Json<SyncStatusResponse>, (StatusCode, Json<ApiError>)> {
    let sm = sync_manager(&state)?;

    let hub_initialized = sm.is_initialized();
    let remote = sm.remote().to_string();

    let (active_lock_count, stale_lock_count) = if hub_initialized {
        let locks = sm
            .read_locks_auto()
            .map_err(|e| internal_error("Failed to read locks", e))?;
        let active = locks.locks.len();

        let stale_count = sm.find_stale_locks_with_age().map_or(0, |v| v.len());

        (active, stale_count)
    } else {
        (0, 0)
    };

    let last_fetch_at = if !hub_initialized {
        None
    } else if sm.hub_mode().is_v3() {
        fetch_head_mtime(sm.cache_path())
    } else {
        std::fs::metadata(sm.cache_path())
            .ok()
            .and_then(|m| m.modified().ok())
            .map(chrono::DateTime::<chrono::Utc>::from)
    };

    Ok(Json(SyncStatusResponse {
        hub_initialized,

        hub_branch: if sm.hub_mode().is_v3() {
            "crosslink/checkpoint".to_string()
        } else {
            "crosslink/hub".to_string()
        },
        remote,
        last_fetch_at,
        active_lock_count,
        stale_lock_count,
    }))
}

pub async fn sync_fetch(
    State(state): State<AppState>,
) -> Result<Json<SyncActionResponse>, (StatusCode, Json<ApiError>)> {
    let sm = sync_manager(&state)?;

    if !sm.is_initialized() {
        return Err((
            StatusCode::CONFLICT,
            Json(ApiError {
                error: "Hub not initialized".to_string(),
                detail: Some(
                    "Run `crosslink sync` from the CLI to initialize the hub cache first."
                        .to_string(),
                ),
            }),
        ));
    }

    sm.fetch().map_err(|e| internal_error("Fetch failed", e))?;

    Ok(Json(SyncActionResponse {
        success: true,
        message: "Fetched latest hub state from remote".to_string(),
    }))
}

pub async fn sync_push(
    State(state): State<AppState>,
) -> Result<Json<SyncActionResponse>, (StatusCode, Json<ApiError>)> {
    let sm = sync_manager(&state)?;

    if !sm.is_initialized() {
        return Err((
            StatusCode::CONFLICT,
            Json(ApiError {
                error: "Hub not initialized".to_string(),
                detail: Some(
                    "Run `crosslink sync` from the CLI to initialize the hub cache first."
                        .to_string(),
                ),
            }),
        ));
    }

    sm.fetch().map_err(|e| internal_error("Sync failed", e))?;

    Ok(Json(SyncActionResponse {
        success: true,
        message: "Synchronized hub state with remote (v3 refs)".to_string(),
    }))
}

#[cfg(test)]
mod tests {
    use axum::{
        body::Body,
        http::{Method, Request, StatusCode},
        Router,
    };
    use serde_json::Value;
    use tower::util::ServiceExt;

    use crate::db::Database;
    use crate::server::{routes::build_router, state::AppState};

    fn test_app() -> (Router, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("test.db");
        let db = Database::open(&db_path).expect("test db");
        let state = AppState::new(db, dir.path().join(".crosslink"));
        (build_router(state, None), dir)
    }

    async fn body_json(resp: axum::response::Response) -> Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn test_sync_status_no_hub() {
        let (app, _dir) = test_app();
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/v1/sync/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["hub_initialized"], false);
        assert_eq!(body["hub_branch"], "crosslink/hub");
        assert_eq!(body["active_lock_count"], 0);
        assert_eq!(body["stale_lock_count"], 0);
    }

    #[tokio::test]
    async fn test_sync_status_has_remote_field() {
        let (app, _dir) = test_app();
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/v1/sync/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;

        assert_eq!(body["remote"], "origin");
    }

    #[tokio::test]
    async fn test_sync_fetch_no_hub_returns_conflict() {
        let (app, _dir) = test_app();
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/v1/sync/fetch")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let body = body_json(resp).await;
        assert!(body["error"].as_str().unwrap().contains("not initialized"));
    }

    #[tokio::test]
    async fn test_sync_push_no_hub_returns_conflict() {
        let (app, _dir) = test_app();
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/v1/sync/push")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let body = body_json(resp).await;
        assert!(body["error"].as_str().unwrap().contains("not initialized"));
    }

    fn test_app_with_hub() -> (axum::Router, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("test.db");
        let db = Database::open(&db_path).expect("test db");
        let crosslink_dir = dir.path().join(".crosslink");

        let hub_cache = crosslink_dir.join(".hub-cache");
        std::fs::create_dir_all(&hub_cache).unwrap();

        let locks_json = serde_json::json!({
            "version": 1,
            "locks": {},
            "settings": {"stale_lock_timeout_minutes": 30},
            "updated_at": chrono::Utc::now().to_rfc3339()
        });
        std::fs::write(
            hub_cache.join("locks.json"),
            serde_json::to_string(&locks_json).unwrap(),
        )
        .unwrap();
        let state = AppState::new(db, crosslink_dir);
        (build_router(state, None), dir)
    }

    #[tokio::test]
    async fn test_sync_status_with_hub_initialized() {
        let (app, _dir) = test_app_with_hub();
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/v1/sync/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;

        assert_eq!(body["hub_initialized"], true);
        assert_eq!(body["hub_branch"], "crosslink/hub");

        assert_eq!(body["active_lock_count"], 0);
        assert_eq!(body["stale_lock_count"], 0);
    }

    #[tokio::test]
    async fn test_sync_fetch_with_hub_initialized_offline() {
        let (app, _dir) = test_app_with_hub();
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/v1/sync/fetch")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_ne!(resp.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn test_sync_push_with_hub_initialized_offline() {
        let (app, _dir) = test_app_with_hub();
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/v1/sync/push")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_ne!(resp.status(), StatusCode::CONFLICT);
    }

    #[test]
    fn test_internal_error_helper() {
        let (status, json) = crate::server::errors::internal_error("sync failed", "some error");
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(json.error, "sync failed");
        assert_eq!(json.detail.as_deref(), Some("some error"));
    }
}
