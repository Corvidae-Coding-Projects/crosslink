use axum::{
    body::Body,
    http::{header, StatusCode, Uri},
    response::{IntoResponse, Response},
};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "../dashboard/dist/"]
struct DashboardAssets;

pub async fn serve_embedded(uri: Uri) -> Response {
    let raw_path = uri.path().trim_start_matches('/');

    if raw_path.starts_with("api/") || raw_path == "api" || raw_path.starts_with("ws") {
        return StatusCode::NOT_FOUND.into_response();
    }

    let path = if raw_path.is_empty() {
        "index.html"
    } else {
        raw_path
    };

    if let Some(asset) = DashboardAssets::get(path) {
        let mime = mime_guess::from_path(path).first_or_octet_stream();
        return Response::builder()
            .header(header::CONTENT_TYPE, mime.as_ref())
            .body(Body::from(asset.data.into_owned()))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
    }

    if let Some(index) = DashboardAssets::get("index.html") {
        return Response::builder()
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .body(Body::from(index.data.into_owned()))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
    }

    StatusCode::NOT_FOUND.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_index_html_is_embedded() {
        assert!(
            DashboardAssets::get("index.html").is_some(),
            "dashboard/dist/index.html must be embedded — run \
             `npm --prefix dashboard run build` before `cargo build`"
        );
    }

    #[tokio::test]
    async fn test_serve_embedded_returns_index_for_root() {
        let uri: Uri = "/".parse().unwrap();
        let resp = serve_embedded(uri).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        assert!(
            ct.starts_with("text/html"),
            "root should serve HTML, got: {ct}"
        );
    }

    #[tokio::test]
    async fn test_serve_embedded_spa_fallback_for_unknown_path() {
        let uri: Uri = "/some/deep/client/route".parse().unwrap();
        let resp = serve_embedded(uri).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        assert!(ct.starts_with("text/html"));
    }

    #[tokio::test]
    async fn test_serve_embedded_returns_404_for_unknown_api_path() {
        let uri: Uri = "/api/v1/nonexistent".parse().unwrap();
        let resp = serve_embedded(uri).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_serve_embedded_returns_404_for_unknown_ws_path() {
        let uri: Uri = "/ws/unknown".parse().unwrap();
        let resp = serve_embedded(uri).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
