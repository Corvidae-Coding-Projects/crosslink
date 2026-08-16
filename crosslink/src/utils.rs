use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn read_agent_binary(crosslink_dir: &Path) -> String {
    crate::agents::resolve_agent(crosslink_dir).map_or_else(
        |_| "claude".to_string(),
        |agent| agent.binary.to_string_lossy().into_owned(),
    )
}

pub fn read_kickoff_template(crosslink_dir: &Path) -> Option<String> {
    let config_path = crosslink_dir.join("hook-config.json");
    let content = std::fs::read_to_string(&config_path).ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&content).ok()?;
    let path = parsed
        .get("agent")
        .and_then(|a| a.get("kickoff_template"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())?;
    let template_path = if std::path::Path::new(path).is_absolute() {
        std::path::PathBuf::from(path)
    } else {
        crosslink_dir.join(path)
    };

    std::fs::read_to_string(&template_path)
        .ok()
        .filter(|s| !s.is_empty())
}

pub fn resolve_kickoff_template(
    crosslink_dir: &Path,
    override_path: Option<&Path>,
) -> Option<String> {
    if let Some(path) = override_path {
        return std::fs::read_to_string(path).ok().filter(|s| !s.is_empty());
    }
    read_kickoff_template(crosslink_dir)
}

pub fn read_no_template(crosslink_dir: &Path) -> bool {
    let config_path = crosslink_dir.join("hook-config.json");
    let Ok(content) = std::fs::read_to_string(&config_path) else {
        return false;
    };
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&content) else {
        return false;
    };
    parsed
        .get("agent")
        .and_then(|a| a.get("no_template"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

#[must_use]
pub fn resolve_main_repo_root(repo_root: &Path) -> Option<PathBuf> {
    let repo_str = repo_root.to_string_lossy();

    let common_output = Command::new("git")
        .args(["-C", &repo_str, "rev-parse", "--git-common-dir"])
        .output()
        .ok()?;

    let git_dir_output = Command::new("git")
        .args(["-C", &repo_str, "rev-parse", "--git-dir"])
        .output()
        .ok()?;

    if !common_output.status.success() || !git_dir_output.status.success() {
        return None;
    }

    let common_raw = String::from_utf8_lossy(&common_output.stdout)
        .trim()
        .to_string();
    let git_dir_raw = String::from_utf8_lossy(&git_dir_output.stdout)
        .trim()
        .to_string();

    let common_path = if Path::new(&common_raw).is_absolute() {
        PathBuf::from(&common_raw)
    } else {
        repo_root.join(&common_raw)
    };

    let git_dir_path = if Path::new(&git_dir_raw).is_absolute() {
        PathBuf::from(&git_dir_raw)
    } else {
        repo_root.join(&git_dir_raw)
    };

    let common_canonical = common_path.canonicalize().unwrap_or(common_path);
    let git_dir_canonical = git_dir_path.canonicalize().unwrap_or(git_dir_path);

    if common_canonical == git_dir_canonical {
        Some(repo_root.to_path_buf())
    } else {
        common_canonical.parent().map(std::path::Path::to_path_buf)
    }
}

#[must_use]
pub fn format_issue_id(id: i64) -> String {
    if id < 0 {
        format!("L{}", id.unsigned_abs())
    } else {
        format!("#{id}")
    }
}

#[must_use]
pub fn truncate(s: &str, max_chars: usize) -> String {
    let char_count = s.chars().count();
    if char_count <= max_chars {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max_chars.saturating_sub(3)).collect();
        format!("{truncated}...")
    }
}

#[must_use]
pub fn is_windows_reserved_name(name: &str) -> bool {
    let upper = name.to_uppercase();
    let stem = upper.split('.').next().unwrap_or(&upper);
    matches!(
        stem,
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

pub fn atomic_write(path: &std::path::Path, content: &[u8]) -> anyhow::Result<()> {
    use anyhow::Context;
    use std::io::Write;

    let parent = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let basename = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");

    let mut tmp = tempfile::Builder::new()
        .prefix(&format!(".{basename}."))
        .suffix(".tmp")
        .tempfile_in(parent)
        .with_context(|| format!("Failed to create temp file in {}", parent.display()))?;

    tmp.as_file_mut()
        .write_all(content)
        .with_context(|| format!("Failed to write temp file for {}", path.display()))?;

    tmp.as_file()
        .sync_all()
        .with_context(|| format!("Failed to fsync temp file for {}", path.display()))?;

    persist_atomic_temp_file(tmp, path)?;

    #[cfg(unix)]
    {
        match std::fs::File::open(parent) {
            Ok(dir) => {
                if let Err(e) = dir.sync_all() {
                    tracing::warn!(
                        "failed to fsync parent dir {} after atomic write: {}",
                        parent.display(),
                        e
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    "failed to open parent dir {} for fsync: {}",
                    parent.display(),
                    e
                );
            }
        }
    }

    Ok(())
}

#[cfg(windows)]
fn persist_atomic_temp_file(
    mut tmp: tempfile::NamedTempFile,
    path: &std::path::Path,
) -> anyhow::Result<()> {
    use std::sync::{Mutex, OnceLock};

    static PERSIST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let _guard = PERSIST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    const MAX_ATTEMPTS: usize = 20;
    for attempt in 1..=MAX_ATTEMPTS {
        match tmp.persist(path) {
            Ok(_) => return Ok(()),
            Err(error)
                if error.error.kind() == std::io::ErrorKind::PermissionDenied
                    && attempt < MAX_ATTEMPTS =>
            {
                tmp = error.file;
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(error) => {
                return Err(anyhow::anyhow!(
                    "Failed to rename temp file to {}: {}",
                    path.display(),
                    error.error
                ));
            }
        }
    }

    unreachable!("bounded persist loop always returns")
}

#[cfg(not(windows))]
fn persist_atomic_temp_file(
    tmp: tempfile::NamedTempFile,
    path: &std::path::Path,
) -> anyhow::Result<()> {
    tmp.persist(path).map(|_| ()).map_err(|error| {
        anyhow::anyhow!(
            "Failed to rename temp file to {}: {}",
            path.display(),
            error.error
        )
    })
}

#[must_use]
pub fn shell_escape_arg(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[must_use]
pub fn shell_escape_path(path: &std::path::Path) -> String {
    let rendered = path.to_string_lossy().replace('\\', "/");
    shell_escape_arg(&rendered)
}

const BASE62_CHARS: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

pub fn generate_compact_id() -> String {
    use std::time::SystemTime;

    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    let pid = std::process::id();
    let count = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mixed = u64::from(nanos)
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(u64::from(pid))
        .wrapping_add(u64::from(count));
    base62_encode_4(mixed)
}

#[must_use]
pub fn base62_encode_4(mut value: u64) -> String {
    let mut result = String::with_capacity(4);
    let mut buf = [0u8; 4];
    for b in buf.iter_mut().rev() {
        *b = BASE62_CHARS[(value % 62) as usize];
        value /= 62;
    }
    for &b in &buf {
        result.push(b as char);
    }
    result
}

#[must_use]
pub fn compose_compact_name(repo_id: &str, agent_id: &str, slug: &str) -> String {
    let prefix_len = repo_id.len() + 1 + agent_id.len() + 1;
    let max_slug = 64 - prefix_len;
    let truncated_slug = truncate_slug(slug, max_slug);
    format!("{repo_id}-{agent_id}-{truncated_slug}")
}

#[must_use]
pub fn truncate_slug(slug: &str, max_len: usize) -> &str {
    if slug.len() <= max_len {
        return slug;
    }

    slug[..max_len]
        .rfind('-')
        .map_or(&slug[..max_len], |pos| &slug[..pos])
}

pub fn validate_compact_name(name: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        name.len() <= 64,
        "Composed name '{}' exceeds 64-char limit ({} chars)",
        name,
        name.len()
    );
    anyhow::ensure!(
        name.chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_'),
        "Composed name contains invalid characters: '{name}'"
    );
    Ok(())
}

fn parse_bare_date(s: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()
}

fn parse_rfc3339_as_utc(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

const fn date_at_time(d: NaiveDate, t: NaiveTime) -> DateTime<Utc> {
    DateTime::<Utc>::from_naive_utc_and_offset(NaiveDateTime::new(d, t), Utc)
}

fn end_of_day() -> NaiveTime {
    NaiveTime::from_hms_opt(23, 59, 59).unwrap_or(NaiveTime::MIN)
}

pub fn parse_scheduled_date(s: &str) -> Result<DateTime<Utc>, String> {
    if let Some(d) = parse_bare_date(s) {
        return Ok(date_at_time(d, NaiveTime::MIN));
    }
    parse_rfc3339_as_utc(s).ok_or_else(|| {
        format!("expected YYYY-MM-DD or RFC 3339 datetime (e.g. 2026-03-20T14:00:00Z), got: {s}")
    })
}

pub fn parse_due_date(s: &str) -> Result<DateTime<Utc>, String> {
    if let Some(d) = parse_bare_date(s) {
        return Ok(date_at_time(d, end_of_day()));
    }
    parse_rfc3339_as_utc(s).ok_or_else(|| {
        format!("expected YYYY-MM-DD or RFC 3339 datetime (e.g. 2026-03-20T14:00:00Z), got: {s}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as StdCommand;
    use tempfile::tempdir;

    fn init_git_repo(path: &Path) {
        let p = path.to_string_lossy().to_string();
        StdCommand::new("git")
            .args(["-C", &p, "init"])
            .output()
            .unwrap();
        StdCommand::new("git")
            .args(["-C", &p, "config", "user.email", "test@test.com"])
            .output()
            .unwrap();
        StdCommand::new("git")
            .args(["-C", &p, "config", "user.name", "Test"])
            .output()
            .unwrap();
        StdCommand::new("git")
            .args(["-C", &p, "commit", "--allow-empty", "-m", "init"])
            .output()
            .unwrap();
    }

    #[test]
    fn test_resolve_main_repo_root_not_a_repo() {
        let dir = tempdir().unwrap();
        let result = resolve_main_repo_root(dir.path());
        assert!(result.is_none());
    }

    #[test]
    fn test_resolve_main_repo_root_normal_repo() {
        let dir = tempdir().unwrap();
        init_git_repo(dir.path());

        let result = resolve_main_repo_root(dir.path());
        assert!(result.is_some());
        assert_eq!(
            result.unwrap().canonicalize().unwrap(),
            dir.path().canonicalize().unwrap()
        );
    }

    #[test]
    fn test_resolve_main_repo_root_in_worktree() {
        let dir = tempdir().unwrap();
        let main_root = dir.path().join("main");
        std::fs::create_dir_all(&main_root).unwrap();
        init_git_repo(&main_root);

        StdCommand::new("git")
            .args([
                "-C",
                &main_root.to_string_lossy(),
                "branch",
                "feature/wt-test",
            ])
            .output()
            .unwrap();

        let wt_path = main_root.join(".worktrees").join("wt-test");
        std::fs::create_dir_all(wt_path.parent().unwrap()).unwrap();
        StdCommand::new("git")
            .args([
                "-C",
                &main_root.to_string_lossy(),
                "worktree",
                "add",
                &wt_path.to_string_lossy(),
                "feature/wt-test",
            ])
            .output()
            .unwrap();

        let result = resolve_main_repo_root(&wt_path);
        assert!(result.is_some());
        assert_eq!(
            result.unwrap().canonicalize().unwrap(),
            main_root.canonicalize().unwrap()
        );
    }

    #[test]
    fn test_truncate_short_string() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn test_truncate_exact_length() {
        assert_eq!(truncate("hello", 5), "hello");
    }

    #[test]
    fn test_truncate_long_string() {
        assert_eq!(truncate("hello world", 8), "hello...");
    }

    #[test]
    fn test_truncate_unicode() {
        assert_eq!(truncate("héllo wörld", 8), "héllo...");
    }

    #[test]
    fn test_truncate_emoji() {
        assert_eq!(truncate("👋🌍🎉🚀🎯", 4), "👋...");
    }

    #[test]
    fn test_truncate_empty() {
        assert_eq!(truncate("", 10), "");
    }

    #[test]
    fn test_truncate_zero_max() {
        assert_eq!(truncate("hello", 0), "...");
    }

    #[test]
    fn test_windows_reserved_names_rejected() {
        for name in &["CON", "PRN", "AUX", "NUL", "COM1", "COM9", "LPT1", "LPT9"] {
            assert!(is_windows_reserved_name(name), "{name} should be reserved");
        }
    }

    #[test]
    fn test_windows_reserved_names_case_insensitive() {
        assert!(is_windows_reserved_name("con"));
        assert!(is_windows_reserved_name("Con"));
        assert!(is_windows_reserved_name("nul"));
        assert!(is_windows_reserved_name("Aux"));
    }

    #[test]
    fn test_windows_reserved_names_with_extension() {
        assert!(is_windows_reserved_name("CON.txt"));
        assert!(is_windows_reserved_name("nul.md"));
    }

    #[test]
    fn test_non_reserved_names_allowed() {
        assert!(!is_windows_reserved_name("console"));
        assert!(!is_windows_reserved_name("printer"));
        assert!(!is_windows_reserved_name("auxiliary"));
        assert!(!is_windows_reserved_name("my-agent"));
        assert!(!is_windows_reserved_name("com10"));
        assert!(!is_windows_reserved_name("lpt10"));
    }

    #[test]
    fn test_format_issue_id_positive() {
        assert_eq!(format_issue_id(1), "#1");
        assert_eq!(format_issue_id(42), "#42");
        assert_eq!(format_issue_id(0), "#0");
    }

    #[test]
    fn test_format_issue_id_negative() {
        assert_eq!(format_issue_id(-1), "L1");
        assert_eq!(format_issue_id(-99), "L99");
    }

    #[test]
    fn test_atomic_write_creates_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("output.txt");
        atomic_write(&path, b"hello world").unwrap();
        let contents = std::fs::read(&path).unwrap();
        assert_eq!(contents, b"hello world");
    }

    #[test]
    fn test_atomic_write_overwrites_existing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("output.txt");
        atomic_write(&path, b"first").unwrap();
        atomic_write(&path, b"second").unwrap();
        let contents = std::fs::read(&path).unwrap();
        assert_eq!(contents, b"second");
    }

    #[test]
    fn test_atomic_write_leaves_no_tmp_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("output.txt");
        atomic_write(&path, b"data").unwrap();

        let tmp_path = dir.path().join(".output.txt.tmp");
        assert!(!tmp_path.exists());
    }

    #[test]
    fn test_base62_encode_4_produces_4_chars() {
        assert_eq!(base62_encode_4(0).len(), 4);
        assert_eq!(base62_encode_4(u64::MAX).len(), 4);
        assert_eq!(base62_encode_4(12345).len(), 4);
    }

    #[test]
    fn test_base62_encode_4_zero() {
        assert_eq!(base62_encode_4(0), "0000");
    }

    #[test]
    fn test_base62_encode_4_deterministic() {
        assert_eq!(base62_encode_4(999_999), base62_encode_4(999_999));
    }

    #[test]
    fn test_base62_encode_4_all_valid_chars() {
        let result = base62_encode_4(0xDEAD_BEEF);
        assert!(result.chars().all(char::is_alphanumeric));
    }

    #[test]
    fn test_generate_compact_id_length() {
        let id = generate_compact_id();
        assert_eq!(id.len(), 4);
    }

    #[test]
    fn test_generate_compact_id_unique() {
        let ids: Vec<String> = (0..100).map(|_| generate_compact_id()).collect();
        let unique: std::collections::HashSet<_> = ids.iter().collect();

        assert_eq!(unique.len(), 100);
    }

    #[test]
    fn test_compose_compact_name_basic() {
        let name = compose_compact_name("XZ3j", "81jF", "auth-system");
        assert_eq!(name, "XZ3j-81jF-auth-system");
        assert!(name.len() <= 64);
    }

    #[test]
    fn test_compose_compact_name_truncates_long_slug() {
        let long_slug = "a]".repeat(60);
        let slug = long_slug.trim_end_matches(']').replace(']', "-long");
        let name = compose_compact_name("XZ3j", "81jF", &slug);
        assert!(name.len() <= 64, "Name too long: {} chars", name.len());
    }

    #[test]
    fn test_truncate_slug_short() {
        assert_eq!(truncate_slug("hello", 10), "hello");
    }

    #[test]
    fn test_truncate_slug_at_word_boundary() {
        assert_eq!(truncate_slug("hello-world-test", 12), "hello-world");
    }

    #[test]
    fn test_truncate_slug_no_hyphen() {
        assert_eq!(truncate_slug("abcdefghij", 5), "abcde");
    }

    #[test]
    fn test_validate_compact_name_ok() {
        assert!(validate_compact_name("XZ3j-81jF-auth-system").is_ok());
    }

    #[test]
    fn test_validate_compact_name_too_long() {
        let name = "a".repeat(65);
        assert!(validate_compact_name(&name).is_err());
    }

    #[test]
    fn test_validate_compact_name_invalid_chars() {
        assert!(validate_compact_name("hello world").is_err());
        assert!(validate_compact_name("hello/world").is_err());
    }

    #[test]
    fn test_parse_scheduled_date_iso_maps_to_start_of_day() {
        let dt = parse_scheduled_date("2026-03-20").unwrap();
        assert_eq!(dt.to_rfc3339(), "2026-03-20T00:00:00+00:00");
    }

    #[test]
    fn test_parse_due_date_iso_maps_to_end_of_day() {
        let dt = parse_due_date("2026-03-25").unwrap();
        assert_eq!(dt.to_rfc3339(), "2026-03-25T23:59:59+00:00");
    }

    #[test]
    fn test_parse_due_date_rfc3339_passthrough() {
        let dt = parse_due_date("2026-03-20T14:00:00Z").unwrap();
        assert_eq!(dt.to_rfc3339(), "2026-03-20T14:00:00+00:00");
    }

    #[test]
    fn test_parse_scheduled_date_rfc3339_passthrough() {
        let dt = parse_scheduled_date("2026-03-20T09:30:00Z").unwrap();
        assert_eq!(dt.to_rfc3339(), "2026-03-20T09:30:00+00:00");
    }

    #[test]
    fn test_parse_scheduled_date_rejects_garbage() {
        let err = parse_scheduled_date("not a date").unwrap_err();
        assert!(
            err.contains("YYYY-MM-DD") || err.contains("RFC 3339"),
            "error message should hint at accepted formats: {err}"
        );
    }

    #[test]
    fn test_parse_due_date_rejects_empty() {
        assert!(parse_due_date("").is_err());
    }

    #[test]
    fn test_parse_date_rejects_invalid_month() {
        assert!(parse_scheduled_date("2026-13-01").is_err());
    }

    #[test]
    fn test_parse_date_rejects_invalid_day() {
        assert!(parse_due_date("2026-02-31").is_err());
    }

    #[test]
    fn test_atomic_write_concurrent_no_interleaving() {
        use std::sync::Arc;

        let dir = tempdir().unwrap();
        let target = Arc::new(dir.path().join("shared.json"));

        let payload_a = serde_json::json!({"writer": "A", "data": "a".repeat(4096)})
            .to_string()
            .into_bytes();
        let payload_b = serde_json::json!({"writer": "B", "data": "b".repeat(4096)})
            .to_string()
            .into_bytes();

        let payload_a = Arc::new(payload_a);
        let payload_b = Arc::new(payload_b);

        const ITERATIONS: usize = 50;

        for _ in 0..ITERATIONS {
            let path_a = Arc::clone(&target);
            let path_b = Arc::clone(&target);
            let pa = Arc::clone(&payload_a);
            let pb = Arc::clone(&payload_b);

            let t_a = std::thread::spawn(move || {
                atomic_write(&path_a, &pa).expect("atomic_write A failed");
            });
            let t_b = std::thread::spawn(move || {
                atomic_write(&path_b, &pb).expect("atomic_write B failed");
            });

            t_a.join().expect("thread A panicked");
            t_b.join().expect("thread B panicked");

            let raw = std::fs::read(&*target).expect("target file missing");
            let v: serde_json::Value =
                serde_json::from_slice(&raw).expect("file is not valid JSON (interleaved bytes)");
            let writer = v["writer"].as_str().expect("missing writer field");
            assert!(
                writer == "A" || writer == "B",
                "unexpected writer field: {writer}"
            );
        }
    }
}
