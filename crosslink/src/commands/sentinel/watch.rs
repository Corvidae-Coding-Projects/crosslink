use anyhow::{Context, Result};
use std::fs;
use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::application::LocalStateService;
use crate::db::Database;

use super::config::SentinelConfig;
use super::engine;

pub fn start(crosslink_dir: &Path, interval: u64, model: Option<&str>) -> Result<()> {
    let pid_file = crosslink_dir.join("sentinel.pid");
    let log_file = crosslink_dir.join("sentinel.log");

    if let Some(pid) = read_pid(&pid_file) {
        if is_process_running(pid) {
            println!("Sentinel already running (PID {pid})");
            return Ok(());
        }
        fs::remove_file(&pid_file).with_context(|| {
            format!(
                "Cannot remove stale sentinel PID file at {}",
                pid_file.display()
            )
        })?;
    }

    let exe = std::env::current_exe().context("Failed to get executable path")?;

    let log_handle = fs::File::create(&log_file).context("Failed to create sentinel log file")?;
    let log_handle_err = log_handle
        .try_clone()
        .context("Failed to clone log file handle")?;
    let mut cmd = Command::new(&exe);
    cmd.arg("sentinel")
        .arg("run-daemon")
        .arg("--dir")
        .arg(crosslink_dir)
        .arg("--interval")
        .arg(interval.to_string());
    if let Some(m) = model {
        cmd.arg("--model").arg(m);
    }
    let child = cmd
        .stdin(Stdio::null())
        .stdout(log_handle)
        .stderr(log_handle_err)
        .spawn()
        .context("Failed to spawn sentinel daemon")?;

    let pid = child.id();
    fs::write(&pid_file, pid.to_string()).context("Failed to write sentinel PID file")?;

    println!("Sentinel started (PID {pid})");
    println!("  Interval: {interval} minutes");
    println!("  Log file: {}", log_file.display());
    Ok(())
}

pub fn stop(crosslink_dir: &Path) -> Result<()> {
    let pid_file = crosslink_dir.join("sentinel.pid");

    let Some(pid) = read_pid(&pid_file) else {
        println!("Sentinel not running (no PID file)");
        return Ok(());
    };

    if !is_process_running(pid) {
        fs::remove_file(&pid_file).ok();
        println!("Sentinel not running (stale PID file removed)");
        return Ok(());
    }

    kill_process(pid)?;
    fs::remove_file(&pid_file).ok();
    println!("Sentinel stopped (PID {pid})");
    Ok(())
}

pub fn status(crosslink_dir: &Path, db: &impl LocalStateService) -> Result<()> {
    let pid_file = crosslink_dir.join("sentinel.pid");

    let running = read_pid(&pid_file).map_or_else(
        || {
            println!("Sentinel not running");
            false
        },
        |pid| {
            if is_process_running(pid) {
                println!("Sentinel running (PID {pid})");
                true
            } else {
                println!("Sentinel not running (stale PID file)");
                false
            }
        },
    );

    let pending_dispatches = db.get_pending_dispatches()?;
    let config = SentinelConfig::load(crosslink_dir)?;
    println!(
        "  In-flight: {} / {} agents",
        pending_dispatches.len(),
        config.max_concurrent_agents
    );

    for d in &pending_dispatches {
        let elapsed = super::collect::format_elapsed(&d.created_at);
        let agent = d.agent_id.as_deref().unwrap_or("unknown");
        let model = d.model_used.as_deref().unwrap_or("?");
        println!(
            "    {} — {} (attempt {}, {}, {})",
            d.signal_ref, agent, d.attempt_number, model, elapsed
        );
    }

    let runs = db.list_sentinel_runs(1)?;
    if let Some(last) = runs.first() {
        let started = last
            .started_at
            .get(..19)
            .unwrap_or(&last.started_at)
            .replace('T', " ");
        println!(
            "  Last run:  {} ({} signals, {} dispatched)",
            started, last.signals_found, last.dispatched
        );
    }

    if config.webhook.enabled {
        println!(
            "  Webhook:   enabled on port {} ({})",
            config.webhook.port,
            if config.webhook.secret.is_some() {
                "secret configured"
            } else {
                "no secret — signature verification disabled"
            }
        );
    }

    if !running && !pending_dispatches.is_empty() {
        println!(
            "  Warning: {} agent(s) in-flight but daemon not running — results won't be collected",
            pending_dispatches.len()
        );
    }

    Ok(())
}

