use std::time::Duration;

use crate::commands::kickoff::VerifyLevel;

use super::config::SentinelConfig;
use super::sources::{Signal, SignalDecision, SourceKind};

#[derive(Debug, Clone)]
pub enum Disposition {
    Dispatch {
        description: String,
        scope: AgentScope,
        attempt: u32,
    },

    Triage {
        priority: String,
        labels: Vec<String>,
    },

    Skip {
        reason: String,
    },

    Defer {
        reason: String,
    },
}

#[derive(Debug, Clone)]
pub struct AgentScope {
    pub allowed_paths: Vec<String>,
    pub verify: VerifyLevel,
    pub timeout: Duration,
    pub model: String,
}

pub fn triage(
    signal: &Signal,
    decision: &SignalDecision,
    config: &SentinelConfig,
    tuning: Option<&super::tuning::TuningOverride>,
    model_override: Option<&str>,
) -> Disposition {
    let label = signal
        .metadata
        .get("label")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let (model, attempt) = match decision {
        SignalDecision::New => {
            let model = model_override.map_or_else(
                || {
                    tuning
                        .and_then(|t| t.model_for_label(label))
                        .map_or_else(|| config.default_agent.model.clone(), String::from)
                },
                String::from,
            );
            (model, 1u32)
        }
        SignalDecision::Escalate => (
            model_override.map_or_else(|| config.escalation.model.clone(), String::from),
            2u32,
        ),
        SignalDecision::Skip(reason) => {
            return Disposition::Skip {
                reason: reason.to_string(),
            };
        }
    };

    let base_timeout_secs = config.default_agent.timeout_minutes * 60;
    let timeout_secs = if attempt > 1 {
        base_timeout_secs * u64::from(config.escalation.timeout_multiplier_pct) / 100
    } else {
        base_timeout_secs
    };

    match &signal.source {
        SourceKind::GitHub => match label {
            "agent-todo: replicate" => {
                let gh_num = signal
                    .metadata
                    .get("number")
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or(0);

                let description = build_replicate_prompt(gh_num, &signal.title, &signal.body);

                Disposition::Dispatch {
                    description,
                    scope: AgentScope {
                        allowed_paths: vec!["tests/".into()],

                        verify: config.default_agent.verify_level(),
                        timeout: Duration::from_secs(timeout_secs),
                        model,
                    },
                    attempt,
                }
            }
            "agent-todo: fix" => {
                let gh_num = signal
                    .metadata
                    .get("number")
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or(0);

                let fix_timeout_secs = (config.default_agent.timeout_minutes * 60) * 2;
                let fix_timeout = if attempt > 1 {
                    fix_timeout_secs * u64::from(config.escalation.timeout_multiplier_pct) / 100
                } else {
                    fix_timeout_secs
                };

                let description = build_fix_prompt(gh_num, &signal.title, &signal.body);

                Disposition::Dispatch {
                    description,
                    scope: AgentScope {
                        allowed_paths: vec!["src/".into(), "tests/".into()],

                        verify: VerifyLevel::Ci,
                        timeout: Duration::from_secs(fix_timeout),
                        model,
                    },
                    attempt,
                }
            }
            other => Disposition::Skip {
                reason: format!("unrecognized agent-todo label: {other}"),
            },
        },
        SourceKind::Internal => Disposition::Triage {
            priority: "low".into(),
            labels: vec!["hygiene".into()],
        },
        SourceKind::CI => Disposition::Triage {
            priority: "high".into(),
            labels: vec!["ci-failure".into()],
        },
    }
}

fn truncate_body(body: &str, max_len: usize) -> String {
    if body.len() > max_len {
        format!("{}...\n\n(truncated)", &body[..max_len])
    } else {
        body.to_string()
    }
}

fn build_replicate_prompt(gh_issue_number: i64, title: &str, body: &str) -> String {
    let body_truncated = truncate_body(body, 4000);

    format!(
        "Reproduce the bug described in GitHub issue #{gh_issue_number}.

Title: {title}
Body:
{body_truncated}

Your task:
1. Read the issue carefully and understand the expected vs actual behavior
2. Explore the codebase to find the relevant code paths
3. Write a failing test that demonstrates the bug
4. Run the test suite to confirm your test fails for the right reason
5. Record your findings as a crosslink comment (--kind observation)
6. If you cannot reproduce, explain why (--kind resolution)

Constraints:
- You may ONLY create or modify files in tests/ directories
- Do NOT fix the bug — only reproduce it
- Do NOT push code or create PRs
- Time limit: 30 minutes"
    )
}

fn build_fix_prompt(gh_issue_number: i64, title: &str, body: &str) -> String {
    let body_truncated = truncate_body(body, 4000);

    format!(
        "Fix the bug described in GitHub issue #{gh_issue_number}.

Title: {title}
Body:
{body_truncated}

Your task:
1. Read the issue carefully and understand the expected vs actual behavior
2. Explore the codebase to find the root cause
3. Write a failing test that demonstrates the bug
4. Implement the fix
5. Run the full test suite to confirm the fix works and nothing else breaks
6. Record your findings as a crosslink comment (--kind resolution)
7. Push your branch and open a draft PR linking GH#{gh_issue_number}

Draft PR title: fix: {title} (sentinel GH#{gh_issue_number})

Constraints:
- You may modify files in src/ and tests/
- Push your branch when tests pass
- Open a DRAFT PR (not ready for review) — a human will review it
- Time limit: 60 minutes"
    )
}
