use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;

/// The agent image repository this build defaults to: the published one,
/// unless the build set `CROSSLINK_AGENT_IMAGE_REPOSITORY` (forks that publish
/// their own image). Chosen in `build.rs`.
pub const AGENT_IMAGE_REPOSITORY: &str = env!("CROSSLINK_AGENT_IMAGE_REPOSITORY");

/// The tag this build defaults to, chosen in `build.rs` from positive
/// evidence only: the release version for published packages and for
/// checkouts at their `v<version>` tag, `nightly` (the image built from
/// `develop`) for every other build, or an explicit build-time
/// `CROSSLINK_AGENT_IMAGE_TAG`. A release build therefore runs the crosslink
/// it was released with inside the container; a development build runs
/// develop's crosslink, which can differ from a feature branch's code (build
/// and pass your own image when changing hub or readiness code).
pub const AGENT_IMAGE_TAG: &str = env!("CROSSLINK_AGENT_IMAGE_TAG");

/// The image kickoff, swarm, sentinel and `container` commands use unless an
/// image is given explicitly.
pub const DEFAULT_AGENT_IMAGE: &str = concat!(
    env!("CROSSLINK_AGENT_IMAGE_REPOSITORY"),
    ":",
    env!("CROSSLINK_AGENT_IMAGE_TAG")
);

/// Environment override for the agent image, honoured by every container
/// command. Forks publish the agent image under their own registry path.
pub const AGENT_IMAGE_ENV: &str = "CROSSLINK_CONTAINER_IMAGE";

/// Where the agent image a command uses came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentImageSource {
    Flag,
    Environment,
    BuildDefault,
}

impl std::fmt::Display for AgentImageSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Flag => "--image",
            Self::Environment => AGENT_IMAGE_ENV,
            Self::BuildDefault => "this build's default",
        })
    }
}

/// The agent image every container command uses: an explicit `--image` wins,
/// then `CROSSLINK_CONTAINER_IMAGE`, then this build's default. Blank values
/// count as unset.
pub fn resolve_agent_image_from(
    explicit: Option<&str>,
    env_value: Option<&str>,
) -> Result<(String, AgentImageSource)> {
    let non_empty = |value: Option<&str>| {
        value
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(String::from)
    };
    let (image, source) = non_empty(explicit)
        .map(|image| (image, AgentImageSource::Flag))
        .or_else(|| non_empty(env_value).map(|image| (image, AgentImageSource::Environment)))
        .unwrap_or_else(|| {
            (
                DEFAULT_AGENT_IMAGE.to_string(),
                AgentImageSource::BuildDefault,
            )
        });
    if image.starts_with('-') {
        bail!("container image name cannot start with '-': {image}");
    }
    Ok((image, source))
}

/// The agent image for a launch in `mode`: resolved as for every container
/// command when a container is used, and this build's default otherwise (a
/// local launch never reads the image settings, so they cannot fail it).
pub fn agent_image_for(
    mode: &ContainerMode,
    explicit: Option<&str>,
) -> Result<(String, AgentImageSource)> {
    if *mode == ContainerMode::None {
        return Ok((
            DEFAULT_AGENT_IMAGE.to_string(),
            AgentImageSource::BuildDefault,
        ));
    }
    resolve_agent_image(explicit)
}

/// `resolve_agent_image_from` over the live `CROSSLINK_CONTAINER_IMAGE`.
pub fn resolve_agent_image(explicit: Option<&str>) -> Result<(String, AgentImageSource)> {
    resolve_agent_image_from(explicit, std::env::var(AGENT_IMAGE_ENV).ok().as_deref())
}

/// Tags that move to a new build without a version change. A local copy of
/// one of these is re-pulled on every launch, so it cannot go stale.
const FLOATING_IMAGE_TAGS: [&str; 2] = ["nightly", "latest"];

/// Whether an image reference names a floating tag (`:nightly`, `:latest`,
/// or no tag at all, which means `:latest`) that should be refreshed before a
/// launch. Pinned tags and digests are not.
pub fn is_floating_image(image: &str) -> bool {
    if image.contains('@') {
        return false;
    }
    let tag = image
        .rsplit_once(':')
        .filter(|(_, tag)| !tag.contains('/'))
        .map_or("latest", |(_, tag)| tag);
    FLOATING_IMAGE_TAGS.contains(&tag)
}

