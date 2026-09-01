use axum::response::{IntoResponse as _, Response};
use axum::{extract::State, response::Json};

use crate::server::state::AppState;
use crate::server::types::HealthResponse;

pub async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok".to_string(),
        version: state.version.to_string(),
    })
}

pub async fn readiness(State(state): State<AppState>) -> Response {
    match crate::reconcile::readiness::read_record(&state.crosslink_dir) {
        Ok(Some(record)) => {
            match crate::reconcile::readiness::validate_record(&state.crosslink_dir, &record) {
                Ok(()) => Json(crate::reconcile::readiness::DaemonResponse::from_record(
                    &record,
                ))
                .into_response(),
                Err(error) => (
                    axum::http::StatusCode::SERVICE_UNAVAILABLE,
                    Json(crate::reconcile::readiness::DaemonResponse::error(
                        error.to_string(),
                    )),
                )
                    .into_response(),
            }
        }
        Ok(None) => (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            Json(crate::reconcile::readiness::DaemonResponse::error(
                "repository readiness has not been published".to_string(),
            )),
        )
            .into_response(),
        Err(error) => (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            Json(crate::reconcile::readiness::DaemonResponse::error(
                error.to_string(),
            )),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use axum::{body::Body, http::Request, Router};
    use tower::util::ServiceExt;

    use crate::db::Database;
    use crate::server::{routes::build_router, state::AppState};

    #[tokio::test]
    async fn test_health_endpoint() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("test.db")).unwrap();
        let state = AppState::new(db, dir.path().join(".crosslink"));
        let app: Router = build_router(state, None);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["status"], "ok");
        assert!(body["version"].is_string());
    }

    #[tokio::test]
    async fn readiness_endpoint_validates_record_authority_before_reporting_it() {
        let dir = tempfile::tempdir().unwrap();
        let crosslink = dir.path().join(".crosslink");
        std::fs::create_dir(&crosslink).unwrap();
        std::fs::write(crosslink.join("hook-config.json"), "{}").unwrap();
        let db = Database::open(&crosslink.join("issues.db")).unwrap();
        let identity = crate::reconcile::readiness::DaemonIdentity {
            schema_version: crate::reconcile::readiness::READINESS_SCHEMA_VERSION,
            repository_id: crate::reconcile::readiness::repository_id(&crosslink).unwrap(),
            daemon_epoch: "server-readiness".to_string(),
            pid: std::process::id(),
            process_start: crate::reconcile::readiness::current_process_start_token().unwrap(),
        };
        crate::reconcile::readiness::write_daemon_identity(&crosslink, &identity).unwrap();
        crate::reconcile::readiness::write_record(
            &crosslink,
            crate::reconcile::readiness::ReadinessDraft {
                daemon_epoch: &identity.daemon_epoch,
                daemon_pid: identity.pid,
                attempt_id: "server-readiness",
                state: crate::reconcile::readiness::ReadinessState::BlockedCorrupt,
                generation_id: None,
                reason: Some("projection is corrupt"),
            },
        )
        .unwrap();
        let app: Router = build_router(AppState::new(db, crosslink.clone()), None);
        let valid = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/readiness")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(valid.status(), 200);
        let bytes = axum::body::to_bytes(valid.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: crate::reconcile::readiness::DaemonResponse =
            serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            body.schema_version,
            crate::reconcile::readiness::READINESS_SCHEMA_VERSION
        );
        assert_eq!(
            body.protocol_version,
            crate::reconcile::readiness::READINESS_PROTOCOL_VERSION
        );
        assert_eq!(
            body.state,
            Some(crate::reconcile::readiness::ReadinessOutcomeState::BlockedCorrupt)
        );
        assert!(!body.ready);
        assert!(body.running);
        assert_eq!(
            body.repository_id.as_deref(),
            Some(identity.repository_id.as_str())
        );
        assert_eq!(
            body.daemon_epoch.as_deref(),
            Some(identity.daemon_epoch.as_str())
        );
        assert_eq!(body.daemon_pid, Some(identity.pid));
        assert!(body.generation_id.is_none());
        assert!(body.evidence_path.is_some());
        assert!(body.evidence_sha256.is_some());
        let mut records = std::fs::read_dir(crosslink.join("readiness"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|value| value == "json"))
            .collect::<Vec<_>>();
        records.sort();
        let record_path = records.pop().unwrap();
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&record_path).unwrap()).unwrap();
        value["repository_id"] = serde_json::Value::String("foreign".to_string());
        std::fs::write(&record_path, serde_json::to_vec(&value).unwrap()).unwrap();
        let invalid = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/readiness")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(invalid.status(), 503);
        let bytes = axum::body::to_bytes(invalid.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: crate::reconcile::readiness::DaemonResponse =
            serde_json::from_slice(&bytes).unwrap();
        assert!(!body.ready);
        assert!(!body.running);
        assert!(body.state.is_none());
        assert!(body
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("different repository")));
    }

    #[tokio::test]
    async fn readiness_endpoint_reports_ready_with_the_daemon_envelope() {
        let (_work, _remote, crosslink, _cache) =
            crate::reconcile::migration::tests::setup_v2_hub();
        crate::reconcile::migration::hub_v3(&crosslink, false, false, false, false).unwrap();
        let activation = crate::reconcile::migration::activate_repository(&crosslink).unwrap();
        let (state, generation_id) = match activation {
            crate::reconcile::migration::RepositoryActivation::ReadyCurrent { generation_id } => (
                crate::reconcile::readiness::ReadinessState::ReadyCurrent,
                generation_id,
            ),
            crate::reconcile::migration::RepositoryActivation::ReadyMigrated { generation_id } => (
                crate::reconcile::readiness::ReadinessState::ReadyMigrated,
                generation_id,
            ),
            crate::reconcile::migration::RepositoryActivation::ReadyAdopted { generation_id } => (
                crate::reconcile::readiness::ReadinessState::ReadyAdopted,
                generation_id,
            ),
            other => panic!("unexpected activation: {other:?}"),
        };
        let identity = crate::reconcile::readiness::DaemonIdentity {
            schema_version: crate::reconcile::readiness::READINESS_SCHEMA_VERSION,
            repository_id: crate::reconcile::readiness::repository_id(&crosslink).unwrap(),
            daemon_epoch: "server-ready".to_string(),
            pid: std::process::id(),
            process_start: crate::reconcile::readiness::current_process_start_token().unwrap(),
        };
        crate::reconcile::readiness::write_daemon_identity(&crosslink, &identity).unwrap();
        crate::reconcile::readiness::write_record(
            &crosslink,
            crate::reconcile::readiness::ReadinessDraft {
                daemon_epoch: &identity.daemon_epoch,
                daemon_pid: identity.pid,
                attempt_id: "server-ready",
                state,
                generation_id: Some(&generation_id),
                reason: None,
            },
        )
        .unwrap();
        let db = Database::open_read_only(&crosslink.join("issues.db")).unwrap();
        let app: Router = build_router(AppState::new(db, crosslink), None);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/readiness")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: crate::reconcile::readiness::DaemonResponse =
            serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            body.schema_version,
            crate::reconcile::readiness::READINESS_SCHEMA_VERSION
        );
        assert_eq!(
            body.protocol_version,
            crate::reconcile::readiness::READINESS_PROTOCOL_VERSION
        );
        assert!(body.ready);
        assert!(body.running);
        assert_eq!(
            body.repository_id.as_deref(),
            Some(identity.repository_id.as_str())
        );
        assert_eq!(
            body.daemon_epoch.as_deref(),
            Some(identity.daemon_epoch.as_str())
        );
        assert_eq!(body.daemon_pid, Some(identity.pid));
        assert_eq!(body.generation_id.as_deref(), Some(generation_id.as_str()));
        assert!(matches!(
            body.state,
            Some(
                crate::reconcile::readiness::ReadinessOutcomeState::ReadyCurrent
                    | crate::reconcile::readiness::ReadinessOutcomeState::ReadyMigrated
                    | crate::reconcile::readiness::ReadinessOutcomeState::ReadyAdopted
            )
        ));
    }
}
