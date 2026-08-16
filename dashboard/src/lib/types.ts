













export interface Issue {
  id: number;
  title: string;
  description: string | null;
  status: IssueStatus;
  priority: IssuePriority;
  parent_id: number | null;
  created_at: string;
  updated_at: string;
  closed_at: string | null;
}

export type IssueStatus = "open" | "closed" | "archived";
export type IssuePriority = "low" | "medium" | "high" | "critical";

export interface Comment {
  id: number;
  issue_id: number;
  content: string;
  created_at: string;
  kind: CommentKind;
  trigger_type: string | null;
  intervention_context: string | null;
  driver_key_fingerprint: string | null;
}

export type CommentKind =
  | "note"
  | "plan"
  | "decision"
  | "observation"
  | "blocker"
  | "resolution"
  | "result"
  | "intervention";

export interface Session {
  id: number;
  started_at: string;
  ended_at: string | null;
  active_issue_id: number | null;
  handoff_notes: string | null;
  last_action: string | null;
  agent_id: string | null;
}

export interface Milestone {
  id: number;
  name: string;
  description: string | null;
  status: "open" | "closed";
  created_at: string;
  closed_at: string | null;
}

export interface Heartbeat {
  agent_id: string;
  last_heartbeat: string;
  active_issue_id: number | null;
  machine_id: string;
}





export interface HealthResponse {
  status: string;
  version: string;
}






export interface IssueDetail extends Issue {
  labels: string[];
  comments: Comment[];
  blockers: number[];
  blocking: number[];
  subissues: Issue[];
  milestone: MilestoneSummary | null;
}





export interface MilestoneSummary {
  id: number;
  name: string;
  status: "open" | "closed";
}

export interface MilestoneDetail extends Milestone {
  issue_count: number;
  completed_count: number;

  progress_percent: number;
}





export interface KnowledgeSource {
  url: string;
  title: string;
  accessed_at?: string;
}

export interface KnowledgePage {
  slug: string;
  title: string;
  tags: string[];
  sources: KnowledgeSource[];
  contributors: string[];
  created: string;
  updated: string;
  content: string;
}

export interface CreateKnowledgePageRequest {
  slug: string;
  title: string;
  content: string;
  tags?: string[];
  sources?: KnowledgeSource[];
}

export interface KnowledgeSearchMatch {
  slug: string;
  title: string;
  line_number: number;
  context_lines: [number, string][];
}





export type AgentStatus = "running" | "active" | "idle" | "stale" | "done" | "failed" | "unknown";


export interface AgentHeartbeat {
  agent_id: string;
  timestamp: string;
  issue_id: number | null;
  session_id: number | null;
  message: string | null;
}


export interface AgentLockEntry {
  issue_id: number;
  claimed_at: string;
  age_seconds: number;
  stale: boolean;
}


export interface LockEntry {
  issue_id: number;
  agent_id: string;
  branch: string | null;
  claimed_at: string;
  signed_by: string;
  age_seconds: number;
  is_stale: boolean;
}


export interface AgentSummary {
  agent_id: string;
  machine_id: string;
  description: string | null;
  status: AgentStatus;
  last_heartbeat: string;
  active_issue_id: number | null;
  branch: string | null;
  worktree_path: string | null;
  locks: number[];
}





export interface SyncStatus {
  hub_initialized: boolean;
  hub_branch: string;
  remote: string;
  last_fetch_at: string | null;
  active_lock_count: number;
  stale_lock_count: number;
}





export type TrackingMode = "strict" | "normal" | "relaxed";
export type SigningEnforcement = "audit" | "required" | "disabled";

export interface Config {
  tracking_mode: TrackingMode;
  stale_lock_timeout_minutes: number;
  remote: string;
  signing_enforcement: SigningEnforcement;
  intervention_tracking: boolean;
  auto_steal_stale_locks: boolean;
}





export interface OrchestratorTask {
  id: string;
  title: string;
  description: string;
  complexity_hours: number;
}

export interface OrchestratorStage {
  id: string;
  title: string;
  description: string;
  tasks: OrchestratorTask[];
  depends_on: string[];
  agent_count: number;
  complexity_hours: number;

  status?: StageStatus;

  agent_id?: string;
}

export interface OrchestratorPhase {
  id: string;
  title: string;
  description: string;
  stages: OrchestratorStage[];
  gate_criteria: string[];
}

