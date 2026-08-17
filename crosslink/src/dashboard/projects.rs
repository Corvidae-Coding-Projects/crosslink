use anyhow::{bail, Context, Result};
use rusqlite::params;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::db::DashboardDb;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    pub id: i64,
    pub slug: String,

    pub clone_path: PathBuf,
    pub default_branch: String,
    pub hub_sha: Option<String>,
    pub hub_fetched_at: Option<String>,
    pub status: String,
    pub added_at: String,
    pub last_activity_at: Option<String>,
    pub pinned: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteCapability {
    Ready,

    AgentMissing,

    NotInitialized,
}

impl WriteCapability {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::AgentMissing => "agent_missing",
            Self::NotInitialized => "not_initialized",
        }
    }
}

#[must_use]
pub fn write_capability(clone_path: &Path) -> WriteCapability {
    let cl = clone_path.join(".crosslink");
    if !cl.join("issues.db").is_file() {
        return WriteCapability::NotInitialized;
    }
    let has_driver_key = cl.join("driver-key.pub").is_file();
    let has_agent_json = cl.join("agent.json").is_file();
    if !has_driver_key || !has_agent_json {
        return WriteCapability::AgentMissing;
    }
    WriteCapability::Ready
}

fn parse_slug(slug: &str) -> Result<(&str, &str)> {
    let mut parts = slug.splitn(2, '/');
    let owner = parts.next().unwrap_or_default();
    let repo = parts.next().unwrap_or_default();
    if owner.is_empty() || repo.is_empty() || parts.next().is_some() {
        bail!("slug must be in the form `owner/repo`, got: {slug}");
    }
    if owner.contains(std::path::is_separator)
        || repo.contains(std::path::is_separator)
        || owner.contains('\\')
        || repo.contains('\\')
    {
        bail!("slug must not contain path separators: {slug}");
    }
    Ok((owner, repo))
}

pub(super) fn slug_from_remote_url(url: &str) -> Option<String> {
    let url = url.trim().trim_end_matches(".git");

    if let Some((host_part, path_part)) = url.split_once(':') {
        if !path_part.is_empty() && !path_part.starts_with("//") {
            let looks_like_scheme =
                matches!(host_part, "http" | "https" | "ssh" | "git" | "ftp" | "file");
            if !looks_like_scheme {
                return extract_owner_repo(path_part);
            }
        }
    }

    if let Some(path_part) = url
        .split_once("://")
        .and_then(|(_s, rest)| rest.split_once('/').map(|(_host, path)| path))
    {
        return extract_owner_repo(path_part);
    }
    None
}

fn extract_owner_repo(path_part: &str) -> Option<String> {
    let parts: Vec<&str> = path_part
        .trim_start_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();

    if parts.len() < 2 {
        return None;
    }
    let owner = parts[parts.len() - 2];
    let repo = parts[parts.len() - 1];
    Some(format!("{owner}/{repo}"))
}

fn detect_default_branch(clone_path: &Path) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(clone_path)
        .args(["symbolic-ref", "--short", "refs/remotes/origin/HEAD"])
        .output()
        .ok()?;
    if out.status.success() {
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();

        if let Some(branch) = s.strip_prefix("origin/") {
            if !branch.is_empty() {
                return Some(branch.to_string());
            }
        }
    }

    let out = Command::new("git")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "")
        .arg("-C")
        .arg(clone_path)
        .args(["-c", "credential.helper=", "remote", "show", "origin"])
        .output()
        .ok()?;
    if out.status.success() {
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let t = line.trim();
            if let Some(branch) = t.strip_prefix("HEAD branch:") {
                let branch = branch.trim();
                if !branch.is_empty() && branch != "(unknown)" {
                    return Some(branch.to_string());
                }
            }
        }
    }

    for candidate in ["main", "master"] {
        let ok = Command::new("git")
            .arg("-C")
            .arg(clone_path)
            .args(["rev-parse", "--verify", &format!("refs/heads/{candidate}")])
            .output()
            .is_ok_and(|o| o.status.success());
        if ok {
            return Some(candidate.to_string());
        }
    }

    None
}

