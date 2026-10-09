//! Undo for a kickoff that fails before its agent starts, and the lock,
//! session and branch steps that `kickoff cleanup` shares with it.
//!
//! Design: `.design/lock-claim-outcome-and-kickoff-rollback.md`.

use std::path::{Path, PathBuf};
use std::process::Command;

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
        self.entries
            .iter()
            .all(|(_, outcome)| matches!(outcome, UndoOutcome::Done | UndoOutcome::NothingToUndo))
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
        if self.disarmed {
            return Vec::new();
        }
        let mut steps = Vec::new();
        if let Some((runtime, name)) = &self.container {
            steps.push(UndoStep::RemoveContainer {
                runtime: runtime.clone(),
                name: name.clone(),
            });
        }
        if let Some(name) = &self.tmux_session {
            steps.push(UndoStep::KillTmuxSession { name: name.clone() });
        }
        if self.agent_initialized {
            if let Some(issue_id) = self.claimed_issue {
                steps.push(UndoStep::ReleaseLock { issue_id });
            }
            steps.push(UndoStep::EndSession);
            steps.push(UndoStep::StopDaemon);
        }
        if let Some((doc_path, _)) = &self.pipeline {
            steps.push(UndoStep::RestorePipeline {
                doc_path: doc_path.clone(),
            });
        }
        if let Some((path, Origin::Created)) = &self.worktree {
            steps.push(UndoStep::RemoveWorktree { path: path.clone() });
        }
        if let Some((name, Origin::Created, Some(base_commit))) = &self.branch {
            steps.push(UndoStep::DeleteBranch {
                name: name.clone(),
                base_commit: base_commit.clone(),
            });
        }
        steps
    }

    /// The pipeline state recorded before kickoff changed it, for the
    /// `RestorePipeline` step.
    pub(super) fn prior_pipeline(&self) -> Option<&PipelineState> {
        self.pipeline.as_ref().and_then(|(_, prior)| prior.as_ref())
    }

    /// Runs every step, continuing past failures, and reports each outcome.
    pub(super) fn unwind(self, ops: &mut impl UndoOps) -> UndoReport {
        let entries = self
            .steps()
            .into_iter()
            .map(|step| {
                let outcome = ops.run(&step);
                (step, outcome)
            })
            .collect();
        UndoReport { entries }
    }
}

/// Runs undo steps against the system for one kickoff worktree.
pub(super) struct KickoffUndo<'a> {
    pub(super) repo_root: &'a Path,
    pub(super) worktree_dir: &'a Path,
    pub(super) prior_pipeline: Option<PipelineState>,
}

