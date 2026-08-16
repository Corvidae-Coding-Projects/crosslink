use aes_gcm::aead::rand_core::RngCore;
use aes_gcm::aead::{Aead, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use anyhow::{Context, Result};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use rusqlite::params;
use sha2::{Digest, Sha256};

use super::db::DashboardDb;

const KEY_TOKEN: &str = "github.token";

pub const KEY_DEFAULT_ORG: &str = "github.default_org";

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct Sealed {
    v: u32,

    nonce: String,

    ct: String,
}

fn derive_machine_key(db_path: &std::path::Path) -> [u8; 32] {
    let mut h = Sha256::new();
    if let Ok(mid) = std::fs::read_to_string("/etc/machine-id") {
        h.update(mid.trim().as_bytes());
    } else if let Ok(mid) = std::fs::read_to_string("/var/lib/dbus/machine-id") {
        h.update(mid.trim().as_bytes());
    } else if let Ok(hn) = std::env::var("HOSTNAME") {
        h.update(hn.as_bytes());
    } else if let Ok(out) = std::process::Command::new("hostname").output() {
        h.update(&out.stdout);
    }
    if let Ok(user) = std::env::var("USER") {
        h.update(user.as_bytes());
    }
    h.update(b"crosslink-dashboard-pat-v1");

    let fallback_path = db_path.with_file_name(".dashboard-key");
    let fallback = match std::fs::read(&fallback_path) {
        Ok(b) if b.len() >= 32 => b,
        _ => {
            let mut buf = [0u8; 32];

            let _ = OsRng.try_fill_bytes(&mut buf);
            let _ = std::fs::write(&fallback_path, buf);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(
                    &fallback_path,
                    std::fs::Permissions::from_mode(0o600),
                );
            }
            buf.to_vec()
        }
    };
    h.update(&fallback);

    let digest = h.finalize();
    let mut key = [0u8; 32];
    key.copy_from_slice(&digest);
    key
}

pub fn seal(plaintext: &str, db_path: &std::path::Path) -> Result<String> {
    let key_bytes = derive_machine_key(db_path);
    let key = Key::<Aes256Gcm>::from_slice(&key_bytes);
    let cipher = Aes256Gcm::new(key);

    let mut nonce_bytes = [0u8; 12];
    OsRng
        .try_fill_bytes(&mut nonce_bytes)
        .map_err(|e| anyhow::anyhow!("operating-system nonce generation failed: {e}"))?;
    let nonce = Nonce::from_slice(&nonce_bytes);

    let ct = cipher
        .encrypt(nonce, plaintext.as_bytes())
        .map_err(|e| anyhow::anyhow!("aes-gcm encrypt: {e}"))?;

    let sealed = Sealed {
        v: 1,
        nonce: B64.encode(nonce_bytes),
        ct: B64.encode(&ct),
    };
    let json = serde_json::to_string(&sealed).context("serialize sealed blob")?;
    Ok(B64.encode(json))
}

pub fn unseal(value: &str, db_path: &std::path::Path) -> Option<String> {
    let json = B64.decode(value).ok()?;
    let sealed: Sealed = serde_json::from_slice(&json).ok()?;
    if sealed.v != 1 {
        return None;
    }
    let nonce_bytes = B64.decode(sealed.nonce).ok()?;
    let ct = B64.decode(sealed.ct).ok()?;
    let key_bytes = derive_machine_key(db_path);
    let key = Key::<Aes256Gcm>::from_slice(&key_bytes);
    let cipher = Aes256Gcm::new(key);
    let pt = cipher
        .decrypt(Nonce::from_slice(&nonce_bytes), ct.as_ref())
        .ok()?;
    String::from_utf8(pt).ok()
}

