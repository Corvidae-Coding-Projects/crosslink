use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::db::SCHEMA_VERSION;

pub mod migration;
pub mod publication;
pub mod readiness;

const LEGACY_LOCKS_REF: &str = "refs/heads/crosslink/locks";
const V2_HUB_REF: &str = "refs/heads/crosslink/hub";
const HIDDEN_META_REF: &str = "refs/crosslink/meta";
const HIDDEN_CHECKPOINT_REF: &str = "refs/crosslink/checkpoint";
const HIDDEN_AGENT_PREFIX: &str = "refs/crosslink/agents/";
const VISIBLE_META_REF: &str = "refs/heads/crosslink/meta";
const VISIBLE_CHECKPOINT_REF: &str = "refs/heads/crosslink/checkpoint";
const VISIBLE_AGENT_PREFIX: &str = "refs/heads/crosslink/agents/";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LocalDatabaseFormat {
    Missing,
    Sqlite {
        version: i32,
        schema_fingerprint: String,
        issue_count: Option<u64>,
        size_bytes: u64,
    },
    Future {
        version: i32,
        supported_version: i32,
        schema_fingerprint: String,
        size_bytes: u64,
    },
    Unreadable {
        reason: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SharedStoreFamily {
    LegacyLocks,
    V2,
    HiddenV3,
    VisibleV3,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SharedStoreFormat {
    Absent,
    LegacyLocks {
        refs: Vec<String>,
    },
    V2 {
        refs: Vec<String>,
    },
    HiddenV3 {
        refs: Vec<String>,
    },
    VisibleV3 {
        refs: Vec<String>,
    },
    Mixed {
        families: Vec<SharedStoreFamily>,
        refs: Vec<String>,
    },
    Unreadable {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryFormat {
    pub local_database: LocalDatabaseFormat,
    pub shared_store: SharedStoreFormat,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum MigrationAction {
    CreateLocalDatabase,
    MigrateLocalDatabase { from: i32, to: i32 },
    InitializeSharedStore,
    ImportLocalOnly { issue_count: u64 },
    ImportLegacyLocks,
    ImportV2,
    RenameHiddenV3Refs,
    ResolveMixedSharedStore,
    EstablishReconciliationGeneration,
    ResumeReconciliation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadinessState {
    ReadyCurrent,
    MigrationRequired,
    BlockedCorrupt,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationPlan {
    pub state: ReadinessState,
    pub actions: Vec<MigrationAction>,
    pub blockers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SemanticSnapshot {
    pub issues: BTreeMap<String, Value>,
    pub comments: BTreeMap<String, Value>,
    pub milestones: BTreeMap<String, Value>,
    pub locks: BTreeMap<String, Value>,
    pub trust: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemanticComparison {
    pub equivalent: bool,
    pub differing_sections: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconcileReport {
    pub repository_root: PathBuf,
    pub format: RepositoryFormat,
    pub plan: MigrationPlan,
}

impl ReconcileReport {
    #[must_use]
    pub fn text(&self) -> String {
        let mut lines = vec![
            format!("Repository: {}", self.repository_root.display()),
            format!("Readiness: {:?}", self.plan.state),
            format!("Local database: {:?}", self.format.local_database),
            format!("Shared store: {:?}", self.format.shared_store),
        ];
        if self.plan.actions.is_empty() {
            lines.push("Actions: none".to_string());
        } else {
            lines.push("Actions:".to_string());
            lines.extend(
                self.plan
                    .actions
                    .iter()
                    .map(|action| format!("  - {action:?}")),
            );
        }
        if !self.plan.blockers.is_empty() {
            lines.push("Blockers:".to_string());
            lines.extend(
                self.plan
                    .blockers
                    .iter()
                    .map(|blocker| format!("  - {blocker}")),
            );
        }
        lines.join("\n")
    }
}

pub fn check_repository(crosslink_dir: &Path) -> ReconcileReport {
    let repository_root = crosslink_dir
        .parent()
        .map_or_else(|| crosslink_dir.to_path_buf(), Path::to_path_buf);
    let local_database = detect_local_database(&crosslink_dir.join("issues.db"));
    let shared_store = detect_shared_store(&repository_root);
    let format = RepositoryFormat {
        local_database,
        shared_store,
    };
    let plan = plan_repository_reconciliation(&repository_root, crosslink_dir, &format);
    ReconcileReport {
        repository_root,
        format,
        plan,
    }
}

fn plan_repository_reconciliation(
    repository_root: &Path,
    crosslink_dir: &Path,
    format: &RepositoryFormat,
) -> MigrationPlan {
    let mut plan = plan_migration(format);
    if !matches!(format.shared_store, SharedStoreFormat::VisibleV3 { .. }) {
        return plan;
    }
    if crosslink_dir.join("reconciliation-journal.json").exists() {
        plan.actions.push(MigrationAction::ResumeReconciliation);
    } else {
        match publication::generation_id_at_ref(repository_root, publication::GENERATION_REF) {
            Ok(Some(_)) => {}
            Ok(None) => plan
                .actions
                .push(MigrationAction::EstablishReconciliationGeneration),
            Err(error) => plan.blockers.push(format!(
                "reconciliation generation is unreadable: {error:#}"
            )),
        }
    }
    plan.state = if plan.blockers.is_empty() {
        if plan.actions.is_empty() {
            ReadinessState::ReadyCurrent
        } else {
            ReadinessState::MigrationRequired
        }
    } else {
        ReadinessState::BlockedCorrupt
    };
    plan
}

#[must_use]
pub fn plan_migration(format: &RepositoryFormat) -> MigrationPlan {
    let mut actions = Vec::new();
    let mut blockers = Vec::new();
    let mut local_issue_count = 0;

    match &format.local_database {
        LocalDatabaseFormat::Missing => actions.push(MigrationAction::CreateLocalDatabase),
        LocalDatabaseFormat::Sqlite {
            version,
            issue_count,
            ..
        } => {
            local_issue_count = issue_count.unwrap_or(0);
            if *version < SCHEMA_VERSION {
                actions.push(MigrationAction::MigrateLocalDatabase {
                    from: *version,
                    to: SCHEMA_VERSION,
                });
            }
        }
        LocalDatabaseFormat::Future {
            version,
            supported_version,
            ..
        } => blockers.push(format!(
            "local database version {version} is newer than supported version {supported_version}"
        )),
        LocalDatabaseFormat::Unreadable { reason } => {
            blockers.push(format!("local database is unreadable: {reason}"));
        }
    }

    match &format.shared_store {
        SharedStoreFormat::Absent if local_issue_count > 0 => {
            actions.push(MigrationAction::ImportLocalOnly {
                issue_count: local_issue_count,
            });
        }
        SharedStoreFormat::Absent => actions.push(MigrationAction::InitializeSharedStore),
        SharedStoreFormat::LegacyLocks { .. } => {
            actions.push(MigrationAction::ImportLegacyLocks);
        }
        SharedStoreFormat::V2 { .. } => actions.push(MigrationAction::ImportV2),
        SharedStoreFormat::HiddenV3 { .. } => {
            actions.push(MigrationAction::RenameHiddenV3Refs);
        }
        SharedStoreFormat::VisibleV3 { .. } => {}
        SharedStoreFormat::Mixed { .. } => {
            actions.push(MigrationAction::ResolveMixedSharedStore);
        }
        SharedStoreFormat::Unreadable { reason } => {
            blockers.push(format!("shared store is unreadable: {reason}"));
        }
    }

    let state = if blockers.is_empty() {
        if actions.is_empty() {
            ReadinessState::ReadyCurrent
        } else {
            ReadinessState::MigrationRequired
        }
    } else {
        ReadinessState::BlockedCorrupt
    };

    MigrationPlan {
        state,
        actions,
        blockers,
    }
}

#[must_use]
pub fn compare_semantic_snapshots(
    left: &SemanticSnapshot,
    right: &SemanticSnapshot,
) -> SemanticComparison {
    let sections = [
        ("issues", left.issues != right.issues),
        ("comments", left.comments != right.comments),
        ("milestones", left.milestones != right.milestones),
        ("locks", left.locks != right.locks),
        ("trust", left.trust != right.trust),
    ];
    let differing_sections = sections
        .into_iter()
        .filter(|(_, differs)| *differs)
        .map(|(name, _)| name.to_string())
        .collect::<Vec<_>>();
    SemanticComparison {
        equivalent: differing_sections.is_empty(),
        differing_sections,
    }
}

fn detect_local_database(path: &Path) -> LocalDatabaseFormat {
    if !path.exists() {
        return LocalDatabaseFormat::Missing;
    }

    let size_bytes = match std::fs::metadata(path) {
        Ok(metadata) => metadata.len(),
        Err(error) => {
            return LocalDatabaseFormat::Unreadable {
                reason: format!("failed to inspect {}: {error}", path.display()),
            };
        }
    };

    match inspect_local_database(path, size_bytes) {
        Ok(format) => format,
        Err(error) => LocalDatabaseFormat::Unreadable {
            reason: format!("{error:#}"),
        },
    }
}

fn inspect_local_database(path: &Path, size_bytes: u64) -> Result<LocalDatabaseFormat> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("failed to open {} read-only", path.display()))?;
    let quick_check: String = connection
        .query_row("PRAGMA quick_check(1)", [], |row| row.get(0))
        .context("failed to run SQLite quick_check")?;
    anyhow::ensure!(
        quick_check == "ok",
        "SQLite quick_check reported {quick_check}"
    );

    let version: i32 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .context("failed to read PRAGMA user_version")?;
    let schema_fingerprint = schema_fingerprint(&connection)?;
    let issue_count = if table_exists(&connection, "issues")? {
        let count: i64 = connection
            .query_row("SELECT COUNT(*) FROM issues", [], |row| row.get(0))
            .context("failed to count local issues")?;
        Some(u64::try_from(count).context("local issue count cannot be negative")?)
    } else {
        None
    };

    if version > SCHEMA_VERSION {
        Ok(LocalDatabaseFormat::Future {
            version,
            supported_version: SCHEMA_VERSION,
            schema_fingerprint,
            size_bytes,
        })
    } else if version < 0 {
        anyhow::bail!("database schema version cannot be negative: {version}")
    } else {
        Ok(LocalDatabaseFormat::Sqlite {
            version,
            schema_fingerprint,
            issue_count,
            size_bytes,
        })
    }
}

fn schema_fingerprint(connection: &Connection) -> Result<String> {
    let mut statement = connection.prepare(
        "SELECT type, name, tbl_name, COALESCE(sql, '') FROM sqlite_schema \
         WHERE name NOT LIKE 'sqlite_%' ORDER BY type, name",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;
    let mut hash = Sha256::new();
    for row in rows {
        let (object_type, name, table, sql) = row?;
        for field in [&object_type, &name, &table, &sql] {
            hash.update(field.as_bytes());
            hash.update([0]);
        }
        hash.update(b"\n");
    }
    Ok(hex::encode(hash.finalize()))
}

fn table_exists(connection: &Connection, table: &str) -> Result<bool> {
    connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = ?1)",
            [table],
            |row| row.get(0),
        )
        .context("failed to inspect SQLite tables")
}

fn detect_shared_store(repository_root: &Path) -> SharedStoreFormat {
    match read_crosslink_refs(repository_root) {
        Ok(refs) => classify_shared_store(&refs),
        Err(error) => SharedStoreFormat::Unreadable {
            reason: format!("{error:#}"),
        },
    }
}

fn read_crosslink_refs(repository_root: &Path) -> Result<BTreeSet<String>> {
    let output = Command::new("git")
        .current_dir(repository_root)
        .args([
            "for-each-ref",
            "--format=%(refname)",
            "refs/crosslink/",
            "refs/heads/crosslink/",
        ])
        .output()
        .with_context(|| {
            format!(
                "failed to inspect Git refs in {}",
                repository_root.display()
            )
        })?;
    anyhow::ensure!(
        output.status.success(),
        "git for-each-ref failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect())
}

#[must_use]
pub fn classify_shared_store(refs: &BTreeSet<String>) -> SharedStoreFormat {
    let legacy_locks = refs.contains(LEGACY_LOCKS_REF);
    let v2 = refs.contains(V2_HUB_REF);
    let hidden_refs: Vec<String> = refs
        .iter()
        .filter(|name| {
            name.as_str() == HIDDEN_META_REF
                || name.as_str() == HIDDEN_CHECKPOINT_REF
                || name.starts_with(HIDDEN_AGENT_PREFIX)
        })
        .cloned()
        .collect();
    let visible_refs: Vec<String> = refs
        .iter()
        .filter(|name| {
            name.as_str() == VISIBLE_META_REF
                || name.as_str() == VISIBLE_CHECKPOINT_REF
                || name.starts_with(VISIBLE_AGENT_PREFIX)
        })
        .cloned()
        .collect();

    let hidden_complete = hidden_refs.iter().any(|name| name == HIDDEN_META_REF)
        && hidden_refs.iter().any(|name| name == HIDDEN_CHECKPOINT_REF);
    let visible_complete = visible_refs.iter().any(|name| name == VISIBLE_META_REF)
        && visible_refs
            .iter()
            .any(|name| name == VISIBLE_CHECKPOINT_REF);

    let mut families = BTreeSet::new();
    if legacy_locks {
        families.insert(SharedStoreFamily::LegacyLocks);
    }
    if v2 {
        families.insert(SharedStoreFamily::V2);
    }
    if !hidden_refs.is_empty() {
        families.insert(SharedStoreFamily::HiddenV3);
    }
    if !visible_refs.is_empty() {
        families.insert(SharedStoreFamily::VisibleV3);
    }

    let relevant_refs: Vec<String> = refs
        .iter()
        .filter(|name| {
            name.as_str() == LEGACY_LOCKS_REF
                || name.as_str() == V2_HUB_REF
                || hidden_refs.contains(name)
                || visible_refs.contains(name)
        })
        .cloned()
        .collect();

    match families.iter().copied().collect::<Vec<_>>().as_slice() {
        [] => SharedStoreFormat::Absent,
        [SharedStoreFamily::LegacyLocks] => SharedStoreFormat::LegacyLocks {
            refs: relevant_refs,
        },
        [SharedStoreFamily::V2] => SharedStoreFormat::V2 {
            refs: relevant_refs,
        },
        [SharedStoreFamily::HiddenV3] if hidden_complete => SharedStoreFormat::HiddenV3 {
            refs: relevant_refs,
        },
        [SharedStoreFamily::VisibleV3] if visible_complete => SharedStoreFormat::VisibleV3 {
            refs: relevant_refs,
        },
        _ => SharedStoreFormat::Mixed {
            families: families.into_iter().collect(),
            refs: relevant_refs,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;
    use std::collections::BTreeMap;

    #[derive(Deserialize)]
    struct FixtureManifest {
        sqlite: Vec<SqliteFixture>,
        git: Vec<GitFixture>,
    }

    #[derive(Deserialize)]
    struct SqliteFixture {
        file: String,
        version: i32,
        source: String,
    }

    #[derive(Deserialize)]
    struct GitFixture {
        file: String,
        expected_kind: String,
    }

    fn refs(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|name| (*name).to_string()).collect()
    }

    fn fixture_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/reconcile")
    }

    fn fixture_manifest() -> FixtureManifest {
        serde_json::from_str(include_str!("../tests/fixtures/reconcile/manifest.json")).unwrap()
    }

    fn create_fixture_database(sql_path: &Path, database_path: &Path) {
        let sql = std::fs::read_to_string(sql_path).unwrap();
        let connection = Connection::open(database_path).unwrap();
        connection.execute_batch(&sql).unwrap();
    }

    fn schema_contract(connection: &Connection) -> BTreeMap<String, Vec<String>> {
        let mut statement = connection
            .prepare(
                "SELECT type, name FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%' \
                 AND type IN ('table', 'index') ORDER BY type, name",
            )
            .unwrap();
        let objects: Vec<(String, String)> = statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        drop(statement);

        let mut contract = BTreeMap::new();
        for (object_type, name) in objects {
            let key = format!("{object_type}:{name}");
            if object_type == "table" {
                let mut columns = connection
                    .prepare(&format!("PRAGMA table_info('{name}')"))
                    .unwrap();
                let mut values = columns
                    .query_map([], |row| {
                        Ok(format!(
                            "{}|{}|{}|{}|{}",
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, i32>(3)?,
                            row.get::<_, Option<String>>(4)?.unwrap_or_default(),
                            row.get::<_, i32>(5)?,
                        ))
                    })
                    .unwrap()
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .unwrap();
                values.sort();
                contract.insert(key, values);
            } else {
                contract.insert(key, Vec::new());
            }
        }
        contract
    }

    fn shared_kind(format: &SharedStoreFormat) -> &'static str {
        match format {
            SharedStoreFormat::Absent => "absent",
            SharedStoreFormat::LegacyLocks { .. } => "legacy_locks",
            SharedStoreFormat::V2 { .. } => "v2",
            SharedStoreFormat::HiddenV3 { .. } => "hidden_v3",
            SharedStoreFormat::VisibleV3 { .. } => "visible_v3",
            SharedStoreFormat::Mixed { .. } => "mixed",
            SharedStoreFormat::Unreadable { .. } => "unreadable",
        }
    }

    #[test]
    fn classifies_all_shared_store_families() {
        assert_eq!(
            classify_shared_store(&BTreeSet::new()),
            SharedStoreFormat::Absent
        );
        assert!(matches!(
            classify_shared_store(&refs(&[LEGACY_LOCKS_REF])),
            SharedStoreFormat::LegacyLocks { .. }
        ));
        assert!(matches!(
            classify_shared_store(&refs(&[V2_HUB_REF])),
            SharedStoreFormat::V2 { .. }
        ));
        assert!(matches!(
            classify_shared_store(&refs(&[HIDDEN_META_REF, HIDDEN_CHECKPOINT_REF])),
            SharedStoreFormat::HiddenV3 { .. }
        ));
        assert!(matches!(
            classify_shared_store(&refs(&[VISIBLE_META_REF, VISIBLE_CHECKPOINT_REF])),
            SharedStoreFormat::VisibleV3 { .. }
        ));
    }

    #[test]
    fn incomplete_or_overlapping_ref_families_are_mixed() {
        assert!(matches!(
            classify_shared_store(&refs(&[VISIBLE_META_REF])),
            SharedStoreFormat::Mixed { .. }
        ));
        assert!(matches!(
            classify_shared_store(&refs(&[
                V2_HUB_REF,
                VISIBLE_META_REF,
                VISIBLE_CHECKPOINT_REF,
            ])),
            SharedStoreFormat::Mixed { .. }
        ));
    }

    #[test]
    fn ignores_unrelated_crosslink_refs() {
        assert_eq!(
            classify_shared_store(&refs(&[
                "refs/heads/crosslink/knowledge",
                "refs/heads/crosslink/hub-v3-host",
            ])),
            SharedStoreFormat::Absent
        );
    }

    #[test]
    fn semantic_comparison_names_changed_sections() {
        let left = SemanticSnapshot::default();
        let mut right = SemanticSnapshot::default();
        right.issues.insert(
            "issue-1".to_string(),
            serde_json::json!({"title": "changed"}),
        );
        let comparison = compare_semantic_snapshots(&left, &right);
        assert!(!comparison.equivalent);
        assert_eq!(comparison.differing_sections, vec!["issues"]);
    }

    #[test]
    fn immutable_git_fixtures_cover_every_shared_store_family() {
        let manifest = fixture_manifest();
        let root = fixture_root();
        let mut actual_kinds = BTreeSet::new();
        for fixture in manifest.git {
            let content = std::fs::read_to_string(root.join(&fixture.file)).unwrap();
            let fixture_refs: BTreeSet<String> = content
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_string)
                .collect();
            let format = classify_shared_store(&fixture_refs);
            assert_eq!(
                shared_kind(&format),
                fixture.expected_kind,
                "fixture {} classified as {format:?}",
                fixture.file
            );
            actual_kinds.insert(fixture.expected_kind);
        }
        assert_eq!(
            actual_kinds,
            refs(&[
                "absent",
                "legacy_locks",
                "v2",
                "hidden_v3",
                "visible_v3",
                "mixed",
            ])
        );
    }

    #[test]
    fn every_sqlite_fixture_is_detected_read_only_and_migrates_to_current() {
        let manifest = fixture_manifest();
        let root = fixture_root();
        let fresh_dir = tempfile::tempdir().unwrap();
        let fresh_path = fresh_dir.path().join("fresh.db");
        let fresh = Database::open(&fresh_path).unwrap();
        let expected_contract = schema_contract(&fresh.conn);
        drop(fresh);

        let versions: BTreeSet<i32> = manifest.sqlite.iter().map(|entry| entry.version).collect();
        assert_eq!(versions, (0..=SCHEMA_VERSION).collect());

        for fixture in manifest.sqlite {
            assert!(!fixture.source.is_empty());
            let directory = tempfile::tempdir().unwrap();
            let database_path = directory.path().join("issues.db");
            create_fixture_database(&root.join(&fixture.file), &database_path);

            let before_check = std::fs::read(&database_path).unwrap();
            let detected = detect_local_database(&database_path);
            let after_check = std::fs::read(&database_path).unwrap();
            assert_eq!(
                before_check, after_check,
                "read-only detection changed {}",
                fixture.file
            );
            assert!(
                matches!(detected, LocalDatabaseFormat::Sqlite { version, .. } if version == fixture.version),
                "unexpected detection for {}: {detected:?}",
                fixture.file
            );

            let database = Database::open(&database_path)
                .unwrap_or_else(|error| panic!("{} failed migration: {error:#}", fixture.file));
            assert_eq!(database.get_schema_version().unwrap(), SCHEMA_VERSION);
            assert_eq!(
                schema_contract(&database.conn),
                expected_contract,
                "schema contract mismatch after migrating {}",
                fixture.file
            );
            let foreign_key_error_count: i64 = database
                .conn
                .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(foreign_key_error_count, 0, "{}", fixture.file);
            if fixture.file != "sqlite/v00-empty.sql" {
                let title: String = database
                    .conn
                    .query_row("SELECT title FROM issues WHERE id = 1", [], |row| {
                        row.get(0)
                    })
                    .unwrap();
                assert!(title.starts_with("fixture-v"), "{}", fixture.file);
            }
        }
    }

    #[test]
    fn migration_failure_rolls_back_schema_and_version() {
        let root = fixture_root();
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("issues.db");
        create_fixture_database(&root.join("sqlite/v17.sql"), &database_path);
        let connection = Connection::open(&database_path).unwrap();
        connection
            .execute_batch("CREATE TABLE idx_token_usage_provider (value INTEGER);")
            .unwrap();
        drop(connection);

        let Err(error) = Database::open(&database_path) else {
            panic!("migration unexpectedly succeeded");
        };
        assert!(error.to_string().contains("migration v18"));

        let connection = Connection::open(&database_path).unwrap();
        let version: i32 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 17);
        let provider_exists: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('token_usage') WHERE name = 'provider')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!provider_exists);
    }

    #[test]
    fn current_database_open_is_a_byte_for_byte_no_op() {
        let root = fixture_root();
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("issues.db");
        create_fixture_database(&root.join("sqlite/v18.sql"), &database_path);
        let before = std::fs::read(&database_path).unwrap();
        let database = Database::open(&database_path).unwrap();
        drop(database);
        let after = std::fs::read(&database_path).unwrap();
        assert_eq!(before, after);
    }
}
