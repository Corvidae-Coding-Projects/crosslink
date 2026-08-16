use anyhow::{bail, Context, Result};
use chrono::Utc;
use std::process::Command;

use super::core::{
    has_conflict_markers, resolve_accept_both, KnowledgeManager, SyncOutcome, KNOWLEDGE_BRANCH,
};

impl KnowledgeManager {
    pub fn init_cache(&self) -> Result<()> {
        if self.cache_dir.exists() {
            return Ok(());
        }

        let has_remote = self
            .git_in_repo(&["ls-remote", "--heads", &self.remote, KNOWLEDGE_BRANCH])
            .is_ok_and(|o| !String::from_utf8_lossy(&o.stdout).trim().is_empty());

        if has_remote {
            self.git_in_repo(&["fetch", &self.remote, KNOWLEDGE_BRANCH])?;

            let has_local = self
                .git_in_repo(&["rev-parse", "--verify", KNOWLEDGE_BRANCH])
                .is_ok();

            if has_local {
                self.git_in_repo(&["worktree", "add", &self.cache_path_str(), KNOWLEDGE_BRANCH])?;
            } else {
                let remote_ref = format!("{}/{}", self.remote, KNOWLEDGE_BRANCH);
                self.git_in_repo(&[
                    "worktree",
                    "add",
                    "-b",
                    KNOWLEDGE_BRANCH,
                    &self.cache_path_str(),
                    &remote_ref,
                ])?;
            }
        } else {
            crate::git_compat::add_orphan_worktree(
                &self.repo_root,
                KNOWLEDGE_BRANCH,
                &self.cache_path_str(),
            )?;

            let now = Utc::now().format("%Y-%m-%d").to_string();
            let index_content = format!(
                "\
---
title: Knowledge Index
tags: [index]
sources: []
contributors: []
created: {now}
updated: {now}
---

# Knowledge Index

This is the shared knowledge repository for the project.
"
            );

            std::fs::write(self.cache_dir.join("index.md"), index_content)?;

            self.git_in_cache(&["add", "index.md"])?;
            self.git_in_cache(&["commit", "-m", "Initialize crosslink/knowledge branch"])?;
        }

        Ok(())
    }

    pub fn sync(&self) -> Result<SyncOutcome> {
        let fetch_result = self.git_in_cache(&["fetch", &self.remote, KNOWLEDGE_BRANCH]);
        if let Err(e) = &fetch_result {
            let err_str = e.to_string();
            if err_str.contains("Could not resolve host")
                || err_str.contains("Could not read from remote")
                || err_str.contains("does not appear to be a git repository")
                || err_str.contains("No such remote")
                || err_str.contains("couldn't find remote ref")
            {
                return Ok(SyncOutcome::default());
            }
            fetch_result?;
        }

        let remote_ref = format!("{}/{}", self.remote, KNOWLEDGE_BRANCH);
        let log_result = self.git_in_cache(&["log", &format!("{remote_ref}..HEAD"), "--oneline"]);
        if let Ok(output) = &log_result {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if !stdout.trim().is_empty() {
                let rebase_result = self.git_in_cache(&["rebase", &remote_ref]);
                if let Err(e) = &rebase_result {
                    let err_str = e.to_string();
                    if err_str.contains("unknown revision")
                        || err_str.contains("ambiguous argument")
                    {
                        return Ok(SyncOutcome::default());
                    }

                    let outcome = self.handle_rebase_conflict(&remote_ref)?;
                    if !outcome.resolved_conflicts.is_empty() {
                        return Ok(outcome);
                    }
                    rebase_result?;
                }
                return Ok(SyncOutcome::default());
            }
        }

        if let Ok(status_output) = self.git_in_cache(&["status", "--porcelain"]) {
            let status_str = String::from_utf8_lossy(&status_output.stdout);
            if !status_str.trim().is_empty() {
                tracing::warn!("knowledge sync: skipping reset — worktree has uncommitted changes");
                return Ok(SyncOutcome::default());
            }
        }
        let reset_result = self.git_in_cache(&["reset", "--hard", &remote_ref]);
        if let Err(e) = &reset_result {
            let err_str = e.to_string();
            if err_str.contains("unknown revision") || err_str.contains("ambiguous argument") {
                return Ok(SyncOutcome::default());
            }
            reset_result?;
        }

        Ok(SyncOutcome::default())
    }

