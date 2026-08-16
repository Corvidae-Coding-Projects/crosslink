use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::Utc;

use super::Database;

pub const SNAPSHOT_DIR: &str = "integrity";

pub const HYDRATION_BACKUP_PREFIX: &str = "hydration-backup-";

pub fn snapshot_to_integrity_dir(
    db: &Database,
    crosslink_dir: &Path,
    prefix: &str,
) -> Result<PathBuf> {
    let snapshot_dir = crosslink_dir.join(SNAPSHOT_DIR);
    std::fs::create_dir_all(&snapshot_dir).with_context(|| {
        format!(
            "Failed to create snapshot directory {}",
            snapshot_dir.display()
        )
    })?;

    let ts = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let filename = format!("{prefix}{ts}.sqlite");
    let dest_path = snapshot_dir.join(filename);

    let escaped = dest_path.to_string_lossy().replace('\'', "''");
    db.conn
        .execute(&format!("VACUUM INTO '{escaped}'"), [])
        .with_context(|| format!("Failed to write SQLite snapshot to {}", dest_path.display()))?;

    Ok(dest_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_snapshot_creates_file() {
        let dir = tempdir().unwrap();
        let db = Database::open(&dir.path().join("source.db")).unwrap();
        db.create_issue("test", None, "medium").unwrap();

        let crosslink_dir = dir.path().join(".crosslink");
        std::fs::create_dir_all(&crosslink_dir).unwrap();

        let snap_path =
            snapshot_to_integrity_dir(&db, &crosslink_dir, HYDRATION_BACKUP_PREFIX).unwrap();

        assert!(snap_path.exists(), "snapshot file must be created");
        assert!(snap_path.starts_with(crosslink_dir.join(SNAPSHOT_DIR)));
        assert!(snap_path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with(HYDRATION_BACKUP_PREFIX));

        let restored = Database::open(&snap_path).unwrap();
        let issues = restored.list_issues(None, None, None).unwrap();
        assert_eq!(issues.len(), 1, "snapshot must contain the source row");
    }

    #[test]
    fn test_snapshot_creates_integrity_dir_if_missing() {
        let dir = tempdir().unwrap();
        let db = Database::open(&dir.path().join("source.db")).unwrap();
        let crosslink_dir = dir.path().join(".crosslink");

        let snap_path =
            snapshot_to_integrity_dir(&db, &crosslink_dir, HYDRATION_BACKUP_PREFIX).unwrap();

        assert!(snap_path.parent().unwrap().exists());
    }

    #[test]
    fn test_snapshot_filename_has_utc_timestamp() {
        let dir = tempdir().unwrap();
        let db = Database::open(&dir.path().join("source.db")).unwrap();
        let crosslink_dir = dir.path().join(".crosslink");

        let snap_path =
            snapshot_to_integrity_dir(&db, &crosslink_dir, HYDRATION_BACKUP_PREFIX).unwrap();
        let name = snap_path.file_name().unwrap().to_string_lossy();

        let stripped = name
            .strip_prefix(HYDRATION_BACKUP_PREFIX)
            .and_then(|s| s.strip_suffix(".sqlite"))
            .expect("filename must follow <prefix><ts>.sqlite shape");
        assert_eq!(
            stripped.len(),
            16,
            "timestamp section must be exactly 16 chars (YYYYMMDDTHHMMSSZ), got: {stripped}"
        );
        assert!(stripped.ends_with('Z'), "timestamp must be UTC (ends in Z)");
    }
}
