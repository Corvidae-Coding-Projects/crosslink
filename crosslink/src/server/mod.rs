pub mod embedded;
pub mod errors;
pub mod handlers;
pub mod routes;
pub mod state;
pub mod types;
pub mod watcher;
pub mod ws;

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::Result;
use axum::extract::DefaultBodyLimit;
use axum::http::{Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::IntoResponse as _;
use axum::response::Response;
use axum::Json;
use tower_http::cors::CorsLayer;

use crate::db::Database;
use state::AppState;

const MAX_BODY_SIZE: usize = 10 * 1024 * 1024;

async fn auth_middleware(
    axum::extract::State(state): axum::extract::State<AppState>,
    request: Request<axum::body::Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    let path = request.uri().path();

    if path == "/api/v1/health"
        || path == "/api/v1/readiness"
        || path == "/ws"
        || !path.starts_with("/api/")
    {
        return Ok(next.run(request).await);
    }

    let authorized = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|token| token == state.auth_token);

    if authorized {
        Ok(next.run(request).await)
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

pub(crate) async fn readiness_middleware(
    axum::extract::State(state): axum::extract::State<AppState>,
    request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let path = request.uri().path();
    let side_effecting_get =
        path == "/api/v1/orchestrator/agents/poll" || path.starts_with("/ws/pty/");
    let protected = request.method() != axum::http::Method::GET
        && request.method() != axum::http::Method::HEAD
        && request.method() != axum::http::Method::OPTIONS
        || side_effecting_get;
    let database_unavailable = state.database_unavailable.lock().await.clone();
    if let Some(reason) = database_unavailable {
        let core_data = path.starts_with("/api/v1/")
            && !path.starts_with("/api/v1/dashboard")
            && !path.starts_with("/api/v1/pty")
            && path != "/api/v1/health"
            && path != "/api/v1/readiness";
        if core_data && !protected {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({
                    "error": "database_unavailable",
                    "reason": reason,
                })),
            )
                .into_response();
        }
    }
    if !protected || path == "/api/v1/health" {
        return next.run(request).await;
    }
    let delegated = path.starts_with("/api/v1/dashboard")
        || path == "/api/v1/pty"
        || path.starts_with("/ws/pty/");
    if delegated {
        let permit_dir = state.crosslink_dir.clone();
        let permit = tokio::task::spawn_blocking(move || {
            crate::reconcile::readiness::acquire_mutation_permit(&permit_dir)
        })
        .await;
        return match permit {
            Ok(Ok(permit)) => {
                let response = next.run(request).await;
                drop(permit);
                response
            }
            Ok(Err(error)) => readiness_unavailable(&state, &error),
            Err(error) => readiness_unavailable(
                &state,
                &anyhow::anyhow!("repository mutation permit task failed: {error}"),
            ),
        };
    }
    let permit_dir = state.crosslink_dir.clone();
    let permit = tokio::task::spawn_blocking(move || {
        crate::reconcile::readiness::acquire_mutation_operation_permit(&permit_dir)
    })
    .await;
    match permit {
        Ok(Ok(permit)) => {
            if let Err(error) = state.reopen_db_writable().await {
                drop(permit);
                return readiness_unavailable(
                    &state,
                    &anyhow::anyhow!("repository database is unavailable: {error}"),
                );
            }
            let response = next.run(request).await;
            drop(permit);
            response
        }
        Ok(Err(error)) => readiness_unavailable(&state, &error),
        Err(error) => readiness_unavailable(
            &state,
            &anyhow::anyhow!("repository mutation permit task failed: {error}"),
        ),
    }
}

fn readiness_unavailable(state: &AppState, error: &anyhow::Error) -> Response {
    let response = crate::reconcile::readiness::read_record(&state.crosslink_dir)
        .ok()
        .flatten()
        .filter(|record| {
            crate::reconcile::readiness::validate_record(&state.crosslink_dir, record).is_ok()
        })
        .map_or_else(
            || crate::reconcile::readiness::DaemonResponse::error(format!("{error:#}")),
            |record| {
                if record.state.grants_mutations() {
                    crate::reconcile::readiness::DaemonResponse::live_error(
                        &record,
                        format!("{error:#}"),
                    )
                } else {
                    crate::reconcile::readiness::DaemonResponse::from_record(&record)
                }
            },
        );
    (StatusCode::SERVICE_UNAVAILABLE, Json(response)).into_response()
}

pub async fn run(
    port: u16,
    dashboard_dir: Option<PathBuf>,
    db: Database,
    crosslink_dir: PathBuf,
    database_unavailable: Option<String>,
) -> Result<()> {
    run_with_dashboard_db(
        port,
        dashboard_dir,
        db,
        crosslink_dir,
        None,
        database_unavailable,
    )
    .await
}

pub async fn run_with_dashboard_db(
    port: u16,
    dashboard_dir: Option<PathBuf>,
    db: Database,
    crosslink_dir: PathBuf,
    dashboard_db_path: Option<PathBuf>,
    database_unavailable: Option<String>,
) -> Result<()> {
    let mut state =
        AppState::new(db, crosslink_dir.clone()).with_database_unavailable(database_unavailable);

    let poll_handle = if let Some(path) = dashboard_db_path {
        state = state.with_dashboard_db(path.clone());
        let cancel = tokio_util::sync::CancellationToken::new();
        let cancel_clone = cancel.clone();
        let tx = state.ws_tx.clone();
        let poll_crosslink_dir = crosslink_dir.clone();
        let handle = tokio::spawn(async move {
            crate::dashboard::poll::run(path, poll_crosslink_dir, cancel_clone, Some(tx)).await;
        });
        Some((cancel, handle))
    } else {
        None
    };

    watcher::start_watcher(crosslink_dir, state.ws_tx.clone());

    let localhost: axum::http::HeaderValue = "http://localhost:5173".parse()?;
    let loopback: axum::http::HeaderValue = "http://127.0.0.1:5173".parse()?;
    let cors = CorsLayer::new()
        .allow_origin([localhost, loopback])
        .allow_methods(tower_http::cors::Any)
        .allow_headers([
            axum::http::header::CONTENT_TYPE,
            axum::http::header::AUTHORIZATION,
            axum::http::header::ACCEPT,
        ]);

    let has_dashboard = dashboard_dir.is_some();

    let app = routes::build_router(state.clone(), dashboard_dir)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .layer(DefaultBodyLimit::max(MAX_BODY_SIZE))
        .layer(cors);

    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    println!("crosslink dashboard: listening on http://{addr}");

    if has_dashboard {
        println!(
            "  Dashboard: http://{addr}/?token={}  (from --dashboard-dir)",
            state.auth_token
        );
    } else {
        println!("  Dashboard: http://{addr}/?token={}", state.auth_token);
    }
    println!("  API:       http://{addr}/api/v1/health");
    println!("  WebSocket: ws://{addr}/ws");
    println!("  Auth:      Bearer {}", state.auth_token);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    let serve_result = axum::serve(listener, app).await;

    if let Some((cancel, handle)) = poll_handle {
        cancel.cancel();
        let _ = handle.await;
    }

    serve_result?;
    Ok(())
}