    pub fn push(&self) -> Result<SyncOutcome> {
        let push_result = self.git_in_cache(&["push", &self.remote, KNOWLEDGE_BRANCH]);
        if let Err(e) = &push_result {
            let err_str = e.to_string();
            if err_str.contains("Could not resolve host")
                || err_str.contains("Could not read from remote")
            {
                return Ok(SyncOutcome::default());
            }
            if err_str.contains("rejected") || err_str.contains("non-fast-forward") {
                let remote_ref = format!("{}/{}", self.remote, KNOWLEDGE_BRANCH);

                let _ = self.git_in_cache(&["fetch", &self.remote, KNOWLEDGE_BRANCH]);

                let rebase_result = self.git_in_cache(&["rebase", &remote_ref]);
                if rebase_result.is_err() {
                    let outcome = self.handle_rebase_conflict(&remote_ref)?;

                    if let Err(e) = self.git_in_cache(&["push", &self.remote, KNOWLEDGE_BRANCH]) {
                        tracing::warn!("knowledge push after conflict resolution failed: {e}");
                    }
                    return Ok(outcome);
                }

                if let Err(e) = self.git_in_cache(&["push", &self.remote, KNOWLEDGE_BRANCH]) {
                    tracing::warn!("knowledge push after rebase failed: {e}");
                }
                return Ok(SyncOutcome::default());
            }
            push_result?;
        }
        Ok(SyncOutcome::default())
    }

    pub(super) fn handle_rebase_conflict(&self, remote_ref: &str) -> Result<SyncOutcome> {
        let _ = self.git_in_cache(&["rebase", "--abort"]);

        let merge_result = self.git_in_cache(&["merge", remote_ref, "--no-edit"]);

        let resolved = if merge_result.is_err() {
            self.resolve_conflicts_in_cache()?
        } else {
            Vec::new()
        };

        if !resolved.is_empty() {
            self.git_in_cache(&["add", "-A"])?;
            let slugs_str = resolved.join(", ");
            self.commit(&format!(
                "knowledge: accept-both conflict resolution for {slugs_str}"
            ))?;
        }

        Ok(SyncOutcome {
            resolved_conflicts: resolved,
        })
    }

    pub(super) fn resolve_conflicts_in_cache(&self) -> Result<Vec<String>> {
        let mut resolved = Vec::new();

        if !self.cache_dir.exists() {
            return Ok(resolved);
        }

        for entry in std::fs::read_dir(&self.cache_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "md") {
                let content = std::fs::read_to_string(&path)?;
                if has_conflict_markers(&content) {
                    let slug = path
                        .file_stem()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string();
                    let resolved_content = resolve_accept_both(&content);
                    std::fs::write(&path, &resolved_content)?;
                    resolved.push(slug);
                }
            }
        }

        Ok(resolved)
    }

    pub fn commit(&self, message: &str) -> Result<()> {
        self.git_in_cache(&["add", "-A"])?;

        let commit_result = self.git_in_cache(&["commit", "-m", message]);
        if let Err(e) = &commit_result {
            let err_str = e.to_string();
            if err_str.contains("nothing to commit") || err_str.contains("no changes added") {
                return Ok(());
            }
            commit_result?;
        }
        Ok(())
    }

    pub(super) fn git_in_repo(&self, args: &[&str]) -> Result<std::process::Output> {
        let output = Command::new("git")
            .current_dir(&self.repo_root)
            .args(args)
            .output()
            .with_context(|| format!("Failed to run git {args:?}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("git {args:?} failed: {stderr}");
        }
        Ok(output)
    }

    pub(super) fn git_in_cache(&self, args: &[&str]) -> Result<std::process::Output> {
        let output = Command::new("git")
            .current_dir(&self.cache_dir)
            .args(args)
            .output()
            .with_context(|| format!("Failed to run git {args:?} in knowledge cache"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("git {args:?} in knowledge cache failed: {stderr}");
        }
        Ok(output)
    }
}
