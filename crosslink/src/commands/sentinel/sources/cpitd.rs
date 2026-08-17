use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::{DateTime, Utc};

use super::{Signal, SignalKind, Source, SourceKind};
use crate::commands::cpitd::ScanOutcome;
use crate::db::Database;

const LAST_SCAN_FILE: &str = "sentinel-cpitd-last-scan";

type ScanFn = Box<dyn FnMut(&Database, u32) -> Result<ScanOutcome> + Send>;

type AvailableFn = Box<dyn Fn() -> bool + Send>;

pub struct CpitdSource {
    interval_hours: u64,
    min_tokens: u32,
    db_path: PathBuf,
    state_file: PathBuf,
    available: AvailableFn,
    scan: ScanFn,
}

impl CpitdSource {
    pub fn new(crosslink_dir: &Path, interval_hours: u64, min_tokens: u32) -> Self {
        Self {
            interval_hours,
            min_tokens,
            db_path: crosslink_dir.join("issues.db"),
            state_file: crosslink_dir.join(LAST_SCAN_FILE),
            available: Box::new(crate::commands::cpitd::cpitd_available),
            scan: Box::new(|db, min_tokens| {
                crate::commands::cpitd::scan_and_file(db, &[], min_tokens, &[])
            }),
        }
    }

    fn last_scan(&self) -> Option<DateTime<Utc>> {
        let content = std::fs::read_to_string(&self.state_file).ok()?;
        DateTime::parse_from_rfc3339(content.trim())
            .ok()
            .map(|dt| dt.with_timezone(&Utc))
    }

    fn record_scan(&self, when: DateTime<Utc>) {
        if let Err(e) = std::fs::write(&self.state_file, when.to_rfc3339()) {
            tracing::warn!("failed to persist cpitd last-scan timestamp: {e}");
        }
    }

    fn interval_elapsed(&self, now: DateTime<Utc>) -> bool {
        self.last_scan().is_none_or(|last| {
            now.signed_duration_since(last).num_hours() >= self.interval_hours as i64
        })
    }

    fn outcome_to_signals(outcome: &ScanOutcome, now: DateTime<Utc>) -> Vec<Signal> {
        outcome
            .created
            .iter()
            .map(|(issue_id, file_a, file_b)| Signal {
                source: SourceKind::Internal,
                kind: SignalKind::CodeClone,

                reference: format!("CPITD:CL#{issue_id}"),
                title: format!("Code clone detected: {file_a} <-> {file_b}"),
                body: format!(
                    "cpitd detected duplicated code between `{file_a}` and `{file_b}`. \
                     Filed as crosslink issue #{issue_id} (label: cpitd). \
                     Consider extracting the shared logic."
                ),
                metadata: serde_json::json!({
                    "type": "code_clone",
                    "issue_id": issue_id,
                    "file_a": file_a,
                    "file_b": file_b,
                }),
                detected_at: now,
            })
            .collect()
    }

    fn poll_at(&mut self, now: DateTime<Utc>) -> Result<Vec<Signal>> {
        if !(self.available)() {
            tracing::debug!(
                "cpitd binary not on PATH; skipping clone scan (install guidance: crosslink init)"
            );
            return Ok(Vec::new());
        }

        if !self.interval_elapsed(now) {
            tracing::debug!("cpitd scan interval not elapsed; skipping");
            return Ok(Vec::new());
        }

        let db = Database::open(&self.db_path)?;
        let outcome = (self.scan)(&db, self.min_tokens)?;
        self.record_scan(now);

        Ok(Self::outcome_to_signals(&outcome, now))
    }
}

impl Source for CpitdSource {
    fn name(&self) -> &'static str {
        "cpitd"
    }

    fn poll(&mut self) -> Result<Vec<Signal>> {
        self.poll_at(Utc::now())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_source(dir: &Path, interval_hours: u64, available: bool, scan: ScanFn) -> CpitdSource {
        CpitdSource {
            interval_hours,
            min_tokens: 50,
            db_path: dir.join("issues.db"),
            state_file: dir.join(LAST_SCAN_FILE),
            available: Box::new(move || available),
            scan,
        }
    }

    fn tmpdir() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "cpitd-src-test-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn binary_absent_yields_no_signals_no_error() {
        let dir = tmpdir();
        let scan_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let sc = scan_count.clone();
        let scan: ScanFn = Box::new(move |_db, _mt| {
            sc.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(ScanOutcome::default())
        });
        let mut src = test_source(&dir, 168, false, scan);

        let signals = src.poll_at(Utc::now()).unwrap();
        assert!(signals.is_empty());

        assert_eq!(scan_count.load(std::sync::atomic::Ordering::SeqCst), 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn interval_not_elapsed_skips_scan() {
        let dir = tmpdir();
        let now = Utc::now();

        std::fs::write(
            dir.join(LAST_SCAN_FILE),
            (now - chrono::Duration::hours(1)).to_rfc3339(),
        )
        .unwrap();

        let scanned = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let s = scanned.clone();
        let scan: ScanFn = Box::new(move |_db, _mt| {
            s.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(ScanOutcome::default())
        });
        let mut src = test_source(&dir, 168, true, scan);

        let signals = src.poll_at(now).unwrap();
        assert!(signals.is_empty());
        assert!(!scanned.load(std::sync::atomic::Ordering::SeqCst));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn interval_elapsed_invokes_scan_and_maps_signals() {
        let dir = tmpdir();
        let now = Utc::now();

        std::fs::write(
            dir.join(LAST_SCAN_FILE),
            (now - chrono::Duration::hours(200)).to_rfc3339(),
        )
        .unwrap();

        let scan: ScanFn = Box::new(|_db, _mt| {
            Ok(ScanOutcome {
                created: vec![
                    (101, "src/a.rs".to_string(), "src/b.rs".to_string()),
                    (102, "src/c.rs".to_string(), "src/d.rs".to_string()),
                ],
                updated: vec![55],
            })
        });
        let mut src = test_source(&dir, 168, true, scan);

        let signals = src.poll_at(now).unwrap();

        assert_eq!(signals.len(), 2);
        assert_eq!(signals[0].kind, SignalKind::CodeClone);
        assert_eq!(signals[0].reference, "CPITD:CL#101");
        assert_eq!(signals[1].reference, "CPITD:CL#102");
        assert!(signals[0].title.contains("src/a.rs"));

        assert!(dir.join(LAST_SCAN_FILE).exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn never_scanned_is_elapsed() {
        let dir = tmpdir();
        let scan: ScanFn = Box::new(|_db, _mt| Ok(ScanOutcome::default()));
        let src = test_source(&dir, 168, true, scan);
        assert!(src.interval_elapsed(Utc::now()));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn outcome_to_signals_uses_stable_reference_for_dedup() {
        let now = Utc::now();
        let outcome = ScanOutcome {
            created: vec![(777, "x.rs".to_string(), "y.rs".to_string())],
            updated: vec![],
        };
        let s1 = CpitdSource::outcome_to_signals(&outcome, now);
        let s2 = CpitdSource::outcome_to_signals(&outcome, now);
        assert_eq!(s1[0].reference, s2[0].reference);
        assert_eq!(s1[0].reference, "CPITD:CL#777");
    }
}