impl KickoffUndo<'_> {
    fn worktree_crosslink(&self) -> PathBuf {
        self.worktree_dir.join(".crosslink")
    }

    fn has_agent(&self) -> bool {
        self.worktree_crosslink().join("agent.json").exists()
    }

    fn release_lock(&self, issue_id: i64) -> UndoOutcome {
        if !self.has_agent() {
            return UndoOutcome::NothingToUndo;
        }
        let remedy = format!(
            "run `crosslink locks release {issue_id}` in {}",
            self.worktree_dir.display()
        );
        let writer = match crate::shared_writer::SharedWriter::new(&self.worktree_crosslink()) {
            Ok(Some(writer)) => writer,
            Ok(None) => return UndoOutcome::NothingToUndo,
            Err(error) => {
                return UndoOutcome::Failed {
                    error: format!("{error:#}"),
                    remedy,
                }
            }
        };
        if let Err(error) = writer.release_lock_v2(issue_id) {
            return UndoOutcome::Failed {
                error: format!("{error:#}"),
                remedy,
            };
        }
        match writer.confirmed_lock_holder(issue_id) {
            Ok(Some(holder)) if holder == writer.agent_id() => UndoOutcome::Stranded { remedy },
            Ok(_) => UndoOutcome::Done,
            Err(error) => UndoOutcome::Unverified {
                remedy: format!("{remedy} (the release could not be confirmed: {error:#})"),
            },
        }
    }

    fn end_session(&self) -> UndoOutcome {
        if !self.has_agent() {
            return UndoOutcome::NothingToUndo;
        }
        command_outcome(
            Command::new("crosslink")
                .current_dir(self.worktree_dir)
                .args(["session", "end"]),
            &format!(
                "run `crosslink session end` in {}",
                self.worktree_dir.display()
            ),
            &["No active session"],
        )
    }

    fn stop_daemon(&self) -> UndoOutcome {
        let crosslink = self.worktree_crosslink();
        if !crate::reconcile::readiness::requires_readiness(&crosslink) {
            return UndoOutcome::NothingToUndo;
        }
        match crate::daemon::stop(&crosslink) {
            Ok(()) => UndoOutcome::Done,
            Err(error) => UndoOutcome::Failed {
                error: format!("{error:#}"),
                remedy: format!(
                    "run `crosslink daemon stop` in {}",
                    self.worktree_dir.display()
                ),
            },
        }
    }

    fn restore_pipeline(&self, doc_path: &Path) -> UndoOutcome {
        let result = if let Some(prior) = &self.prior_pipeline {
            super::pipeline::write_pipeline_state(doc_path, prior)
        } else {
            let path = super::pipeline::pipeline_path_for_doc(doc_path);
            if !path.exists() {
                return UndoOutcome::NothingToUndo;
            }
            std::fs::remove_file(&path).map_err(anyhow::Error::from)
        };
        match result {
            Ok(()) => UndoOutcome::Done,
            Err(error) => UndoOutcome::Failed {
                error: format!("{error:#}"),
                remedy: format!(
                    "restore {} by hand",
                    super::pipeline::pipeline_path_for_doc(doc_path).display()
                ),
            },
        }
    }

    fn remove_worktree(&self, path: &Path) -> UndoOutcome {
        if !path.exists() {
            return UndoOutcome::NothingToUndo;
        }
        let path_arg = path.to_string_lossy();
        command_outcome(
            Command::new("git")
                .current_dir(self.repo_root)
                .args(["worktree", "remove", "--force", &path_arg]),
            &format!("run `git worktree remove --force {path_arg}`"),
            &[],
        )
    }

    fn delete_branch(&self, name: &str, base_commit: &str) -> UndoOutcome {
        let tip = Command::new("git")
            .current_dir(self.repo_root)
            .args([
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{name}"),
            ])
            .output();
        let Ok(tip) = tip else {
            return UndoOutcome::NothingToUndo;
        };
        if !tip.status.success() {
            return UndoOutcome::NothingToUndo;
        }
        let tip = String::from_utf8_lossy(&tip.stdout).trim().to_string();
        if !branch_deletable(&tip, Some(base_commit)) {
            return UndoOutcome::NothingToUndo;
        }
        command_outcome(
            Command::new("git")
                .current_dir(self.repo_root)
                .args(["branch", "-D", name]),
            &format!("run `git branch -D {name}`"),
            &[],
        )
    }
}

impl UndoOps for KickoffUndo<'_> {
    fn run(&mut self, step: &UndoStep) -> UndoOutcome {
        match step {
            UndoStep::RemoveContainer { runtime, name } => command_outcome(
                Command::new(runtime).args(["rm", "-f", name]),
                &format!("run `{runtime} rm -f {name}`"),
                &["No such container", "no such container"],
            ),
            UndoStep::KillTmuxSession { name } => command_outcome(
                Command::new("tmux").args(["kill-session", "-t", name]),
                &format!("run `tmux kill-session -t {name}`"),
                &["can't find session", "no server running"],
            ),
            UndoStep::ReleaseLock { issue_id } => self.release_lock(*issue_id),
            UndoStep::EndSession => self.end_session(),
            UndoStep::StopDaemon => self.stop_daemon(),
            UndoStep::RestorePipeline { doc_path } => self.restore_pipeline(doc_path),
            UndoStep::RemoveWorktree { path } => self.remove_worktree(path),
            UndoStep::DeleteBranch { name, base_commit } => self.delete_branch(name, base_commit),
        }
    }
}

