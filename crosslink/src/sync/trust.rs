use anyhow::{bail, Context, Result};
use chrono::Utc;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::core::SyncManager;
use super::SignatureVerification;
use crate::identity::{AgentConfig, AgentRole};
use crate::signing;

fn home_dir() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        std::env::var("USERPROFILE").ok().map(PathBuf::from)
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::env::var("HOME").ok().map(PathBuf::from)
    }
}

fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        return home_dir().map_or_else(
            || {
                tracing::warn!(
                    "tilde expansion failed: cannot determine home directory for '{}'",
                    path
                );
                PathBuf::from(path)
            },
            |home| home.join(rest),
        );
    }
    if path == "~" {
        return home_dir().unwrap_or_else(|| PathBuf::from(path));
    }
    PathBuf::from(path)
}

pub(super) fn resolve_agent_key(worktree_crosslink_dir: &Path, rel_key: &str) -> PathBuf {
    let host = crate::signing::host_crosslink_dir(worktree_crosslink_dir);
    let host_path = host.join(rel_key);
    if host_path.exists() {
        return host_path;
    }
    worktree_crosslink_dir.join(rel_key)
}

fn signingkey_value_is_path(value: &str) -> bool {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return false;
    }

    if trimmed.starts_with("ssh-")
        || trimmed.starts_with("ecdsa-")
        || trimmed.starts_with("sk-")
        || trimmed.starts_with("-----BEGIN")
    {
        return false;
    }
    true
}

impl SyncManager {
    pub fn configure_signing(&self, crosslink_dir: &Path) -> Result<()> {
        if !self.cache_dir.exists() {
            return Ok(());
        }

        let allowed_signers = self.cache_dir.join("trust").join("allowed_signers");
        if !allowed_signers.exists() {
            signing::AllowedSigners::default().save(&allowed_signers)?;
        }

        let is_agent_worktree =
            AgentConfig::load(crosslink_dir)?.is_some_and(|c| matches!(c.role, AgentRole::Agent));

        if is_agent_worktree {
            if let Some(agent) = AgentConfig::load(crosslink_dir)? {
                if let (Some(rel_key), Some(_)) = (&agent.ssh_key_path, &agent.ssh_fingerprint) {
                    let private_key = resolve_agent_key(&self.crosslink_dir, rel_key);
                    if private_key.exists() {
                        signing::configure_git_ssh_signing(
                            &self.cache_dir,
                            &private_key,
                            Some(&allowed_signers),
                        )?;
                        register_active_key_as_trusted(
                            &self.cache_dir,
                            crosslink_dir,
                            &private_key,
                            &allowed_signers,
                        )?;
                        return Ok(());
                    }
                }
            }
        }

        if let Some(driver_key) = self.driver_signing_key() {
            if driver_key.exists() {
                signing::configure_git_ssh_signing(
                    &self.cache_dir,
                    &driver_key,
                    Some(&allowed_signers),
                )?;
                register_active_key_as_trusted(
                    &self.cache_dir,
                    crosslink_dir,
                    &driver_key,
                    &allowed_signers,
                )?;
                return Ok(());
            }
            tracing::warn!(
                "driver signing key configured but not found at {}; falling back to agent key",
                driver_key.display()
            );
        }

        if let Some(agent) = AgentConfig::load(crosslink_dir)? {
            if let (Some(rel_key), Some(_)) = (&agent.ssh_key_path, &agent.ssh_fingerprint) {
                let private_key = resolve_agent_key(&self.crosslink_dir, rel_key);
                if private_key.exists() {
                    signing::configure_git_ssh_signing(
                        &self.cache_dir,
                        &private_key,
                        Some(&allowed_signers),
                    )?;
                    register_active_key_as_trusted(
                        &self.cache_dir,
                        crosslink_dir,
                        &private_key,
                        &allowed_signers,
                    )?;
                    return Ok(());
                }
            }
        }

        tracing::warn!(
            "no usable signing key for {} workspace; disabling hub-commit signing",
            if is_agent_worktree { "agent" } else { "driver" }
        );
        signing::disable_git_signing(&self.cache_dir)?;
        Ok(())
    }

