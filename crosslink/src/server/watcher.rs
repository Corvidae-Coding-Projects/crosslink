use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Result;
use chrono::{Duration, Utc};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::broadcast;
use tokio::time;

use crate::locks::Heartbeat;
use crate::server::types::{AgentStatus, WsAgentStatusEvent, WsHeartbeatEvent};
use crate::server::ws::WsEvent;
use crate::sync::SyncManager;

const POLL_INTERVAL_SECS: u64 = 30;

#[must_use]
pub fn status_from_heartbeat(heartbeat: &Heartbeat) -> AgentStatus {
    let age = Utc::now() - heartbeat.last_heartbeat;
    if age < Duration::minutes(5) {
        AgentStatus::Active
    } else if age < Duration::minutes(30) {
        AgentStatus::Idle
    } else {
        AgentStatus::Stale
    }
}

fn resolve_watch_target(
    sync: &SyncManager,
    crosslink_dir: &std::path::Path,
) -> (PathBuf, RecursiveMode) {
    let v2_path = crosslink_dir.join(".hub-cache").join("heartbeats");
    if !sync.hub_mode().is_v3() {
        return (v2_path, RecursiveMode::NonRecursive);
    }
    git_common_dir(sync.cache_path()).map_or((v2_path, RecursiveMode::NonRecursive), |git_dir| {
        (
            git_dir.join("refs").join("heads").join("crosslink"),
            RecursiveMode::Recursive,
        )
    })
}

fn git_common_dir(repo_dir: &std::path::Path) -> Option<PathBuf> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo_dir)
        .args(["rev-parse", "--git-common-dir"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if raw.is_empty() {
        return None;
    }
    let path = PathBuf::from(&raw);
    if path.is_absolute() {
        Some(path)
    } else {
        Some(repo_dir.join(path))
    }
}

pub fn start_watcher(crosslink_dir: PathBuf, tx: broadcast::Sender<WsEvent>) {
    tokio::spawn(async move {
        if let Err(e) = run_watcher(crosslink_dir, tx).await {
            tracing::error!("watcher: error: {e}");
        }
    });
}

async fn run_watcher(crosslink_dir: PathBuf, tx: broadcast::Sender<WsEvent>) -> Result<()> {
    let sync = SyncManager::new(&crosslink_dir)?;

    let (watch_path, watch_recursive) = resolve_watch_target(&sync, &crosslink_dir);

    let (notify_tx, mut notify_rx) = tokio::sync::mpsc::channel::<()>(16);

    let mut watcher: RecommendedWatcher = {
        let notify_tx = notify_tx.clone();
        notify::recommended_watcher(move |_res: notify::Result<notify::Event>| {
            let _ = notify_tx.blocking_send(());
        })?
    };

    let watch_active = if watch_path.exists() {
        match watcher.watch(&watch_path, watch_recursive) {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(
                    "watcher: could not watch {}: {e}, falling back to polling",
                    watch_path.display()
                );
                false
            }
        }
    } else {
        tracing::info!(
            "watcher: heartbeat watch path not found at {}, polling only",
            watch_path.display()
        );
        false
    };

    if watch_active {
        tracing::info!(
            "watcher: watching {} for heartbeat changes",
            watch_path.display()
        );
    }

    let mut last_state: HashMap<String, Heartbeat> = HashMap::new();
    let mut last_statuses: HashMap<String, AgentStatus> = HashMap::new();

    if let Ok(heartbeats) = sync.read_heartbeats_auto() {
        for hb in heartbeats {
            last_statuses.insert(hb.agent_id.clone(), status_from_heartbeat(&hb));
            last_state.insert(hb.agent_id.clone(), hb);
        }
    }

    let mut poll_interval = time::interval(time::Duration::from_secs(POLL_INTERVAL_SECS));
    poll_interval.tick().await;

    loop {
        tokio::select! {

            result = notify_rx.recv() => {
                match result {
                    Some(()) => {
                        diff_and_broadcast(&sync, &mut last_state, &mut last_statuses, &tx);
                    }

                    None => break,
                }
            }


            _ = poll_interval.tick() => {
                diff_and_broadcast(&sync, &mut last_state, &mut last_statuses, &tx);
            }
        }

        if tx.receiver_count() == 0 {
            break;
        }
    }

    Ok(())
}

