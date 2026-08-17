use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::Result;

use super::db::DashboardDb;

pub const DEFAULT_DEPTH: usize = 4;

const NOISE_DIRS: &[&str] = &[
    "node_modules",
    "target",
    "build",
    "dist",
    "out",
    ".next",
    ".nuxt",
    ".svelte-kit",
    ".venv",
    "venv",
    "env",
    "__pycache__",
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
    ".tox",
    ".cache",
    ".cargo",
    ".rustup",
    ".npm",
    ".yarn",
    ".pnpm-store",
    "Library",
    "AppData",
    ".Trash",
    ".trash",
    ".local",
    "Downloads",
    "Desktop",
    "Movies",
    "Music",
    "Pictures",
    "Videos",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredRepo {
    pub path: PathBuf,

    pub slug: String,

    pub already_tracked: bool,
}

#[derive(Debug, Clone)]
pub struct DiscoverOptions {
    pub roots: Vec<PathBuf>,
    pub depth: usize,
}

impl DiscoverOptions {
    #[must_use]
    pub fn defaults() -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        Self {
            roots: vec![home],
            depth: DEFAULT_DEPTH,
        }
    }
}

pub fn discover(db: &DashboardDb, opts: &DiscoverOptions) -> Result<Vec<DiscoveredRepo>> {
    let tracked = load_tracked_slugs(db)?;
    let mut hits: BTreeMap<PathBuf, String> = BTreeMap::new();
    let mut visited: HashSet<PathBuf> = HashSet::new();

    for root in &opts.roots {
        walk(root, 0, opts.depth, &mut hits, &mut visited);
    }

    let mut out: Vec<DiscoveredRepo> = hits
        .into_iter()
        .map(|(path, slug)| {
            let already_tracked = tracked.contains(&slug);
            DiscoveredRepo {
                path,
                slug,
                already_tracked,
            }
        })
        .collect();
    out.sort_by(|a, b| a.slug.cmp(&b.slug));
    Ok(out)
}

fn load_tracked_slugs(db: &DashboardDb) -> Result<HashSet<String>> {
    let mut stmt = db.conn.prepare("SELECT slug FROM projects")?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    let mut out = HashSet::new();
    for r in rows {
        out.insert(r?);
    }
    Ok(out)
}

fn walk(
    dir: &Path,
    depth: usize,
    max_depth: usize,
    hits: &mut BTreeMap<PathBuf, String>,
    visited: &mut HashSet<PathBuf>,
) {
    let Ok(canon) = dir.canonicalize() else {
        return;
    };
    if !visited.insert(canon.clone()) {
        return;
    }
    if !canon.is_dir() {
        return;
    }

    let has_git = canon.join(".git").exists();
    let has_crosslink = canon.join(".crosslink").is_dir();
    if has_git && has_crosslink {
        if let Some(slug) = derive_slug(&canon) {
            hits.entry(canon.clone()).or_insert(slug);
        }

        return;
    }

    if depth >= max_depth {
        return;
    }

    let Ok(entries) = std::fs::read_dir(&canon) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if should_skip(name_str.as_ref()) {
            continue;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };

        if file_type.is_symlink() {
            continue;
        }
        if !file_type.is_dir() {
            continue;
        }
        walk(&entry.path(), depth + 1, max_depth, hits, visited);
    }
}

fn should_skip(name: &str) -> bool {
    if NOISE_DIRS.contains(&name) {
        return true;
    }

    if name.starts_with('.') {
        return true;
    }
    false
}

fn derive_slug(repo_path: &Path) -> Option<String> {
    if let Some(slug) = origin_slug(repo_path) {
        return Some(slug);
    }
    let basename = repo_path.file_name()?.to_string_lossy().into_owned();
    Some(format!("local/{basename}"))
}

