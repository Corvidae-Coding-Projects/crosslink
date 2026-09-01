use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Partition {
    pub label: String,
    pub files: Vec<PathBuf>,
    pub line_count: usize,
}

const SOURCE_EXTENSIONS: &[&str] = &[
    "rs", "py", "ts", "tsx", "js", "jsx", "go", "java", "c", "cpp", "h", "hpp", "cc", "cxx", "cs",
    "rb", "swift", "kt", "scala", "zig", "hs", "ml", "ex", "exs", "erl", "clj", "lua", "sh",
    "bash", "zsh", "vue", "svelte",
];

const IGNORED_DIRS: &[&str] = &[
    "target",
    "node_modules",
    ".git",
    "vendor",
    "dist",
    "build",
    ".next",
    "__pycache__",
    ".mypy_cache",
    ".pytest_cache",
    ".tox",
    "venv",
    ".venv",
    "env",
    ".crosslink",
    ".claude",
];

const MAX_PARTITION_LINES: usize = 2_000;

const MIN_PARTITION_LINES: usize = 200;

const GIT_LOG_DEPTH: usize = 200;

const COUPLING_THRESHOLD: usize = 3;

pub fn detect_seams(repo_root: &Path, max_partitions: usize) -> Result<Vec<Partition>> {
    let max_partitions = max_partitions.max(1);

    let all_files = collect_source_files(repo_root)?;
    if all_files.is_empty() {
        return Ok(vec![]);
    }

    let mut partitions = detect_module_boundaries(repo_root, &all_files)?;

    if partitions.len() < 2 {
        partitions = directory_based_partitions(repo_root, &all_files);
    }

    partitions = ensure_complete_coverage(partitions, &all_files, repo_root);

    let coupling = git_coupling(repo_root);
    partitions = apply_coupling(partitions, &coupling);

    partitions = adjust_sizes(partitions);

    while partitions.len() > max_partitions {
        partitions = merge_smallest_pair(partitions);
    }

    partitions.sort_by(|a, b| a.label.cmp(&b.label));

    Ok(partitions)
}

fn collect_source_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    walk_dir(root, root, &mut files)?;
    files.sort();
    Ok(files)
}

fn walk_dir(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let entries =
        std::fs::read_dir(dir).with_context(|| format!("reading directory {}", dir.display()))?;

    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();

        if path.is_dir() {
            if IGNORED_DIRS.contains(&name.as_ref()) {
                continue;
            }
            walk_dir(root, &path, out)?;
        } else if is_source_file(&path) {
            if let Ok(rel) = path.strip_prefix(root) {
                out.push(rel.to_path_buf());
            }
        }
    }
    Ok(())
}

fn is_source_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| SOURCE_EXTENSIONS.contains(&ext))
}

fn count_lines(root: &Path, file: &Path) -> usize {
    let full = root.join(file);
    std::fs::read_to_string(&full).map_or(0, |contents| contents.lines().count())
}

fn count_lines_many(root: &Path, files: &[PathBuf]) -> usize {
    files.iter().map(|f| count_lines(root, f)).sum()
}

fn make_partition(root: &Path, label: String, files: Vec<PathBuf>) -> Partition {
    let line_count = count_lines_many(root, &files);
    Partition {
        label,
        files,
        line_count,
    }
}

fn detect_module_boundaries(root: &Path, all_files: &[PathBuf]) -> Result<Vec<Partition>> {
    let crate_roots = find_cargo_tomls(root)?;
    if crate_roots.is_empty() {
        return Ok(vec![]);
    }

    let mut partitions: Vec<Partition> = Vec::new();

    for crate_root in &crate_roots {
        let rel_crate = crate_root
            .strip_prefix(root)
            .unwrap_or(crate_root)
            .to_path_buf();
        let crate_label = if rel_crate == Path::new("") {
            "root".to_string()
        } else {
            rel_crate.display().to_string().replace('/', "::")
        };

        let src_dir = crate_root.join("src");
        if !src_dir.is_dir() {
            continue;
        }

        let entry_points = ["lib.rs", "main.rs"];
        let mut mod_map: HashMap<String, Vec<PathBuf>> = HashMap::new();
        let mut claimed: HashSet<PathBuf> = HashSet::new();

        for ep in &entry_points {
            let ep_path = src_dir.join(ep);
            if ep_path.is_file() {
                if let Ok(contents) = std::fs::read_to_string(&ep_path) {
                    for mod_name in parse_mod_declarations(&contents) {
                        let mod_files = find_mod_files(root, &src_dir, &mod_name, all_files);
                        if !mod_files.is_empty() {
                            for f in &mod_files {
                                claimed.insert(f.clone());
                            }
                            mod_map.insert(mod_name, mod_files);
                        }
                    }
                }
            }
        }

        for (mod_name, files) in &mod_map {
            let label = format!("{crate_label}::{mod_name}");
            partitions.push(make_partition(root, label, files.clone()));
        }

        let crate_src_rel = src_dir.strip_prefix(root).unwrap_or(&src_dir).to_path_buf();
        let unclaimed: Vec<PathBuf> = all_files
            .iter()
            .filter(|f| f.starts_with(&crate_src_rel) && !claimed.contains(*f))
            .cloned()
            .collect();
        if !unclaimed.is_empty() {
            partitions.push(make_partition(
                root,
                format!("{crate_label}::_root"),
                unclaimed,
            ));
        }
    }

    Ok(partitions)
}

fn find_cargo_tomls(root: &Path) -> Result<Vec<PathBuf>> {
    let mut results = Vec::new();
    find_cargo_tomls_recurse(root, root, &mut results)?;

    results.sort_by_key(|p| p.components().count());
    Ok(results)
}