fn diff_and_broadcast(
    sync: &SyncManager,
    last_state: &mut HashMap<String, Heartbeat>,
    last_statuses: &mut HashMap<String, AgentStatus>,
    tx: &broadcast::Sender<WsEvent>,
) {
    let heartbeats = match sync.read_heartbeats_auto() {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!("watcher: failed to read heartbeats: {e}");
            return;
        }
    };

    let mut current_state: HashMap<String, Heartbeat> = HashMap::new();
    for hb in heartbeats {
        current_state.insert(hb.agent_id.clone(), hb);
    }

    for (agent_id, hb) in &current_state {
        let is_new_or_updated = last_state
            .get(agent_id)
            .is_none_or(|prev| prev.last_heartbeat != hb.last_heartbeat);

        if is_new_or_updated {
            let _ = tx.send(WsEvent::Heartbeat(WsHeartbeatEvent {
                event_type: crate::server::types::WsEventType::Heartbeat,
                agent_id: agent_id.clone(),
                timestamp: hb.last_heartbeat,
                active_issue_id: hb.active_issue_id,
            }));

            let new_status = status_from_heartbeat(hb);
            let status_changed = last_statuses.get(agent_id) != Some(&new_status);

            if status_changed {
                let _ = tx.send(WsEvent::AgentStatus(WsAgentStatusEvent {
                    event_type: crate::server::types::WsEventType::AgentStatus,
                    agent_id: agent_id.clone(),
                    status: new_status.clone(),
                }));
                last_statuses.insert(agent_id.clone(), new_status);
            }
        }
    }

    *last_state = current_state;
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use std::process::Command;
    use tempfile::tempdir;

    fn make_heartbeat(agent_id: &str, age_minutes: i64) -> Heartbeat {
        Heartbeat {
            agent_id: agent_id.to_string(),
            last_heartbeat: Utc::now() - Duration::minutes(age_minutes),
            active_issue_id: None,
            machine_id: "test-machine".to_string(),
        }
    }

    fn setup_watcher_env() -> (tempfile::TempDir, tempfile::TempDir, SyncManager) {
        let remote_dir = tempdir().unwrap();
        let work_dir = tempdir().unwrap();

        Command::new("git")
            .current_dir(remote_dir.path())
            .args(["init", "--bare", "-b", "main"])
            .output()
            .unwrap();

        Command::new("git")
            .current_dir(work_dir.path())
            .args(["init", "-b", "main"])
            .output()
            .unwrap();

        for args in [
            vec!["config", "user.email", "test@test.local"],
            vec!["config", "user.name", "Test"],
            vec![
                "remote",
                "add",
                "origin",
                remote_dir.path().to_str().unwrap(),
            ],
        ] {
            Command::new("git")
                .current_dir(work_dir.path())
                .args(&args)
                .output()
                .unwrap();
        }

        std::fs::write(work_dir.path().join("README.md"), "# test\n").unwrap();
        Command::new("git")
            .current_dir(work_dir.path())
            .args(["add", "."])
            .output()
            .unwrap();
        Command::new("git")
            .current_dir(work_dir.path())
            .args(["commit", "-m", "init", "--no-gpg-sign"])
            .output()
            .unwrap();
        Command::new("git")
            .current_dir(work_dir.path())
            .args(["push", "-u", "origin", "main"])
            .output()
            .unwrap();

        let crosslink_dir = work_dir.path().join(".crosslink");
        std::fs::create_dir_all(&crosslink_dir).unwrap();
        std::fs::write(
            crosslink_dir.join("hook-config.json"),
            r#"{"remote":"origin"}"#,
        )
        .unwrap();

        let sync = SyncManager::new(&crosslink_dir).unwrap();
        sync.init_cache().unwrap();

        (work_dir, remote_dir, sync)
    }

    fn write_heartbeat_file(sync: &SyncManager, hb: &Heartbeat) {
        crate::hub_v3::write_heartbeat_to_ref(sync.cache_path(), &hb.agent_id, hb).unwrap();
    }

    #[test]
    fn test_resolve_watch_target_v2_heartbeats_dir() {
        let dir = tempfile::tempdir().unwrap();
        let crosslink_dir = dir.path().join(".crosslink");
        std::fs::create_dir_all(crosslink_dir.join(".hub-cache")).unwrap();
        let sync = SyncManager::new(&crosslink_dir).unwrap();
        assert!(!sync.hub_mode().is_v3());

        let (path, mode) = resolve_watch_target(&sync, &crosslink_dir);
        assert_eq!(path, crosslink_dir.join(".hub-cache").join("heartbeats"));
        assert!(matches!(mode, RecursiveMode::NonRecursive));
    }

    #[test]
    fn test_resolve_watch_target_v3_refs_dir() {
        let (_work, _remote, sync) = setup_watcher_env();
        let cache_dir = sync.cache_path().to_path_buf();
        let crosslink_dir = cache_dir.parent().unwrap().to_path_buf();

        let meta = crate::hub_v3::HubMeta {
            hub_version: 3,
            migrated_from_commit: "0".repeat(40),
            migrated_at: Utc::now(),
            finalized_at: None,
            genesis_checkpoint_commit: None,
            seed_agent_tips: None,
        };
        let hub_json = serde_json::to_vec_pretty(&meta).unwrap();
        crate::hub_v3::commit_files_to_ref(
            &cache_dir,
            crate::hub_v3::META_REF,
            &[("hub.json", &hub_json)],
            "v3 meta",
        )
        .unwrap();
        let state = crate::checkpoint::CheckpointState::default();
        let state_json = serde_json::to_vec_pretty(&state).unwrap();
        crate::hub_v3::commit_blob_to_ref(
            &cache_dir,
            crate::hub_v3::CHECKPOINT_REF,
            "state.json",
            &state_json,
            "v3 checkpoint",
        )
        .unwrap();

        let sync_v3 = SyncManager::new(&crosslink_dir).unwrap();
        assert!(sync_v3.hub_mode().is_v3(), "hub must resolve to V3");

        let (path, mode) = resolve_watch_target(&sync_v3, &crosslink_dir);
        assert!(
            path.ends_with(std::path::Path::new("refs").join("heads").join("crosslink")),
            "v3 watch target must be .git/refs/heads/crosslink, got {}",
            path.display()
        );
        assert!(matches!(mode, RecursiveMode::Recursive));
    }

    #[test]
    fn test_diff_and_broadcast_new_agent() {
        let (_work, _remote, sync) = setup_watcher_env();
        let (tx, mut rx) = broadcast::channel::<WsEvent>(16);
        let mut last_state: HashMap<String, Heartbeat> = HashMap::new();
        let mut last_statuses: HashMap<String, AgentStatus> = HashMap::new();

        let hb = Heartbeat {
            agent_id: "worker-1".to_string(),
            last_heartbeat: Utc::now(),
            active_issue_id: Some(42),
            machine_id: "test-host".to_string(),
        };
        write_heartbeat_file(&sync, &hb);

        diff_and_broadcast(&sync, &mut last_state, &mut last_statuses, &tx);

        let ev1 = rx.try_recv().expect("Heartbeat event");
        let ev2 = rx.try_recv().expect("AgentStatus event");
        assert!(rx.try_recv().is_err(), "no extra events");

        assert!(matches!(ev1, WsEvent::Heartbeat(_)));
        assert!(matches!(ev2, WsEvent::AgentStatus(_)));

        if let WsEvent::Heartbeat(e) = ev1 {
            assert_eq!(e.agent_id, "worker-1");
            assert_eq!(e.active_issue_id, Some(42));
        }
        if let WsEvent::AgentStatus(e) = ev2 {
            assert_eq!(e.agent_id, "worker-1");
            assert_eq!(e.status, AgentStatus::Active);
        }

        assert_eq!(last_state.len(), 1);
        assert_eq!(last_statuses.len(), 1);
    }

    #[test]
    fn test_diff_and_broadcast_unchanged() {
        let (_work, _remote, sync) = setup_watcher_env();
        let (tx, mut rx) = broadcast::channel::<WsEvent>(16);
        let mut last_state: HashMap<String, Heartbeat> = HashMap::new();
        let mut last_statuses: HashMap<String, AgentStatus> = HashMap::new();

        let hb = Heartbeat {
            agent_id: "worker-2".to_string(),
            last_heartbeat: Utc::now() - Duration::minutes(2),
            active_issue_id: None,
            machine_id: "test-host".to_string(),
        };
        write_heartbeat_file(&sync, &hb);

        diff_and_broadcast(&sync, &mut last_state, &mut last_statuses, &tx);

        while rx.try_recv().is_ok() {}

        diff_and_broadcast(&sync, &mut last_state, &mut last_statuses, &tx);
        assert!(rx.try_recv().is_err(), "no events for unchanged heartbeat");
    }

    #[test]
    fn test_diff_and_broadcast_updated_timestamp() {
        let (_work, _remote, sync) = setup_watcher_env();
        let (tx, mut rx) = broadcast::channel::<WsEvent>(16);
        let mut last_state: HashMap<String, Heartbeat> = HashMap::new();
        let mut last_statuses: HashMap<String, AgentStatus> = HashMap::new();

        let hb1 = Heartbeat {
            agent_id: "worker-3".to_string(),
            last_heartbeat: Utc::now() - Duration::minutes(2),
            active_issue_id: None,
            machine_id: "test-host".to_string(),
        };
        write_heartbeat_file(&sync, &hb1);
        diff_and_broadcast(&sync, &mut last_state, &mut last_statuses, &tx);
        while rx.try_recv().is_ok() {}

        let hb2 = Heartbeat {
            last_heartbeat: Utc::now() - Duration::minutes(1),
            ..hb1
        };
        write_heartbeat_file(&sync, &hb2);
        diff_and_broadcast(&sync, &mut last_state, &mut last_statuses, &tx);

        let ev = rx.try_recv().expect("Heartbeat event");
        assert!(matches!(ev, WsEvent::Heartbeat(_)));

        assert!(
            rx.try_recv().is_err(),
            "no AgentStatus when status unchanged"
        );
    }

    #[test]
    fn test_diff_and_broadcast_status_change() {
        let (_work, _remote, sync) = setup_watcher_env();
        let (tx, mut rx) = broadcast::channel::<WsEvent>(16);
        let mut last_state: HashMap<String, Heartbeat> = HashMap::new();
        let mut last_statuses: HashMap<String, AgentStatus> = HashMap::new();

        let hb1 = Heartbeat {
            agent_id: "worker-4".to_string(),
            last_heartbeat: Utc::now() - Duration::minutes(10),
            active_issue_id: None,
            machine_id: "test-host".to_string(),
        };
        write_heartbeat_file(&sync, &hb1);
        diff_and_broadcast(&sync, &mut last_state, &mut last_statuses, &tx);
        while rx.try_recv().is_ok() {}

        let hb2 = Heartbeat {
            last_heartbeat: Utc::now() - Duration::minutes(35),
            ..hb1
        };
        write_heartbeat_file(&sync, &hb2);
        diff_and_broadcast(&sync, &mut last_state, &mut last_statuses, &tx);

        let ev1 = rx.try_recv().expect("Heartbeat event");
        let ev2 = rx.try_recv().expect("AgentStatus event");
        assert!(rx.try_recv().is_err(), "no extra events");

        assert!(matches!(ev1, WsEvent::Heartbeat(_)));
        assert!(matches!(ev2, WsEvent::AgentStatus(_)));
        if let WsEvent::AgentStatus(e) = ev2 {
            assert_eq!(e.status, AgentStatus::Stale);
        }
    }

    #[test]
    fn test_diff_and_broadcast_read_error_returns_gracefully() {
        let dir = tempdir().unwrap();
        let crosslink_dir = dir.path().join(".crosslink");
        std::fs::create_dir_all(&crosslink_dir).unwrap();

        let sync = SyncManager::new(&crosslink_dir).unwrap();

        let (tx, mut rx) = broadcast::channel::<WsEvent>(16);
        let mut last_state: HashMap<String, Heartbeat> = HashMap::new();
        let mut last_statuses: HashMap<String, AgentStatus> = HashMap::new();

        diff_and_broadcast(&sync, &mut last_state, &mut last_statuses, &tx);
        assert!(rx.try_recv().is_err(), "no events when no heartbeats");
    }

    #[test]
    fn test_status_active() {
        let hb = make_heartbeat("a1", 2);
        assert_eq!(status_from_heartbeat(&hb), AgentStatus::Active);
    }

    #[test]
    fn test_status_idle() {
        let hb = make_heartbeat("a1", 10);
        assert_eq!(status_from_heartbeat(&hb), AgentStatus::Idle);
    }

    #[test]
    fn test_status_stale() {
        let hb = make_heartbeat("a1", 45);
        assert_eq!(status_from_heartbeat(&hb), AgentStatus::Stale);
    }

    #[test]
    fn test_status_boundary_five_min() {
        let hb = make_heartbeat("a1", 5);
        assert_eq!(status_from_heartbeat(&hb), AgentStatus::Idle);
    }

    #[test]
    fn test_status_boundary_thirty_min() {
        let hb = make_heartbeat("a1", 30);
        assert_eq!(status_from_heartbeat(&hb), AgentStatus::Stale);
    }

    #[test]
    fn test_diff_broadcasts_new_heartbeat() {
        let (tx, mut rx) = broadcast::channel::<WsEvent>(16);
        let mut last_state: HashMap<String, Heartbeat> = HashMap::new();
        let mut last_statuses: HashMap<String, AgentStatus> = HashMap::new();

        let hb = make_heartbeat("worker-1", 1);
        let mut current: HashMap<String, Heartbeat> = HashMap::new();
        current.insert("worker-1".to_string(), hb);

        for (agent_id, hb) in &current {
            let is_new = !last_state.contains_key(agent_id);
            if is_new {
                let _ = tx.send(WsEvent::Heartbeat(WsHeartbeatEvent {
                    event_type: crate::server::types::WsEventType::Heartbeat,
                    agent_id: agent_id.clone(),
                    timestamp: hb.last_heartbeat,
                    active_issue_id: hb.active_issue_id,
                }));
                let new_status = status_from_heartbeat(hb);
                let _ = tx.send(WsEvent::AgentStatus(WsAgentStatusEvent {
                    event_type: crate::server::types::WsEventType::AgentStatus,
                    agent_id: agent_id.clone(),
                    status: new_status.clone(),
                }));
                last_statuses.insert(agent_id.clone(), new_status);
            }
        }
        last_state = current;

        let ev1 = rx.try_recv().unwrap();
        let ev2 = rx.try_recv().unwrap();
        assert!(rx.try_recv().is_err(), "no extra events");

        assert!(matches!(ev1, WsEvent::Heartbeat(_)));
        assert!(matches!(ev2, WsEvent::AgentStatus(_)));
        assert_eq!(last_state.len(), 1);
        assert_eq!(last_statuses.len(), 1);
    }

    #[test]
    fn test_diff_no_broadcast_on_unchanged() {
        let (tx, mut rx) = broadcast::channel::<WsEvent>(16);
        let mut last_state: HashMap<String, Heartbeat> = HashMap::new();

        let hb = make_heartbeat("worker-1", 1);
        last_state.insert("worker-1".to_string(), hb.clone());

        let mut current: HashMap<String, Heartbeat> = HashMap::new();
        current.insert("worker-1".to_string(), hb);

        for (agent_id, hb) in &current {
            let is_new_or_updated = last_state
                .get(agent_id)
                .is_none_or(|prev| prev.last_heartbeat != hb.last_heartbeat);
            if is_new_or_updated {
                let _ = tx.send(WsEvent::Heartbeat(WsHeartbeatEvent {
                    event_type: crate::server::types::WsEventType::Heartbeat,
                    agent_id: agent_id.clone(),
                    timestamp: hb.last_heartbeat,
                    active_issue_id: hb.active_issue_id,
                }));
            }
        }

        assert!(rx.try_recv().is_err(), "no events for unchanged heartbeat");
    }
}