    fn driver_signing_key(&self) -> Option<std::path::PathBuf> {
        let output = Command::new("git")
            .current_dir(&self.repo_root)
            .args(["config", "user.signingkey"])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if raw.is_empty() {
            return None;
        }
        Some(expand_tilde(&raw))
    }

    pub(super) fn fallback_to_driver_signing(&self) -> Result<()> {
        let output = Command::new("git")
            .current_dir(&self.repo_root)
            .args(["config", "user.signingkey"])
            .output();

        let driver_key = output.ok().and_then(|o| {
            if o.status.success() {
                let key = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if key.is_empty() {
                    None
                } else {
                    Some(key)
                }
            } else {
                None
            }
        });

        if let Some(key_path) = driver_key {
            let expanded = expand_tilde(&key_path);

            if expanded.exists() {
                tracing::info!(
                    "agent key missing, falling back to driver signing key: {}",
                    expanded.display()
                );
                signing::configure_git_ssh_signing(&self.cache_dir, &expanded, None)?;
            } else {
                tracing::warn!(
                    "agent key missing and driver key not found at {}, disabling signing",
                    expanded.display()
                );
                signing::disable_git_signing(&self.cache_dir)?;
            }
        } else {
            tracing::warn!(
                "agent key missing and no driver signing key configured, disabling signing"
            );
            signing::disable_git_signing(&self.cache_dir)?;
        }

        Ok(())
    }

    pub fn repair_stale_signingkey(&self) -> Result<bool> {
        if !self.cache_dir.exists() {
            return Ok(false);
        }

        let output = Command::new("git")
            .current_dir(&self.cache_dir)
            .args(["config", "user.signingkey"])
            .output();

        let Ok(output) = output else { return Ok(false) };
        if !output.status.success() {
            return Ok(false);
        }

        let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !signingkey_value_is_path(&value) {
            return Ok(false);
        }

        let expanded = expand_tilde(&value);
        if expanded.exists() {
            return Ok(false);
        }

        tracing::warn!(
            "hub-cache user.signingkey points at missing file '{}' \
             (agent worktree likely deleted) — repairing (GH #565)",
            expanded.display()
        );
        self.fallback_to_driver_signing()?;
        Ok(true)
    }

    pub fn ensure_agent_key_published(&self, crosslink_dir: &Path) -> Result<bool> {
        if !self.cache_dir.exists() {
            return Ok(false);
        }

        let Some(agent) = AgentConfig::load(crosslink_dir)? else {
            return Ok(false);
        };

        let Some(public_key) = agent.ssh_public_key.clone() else {
            return Ok(false);
        };

        let key_file = self
            .cache_dir
            .join("trust")
            .join("keys")
            .join(format!("{}.pub", agent.agent_id));

        if key_file.exists() {
            return Ok(false);
        }

        let keys_dir = self.cache_dir.join("trust").join("keys");
        std::fs::create_dir_all(&keys_dir)?;
        std::fs::write(&key_file, format!("{public_key}\n"))?;

        self.git_in_cache(&["add", "trust/"])?;

        let output = Command::new("git")
            .current_dir(&self.cache_dir)
            .args([
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-m",
                &format!("trust: publish key for agent '{}'", agent.agent_id),
            ])
            .output()
            .context("Failed to commit key publication")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if !stderr.contains("nothing to commit") {
                bail!("git commit for key publication failed: {stderr}");
            }
        }