#[allow(clippy::only_used_in_recursion)]
fn find_cargo_tomls_recurse(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let ct = dir.join("Cargo.toml");
    if ct.is_file() {
        out.push(dir.to_path_buf());
    }
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if path.is_dir() && !IGNORED_DIRS.contains(&name_str.as_ref()) {
                find_cargo_tomls_recurse(root, &path, out)?;
            }
        }
    }
    Ok(())
}

fn parse_mod_declarations(source: &str) -> Vec<String> {
    let mut mods = Vec::new();
    for line in source.lines() {
        let trimmed = line.trim();

        if let Some(name) = extract_mod_name(trimmed) {
            mods.push(name);
        }
    }
    mods
}

fn extract_mod_name(line: &str) -> Option<String> {
    let line = line.trim();

    if !line.ends_with(';') {
        return None;
    }
    let line = line.trim_end_matches(';').trim();

    let rest = if line.starts_with("pub(") {
        let idx = line.find(')')?;
        line[idx + 1..].trim()
    } else if let Some(rest) = line.strip_prefix("pub ") {
        rest.trim()
    } else {
        line
    };

    let rest = rest.strip_prefix("mod ")?.trim();

    if rest.is_empty() || !rest.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return None;
    }

    Some(rest.to_string())
}

fn find_mod_files(
    root: &Path,
    src_dir: &Path,
    mod_name: &str,
    all_files: &[PathBuf],
) -> Vec<PathBuf> {
    let src_rel = src_dir.strip_prefix(root).unwrap_or(src_dir);

    let single_file = src_rel.join(format!("{mod_name}.rs"));
    let dir_prefix = src_rel.join(mod_name);

    let mut files: Vec<PathBuf> = Vec::new();

    for f in all_files {
        if *f == single_file || f.starts_with(&dir_prefix) {
            files.push(f.clone());
        }
    }

    files
}

fn directory_based_partitions(root: &Path, all_files: &[PathBuf]) -> Vec<Partition> {
    let mut groups: HashMap<String, Vec<PathBuf>> = HashMap::new();

    for f in all_files {
        let key = f.components().next().map_or_else(
            || "_root".to_string(),
            |c| c.as_os_str().to_string_lossy().to_string(),
        );

        if f.components().count() == 1 {
            groups
                .entry("_root".to_string())
                .or_default()
                .push(f.clone());
        } else {
            groups.entry(key).or_default().push(f.clone());
        }
    }

    let mut partitions: Vec<Partition> = groups
        .into_iter()
        .map(|(label, files)| make_partition(root, label, files))
        .collect();

    partitions.sort_by(|a, b| a.label.cmp(&b.label));
    partitions
}

fn ensure_complete_coverage(
    mut partitions: Vec<Partition>,
    all_files: &[PathBuf],
    repo_root: &Path,
) -> Vec<Partition> {
    let assigned: HashSet<PathBuf> = partitions
        .iter()
        .flat_map(|p| p.files.iter().cloned())
        .collect();

    let missing: Vec<PathBuf> = all_files
        .iter()
        .filter(|f| !assigned.contains(*f))
        .cloned()
        .collect();

    if !missing.is_empty() {
        let line_count = count_lines_many(repo_root, &missing);
        partitions.push(Partition {
            label: "_uncategorized".to_string(),
            files: missing,
            line_count,
        });
    }

    let mut seen: HashSet<PathBuf> = HashSet::new();
    for part in &mut partitions {
        part.files.retain(|f| seen.insert(f.clone()));
    }

    partitions.retain(|p| !p.files.is_empty());

    partitions
}

type CouplingMap = HashMap<PathBuf, HashSet<PathBuf>>;

fn git_coupling(repo_root: &Path) -> CouplingMap {
    git_coupling_inner(repo_root).unwrap_or_default()
}

fn git_coupling_inner(repo_root: &Path) -> Result<CouplingMap> {
    let output = std::process::Command::new("git")
        .args([
            "log",
            "--name-only",
            "--pretty=format:",
            "-n",
            &GIT_LOG_DEPTH.to_string(),
        ])
        .current_dir(repo_root)
        .output()
        .context("running git log")?;

    if !output.status.success() {
        return Ok(HashMap::new());
    }

    let text = String::from_utf8_lossy(&output.stdout);

    let mut pair_counts: HashMap<(PathBuf, PathBuf), usize> = HashMap::new();
    let mut current_commit: Vec<PathBuf> = Vec::new();

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            record_pairs(&current_commit, &mut pair_counts);
            current_commit.clear();
        } else {
            let p = PathBuf::from(line);
            if is_source_file(&p) {
                current_commit.push(p);
            }
        }
    }

    record_pairs(&current_commit, &mut pair_counts);

    let mut coupling: CouplingMap = HashMap::new();
    for ((a, b), count) in &pair_counts {
        if *count >= COUPLING_THRESHOLD {
            coupling.entry(a.clone()).or_default().insert(b.clone());
            coupling.entry(b.clone()).or_default().insert(a.clone());
        }
    }

    Ok(coupling)
}

fn record_pairs(files: &[PathBuf], counts: &mut HashMap<(PathBuf, PathBuf), usize>) {
    if files.len() < 2 {
        return;
    }
    for i in 0..files.len() {
        for j in (i + 1)..files.len() {
            let a = files[i].clone();
            let b = files[j].clone();
            let key = if a < b { (a, b) } else { (b, a) };
            *counts.entry(key).or_insert(0) += 1;
        }
    }
}

