use anyhow::Result;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};

use super::db::DashboardDb;
use super::github;
use crate::server::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/github/config", get(get_config).post(set_config))
        .route("/github/orgs/{org}/repos", get(list_repos))
        .route("/github/orgs/{org}/track-all", post(track_all))
}

#[derive(Debug, Serialize)]
struct ConfigView {
    token_present: bool,

    token_fingerprint: Option<String>,
    default_org: Option<String>,

    token_source: Option<&'static str>,
}

const fn source_tag(s: github::TokenSource) -> &'static str {
    match s {
        github::TokenSource::Stored => "stored",
        github::TokenSource::GhCli => "gh-cli",
    }
}

#[allow(clippy::option_option)]
#[derive(Debug, Deserialize)]
struct SetConfigBody {
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    default_org: Option<Option<String>>,
}

async fn get_config(State(state): State<AppState>) -> Result<Json<ConfigView>, GitHubApiError> {
    let db_path = require_db_path(&state)?;
    let view = tokio::task::spawn_blocking(move || -> Result<ConfigView, GitHubApiError> {
        let db = DashboardDb::open(&db_path)
            .map_err(|e| GitHubApiError::Internal(format!("open db: {e}")))?;
        let effective = github::get_effective_token(&db, &db_path)
            .map_err(|e| GitHubApiError::Internal(format!("read token: {e}")))?;
        let default_org = github::get_plain(&db, github::KEY_DEFAULT_ORG)
            .map_err(|e| GitHubApiError::Internal(format!("read org: {e}")))?;
        Ok(match effective {
            Some((tok, src)) => ConfigView {
                token_present: true,
                token_fingerprint: Some(mask_token(&tok)),
                default_org,
                token_source: Some(source_tag(src)),
            },
            None => ConfigView {
                token_present: false,
                token_fingerprint: None,
                default_org,
                token_source: None,
            },
        })
    })
    .await
    .map_err(|e| GitHubApiError::Internal(format!("task panicked: {e}")))??;
    Ok(Json(view))
}

async fn set_config(
    State(state): State<AppState>,
    Json(body): Json<SetConfigBody>,
) -> Result<Json<ConfigView>, GitHubApiError> {
    let db_path = require_db_path(&state)?;

    if let Some(ref t) = body.token {
        if !t.is_empty() && t.len() < 10 {
            return Err(GitHubApiError::BadRequest(
                "token looks too short — paste the full PAT".into(),
            ));
        }
    }

    let view = tokio::task::spawn_blocking(move || -> Result<ConfigView, GitHubApiError> {
        let db = DashboardDb::open(&db_path)
            .map_err(|e| GitHubApiError::Internal(format!("open db: {e}")))?;
        if let Some(t) = body.token.as_deref() {
            github::set_token(&db, t, &db_path)
                .map_err(|e| GitHubApiError::Internal(format!("set token: {e}")))?;
        }
        if let Some(org_change) = body.default_org {
            github::set_plain(&db, github::KEY_DEFAULT_ORG, org_change.as_deref())
                .map_err(|e| GitHubApiError::Internal(format!("set org: {e}")))?;
        }
        let effective = github::get_effective_token(&db, &db_path)
            .map_err(|e| GitHubApiError::Internal(format!("read token: {e}")))?;
        let default_org = github::get_plain(&db, github::KEY_DEFAULT_ORG)
            .map_err(|e| GitHubApiError::Internal(format!("read org: {e}")))?;
        Ok(match effective {
            Some((tok, src)) => ConfigView {
                token_present: true,
                token_fingerprint: Some(mask_token(&tok)),
                default_org,
                token_source: Some(source_tag(src)),
            },
            None => ConfigView {
                token_present: false,
                token_fingerprint: None,
                default_org,
                token_source: None,
            },
        })
    })
    .await
    .map_err(|e| GitHubApiError::Internal(format!("task panicked: {e}")))??;
    Ok(Json(view))
}

fn mask_token(t: &str) -> String {
    if t.len() <= 12 {
        return "*".repeat(t.len());
    }
    let (head, tail) = t.split_at(8);
    let tail_start = tail.len().saturating_sub(4);
    format!("{head}…{}", &tail[tail_start..])
}

