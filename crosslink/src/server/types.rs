use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub use crate::models::{Comment, Issue, Milestone, Session};

#[derive(Debug, Clone, Serialize)]
pub struct HealthResponse {
    pub status: String,
    pub version: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ApiPriority {
    Low,
    #[default]
    Medium,
    High,
}

impl std::fmt::Display for ApiPriority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Low => write!(f, "low"),
            Self::Medium => write!(f, "medium"),
            Self::High => write!(f, "high"),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateIssueRequest {
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub priority: ApiPriority,
    #[serde(default)]
    pub parent_id: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UpdateIssueRequest {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub priority: Option<ApiPriority>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateSubissueRequest {
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub priority: ApiPriority,
}

#[derive(Debug, Clone, Serialize)]
pub struct IssueDetail {
    #[serde(flatten)]
    pub issue: Issue,
    pub labels: Vec<String>,
    pub comments: Vec<Comment>,
    pub blockers: Vec<i64>,
    pub blocking: Vec<i64>,
    pub subissues: Vec<Issue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub milestone: Option<MilestoneSummary>,
}

#[derive(Debug, Clone, Serialize)]
pub struct IssueSummary {
    #[serde(flatten)]
    pub issue: Issue,
    pub labels: Vec<String>,
    pub blocker_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct IssueListResponse {
    pub items: Vec<IssueSummary>,
    pub total: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IssueListQuery {
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub priority: Option<String>,
    #[serde(default)]
    pub search: Option<String>,
    #[serde(default)]
    pub parent_id: Option<i64>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CommentKind {
    #[default]
    Note,
    Plan,
    Decision,
    Observation,
    Blocker,
    Resolution,
    Result,
    Intervention,
}

impl std::fmt::Display for CommentKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Note => write!(f, "note"),
            Self::Plan => write!(f, "plan"),
            Self::Decision => write!(f, "decision"),
            Self::Observation => write!(f, "observation"),
            Self::Blocker => write!(f, "blocker"),
            Self::Resolution => write!(f, "resolution"),
            Self::Result => write!(f, "result"),
            Self::Intervention => write!(f, "intervention"),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateCommentRequest {
    pub content: String,
    #[serde(default)]
    pub kind: CommentKind,

    #[serde(default)]
    pub trigger_type: Option<String>,
    #[serde(default)]
    pub intervention_context: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AddLabelRequest {
    pub label: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AddBlockerRequest {
    pub blocker_id: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StartSessionRequest {
    #[serde(default)]
    pub agent_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EndSessionRequest {
    #[serde(default)]
    pub notes: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WorkOnIssueRequest {
    #[serde(default)]
    pub agent_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionResponse {
    #[serde(flatten)]
    pub session: Session,
}

#[derive(Debug, Clone, Serialize)]
pub struct MilestoneSummary {
    pub id: i64,
    pub name: String,
    pub status: crate::models::IssueStatus,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateMilestoneRequest {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AssignMilestoneRequest {
    pub issue_id: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct MilestoneDetail {
    #[serde(flatten)]
    pub milestone: Milestone,
    pub issue_count: usize,
    pub completed_count: usize,

    pub progress_percent: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct MilestoneListResponse {
    pub items: Vec<MilestoneDetail>,
    pub total: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MilestoneListQuery {
    #[serde(default)]
    pub status: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnowledgeSource {
    pub url: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accessed_at: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct KnowledgePage {
    pub slug: String,
    pub title: String,
    pub tags: Vec<String>,
    pub sources: Vec<KnowledgeSource>,
    pub contributors: Vec<String>,
    pub created: String,
    pub updated: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct KnowledgePageSummary {
    pub slug: String,
    pub title: String,
    pub tags: Vec<String>,
    pub updated: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateKnowledgePageRequest {
    pub slug: String,
    pub title: String,
    pub content: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub sources: Vec<KnowledgeSource>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct KnowledgeSearchQuery {
    pub q: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct KnowledgeSearchMatch {
    pub slug: String,
    pub title: String,
    pub line_number: usize,
    pub context_lines: Vec<(usize, String)>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AgentStatus {
    Active,

    Idle,

    Stale,

    Unknown,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentSummary {
    pub agent_id: String,
    pub machine_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub status: AgentStatus,
    pub last_heartbeat: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_issue_id: Option<i64>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree_path: Option<String>,

    pub locks: Vec<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentDetail {
    #[serde(flatten)]
    pub summary: AgentSummary,

    pub heartbeat_history: Vec<DateTime<Utc>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub kickoff_status: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LockEntry {
    pub issue_id: i64,
    pub agent_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    pub claimed_at: DateTime<Utc>,
    pub signed_by: String,

    pub age_seconds: i64,
    pub is_stale: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SyncStatusResponse {
    pub hub_initialized: bool,
    pub hub_branch: String,
    pub remote: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_fetch_at: Option<DateTime<Utc>>,
    pub active_lock_count: usize,
    pub stale_lock_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct SyncActionResponse {
    pub success: bool,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigResponse {
    pub tracking_mode: String,
    pub stale_lock_timeout_minutes: u64,
    pub remote: String,
    pub signing_enforcement: String,
    pub intervention_tracking: bool,
    pub auto_steal_stale_locks: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UpdateConfigRequest {
    #[serde(default)]
    pub tracking_mode: Option<String>,
    #[serde(default)]
    pub stale_lock_timeout_minutes: Option<u64>,
    #[serde(default)]
    pub remote: Option<String>,
    #[serde(default)]
    pub signing_enforcement: Option<String>,
    #[serde(default)]
    pub intervention_tracking: Option<bool>,
    #[serde(default)]
    pub auto_steal_stale_locks: Option<bool>,
}

pub use crate::db::UsageSummaryRow;
pub use crate::models::TokenUsage;

#[derive(Debug, Clone, Deserialize)]
pub struct CreateTokenUsageRequest {
    pub agent_id: String,
    #[serde(default = "default_provider")]
    pub provider: String,
    #[serde(default)]
    pub session_id: Option<i64>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    #[serde(default)]
    pub cached_input_tokens: Option<i64>,
    #[serde(default)]
    pub reasoning_output_tokens: Option<i64>,
    #[serde(default)]
    pub cache_read_tokens: Option<i64>,
    #[serde(default)]
    pub cache_creation_tokens: Option<i64>,
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default)]
    pub cost_estimate: Option<f64>,
    #[serde(default)]
    pub provider_metadata: Option<serde_json::Value>,
}

fn default_provider() -> String {
    "claude".to_string()
}

fn default_model() -> String {
    "unknown".to_string()
}

#[derive(Debug, Clone, Deserialize)]
pub struct TokenUsageListQuery {
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub session_id: Option<i64>,
    #[serde(default)]
    pub model: Option<String>,

    #[serde(default)]
    pub from: Option<String>,

    #[serde(default)]
    pub to: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TokenUsageSummaryQuery {
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default)]
    pub to: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TokenUsageListResponse {
    pub items: Vec<TokenUsage>,
    pub total: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct TokenUsageSummaryResponse {
    pub items: Vec<UsageSummaryRow>,
    pub total_input_tokens: i64,
    pub total_output_tokens: i64,
    pub total_cost: f64,
}

pub use crate::orchestrator::models::OrchestratorPlan;

#[derive(Debug, Clone, Deserialize)]
pub struct DecomposeRequest {
    pub document: String,

    #[serde(default)]
    pub slug: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum StageStatus {
    Pending,
    Running,
    Done,
    Failed,
    Skipped,

    Blocked,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ExecutionState {
    Idle,
    Running,
    Paused,
    Done,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExecutionStatus {
    pub plan_id: String,
    pub state: ExecutionState,
    pub current_phase_id: Option<String>,
    pub progress_percent: f64,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,

    pub stage_statuses: std::collections::HashMap<String, StageStatus>,

    pub stage_agents: std::collections::HashMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WsEventType {
    Heartbeat,
    AgentStatus,
    IssueUpdated,
    LockChanged,
    ExecutionProgress,

    DashboardProjectUpdated,

    DashboardAlertsChanged,
}

#[derive(Debug, Clone, Serialize)]
pub struct WsHeartbeatEvent {
    #[serde(rename = "type")]
    pub event_type: WsEventType,
    pub agent_id: String,
    pub timestamp: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_issue_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WsAgentStatusEvent {
    #[serde(rename = "type")]
    pub event_type: WsEventType,
    pub agent_id: String,
    pub status: AgentStatus,
}

#[derive(Debug, Clone, Serialize)]
pub struct WsIssueUpdatedEvent {
    #[serde(rename = "type")]
    pub event_type: WsEventType,
    pub issue_id: i64,

    pub field: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct WsLockChangedEvent {
    #[serde(rename = "type")]
    pub event_type: WsEventType,
    pub issue_id: i64,
    pub action: LockAction,
    pub agent_id: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LockAction {
    Claimed,
    Released,
}

#[derive(Debug, Clone, Serialize)]
pub struct WsExecutionProgressEvent {
    #[serde(rename = "type")]
    pub event_type: WsEventType,
    pub plan_id: String,
    pub phase_id: String,
    pub stage_id: String,
    pub status: StageStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WsDashboardProjectEvent {
    #[serde(rename = "type")]
    pub event_type: WsEventType,

    pub slug: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct WsDashboardAlertsEvent {
    #[serde(rename = "type")]
    pub event_type: WsEventType,

    pub slug: String,

    pub opened: u32,

    pub resolved: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WsSubscribeMessage {
    #[serde(rename = "type")]
    pub message_type: String,

    pub channels: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApiError {
    pub error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct OkResponse {
    pub ok: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    #[test]
    fn test_health_response_serializes() {
        let r = HealthResponse {
            status: "ok".to_string(),
            version: "0.4.0".to_string(),
        };
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("\"status\":\"ok\""));
        assert!(json.contains("\"version\":\"0.4.0\""));
    }

    #[test]
    fn test_create_issue_request_deserializes() {
        let json = r#"{"title": "Fix bug", "priority": "high"}"#;
        let req: CreateIssueRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.title, "Fix bug");
        assert_eq!(req.priority, ApiPriority::High);
        assert!(req.description.is_none());
        assert!(req.parent_id.is_none());
    }

    #[test]
    fn test_create_issue_request_default_priority() {
        let json = r#"{"title": "Fix bug"}"#;
        let req: CreateIssueRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.priority, ApiPriority::Medium);
    }

    #[test]
    fn test_update_issue_request_all_optional() {
        let json = r"{}";
        let req: UpdateIssueRequest = serde_json::from_str(json).unwrap();
        assert!(req.title.is_none());
        assert!(req.description.is_none());
        assert!(req.priority.is_none());
    }

    #[test]
    fn test_agent_status_serializes_lowercase() {
        let json = serde_json::to_string(&AgentStatus::Active).unwrap();
        assert_eq!(json, "\"active\"");
        let json = serde_json::to_string(&AgentStatus::Stale).unwrap();
        assert_eq!(json, "\"stale\"");
    }

    #[test]
    fn test_stage_status_round_trip() {
        let statuses = [
            StageStatus::Pending,
            StageStatus::Running,
            StageStatus::Done,
            StageStatus::Failed,
        ];
        for status in &statuses {
            let json = serde_json::to_string(status).unwrap();
            let parsed: StageStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(status, &parsed);
        }
    }

    #[test]
    fn test_ws_heartbeat_event_serializes() {
        let event = WsHeartbeatEvent {
            event_type: WsEventType::Heartbeat,
            agent_id: "worker-1".to_string(),
            timestamp: Utc::now(),
            active_issue_id: Some(42),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"heartbeat\""));
        assert!(json.contains("\"agent_id\":\"worker-1\""));
        assert!(json.contains("\"active_issue_id\":42"));
    }

    #[test]
    fn test_ws_heartbeat_event_skips_null_issue() {
        let event = WsHeartbeatEvent {
            event_type: WsEventType::Heartbeat,
            agent_id: "worker-1".to_string(),
            timestamp: Utc::now(),
            active_issue_id: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(!json.contains("active_issue_id"));
    }

    #[test]
    fn test_lock_action_serializes() {
        assert_eq!(
            serde_json::to_string(&LockAction::Claimed).unwrap(),
            "\"claimed\""
        );
        assert_eq!(
            serde_json::to_string(&LockAction::Released).unwrap(),
            "\"released\""
        );
    }

    #[test]
    fn test_orchestrator_plan_round_trip() {
        let plan = OrchestratorPlan {
            id: "plan-1".to_string(),
            document_slug: "my-doc".to_string(),
            phases: vec![],
            created_at: Utc::now(),
            total_stages: 0,
            estimated_hours: 0.0,
        };
        let json = serde_json::to_string(&plan).unwrap();
        let parsed: OrchestratorPlan = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, "plan-1");
        assert_eq!(parsed.document_slug, "my-doc");
    }

    #[test]
    fn test_api_error_serializes() {
        let err = ApiError {
            error: "not found".to_string(),
            detail: Some("Issue #999 does not exist".to_string()),
        };
        let json = serde_json::to_string(&err).unwrap();
        assert!(json.contains("\"error\":\"not found\""));
        assert!(json.contains("\"detail\""));
    }

    #[test]
    fn test_api_error_skips_null_detail() {
        let err = ApiError {
            error: "bad request".to_string(),
            detail: None,
        };
        let json = serde_json::to_string(&err).unwrap();
        assert!(!json.contains("detail"));
    }

    #[test]
    fn test_config_response_round_trip() {
        let config = ConfigResponse {
            tracking_mode: "strict".to_string(),
            stale_lock_timeout_minutes: 60,
            remote: "origin".to_string(),
            signing_enforcement: "audit".to_string(),
            intervention_tracking: true,
            auto_steal_stale_locks: false,
        };
        let json = serde_json::to_string(&config).unwrap();
        let parsed: ConfigResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.tracking_mode, "strict");
        assert_eq!(parsed.stale_lock_timeout_minutes, 60);
    }

    #[test]
    fn test_create_comment_request_default_kind() {
        let json = r#"{"content": "A comment"}"#;
        let req: CreateCommentRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.kind, CommentKind::Note);
    }

    #[test]
    fn test_knowledge_source_round_trip() {
        let source = KnowledgeSource {
            url: "https://example.com".to_string(),
            title: "Example".to_string(),
            accessed_at: Some("2026-03-01".to_string()),
        };
        let json = serde_json::to_string(&source).unwrap();
        let parsed: KnowledgeSource = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.url, "https://example.com");
        assert_eq!(parsed.accessed_at, Some("2026-03-01".to_string()));
    }

    #[test]
    fn test_ws_subscribe_message_deserializes() {
        let json = r#"{"type": "subscribe", "channels": ["agents", "issues"]}"#;
        let msg: WsSubscribeMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.message_type, "subscribe");
        assert_eq!(msg.channels, vec!["agents", "issues"]);
    }
}
