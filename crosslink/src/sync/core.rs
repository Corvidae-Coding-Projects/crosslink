use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

use super::{read_tracker_remote, HUB_CACHE_DIR};
use crate::signing;
use crate::utils::resolve_main_repo_root;

pub struct SyncManager {
    pub(super) crosslink_dir: PathBuf,

    pub(super) cache_dir: PathBuf,

    pub(super) repo_root: PathBuf,

    pub(super) remote: String,

    pub(super) hub_mode: std::cell::Cell<crate::hub_v3::HubMode>,
}

impl SyncManager {
    pub fn new(crosslink_dir: &Path) -> Result<Self> {
        let local_repo_root = crosslink_dir
            .parent()
            .ok_or_else(|| anyhow::anyhow!("Cannot determine repo root from .crosslink dir"))?
            .to_path_buf();

        let repo_root =
            resolve_main_repo_root(&local_repo_root).unwrap_or_else(|| local_repo_root.clone());

        let cache_dir = repo_root.join(".crosslink").join(HUB_CACHE_DIR);
        let remote = read_tracker_remote(crosslink_dir);

        let hub_mode = crate::hub_v3::HubMode::resolve(&cache_dir);

        Ok(Self {
            crosslink_dir: crosslink_dir.to_path_buf(),
            cache_dir,
            repo_root,
            remote,
            hub_mode: std::cell::Cell::new(hub_mode),
        })
    }

    #[must_use]
    pub const fn hub_mode(&self) -> crate::hub_v3::HubMode {
        self.hub_mode.get()
    }

    #[must_use]
    pub fn remote(&self) -> &str {
        &self.remote
    }

    #[must_use]
    pub(crate) fn reconciliation_remote(&self) -> &str {
        if self.remote_exists() {
            &self.remote
        } else {
            "."
        }
    }

    #[must_use]
    pub fn is_initialized(&self) -> bool {
        self.cache_dir.exists()
    }

    pub(crate) fn validate_cache_repository(&self) -> Result<()> {
        anyhow::ensure!(
            self.cache_dir.is_dir(),
            "repository authority cache is missing"
        );
        let output = Command::new("git")
            .current_dir(&self.cache_dir)
            .args(["rev-parse", "--show-toplevel"])
            .output()
            .context("validating repository authority cache")?;
        anyhow::ensure!(
            output.status.success(),
            "repository authority cache is not a Git worktree: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        let observed = PathBuf::from(
            String::from_utf8(output.stdout)
                .context("repository authority cache path was not UTF-8")?
                .trim(),
        );
        let expected = self
            .cache_dir
            .canonicalize()
            .context("canonicalizing repository authority cache")?;
        let observed = observed
            .canonicalize()
            .context("canonicalizing observed authority cache worktree")?;
        anyhow::ensure!(
            observed == expected,
            "repository authority cache resolves to a different Git worktree"
        );
        Ok(())
    }

    #[must_use]
    pub fn cache_path(&self) -> &Path {
        &self.cache_dir
    }

    #[must_use]
    pub fn remote_exists(&self) -> bool {
        Command::new("git")
            .current_dir(&self.repo_root)
            .args(["remote", "get-url", &self.remote])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    #[must_use]
    pub fn is_v2_layout(&self) -> bool {
        let meta_dir = self.cache_dir.join("meta");
        crate::issue_file::read_layout_version(&meta_dir).unwrap_or(1) >= 2
    }

    pub(super) fn cache_path_str(&self) -> String {
        self.cache_dir.to_str().map_or_else(
            || {
                tracing::error!(
                    "hub cache path contains non-UTF-8 characters: {:?}; \
                     git operations may fail",
                    self.cache_dir
                );
                self.cache_dir.to_string_lossy().to_string()
            },
            str::to_string,
        )
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

    pub(super) fn git_commit_in_cache(&self, args: &[&str]) -> Result<std::process::Output> {
        if let Err(e) = self.repair_stale_signingkey() {
            tracing::warn!("signingkey self-heal failed (non-fatal): {e}");
        }

        let local_configured = Command::new("git")
            .current_dir(&self.cache_dir)
            .args(["config", "--local", "commit.gpgsign"])
            .output()
            .is_ok_and(|o| o.status.success());
        let worktree_configured = Command::new("git")
            .current_dir(&self.cache_dir)
            .args(["config", "--worktree", "commit.gpgsign"])
            .output()
            .is_ok_and(|o| o.status.success());

        let mut cmd = Command::new("git");
        cmd.current_dir(&self.cache_dir);
        if !local_configured && !worktree_configured {
            cmd.args(["-c", "commit.gpgsign=false"]);
        }
        cmd.arg("commit").args(args);
        let output = cmd
            .output()
            .with_context(|| format!("Failed to run git commit {args:?} in cache"))?;
        if !output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!(
                "git commit {args:?} in cache failed ({}):\nstdout: {}\nstderr: {}",
                output.status,
                stdout.trim(),
                stderr.trim(),
            );
        }
        Ok(output)
    }

    pub(super) fn git_in_cache(&self, args: &[&str]) -> Result<std::process::Output> {
        let output = Command::new("git")
            .current_dir(&self.cache_dir)
            .args(args)
            .output()
            .with_context(|| format!("Failed to run git {args:?} in cache"))?;
        if !output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!(
                "git {args:?} in cache failed ({}):\nstdout: {}\nstderr: {}",
                output.status,
                stdout.trim(),
                stderr.trim(),
            );
        }
        Ok(output)
    }