fn apply_coupling(mut partitions: Vec<Partition>, coupling: &CouplingMap) -> Vec<Partition> {
    const fn find(parent: &mut [usize], mut x: usize) -> usize {
        while parent[x] != x {
            parent[x] = parent[parent[x]];
            x = parent[x];
        }
        x
    }

    if coupling.is_empty() {
        return partitions;
    }

    let file_to_idx: HashMap<PathBuf, usize> = partitions
        .iter()
        .enumerate()
        .flat_map(|(idx, p)| p.files.iter().map(move |f| (f.clone(), idx)))
        .collect();

    let mut merge_votes: HashMap<(usize, usize), usize> = HashMap::new();
    for (file, coupled_files) in coupling {
        if let Some(&idx_a) = file_to_idx.get(file) {
            for cf in coupled_files {
                if let Some(&idx_b) = file_to_idx.get(cf) {
                    if idx_a != idx_b {
                        let key = if idx_a < idx_b {
                            (idx_a, idx_b)
                        } else {
                            (idx_b, idx_a)
                        };
                        *merge_votes.entry(key).or_insert(0) += 1;
                    }
                }
            }
        }
    }

    let n = partitions.len();
    let mut parent: Vec<usize> = (0..n).collect();

    let mut merges: Vec<((usize, usize), usize)> = merge_votes.into_iter().collect();
    merges.sort_by_key(|b| std::cmp::Reverse(b.1));

    for ((a, b), votes) in merges {
        if votes < COUPLING_THRESHOLD {
            break;
        }
        let ra = find(&mut parent, a);
        let rb = find(&mut parent, b);
        if ra != rb {
            parent[rb] = ra;
        }
    }

    let mut groups: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..n {
        let root = find(&mut parent, i);
        groups.entry(root).or_default().push(i);
    }

    let mut result: Vec<Partition> = Vec::new();
    for (_root, indices) in groups {
        if indices.len() == 1 {
            result.push(partitions[indices[0]].clone());
        } else {
            let label = indices
                .iter()
                .map(|&i| partitions[i].label.as_str())
                .collect::<Vec<_>>()
                .join("+");
            let mut files: Vec<PathBuf> = Vec::new();
            let mut line_count = 0;
            for &i in &indices {
                files.append(&mut partitions[i].files);
                line_count += partitions[i].line_count;
            }
            files.sort();
            result.push(Partition {
                label,
                files,
                line_count,
            });
        }
    }

    result
}

fn adjust_sizes(partitions: Vec<Partition>) -> Vec<Partition> {
    let mut split_result: Vec<Partition> = Vec::new();
    for part in partitions {
        if part.line_count > MAX_PARTITION_LINES && part.files.len() > 1 {
            split_result.extend(split_partition(part));
        } else {
            split_result.push(part);
        }
    }

    merge_small_partitions(split_result)
}

fn split_partition(part: Partition) -> Vec<Partition> {
    let total = part.line_count;
    if total == 0 || part.files.len() <= 1 {
        return vec![part];
    }

    let half = total / 2;
    let mut left_files: Vec<PathBuf> = Vec::new();
    let mut left_lines = 0usize;
    let mut right_files: Vec<PathBuf> = Vec::new();
    let mut right_lines = 0usize;

    let per_file = total / part.files.len().max(1);
    for f in part.files {
        if left_lines < half {
            left_lines += per_file;
            left_files.push(f);
        } else {
            right_lines += per_file;
            right_files.push(f);
        }
    }

    let mut results = Vec::new();
    if !left_files.is_empty() {
        results.push(Partition {
            label: format!("{}/a", part.label),
            files: left_files,
            line_count: left_lines,
        });
    }
    if !right_files.is_empty() {
        results.push(Partition {
            label: format!("{}/b", part.label),
            files: right_files,
            line_count: right_lines,
        });
    }

    let mut final_results = Vec::new();
    for p in results {
        if p.line_count > MAX_PARTITION_LINES && p.files.len() > 1 {
            final_results.extend(split_partition(p));
        } else {
            final_results.push(p);
        }
    }

    final_results
}

fn merge_small_partitions(mut partitions: Vec<Partition>) -> Vec<Partition> {
    if partitions.len() <= 1 {
        return partitions;
    }

    partitions.sort_by_key(|p| p.line_count);

    let mut merged: Vec<Partition> = Vec::new();
    let mut carry: Option<Partition> = None;

    for part in partitions {
        match carry.take() {
            None => {
                if part.line_count < MIN_PARTITION_LINES {
                    carry = Some(part);
                } else {
                    merged.push(part);
                }
            }
            Some(mut prev) => {
                if prev.line_count + part.line_count < MIN_PARTITION_LINES
                    || prev.line_count < MIN_PARTITION_LINES
                {
                    let label = format!("{}+{}", prev.label, part.label);
                    let line_count = prev.line_count + part.line_count;
                    let mut files = Vec::new();
                    files.append(&mut prev.files);
                    files.extend(part.files);
                    let merged_part = Partition {
                        label,
                        files,
                        line_count,
                    };
                    if merged_part.line_count < MIN_PARTITION_LINES {
                        carry = Some(merged_part);
                    } else {
                        merged.push(merged_part);
                    }
                } else {
                    merged.push(prev);
                    if part.line_count < MIN_PARTITION_LINES {
                        carry = Some(part);
                    } else {
                        merged.push(part);
                    }
                }
            }
        }
    }

    if let Some(leftover) = carry {
        if let Some(last) = merged.last_mut() {
            last.label = format!("{}+{}", last.label, leftover.label);
            last.line_count += leftover.line_count;
            last.files.extend(leftover.files);
        } else {
            merged.push(leftover);
        }
    }

    merged
}

