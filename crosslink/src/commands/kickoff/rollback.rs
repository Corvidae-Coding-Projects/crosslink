//! Undo for a kickoff that fails before its agent starts, and the lock,
//! session and branch steps that `kickoff cleanup` shares with it.
//!
//! Design: `.design/lock-claim-outcome-and-kickoff-rollback.md`.
// Scaffold: kickoff run, plan and cleanup call this in the implementation commit.
#![allow(dead_code)]

use std::path::PathBuf;

use super::pipeline::PipelineState;

/// Whether this kickoff invocation created a resource or found it in place.
/// Only created resources are removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Origin {
    Created,
    Reused,
}

/// One undo action, in the order `KickoffRollback::steps` returns them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum UndoStep {
    RemoveContainer {
        runtime: String,
        name: String,
    },
    KillTmuxSession {
        name: String,
    },
    /// Run inside the worktree, so the worktree agent signs the release.
    ReleaseLock {
        issue_id: i64,
    },
    EndSession,
    StopDaemon,
    RestorePipeline {
        doc_path: PathBuf,
    },
    RemoveWorktree {
        path: PathBuf,
    },
    DeleteBranch {
        name: String,
        base_commit: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum UndoOutcome {
    Done,
    NothingToUndo,
    Failed {
        error: String,
        remedy: String,
    },
    /// The release ran, but confirmation shows the worktree agent still
    /// holding the lock.
    Stranded {
        remedy: String,
    },
    /// The release ran, but confirmation could not complete.
    Unverified {
        remedy: String,
    },
}

#[derive(Debug, Default)]
pub(super) struct UndoReport {
    pub(super) entries: Vec<(UndoStep, UndoOutcome)>,
}

impl UndoReport {
    /// True when every step either completed or had nothing to undo.
    pub(super) fn is_clean(&self) -> bool {
        unimplemented!("scaffold: undo report classification")
    }
}

/// Performs one undo step. The kickoff implementation runs subprocesses;
/// tests record the calls.
pub(super) trait UndoOps {
    fn run(&mut self, step: &UndoStep) -> UndoOutcome;
}

/// What a kickoff invocation has created so far. Record each resource as it
/// is created; call `disarm` once the agent has started.
#[derive(Debug, Default)]
pub(super) struct KickoffRollback {
    worktree: Option<(PathBuf, Origin)>,
    branch: Option<(String, Origin, Option<String>)>,
    agent_initialized: bool,
    claimed_issue: Option<i64>,
    pipeline: Option<(PathBuf, Option<PipelineState>)>,
    tmux_session: Option<String>,
    container: Option<(String, String)>,
    disarmed: bool,
}

impl KickoffRollback {
    pub(super) fn new() -> Self {
        Self::default()
    }

    pub(super) fn record_worktree(&mut self, path: PathBuf, origin: Origin) {
        self.worktree = Some((path, origin));
    }

    pub(super) fn record_branch(&mut self, name: String, origin: Origin, base: Option<String>) {
        self.branch = Some((name, origin, base));
    }

    /// The worktree has an agent, a daemon and a session.
    pub(super) const fn record_agent(&mut self) {
        self.agent_initialized = true;
    }

    /// Record before invoking `session work`: a claim that fails to confirm
    /// may still have been published. Releasing a lock this agent does not
    /// hold is ignored by the reducer.
    pub(super) const fn record_claim(&mut self, issue_id: i64) {
        self.claimed_issue = Some(issue_id);
    }

    /// Record the pipeline state as it was before `mark_running` or
    /// `mark_planning` changed it.
    pub(super) fn record_pipeline(&mut self, doc_path: PathBuf, prior: Option<PipelineState>) {
        self.pipeline = Some((doc_path, prior));
    }

    pub(super) fn record_tmux_session(&mut self, name: String) {
        self.tmux_session = Some(name);
    }

    pub(super) fn record_container(&mut self, runtime: String, name: String) {
        self.container = Some((runtime, name));
    }

    /// The agent has started: from here on, its resources are its own.
    pub(super) const fn disarm(&mut self) {
        self.disarmed = true;
    }

    /// The undo steps for what was recorded, in the order they must run.
    pub(super) fn steps(&self) -> Vec<UndoStep> {
        unimplemented!("scaffold: undo step plan")
    }

    /// Runs every step, continuing past failures, and reports each outcome.
    #[allow(clippy::needless_pass_by_ref_mut)] // scaffold: the implementation calls `ops.run`
    pub(super) fn unwind(self, ops: &mut impl UndoOps) -> UndoReport {
        let _ = ops;
        unimplemented!("scaffold: undo execution")
    }
}

/// A branch may be deleted only when a base was recorded and the branch has
/// no commits beyond it.
pub(super) fn branch_deletable(tip: &str, base_commit: Option<&str>) -> bool {
    let _ = (tip, base_commit);
    unimplemented!("scaffold: branch deletion rule")
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "1111111111111111111111111111111111111111";

    fn created_worktree_with_agent_and_claim() -> KickoffRollback {
        let mut rollback = KickoffRollback::new();
        rollback.record_worktree(PathBuf::from("/wt/feature-x"), Origin::Created);
        rollback.record_branch(
            "feature/feature-x".to_string(),
            Origin::Created,
            Some(BASE.to_string()),
        );
        rollback.record_agent();
        rollback.record_claim(7);
        rollback
    }

    fn tail_steps() -> Vec<UndoStep> {
        vec![
            UndoStep::ReleaseLock { issue_id: 7 },
            UndoStep::EndSession,
            UndoStep::StopDaemon,
        ]
    }

    fn removal_steps() -> Vec<UndoStep> {
        vec![
            UndoStep::RemoveWorktree {
                path: PathBuf::from("/wt/feature-x"),
            },
            UndoStep::DeleteBranch {
                name: "feature/feature-x".to_string(),
                base_commit: BASE.to_string(),
            },
        ]
    }

    #[derive(Default)]
    struct RecordingOps {
        calls: Vec<UndoStep>,
        outcomes: Vec<(UndoStep, UndoOutcome)>,
    }

    impl UndoOps for RecordingOps {
        fn run(&mut self, step: &UndoStep) -> UndoOutcome {
            self.calls.push(step.clone());
            self.outcomes
                .iter()
                .find(|(s, _)| s == step)
                .map_or(UndoOutcome::Done, |(_, outcome)| outcome.clone())
        }
    }

    /// AC-8: activation fails after the claim.
    #[test]
    fn activation_failure_undoes_in_reverse_order() {
        let rollback = created_worktree_with_agent_and_claim();
        let mut expected = tail_steps();
        expected.extend(removal_steps());
        assert_eq!(rollback.steps(), expected);
    }

    /// AC-8: container start or early container exit, for `run` after
    /// `mark_running`.
    #[test]
    fn container_failure_removes_the_container_first_and_restores_the_pipeline() {
        let mut rollback = created_worktree_with_agent_and_claim();
        rollback.record_pipeline(PathBuf::from("/repo/.design/x.md"), None);
        rollback.record_container("docker".to_string(), "crosslink-feature-x".to_string());
        let mut expected = vec![UndoStep::RemoveContainer {
            runtime: "docker".to_string(),
            name: "crosslink-feature-x".to_string(),
        }];
        expected.extend(tail_steps());
        expected.push(UndoStep::RestorePipeline {
            doc_path: PathBuf::from("/repo/.design/x.md"),
        });
        expected.extend(removal_steps());
        assert_eq!(rollback.steps(), expected);
    }

    /// AC-8: tmux start fails after the session was created.
    #[test]
    fn tmux_failure_kills_the_session_first() {
        let mut rollback = created_worktree_with_agent_and_claim();
        rollback.record_tmux_session("feat-x".to_string());
        let steps = rollback.steps();
        assert_eq!(
            steps.first(),
            Some(&UndoStep::KillTmuxSession {
                name: "feat-x".to_string()
            })
        );
        assert_eq!(steps.len(), 6);
    }

    /// AC-9: a reused worktree and branch are never removed.
    #[test]
    fn reused_worktree_and_branch_are_kept() {
        let mut rollback = KickoffRollback::new();
        rollback.record_worktree(PathBuf::from("/wt/feature-x"), Origin::Reused);
        rollback.record_branch("feature/feature-x".to_string(), Origin::Reused, None);
        rollback.record_agent();
        rollback.record_claim(7);
        assert_eq!(rollback.steps(), tail_steps());
    }

    /// AC-11 (unit level): without a recorded base, the branch is kept.
    #[test]
    fn branch_without_a_recorded_base_is_kept() {
        let mut rollback = created_worktree_with_agent_and_claim();
        rollback.record_branch("feature/feature-x".to_string(), Origin::Created, None);
        let steps = rollback.steps();
        assert!(!steps
            .iter()
            .any(|step| matches!(step, UndoStep::DeleteBranch { .. })));
    }

    /// AC-10: once the agent has started, nothing is undone.
    #[test]
    fn disarmed_rollback_does_nothing() {
        let mut rollback = created_worktree_with_agent_and_claim();
        rollback.disarm();
        assert!(rollback.steps().is_empty());
        let mut ops = RecordingOps::default();
        let report = rollback.unwind(&mut ops);
        assert!(ops.calls.is_empty());
        assert!(report.entries.is_empty());
    }

    /// AC-12: a stranded lock and a failed step are reported, and every
    /// remaining step still runs.
    #[test]
    fn failures_are_reported_and_do_not_stop_later_steps() {
        let rollback = created_worktree_with_agent_and_claim();
        let expected_calls = rollback.steps();
        let mut ops = RecordingOps {
            outcomes: vec![
                (
                    UndoStep::ReleaseLock { issue_id: 7 },
                    UndoOutcome::Stranded {
                        remedy: "crosslink locks release 7".to_string(),
                    },
                ),
                (
                    UndoStep::StopDaemon,
                    UndoOutcome::Failed {
                        error: "daemon did not stop".to_string(),
                        remedy: "crosslink daemon stop".to_string(),
                    },
                ),
            ],
            ..RecordingOps::default()
        };
        let report = rollback.unwind(&mut ops);
        assert_eq!(ops.calls, expected_calls);
        assert_eq!(report.entries.len(), expected_calls.len());
        assert!(!report.is_clean());
        assert!(report
            .entries
            .iter()
            .any(|(_, outcome)| matches!(outcome, UndoOutcome::Stranded { .. })));
    }

    #[test]
    fn branch_is_deletable_only_at_its_recorded_base() {
        assert!(branch_deletable(BASE, Some(BASE)));
        assert!(!branch_deletable(
            "2222222222222222222222222222222222222222",
            Some(BASE)
        ));
        assert!(!branch_deletable(BASE, None));
    }

    /// REQ-10 (guard): metadata written before `base_commit` existed still
    /// parses, and a recorded base round-trips.
    #[test]
    fn metadata_base_commit_is_optional_and_round_trips() {
        let old = r#"{"started_at":"2026-10-01T00:00:00Z","timeout_secs":3600}"#;
        let parsed: super::super::types::KickoffMetadata =
            serde_json::from_str(old).expect("old metadata parses");
        assert_eq!(parsed.base_commit, None);

        let mut with_base = parsed;
        with_base.base_commit = Some(BASE.to_string());
        let json = serde_json::to_string(&with_base).expect("serializes");
        assert!(json.contains(BASE));
        let back: super::super::types::KickoffMetadata =
            serde_json::from_str(&json).expect("round-trips");
        assert_eq!(back.base_commit.as_deref(), Some(BASE));
    }
}