pub fn run_watch_loop(
    crosslink_dir: &Path,
    interval_minutes: u64,
    model: Option<String>,
) -> Result<()> {
    let db_path = crosslink_dir.join("issues.db");
    if !db_path.exists() {
        anyhow::bail!(
            "Invalid crosslink directory: {} does not contain issues.db",
            crosslink_dir.display()
        );
    }

    let config = SentinelConfig::load(crosslink_dir)?;
    if !config.enabled {
        println!("Sentinel is disabled in hook-config.json");
        return Ok(());
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("Failed to build tokio runtime for sentinel daemon")?;

    runtime.block_on(async_watch_loop(
        crosslink_dir.to_path_buf(),
        interval_minutes,
        config,
        model,
    ))
}

async fn async_watch_loop(
    crosslink_dir: PathBuf,
    interval_minutes: u64,
    config: SentinelConfig,
    model: Option<String>,
) -> Result<()> {
    let interval = Duration::from_secs(interval_minutes * 60);
    let mut backoff_multiplier: u32 = 1;

    println!("Sentinel daemon starting...");
    println!("  Watching: {}", crosslink_dir.display());
    println!("  Interval: {interval_minutes} minutes");

    let mut webhook_rx = if config.webhook.enabled {
        let webhook_config = super::webhook::WebhookConfig {
            port: config.webhook.port,
            secret: config.webhook.secret.clone(),
        };
        match super::webhook::start_webhook_server(&webhook_config).await {
            Ok(rx) => {
                println!("  Webhook:  listening on port {}", config.webhook.port);
                Some(rx)
            }
            Err(e) => {
                tracing::error!("Failed to start webhook server: {e}");
                None
            }
        }
    } else {
        None
    };

    let should_exit = Arc::new(AtomicBool::new(false));

    let (shutdown_tx, mut shutdown_rx) = tokio::sync::mpsc::channel::<&str>(1);
    {
        let tx = shutdown_tx;
        tokio::spawn(async move {
            #[cfg(unix)]
            {
                use tokio::signal::unix::{signal, SignalKind};
                let Ok(mut sigterm) = signal(SignalKind::terminate()) else {
                    tracing::error!("Failed to register SIGTERM handler");
                    return;
                };
                let Ok(mut sigint) = signal(SignalKind::interrupt()) else {
                    tracing::error!("Failed to register SIGINT handler");
                    return;
                };
                tokio::select! {
                    _ = sigterm.recv() => { let _ = tx.send("SIGTERM").await; }
                    _ = sigint.recv() => { let _ = tx.send("SIGINT").await; }
                }
            }
            #[cfg(windows)]
            {
                let _ = tokio::signal::ctrl_c().await;
                let _ = tx.send("Ctrl-C").await;
            }
        });
    }

    if std::io::stdin().is_terminal() {
        let stdin_flag = Arc::clone(&should_exit);
        thread::spawn(move || {
            let mut stdin = std::io::stdin();
            let mut buf = [0u8; 1];
            loop {
                match stdin.read(&mut buf) {
                    Ok(0) | Err(_) => {
                        tracing::info!("Stdin closed, sentinel shutting down");
                        stdin_flag.store(true, Ordering::SeqCst);
                        break;
                    }
                    Ok(_) => {}
                }
            }
        });
    } else {
        tracing::debug!(
            "stdin is not a TTY; skipping stdin-close zombie-prevention check \
             (SIGTERM/SIGINT remain active)"
        );
    }

    let mut next_poll_at = tokio::time::Instant::now();

    loop {
        if should_exit.load(Ordering::SeqCst) {
            println!("Sentinel exiting");
            break;
        }

        let sleep_until = tokio::time::sleep_until(next_poll_at);
        tokio::pin!(sleep_until);

        tokio::select! {

            () = &mut sleep_until => {
                let cycle_dir = crosslink_dir.clone();
                let cycle_config = config.clone();
                let cycle_model = model.clone();
                let result = tokio::task::spawn_blocking(move || {
                    run_polling_cycle(&cycle_dir, &cycle_config, cycle_model.as_deref())
                })
                .await
                .context("polling cycle task panicked")?;

                if let Err(e) = result {
                    tracing::error!("sentinel polling cycle failed: {e}");
                    backoff_multiplier = (backoff_multiplier * 2).min(8);
                } else {
                    backoff_multiplier = 1;
                }

                next_poll_at = tokio::time::Instant::now() + interval * backoff_multiplier;
            }


            maybe_event = recv_webhook(&mut webhook_rx) => {
                if let Some(event) = maybe_event {
                    let cycle_dir = crosslink_dir.clone();
                    let cycle_config = config.clone();
                    let cycle_model = model.clone();
                    let signal = event.signal;
                    let result = tokio::task::spawn_blocking(move || {
                        run_webhook_cycle(
                            &cycle_dir,
                            &cycle_config,
                            signal,
                            cycle_model.as_deref(),
                        )
                    })
                    .await
                    .context("webhook cycle task panicked")?;

                    if let Err(e) = result {
                        tracing::error!("webhook cycle failed: {e}");
                    }
                }
            }


            signal_name = shutdown_rx.recv() => {
                let name = signal_name.unwrap_or("unknown");
                println!("Sentinel received {name}, exiting");
                break;
            }
        }
    }

    Ok(())
}

async fn recv_webhook(
    rx: &mut Option<tokio::sync::mpsc::Receiver<super::webhook::WebhookEvent>>,
) -> Option<super::webhook::WebhookEvent> {
    match rx {
        Some(receiver) => receiver.recv().await,
        None => std::future::pending().await,
    }
}

fn run_polling_cycle(
    crosslink_dir: &Path,
    config: &SentinelConfig,
    model_override: Option<&str>,
) -> Result<()> {
    let _permit = crate::reconcile::readiness::acquire_mutation_operation_permit(crosslink_dir)?;
    let db = Database::open(&crosslink_dir.join("issues.db"))?;
    let service = crate::application::RepositoryService::new(&db, crosslink_dir)?;

    let stats = engine::run_oneshot(
        crosslink_dir,
        &db,
        &service,
        config,
        false,
        None,
        engine::CycleOptions {
            quiet: true,
            model_override,
        },
    )?;

    if stats.signals_found > 0 || stats.collected > 0 {
        println!(
            "Polling cycle at {}: {} signals, {} dispatched, {} skipped, {} deferred, {} collected",
            chrono::Utc::now().format("%H:%M:%S"),
            stats.signals_found,
            stats.dispatched,
            stats.skipped,
            stats.deferred,
            stats.collected,
        );
    }

    Ok(())
}

fn run_webhook_cycle(
    crosslink_dir: &Path,
    config: &SentinelConfig,
    signal: super::sources::Signal,
    model_override: Option<&str>,
) -> Result<()> {
    let _permit = crate::reconcile::readiness::acquire_mutation_operation_permit(crosslink_dir)?;
    let db = Database::open(&crosslink_dir.join("issues.db"))?;
    let service = crate::application::RepositoryService::new(&db, crosslink_dir)?;

    let signal_ref = signal.reference.clone();
    let stats = engine::process_signal_batch(
        crosslink_dir,
        &db,
        &service,
        config,
        &[signal],
        "webhook",
        engine::CycleOptions {
            quiet: true,
            model_override,
        },
    )?;

    println!(
        "Webhook cycle at {}: {} ({} dispatched, {} skipped, {} deferred)",
        chrono::Utc::now().format("%H:%M:%S"),
        signal_ref,
        stats.dispatched,
        stats.skipped,
        stats.deferred,
    );

    Ok(())
}

fn read_pid(pid_file: &Path) -> Option<u32> {
    let mut file = fs::File::open(pid_file).ok()?;
    let mut contents = String::new();
    file.read_to_string(&mut contents).ok()?;
    contents.trim().parse().ok()
}

#[cfg(not(windows))]
fn is_process_running(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .is_ok_and(|s| s.success())
}

#[cfg(windows)]
fn is_process_running(pid: u32) -> bool {
    crate::reconcile::readiness::is_process_running(pid)
}

#[cfg(not(windows))]
fn kill_process(pid: u32) -> Result<()> {
    Command::new("kill")
        .arg(pid.to_string())
        .status()
        .context("Failed to kill sentinel process")?;
    Ok(())
}

#[cfg(windows)]
fn kill_process(pid: u32) -> Result<()> {
    Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/F"])
        .status()
        .context("Failed to kill sentinel process")?;
    Ok(())
}
