use axum::{routing::get, Router};

use crate::server::{
    handlers::{
        agents::{
            get_agent, get_agent_status, list_agents, list_locks, list_stale_locks,
            notify_lock_changed,
        },
        config::{get_config, update_config},
        health::{health, readiness},
        issues::{
            add_blocker, add_comment, add_label, close_issue, create_issue, create_subissue,
            delete_issue, get_issue, list_blocked, list_comments, list_issues, list_ready,
            remove_blocker, remove_label, reopen_issue, update_issue,
        },
        knowledge::{
            create_knowledge_page, get_knowledge_page, list_knowledge_pages, search_knowledge,
        },
        milestones::{
            assign_milestone, close_milestone, create_milestone, get_milestone, list_milestones,
        },
        orchestrator::{
            decompose_handler, execute, get_plan, get_plan_by_id, get_snapshot, get_status,
            list_plans_handler, mark_stage_done_handler, mark_stage_failed_handler,
            mark_stage_running_handler, pause, poll_agents, resume_execution, retry_stage,
            skip_stage,
        },
        search::global_search,
        sessions::{end_session, get_current_session, start_session, work_on_issue},
        sync::{sync_fetch, sync_push, sync_status},
        usage::{create_usage, list_usage, usage_summary},
    },
    state::AppState,
    ws::ws_handler,
};

