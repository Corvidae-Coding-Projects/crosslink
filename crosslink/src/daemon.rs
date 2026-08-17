use anyhow::{Context, Result};
use std::fs;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::db::Database;
use crate::hydration::hydrate_to_sqlite;

const FLUSH_INTERVAL_SECS: u64 = 30;

fn hydrate_v3_tick(cache_dir: &Path, db: &Database) -> Result<crate::hydration::HydrationStats> {
    let source = crate::hub_source::RefHubSource::new(cache_dir)?;
    let outcome = crate::compaction::reduce(&source)?;
    crate::hydration::hydrate_from_state(&outcome.state, db)
}

pub fn start(crosslink_dir: &Path) -> Result<()> {
    let pid_file = crosslink_dir.join("daemon.pid");
    let log_file = crosslink_dir.join("daemon.log");

    if let Some(pid) = read_pid(&pid_file) {
        if is_process_running(pid) {
            println!("Daemon already running (PID {pid})");
            return Ok(());
        }
        fs::remove_file(&pid_file).with_context(|| {
            format!(
                "Cannot remove stale daemon PID file at {}",
                pid_file.display()
            )
        })?;
    }

    let exe = std::env::current_exe().context("Failed to get executable path")?;

    let log_handle = fs::File::create(&log_file).context("Failed to create log file")?;
    let log_handle_err = log_handle
        .try_clone()
        .context("Failed to clone log file handle")?;
    let child = Command::new(&exe)
        .arg("daemon")
        .arg("run")
        .arg("--dir")
        .arg(crosslink_dir)
        .stdin(Stdio::null())
        .stdout(log_handle)
        .stderr(log_handle_err)
        .spawn()
        .context("Failed to spawn daemon process")?;

    let pid = child.id();

    fs::write(&pid_file, pid.to_string()).context("Failed to write PID file")?;

    println!("Daemon started (PID {pid})");
    println!("Log file: {}", log_file.display());
    Ok(())
}

pub fn stop(crosslink_dir: &Path) -> Result<()> {
    let pid_file = crosslink_dir.join("daemon.pid");

    let Some(pid) = read_pid(&pid_file) else {
        println!("Daemon not running (no PID file)");
        return Ok(());
    };

    if !is_process_running(pid) {
        fs::remove_file(&pid_file).ok();
        println!("Daemon not running (stale PID file removed)");
        return Ok(());
    }

    kill_process(pid)?;

    fs::remove_file(&pid_file).ok();

    println!("Daemon stopped (PID {pid})");
    Ok(())
}

pub fn status(crosslink_dir: &Path) {
    let pid_file = crosslink_dir.join("daemon.pid");

    if let Some(pid) = read_pid(&pid_file) {
        if is_process_running(pid) {
            println!("Daemon running (PID {pid})");
        } else {
            println!("Daemon not running (stale PID file)");
        }
    } else {
        println!("Daemon not running");
    }
}