pub const EFFORT_LEVELS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContainerMode {
    None,

    Docker,

    Podman,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyLevel {
    Local,

    Ci,

    Thorough,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Criterion {
    pub id: String,
    pub text: String,
    #[serde(rename = "type")]
    pub criterion_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CriteriaFile {
    pub source_doc: String,
    pub extracted_at: String,
    pub criteria: Vec<Criterion>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KickoffMetadata {
    pub started_at: String,

    pub timeout_secs: u64,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_usd: Option<String>,
}

impl KickoffMetadata {
    pub fn for_launch(opts: &KickoffOpts, started_at: String) -> Self {
        Self {
            started_at,
            timeout_secs: opts.timeout.as_secs(),
            provider: None,
            model: Some(opts.model.to_string()),
            effort: opts.policy.effort.clone(),
            budget_usd: opts.policy.monetary_budget_usd.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KickoffDocBreadcrumb {
    pub rel_path: String,

    pub doc_hash: String,
}

pub struct KickoffOpts<'a> {
    pub description: &'a str,
    pub issue: Option<i64>,
    pub container: ContainerMode,
    pub verify: VerifyLevel,
    pub model: &'a str,
    pub image: &'a str,
    pub timeout: Duration,
    pub dry_run: bool,
    pub branch: Option<&'a str>,
    pub quiet: bool,
    pub design_doc: Option<&'a super::super::design_doc::DesignDoc>,
    pub doc_path: Option<&'a str>,
    pub policy: crate::agents::ExecutionPolicy,

    pub template: Option<&'a Path>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CriterionVerdict {
    pub id: String,
    pub verdict: String,
    pub evidence: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReportSummary {
    pub total: usize,
    pub pass: usize,
    pub fail: usize,
    pub partial: usize,
    pub not_applicable: usize,
    pub needs_clarification: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct PhaseTiming {
    pub duration_s: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files_read: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files_modified: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines_added: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines_removed: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tests_run: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tests_passed: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tests_failed: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comments_added: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub criteria_checked: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issues_found: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issues_fixed: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct PhaseTimings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exploration: Option<PhaseTiming>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub planning: Option<PhaseTiming>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub implementation: Option<PhaseTiming>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub testing: Option<PhaseTiming>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validation: Option<PhaseTiming>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<PhaseTiming>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KickoffReport {
    pub validated_at: String,
    pub criteria: Vec<CriterionVerdict>,
    pub summary: ReportSummary,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_version: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phases: Option<PhaseTimings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unresolved_questions: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commits: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files_changed: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportFormat {
    Table,

    Json,

    Markdown,
}

pub struct PlanOpts<'a> {
    pub doc: &'a super::super::design_doc::DesignDoc,

    pub doc_path: Option<&'a std::path::Path>,
    pub model: &'a str,
    pub timeout: Duration,
    pub dry_run: bool,
    pub issue: Option<i64>,
    pub quiet: bool,

    pub policy: crate::agents::ExecutionPolicy,

    pub template: Option<&'a Path>,
}

pub(crate) struct ProjectConventions {
    pub(crate) test_command: Option<String>,
    pub(crate) lint_commands: Vec<String>,
    pub(crate) allowed_tools: Vec<String>,
}

pub(crate) struct PreflightResult {
    pub timeout_cmd: &'static str,

    pub sandbox_command: Option<String>,

    pub agent: crate::agents::ResolvedAgent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Platform {
    MacOS,
    Linux(LinuxDistro),
    Windows,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LinuxDistro {
    Debian,
    Fedora,
    Arch,
    Alpine,
    Other,
}

pub(super) struct WatchdogConfig {
    pub enabled: bool,

    pub staleness_secs: u64,

    pub max_nudges: u32,

    pub check_interval_secs: u64,

    pub grace_period_secs: u64,
}

impl Default for WatchdogConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            staleness_secs: 300,
            max_nudges: 5,
            check_interval_secs: 120,
            grace_period_secs: 300,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct AgentInfo {
    pub id: String,
    pub issue: Option<String>,
    pub status: String,
    pub session: Option<String>,
    pub worktree: String,
    pub docker: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(super) enum CleanupClass {
    Done,

    Stale,

    Active,
}

#[derive(Debug, Serialize)]
pub(super) struct CleanupResult {
    pub id: String,
    pub class: CleanupClass,
    pub worktree_removed: bool,
    pub tmux_killed: bool,
    pub container_removed: bool,
    pub error: Option<String>,
}

pub fn parse_container_mode(s: &str) -> Result<ContainerMode> {
    match s.to_lowercase().as_str() {
        "none" | "local" => Ok(ContainerMode::None),
        "docker" => Ok(ContainerMode::Docker),
        "podman" => Ok(ContainerMode::Podman),
        _ => bail!("Unknown container runtime '{s}'. Use: none, docker, podman"),
    }
}

pub fn parse_verify_level(s: &str) -> Result<VerifyLevel> {
    match s.to_lowercase().as_str() {
        "local" => Ok(VerifyLevel::Local),
        "ci" => Ok(VerifyLevel::Ci),
        "thorough" => Ok(VerifyLevel::Thorough),
        _ => bail!("Unknown verification level '{s}'. Use: local, ci, thorough"),
    }
}

pub fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    if s.is_empty() {
        bail!("Empty duration string");
    }

    let (num_str, unit) = s
        .strip_suffix('h')
        .map(|n| (n, 'h'))
        .or_else(|| s.strip_suffix('m').map(|n| (n, 'm')))
        .or_else(|| s.strip_suffix('s').map(|n| (n, 's')))
        .unwrap_or((s, 's'));

    let value: u64 = num_str
        .parse()
        .with_context(|| format!("Invalid duration number: '{num_str}'"))?;

    let secs = match unit {
        'h' => value * 3600,
        'm' => value * 60,
        's' => value,
        _ => unreachable!(),
    };

    if secs == 0 {
        bail!("Duration must be greater than zero");
    }

    Ok(Duration::from_secs(secs))
}

pub(super) fn is_timed_out(wt_path: &Path) -> bool {
    let meta_path = wt_path.join(".kickoff-metadata.json");
    let Ok(content) = std::fs::read_to_string(&meta_path) else {
        return false;
    };
    let meta: KickoffMetadata = match serde_json::from_str(&content) {
        Ok(m) => m,
        Err(_) => return false,
    };
    let started = match chrono::DateTime::parse_from_rfc3339(&meta.started_at) {
        Ok(dt) => dt.with_timezone(&chrono::Utc),
        Err(_) => return false,
    };
    let elapsed = chrono::Utc::now().signed_duration_since(started);
    elapsed.num_seconds() > meta.timeout_secs as i64
}

use anyhow::Context;