fn origin_url(clone_path: &Path) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(clone_path)
        .args(["remote", "get-url", "origin"])
        .output()
        .context("Failed to invoke git")?;
    if !out.status.success() {
        bail!(
            "`git remote get-url origin` failed for {}: {}",
            clone_path.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub fn track(clone_path: &Path, slug_override: Option<&str>) -> Result<()> {
    let db_path = DashboardDb::default_path()?;
    track_at_path(clone_path, slug_override, &db_path)
}

pub fn track_with_init(
    clone_path: &Path,
    slug_override: Option<&str>,
    agent_id: &str,
) -> Result<()> {
    let db_path = DashboardDb::default_path()?;
    if write_capability(clone_path) != WriteCapability::Ready {
        run_init_and_agent_in(clone_path, agent_id)?;
    }
    track_at_path(clone_path, slug_override, &db_path)
}

pub fn run_init_and_agent_in(workspace: &Path, agent_id: &str) -> Result<()> {
    let cmd_name = resolve_crosslink_bin();

    run_init_and_agent_inner(&cmd_name, workspace, agent_id)?;
    let issues_db = workspace.join(".crosslink").join("issues.db");
    if issues_db.is_file() {
        return Ok(());
    }

    tracing::warn!(
        "crosslink init in {} produced no issues.db; cleaning partial \
         state and retrying",
        workspace.display()
    );
    wipe_partial_crosslink_state(workspace)?;
    run_init_and_agent_inner(&cmd_name, workspace, agent_id)?;
    if !issues_db.is_file() {
        bail!(
            "crosslink init in {} reported success but `.crosslink/issues.db` \
             is still missing after a clean retry — the workspace may be on a \
             filesystem that doesn't support SQLite, or the CLI build is \
             broken. Investigate `crosslink init --defaults` manually there.",
            workspace.display()
        );
    }
    Ok(())
}

fn wipe_partial_crosslink_state(workspace: &Path) -> Result<()> {
    let dot_crosslink = workspace.join(".crosslink");
    if !dot_crosslink.is_dir() {
        return Ok(());
    }

    let hub_cache = dot_crosslink.join(".hub-cache");
    if hub_cache.is_dir() {
        let _ = Command::new("git")
            .arg("-C")
            .arg(workspace)
            .args([
                "worktree",
                "remove",
                "--force",
                hub_cache.to_string_lossy().as_ref(),
            ])
            .output();

        let _ = Command::new("git")
            .arg("-C")
            .arg(workspace)
            .args(["worktree", "prune"])
            .output();
    }

    std::fs::remove_dir_all(&dot_crosslink)
        .with_context(|| format!("remove_dir_all {}", dot_crosslink.display()))?;
    Ok(())
}

fn run_init_and_agent_inner(
    cmd_name: &std::ffi::OsStr,
    workspace: &Path,
    agent_id: &str,
) -> Result<()> {
    let init_out = Command::new(cmd_name)
        .current_dir(workspace)
        .args(["init", "--defaults", "-q", "--force"])
        .output()
        .with_context(|| {
            format!(
                "spawn `crosslink init` (binary: {}, workspace: {})",
                std::path::Path::new(cmd_name).display(),
                workspace.display()
            )
        })?;
    if !init_out.status.success() {
        let stderr = String::from_utf8_lossy(&init_out.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&init_out.stdout).trim().to_string();
        bail!(
            "crosslink init failed (exit {}): {}{}{}",
            init_out
                .status
                .code()
                .map_or_else(|| "signal".into(), |c| c.to_string()),
            stderr,
            if !stderr.is_empty() && !stdout.is_empty() {
                "; stdout: "
            } else {
                ""
            },
            stdout,
        );
    }

    let agent_out = Command::new(cmd_name)
        .current_dir(workspace)
        .args([
            "agent",
            "init",
            agent_id,
            "-q",
            "--force",
            "--role",
            "driver",
            "--description",
            "dashboard auto-bootstrap",
        ])
        .output()
        .with_context(|| {
            format!(
                "spawn `crosslink agent init` (binary: {}, workspace: {})",
                std::path::Path::new(cmd_name).display(),
                workspace.display()
            )
        })?;
    if !agent_out.status.success() {
        let stderr = String::from_utf8_lossy(&agent_out.stderr)
            .trim()
            .to_string();
        let stdout = String::from_utf8_lossy(&agent_out.stdout)
            .trim()
            .to_string();
        bail!(
            "crosslink agent init failed (exit {}): {}{}{}",
            agent_out
                .status
                .code()
                .map_or_else(|| "signal".into(), |c| c.to_string()),
            stderr,
            if !stderr.is_empty() && !stdout.is_empty() {
                "; stdout: "
            } else {
                ""
            },
            stdout,
        );
    }
    Ok(())
}

pub fn track_at_path(clone_path: &Path, slug_override: Option<&str>, db_path: &Path) -> Result<()> {
    if !clone_path.is_dir() {
        bail!(
            "path does not exist or is not a directory: {}",
            clone_path.display()
        );
    }
    if !clone_path.join(".git").exists() {
        bail!(
            "path is not a git repository (no .git found): {}",
            clone_path.display()
        );
    }

    let slug = if let Some(s) = slug_override {
        parse_slug(s)?;
        s.to_string()
    } else {
        let url = origin_url(clone_path)?;
        slug_from_remote_url(&url).ok_or_else(|| {
            anyhow::anyhow!(
                "could not derive slug from origin URL `{url}` — pass --slug owner/repo"
            )
        })?
    };
    parse_slug(&slug)?;

    let has_hub = !matches!(
        crate::hub_v3::detect_hub_version(clone_path),
        Ok(crate::hub_v3::HubVersion::Absent)
    ) || ["origin/crosslink/checkpoint", "origin/crosslink/hub"]
        .iter()
        .any(|r| {
            Command::new("git")
                .arg("-C")
                .arg(clone_path)
                .args(["rev-parse", "--verify", "--quiet", r])
                .output()
                .is_ok_and(|o| o.status.success())
        });
    if !has_hub {
        eprintln!(
            "warning: {slug} has no crosslink hub state yet (neither a v2 \
             `crosslink/hub` branch nor v3 hub refs) — tracking anyway, \
             dashboard will surface this as unreachable."
        );
    }

    let db = DashboardDb::open(db_path)?;
    let existing: Option<i64> = db
        .conn
        .query_row("SELECT id FROM projects WHERE slug = ?1", [&slug], |row| {
            row.get(0)
        })
        .ok();
    if existing.is_some() {
        bail!("{slug} is already tracked");
    }

    let canonical = clone_path
        .canonicalize()
        .unwrap_or_else(|_| clone_path.to_path_buf());
    let default_branch = detect_default_branch(&canonical).unwrap_or_else(|| "main".into());
    let now = chrono::Utc::now().to_rfc3339();
    db.conn.execute(
        "INSERT INTO projects (slug, clone_path, default_branch, status, added_at)
         VALUES (?1, ?2, ?3, 'active', ?4)",
        params![
            slug,
            canonical.to_string_lossy().as_ref(),
            default_branch,
            now
        ],
    )?;

    println!(
        "Tracking {slug} at {}{}",
        canonical.display(),
        if has_hub {
            ""
        } else {
            " (crosslink/hub missing)"
        }
    );
    Ok(())
}

pub fn untrack(slug: &str) -> Result<()> {
    let db_path = DashboardDb::default_path()?;
    untrack_with_path(slug, &db_path)
}

pub fn untrack_with_path(slug: &str, db_path: &Path) -> Result<()> {
    parse_slug(slug)?;

    let db = DashboardDb::open(db_path)?;

    let tx = db.conn.unchecked_transaction()?;
    tx.execute(
        "UPDATE actions SET project_id = NULL
         WHERE project_id = (SELECT id FROM projects WHERE slug = ?1)",
        [slug],
    )?;
    let rows = tx.execute("DELETE FROM projects WHERE slug = ?1", [slug])?;
    if rows == 0 {
        bail!("{slug} is not currently tracked");
    }
    tx.commit()?;
    println!("Untracked {slug} (local working copy left intact)");
    Ok(())
}

pub fn list() -> Result<()> {
    let db_path = DashboardDb::default_path()?;
    list_with_path(&db_path)
}

pub fn list_with_path(db_path: &Path) -> Result<()> {
    let db = DashboardDb::open(db_path)?;

    let mut stmt = db.conn.prepare(
        "SELECT id, slug, clone_path, default_branch, hub_sha, hub_fetched_at,
                status, added_at, last_activity_at, pinned
         FROM projects
         ORDER BY pinned DESC, slug ASC",
    )?;
    let projects: Vec<Project> = stmt
        .query_map([], |row| {
            Ok(Project {
                id: row.get(0)?,
                slug: row.get(1)?,
                clone_path: PathBuf::from(row.get::<_, String>(2)?),
                default_branch: row.get(3)?,
                hub_sha: row.get(4)?,
                hub_fetched_at: row.get(5)?,
                status: row.get(6)?,
                added_at: row.get(7)?,
                last_activity_at: row.get(8)?,
                pinned: row.get::<_, i64>(9)? != 0,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    if projects.is_empty() {
        println!(
            "No tracked projects. Add one with \
             `crosslink dashboard track <path-to-repo>`."
        );
        return Ok(());
    }

    println!(
        "{:<5} {:<40} {:<10} {:<25} Working copy",
        "PIN", "SLUG", "STATUS", "LAST FETCH"
    );
    for p in &projects {
        let pin = if p.pinned { "●" } else { " " };
        let last_fetch = p.hub_fetched_at.as_deref().unwrap_or("—");
        println!(
            "{pin:<5} {:<40} {:<10} {:<25} {}",
            p.slug,
            p.status,
            last_fetch,
            p.clone_path.display()
        );
    }
    Ok(())
}

pub fn resolve_crosslink_bin() -> std::ffi::OsString {
    if let Some(override_path) = std::env::var_os("CROSSLINK_BIN") {
        if !override_path.is_empty() {
            return override_path;
        }
    }
    if which_on_path("crosslink").is_some() {
        return "crosslink".into();
    }
    if let Ok(exe) = std::env::current_exe() {
        let looks_like_test = exe
            .components()
            .any(|c| c.as_os_str() == std::ffi::OsStr::new("deps"));
        if !looks_like_test && exe.is_file() {
            return exe.into_os_string();
        }
    }
    "crosslink".into()
}

fn which_on_path(name: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| {
        let candidate = dir.join(name);
        if candidate.is_file() {
            Some(candidate)
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as StdCommand;
    use tempfile::tempdir;

    fn temp_db() -> (tempfile::TempDir, PathBuf) {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("dashboard.db");
        DashboardDb::open(&db_path).unwrap();
        (dir, db_path)
    }

    #[test]
    fn test_write_capability_not_initialized_when_crosslink_dir_missing() {
        let dir = tempdir().unwrap();
        assert_eq!(
            write_capability(dir.path()),
            WriteCapability::NotInitialized
        );
    }

    #[test]
    fn test_write_capability_not_initialized_when_issues_db_missing() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".crosslink")).unwrap();

        assert_eq!(
            write_capability(dir.path()),
            WriteCapability::NotInitialized
        );
    }

    #[test]
    fn test_write_capability_agent_missing_when_key_absent() {
        let dir = tempdir().unwrap();
        let cl = dir.path().join(".crosslink");
        std::fs::create_dir_all(&cl).unwrap();
        std::fs::write(cl.join("issues.db"), []).unwrap();

        assert_eq!(write_capability(dir.path()), WriteCapability::AgentMissing);
    }

    #[test]
    fn test_write_capability_agent_missing_when_agent_json_absent() {
        let dir = tempdir().unwrap();
        let cl = dir.path().join(".crosslink");
        std::fs::create_dir_all(&cl).unwrap();
        std::fs::write(cl.join("issues.db"), []).unwrap();
        std::fs::write(cl.join("driver-key.pub"), b"ssh-ed25519 AAAA...").unwrap();
        assert_eq!(write_capability(dir.path()), WriteCapability::AgentMissing);
    }

    #[test]
    fn test_write_capability_ready_when_all_present() {
        let dir = tempdir().unwrap();
        let cl = dir.path().join(".crosslink");
        std::fs::create_dir_all(&cl).unwrap();
        std::fs::write(cl.join("issues.db"), []).unwrap();
        std::fs::write(cl.join("driver-key.pub"), b"ssh-ed25519 AAAA...").unwrap();
        std::fs::write(cl.join("agent.json"), b"{\"agent_id\":\"x\"}").unwrap();
        assert_eq!(write_capability(dir.path()), WriteCapability::Ready);
    }

    #[test]
    fn test_write_capability_as_str_is_stable() {
        assert_eq!(WriteCapability::Ready.as_str(), "ready");
        assert_eq!(WriteCapability::AgentMissing.as_str(), "agent_missing");
        assert_eq!(WriteCapability::NotInitialized.as_str(), "not_initialized");
    }

    fn make_fake_repo(path: &Path, origin: &str, with_hub: bool) {
        StdCommand::new("git")
            .arg("-C")
            .arg(path)
            .args(["init", "-q", "-b", "main"])
            .status()
            .unwrap();
        StdCommand::new("git")
            .arg("-C")
            .arg(path)
            .args(["config", "user.email", "test@test.local"])
            .status()
            .unwrap();
        StdCommand::new("git")
            .arg("-C")
            .arg(path)
            .args(["config", "user.name", "Test"])
            .status()
            .unwrap();
        StdCommand::new("git")
            .arg("-C")
            .arg(path)
            .args(["commit", "--allow-empty", "-q", "-m", "init"])
            .status()
            .unwrap();
        StdCommand::new("git")
            .arg("-C")
            .arg(path)
            .args(["remote", "add", "origin", origin])
            .status()
            .unwrap();

        StdCommand::new("git")
            .arg("-C")
            .arg(path)
            .args(["update-ref", "refs/remotes/origin/main", "HEAD"])
            .status()
            .unwrap();
        StdCommand::new("git")
            .arg("-C")
            .arg(path)
            .args([
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/main",
            ])
            .status()
            .unwrap();
        if with_hub {
            StdCommand::new("git")
                .arg("-C")
                .arg(path)
                .args(["checkout", "-q", "--orphan", "crosslink/hub"])
                .status()
                .unwrap();
            StdCommand::new("git")
                .arg("-C")
                .arg(path)
                .args(["commit", "--allow-empty", "-q", "-m", "hub init"])
                .status()
                .unwrap();
            StdCommand::new("git")
                .arg("-C")
                .arg(path)
                .args(["checkout", "-q", "main"])
                .status()
                .unwrap();
        }
    }

    #[test]
    fn test_parse_slug_valid() {
        assert_eq!(
            parse_slug("Corvidae-Coding-Projects/crosslink").unwrap(),
            ("Corvidae-Coding-Projects", "crosslink")
        );
    }

    #[test]
    fn test_parse_slug_rejects_single_segment() {
        assert!(parse_slug("crosslink").is_err());
    }

    #[test]
    fn test_parse_slug_rejects_empty_owner() {
        assert!(parse_slug("/crosslink").is_err());
    }

    #[test]
    fn test_parse_slug_rejects_path_traversal() {
        assert!(parse_slug("../etc/passwd").is_err());
        assert!(parse_slug("foo\\bar").is_err());
    }

    #[test]
    fn test_slug_from_ssh_url() {
        assert_eq!(
            slug_from_remote_url("git@github.com:Corvidae-Coding-Projects/sigil.git"),
            Some("Corvidae-Coding-Projects/sigil".to_string())
        );
    }

    #[test]
    fn test_slug_from_https_url_with_git_suffix() {
        assert_eq!(
            slug_from_remote_url("https://github.com/Corvidae-Coding-Projects/sigil.git"),
            Some("Corvidae-Coding-Projects/sigil".to_string())
        );
    }

    #[test]
    fn test_slug_from_https_url_without_git_suffix() {
        assert_eq!(
            slug_from_remote_url("https://github.com/Corvidae-Coding-Projects/sigil"),
            Some("Corvidae-Coding-Projects/sigil".to_string())
        );
    }

    #[test]
    fn test_slug_from_nested_gitlab_path_takes_last_two() {
        assert_eq!(
            slug_from_remote_url("https://gitlab.example.com/group/subgroup/project"),
            Some("subgroup/project".to_string())
        );
    }

    #[test]
    fn test_slug_from_garbage_returns_none() {
        assert_eq!(slug_from_remote_url("not a url"), None);
        assert_eq!(slug_from_remote_url(""), None);
    }

    #[test]
    fn test_track_rejects_nonexistent_path() {
        let (_home, db_path) = temp_db();
        let err =
            track_at_path(Path::new("/definitely/not/a/real/path"), None, &db_path).unwrap_err();
        assert!(err.to_string().contains("does not exist"));
    }

    #[test]
    fn test_track_rejects_non_git_directory() {
        let (_home, db_path) = temp_db();
        let dir = tempdir().unwrap();
        let err = track_at_path(dir.path(), None, &db_path).unwrap_err();
        assert!(err.to_string().contains("not a git repository"));
    }

    #[test]
    fn test_track_inserts_row_with_derived_slug() {
        let (_home, db_path) = temp_db();
        let repo = tempdir().unwrap();
        make_fake_repo(
            repo.path(),
            "https://github.com/Corvidae-Coding-Projects/test-a.git",
            true,
        );

        track_at_path(repo.path(), None, &db_path).unwrap();

        let db = DashboardDb::open(&db_path).unwrap();
        let slug: String = db
            .conn
            .query_row("SELECT slug FROM projects WHERE id = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(slug, "Corvidae-Coding-Projects/test-a");
    }

    #[test]
    fn test_track_with_slug_override_wins_over_origin() {
        let (_home, db_path) = temp_db();
        let repo = tempdir().unwrap();
        make_fake_repo(
            repo.path(),
            "https://github.com/Corvidae-Coding-Projects/test-b.git",
            true,
        );

        track_at_path(repo.path(), Some("custom/override"), &db_path).unwrap();

        let db = DashboardDb::open(&db_path).unwrap();
        let slug: String = db
            .conn
            .query_row("SELECT slug FROM projects WHERE id = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(slug, "custom/override");
    }

    #[test]
    fn test_track_rejects_duplicate_slug() {
        let (_home, db_path) = temp_db();
        let repo = tempdir().unwrap();
        make_fake_repo(
            repo.path(),
            "https://github.com/Corvidae-Coding-Projects/test-c.git",
            true,
        );

        track_at_path(repo.path(), None, &db_path).unwrap();
        let err = track_at_path(repo.path(), None, &db_path).unwrap_err();
        assert!(err.to_string().contains("already tracked"));
    }

    #[test]
    fn test_track_repo_without_hub_branch_still_succeeds() {
        let (_home, db_path) = temp_db();
        let repo = tempdir().unwrap();
        make_fake_repo(
            repo.path(),
            "https://github.com/Corvidae-Coding-Projects/test-d.git",
            false,
        );

        track_at_path(repo.path(), None, &db_path).unwrap();

        let db = DashboardDb::open(&db_path).unwrap();
        let count: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM projects", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_track_v3_repo_recognized_as_hubbed() {
        let (_home, db_path) = temp_db();
        let repo = tempdir().unwrap();
        make_fake_repo(
            repo.path(),
            "https://github.com/Corvidae-Coding-Projects/test-e.git",
            false,
        );

        for branch in ["crosslink/checkpoint", "crosslink/meta"] {
            let out = Command::new("git")
                .arg("-C")
                .arg(repo.path())
                .args(["branch", branch])
                .output()
                .unwrap();
            assert!(out.status.success());
        }

        track_at_path(repo.path(), None, &db_path).unwrap();

        let db = DashboardDb::open(&db_path).unwrap();
        let count: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM projects", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_untrack_removes_row_and_leaves_working_copy() {
        let (_home, db_path) = temp_db();
        let repo = tempdir().unwrap();
        make_fake_repo(
            repo.path(),
            "https://github.com/Corvidae-Coding-Projects/test-e.git",
            true,
        );

        track_at_path(repo.path(), None, &db_path).unwrap();
        untrack_with_path("Corvidae-Coding-Projects/test-e", &db_path).unwrap();

        let db = DashboardDb::open(&db_path).unwrap();
        let count: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM projects", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);

        assert!(repo.path().join(".git").exists());
    }

    #[test]
    fn test_untrack_rejects_unknown_slug() {
        let (_home, db_path) = temp_db();
        let err = untrack_with_path("owner/never-tracked", &db_path).unwrap_err();
        assert!(err.to_string().contains("not currently tracked"));
    }

    #[test]
    fn test_list_on_empty_db_prints_help() {
        let (_home, db_path) = temp_db();

        list_with_path(&db_path).unwrap();
    }

    #[test]
    fn detect_default_branch_reads_local_remote_head() {
        let dir = tempdir().unwrap();
        make_fake_repo(
            dir.path(),
            "https://github.com/Corvidae-Coding-Projects/test-x.git",
            false,
        );
        assert_eq!(detect_default_branch(dir.path()).as_deref(), Some("main"));
    }

    #[test]
    fn detect_default_branch_bogus_origin_falls_through_without_hanging() {
        let dir = tempdir().unwrap();
        let p = dir.path();
        for args in [
            vec!["init", "-q", "-b", "main"],
            vec!["config", "user.email", "test@test.local"],
            vec!["config", "user.name", "Test"],
            vec!["commit", "--allow-empty", "-q", "-m", "init"],
            vec![
                "remote",
                "add",
                "origin",
                "file:///nonexistent/gh34/repo.git",
            ],
        ] {
            StdCommand::new("git")
                .arg("-C")
                .arg(p)
                .args(&args)
                .status()
                .unwrap();
        }

        assert_eq!(detect_default_branch(p).as_deref(), Some("main"));
    }
}