fn origin_slug(repo_path: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["remote", "get-url", "origin"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
    super::projects::slug_from_remote_url(&url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn mk_repo(path: &Path, origin: Option<&str>, crosslinked: bool) {
        fs::create_dir_all(path).unwrap();

        fs::write(path.join(".git"), "gitdir: /fake").unwrap();
        if crosslinked {
            fs::create_dir_all(path.join(".crosslink")).unwrap();
            fs::write(path.join(".crosslink").join("issues.db"), b"").unwrap();
        }
        if let Some(url) = origin {
            fs::write(path.join(".origin"), url).unwrap();
        }
    }

    #[test]
    fn test_discover_finds_crosslinked_repos_only() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();

        mk_repo(&root.join("code/crosslink/alpha"), None, true);
        mk_repo(&root.join("code/crosslink/beta"), None, true);
        mk_repo(&root.join("code/crosslink/gamma"), None, false);
        mk_repo(&root.join("code/other/delta"), None, true);
        mk_repo(&root.join("code/vendored/node_modules/package"), None, true);
        mk_repo(&root.join(".dotdir/hidden"), None, true);

        let db_dir = tempdir().unwrap();
        let db = DashboardDb::open(&db_dir.path().join("d.db")).unwrap();
        let opts = DiscoverOptions {
            roots: vec![root.to_path_buf()],
            depth: 6,
        };
        let hits = discover(&db, &opts).unwrap();

        let slugs: Vec<_> = hits.iter().map(|h| h.slug.as_str()).collect();
        assert_eq!(slugs.len(), 3, "unexpected hits: {hits:?}");
        assert!(slugs.contains(&"local/alpha"));
        assert!(slugs.contains(&"local/beta"));
        assert!(slugs.contains(&"local/delta"));
        for h in &hits {
            assert!(!h.already_tracked, "empty DB — nothing tracked yet");
        }
    }

    #[test]
    fn test_discover_respects_depth() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();

        mk_repo(&root.join("repo1"), None, true);

        mk_repo(&root.join("a/b/c/repo2"), None, true);

        let db_dir = tempdir().unwrap();
        let db = DashboardDb::open(&db_dir.path().join("d.db")).unwrap();

        let shallow = discover(
            &db,
            &DiscoverOptions {
                roots: vec![root.to_path_buf()],
                depth: 2,
            },
        )
        .unwrap();
        let deep = discover(
            &db,
            &DiscoverOptions {
                roots: vec![root.to_path_buf()],
                depth: 5,
            },
        )
        .unwrap();

        let shallow_slugs: Vec<_> = shallow.iter().map(|h| h.slug.as_str()).collect();
        let deep_slugs: Vec<_> = deep.iter().map(|h| h.slug.as_str()).collect();
        assert!(shallow_slugs.contains(&"local/repo1"));
        assert!(!shallow_slugs.contains(&"local/repo2"));
        assert!(deep_slugs.contains(&"local/repo1"));
        assert!(deep_slugs.contains(&"local/repo2"));
    }

    #[test]
    fn test_discover_flags_already_tracked() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        mk_repo(&root.join("repo1"), None, true);
        mk_repo(&root.join("repo2"), None, true);

        let db_dir = tempdir().unwrap();
        let db = DashboardDb::open(&db_dir.path().join("d.db")).unwrap();
        db.conn
            .execute(
                "INSERT INTO projects (slug, clone_path, default_branch, status, added_at)
                 VALUES ('local/repo1', '/tmp/x', 'main', 'active', '2026-04-21T00:00:00Z')",
                [],
            )
            .unwrap();

        let hits = discover(
            &db,
            &DiscoverOptions {
                roots: vec![root.to_path_buf()],
                depth: 3,
            },
        )
        .unwrap();

        let r1 = hits.iter().find(|h| h.slug == "local/repo1").unwrap();
        let r2 = hits.iter().find(|h| h.slug == "local/repo2").unwrap();
        assert!(r1.already_tracked);
        assert!(!r2.already_tracked);
    }

    #[test]
    fn test_discover_does_not_descend_into_a_hit() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();

        mk_repo(&root.join("outer"), None, true);
        mk_repo(&root.join("outer/nested-repo"), None, true);

        let db_dir = tempdir().unwrap();
        let db = DashboardDb::open(&db_dir.path().join("d.db")).unwrap();
        let hits = discover(
            &db,
            &DiscoverOptions {
                roots: vec![root.to_path_buf()],
                depth: 5,
            },
        )
        .unwrap();

        let slugs: Vec<_> = hits.iter().map(|h| h.slug.as_str()).collect();
        assert_eq!(slugs, vec!["local/outer"]);
    }

    #[test]
    fn test_should_skip_catches_noise_and_hidden() {
        assert!(should_skip("node_modules"));
        assert!(should_skip("target"));
        assert!(should_skip(".venv"));
        assert!(should_skip(".local"));
        assert!(should_skip(".hidden-whatever"));
        assert!(!should_skip("code"));
        assert!(!should_skip("work"));
    }
}
