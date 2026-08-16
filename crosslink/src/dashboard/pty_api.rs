use anyhow::Result;
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, State,
    },
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use base64::Engine;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::error::RecvError;

use super::actions;
use super::db::DashboardDb;
use super::pty::{spawn_pty, ClientFrame, PtySessionView, ServerFrame, DEFAULT_MAX_CONCURRENT};
use crate::server::state::AppState;

pub fn rest_router() -> Router<AppState> {
    Router::new()
        .route("/pty", post(spawn_session))
        .route("/pty/sessions", get(list_sessions))
}

pub fn ws_router() -> Router<AppState> {
    Router::new().route("/pty/{session_id}", get(ws_handler))
}

#[derive(Debug, Deserialize)]
struct SpawnRequest {
    project_slug: String,

    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default = "default_rows")]
    rows: u16,
    #[serde(default = "default_cols")]
    cols: u16,
}

const fn default_rows() -> u16 {
    40
}
const fn default_cols() -> u16 {
    140
}

#[derive(Debug, Serialize)]
struct SpawnResponse {
    session_id: String,
    project_slug: String,
    command: String,
    started_at: String,
}

async fn spawn_session(
    State(state): State<AppState>,
    Json(req): Json<SpawnRequest>,
) -> Result<Json<SpawnResponse>, PtyError> {
    if state.pty_registry.len().await >= DEFAULT_MAX_CONCURRENT {
        return Err(PtyError::TooMany);
    }
    let db_path = state
        .dashboard_db_path
        .as_ref()
        .ok_or_else(|| PtyError::BadRequest("dashboard DB not configured".into()))?
        .clone();

    let slug_lookup = req.project_slug.clone();
    let project = tokio::task::spawn_blocking(move || -> Result<_, PtyError> {
        let db =
            DashboardDb::open(&db_path).map_err(|e| PtyError::Internal(format!("open db: {e}")))?;
        actions::find_project_by_slug(&db, &slug_lookup)
            .map_err(|e| PtyError::Internal(format!("lookup: {e}")))?
            .ok_or_else(|| PtyError::NotFound(format!("project '{slug_lookup}' not tracked")))
    })
    .await
    .map_err(|e| PtyError::Internal(format!("lookup task panicked: {e}")))??;

    let session = spawn_pty(
        &project.clone_path,
        &req.command,
        &req.args,
        req.rows,
        req.cols,
    )
    .map_err(|e| PtyError::Internal(format!("spawn pty: {e}")))?;

    let resp = SpawnResponse {
        session_id: session.id.clone(),
        project_slug: req.project_slug,
        command: req.command,
        started_at: session.started_at.clone(),
    };
    state.pty_registry.insert(session).await;
    Ok(Json(resp))
}

async fn list_sessions(
    State(state): State<AppState>,
) -> Result<Json<Vec<PtySessionView>>, PtyError> {
    let mut out = Vec::new();
    let ids = state.pty_registry.list_ids().await;
    for id in ids {
        if let Some(s) = state.pty_registry.get(&id).await {
            out.push(PtySessionView::from(s.as_ref()));
        }
    }
    Ok(Json(out))
}

async fn ws_handler(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    ws: WebSocketUpgrade,
) -> Response {
    let Some(session) = state.pty_registry.get(&session_id).await else {
        return (StatusCode::NOT_FOUND, "no such pty session").into_response();
    };
    ws.on_upgrade(move |socket| handle_socket(socket, session))
}

async fn handle_socket(mut socket: WebSocket, session: std::sync::Arc<super::pty::PtySession>) {
    let engine = base64::engine::general_purpose::STANDARD;

    let (mut rx, replay) = session.subscribe();
    if !replay.is_empty() {
        let frame = ServerFrame::Stdout {
            data: engine.encode(&replay),
        };
        if let Ok(s) = serde_json::to_string(&frame) {
            let _ = socket.send(Message::Text(s.into())).await;
        }
    }

    loop {
        tokio::select! {

            broadcast_msg = rx.recv() => {
                match broadcast_msg {
                    Ok(bytes) if bytes.is_empty() => {

                        let code = session.exit_code.lock().ok().and_then(|g| *g);
                        let frame = ServerFrame::Exit { code };
                        if let Ok(s) = serde_json::to_string(&frame) {
                            let _ = socket.send(Message::Text(s.into())).await;
                        }
                        let _ = socket.send(Message::Close(None)).await;
                        break;
                    }
                    Ok(bytes) => {
                        let frame = ServerFrame::Stdout { data: engine.encode(&bytes) };
                        if let Ok(s) = serde_json::to_string(&frame) {
                            if socket.send(Message::Text(s.into())).await.is_err() {
                                break;
                            }
                        }
                    }
                    Err(RecvError::Lagged(_)) => {

                        let (_new_rx, snapshot) = session.subscribe();
                        let frame = ServerFrame::Stdout { data: engine.encode(&snapshot) };
                        if let Ok(s) = serde_json::to_string(&frame) {
                            let _ = socket.send(Message::Text(s.into())).await;
                        }
                    }
                    Err(RecvError::Closed) => break,
                }
            }

            client_msg = socket.recv() => {
                match client_msg {
                    Some(Ok(Message::Text(text))) => {
                        let Ok(frame) = serde_json::from_str::<ClientFrame>(&text) else {
                            continue;
                        };
                        match frame {
                            ClientFrame::Stdin { data } => {
                                if let Ok(decoded) = engine.decode(&data) {
                                    let _ = session.write_stdin(&decoded);
                                }
                            }
                            ClientFrame::Resize { rows, cols } => {
                                let _ = session.resize(rows, cols);
                            }
                        }
                    }
                    Some(Ok(Message::Binary(bytes))) => {


                        let _ = session.write_stdin(&bytes);
                    }
                    Some(Ok(Message::Close(_)) | Err(_)) | None => break,
                    Some(Ok(_)) => {}
                }
            }
        }
    }
}

#[derive(Debug)]
enum PtyError {
    BadRequest(String),
    NotFound(String),
    TooMany,
    Internal(String),
}

impl IntoResponse for PtyError {
    fn into_response(self) -> Response {
        let (status, msg) = match self {
            Self::BadRequest(m) => (StatusCode::BAD_REQUEST, m),
            Self::NotFound(m) => (StatusCode::NOT_FOUND, m),
            Self::TooMany => (
                StatusCode::TOO_MANY_REQUESTS,
                format!("PTY session limit ({DEFAULT_MAX_CONCURRENT}) reached"),
            ),
            Self::Internal(m) => (StatusCode::INTERNAL_SERVER_ERROR, m),
        };
        (status, Json(serde_json::json!({"error": msg}))).into_response()
    }
}