    pub(super) fn propagate_agent_hooks(&self) -> Result<()> {
        let src = self
            .repo_root
            .join(".crosslink")
            .join("integrations")
            .join("hooks");
        if !src.is_dir() {
            return Ok(());
        }
        let dst = self
            .cache_dir
            .join(".crosslink")
            .join("integrations")
            .join("hooks");
        if dst.is_dir() {
            return Ok(());
        }
        std::fs::create_dir_all(&dst)?;
        for entry in std::fs::read_dir(&src)? {
            let entry = entry?;
            let ty = entry.file_type()?;
            if ty.is_file() {
                std::fs::copy(entry.path(), dst.join(entry.file_name()))?;
            }
        }
        Ok(())
    }

    pub(super) fn ensure_cache_git_identity(&self) -> Result<()> {
        let has_identity = git_config_nonempty(&self.cache_dir, "user.email")
            && git_config_nonempty(&self.cache_dir, "user.name");
        if !has_identity {
            let use_worktree = signing::is_linked_worktree(&self.cache_dir);
            if use_worktree {
                signing::enable_worktree_config(&self.cache_dir)?;
            }
            let scope_flag = if use_worktree {
                "--worktree"
            } else {
                "--local"
            };
            let email_output = Command::new("git")
                .current_dir(&self.cache_dir)
                .args(["config", scope_flag, "user.email", "crosslink@localhost"])
                .output()
                .context("Failed to run git config for user.email")?;
            if !email_output.status.success() {
                bail!(
                    "git config {} user.email failed: {}",
                    scope_flag,
                    String::from_utf8_lossy(&email_output.stderr)
                );
            }

            let name_output = Command::new("git")
                .current_dir(&self.cache_dir)
                .args(["config", scope_flag, "user.name", "crosslink"])
                .output()
                .context("Failed to run git config for user.name")?;
            if !name_output.status.success() {
                bail!(
                    "git config {} user.name failed: {}",
                    scope_flag,
                    String::from_utf8_lossy(&name_output.stderr)
                );
            }

            if !(git_config_nonempty(&self.cache_dir, "user.email")
                && git_config_nonempty(&self.cache_dir, "user.name"))
            {
                bail!(
                    "Failed to verify git identity in hub cache: \
                     git config set succeeded but user.email/user.name is empty or unreadable"
                );
            }
        }
        Ok(())
    }
}

fn git_config_nonempty(dir: &Path, key: &str) -> bool {
    Command::new("git")
        .current_dir(dir)
        .args(["config", key])
        .output()
        .is_ok_and(|o| o.status.success() && !String::from_utf8_lossy(&o.stdout).trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::git_config_nonempty;
    use std::path::Path;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) {
        assert!(
            Command::new("git")
                .current_dir(dir)
                .args(args)
                .status()
                .unwrap()
                .success(),
            "git {args:?} failed"
        );
    }

    #[test]
    fn git_config_nonempty_rejects_unset_and_empty_identity() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        git(d, &["init", "-q"]);

        assert!(!git_config_nonempty(d, "user.email"));

        git(d, &["config", "user.email", ""]);
        assert!(!git_config_nonempty(d, "user.email"));

        git(d, &["config", "user.email", "someone@example.com"]);
        assert!(git_config_nonempty(d, "user.email"));
    }
}
