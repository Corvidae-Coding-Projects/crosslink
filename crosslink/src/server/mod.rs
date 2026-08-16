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
use axum::response::Response;
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

    if path == "/api/v1/health" || path == "/ws" || !path.starts_with("/api/") {
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

pub async fn run(
    port: u16,
    dashboard_dir: Option<PathBuf>,
    db: Database,
    crosslink_dir: PathBuf,
) -> Result<()> {
    run_with_dashboard_db(port, dashboard_dir, db, crosslink_dir, None).await
}

pub async fn run_with_dashboard_db(
    port: u16,
    dashboard_dir: Option<PathBuf>,
    db: Database,
    crosslink_dir: PathBuf,
    dashboard_db_path: Option<PathBuf>,
) -> Result<()> {
    let mut state = AppState::new(db, crosslink_dir.clone());

    let poll_handle = if let Some(path) = dashboard_db_path {
        state = state.with_dashboard_db(path.clone());
        let cancel = tokio_util::sync::CancellationToken::new();
        let cancel_clone = cancel.clone();
        let tx = state.ws_tx.clone();
        let handle = tokio::spawn(async move {
            crate::dashboard::poll::run(path, cancel_clone, Some(tx)).await;
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
