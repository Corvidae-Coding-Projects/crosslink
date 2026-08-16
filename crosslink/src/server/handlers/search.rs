use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::Json,
};
use serde::Serialize;
use serde_json::Value;

use crate::{
    knowledge::KnowledgeManager,
    server::{
        errors::{bad_request, internal_error},
        state::AppState,
        types::{ApiError, KnowledgeSearchQuery},
    },
};

#[derive(Debug, Clone, Serialize)]
pub struct SearchResultItem {
    pub kind: String,

    pub title: String,

    pub snippet: String,

    pub id: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub issue_id: Option<i64>,
}

pub async fn global_search(
    State(state): State<AppState>,
    Query(params): Query<KnowledgeSearchQuery>,
) -> Result<Json<Value>, (StatusCode, Json<ApiError>)> {
    let query = params.q.trim().to_string();
    if query.is_empty() {
        return Err(bad_request("Search query 'q' cannot be empty"));
    }

    let mut results: Vec<SearchResultItem> = Vec::new();

    {
        let db = state.db().await;

        let issues = db
            .search_issues(&query)
            .map_err(|e| internal_error("Issue search failed", e))?;

        let matching_comments = db
            .search_comments(&query)
            .map_err(|e| internal_error("Failed to search comments", e))?;

        drop(db);

        for issue in issues {
            let snippet = issue
                .description
                .as_deref()
                .unwrap_or("")
                .chars()
                .take(200)
                .collect::<String>();

            results.push(SearchResultItem {
                kind: "issue".to_string(),
                title: issue.title.clone(),
                snippet,
                id: issue.id.to_string(),
                issue_id: None,
            });
        }

        for (comment, issue_id, issue_title) in matching_comments {
            let snippet = comment.content.chars().take(200).collect::<String>();
            results.push(SearchResultItem {
                kind: "comment".to_string(),
                title: format!("Comment on #{issue_id}: {issue_title}"),
                snippet,
                id: comment.id.to_string(),
                issue_id: Some(issue_id),
            });
        }
    }

    {
        let km = KnowledgeManager::new(&state.crosslink_dir)
            .map_err(|e| internal_error("Failed to initialize knowledge manager", e))?;

        if km.is_initialized() {
            let matches = km
                .search_content(&query, 1)
                .map_err(|e| internal_error("Knowledge search failed", e))?;

            let pages = km.list_pages().unwrap_or_default();
            let title_map: std::collections::HashMap<String, String> = pages
                .into_iter()
                .map(|p| (p.slug.clone(), p.frontmatter.title))
                .collect();

            let mut seen_slugs = std::collections::HashSet::new();
            for m in matches {
                if !seen_slugs.insert(m.slug.clone()) {
                    continue;
                }

                let title = title_map
                    .get(&m.slug)
                    .cloned()
                    .unwrap_or_else(|| m.slug.clone());

                let snippet = m
                    .context_lines
                    .iter()
                    .map(|(_, line): &(usize, String)| line.as_str())
                    .collect::<Vec<_>>()
                    .join(" ")
                    .chars()
                    .take(200)
                    .collect::<String>();

                results.push(SearchResultItem {
                    kind: "knowledge".to_string(),
                    title,
                    snippet,
                    id: m.slug,
                    issue_id: None,
                });
            }
        }
    }

    let total = results.len();
    Ok(Json(
        serde_json::json!({ "items": results, "total": total }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request, routing::get, Router};
    use serde_json::Value;
    use tower::ServiceExt;

    fn test_state(tmp_dir: &std::path::Path) -> AppState {
        let db_path = tmp_dir.join("test.db");
        let db = crate::db::Database::open(&db_path).unwrap();
        let crosslink_dir = tmp_dir.join(".crosslink");
        std::fs::create_dir_all(&crosslink_dir).unwrap();
        AppState::new(db, crosslink_dir)
    }

    fn build_router(state: AppState) -> Router {
        Router::new()
            .route("/search", get(global_search))
            .with_state(state)
    }

    #[tokio::test]
    async fn test_search_empty_query_returns_400() {
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(tmp.path());
        let app = build_router(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/search?q=")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_search_no_results() {
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(tmp.path());
        let app = build_router(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/search?q=nonexistent")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["total"], 0);
    }

    #[tokio::test]
    async fn test_search_finds_issues() {
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(tmp.path());

        {
            let db = state.db.lock().await;
            db.create_issue("Fix authentication bug", None, "high")
                .unwrap();
        }

        let app = build_router(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/search?q=authentication")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(body["total"].as_u64().unwrap() >= 1);
        assert_eq!(body["items"][0]["kind"], "issue");
        assert!(body["items"][0]["title"]
            .as_str()
            .unwrap()
            .contains("authentication"));
    }

    #[tokio::test]
    async fn test_search_finds_comments() {
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(tmp.path());

        {
            let db = state.db.lock().await;
            db.create_issue("Some issue", None, "medium").unwrap();
            db.add_comment(1, "The frobulator is broken and needs replacement", "note")
                .unwrap();
        }

        let app = build_router(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/search?q=frobulator")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        let items = body["items"].as_array().unwrap();
        let comment_results: Vec<_> = items.iter().filter(|i| i["kind"] == "comment").collect();
        assert!(!comment_results.is_empty());
        assert!(comment_results[0]["snippet"]
            .as_str()
            .unwrap()
            .contains("frobulator"));
        assert_eq!(comment_results[0]["issue_id"], 1);
    }

    #[tokio::test]
    async fn test_search_finds_knowledge_pages() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_dir = tmp.path().join(".crosslink").join(".knowledge-cache");
        std::fs::create_dir_all(&cache_dir).unwrap();

        let page = "---\ntitle: \"Enzyme Kinetics\"\ntags: [biology]\nsources: []\ncontributors: []\ncreated: \"2026-01-01\"\nupdated: \"2026-01-01\"\n---\n\nMichaelis-Menten kinetics describes enzyme catalysis rates.\n";
        std::fs::write(cache_dir.join("enzyme-kinetics.md"), page).unwrap();

        let state = test_state(tmp.path());
        let app = build_router(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/search?q=Michaelis")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        let items = body["items"].as_array().unwrap();
        let knowledge_results: Vec<_> = items.iter().filter(|i| i["kind"] == "knowledge").collect();
        assert!(!knowledge_results.is_empty());
        assert_eq!(knowledge_results[0]["id"], "enzyme-kinetics");
        assert_eq!(knowledge_results[0]["title"], "Enzyme Kinetics");
    }

    #[tokio::test]
    async fn test_search_whitespace_only_query_returns_400() {
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(tmp.path());
        let app = build_router(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/search?q=+++")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_search_issue_without_description() {
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(tmp.path());

        {
            let db = state.db.lock().await;
            db.create_issue("Undescribed widget", None, "low").unwrap();
        }

        let app = build_router(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/search?q=widget")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(body["total"].as_u64().unwrap() >= 1);
        let items = body["items"].as_array().unwrap();
        let issue_result = items.iter().find(|i| i["kind"] == "issue").unwrap();

        assert_eq!(issue_result["snippet"], "");
    }

    #[test]
    fn test_helper_functions_directly() {
        let (status, json) = crate::server::errors::internal_error("ctx", "detail");
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(json.error, "ctx");
        assert_eq!(json.detail.as_deref(), Some("detail"));

        let (status, json) = crate::server::errors::bad_request("bad input");
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(json.error, "bad request");
        assert_eq!(json.detail.as_deref(), Some("bad input"));
    }

    #[tokio::test]
    async fn test_search_knowledge_page_title_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_dir = tmp.path().join(".crosslink").join(".knowledge-cache");
        std::fs::create_dir_all(&cache_dir).unwrap();

        let page = "No frontmatter here. Just a raw doc about widgets.\n";
        std::fs::write(cache_dir.join("raw-widget-doc.md"), page).unwrap();

        let state = test_state(tmp.path());
        let app = build_router(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/search?q=widgets")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        let items = body["items"].as_array().unwrap();
        let knowledge_results: Vec<_> = items.iter().filter(|i| i["kind"] == "knowledge").collect();

        assert!(!knowledge_results.is_empty());
        assert_eq!(knowledge_results[0]["id"], "raw-widget-doc");

        assert_eq!(knowledge_results[0]["title"], "raw-widget-doc");
    }
}