#[derive(Debug, Serialize)]
struct RepoHit {
    owner: String,
    repo: String,
    full_name: String,
    default_branch: String,
    ssh_url: String,
    https_url: String,

    has_hub_branch: bool,
}

async fn list_repos(
    State(state): State<AppState>,
    Path(org): Path<String>,
) -> Result<Json<Vec<RepoHit>>, GitHubApiError> {
    let db_path = require_db_path(&state)?;
    let token = load_token(&db_path).await?;

    let hits = enumerate_org_crosslink_repos(&org, &token)
        .await
        .map_err(|e| GitHubApiError::Upstream(e.to_string()))?;
    Ok(Json(hits))
}

#[derive(Debug, Deserialize)]
struct TrackAllBody {
    #[serde(default)]
    clone_root: Option<String>,

    #[serde(default)]
    init: bool,

    #[serde(default)]
    agent_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct TrackAllOutcome {
    tracked: Vec<String>,
    skipped: Vec<SkippedRepo>,
}

#[derive(Debug, Serialize)]
struct SkippedRepo {
    slug: String,
    reason: String,
}

async fn track_all(
    State(state): State<AppState>,
    Path(org): Path<String>,
    Json(body): Json<TrackAllBody>,
) -> Result<Json<TrackAllOutcome>, GitHubApiError> {
    let db_path = require_db_path(&state)?;
    let token = load_token(&db_path).await?;

    let hits = enumerate_org_crosslink_repos(&org, &token)
        .await
        .map_err(|e| GitHubApiError::Upstream(e.to_string()))?;

    let clone_root = body
        .clone_root
        .clone()
        .unwrap_or_else(|| std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()));

    let init_config = if body.init {
        let id = body
            .agent_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                GitHubApiError::BadRequest(
                    "init=true requires agent_id (alphanumeric + hyphens + underscores)".into(),
                )
            })?;
        Some(id.to_string())
    } else {
        None
    };

    let mut tracked = Vec::new();
    let mut skipped = Vec::new();
    for hit in hits {
        let slug = hit.full_name.clone();

        let target = std::path::PathBuf::from(&clone_root).join(&hit.repo);
        let result = tokio::task::spawn_blocking({
            let db_path = db_path.clone();
            let target = target.clone();
            let ssh_url = hit.ssh_url.clone();
            let https_url = hit.https_url.clone();
            let slug = slug.clone();
            let init = init_config.clone();
            move || {
                ensure_clone_and_track(
                    &db_path,
                    &target,
                    &ssh_url,
                    &https_url,
                    &slug,
                    init.as_deref(),
                )
            }
        })
        .await
        .map_err(|e| GitHubApiError::Internal(format!("track task panicked: {e}")))?;
        match result {
            Ok(()) => tracked.push(slug),
            Err(e) => skipped.push(SkippedRepo {
                slug,
                reason: e.to_string(),
            }),
        }
    }

    Ok(Json(TrackAllOutcome { tracked, skipped }))
}

fn ensure_clone_and_track(
    db_path: &std::path::Path,
    target: &std::path::Path,
    ssh_url: &str,
    https_url: &str,
    slug: &str,
    init_agent_id: Option<&str>,
) -> Result<()> {
    let freshly_cloned = !target.is_dir();
    if freshly_cloned {
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let clone_res = std::process::Command::new("git")
            .args([
                "clone",
                "--quiet",
                ssh_url,
                target.to_string_lossy().as_ref(),
            ])
            .status();
        let cloned = matches!(clone_res, Ok(s) if s.success());
        if !cloned {
            let https = std::process::Command::new("git")
                .args([
                    "clone",
                    "--quiet",
                    https_url,
                    target.to_string_lossy().as_ref(),
                ])
                .status()?;
            anyhow::ensure!(https.success(), "git clone failed for {slug}");
        }
    }

    if let Some(agent_id) = init_agent_id {
        if super::projects::write_capability(target) != super::projects::WriteCapability::Ready {
            super::projects::run_init_and_agent_in(target, agent_id)?;
        }
    }

    super::projects::track_at_path(target, Some(slug), db_path)?;
    Ok(())
}

