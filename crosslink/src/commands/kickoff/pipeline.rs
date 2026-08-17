use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineState {
    pub schema_version: u32,
    pub design_doc: String,
    pub doc_hash: String,
    pub stage: String,
    #[serde(default)]
    pub plans: Vec<PlanRecord>,
    #[serde(default)]
    pub runs: Vec<RunRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanRecord {
    pub agent_id: String,
    pub worktree: String,
    pub started_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
    pub status: String,
    #[serde(default)]
    pub blocking_gaps: u32,
    #[serde(default)]
    pub advisory_gaps: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_file: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRecord {
    pub agent_id: String,
    pub worktree: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue_id: Option<i64>,
    pub started_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
    pub status: String,
}

pub fn compute_doc_hash(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    let result = hasher.finalize();
    format!("sha256:{result:x}")
}

pub fn is_plan_stale(pipeline: &PipelineState, design_doc_path: &Path) -> bool {
    let Ok(content) = std::fs::read_to_string(design_doc_path) else {
        return false;
    };
    let current_hash = compute_doc_hash(&content);
    current_hash != pipeline.doc_hash
}

pub fn pipeline_path_for_doc(doc_path: &Path) -> std::path::PathBuf {
    let stem = doc_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown");
    doc_path.with_file_name(format!("{stem}.pipeline.json"))
}

pub fn plan_path_for_doc(doc_path: &Path) -> std::path::PathBuf {
    let stem = doc_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown");
    doc_path.with_file_name(format!("{stem}.plan.json"))
}

pub fn read_pipeline_state(doc_path: &Path) -> Option<PipelineState> {
    let path = pipeline_path_for_doc(doc_path);
    let content = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&content).ok()
}

pub fn write_pipeline_state(doc_path: &Path, state: &PipelineState) -> Result<()> {
    let path = pipeline_path_for_doc(doc_path);
    let json = serde_json::to_string_pretty(state).context("Failed to serialize pipeline state")?;
    std::fs::write(&path, json)
        .with_context(|| format!("Failed to write pipeline state to {}", path.display()))
}

pub fn create_initial_pipeline(doc_path: &Path) -> Result<PipelineState> {
    let content = std::fs::read_to_string(doc_path)
        .with_context(|| format!("Failed to read design doc: {}", doc_path.display()))?;
    let doc_hash = compute_doc_hash(&content);

    let state = PipelineState {
        schema_version: 1,
        design_doc: doc_path.to_string_lossy().to_string(),
        doc_hash,
        stage: "designed".to_string(),
        plans: Vec::new(),
        runs: Vec::new(),
    };

    write_pipeline_state(doc_path, &state)?;
    Ok(state)
}

pub fn ensure_pipeline_state(doc_path: &Path) -> Result<PipelineState> {
    read_pipeline_state(doc_path).map_or_else(|| create_initial_pipeline(doc_path), Ok)
}

pub fn mark_planning(doc_path: &Path, agent_id: &str, worktree: &str) -> Result<PipelineState> {
    let mut state = ensure_pipeline_state(doc_path)?;

    if let Ok(content) = std::fs::read_to_string(doc_path) {
        state.doc_hash = compute_doc_hash(&content);
    }

    state.stage = "planning".to_string();
    state.plans.push(PlanRecord {
        agent_id: agent_id.to_string(),
        worktree: worktree.to_string(),
        started_at: chrono::Utc::now().to_rfc3339(),
        completed_at: None,
        status: "running".to_string(),
        blocking_gaps: 0,
        advisory_gaps: 0,
        plan_file: None,
    });

    write_pipeline_state(doc_path, &state)?;
    Ok(state)
}

#[allow(dead_code)]
pub fn mark_planned(
    doc_path: &Path,
    agent_id: &str,
    blocking_gaps: u32,
    advisory_gaps: u32,
    plan_file: &str,
) -> Result<PipelineState> {
    let mut state = ensure_pipeline_state(doc_path)?;
    state.stage = "planned".to_string();

    if let Some(plan) = state
        .plans
        .iter_mut()
        .rev()
        .find(|p| p.agent_id == agent_id)
    {
        plan.completed_at = Some(chrono::Utc::now().to_rfc3339());
        plan.status = "done".to_string();
        plan.blocking_gaps = blocking_gaps;
        plan.advisory_gaps = advisory_gaps;
        plan.plan_file = Some(plan_file.to_string());
    }

    write_pipeline_state(doc_path, &state)?;
    Ok(state)
}

pub fn mark_running(
    doc_path: &Path,
    agent_id: &str,
    worktree: &str,
    issue_id: Option<i64>,
) -> Result<PipelineState> {
    let mut state = ensure_pipeline_state(doc_path)?;
    state.stage = "running".to_string();
    state.runs.push(RunRecord {
        agent_id: agent_id.to_string(),
        worktree: worktree.to_string(),
        issue_id,
        started_at: chrono::Utc::now().to_rfc3339(),
        completed_at: None,
        status: "running".to_string(),
    });

    write_pipeline_state(doc_path, &state)?;
    Ok(state)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunProbe {
    SentinelDone,

    SentinelFailed,

    LiveRunning,

    Gone,

    Indeterminate,
}

pub fn probe_run_worktree(
    run: &RunRecord,
    live_agent_ids: &[String],
) -> (RunProbe, Option<String>) {
    let is_live = live_agent_ids.iter().any(|id| id == &run.agent_id) && run.agent_id != "pending";

    if run.worktree.is_empty() || run.worktree == "pending" {
        return if is_live {
            (RunProbe::LiveRunning, None)
        } else {
            (RunProbe::Gone, None)
        };
    }

    let wt = Path::new(&run.worktree);
    if !wt.exists() {
        return if is_live {
            (RunProbe::LiveRunning, None)
        } else {
            (RunProbe::Gone, None)
        };
    }

    let status_file = wt.join(".kickoff-status");
    let sentinel_mtime = std::fs::metadata(&status_file)
        .and_then(|m| m.modified())
        .ok()
        .map(|t| chrono::DateTime::<chrono::Utc>::from(t).to_rfc3339());

    if let Ok(raw) = std::fs::read_to_string(&status_file) {
        let lower = raw.to_lowercase();
        if lower.contains("done") {
            return (RunProbe::SentinelDone, sentinel_mtime);
        }
        if lower.contains("fail") || lower.contains("error") {
            return (RunProbe::SentinelFailed, sentinel_mtime);
        }
    }

    if is_live {
        (RunProbe::LiveRunning, None)
    } else {
        (RunProbe::Indeterminate, None)
    }
}

pub fn reconcile_runs<F>(state: &mut PipelineState, now: &str, mut probe: F) -> bool
where
    F: FnMut(&RunRecord) -> (RunProbe, Option<String>),
{
    let mut changed = false;
    for run in &mut state.runs {
        if run.status != "running" {
            continue;
        }
        let (verdict, sentinel_mtime) = probe(run);
        match verdict {
            RunProbe::SentinelDone => {
                run.status = "completed".to_string();
                run.completed_at = Some(sentinel_mtime.unwrap_or_else(|| now.to_string()));
                changed = true;
            }
            RunProbe::SentinelFailed => {
                run.status = "failed".to_string();
                run.completed_at = Some(sentinel_mtime.unwrap_or_else(|| now.to_string()));
                changed = true;
            }
            RunProbe::Gone => {
                run.status = "aborted".to_string();
                run.completed_at = Some(now.to_string());
                changed = true;
            }
            RunProbe::LiveRunning | RunProbe::Indeterminate => {}
        }
    }

    if changed && state.stage == "running" {
        let any_running = state.runs.iter().any(|r| r.status == "running");
        if !any_running {
            state.stage = stage_after_runs_settle(state);
        }
    }

    changed
}

fn stage_after_runs_settle(state: &PipelineState) -> String {
    match state.runs.last().map(|r| r.status.as_str()) {
        Some("completed") => "complete".to_string(),
        _ if state.plans.iter().any(|p| p.status == "done") => "planned".to_string(),
        _ if !state.plans.is_empty() => "planned".to_string(),
        _ => "designed".to_string(),
    }
}

pub fn reconcile_runs_for_display(
    doc_path: &Path,
    state: &mut PipelineState,
    live_agent_ids: &[String],
) -> bool {
    let now = chrono::Utc::now().to_rfc3339();
    let changed = reconcile_runs(state, &now, |run| probe_run_worktree(run, live_agent_ids));
    if changed {
        let _ = write_pipeline_state(doc_path, state);
    }
    changed
}

pub fn mark_run_finished(
    doc_path: &Path,
    state: &mut PipelineState,
    worktree: &str,
    started_near: Option<&str>,
    status: &str,
) -> bool {
    let now = chrono::Utc::now().to_rfc3339();
    let idx = match_run_index(state, worktree, started_near);
    let Some(idx) = idx else {
        return false;
    };
    let run = &mut state.runs[idx];
    if run.status != "running" {
        return false;
    }
    run.status = status.to_string();
    run.completed_at = Some(now);

    if state.stage == "running" && !state.runs.iter().any(|r| r.status == "running") {
        state.stage = stage_after_runs_settle(state);
    }

    let _ = write_pipeline_state(doc_path, state);
    true
}

fn match_run_index(
    state: &PipelineState,
    worktree: &str,
    started_near: Option<&str>,
) -> Option<usize> {
    if !worktree.is_empty() && worktree != "pending" {
        if let Some(i) = state
            .runs
            .iter()
            .position(|r| r.status == "running" && r.worktree == worktree)
        {
            return Some(i);
        }
    }

    let near = started_near.and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())?;
    state
        .runs
        .iter()
        .enumerate()
        .filter(|(_, r)| r.status == "running")
        .filter_map(|(i, r)| {
            let started = chrono::DateTime::parse_from_rfc3339(&r.started_at).ok()?;
            let delta = (started - near).num_seconds().abs();
            Some((i, delta))
        })
        .min_by_key(|(_, delta)| *delta)
        .map(|(i, _)| i)
}

pub fn reconcile_completion_by_worktree(repo_root: &Path, worktree: &str, status: &str) -> bool {
    if worktree.is_empty() {
        return false;
    }
    for (doc_path, mut state) in scan_pipeline_states(repo_root) {
        if state
            .runs
            .iter()
            .any(|r| r.status == "running" && r.worktree == worktree)
            && mark_run_finished(&doc_path, &mut state, worktree, None, status)
        {
            return true;
        }
    }
    false
}

pub fn stage_display(pipeline: &PipelineState, doc_path: &Path) -> String {
    let stale = if pipeline.stage == "planned" && is_plan_stale(pipeline, doc_path) {
        " \u{26a0} stale"
    } else {
        ""
    };

    match pipeline.stage.as_str() {
        "designed" => "designed".to_string(),
        "planning" => pipeline.plans.last().map_or_else(
            || "planning \u{27f3}".to_string(),
            |plan| format!("planning \u{27f3}  {}", plan.agent_id),
        ),
        "planned" => pipeline.plans.last().map_or_else(
            || format!("planned{stale}"),
            |plan| {
                format!(
                    "planned \u{2713}{}   {}/{}",
                    stale, plan.blocking_gaps, plan.advisory_gaps
                )
            },
        ),
        "running" => pipeline.runs.last().map_or_else(
            || "running \u{27f3}".to_string(),
            |run| format!("running  {} \u{27f3}", run.agent_id),
        ),
        "complete" => "complete \u{2713}".to_string(),
        other => other.to_string(),
    }
}

#[allow(dead_code)]
fn plan_age_display(completed_at: &Option<String>) -> String {
    let Some(ts) = completed_at else {
        return String::new();
    };
    let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts) else {
        return String::new();
    };
    let elapsed = chrono::Utc::now().signed_duration_since(dt.with_timezone(&chrono::Utc));
    let mins = elapsed.num_minutes();
    if mins < 60 {
        format!("({mins}m ago)")
    } else {
        let hours = mins / 60;
        format!("({hours}h ago)")
    }
}

pub fn scan_pipeline_states(repo_root: &Path) -> Vec<(std::path::PathBuf, PipelineState)> {
    let design_dir = repo_root.join(".design");
    if !design_dir.is_dir() {
        return Vec::new();
    }

    let mut results = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&design_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("md") {
                if let Some(state) = read_pipeline_state(&path) {
                    results.push((path, state));
                }
            }
        }
    }
    results
}

pub fn scan_design_docs(repo_root: &Path) -> Vec<std::path::PathBuf> {
    let design_dir = repo_root.join(".design");
    if !design_dir.is_dir() {
        return Vec::new();
    }

    let mut docs = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&design_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("md") {
                docs.push(path);
            }
        }
    }
    docs.sort();
    docs
}