fn merge_smallest_pair(mut partitions: Vec<Partition>) -> Vec<Partition> {
    if partitions.len() <= 1 {
        return partitions;
    }

    let Some(min_idx) = partitions
        .iter()
        .enumerate()
        .min_by_key(|(_, p)| p.line_count)
        .map(|(i, _)| i)
    else {
        return partitions;
    };

    let Some(partner_idx) = partitions
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != min_idx)
        .min_by_key(|(_, p)| p.line_count)
        .map(|(i, _)| i)
    else {
        return partitions;
    };

    let (lo, hi) = if min_idx < partner_idx {
        (min_idx, partner_idx)
    } else {
        (partner_idx, min_idx)
    };

    let removed = partitions.remove(hi);
    let target = &mut partitions[lo];
    target.label = format!("{}+{}", target.label, removed.label);
    target.line_count += removed.line_count;
    target.files.extend(removed.files);

    partitions
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn setup_repo(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (path, content) in files {
            let full = dir.path().join(path);
            if let Some(parent) = full.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&full, content).unwrap();
        }

        std::process::Command::new("git")
            .args(["init"])
            .current_dir(dir.path())
            .output()
            .ok();
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(dir.path())
            .output()
            .ok();
        std::process::Command::new("git")
            .args(["commit", "-m", "init", "--allow-empty"])
            .current_dir(dir.path())
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@test.com")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@test.com")
            .output()
            .ok();
        dir
    }

    #[test]
    fn test_is_source_file() {
        assert!(is_source_file(Path::new("foo.rs")));
        assert!(is_source_file(Path::new("bar/baz.ts")));
        assert!(is_source_file(Path::new("main.go")));
        assert!(!is_source_file(Path::new("readme.md")));
        assert!(!is_source_file(Path::new("Cargo.toml")));
        assert!(!is_source_file(Path::new("data.json")));
    }

    #[test]
    fn test_parse_mod_declarations() {
        let src = r"
mod foo;
pub mod bar;
pub(crate) mod baz;
#[allow(dead_code)]
mod qux;
mod inline_mod {
    fn something() {}
}
";
        let mods = parse_mod_declarations(src);
        assert_eq!(mods, vec!["foo", "bar", "baz", "qux"]);
    }

    #[test]
    fn test_extract_mod_name_edge_cases() {
        assert_eq!(extract_mod_name("mod foo;"), Some("foo".to_string()));
        assert_eq!(extract_mod_name("pub mod bar;"), Some("bar".to_string()));
        assert_eq!(
            extract_mod_name("pub(crate) mod baz;"),
            Some("baz".to_string())
        );
        assert_eq!(
            extract_mod_name("pub(super) mod thing;"),
            Some("thing".to_string())
        );

        assert_eq!(extract_mod_name("mod inline {"), None);

        assert_eq!(extract_mod_name("use foo;"), None);

        assert_eq!(extract_mod_name("mod ;"), None);
    }

    #[test]
    fn test_collect_source_files_ignores_target() {
        let repo = setup_repo(&[
            ("src/main.rs", "fn main() {}"),
            ("src/lib.rs", "pub mod foo;"),
            ("target/debug/build.rs", "// build artifact"),
            ("node_modules/pkg/index.js", "module.exports = {}"),
        ]);
        let files = collect_source_files(repo.path()).unwrap();
        assert!(files.contains(&PathBuf::from("src/main.rs")));
        assert!(files.contains(&PathBuf::from("src/lib.rs")));
        assert!(!files.iter().any(|f| f.starts_with("target")));
        assert!(!files.iter().any(|f| f.starts_with("node_modules")));
    }

    #[test]
    fn test_directory_based_partitions() {
        let repo = setup_repo(&[
            ("src/main.rs", "fn main() {}\nfn a() {}\nfn b() {}"),
            ("src/lib.rs", "pub fn lib() {}"),
            ("tests/test1.rs", "fn test() {}"),
            ("benches/bench.rs", "fn bench() {}"),
        ]);
        let files = collect_source_files(repo.path()).unwrap();
        let parts = directory_based_partitions(repo.path(), &files);

        let labels: Vec<&str> = parts.iter().map(|p| p.label.as_str()).collect();
        assert!(labels.contains(&"src"));
        assert!(labels.contains(&"tests"));
        assert!(labels.contains(&"benches"));
    }

    #[test]
    fn test_detect_seams_rust_crate() {
        let repo = setup_repo(&[
            (
                "Cargo.toml",
                "[package]\nname = \"test\"\nversion = \"0.1.0\"\nedition = \"2021\"",
            ),
            (
                "src/main.rs",
                "mod foo;\nmod bar;\nfn main() { foo::run(); bar::run(); }",
            ),
            ("src/foo.rs", &"fn run() {}\n".repeat(100)),
            ("src/bar.rs", &"fn run() {}\n".repeat(100)),
        ]);

        let partitions = detect_seams(repo.path(), 10).unwrap();
        assert!(!partitions.is_empty());

        let all_files: HashSet<PathBuf> = partitions
            .iter()
            .flat_map(|p| p.files.iter().cloned())
            .collect();
        assert!(all_files.contains(&PathBuf::from("src/main.rs")));
        assert!(all_files.contains(&PathBuf::from("src/foo.rs")));
        assert!(all_files.contains(&PathBuf::from("src/bar.rs")));
    }

    #[test]
    fn test_detect_seams_empty_repo() {
        let repo = setup_repo(&[("README.md", "# Hello")]);
        let partitions = detect_seams(repo.path(), 5).unwrap();
        assert!(partitions.is_empty());
    }

    #[test]
    fn test_non_overlapping() {
        let repo = setup_repo(&[
            (
                "Cargo.toml",
                "[package]\nname = \"test\"\nversion = \"0.1.0\"\nedition = \"2021\"",
            ),
            ("src/main.rs", "mod a;\nmod b;\nfn main() {}"),
            ("src/a.rs", &"fn a() {}\n".repeat(50)),
            ("src/b.rs", &"fn b() {}\n".repeat(50)),
            ("src/b/extra.rs", &"fn extra() {}\n".repeat(50)),
            ("other/script.py", "print('hello')\n"),
        ]);

        let partitions = detect_seams(repo.path(), 10).unwrap();

        let mut seen: HashSet<PathBuf> = HashSet::new();
        for part in &partitions {
            for f in &part.files {
                assert!(
                    seen.insert(f.clone()),
                    "file {f:?} appears in multiple partitions"
                );
            }
        }
    }

    #[test]
    fn test_max_partitions_respected() {
        let mut files = Vec::new();
        for i in 0..20 {
            let dir = format!("dir{i}");
            files.push((format!("{dir}/file.rs"), "fn foo() {}\n".repeat(100)));
        }
        let file_refs: Vec<(&str, &str)> = files
            .iter()
            .map(|(p, c)| (p.as_str(), c.as_str()))
            .collect();
        let repo = setup_repo(&file_refs);

        let partitions = detect_seams(repo.path(), 3).unwrap();
        assert!(
            partitions.len() <= 3,
            "expected <=3 partitions, got {}",
            partitions.len()
        );

        let total_files: usize = partitions.iter().map(|p| p.files.len()).sum();
        assert_eq!(total_files, 20);
    }

    #[test]
    fn test_size_based_splitting() {
        let big_content = "fn line() {}\n".repeat(2500);
        let repo = setup_repo(&[
            (
                "Cargo.toml",
                "[package]\nname = \"test\"\nversion = \"0.1.0\"\nedition = \"2021\"",
            ),
            ("src/main.rs", "mod big;\nfn main() {}"),
            ("src/big/mod.rs", &big_content),
            ("src/big/sub1.rs", &"fn s1() {}\n".repeat(500)),
            ("src/big/sub2.rs", &"fn s2() {}\n".repeat(500)),
        ]);

        let partitions = detect_seams(repo.path(), 20).unwrap();

        let big_parts: Vec<&Partition> = partitions
            .iter()
            .filter(|p| p.label.contains("big"))
            .collect();

        let total_big_lines: usize = big_parts.iter().map(|p| p.line_count).sum();
        assert!(
            total_big_lines > 2000,
            "big module lines = {total_big_lines}"
        );
    }

    #[test]
    fn test_merge_small_partitions() {
        let partitions = vec![
            Partition {
                label: "tiny1".to_string(),
                files: vec![PathBuf::from("a.rs")],
                line_count: 50,
            },
            Partition {
                label: "tiny2".to_string(),
                files: vec![PathBuf::from("b.rs")],
                line_count: 30,
            },
            Partition {
                label: "big".to_string(),
                files: vec![PathBuf::from("c.rs")],
                line_count: 500,
            },
        ];

        let result = merge_small_partitions(partitions);

        assert!(
            result.len() <= 2,
            "expected <=2 after merge, got {}",
            result.len()
        );
    }

    #[test]
    fn test_record_pairs() {
        let files = vec![
            PathBuf::from("a.rs"),
            PathBuf::from("b.rs"),
            PathBuf::from("c.rs"),
        ];
        let mut counts = HashMap::new();
        record_pairs(&files, &mut counts);

        assert_eq!(counts.len(), 3);
        for count in counts.values() {
            assert_eq!(*count, 1);
        }
    }

    #[test]
    fn test_partition_serialization() {
        let part = Partition {
            label: "test".to_string(),
            files: vec![PathBuf::from("src/main.rs")],
            line_count: 42,
        };
        let json = serde_json::to_string(&part).unwrap();
        let deserialized: Partition = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.label, "test");
        assert_eq!(deserialized.line_count, 42);
        assert_eq!(deserialized.files.len(), 1);
    }

    #[test]
    fn test_count_lines() {
        let repo = setup_repo(&[("src/file.rs", "line1\nline2\nline3\n")]);
        let count = count_lines(repo.path(), Path::new("src/file.rs"));
        assert_eq!(count, 3);
    }

    #[test]
    fn test_merge_smallest_pair() {
        let partitions = vec![
            Partition {
                label: "a".to_string(),
                files: vec![PathBuf::from("a.rs")],
                line_count: 10,
            },
            Partition {
                label: "b".to_string(),
                files: vec![PathBuf::from("b.rs")],
                line_count: 20,
            },
            Partition {
                label: "c".to_string(),
                files: vec![PathBuf::from("c.rs")],
                line_count: 500,
            },
        ];
        let result = merge_smallest_pair(partitions);
        assert_eq!(result.len(), 2);

        let merged = result.iter().find(|p| p.label.contains('a')).unwrap();
        assert!(merged.label.contains('b'));
        assert_eq!(merged.line_count, 30);
    }

    #[test]
    fn test_count_lines_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let count = count_lines(dir.path(), Path::new("does_not_exist.rs"));
        assert_eq!(count, 0);
    }

    #[test]
    fn test_detect_module_boundaries_nested_crate_label() {
        let repo = setup_repo(&[
            ("Cargo.toml", "[workspace]\nmembers = [\"sub\"]\n"),
            (
                "sub/Cargo.toml",
                "[package]\nname = \"sub\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            ),
            ("sub/src/lib.rs", "pub mod util;\n"),
            ("sub/src/util.rs", "pub fn helper() {}\n"),
        ]);
        let files = collect_source_files(repo.path()).unwrap();
        let parts = detect_module_boundaries(repo.path(), &files).unwrap();

        let has_nested_label = parts.iter().any(|p| p.label.contains("sub"));
        assert!(
            has_nested_label,
            "expected a partition labelled with the nested crate path, got {:?}",
            parts.iter().map(|p| &p.label).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_extract_mod_name_unclosed_pub_paren() {
        assert_eq!(extract_mod_name("pub(crate mod foo;"), None);
    }

    #[test]
    fn test_extract_mod_name_invalid_identifier() {
        assert_eq!(extract_mod_name("mod foo-bar;"), None);

        assert_eq!(extract_mod_name("mod foo bar;"), None);
    }

    #[test]
    fn test_directory_based_partitions_root_files() {
        let repo = setup_repo(&[
            ("main.rs", "fn main() {}"),
            ("lib.rs", "pub fn lib() {}"),
            ("sub/helper.rs", "fn help() {}"),
        ]);
        let files = collect_source_files(repo.path()).unwrap();
        let parts = directory_based_partitions(repo.path(), &files);
        let labels: Vec<&str> = parts.iter().map(|p| p.label.as_str()).collect();
        assert!(
            labels.contains(&"_root"),
            "expected a '_root' partition for root-level files, got {labels:?}"
        );
        let root_part = parts.iter().find(|p| p.label == "_root").unwrap();

        assert_eq!(root_part.files.len(), 2);
    }

    #[test]
    fn test_git_coupling_builds_map() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/a.rs"), "fn a() {}\n").unwrap();
        fs::write(root.join("src/b.rs"), "fn b() {}\n").unwrap();

        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(root)
                .env("GIT_AUTHOR_NAME", "test")
                .env("GIT_AUTHOR_EMAIL", "test@test.com")
                .env("GIT_COMMITTER_NAME", "test")
                .env("GIT_COMMITTER_EMAIL", "test@test.com")
                .output()
                .ok();
        };

        git(&["init"]);
        git(&["add", "."]);
        git(&["commit", "-m", "init"]);

        for i in 0..COUPLING_THRESHOLD {
            let msg = format!("change {i}");
            let content_a = format!("fn a() {{ {i} }}\n");
            let content_b = format!("fn b() {{ {i} }}\n");
            fs::write(root.join("src/a.rs"), &content_a).unwrap();
            fs::write(root.join("src/b.rs"), &content_b).unwrap();
            git(&["add", "src/a.rs", "src/b.rs"]);
            git(&["commit", "-m", &msg]);
        }

        let coupling = git_coupling(root);

        assert!(
            !coupling.is_empty(),
            "expected non-empty coupling map after {COUPLING_THRESHOLD} co-commits"
        );
    }

    #[test]
    fn test_split_partition_zero_lines() {
        let part = Partition {
            label: "zero".to_string(),
            files: vec![PathBuf::from("a.rs"), PathBuf::from("b.rs")],
            line_count: 0,
        };
        let result = split_partition(part);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].label, "zero");
    }

    #[test]
    fn test_split_partition_single_file() {
        let part = Partition {
            label: "single".to_string(),
            files: vec![PathBuf::from("a.rs")],
            line_count: 5000,
        };
        let result = split_partition(part);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].label, "single");
    }

    #[test]
    fn test_merge_small_partitions_single() {
        let partitions = vec![Partition {
            label: "only".to_string(),
            files: vec![PathBuf::from("a.rs")],
            line_count: 10,
        }];
        let result = merge_small_partitions(partitions);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].label, "only");
    }

    #[test]
    fn test_merge_small_partitions_empty() {
        let result = merge_small_partitions(vec![]);
        assert!(result.is_empty());
    }

    #[test]
    fn test_merge_small_partitions_both_big() {
        let partitions = vec![
            Partition {
                label: "big1".to_string(),
                files: vec![PathBuf::from("a.rs")],
                line_count: 500,
            },
            Partition {
                label: "big2".to_string(),
                files: vec![PathBuf::from("b.rs")],
                line_count: 600,
            },
        ];
        let result = merge_small_partitions(partitions);

        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_merge_small_partitions_prev_big_part_small() {
        let partitions = vec![
            Partition {
                label: "small1".to_string(),
                files: vec![PathBuf::from("a.rs")],
                line_count: 50,
            },
            Partition {
                label: "big".to_string(),
                files: vec![PathBuf::from("b.rs")],
                line_count: 500,
            },
            Partition {
                label: "small2".to_string(),
                files: vec![PathBuf::from("c.rs")],
                line_count: 50,
            },
        ];
        let result = merge_small_partitions(partitions);

        assert!(result.len() <= 2);
    }

    #[test]
    fn test_merge_small_partitions_leftover_absorbed() {
        let partitions = vec![
            Partition {
                label: "x".to_string(),
                files: vec![PathBuf::from("x.rs")],
                line_count: 50,
            },
            Partition {
                label: "y".to_string(),
                files: vec![PathBuf::from("y.rs")],
                line_count: 60,
            },
        ];

        let result = merge_small_partitions(partitions);
        assert_eq!(result.len(), 1);
        assert!(result[0].label.contains('x') || result[0].label.contains('y'));
        assert_eq!(result[0].line_count, 110);
    }

    #[test]
    fn test_merge_small_leftover_absorbed_into_last() {
        let partitions = vec![
            Partition {
                label: "p1".to_string(),
                files: vec![PathBuf::from("p1.rs")],
                line_count: 80,
            },
            Partition {
                label: "p2".to_string(),
                files: vec![PathBuf::from("p2.rs")],
                line_count: 90,
            },
            Partition {
                label: "p3".to_string(),
                files: vec![PathBuf::from("p3.rs")],
                line_count: 100,
            },
        ];

        let result = merge_small_partitions(partitions);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].line_count, 270);
        assert_eq!(result[0].files.len(), 3);
    }

    #[test]
    fn test_merge_smallest_pair_single() {
        let part = Partition {
            label: "only".to_string(),
            files: vec![PathBuf::from("a.rs")],
            line_count: 100,
        };
        let result = merge_smallest_pair(vec![part]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].label, "only");
    }

    #[test]
    fn test_merge_smallest_pair_empty() {
        let result = merge_smallest_pair(vec![]);
        assert!(result.is_empty());
    }

    #[test]
    fn test_merge_smallest_pair_partner_before_min() {
        let partitions = vec![
            Partition {
                label: "partner".to_string(),
                files: vec![PathBuf::from("p.rs")],
                line_count: 10,
            },
            Partition {
                label: "middle".to_string(),
                files: vec![PathBuf::from("m.rs")],
                line_count: 500,
            },
            Partition {
                label: "min".to_string(),
                files: vec![PathBuf::from("n.rs")],
                line_count: 5,
            },
        ];
        let result = merge_smallest_pair(partitions);
        assert_eq!(result.len(), 2);

        let merged = result
            .iter()
            .find(|p| p.label.contains("partner") || p.label.contains("min"))
            .unwrap();
        assert_eq!(merged.line_count, 15);
        assert!(merged.label.contains("partner"));
        assert!(merged.label.contains("min"));
    }

    #[test]
    fn test_apply_coupling_merges_coupled_partitions() {
        let partitions = vec![
            Partition {
                label: "p_a".to_string(),
                files: vec![PathBuf::from("src/a.rs"), PathBuf::from("src/c.rs")],
                line_count: 100,
            },
            Partition {
                label: "p_b".to_string(),
                files: vec![PathBuf::from("src/b.rs")],
                line_count: 100,
            },
        ];

        let mut coupling: CouplingMap = HashMap::new();

        coupling
            .entry(PathBuf::from("src/a.rs"))
            .or_default()
            .insert(PathBuf::from("src/b.rs"));
        coupling
            .entry(PathBuf::from("src/b.rs"))
            .or_default()
            .insert(PathBuf::from("src/a.rs"));

        coupling
            .entry(PathBuf::from("src/c.rs"))
            .or_default()
            .insert(PathBuf::from("src/b.rs"));
        coupling
            .entry(PathBuf::from("src/b.rs"))
            .or_default()
            .insert(PathBuf::from("src/c.rs"));

        let result = apply_coupling(partitions, &coupling);

        assert_eq!(
            result.len(),
            1,
            "expected merge; got {:?}",
            result.iter().map(|p| &p.label).collect::<Vec<_>>()
        );
        assert_eq!(result[0].files.len(), 3);
        assert_eq!(result[0].line_count, 200);
    }

    #[test]
    fn test_apply_coupling_below_threshold_not_merged() {
        let partitions = vec![
            Partition {
                label: "pa".to_string(),
                files: vec![PathBuf::from("a.rs")],
                line_count: 300,
            },
            Partition {
                label: "pb".to_string(),
                files: vec![PathBuf::from("b.rs")],
                line_count: 300,
            },
        ];

        let mut coupling: CouplingMap = HashMap::new();
        coupling
            .entry(PathBuf::from("a.rs"))
            .or_default()
            .insert(PathBuf::from("b.rs"));
        coupling
            .entry(PathBuf::from("b.rs"))
            .or_default()
            .insert(PathBuf::from("a.rs"));

        let result = apply_coupling(partitions, &coupling);

        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_apply_coupling_empty_coupling() {
        let partitions = vec![
            Partition {
                label: "a".to_string(),
                files: vec![PathBuf::from("a.rs")],
                line_count: 10,
            },
            Partition {
                label: "b".to_string(),
                files: vec![PathBuf::from("b.rs")],
                line_count: 20,
            },
        ];
        let coupling: CouplingMap = HashMap::new();
        let result = apply_coupling(partitions, &coupling);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_apply_coupling_unknown_files_ignored() {
        let partitions = vec![Partition {
            label: "solo".to_string(),
            files: vec![PathBuf::from("known.rs")],
            line_count: 50,
        }];
        let mut coupling: CouplingMap = HashMap::new();
        coupling
            .entry(PathBuf::from("unknown.rs"))
            .or_default()
            .insert(PathBuf::from("also_unknown.rs"));
        let result = apply_coupling(partitions, &coupling);

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].label, "solo");
    }

    #[test]
    fn test_git_coupling_non_git_dir() {
        let dir = tempfile::tempdir().unwrap();

        let coupling = git_coupling(dir.path());
        assert!(coupling.is_empty());
    }

    #[test]
    fn test_ensure_complete_coverage_adds_uncategorized() {
        let tmp = tempfile::tempdir().unwrap();
        let partitions = vec![Partition {
            label: "known".to_string(),
            files: vec![PathBuf::from("a.rs")],
            line_count: 10,
        }];
        let all_files = vec![PathBuf::from("a.rs"), PathBuf::from("missing.rs")];
        let result = ensure_complete_coverage(partitions, &all_files, tmp.path());
        assert!(result.iter().any(|p| p.label == "_uncategorized"));
        let uncat = result.iter().find(|p| p.label == "_uncategorized").unwrap();
        assert_eq!(uncat.files, vec![PathBuf::from("missing.rs")]);
    }

    #[test]
    fn test_ensure_complete_coverage_deduplicates() {
        let tmp = tempfile::tempdir().unwrap();

        let partitions = vec![
            Partition {
                label: "first".to_string(),
                files: vec![PathBuf::from("a.rs"), PathBuf::from("b.rs")],
                line_count: 20,
            },
            Partition {
                label: "second".to_string(),
                files: vec![PathBuf::from("a.rs"), PathBuf::from("c.rs")],
                line_count: 20,
            },
        ];
        let all_files = vec![
            PathBuf::from("a.rs"),
            PathBuf::from("b.rs"),
            PathBuf::from("c.rs"),
        ];
        let result = ensure_complete_coverage(partitions, &all_files, tmp.path());
        let mut all_files_in_result: Vec<PathBuf> = result
            .iter()
            .flat_map(|p| p.files.iter().cloned())
            .collect();
        all_files_in_result.sort();
        all_files_in_result.dedup();

        assert_eq!(all_files_in_result.len(), 3);
    }

    #[test]
    fn test_ensure_complete_coverage_removes_empty_partitions() {
        let tmp = tempfile::tempdir().unwrap();

        let partitions = vec![
            Partition {
                label: "first".to_string(),
                files: vec![PathBuf::from("a.rs")],
                line_count: 10,
            },
            Partition {
                label: "empty_after_dedup".to_string(),
                files: vec![PathBuf::from("a.rs")],
                line_count: 10,
            },
        ];
        let all_files = vec![PathBuf::from("a.rs")];
        let result = ensure_complete_coverage(partitions, &all_files, tmp.path());

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].label, "first");
    }

    #[test]
    fn test_find_mod_files_single_file() {
        let root = Path::new("/repo");
        let src_dir = Path::new("/repo/src");
        let all_files = vec![
            PathBuf::from("src/foo.rs"),
            PathBuf::from("src/bar.rs"),
            PathBuf::from("src/main.rs"),
        ];
        let result = find_mod_files(root, src_dir, "foo", &all_files);
        assert_eq!(result, vec![PathBuf::from("src/foo.rs")]);
    }

    #[test]
    fn test_find_mod_files_directory_module() {
        let root = Path::new("/repo");
        let src_dir = Path::new("/repo/src");
        let all_files = vec![
            PathBuf::from("src/mymod/mod.rs"),
            PathBuf::from("src/mymod/helper.rs"),
            PathBuf::from("src/other.rs"),
        ];
        let result = find_mod_files(root, src_dir, "mymod", &all_files);
        assert_eq!(result.len(), 2);
        assert!(result.contains(&PathBuf::from("src/mymod/mod.rs")));
        assert!(result.contains(&PathBuf::from("src/mymod/helper.rs")));
    }

    #[test]
    fn test_find_mod_files_no_match() {
        let root = Path::new("/repo");
        let src_dir = Path::new("/repo/src");
        let all_files = vec![PathBuf::from("src/other.rs")];
        let result = find_mod_files(root, src_dir, "nonexistent", &all_files);
        assert!(result.is_empty());
    }

    #[test]
    fn test_record_pairs_single_file() {
        let files = vec![PathBuf::from("a.rs")];
        let mut counts = HashMap::new();
        record_pairs(&files, &mut counts);
        assert!(counts.is_empty());
    }

    #[test]
    fn test_record_pairs_empty() {
        let files: Vec<PathBuf> = vec![];
        let mut counts = HashMap::new();
        record_pairs(&files, &mut counts);
        assert!(counts.is_empty());
    }

    #[test]
    fn test_record_pairs_key_order() {
        let files1 = vec![PathBuf::from("a.rs"), PathBuf::from("b.rs")];
        let files2 = vec![PathBuf::from("b.rs"), PathBuf::from("a.rs")];
        let mut counts = HashMap::new();
        record_pairs(&files1, &mut counts);
        record_pairs(&files2, &mut counts);

        assert_eq!(counts.len(), 1);
        assert_eq!(*counts.values().next().unwrap(), 2);
    }

    #[test]
    fn test_detect_seams_max_partitions_zero() {
        let repo = setup_repo(&[("dir_a/a.rs", "fn a() {}\n"), ("dir_b/b.rs", "fn b() {}\n")]);

        let result = detect_seams(repo.path(), 0).unwrap();
        assert!(
            result.len() <= 1,
            "expected <=1 partition, got {}",
            result.len()
        );
    }

    #[test]
    fn test_detect_seams_non_rust_fallback() {
        let big_content = "const x = 1;\n".repeat(300);
        let repo = setup_repo(&[
            ("frontend/app.ts", &big_content),
            ("frontend/ui.ts", &big_content),
            ("backend/server.py", &big_content),
            ("backend/db.py", &big_content),
        ]);
        let partitions = detect_seams(repo.path(), 10).unwrap();

        let total_files: usize = partitions.iter().map(|p| p.files.len()).sum();
        assert_eq!(total_files, 4, "all 4 source files should be covered");
        assert!(!partitions.is_empty());
    }

    #[test]
    fn test_adjust_sizes_splits_large_partition() {
        let files: Vec<PathBuf> = (0..10).map(|i| PathBuf::from(format!("f{i}.rs"))).collect();
        let part = Partition {
            label: "big".to_string(),
            files,
            line_count: MAX_PARTITION_LINES + 1000,
        };
        let result = adjust_sizes(vec![part]);

        assert!(
            result.len() >= 2,
            "expected split, got {} partitions",
            result.len()
        );

        assert!(result.iter().all(|p| p.label.starts_with("big")));
    }

    #[test]
    fn test_adjust_sizes_small_partition_unchanged() {
        let part = Partition {
            label: "small".to_string(),
            files: vec![PathBuf::from("a.rs")],
            line_count: 100,
        };
        let result = adjust_sizes(vec![part]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].label, "small");
    }

    #[test]
    fn test_merge_small_partitions_big_prev_big_part() {
        let partitions = vec![
            Partition {
                label: "big".to_string(),
                files: vec![PathBuf::from("big.rs")],
                line_count: 300,
            },
            Partition {
                label: "small".to_string(),
                files: vec![PathBuf::from("small.rs")],
                line_count: 50,
            },
            Partition {
                label: "medium".to_string(),
                files: vec![PathBuf::from("medium.rs")],
                line_count: 150,
            },
        ];
        let result = merge_small_partitions(partitions);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_merge_small_partitions_carry_becomes_big_then_big_part() {
        let partitions = vec![
            Partition {
                label: "big2".to_string(),
                files: vec![PathBuf::from("big2.rs")],
                line_count: 300,
            },
            Partition {
                label: "big1".to_string(),
                files: vec![PathBuf::from("big1.rs")],
                line_count: 250,
            },
            Partition {
                label: "tiny".to_string(),
                files: vec![PathBuf::from("tiny.rs")],
                line_count: 50,
            },
            Partition {
                label: "tiny2".to_string(),
                files: vec![PathBuf::from("tiny2.rs")],
                line_count: 60,
            },
        ];
        let result = merge_small_partitions(partitions);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_merge_small_partitions_leftover_carry_absorbed() {
        let partitions = vec![
            Partition {
                label: "a".to_string(),
                files: vec![PathBuf::from("a.rs")],
                line_count: 150,
            },
            Partition {
                label: "b".to_string(),
                files: vec![PathBuf::from("b.rs")],
                line_count: 100,
            },
            Partition {
                label: "c".to_string(),
                files: vec![PathBuf::from("c.rs")],
                line_count: 250,
            },
        ];

        let result = merge_small_partitions(partitions);
        assert_eq!(result.len(), 2);
    }
}
