pub mod bootstrap;
mod cache;
mod core;
mod heartbeats;
mod locks;
mod migration;
mod trust;

#[cfg(test)]
mod tests;

use std::path::Path;
use std::process::Command;
use std::sync::Once;

pub(crate) const HUB_CACHE_DIR: &str = ".hub-cache";

pub(crate) const HUB_BRANCH: &str = "crosslink/hub";

pub(crate) const HUB_V3_HOST_BRANCH: &str = "crosslink/hub-v3-host";

const OLD_CACHE_DIR: &str = ".locks-cache";

const OLD_BRANCH: &str = "crosslink/locks";

pub use crate::signing::SignatureVerification;

pub use self::cache::HubWriteLock;

#[cfg(test)]
pub use self::cache::acquire_hub_lock;
pub use self::core::SyncManager;

pub fn read_tracker_remote(crosslink_dir: &Path) -> String {
    static CORRUPT_WARNED: Once = Once::new();

    let config_path = crosslink_dir.join("hook-config.json");
    let configured = std::fs::read_to_string(&config_path)
        .ok()
        .and_then(|content| serde_json::from_str::<serde_json::Value>(&content).ok())
        .and_then(|v| {
            v.get("tracker_remote")
                .and_then(|r| r.as_str().map(std::string::ToString::to_string))
        });

    if let Some(remote) = configured {
        if remote == "(text)" {
            CORRUPT_WARNED.call_once(|| {
                tracing::warn!(
                    "tracker_remote in {} is the corrupt placeholder \"(text)\" \
                     (GH#739). Falling back to inferred remote. Repair with: \
                     `crosslink config set tracker_remote <name>` or \
                     `crosslink init --force`.",
                    config_path.display()
                );
            });
        } else {
            return remote;
        }
    }

    let repo_path = crosslink_dir.parent().unwrap_or(crosslink_dir);
    infer_tracker_remote(repo_path)
}

fn list_git_remotes(repo_path: &Path) -> Vec<String> {
    let output = Command::new("git")
        .current_dir(repo_path)
        .args(["remote"])
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let mut remotes: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    remotes.sort();
    remotes
}

fn infer_tracker_remote(repo_path: &Path) -> String {
    static NO_REMOTE_WARNED: Once = Once::new();

    let remotes = list_git_remotes(repo_path);
    if remotes.iter().any(|r| r == "origin") {
        return "origin".to_string();
    }
    if let Some(first) = remotes.first() {
        return first.clone();
    }

    NO_REMOTE_WARNED.call_once(|| {
        tracing::warn!(
            "no git remote configured in {}; defaulting tracker_remote to \"origin\". \
             Add a remote with `git remote add origin <url>` before `crosslink sync`.",
            repo_path.display()
        );
    });
    "origin".to_string()
}

#[allow(dead_code)]
#[must_use]
pub fn validate_remote_exists(repo_root: &Path, remote: &str) -> bool {
    std::process::Command::new("git")
        .current_dir(repo_root)
        .args(["remote", "get-url", remote])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}