pub fn run_daemon(crosslink_dir: &Path) -> Result<()> {
    let db_path = crosslink_dir.join("issues.db");
    if !db_path.exists() {
        anyhow::bail!(
            "Invalid crosslink directory: {} does not contain issues.db",
            crosslink_dir.display()
        );
    }

    let session_file = crosslink_dir.join("session.json");

    println!("Daemon starting...");
    println!("Watching: {}", crosslink_dir.display());
    println!("Flush interval: {FLUSH_INTERVAL_SECS} seconds");

    let mut heartbeat_counter: u64 = 0;
    const HEARTBEAT_EVERY_N: u64 = 5;

    let mut consecutive_db_failures: u32 = 0;
    let mut consecutive_sync_failures: u32 = 0;
    const FAILURE_WARN_THRESHOLD: u32 = 5;

    let should_exit = Arc::new(AtomicBool::new(false));

    #[cfg(unix)]
    {
        let flag = Arc::clone(&should_exit);
        if let Err(e) = signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&flag))
        {
            tracing::warn!(
                "could not register SIGTERM handler: {e} — graceful shutdown unavailable"
            );
        }
        if let Err(e) = signal_hook::flag::register(signal_hook::consts::SIGINT, flag) {
            tracing::warn!(
                "could not register SIGINT handler: {e} — graceful shutdown unavailable"
            );
        }
    }

    let should_exit_clone = Arc::clone(&should_exit);

    thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut buf = [0u8; 1];

        loop {
            match stdin.read(&mut buf) {
                Ok(0) => {
                    tracing::info!("Stdin closed, daemon shutting down (zombie prevention)");
                    should_exit_clone.store(true, Ordering::SeqCst);
                    break;
                }
                Err(_) => {
                    tracing::info!("Stdin error, daemon shutting down (zombie prevention)");
                    should_exit_clone.store(true, Ordering::SeqCst);
                    break;
                }
                Ok(_) => {}
            }
        }
    });

    loop {
        if should_exit.load(Ordering::SeqCst) {
            println!("Daemon exiting due to parent termination");
            break;
        }

        thread::sleep(Duration::from_secs(FLUSH_INTERVAL_SECS));

        if should_exit.load(Ordering::SeqCst) {
            println!("Daemon exiting due to parent termination");
            break;
        }

        let mut active_issue_id: Option<i64> = None;
        match Database::open(&db_path) {
            Ok(db) => {
                consecutive_db_failures = 0;
                let agent_id = crate::identity::AgentConfig::load(crosslink_dir)
                    .ok()
                    .flatten()
                    .map(|a| a.agent_id);
                if let Ok(Some(session)) = db.get_current_session_for_agent(agent_id.as_deref()) {
                    active_issue_id = session.active_issue_id;
                    let session_data = serde_json::json!({
                        "session_id": session.id,
                        "started_at": session.started_at.to_rfc3339(),
                        "active_issue_id": session.active_issue_id,
                    });

                    if let Ok(json) = serde_json::to_string_pretty(&session_data) {
                        if let Err(e) = fs::write(&session_file, json) {
                            tracing::warn!("Failed to write session file: {}", e);
                        } else {
                            println!(
                                "Session flushed at {}",
                                chrono::Utc::now().format("%H:%M:%S")
                            );
                        }
                    }
                }
            }
            Err(e) => {
                consecutive_db_failures += 1;
                tracing::warn!(
                    "Failed to open database: {} (failure #{})",
                    e,
                    consecutive_db_failures
                );
                if consecutive_db_failures == FAILURE_WARN_THRESHOLD {
                    tracing::warn!(
                        "{} consecutive database failures. Daemon may not be functioning correctly.",
                        FAILURE_WARN_THRESHOLD
                    );
                }
            }
        }

        heartbeat_counter += 1;
        if heartbeat_counter.is_multiple_of(HEARTBEAT_EVERY_N) {
            match crate::identity::AgentConfig::load(crosslink_dir) {
                Ok(Some(agent)) => match crate::sync::SyncManager::new(crosslink_dir) {
                    Ok(sync) => {
                        consecutive_sync_failures = 0;
                        if let Err(e) = sync.init_cache() {
                            tracing::warn!("cache init failed, skipping heartbeat: {}", e);
                            continue;
                        }

                        match sync.push_heartbeat(&agent, active_issue_id) {
                            Ok(()) => println!(
                                "Heartbeat pushed at {}",
                                chrono::Utc::now().format("%H:%M:%S")
                            ),
                            Err(e) => tracing::warn!("Heartbeat push failed: {}", e),
                        }

                        match sync.fetch() {
                            Ok(()) => {
                                if let Ok(db) = Database::open(&db_path) {
                                    let hydrate_result = if sync.hub_mode().is_v3() {
                                        hydrate_v3_tick(sync.cache_path(), &db)
                                    } else {
                                        hydrate_to_sqlite(sync.cache_path(), &db)
                                    };
                                    match hydrate_result {
                                        Ok(stats) => {
                                            crate::hydration::record_hydrated_ref(crosslink_dir);
                                            if stats.issues > 0 {
                                                println!(
                                                    "Hydrated {} issue(s) at {}",
                                                    stats.issues,
                                                    chrono::Utc::now().format("%H:%M:%S")
                                                );
                                            }
                                        }
                                        Err(e) => tracing::warn!("Hydration failed: {}", e),
                                    }
                                }
                            }
                            Err(e) => tracing::warn!("Fetch failed: {}", e),
                        }
                    }
                    Err(e) => {
                        consecutive_sync_failures += 1;
                        tracing::warn!(
                            "Sync init failed: {} (failure #{})",
                            e,
                            consecutive_sync_failures
                        );
                        if consecutive_sync_failures == FAILURE_WARN_THRESHOLD {
                            tracing::warn!(
                                    "{} consecutive sync failures. Daemon may not be functioning correctly.",
                                    FAILURE_WARN_THRESHOLD
                                );
                        }
                    }
                },
                Ok(None) => {}
                Err(e) => tracing::warn!("Failed to load agent config: {}", e),
            }
        }
    }

    Ok(())
}

fn read_pid(pid_file: &Path) -> Option<u32> {
    let mut file = fs::File::open(pid_file).ok()?;
    let mut contents = String::new();
    file.read_to_string(&mut contents).ok()?;
    contents.trim().parse().ok()
}

#[cfg(windows)]
fn is_process_running(pid: u32) -> bool {
    use std::process::Command;
    Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .output()
        .is_ok_and(|output| {
            let stdout = String::from_utf8_lossy(&output.stdout);
            stdout.contains(&pid.to_string())
        })
}

#[cfg(not(windows))]
fn is_process_running(pid: u32) -> bool {
    use std::process::Command;
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .is_ok_and(|s| s.success())
}

#[cfg(windows)]
fn kill_process(pid: u32) -> Result<()> {
    use std::process::Command;
    Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/F"])
        .status()
        .context("Failed to kill process")?;
    Ok(())
}

#[cfg(not(windows))]
fn kill_process(pid: u32) -> Result<()> {
    use std::process::Command;
    Command::new("kill")
        .arg(pid.to_string())
        .status()
        .context("Failed to kill process")?;
    Ok(())
}