pub fn build_router(state: AppState, dashboard_dir: Option<std::path::PathBuf>) -> Router {
    use axum::routing::{delete, post};

    let api = Router::new()
        .route("/health", get(health))
        .route("/readiness", get(readiness))
        .route("/agents", get(list_agents))
        .route("/agents/{id}", get(get_agent))
        .route("/agents/{id}/status", get(get_agent_status))
        .route("/locks", get(list_locks))
        .route("/locks/stale", get(list_stale_locks))
        .route("/locks/notify", post(notify_lock_changed))
        .route("/issues/blocked", get(list_blocked))
        .route("/issues/ready", get(list_ready))
        .route("/issues", get(list_issues).post(create_issue))
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
        .route("/sessions/current", get(get_current_session))
        .route("/sessions/start", post(start_session))
        .route("/sessions/end", post(end_session))
        .route("/sessions/work/{id}", post(work_on_issue))
        .route("/milestones", get(list_milestones).post(create_milestone))
        .route("/milestones/{id}", get(get_milestone))
        .route("/milestones/{id}/assign", post(assign_milestone))
        .route("/milestones/{id}/close", post(close_milestone))
        .route("/knowledge/search", get(search_knowledge))
        .route(
            "/knowledge",
            get(list_knowledge_pages).post(create_knowledge_page),
        )
        .route("/knowledge/{slug}", get(get_knowledge_page))
        .route("/search", get(global_search))
        .route("/sync/status", get(sync_status))
        .route("/sync/fetch", post(sync_fetch))
        .route("/sync/push", post(sync_push))
        .route("/config", get(get_config).patch(update_config))
        .route("/usage/summary", get(usage_summary))
        .route("/usage", get(list_usage).post(create_usage))
        .route("/orchestrator/plans", get(list_plans_handler))
        .route("/orchestrator/plans/{id}", get(get_plan_by_id))
        .route("/orchestrator/plan", get(get_plan))
        .route("/orchestrator/status", get(get_status))
        .route("/orchestrator/snapshot", get(get_snapshot))
        .route("/orchestrator/agents/poll", get(poll_agents))
        .route("/orchestrator/decompose", post(decompose_handler))
        .route("/orchestrator/execute", post(execute))
        .route("/orchestrator/pause", post(pause))
        .route("/orchestrator/resume", post(resume_execution))
        .route("/orchestrator/stages/{id}/retry", post(retry_stage))
        .route("/orchestrator/stages/{id}/skip", post(skip_stage))
        .route(
            "/orchestrator/stages/{id}/running",
            post(mark_stage_running_handler),
        )
        .route(
            "/orchestrator/stages/{id}/done",
            post(mark_stage_done_handler),
        )
        .route(
            "/orchestrator/stages/{id}/failed",
            post(mark_stage_failed_handler),
        );

    let mut app = Router::new()
        .nest("/api/v1", api)
        .nest("/api/v1/dashboard", crate::dashboard::api::build_router())
        .nest("/api/v1/dashboard", crate::dashboard::github_api::router())
        .nest("/api/v1/dashboard", crate::dashboard::export::router())
        .nest("/api/v1/dashboard", crate::dashboard::webhook_api::router())
        .nest("/api/v1", crate::dashboard::pty_api::rest_router())
        .nest("/ws", crate::dashboard::pty_api::ws_router())
        .route("/ws", get(ws_handler))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::server::readiness_middleware,
        ))
        .with_state(state);

    if let Some(dir) = dashboard_dir {
        use tower_http::services::ServeDir;
        app = app.fallback_service(ServeDir::new(dir));
    } else {
        app = app.fallback(super::embedded::serve_embedded);
    }

    app
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode};
    use tower::util::ServiceExt;

    fn test_state(ready_barrier: bool) -> (AppState, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let crosslink = dir.path().join(".crosslink");
        std::fs::create_dir(&crosslink).unwrap();
        if ready_barrier {
            std::fs::write(crosslink.join("hook-config.json"), "{}").unwrap();
            std::fs::write(crosslink.join("agent.json"), "{}").unwrap();
        }
        let db = Database::open(&crosslink.join("issues.db")).unwrap();
        (AppState::new(db, crosslink), dir)
    }

    async fn response(
        method: Method,
        path: &str,
        body: &'static str,
        app: Router,
    ) -> axum::response::Response {
        app.oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap()
    }

    #[test]
    fn test_build_router_with_dashboard_dir() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("test.db")).unwrap();
        let state = AppState::new(db, dir.path().join(".crosslink"));
        let dashboard = dir.path().join("dashboard");
        std::fs::create_dir_all(&dashboard).unwrap();

        let _router = build_router(state, Some(dashboard));
    }

    #[tokio::test]
    async fn non_ready_route_matrix_preserves_diagnostics_and_blocks_mutations() {
        let (state, _dir) = test_state(true);
        let app = build_router(state, None);
        for path in [
            "/api/v1/health",
            "/api/v1/issues",
            "/api/v1/orchestrator/status",
            "/api/v1/dashboard/projects",
            "/api/v1/dashboard/github/config",
            "/api/v1/dashboard/webhooks",
            "/api/v1/pty/sessions",
            "/ws",
        ] {
            let result = response(Method::GET, path, "", app.clone()).await;
            assert_ne!(result.status(), StatusCode::SERVICE_UNAVAILABLE, "{path}");
        }
        for (method, path, body) in [
            (
                Method::POST,
                "/api/v1/issues",
                r#"{"title":"blocked","priority":"medium"}"#,
            ),
            (Method::POST, "/api/v1/sync/fetch", "{}"),
            (Method::GET, "/api/v1/orchestrator/agents/poll", ""),
            (Method::POST, "/api/v1/dashboard/clone", "{}"),
            (Method::POST, "/api/v1/dashboard/github/config", "{}"),
            (Method::PUT, "/api/v1/dashboard/webhooks", r#"{"urls":[]}"#),
            (Method::POST, "/api/v1/pty", "{}"),
            (Method::GET, "/ws/pty/missing", ""),
        ] {
            let result = response(method, path, body, app.clone()).await;
            assert_eq!(result.status(), StatusCode::SERVICE_UNAVAILABLE, "{path}");
            let bytes = axum::body::to_bytes(result.into_body(), usize::MAX)
                .await
                .unwrap();
            let envelope: crate::reconcile::readiness::DaemonResponse =
                serde_json::from_slice(&bytes).unwrap();
            assert!(!envelope.ready, "{path}");
            assert!(envelope.state.is_none(), "{path}");
            assert!(envelope
                .reason
                .as_deref()
                .is_some_and(|value| !value.is_empty()));
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mutating_http_request_completes_after_external_operation_releases() {
        let (state, _dir) = test_state(false);
        let crosslink = state.crosslink_dir.clone();
        let app = build_router(state, None);
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let operation =
                crate::reconcile::readiness::acquire_mutation_operation_permit(&crosslink).unwrap();
            held_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            drop(operation);
        });
        held_rx.recv().unwrap();
        let mut request = tokio::spawn(response(
            Method::POST,
            "/api/v1/issues",
            r#"{"title":"serialized","priority":"medium"}"#,
            app,
        ));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), &mut request)
                .await
                .is_err()
        );
        release_tx.send(()).unwrap();
        holder.join().unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), request)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn corrupt_projection_keeps_health_and_readiness_available() {
        let dir = tempfile::tempdir().unwrap();
        let crosslink = dir.path().join(".crosslink");
        std::fs::create_dir(&crosslink).unwrap();
        std::fs::write(crosslink.join("hook-config.json"), "{}").unwrap();
        std::fs::write(crosslink.join("agent.json"), "{}").unwrap();
        let identity = crate::reconcile::readiness::DaemonIdentity {
            schema_version: crate::reconcile::readiness::READINESS_SCHEMA_VERSION,
            repository_id: crate::reconcile::readiness::repository_id(&crosslink).unwrap(),
            daemon_epoch: "server-corrupt-test".to_string(),
            pid: std::process::id(),
            process_start: crate::reconcile::readiness::current_process_start_token().unwrap(),
        };
        crate::reconcile::readiness::write_daemon_identity(&crosslink, &identity).unwrap();
        crate::reconcile::readiness::write_record(
            &crosslink,
            crate::reconcile::readiness::ReadinessDraft {
                daemon_epoch: &identity.daemon_epoch,
                daemon_pid: identity.pid,
                attempt_id: "server-corrupt-attempt",
                state: crate::reconcile::readiness::ReadinessState::BlockedCorrupt,
                generation_id: None,
                reason: Some("truncated projection"),
            },
        )
        .unwrap();
        let state = AppState::new(Database::open_ephemeral().unwrap(), crosslink)
            .with_database_unavailable(Some("truncated projection".to_string()));
        let app = build_router(state, None);
        assert_eq!(
            response(Method::GET, "/api/v1/health", "", app.clone())
                .await
                .status(),
            StatusCode::OK
        );
        let readiness = response(Method::GET, "/api/v1/readiness", "", app.clone()).await;
        assert_eq!(readiness.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(readiness.into_body(), usize::MAX)
            .await
            .unwrap();
        let envelope: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(envelope["state"], "blocked_corrupt");
        let issues = response(Method::GET, "/api/v1/issues", "", app).await;
        assert_eq!(issues.status(), StatusCode::SERVICE_UNAVAILABLE);
        let bytes = axum::body::to_bytes(issues.into_body(), usize::MAX)
            .await
            .unwrap();
        let envelope: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(envelope["error"], "database_unavailable");
    }
}