pub fn set_token(db: &DashboardDb, token: &str, db_path: &std::path::Path) -> Result<()> {
    if token.is_empty() {
        db.conn
            .execute("DELETE FROM config WHERE key = ?1", params![KEY_TOKEN])?;
        return Ok(());
    }
    let sealed = seal(token, db_path)?;
    db.conn.execute(
        "INSERT INTO config (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![KEY_TOKEN, sealed],
    )?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenSource {
    Stored,

    GhCli,
}

pub fn get_token(db: &DashboardDb, db_path: &std::path::Path) -> Result<Option<String>> {
    let value: rusqlite::Result<String> = db.conn.query_row(
        "SELECT value FROM config WHERE key = ?1",
        params![KEY_TOKEN],
        |row| row.get(0),
    );
    let raw = match value {
        Ok(v) => v,
        Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    Ok(unseal(&raw, db_path))
}

#[must_use]
pub fn gh_cli_token() -> Option<String> {
    let out = std::process::Command::new("gh")
        .args(["auth", "token"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let tok = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if tok.is_empty() {
        None
    } else {
        Some(tok)
    }
}

pub fn get_effective_token(
    db: &DashboardDb,
    db_path: &std::path::Path,
) -> Result<Option<(String, TokenSource)>> {
    if let Some(stored) = get_token(db, db_path)? {
        return Ok(Some((stored, TokenSource::Stored)));
    }
    if let Some(gh) = gh_cli_token() {
        return Ok(Some((gh, TokenSource::GhCli)));
    }
    Ok(None)
}

pub fn set_plain(db: &DashboardDb, key: &str, value: Option<&str>) -> Result<()> {
    if let Some(v) = value {
        db.conn.execute(
            "INSERT INTO config (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, v],
        )?;
    } else {
        db.conn
            .execute("DELETE FROM config WHERE key = ?1", params![key])?;
    }
    Ok(())
}

pub fn get_plain(db: &DashboardDb, key: &str) -> Result<Option<String>> {
    match db.conn.query_row(
        "SELECT value FROM config WHERE key = ?1",
        params![key],
        |row| row.get(0),
    ) {
        Ok(v) => Ok(Some(v)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn open_db() -> (tempfile::TempDir, std::path::PathBuf, DashboardDb) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("dashboard.db");
        let db = DashboardDb::open(&path).unwrap();
        (dir, path, db)
    }

    #[test]
    fn test_seal_roundtrip() {
        let (_dir, path, _db) = open_db();
        let sealed = seal("ghp_test_token", &path).unwrap();
        assert_ne!(sealed, "ghp_test_token");
        let round = unseal(&sealed, &path).unwrap();
        assert_eq!(round, "ghp_test_token");
    }

    #[test]
    fn test_set_get_token() {
        let (_dir, path, db) = open_db();
        assert!(get_token(&db, &path).unwrap().is_none());
        set_token(&db, "ghp_xyz", &path).unwrap();
        assert_eq!(get_token(&db, &path).unwrap().as_deref(), Some("ghp_xyz"));
    }

    #[test]
    fn test_set_empty_token_deletes() {
        let (_dir, path, db) = open_db();
        set_token(&db, "ghp_xyz", &path).unwrap();
        set_token(&db, "", &path).unwrap();
        assert!(get_token(&db, &path).unwrap().is_none());
    }

    #[test]
    fn test_plain_config_roundtrip() {
        let (_dir, _path, db) = open_db();
        assert!(get_plain(&db, KEY_DEFAULT_ORG).unwrap().is_none());
        set_plain(&db, KEY_DEFAULT_ORG, Some("Corvidae-Coding-Projects")).unwrap();
        assert_eq!(
            get_plain(&db, KEY_DEFAULT_ORG).unwrap().as_deref(),
            Some("Corvidae-Coding-Projects")
        );
        set_plain(&db, KEY_DEFAULT_ORG, None).unwrap();
        assert!(get_plain(&db, KEY_DEFAULT_ORG).unwrap().is_none());
    }

    #[test]
    fn test_unseal_rejects_garbage() {
        let (_dir, path, _db) = open_db();
        assert!(unseal("not-base64!!", &path).is_none());
        assert!(unseal(&B64.encode("not-json"), &path).is_none());
    }
}