export interface OrchestratorPlan {
  id: string;
  title?: string;
  document_slug: string;
  phases: OrchestratorPhase[];
  created_at: string;
  total_stages: number;
  estimated_hours: number;
}

export type StageStatus =
  | "pending"
  | "running"
  | "done"
  | "failed"
  | "skipped"
  | "blocked";

export type ExecutionState =
  | "idle"
  | "running"
  | "paused"
  | "done"
  | "failed";

export interface ExecutionStatus {
  plan_id: string;
  state: ExecutionState;
  current_phase_id: string | null;
  progress_percent: number;
  started_at: string | null;
  completed_at: string | null;

  stage_statuses: Record<string, StageStatus>;

  stage_agents: Record<string, string>;
}






export type Agent = AgentSummary;






export interface AgentDetailResponse {
  agent_id: string;
  machine_id: string;
  description: string | null;
  status: AgentStatus;

  last_heartbeat: AgentHeartbeat | null;
  active_issue_id: number | null;
  branch: string | null;
  worktree_path: string | null;
  tmux_session: string | null;

  locks: AgentLockEntry[];

  heartbeat_history: string[];
  kickoff_status: string | null;
  kickoff_report: string | null;
}


export type Lock = LockEntry;






export type WsMessage =
  | WsHeartbeatEvent
  | WsAgentStatusEvent
  | WsIssueUpdatedEvent
  | WsLockChangedEvent
  | WsExecutionProgressEvent;


export interface WsHeartbeatEvent {
  type: "heartbeat";
  agent_id: string;
  timestamp: string;
  active_issue_id: number | null;
}


export interface WsAgentStatusEvent {
  type: "agent_status";
  agent_id: string;
  status: AgentStatus;
}


export interface WsIssueUpdatedEvent {
  type: "issue_updated";
  issue_id: number;
  field: string;
}


export interface WsLockChangedEvent {
  type: "lock_changed";
  issue_id: number;
  action: "claimed" | "released";
  agent_id: string;
}


export interface WsExecutionProgressEvent {
  type: "execution_progress";
  plan_id: string;
  phase_id: string;
  stage_id: string;
  status: StageStatus;
  agent_id: string | null;
}


export interface WsSubscribeMessage {
  type: "subscribe";

  channels: WsChannel[];
}

export type WsChannel = "agents" | "issues" | "locks" | "execution";


export type WsClientMessage = WsSubscribeMessage;

export type WsServerMessage = WsMessage;





export type ExecutionEventKind =
  | "stage_started"
  | "stage_completed"
  | "stage_failed"
  | "stage_skipped"
  | "stage_retried"
  | "phase_started"
  | "phase_completed"
  | "execution_started"
  | "execution_paused"
  | "execution_resumed"
  | "execution_completed"
  | "execution_failed";


export interface ExecutionEvent {
  id: string;
  timestamp: string;
  kind: ExecutionEventKind;
  phase_id: string | null;
  stage_id: string | null;
  agent_id: string | null;
  message: string;
}






export interface TokenUsageRecord {
  id: number;
  agent_id: string;
  session_id: number | null;
  timestamp: string;
  input_tokens: number;
  output_tokens: number;
  model: string;
  cost_estimate: number;
}


export interface RawUsageSummaryItem {
  agent_id: string;
  model: string;
  request_count: number;
  total_input_tokens: number;
  total_output_tokens: number;
  total_cost: number;
}

export interface RawUsageSummary {
  items: RawUsageSummaryItem[];
  total_input_tokens: number;
  total_output_tokens: number;
  total_cost: number;
}


export interface UsageSummary {
  total_input_tokens: number;
  total_output_tokens: number;
  total_cost: number;
  by_agent: AgentUsageSummary[];
  by_model: ModelUsageSummary[];
  daily: DailyUsage[];
}


export interface AgentUsageSummary {
  agent_id: string;
  input_tokens: number;
  output_tokens: number;
  cost_estimate: number;
  interaction_count: number;
}


export interface ModelUsageSummary {
  model: string;
  input_tokens: number;
  output_tokens: number;
  cost_estimate: number;
}


export interface DailyUsage {
  date: string;
  input_tokens: number;
  output_tokens: number;
  cost_estimate: number;
}


export interface BudgetConfig {
  daily_limit: number | null;
  monthly_limit: number | null;
  alert_threshold_percent: number;
}





export interface ApiError {
  error: string;
  detail?: string;
}
