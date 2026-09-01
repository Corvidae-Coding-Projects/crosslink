use anyhow::{Context, Result};
use rusqlite::{Connection, Transaction};
use std::path::Path;

pub const SCHEMA_VERSION: i32 = 18;

pub const VALID_PRIORITIES: &[&str] = &["low", "medium", "high", "critical"];

pub const VALID_STATUSES: &[&str] = &["open", "closed", "archived"];

pub const MAX_TITLE_LEN: usize = 512;
pub const MAX_LABEL_LEN: usize = 128;
pub const MAX_DESCRIPTION_LEN: usize = 64 * 1024;
pub const MAX_COMMENT_LEN: usize = 1024 * 1024;

pub fn validate_issue_title(title: &str) -> Result<()> {
    if title.len() > MAX_TITLE_LEN {
        anyhow::bail!("Title exceeds maximum length of {MAX_TITLE_LEN} characters");
    }
    Ok(())
}

pub fn validate_issue_description(description: Option<&str>) -> Result<()> {
    if description.is_some_and(|value| value.len() > MAX_DESCRIPTION_LEN) {
        anyhow::bail!("Description exceeds maximum length of {MAX_DESCRIPTION_LEN} bytes");
    }
    Ok(())
}

pub fn validate_status(status: &str) -> Result<()> {
    if VALID_STATUSES.contains(&status) {
        Ok(())
    } else {
        anyhow::bail!(
            "Invalid status '{}'. Valid values: {}",
            status,
            VALID_STATUSES.join(", ")
        )
    }
}

pub fn validate_priority(priority: &str) -> Result<()> {
    if VALID_PRIORITIES.contains(&priority) {
        Ok(())
    } else {
        anyhow::bail!(
            "Invalid priority '{}'. Valid values: {}",
            priority,
            VALID_PRIORITIES.join(", ")
        )
    }
}

pub struct Database {
    pub(crate) conn: Connection,
}

