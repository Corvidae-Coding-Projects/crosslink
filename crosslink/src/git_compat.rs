use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;

use anyhow::{bail, Context, Result};

const MIN_ORPHAN_WORKTREE: (u32, u32) = (2, 42);

const FORCE_FALLBACK_ENV: &str = "CROSSLINK_FORCE_WORKTREE_ORPHAN_FALLBACK";

fn parse_git_version(output: &str) -> Option<(u32, u32)> {
    let rest = output.trim().strip_prefix("git version ")?;
    let mut nums = rest
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty());
    let major = nums.next()?.parse().ok()?;
    let minor = nums.next()?.parse().ok()?;
    Some((major, minor))
}

#[must_use]
pub fn supports_worktree_orphan() -> bool {
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(|| {
        if std::env::var_os(FORCE_FALLBACK_ENV).is_some() {
            return false;
        }
        let Ok(output) = Command::new("git").arg("--version").output() else {
            return true;
        };
        let stdout = String::from_utf8_lossy(&output.stdout);
        parse_git_version(&stdout).is_none_or(|v| v >= MIN_ORPHAN_WORKTREE)
    })
}

pub fn add_orphan_worktree(repo_root: &Path, branch: &str, worktree_path: &str) -> Result<()> {
    add_orphan_worktree_impl(repo_root, branch, worktree_path, supports_worktree_orphan())
}

fn add_orphan_worktree_impl(
    repo_root: &Path,
    branch: &str,
    worktree_path: &str,
    use_orphan_flag: bool,
) -> Result<()> {
    if use_orphan_flag {
        run_git(
            repo_root,
            &["worktree", "add", "--orphan", "-b", branch, worktree_path],
        )?;
        return Ok(());
    }

    run_git(repo_root, &["worktree", "add", "--detach", worktree_path])?;
    let wt = Path::new(worktree_path);
    run_git(wt, &["checkout", "--orphan", branch])?;

    let out = Command::new("git")
        .current_dir(wt)
        .args(["rm", "-rf", "."])
        .output()
        .with_context(|| format!("Failed to run git rm -rf . in {worktree_path}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        if !stderr.contains("did not match any files") {
            bail!("git rm -rf . failed in {worktree_path}: {stderr}");
        }
    }
    Ok(())
}

fn run_git(dir: &Path, args: &[&str]) -> Result<()> {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .with_context(|| format!("Failed to run git {args:?}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git {args:?} failed: {stderr}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_git_version_handles_common_formats() {
        assert_eq!(parse_git_version("git version 2.34.1"), Some((2, 34)));
        assert_eq!(parse_git_version("git version 2.42.0"), Some((2, 42)));
        assert_eq!(
            parse_git_version("git version 2.39.3 (Apple Git-145)"),
            Some((2, 39))
        );
        assert_eq!(parse_git_version("git version 2.54.0\n"), Some((2, 54)));
        assert_eq!(parse_git_version("not git output"), None);
    }

    #[test]
    fn version_gate_matches_2_42_boundary() {
        assert!((2, 42) >= MIN_ORPHAN_WORKTREE);
        assert!((2, 54) >= MIN_ORPHAN_WORKTREE);
        assert!((3, 0) >= MIN_ORPHAN_WORKTREE);
        assert!((2, 41) < MIN_ORPHAN_WORKTREE);
        assert!((2, 34) < MIN_ORPHAN_WORKTREE);
    }

    #[test]
    fn fallback_creates_empty_unborn_orphan_worktree() {
        let repo = tempfile::tempdir().unwrap();
        let rp = repo.path();
        for args in [
            vec!["init", "-b", "main"],
            vec!["config", "user.email", "t@t.local"],
            vec!["config", "user.name", "T"],
            vec!["config", "commit.gpgsign", "false"],
        ] {
            run_git(rp, &args).unwrap();
        }
        std::fs::write(rp.join("README.md"), "# main\n").unwrap();
        run_git(rp, &["add", "."]).unwrap();
        run_git(rp, &["commit", "-m", "init", "--no-gpg-sign"]).unwrap();

        let wt_path = rp.join(".crosslink").join(".hub-cache");
        let wt_str = wt_path.to_string_lossy().to_string();

        add_orphan_worktree_impl(rp, "crosslink/hub-v3-host", &wt_str, false).unwrap();

        let head = Command::new("git")
            .current_dir(&wt_path)
            .args(["symbolic-ref", "HEAD"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&head.stdout).trim(),
            "refs/heads/crosslink/hub-v3-host"
        );
        assert!(
            !Command::new("git")
                .current_dir(&wt_path)
                .args(["rev-parse", "--verify", "HEAD"])
                .output()
                .unwrap()
                .status
                .success(),
            "HEAD must be unborn (no commit yet), like worktree add --orphan"
        );

        let staged = Command::new("git")
            .current_dir(&wt_path)
            .args(["ls-files"])
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&staged.stdout).trim().is_empty(),
            "orphan worktree index must be empty"
        );
        assert!(
            !wt_path.join("README.md").exists(),
            "main's files must not be present in the orphan worktree"
        );

        run_git(&wt_path, &["commit", "--allow-empty", "-m", "genesis"]).unwrap();
        let parents = Command::new("git")
            .current_dir(&wt_path)
            .args(["rev-list", "--count", "HEAD"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&parents.stdout).trim(),
            "1",
            "the genesis commit must be a root commit"
        );
    }
}
