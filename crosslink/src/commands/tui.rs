use std::path::Path;

use anyhow::Result;

use crate::db::Database;

pub fn run(db: &Database, crosslink_dir: &Path, database_unavailable: Option<&str>) -> Result<()> {
    crate::tui::run(db, crosslink_dir, database_unavailable)
}