impl Database {
    pub fn open_ephemeral() -> Result<Self> {
        let conn = Connection::open_in_memory().context("Failed to open ephemeral database")?;
        let db = Self { conn };
        db.init_schema()?;
        Ok(db)
    }

    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path).context("Failed to open database")?;
        let db = Self { conn };
        db.init_schema()?;
        Ok(db)
    }

    pub fn open_read_only(path: &Path) -> Result<Self> {
        let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .context("Failed to open database read-only")?;
        let db = Self { conn };
        let version = db.get_schema_version()?;
        anyhow::ensure!(
            version <= SCHEMA_VERSION,
            "database schema version {version} is newer than supported version {SCHEMA_VERSION}"
        );
        Ok(db)
    }

    pub(crate) fn open_without_migrations(path: &Path) -> Result<Self> {
        let conn = Connection::open(path).context("Failed to open database")?;
        let db = Self { conn };
        let version = db.get_schema_version()?;
        anyhow::ensure!(
            version >= 0,
            "database schema version cannot be negative: {version}"
        );
        anyhow::ensure!(
            version <= SCHEMA_VERSION,
            "database schema version {version} is newer than supported version {SCHEMA_VERSION}"
        );
        Ok(db)
    }

    pub(crate) fn transaction_with_schema_upgrade<T, F>(&self, apply: F) -> Result<T>
    where
        F: FnOnce() -> Result<T>,
    {
        self.conn.pragma_update(None, "foreign_keys", false)?;
        let version = self.get_schema_version()?;
        let transaction = self
            .conn
            .unchecked_transaction()
            .context("failed to start atomic schema and projection transaction")?;
        let result = (|| {
            for target in (version + 1)..=SCHEMA_VERSION {
                Self::apply_migration(&transaction, target)
                    .with_context(|| format!("database migration v{target} failed"))?;
                transaction
                    .pragma_update(None, "user_version", target)
                    .with_context(|| format!("failed to record database migration v{target}"))?;
            }
            apply()
        })();
        match result {
            Ok(value) => {
                transaction
                    .commit()
                    .context("failed to commit atomic schema and projection transaction")?;
                if let Err(error) = self.conn.pragma_update(None, "foreign_keys", true) {
                    tracing::warn!("failed to re-enable foreign keys after committed projection install: {error}");
                }
                Ok(value)
            }
            Err(error) => {
                drop(transaction);
                if let Err(enable_error) = self.conn.pragma_update(None, "foreign_keys", true) {
                    tracing::warn!("failed to re-enable foreign keys after rolled back projection install: {enable_error}");
                }
                Err(error)
            }
        }
    }

    pub fn transaction<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce() -> Result<T>,
    {
        let tx = self.conn.unchecked_transaction()?;
        let result = f()?;
        tx.commit()?;
        Ok(result)
    }

    pub fn set_foreign_keys(&self, enabled: bool) -> Result<()> {
        let value = if enabled { "ON" } else { "OFF" };
        self.conn
            .execute_batch(&format!("PRAGMA foreign_keys = {value};"))?;
        Ok(())
    }

    fn init_schema(&self) -> Result<()> {
        let version = self.get_schema_version()?;
        anyhow::ensure!(
            version >= 0,
            "database schema version cannot be negative: {version}"
        );
        anyhow::ensure!(
            version <= SCHEMA_VERSION,
            "database schema version {version} is newer than supported version {SCHEMA_VERSION}"
        );
        self.run_migrations(version)?;
        self.conn.pragma_update(None, "foreign_keys", true)?;
        Ok(())
    }

    fn run_migrations(&self, version: i32) -> Result<()> {
        for target in (version + 1)..=SCHEMA_VERSION {
            self.run_migration(target)?;
        }
        Ok(())
    }

    fn run_migration(&self, target: i32) -> Result<()> {
        let tx = self
            .conn
            .unchecked_transaction()
            .with_context(|| format!("failed to start database migration v{target}"))?;
        Self::apply_migration(&tx, target)
            .with_context(|| format!("database migration v{target} failed"))?;
        tx.pragma_update(None, "user_version", target)
            .with_context(|| format!("failed to record database migration v{target}"))?;
        tx.commit()
            .with_context(|| format!("failed to commit database migration v{target}"))?;
        Ok(())
    }

    fn apply_migration(tx: &Transaction<'_>, target: i32) -> Result<()> {
        match target {
            1 => tx.execute_batch(
                r"
                CREATE TABLE IF NOT EXISTS issues (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    title TEXT NOT NULL,
                    description TEXT,
                    status TEXT NOT NULL DEFAULT 'open',
                    priority TEXT NOT NULL DEFAULT 'medium',
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL,
                    closed_at TEXT
                );
                CREATE TABLE IF NOT EXISTS labels (
                    issue_id INTEGER NOT NULL,
                    label TEXT NOT NULL,
                    PRIMARY KEY (issue_id, label),
                    FOREIGN KEY (issue_id) REFERENCES issues(id) ON DELETE CASCADE
                );
                CREATE TABLE IF NOT EXISTS dependencies (
                    blocker_id INTEGER NOT NULL,
                    blocked_id INTEGER NOT NULL,
                    PRIMARY KEY (blocker_id, blocked_id),
                    FOREIGN KEY (blocker_id) REFERENCES issues(id) ON DELETE CASCADE,
                    FOREIGN KEY (blocked_id) REFERENCES issues(id) ON DELETE CASCADE
                );
                CREATE TABLE IF NOT EXISTS comments (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    issue_id INTEGER NOT NULL,
                    content TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    FOREIGN KEY (issue_id) REFERENCES issues(id) ON DELETE CASCADE
                );
                CREATE TABLE IF NOT EXISTS sessions (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    started_at TEXT NOT NULL,
                    ended_at TEXT,
                    active_issue_id INTEGER,
                    handoff_notes TEXT,
                    FOREIGN KEY (active_issue_id) REFERENCES issues(id)
                );
                CREATE INDEX IF NOT EXISTS idx_issues_status ON issues(status);
                CREATE INDEX IF NOT EXISTS idx_issues_priority ON issues(priority);
                CREATE INDEX IF NOT EXISTS idx_labels_issue ON labels(issue_id);
                CREATE INDEX IF NOT EXISTS idx_comments_issue ON comments(issue_id);
                CREATE INDEX IF NOT EXISTS idx_deps_blocker ON dependencies(blocker_id);
                CREATE INDEX IF NOT EXISTS idx_deps_blocked ON dependencies(blocked_id);
                ",
            )?,
            2 => {
                Self::add_column_if_missing(
                    tx,
                    "issues",
                    "parent_id",
                    "ALTER TABLE issues ADD COLUMN parent_id INTEGER REFERENCES issues(id) ON DELETE CASCADE",
                )?;
                tx.execute_batch(
                    "CREATE INDEX IF NOT EXISTS idx_issues_parent ON issues(parent_id);",
                )?;
            }
            3 => tx.execute_batch(
                r"
                CREATE TABLE IF NOT EXISTS time_entries (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    issue_id INTEGER NOT NULL,
                    started_at TEXT NOT NULL,
                    ended_at TEXT,
                    duration_seconds INTEGER,
                    FOREIGN KEY (issue_id) REFERENCES issues(id) ON DELETE CASCADE
                );
                CREATE INDEX IF NOT EXISTS idx_time_entries_issue ON time_entries(issue_id);
                ",
            )?,
            4 | 5 => {},
            6 => tx.execute_batch(
                r"
                CREATE TABLE IF NOT EXISTS relations (
                    issue_id_1 INTEGER NOT NULL,
                    issue_id_2 INTEGER NOT NULL,
                    created_at TEXT NOT NULL,
                    PRIMARY KEY (issue_id_1, issue_id_2),
                    FOREIGN KEY (issue_id_1) REFERENCES issues(id) ON DELETE CASCADE,
                    FOREIGN KEY (issue_id_2) REFERENCES issues(id) ON DELETE CASCADE
                );
                CREATE TABLE IF NOT EXISTS milestones (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    name TEXT NOT NULL,
                    description TEXT,
                    status TEXT NOT NULL DEFAULT 'open',
                    created_at TEXT NOT NULL,
                    closed_at TEXT
                );
                CREATE TABLE IF NOT EXISTS milestone_issues (
                    milestone_id INTEGER NOT NULL,
                    issue_id INTEGER NOT NULL,
                    PRIMARY KEY (milestone_id, issue_id),
                    FOREIGN KEY (milestone_id) REFERENCES milestones(id) ON DELETE CASCADE,
                    FOREIGN KEY (issue_id) REFERENCES issues(id) ON DELETE CASCADE
                );
                CREATE INDEX IF NOT EXISTS idx_relations_1 ON relations(issue_id_1);
                CREATE INDEX IF NOT EXISTS idx_relations_2 ON relations(issue_id_2);
                CREATE INDEX IF NOT EXISTS idx_milestone_issues_m ON milestone_issues(milestone_id);
                CREATE INDEX IF NOT EXISTS idx_milestone_issues_i ON milestone_issues(issue_id);
                ",
            )?,
            7 => tx.execute_batch(
                r"
                DROP TABLE IF EXISTS sessions_new;
                CREATE TABLE sessions_new (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    started_at TEXT NOT NULL,
                    ended_at TEXT,
                    active_issue_id INTEGER,
                    handoff_notes TEXT,
                    FOREIGN KEY (active_issue_id) REFERENCES issues(id) ON DELETE SET NULL
                );
                INSERT OR IGNORE INTO sessions_new (id, started_at, ended_at, active_issue_id, handoff_notes)
                    SELECT id, started_at, ended_at, active_issue_id, handoff_notes FROM sessions;
                DROP TABLE sessions;
                ALTER TABLE sessions_new RENAME TO sessions;
                ",
            )?,
            8 => Self::add_column_if_missing(
                tx,
                "sessions",
                "last_action",
                "ALTER TABLE sessions ADD COLUMN last_action TEXT",
            )?,
            9 => Self::add_column_if_missing(
                tx,
                "sessions",
                "agent_id",
                "ALTER TABLE sessions ADD COLUMN agent_id TEXT",
            )?,
            10 => {
                Self::add_column_if_missing(
                    tx,
                    "issues",
                    "uuid",
                    "ALTER TABLE issues ADD COLUMN uuid TEXT",
                )?;
                Self::add_column_if_missing(
                    tx,
                    "issues",
                    "created_by",
                    "ALTER TABLE issues ADD COLUMN created_by TEXT",
                )?;
                Self::add_column_if_missing(
                    tx,
                    "comments",
                    "uuid",
                    "ALTER TABLE comments ADD COLUMN uuid TEXT",
                )?;
                Self::add_column_if_missing(
                    tx,
                    "comments",
                    "author",
                    "ALTER TABLE comments ADD COLUMN author TEXT",
                )?;
                Self::add_column_if_missing(
                    tx,
                    "milestones",
                    "uuid",
                    "ALTER TABLE milestones ADD COLUMN uuid TEXT",
                )?;
                tx.execute_batch(
                    r"
                    CREATE UNIQUE INDEX IF NOT EXISTS idx_issues_uuid ON issues(uuid);
                    CREATE UNIQUE INDEX IF NOT EXISTS idx_milestones_uuid ON milestones(uuid);
                    ",
                )?;
            }
            11 => Self::add_column_if_missing(
                tx,
                "comments",
                "kind",
                "ALTER TABLE comments ADD COLUMN kind TEXT DEFAULT 'note'",
            )?,
            12 => {
                Self::add_column_if_missing(
                    tx,
                    "comments",
                    "trigger_type",
                    "ALTER TABLE comments ADD COLUMN trigger_type TEXT",
                )?;
                Self::add_column_if_missing(
                    tx,
                    "comments",
                    "intervention_context",
                    "ALTER TABLE comments ADD COLUMN intervention_context TEXT",
                )?;
            }
            13 => Self::add_column_if_missing(
                tx,
                "comments",
                "driver_key_fingerprint",
                "ALTER TABLE comments ADD COLUMN driver_key_fingerprint TEXT",
            )?,
            14 => tx.execute_batch("DROP TABLE IF EXISTS sessions_new;")?,
            15 => tx.execute_batch(
                r"
                CREATE TABLE IF NOT EXISTS token_usage (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    agent_id TEXT NOT NULL,
                    session_id INTEGER,
                    timestamp TEXT NOT NULL,
                    input_tokens INTEGER NOT NULL DEFAULT 0,
                    output_tokens INTEGER NOT NULL DEFAULT 0,
                    cache_read_tokens INTEGER,
                    cache_creation_tokens INTEGER,
                    model TEXT NOT NULL DEFAULT 'unknown',
                    cost_estimate REAL,
                    FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE SET NULL
                );
                CREATE INDEX IF NOT EXISTS idx_token_usage_agent ON token_usage(agent_id);
                CREATE INDEX IF NOT EXISTS idx_token_usage_session ON token_usage(session_id);
                CREATE INDEX IF NOT EXISTS idx_token_usage_timestamp ON token_usage(timestamp);
                ",
            )?,
            16 => tx.execute_batch(
                r"
                CREATE TABLE IF NOT EXISTS sentinel_runs (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    run_id TEXT NOT NULL UNIQUE,
                    started_at TEXT NOT NULL,
                    completed_at TEXT,
                    mode TEXT NOT NULL,
                    signals_found INTEGER DEFAULT 0,
                    dispatched INTEGER DEFAULT 0,
                    collected INTEGER DEFAULT 0,
                    triaged INTEGER DEFAULT 0,
                    skipped INTEGER DEFAULT 0,
                    deferred INTEGER DEFAULT 0
                );
                CREATE TABLE IF NOT EXISTS sentinel_dispatches (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    run_id TEXT NOT NULL,
                    signal_ref TEXT NOT NULL,
                    signal_title TEXT NOT NULL,
                    source TEXT NOT NULL,
                    disposition TEXT NOT NULL,
                    agent_id TEXT,
                    crosslink_issue_id INTEGER,
                    gh_issue_number INTEGER,
                    label TEXT NOT NULL,
                    attempt_number INTEGER DEFAULT 1,
                    model_used TEXT,
                    outcome TEXT DEFAULT 'pending',
                    outcome_detail TEXT,
                    created_at TEXT NOT NULL,
                    completed_at TEXT,
                    FOREIGN KEY (crosslink_issue_id) REFERENCES issues(id)
                );
                CREATE INDEX IF NOT EXISTS idx_sentinel_dispatches_signal_ref
                    ON sentinel_dispatches(signal_ref);
                CREATE INDEX IF NOT EXISTS idx_sentinel_dispatches_outcome
                    ON sentinel_dispatches(outcome);
                CREATE INDEX IF NOT EXISTS idx_sentinel_dispatches_run_id
                    ON sentinel_dispatches(run_id);
                CREATE INDEX IF NOT EXISTS idx_sentinel_dispatches_gh_label
                    ON sentinel_dispatches(gh_issue_number, label);
                ",
            )?,
            17 => {
                Self::add_column_if_missing(
                    tx,
                    "issues",
                    "scheduled_at",
                    "ALTER TABLE issues ADD COLUMN scheduled_at TEXT",
                )?;
                Self::add_column_if_missing(
                    tx,
                    "issues",
                    "due_at",
                    "ALTER TABLE issues ADD COLUMN due_at TEXT",
                )?;
            }
            18 => {
                Self::add_column_if_missing(
                    tx,
                    "token_usage",
                    "provider",
                    "ALTER TABLE token_usage ADD COLUMN provider TEXT NOT NULL DEFAULT 'claude'",
                )?;
                Self::add_column_if_missing(
                    tx,
                    "token_usage",
                    "cached_input_tokens",
                    "ALTER TABLE token_usage ADD COLUMN cached_input_tokens INTEGER",
                )?;
                Self::add_column_if_missing(
                    tx,
                    "token_usage",
                    "reasoning_output_tokens",
                    "ALTER TABLE token_usage ADD COLUMN reasoning_output_tokens INTEGER",
                )?;
                Self::add_column_if_missing(
                    tx,
                    "token_usage",
                    "provider_metadata_json",
                    "ALTER TABLE token_usage ADD COLUMN provider_metadata_json TEXT",
                )?;
                tx.execute_batch(
                    "CREATE INDEX IF NOT EXISTS idx_token_usage_provider ON token_usage(provider);",
                )?;
            }
            _ => anyhow::bail!("no database migration registered for version {target}"),
        }
        Ok(())
    }

    fn add_column_if_missing(
        tx: &Transaction<'_>,
        table: &str,
        column: &str,
        sql: &str,
    ) -> Result<()> {
        let mut statement = tx.prepare(&format!("PRAGMA table_info({table})"))?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            if row.get::<_, String>(1)? == column {
                return Ok(());
            }
        }
        drop(rows);
        drop(statement);
        tx.execute_batch(sql)?;
        Ok(())
    }

    pub fn get_schema_version(&self) -> Result<i32> {
        let version: i32 = self
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))?;
        Ok(version)
    }
}