        Ok(true)
    }

    pub fn read_allowed_signers(&self) -> Result<signing::AllowedSigners> {
        let path = self.cache_dir.join("trust").join("allowed_signers");
        signing::AllowedSigners::load(&path)
    }

    fn verify_commit_signature(&self, commit: &str) -> Result<SignatureVerification> {
        let verify = Command::new("git")
            .current_dir(&self.cache_dir)
            .args(["verify-commit", "--raw", commit])
            .output()
            .context("Failed to run git verify-commit")?;

        let stderr = String::from_utf8_lossy(&verify.stderr);

        if verify.status.success() {
            Ok(SignatureVerification::Valid)
        } else if stderr.contains("NODATA")
            || stderr.contains("no signature")
            || stderr.is_empty()
            || stderr.contains("allowedSignersFile needs to be configured")
        {
            Ok(SignatureVerification::Unsigned)
        } else {
            Ok(SignatureVerification::Invalid)
        }
    }

    pub fn verify_hub_signature_auto(&self) -> Result<SignatureVerification> {
        if self.hub_mode.get().is_v3() {
            let Some(tip) = crate::hub_v3::git_rev_parse_optional(
                &self.cache_dir,
                crate::hub_v3::CHECKPOINT_REF,
            )?
            else {
                return Ok(SignatureVerification::NoCommits);
            };
            return self.verify_commit_signature(&tip);
        }
        self.verify_locks_signature()
    }

    pub fn verify_locks_signature(&self) -> Result<SignatureVerification> {
        let output = self.git_in_cache(&["log", "-1", "--format=%H", "--", "locks.json"])?;
        let commit = String::from_utf8_lossy(&output.stdout).trim().to_string();

        if commit.is_empty() {
            return Ok(SignatureVerification::NoCommits);
        }

        self.verify_commit_signature(&commit)
    }
}

fn register_active_key_as_trusted(
    cache_dir: &Path,
    crosslink_dir: &Path,
    private_key_path: &Path,
    allowed_signers_path: &Path,
) -> Result<bool> {
    use crate::signing::{AllowedSignerEntry, AllowedSigners};

    let public_key_path = with_pub_extension(private_key_path);
    let public_key = match crate::signing::read_public_key(&public_key_path) {
        Ok(k) => k,
        Err(e) => {
            tracing::debug!(
                "skipping allowed_signers self-registration: cannot read pubkey at {}: {e}",
                public_key_path.display()
            );
            return Ok(false);
        }
    };

    let mut signers = AllowedSigners::load(allowed_signers_path)?;
    if signers.contains_key(&public_key) {
        return Ok(false);
    }

    let principal = AgentConfig::load(crosslink_dir)?.map_or_else(
        || "driver@crosslink".to_string(),
        |c| format!("{}@crosslink", c.agent_id),
    );

    signers.add_entry(AllowedSignerEntry {
        principal: principal.clone(),
        public_key,
        metadata_comment: Some(format!(
            "self-registered as workspace signing key at {}",
            Utc::now().format("%Y-%m-%d %H:%M:%S UTC")
        )),
    });
    signers.save(allowed_signers_path)?;

    let bootstrap_completed_now =
        if let Some(state) = super::bootstrap::read_bootstrap_state(cache_dir) {
            if state.status == "pending" {
                super::bootstrap::complete_bootstrap(cache_dir)?;
                true
            } else {
                false
            }
        } else {
            false
        };

    if let Err(e) = commit_allowed_signers_unsigned(cache_dir, &principal) {
        tracing::warn!(
            "registered '{principal}' in allowed_signers on disk but commit failed: {e} \
             (run `crosslink sync` to recover)"
        );
    } else if bootstrap_completed_now {
        tracing::info!("bootstrap completed: self-registered '{principal}' as trusted signer");
    }

    Ok(true)
}

fn with_pub_extension(private_key_path: &Path) -> PathBuf {
    let mut s = private_key_path.as_os_str().to_owned();
    s.push(".pub");
    PathBuf::from(s)
}

fn commit_allowed_signers_unsigned(cache_dir: &Path, principal: &str) -> Result<()> {
    let run = |args: &[&str]| -> Result<()> {
        let output = Command::new("git")
            .current_dir(cache_dir)
            .args(args)
            .output()
            .with_context(|| format!("failed to spawn git {args:?}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);

            if !stderr.contains("nothing to commit") {
                bail!("git {args:?} failed: {}", stderr.trim());
            }
        }
        Ok(())
    };

    run(&["add", "trust/allowed_signers"])?;

    if cache_dir.join("meta").join("bootstrap.json").exists() {
        let _ = run(&["add", "meta/bootstrap.json"]);
    }
    run(&[
        "-c",
        "commit.gpgsign=false",
        "commit",
        "-m",
        &format!("trust: register signing key for '{principal}'"),
    ])?;
    Ok(())
}