/// Runs a command for an undo step. Output matching one of `absent` means the
/// resource was already gone.
fn command_outcome(command: &mut Command, remedy: &str, absent: &[&str]) -> UndoOutcome {
    match command.output() {
        Ok(output) if output.status.success() => UndoOutcome::Done,
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if absent.iter().any(|needle| stderr.contains(needle)) {
                UndoOutcome::NothingToUndo
            } else {
                UndoOutcome::Failed {
                    error: stderr.trim().to_string(),
                    remedy: remedy.to_string(),
                }
            }
        }
        Err(error) => UndoOutcome::Failed {
            error: error.to_string(),
            remedy: remedy.to_string(),
        },
    }
}

impl UndoReport {
    /// Prints every step that did not complete cleanly, with its remedy, and
    /// one summary line.
    pub(super) fn print(&self) {
        for (step, outcome) in &self.entries {
            if let Some(message) = outcome_message(step, outcome) {
                eprintln!("kickoff undo: {message}");
            }
        }
        if self.is_clean() {
            eprintln!("kickoff undo: removed everything this kickoff created");
        } else {
            eprintln!("kickoff undo: some steps did not complete; see above");
        }
    }
}

/// What to tell the user about a step that did not complete cleanly, or
/// `None` when it did.
pub(super) fn outcome_message(step: &UndoStep, outcome: &UndoOutcome) -> Option<String> {
    match outcome {
        UndoOutcome::Done | UndoOutcome::NothingToUndo => None,
        UndoOutcome::Failed { error, remedy } => Some(format!(
            "{} failed: {error}; to clear it, {remedy}",
            step.describe()
        )),
        UndoOutcome::Stranded { remedy } => Some(format!(
            "{} left the lock held (stranded); to clear it, {remedy}",
            step.describe()
        )),
        UndoOutcome::Unverified { remedy } => Some(format!(
            "{} could not be verified; to check it, {remedy}",
            step.describe()
        )),
    }
}

impl UndoStep {
    fn describe(&self) -> String {
        match self {
            Self::RemoveContainer { name, .. } => format!("removing container {name}"),
            Self::KillTmuxSession { name } => format!("killing tmux session {name}"),
            Self::ReleaseLock { issue_id } => format!("releasing the lock on #{issue_id}"),
            Self::EndSession => "ending the worktree session".to_string(),
            Self::StopDaemon => "stopping the worktree daemon".to_string(),
            Self::RestorePipeline { doc_path } => {
                format!("restoring the pipeline state of {}", doc_path.display())
            }
            Self::RemoveWorktree { path } => format!("removing worktree {}", path.display()),
            Self::DeleteBranch { name, .. } => format!("deleting branch {name}"),
        }
    }
}

