use anyhow::Result;
use chrono::Utc;

use crate::application::{QueryService, RepositoryService};

use super::{Signal, SignalKind, Source, SourceKind};

pub struct InternalHygieneConfig {
    pub stale_threshold_days: i64,
}

impl Default for InternalHygieneConfig {
    fn default() -> Self {
        Self {
            stale_threshold_days: 30,
        }
    }
}

pub struct InternalHygieneSource {
    config: InternalHygieneConfig,
    db_path: std::path::PathBuf,
}

impl InternalHygieneSource {
    pub fn new(crosslink_dir: &std::path::Path, config: InternalHygieneConfig) -> Self {
        Self {
            config,
            db_path: crosslink_dir.join("issues.db"),
        }
    }

    fn find_stale_issues(&self) -> Result<Vec<Signal>> {
        let db = crate::db::Database::open(&self.db_path)?;
        let service = RepositoryService::projection(&db);
        let threshold = Utc::now() - chrono::Duration::days(self.config.stale_threshold_days);
        let now = Utc::now();
        let mut issues = service.list_issues(Some("open"), None, None)?;
        issues.retain(|issue| issue.updated_at < threshold);
        issues.sort_by_key(|issue| issue.updated_at);
        let signals = issues
            .into_iter()
            .take(20)
            .map(|issue| Signal {
                source: SourceKind::Internal,
                kind: SignalKind::StaleIssue,
                reference: format!("CL#{}:stale", issue.id),
                title: format!("Stale issue: {}", issue.title),
                body: format!(
                    "Issue #{} has not been updated since {}.",
                    issue.id, issue.updated_at
                ),
                metadata: serde_json::json!({
                    "issue_id": issue.id,
                    "last_updated": issue.updated_at,
                    "stale_days": self.config.stale_threshold_days,
                }),
                detected_at: now,
            })
            .collect();

        Ok(signals)
    }

    fn find_orphaned_subissues(&self) -> Result<Vec<Signal>> {
        let db = crate::db::Database::open(&self.db_path)?;
        let service = RepositoryService::projection(&db);
        let now = Utc::now();
        let issues = service.list_issues(Some("open"), None, None)?;
        let mut orphaned = Vec::new();
        for issue in issues {
            let Some(parent_id) = issue.parent_id else {
                continue;
            };
            let Some(parent) = service.get_issue(parent_id)? else {
                continue;
            };
            if parent.status != crate::models::IssueStatus::Closed {
                continue;
            }
            orphaned.push(Signal {
                source: SourceKind::Internal,
                kind: SignalKind::StaleIssue,
                reference: format!("CL#{}:orphan", issue.id),
                title: format!("Orphaned subissue: {}", issue.title),
                body: format!(
                    "Issue #{} is open but its parent #{} is closed.",
                    issue.id, parent_id
                ),
                metadata: serde_json::json!({
                    "issue_id": issue.id,
                    "parent_id": parent_id,
                }),
                detected_at: now,
            });
            if orphaned.len() == 20 {
                break;
            }
        }
        Ok(orphaned)
    }

    fn find_unlabeled_issues(&self) -> Result<Vec<Signal>> {
        let db = crate::db::Database::open(&self.db_path)?;
        let service = RepositoryService::projection(&db);
        let now = Utc::now();
        let issues = service.list_issues(Some("open"), None, None)?;
        let mut signals = Vec::new();
        for issue in issues {
            if !service.get_labels(issue.id)?.is_empty() {
                continue;
            }
            signals.push(Signal {
                source: SourceKind::Internal,
                kind: SignalKind::StaleIssue,
                reference: format!("CL#{}:unlabeled", issue.id),
                title: format!("Unlabeled issue: {}", issue.title),
                body: format!("Issue #{} has no labels.", issue.id),
                metadata: serde_json::json!({
                    "issue_id": issue.id,
                }),
                detected_at: now,
            });
            if signals.len() == 20 {
                break;
            }
        }
        Ok(signals)
    }
}

impl Source for InternalHygieneSource {
    fn name(&self) -> &'static str {
        "internal-hygiene"
    }

    fn poll(&mut self) -> Result<Vec<Signal>> {
        let mut signals = Vec::new();

        match self.find_stale_issues() {
            Ok(s) => signals.extend(s),
            Err(e) => tracing::warn!("stale issue scan failed: {e}"),
        }
        match self.find_orphaned_subissues() {
            Ok(s) => signals.extend(s),
            Err(e) => tracing::warn!("orphan scan failed: {e}"),
        }
        match self.find_unlabeled_issues() {
            Ok(s) => signals.extend(s),
            Err(e) => tracing::warn!("unlabeled scan failed: {e}"),
        }

        Ok(signals)
    }
}