fn require_db_path(state: &AppState) -> Result<std::path::PathBuf, GitHubApiError> {
    state
        .dashboard_db_path
        .clone()
        .ok_or_else(|| GitHubApiError::BadRequest("dashboard DB not configured".into()))
}

async fn load_token(db_path: &std::path::Path) -> Result<String, GitHubApiError> {
    let db_path_owned = db_path.to_path_buf();
    let resolved = tokio::task::spawn_blocking(move || {
        let db = DashboardDb::open(&db_path_owned).ok()?;
        github::get_effective_token(&db, &db_path_owned)
            .ok()
            .flatten()
    })
    .await
    .map_err(|e| GitHubApiError::Internal(format!("load token task panicked: {e}")))?;
    resolved.map(|(tok, _src)| tok).ok_or_else(|| {
        GitHubApiError::BadRequest(
            "no GitHub token available — store a PAT via POST /github/config, \
             or run `gh auth login` in a shell"
                .into(),
        )
    })
}

async fn enumerate_org_crosslink_repos(org: &str, token: &str) -> Result<Vec<RepoHit>> {
    let client = reqwest::Client::builder()
        .user_agent("crosslink-dashboard")
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let mut out = Vec::new();
    let mut page = 1u32;
    loop {
        let url =
            format!("https://api.github.com/orgs/{org}/repos?per_page=100&page={page}&type=all");
        let resp = client
            .get(&url)
            .bearer_auth(token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await?;
        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            anyhow::bail!("GitHub API returned 401 — token invalid or lacks org access");
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("GitHub API {status}: {}", body.trim());
        }
        let repos: Vec<RepoListItem> = resp.json().await?;
        if repos.is_empty() {
            break;
        }
        for repo in &repos {
            let mut found = false;
            for branch in ["crosslink%2Fhub", "crosslink%2Fcheckpoint"] {
                let check_url = format!(
                    "https://api.github.com/repos/{}/{}/branches/{branch}",
                    repo.owner.login, repo.name
                );
                let check = client
                    .get(&check_url)
                    .bearer_auth(token)
                    .header("Accept", "application/vnd.github+json")
                    .send()
                    .await?;
                if check.status().is_success() {
                    found = true;
                    break;
                }
            }
            if found {
                out.push(RepoHit {
                    owner: repo.owner.login.clone(),
                    repo: repo.name.clone(),
                    full_name: repo.full_name.clone(),
                    default_branch: repo.default_branch.clone(),
                    ssh_url: repo.ssh_url.clone(),
                    https_url: repo.clone_url.clone(),
                    has_hub_branch: true,
                });
            }
        }
        if repos.len() < 100 {
            break;
        }
        page += 1;
    }
    Ok(out)
}

#[derive(Debug, Deserialize)]
struct RepoListItem {
    name: String,
    full_name: String,
    default_branch: String,
    ssh_url: String,
    clone_url: String,
    owner: RepoOwner,
}

#[derive(Debug, Deserialize)]
struct RepoOwner {
    login: String,
}

#[derive(Debug)]
enum GitHubApiError {
    BadRequest(String),
    Upstream(String),
    Internal(String),
}

impl IntoResponse for GitHubApiError {
    fn into_response(self) -> Response {
        let (status, msg) = match self {
            Self::BadRequest(m) => (StatusCode::BAD_REQUEST, m),
            Self::Upstream(m) => (StatusCode::BAD_GATEWAY, m),
            Self::Internal(m) => (StatusCode::INTERNAL_SERVER_ERROR, m),
        };
        (status, Json(serde_json::json!({"error": msg}))).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mask_token_short() {
        assert_eq!(mask_token(""), "");
        assert_eq!(mask_token("abc"), "***");
        assert_eq!(mask_token("0123456789ab"), "************");
    }

    #[test]
    fn test_mask_token_realistic() {
        let s = mask_token("ghp_1234567890abcdefghij");
        assert!(s.starts_with("ghp_1234"));
        assert!(s.ends_with("ghij"));
        assert!(s.contains('…'));
    }
}