/// A branch may be deleted only when a base was recorded and the branch has
/// no commits beyond it.
pub(super) fn branch_deletable(tip: &str, base_commit: Option<&str>) -> bool {
    base_commit.is_some_and(|base| base == tip)
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

    fn git(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .expect("git runs");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    /// A repository with one commit and a kickoff-style worktree on a fresh
    /// branch. Returns the temp dir, repo root, worktree path and base commit.
    fn repo_with_worktree() -> (tempfile::TempDir, PathBuf, PathBuf, String) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("repo");
        std::fs::create_dir_all(&root).expect("repo dir");
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.email", "test@test.local"]);
        git(&root, &["config", "user.name", "Test"]);
        git(&root, &["config", "commit.gpgsign", "false"]);
        std::fs::write(root.join("README.md"), "# test\n").expect("readme");
        git(&root, &["add", "README.md"]);
        git(&root, &["commit", "-q", "-m", "init"]);
        let worktree = dir.path().join("wt");
        git(
            &root,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature/x",
                worktree.to_str().expect("utf-8 path"),
                "HEAD",
            ],
        );
        let base = git(&worktree, &["rev-parse", "HEAD"]);
        (dir, root, worktree, base)
    }

    /// AC-8 (operations): a created worktree and its unchanged branch are
    /// removed; steps with no agent have nothing to undo.
    #[test]
    fn undo_removes_a_created_worktree_and_its_unchanged_branch() {
        let (_dir, root, worktree, base) = repo_with_worktree();
        let mut rollback = KickoffRollback::new();
        rollback.record_worktree(worktree.clone(), Origin::Created);
        rollback.record_branch("feature/x".to_string(), Origin::Created, Some(base));
        rollback.record_agent();
        rollback.record_claim(7);
        let mut ops = KickoffUndo {
            repo_root: &root,
            worktree_dir: &worktree,
            prior_pipeline: None,
        };
        let report = rollback.unwind(&mut ops);
        assert!(report.is_clean(), "{:?}", report.entries);
        assert!(!worktree.exists());
        let branches = git(&root, &["branch", "--list", "feature/x"]);
        assert!(branches.is_empty(), "branch survived: {branches}");
        let nothing: Vec<_> = report
            .entries
            .iter()
            .filter(|(_, outcome)| *outcome == UndoOutcome::NothingToUndo)
            .map(|(step, _)| step.clone())
            .collect();
        assert!(nothing.contains(&UndoStep::ReleaseLock { issue_id: 7 }));
        assert!(nothing.contains(&UndoStep::EndSession));
    }

    /// AC-11 (operations): a branch with its own commits is kept.
    #[test]
    fn undo_keeps_a_branch_with_its_own_commits() {
        let (_dir, root, worktree, base) = repo_with_worktree();
        std::fs::write(worktree.join("work.txt"), "work\n").expect("work");
        git(&worktree, &["add", "work.txt"]);
        git(&worktree, &["commit", "-q", "-m", "agent work"]);
        let mut rollback = KickoffRollback::new();
        rollback.record_worktree(worktree.clone(), Origin::Created);
        rollback.record_branch("feature/x".to_string(), Origin::Created, Some(base));
        let mut ops = KickoffUndo {
            repo_root: &root,
            worktree_dir: &worktree,
            prior_pipeline: None,
        };
        let report = rollback.unwind(&mut ops);
        assert!(report.is_clean(), "{:?}", report.entries);
        assert!(!worktree.exists());
        let branches = git(&root, &["branch", "--list", "feature/x"]);
        assert!(!branches.is_empty(), "branch with commits was deleted");
    }

    /// AC-8 (pipeline): the prior state is written back, or a pipeline file
    /// kickoff created is removed.
    #[test]
    fn undo_restores_or_removes_the_pipeline_state() {
        let dir = tempfile::tempdir().expect("tempdir");
        let doc = dir.path().join("feature.md");
        std::fs::write(&doc, "# Feature\n").expect("doc");
        let prior = super::super::pipeline::create_initial_pipeline(&doc).expect("pipeline");
        let path = super::super::pipeline::pipeline_path_for_doc(&doc);
        super::super::pipeline::mark_running(&doc, "agent", "/wt", Some(7)).expect("running");
        assert_ne!(
            super::super::pipeline::read_pipeline_state(&doc)
                .expect("state")
                .stage,
            prior.stage
        );

        let step = UndoStep::RestorePipeline {
            doc_path: doc.clone(),
        };
        let mut ops = KickoffUndo {
            repo_root: dir.path(),
            worktree_dir: dir.path(),
            prior_pipeline: Some(prior.clone()),
        };
        assert_eq!(ops.run(&step), UndoOutcome::Done);
        let restored = super::super::pipeline::read_pipeline_state(&doc).expect("restored");
        assert_eq!(restored.stage, prior.stage);
        assert!(restored.runs.is_empty());

        ops.prior_pipeline = None;
        assert_eq!(ops.run(&step), UndoOutcome::Done);
        assert!(!path.exists());
        assert_eq!(ops.run(&step), UndoOutcome::NothingToUndo);
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
