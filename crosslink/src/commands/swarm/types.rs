use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::sync::SyncManager;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SwarmPlan {
    pub schema_version: u32,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub design_doc: Option<String>,
    pub created_at: String,
    pub phases: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PhaseDefinition {
    pub name: String,
    pub status: PhaseStatus,
    pub agents: Vec<AgentEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate: Option<GateResult>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PhaseStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
}

impl std::fmt::Display for PhaseStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PhaseStatus::Pending => write!(f, "pending"),
            PhaseStatus::InProgress => write!(f, "in progress"),
            PhaseStatus::Completed => write!(f, "completed"),
            PhaseStatus::Failed => write!(f, "failed"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentEntry {
    pub slug: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    pub status: AgentStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Planned,
    Running,
    Completed,
    Merged,
    Failed,
}

impl std::fmt::Display for AgentStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AgentStatus::Planned => write!(f, "planned"),
            AgentStatus::Running => write!(f, "running"),
            AgentStatus::Completed => write!(f, "completed"),
            AgentStatus::Merged => write!(f, "merged"),
            AgentStatus::Failed => write!(f, "failed"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GateResult {
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tests_total: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tests_passed: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ran_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Checkpoint {
    pub phase: String,
    pub created_at: String,
    pub agents_merged: Vec<String>,
    pub agents_pending: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dev_branch_sha: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_result: Option<TestResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff_notes: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TestResult {
    pub total: u64,
    pub passed: u64,
    pub failed: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BudgetConfig {
    pub budget_window_s: u64,
    pub model: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_usd: Option<String>,
}

impl Default for BudgetConfig {
    fn default() -> Self {
        Self {
            budget_window_s: 18000,
            model: "standard".to_string(),
            effort: None,
            budget_usd: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct CostLog {
    #[serde(default)]
    pub observations: Vec<CostObservation>,
    #[serde(default)]
    pub model_estimates: std::collections::HashMap<String, ModelEstimate>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CostObservation {
    pub agent_id: String,
    pub model: String,
    pub duration_s: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files_changed: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines_added: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelEstimate {
    pub median_duration_s: u64,
    pub p90_duration_s: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BudgetRecommendation {
    Proceed,
    ProceedWithCaution,
    Split { recommended_count: usize },
    Block { reason: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WindowAllocation {
    pub window_index: usize,
    pub phases: Vec<WindowPhase>,
    pub total_estimate_s: u64,
    pub buffer_s: u64,
    pub stop_point: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WindowPhase {
    pub name: String,
    pub agent_count: usize,
    pub estimate_s: u64,
    pub fit: WindowFit,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WindowFit {
    Fits,
    Tight,
    Overflow,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MergePlan {
    pub target_branch: String,
    pub agents: Vec<MergeSource>,
    pub conflicts: Vec<FileConflict>,
    pub merge_order: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MergeSource {
    pub agent_slug: String,
    pub worktree_path: PathBuf,
    pub changed_files: Vec<String>,
    pub commit_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileConflict {
    pub file: String,
    pub agents: Vec<String>,
    pub conflict_type: ConflictType,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConflictType {
    NonOverlapping,

    Overlapping,

    CreateModify,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct ActiveSwarmRef {
    pub uuid: String,
    pub title: String,
    pub created_at: String,
}

pub(super) struct SwarmContext {
    pub base: String,
    pub is_legacy: bool,
}

impl SwarmContext {
    pub fn plan_path(&self) -> String {
        format!("{}/plan.json", self.base)
    }

    pub fn phase_path(&self, phase_name: &str) -> String {
        format!("{}/phases/{}.json", self.base, slugify_phase(phase_name))
    }

    pub fn checkpoints_dir(&self) -> String {
        format!("{}/checkpoints", self.base)
    }

    pub fn checkpoint_path(&self, slug: &str) -> String {
        format!("{}/checkpoints/{}.json", self.base, slug)
    }

    pub fn budget_path(&self) -> String {
        format!("{}/budget.json", self.base)
    }

    pub fn history_path(&self) -> String {
        format!("{}/history/cost-log.json", self.base)
    }
}

pub(super) fn resolve_swarm(sync: &SyncManager) -> anyhow::Result<SwarmContext> {
    use super::io::read_hub_json;
    use anyhow::bail;

    let active_path = sync.cache_path().join("swarm/active.json");
    if active_path.exists() {
        if let Ok(active) = read_hub_json::<ActiveSwarmRef>(sync, "swarm/active.json") {
            return Ok(SwarmContext {
                base: format!("swarm/{}", active.uuid),
                is_legacy: false,
            });
        }
    }

    let plan_path = sync.cache_path().join("swarm/plan.json");
    if plan_path.exists() {
        return Ok(SwarmContext {
            base: "swarm".to_string(),
            is_legacy: true,
        });
    }

    bail!("No swarm plan found. Run `crosslink swarm init --doc <file>` first.")
}

pub(super) fn create_swarm_slot(sync: &SyncManager, title: &str) -> anyhow::Result<SwarmContext> {
    use super::io::write_hub_json;

    let uuid = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();

    let active_ref = ActiveSwarmRef {
        uuid: uuid.clone(),
        title: title.to_string(),
        created_at: now,
    };

    write_hub_json(sync, "swarm/active.json", &active_ref)?;

    let base = format!("swarm/{uuid}");
    let phases_dir = sync.cache_path().join(format!("{base}/phases"));
    std::fs::create_dir_all(&phases_dir)?;

    Ok(SwarmContext {
        base,
        is_legacy: false,
    })
}

#[derive(Serialize)]
pub(super) struct ResolvedAgent {
    pub slug: String,
    pub description: String,
    pub issue_id: Option<i64>,
    pub defined_status: AgentStatus,
    pub live_status: String,

    pub branch: Option<String>,
}

pub fn slugify_phase(name: &str) -> String {
    name.to_lowercase()
        .replace(' ', "-")
        .replace(|c: char| !c.is_ascii_alphanumeric() && c != '-', "")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewPlan {
    pub mandate: String,
    pub mandate_prompt: String,
    pub agent_count: usize,
    pub created_at: String,
    pub agents: Vec<ReviewAgentAssignment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doc_output: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewAgentAssignment {
    pub agent_slug: String,
    pub partition_label: String,
    pub files: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FixPlan {
    pub schema_version: u32,
    pub created_at: String,
    pub issues: Vec<FixTarget>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FixTarget {
    pub issue_number: u64,
    pub title: String,
    pub body: String,
    pub labels: Vec<String>,
    pub agent_slug: String,
    pub status: AgentStatus,
}

pub(super) type LabeledIssue = (u64, String, String, Vec<String>);
