pub mod collect;
pub mod config;
pub mod dispatch;
pub mod engine;
pub mod history;
pub mod metrics;
pub mod notify;
pub mod patterns;
pub mod seen_set;
pub mod sources;
pub mod tuning;
pub mod watch;
pub mod webhook;

use anyhow::Result;
use std::path::Path;

use crate::application::RepositoryService;
use crate::db::Database;
use crate::SentinelCommands;

use config::SentinelConfig;

pub fn dispatch_cmd(
    command: SentinelCommands,
    crosslink_dir: &Path,
    db: &Database,
    quiet: bool,
    json: bool,
    model: Option<String>,
) -> Result<()> {
    let local_service = RepositoryService::projection(db);
    match command {
        SentinelCommands::Run { dry_run, label, .. } => {
            let config = SentinelConfig::load(crosslink_dir)?;
            let service = RepositoryService::new(db, crosslink_dir)?;
            engine::run_oneshot(
                crosslink_dir,
                db,
                &service,
                &config,
                dry_run,
                label.as_deref(),
                engine::CycleOptions {
                    quiet,
                    model_override: model.as_deref(),
                },
            )?;
            Ok(())
        }
        SentinelCommands::Watch { interval, .. } => {
            watch::start(crosslink_dir, interval, model.as_deref())
        }
        SentinelCommands::Status => watch::status(crosslink_dir, &local_service),
        SentinelCommands::History {
            limit,
            detail,
            json: json_flag,
        } => {
            let use_json = json || json_flag;
            history::show_history(&local_service, limit, detail, use_json)
        }
        SentinelCommands::Stop => watch::stop(crosslink_dir),
        SentinelCommands::Metrics { json: json_flag } => {
            let use_json = json || json_flag;
            metrics::show_metrics(&local_service, use_json)
        }
        SentinelCommands::Patterns { json: json_flag } => {
            let use_json = json || json_flag;
            patterns::detect_patterns(&local_service, use_json)
        }
        SentinelCommands::RunDaemon { dir, interval, .. } => {
            watch::run_watch_loop(&dir, interval, model)
        }
    }
}
