use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const FLAG_DIR: &str = "agent-flags";

const PAUSED: &str = "paused";
const KILL: &str = "kill";
const REPRIORITISE: &str = "reprioritise.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReprioritiseHint {
    pub issue_id: i64,

    pub from_request_id: String,
}

fn flag_dir(crosslink_dir: &Path) -> PathBuf {
    crosslink_dir.join(FLAG_DIR)
}

fn ensure_flag_dir(crosslink_dir: &Path) -> Result<PathBuf> {
    let dir = flag_dir(crosslink_dir);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("create agent-flags dir {}", dir.display()))?;
    Ok(dir)
}

#[must_use]
pub fn is_paused(crosslink_dir: &Path) -> bool {
    flag_dir(crosslink_dir).join(PAUSED).exists()
}

#[must_use]
pub fn should_exit(crosslink_dir: &Path) -> bool {
    flag_dir(crosslink_dir).join(KILL).exists()
}

pub fn read_reprioritise_hint(crosslink_dir: &Path) -> Result<Option<ReprioritiseHint>> {
    let path = flag_dir(crosslink_dir).join(REPRIORITISE);
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let hint: ReprioritiseHint =
        serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
    Ok(Some(hint))
}

pub fn set_paused(crosslink_dir: &Path) -> Result<()> {
    let dir = ensure_flag_dir(crosslink_dir)?;
    std::fs::write(dir.join(PAUSED), b"").with_context(|| format!("write {PAUSED} flag"))?;
    Ok(())
}

pub fn clear_paused(crosslink_dir: &Path) -> Result<()> {
    let path = flag_dir(crosslink_dir).join(PAUSED);
    if path.exists() {
        std::fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
    }
    Ok(())
}

pub fn set_kill(crosslink_dir: &Path) -> Result<()> {
    let dir = ensure_flag_dir(crosslink_dir)?;
    std::fs::write(dir.join(KILL), b"").with_context(|| format!("write {KILL} flag"))?;
    Ok(())
}

pub fn set_reprioritise_hint(crosslink_dir: &Path, hint: &ReprioritiseHint) -> Result<()> {
    let dir = ensure_flag_dir(crosslink_dir)?;
    let body = serde_json::to_vec_pretty(hint).context("serialize reprioritise hint")?;
    std::fs::write(dir.join(REPRIORITISE), body)
        .with_context(|| format!("write {REPRIORITISE} hint"))?;
    Ok(())
}

#[allow(dead_code)]
pub fn clear_reprioritise_hint(crosslink_dir: &Path) -> Result<()> {
    let path = flag_dir(crosslink_dir).join(REPRIORITISE);
    if path.exists() {
        std::fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_pause_lifecycle() {
        let dir = tempdir().unwrap();
        assert!(!is_paused(dir.path()));
        set_paused(dir.path()).unwrap();
        assert!(is_paused(dir.path()));

        set_paused(dir.path()).unwrap();
        assert!(is_paused(dir.path()));
        clear_paused(dir.path()).unwrap();
        assert!(!is_paused(dir.path()));

        clear_paused(dir.path()).unwrap();
    }

    #[test]
    fn test_kill_flag() {
        let dir = tempdir().unwrap();
        assert!(!should_exit(dir.path()));
        set_kill(dir.path()).unwrap();
        assert!(should_exit(dir.path()));
    }

    #[test]
    fn test_reprioritise_hint_roundtrip() {
        let dir = tempdir().unwrap();
        assert!(read_reprioritise_hint(dir.path()).unwrap().is_none());

        let hint = ReprioritiseHint {
            issue_id: 42,
            from_request_id: "01HXY000000000000000000001".into(),
        };
        set_reprioritise_hint(dir.path(), &hint).unwrap();
        let loaded = read_reprioritise_hint(dir.path()).unwrap().unwrap();
        assert_eq!(loaded, hint);

        clear_reprioritise_hint(dir.path()).unwrap();
        assert!(read_reprioritise_hint(dir.path()).unwrap().is_none());
    }
}
