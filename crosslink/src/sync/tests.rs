use super::*;
use crate::identity::{AgentConfig, AgentRole};
use chrono::Utc;
use std::path::Path;
use std::process::Command;
use tempfile::tempdir;

#[test]
fn test_sync_manager_new() {
    let dir = tempdir().unwrap();
    let crosslink_dir = dir.path().join(".crosslink");
    std::fs::create_dir_all(&crosslink_dir).unwrap();

    let manager = SyncManager::new(&crosslink_dir).unwrap();
    assert_eq!(manager.cache_dir, crosslink_dir.join(HUB_CACHE_DIR));
    assert_eq!(manager.repo_root, dir.path());
}

#[test]
fn test_sync_manager_not_initialized() {
    let dir = tempdir().unwrap();
    let crosslink_dir = dir.path().join(".crosslink");
    std::fs::create_dir_all(&crosslink_dir).unwrap();

    let manager = SyncManager::new(&crosslink_dir).unwrap();
    assert!(!manager.is_initialized());
}

#[test]
fn test_read_locks_no_cache() {
    let dir = tempdir().unwrap();
    let crosslink_dir = dir.path().join(".crosslink");
    std::fs::create_dir_all(&crosslink_dir).unwrap();

    let manager = SyncManager::new(&crosslink_dir).unwrap();

    let locks_path = manager.cache_dir.join("locks.json");
    assert!(!locks_path.exists());
}

fn init_git_repo(path: &Path) {
    let p = path.to_string_lossy();
    Command::new("git").args(["init", &p]).output().unwrap();

    Command::new("git")
        .args(["-C", &p, "config", "user.email", "test@test.com"])
        .output()
        .unwrap();
    Command::new("git")
        .args(["-C", &p, "config", "user.name", "Test"])
        .output()
        .unwrap();
    Command::new("git")
        .args(["-C", &p, "commit", "--allow-empty", "-m", "init"])
        .output()
        .unwrap();
}

#[test]
fn test_read_locks_auto_v1_default() {
    let dir = tempdir().unwrap();
    let crosslink_dir = dir.path().join(".crosslink");
    let cache_dir = crosslink_dir.join(HUB_CACHE_DIR);
    std::fs::create_dir_all(&cache_dir).unwrap();

    let manager = SyncManager::new(&crosslink_dir).unwrap();
    let locks = manager.read_locks_auto().unwrap();
    assert!(locks.locks.is_empty());
}

#[test]
fn test_read_locks_auto_frozen_v2_is_empty() {
    let dir = tempdir().unwrap();
    let crosslink_dir = dir.path().join(".crosslink");
    let cache_dir = crosslink_dir.join(HUB_CACHE_DIR);
    std::fs::create_dir_all(&cache_dir).unwrap();

    let meta_dir = cache_dir.join("meta");
    std::fs::create_dir_all(&meta_dir).unwrap();
    crate::issue_file::write_layout_version(&meta_dir, 2).unwrap();
    let locks_dir = cache_dir.join("locks");
    std::fs::create_dir_all(&locks_dir).unwrap();
    let lock = crate::issue_file::LockFileV2 {
        issue_id: 3,
        agent_id: "worker-2".to_string(),
        branch: None,
        claimed_at: Utc::now(),
        signed_by: None,
    };
    std::fs::write(
        locks_dir.join("3.json"),
        serde_json::to_string_pretty(&lock).unwrap(),
    )
    .unwrap();

    let manager = SyncManager::new(&crosslink_dir).unwrap();
    let locks = manager.read_locks_auto().unwrap();
    assert!(locks.locks.is_empty());
}

#[test]
fn test_sync_manager_in_worktree_uses_main_hub_cache() {
    let dir = tempdir().unwrap();
    let main_root = dir.path().join("main");
    std::fs::create_dir_all(&main_root).unwrap();
    init_git_repo(&main_root);

    let main_crosslink = main_root.join(".crosslink");
    std::fs::create_dir_all(&main_crosslink).unwrap();

    Command::new("git")
        .args([
            "-C",
            &main_root.to_string_lossy(),
            "branch",
            "feature/hub-test",
        ])
        .output()
        .unwrap();
    let wt_path = main_root.join(".worktrees").join("hub-test");
    std::fs::create_dir_all(wt_path.parent().unwrap()).unwrap();
    Command::new("git")
        .args([
            "-C",
            &main_root.to_string_lossy(),
            "worktree",
            "add",
            &wt_path.to_string_lossy(),
            "feature/hub-test",
        ])
        .output()
        .unwrap();

    let wt_crosslink = wt_path.join(".crosslink");
    std::fs::create_dir_all(&wt_crosslink).unwrap();

    let manager = SyncManager::new(&wt_crosslink).unwrap();

    let expected_parent = main_crosslink.canonicalize().unwrap();
    let actual_parent = manager.cache_dir.parent().unwrap().canonicalize().unwrap();
    assert_eq!(actual_parent, expected_parent);
    assert_eq!(manager.cache_dir.file_name().unwrap(), HUB_CACHE_DIR);

    assert_eq!(
        manager.repo_root.canonicalize().unwrap(),
        main_root.canonicalize().unwrap()
    );
}

fn setup_sync_env() -> (tempfile::TempDir, tempfile::TempDir) {
    let remote_dir = tempfile::tempdir().unwrap();
    let work_dir = tempfile::tempdir().unwrap();

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

    (work_dir, remote_dir)
}

#[test]
fn test_read_tracker_remote_default() {
    let dir = tempdir().unwrap();
    let crosslink_dir = dir.path().join(".crosslink");
    std::fs::create_dir_all(&crosslink_dir).unwrap();

    let remote = read_tracker_remote(&crosslink_dir);
    assert_eq!(remote, "origin");
}

#[test]
fn test_read_tracker_remote_missing_field_defaults_origin() {
    let dir = tempdir().unwrap();
    let crosslink_dir = dir.path().join(".crosslink");
    std::fs::create_dir_all(&crosslink_dir).unwrap();

    std::fs::write(
        crosslink_dir.join("hook-config.json"),
        r#"{"remote":"origin"}"#,
    )
    .unwrap();
    let remote = read_tracker_remote(&crosslink_dir);
    assert_eq!(remote, "origin");
}

#[test]
fn test_read_tracker_remote_custom_value() {
    let dir = tempdir().unwrap();
    let crosslink_dir = dir.path().join(".crosslink");
    std::fs::create_dir_all(&crosslink_dir).unwrap();
    std::fs::write(
        crosslink_dir.join("hook-config.json"),
        r#"{"tracker_remote":"upstream"}"#,
    )
    .unwrap();
    let remote = read_tracker_remote(&crosslink_dir);
    assert_eq!(remote, "upstream");
}

fn add_git_remote(repo: &Path, name: &str) {
    let url = format!("https://example.invalid/{name}.git");
    let status = Command::new("git")
        .current_dir(repo)
        .args(["remote", "add", name, &url])
        .status()
        .expect("git remote add failed to spawn");
    assert!(status.success(), "git remote add {name} failed");
}

#[test]
fn test_read_tracker_remote_single_origin_no_warn() {
    let (work_dir, _remote_dir) = setup_sync_env();
    let crosslink_dir = work_dir.path().join(".crosslink");

    let remote = read_tracker_remote(&crosslink_dir);
    assert_eq!(remote, "origin");
}

#[test]
fn test_read_tracker_remote_single_non_origin_remote() {
    let dir = tempdir().unwrap();
    Command::new("git")
        .current_dir(dir.path())
        .args(["init", "-b", "main"])
        .output()
        .unwrap();
    add_git_remote(dir.path(), "upstream");

    let crosslink_dir = dir.path().join(".crosslink");
    std::fs::create_dir_all(&crosslink_dir).unwrap();

    std::fs::write(crosslink_dir.join("hook-config.json"), "{}").unwrap();

    let remote = read_tracker_remote(&crosslink_dir);
    assert_eq!(
        remote, "upstream",
        "single non-origin remote should be inferred as the tracker_remote"
    );
}

#[test]
fn test_read_tracker_remote_multi_remotes_prefers_origin() {
    let dir = tempdir().unwrap();
    Command::new("git")
        .current_dir(dir.path())
        .args(["init", "-b", "main"])
        .output()
        .unwrap();

    add_git_remote(dir.path(), "zzz");
    add_git_remote(dir.path(), "origin");
    add_git_remote(dir.path(), "fork");

    let crosslink_dir = dir.path().join(".crosslink");
    std::fs::create_dir_all(&crosslink_dir).unwrap();

    let remote = read_tracker_remote(&crosslink_dir);
    assert_eq!(remote, "origin", "origin must win in multi-remote setups");
}

#[test]
fn test_read_tracker_remote_multi_remotes_no_origin_picks_first_alphabetical() {
    let dir = tempdir().unwrap();
    Command::new("git")
        .current_dir(dir.path())
        .args(["init", "-b", "main"])
        .output()
        .unwrap();
    add_git_remote(dir.path(), "upstream");
    add_git_remote(dir.path(), "alice");
    add_git_remote(dir.path(), "bob");

    let crosslink_dir = dir.path().join(".crosslink");
    std::fs::create_dir_all(&crosslink_dir).unwrap();

    let remote = read_tracker_remote(&crosslink_dir);
    assert_eq!(
        remote, "alice",
        "with no origin and multiple remotes, picks the alphabetically first one"
    );
}

#[test]
fn test_read_tracker_remote_explicit_config_wins_over_inference() {
    let dir = tempdir().unwrap();
    Command::new("git")
        .current_dir(dir.path())
        .args(["init", "-b", "main"])
        .output()
        .unwrap();
    add_git_remote(dir.path(), "origin");
    add_git_remote(dir.path(), "upstream");

    let crosslink_dir = dir.path().join(".crosslink");
    std::fs::create_dir_all(&crosslink_dir).unwrap();
    std::fs::write(
        crosslink_dir.join("hook-config.json"),
        r#"{"tracker_remote":"upstream"}"#,
    )
    .unwrap();

    let remote = read_tracker_remote(&crosslink_dir);
    assert_eq!(
        remote, "upstream",
        "explicit hook-config.json value must take precedence over inference"
    );
}

#[test]
fn test_read_tracker_remote_falls_back_when_corrupt_placeholder() {
    let dir = tempdir().unwrap();
    let crosslink_dir = dir.path().join(".crosslink");
    std::fs::create_dir_all(&crosslink_dir).unwrap();
    std::fs::write(
        crosslink_dir.join("hook-config.json"),
        r#"{"tracker_remote":"(text)"}"#,
    )
    .unwrap();
    let remote = read_tracker_remote(&crosslink_dir);
    assert_eq!(
        remote, "origin",
        "corrupt '(text)' placeholder must fall back to 'origin'"
    );
}

#[test]
fn test_sync_manager_new_reads_remote_from_config() {
    let dir = tempdir().unwrap();
    let crosslink_dir = dir.path().join(".crosslink");
    std::fs::create_dir_all(&crosslink_dir).unwrap();
    std::fs::write(
        crosslink_dir.join("hook-config.json"),
        r#"{"tracker_remote":"upstream"}"#,
    )
    .unwrap();
    let manager = SyncManager::new(&crosslink_dir).unwrap();
    assert_eq!(manager.remote(), "upstream");
}

#[test]
fn test_is_v2_layout_false_when_no_meta() {
    let dir = tempdir().unwrap();
    let crosslink_dir = dir.path().join(".crosslink");
    std::fs::create_dir_all(&crosslink_dir).unwrap();
    let manager = SyncManager::new(&crosslink_dir).unwrap();
    assert!(!manager.is_v2_layout());
}

#[test]
fn test_is_v2_layout_true_with_v2_marker() {
    let dir = tempdir().unwrap();
    let crosslink_dir = dir.path().join(".crosslink");
    let cache_dir = crosslink_dir.join(HUB_CACHE_DIR);
    let meta_dir = cache_dir.join("meta");
    std::fs::create_dir_all(&meta_dir).unwrap();
    crate::issue_file::write_layout_version(&meta_dir, 2).unwrap();

    let manager = SyncManager::new(&crosslink_dir).unwrap();
    assert!(manager.is_v2_layout());
}

#[test]
fn test_cache_path_accessor() {
    let dir = tempdir().unwrap();
    let crosslink_dir = dir.path().join(".crosslink");
    std::fs::create_dir_all(&crosslink_dir).unwrap();
    let manager = SyncManager::new(&crosslink_dir).unwrap();
    assert_eq!(manager.cache_path(), manager.cache_dir.as_path());
}

#[test]
fn test_remote_accessor() {
    let dir = tempdir().unwrap();
    let crosslink_dir = dir.path().join(".crosslink");
    std::fs::create_dir_all(&crosslink_dir).unwrap();
    let manager = SyncManager::new(&crosslink_dir).unwrap();
    assert_eq!(manager.remote(), "origin");
}

#[test]
fn test_init_cache_bootstraps_v3() {
    let (work_dir, _remote_dir) = setup_sync_env();
    let crosslink_dir = work_dir.path().join(".crosslink");
    let manager = SyncManager::new(&crosslink_dir).unwrap();

    assert!(!manager.is_initialized());
    manager.init_cache().unwrap();
    assert!(manager.is_initialized());

    assert!(manager.hub_mode().is_v3());
    assert_eq!(
        crate::hub_v3::detect_hub_version(&manager.cache_dir).unwrap(),
        crate::hub_v3::HubVersion::V3 {
            v2_branch_present: false
        }
    );
    assert!(!manager.cache_dir.join("locks.json").exists());
}

#[test]
fn test_init_cache_idempotent() {
    let (work_dir, _remote_dir) = setup_sync_env();
    let crosslink_dir = work_dir.path().join(".crosslink");
    let manager = SyncManager::new(&crosslink_dir).unwrap();

    manager.init_cache().unwrap();

    manager.init_cache().unwrap();
    assert!(manager.is_initialized());
    assert!(manager.hub_mode().is_v3());
}

#[test]
fn test_init_cache_bootstrap_pushes_refs_to_remote() {
    let (work_dir, _remote_dir) = setup_sync_env();
    let crosslink_dir = work_dir.path().join(".crosslink");
    let manager = SyncManager::new(&crosslink_dir).unwrap();
    manager.init_cache().unwrap();

    let remote_version =
        crate::hub_v3::detect_remote_hub_version(&manager.repo_root, "origin").unwrap();
    assert!(matches!(
        remote_version,
        crate::hub_v3::HubVersion::V3 { .. }
    ));
}

#[test]
fn test_init_cache_fresh_clone_joins_v3_remote() {
    let (work_dir, remote_dir) = setup_sync_env();
    let crosslink_dir = work_dir.path().join(".crosslink");
    let manager = SyncManager::new(&crosslink_dir).unwrap();
    manager.init_cache().unwrap();
    assert!(manager.hub_mode().is_v3());

    let work_dir2 = tempfile::tempdir().unwrap();
    Command::new("git")
        .current_dir(work_dir2.path())
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
            .current_dir(work_dir2.path())
            .args(&args)
            .output()
            .unwrap();
    }
    Command::new("git")
        .current_dir(work_dir2.path())
        .args(["fetch", "origin", "main"])
        .output()
        .unwrap();
    Command::new("git")
        .current_dir(work_dir2.path())
        .args(["checkout", "-b", "main", "origin/main"])
        .output()
        .unwrap();

    let crosslink_dir2 = work_dir2.path().join(".crosslink");
    std::fs::create_dir_all(&crosslink_dir2).unwrap();
    std::fs::write(
        crosslink_dir2.join("hook-config.json"),
        r#"{"remote":"origin"}"#,
    )
    .unwrap();

    let manager2 = SyncManager::new(&crosslink_dir2).unwrap();
    manager2.init_cache().unwrap();
    assert!(manager2.is_initialized());
    assert!(manager2.hub_mode().is_v3());
    assert_eq!(
        crate::hub_v3::detect_hub_version(&manager2.cache_dir).unwrap(),
        crate::hub_v3::HubVersion::V3 {
            v2_branch_present: false
        }
    );
}

#[test]
fn test_fresh_v3_hub_create_issue_end_to_end() {
    let (work_dir, _remote_dir) = setup_sync_env();
    let crosslink_dir = work_dir.path().join(".crosslink");
    let agent = make_agent("issuer");
    std::fs::write(
        crosslink_dir.join("agent.json"),
        serde_json::to_string_pretty(&agent).unwrap(),
    )
    .unwrap();

    let sync = SyncManager::new(&crosslink_dir).unwrap();
    sync.init_cache().unwrap();
    assert!(sync.hub_mode().is_v3());
    crate::reconcile::migration::establish_verified_readiness_for_test(&crosslink_dir, "sync-test")
        .unwrap();

    let db = crate::db::Database::open(&crosslink_dir.join("issues.db")).unwrap();
    let writer = crate::shared_writer::SharedWriter::new(&crosslink_dir)
        .unwrap()
        .unwrap();
    let id = writer
        .create_issue(&db, "First v3 issue", None, "high", None, None)
        .unwrap();
    assert_eq!(id, 1, "first reduction-assigned display id must be 1");

    let source = crate::hub_source::RefHubSource::new(sync.cache_path()).unwrap();
    let state = crate::compaction::reduce(&source).unwrap().state;
    assert!(
        state.issues.values().any(|i| i.title == "First v3 issue"),
        "the created issue must surface through the v3 reduction"
    );
}

#[test]
fn test_two_machine_v3_join_round_trip() {
    let (work_dir, remote_dir) = setup_sync_env();
    let cl1 = work_dir.path().join(".crosslink");
    std::fs::write(
        cl1.join("agent.json"),
        serde_json::to_string_pretty(&make_agent("machine-1")).unwrap(),
    )
    .unwrap();
    let sync1 = SyncManager::new(&cl1).unwrap();
    sync1.init_cache().unwrap();
    crate::reconcile::migration::establish_verified_readiness_for_test(&cl1, "sync-test").unwrap();
    let db1 = crate::db::Database::open(&cl1.join("issues.db")).unwrap();
    let w1 = crate::shared_writer::SharedWriter::new(&cl1)
        .unwrap()
        .unwrap();
    w1.create_issue(&db1, "from m1", None, "high", None, None)
        .unwrap();

    let work2 = tempfile::tempdir().unwrap();
    for args in [
        vec!["init", "-b", "main"],
        vec!["config", "user.email", "test@test.local"],
        vec!["config", "user.name", "Test"],
        vec![
            "remote",
            "add",
            "origin",
            remote_dir.path().to_str().unwrap(),
        ],
        vec!["fetch", "origin", "main"],
        vec!["checkout", "-b", "main", "origin/main"],
    ] {
        Command::new("git")
            .current_dir(work2.path())
            .args(&args)
            .output()
            .unwrap();
    }
    let cl2 = work2.path().join(".crosslink");
    std::fs::create_dir_all(&cl2).unwrap();
    std::fs::write(cl2.join("hook-config.json"), r#"{"remote":"origin"}"#).unwrap();
    std::fs::write(
        cl2.join("agent.json"),
        serde_json::to_string_pretty(&make_agent("machine-2")).unwrap(),
    )
    .unwrap();
    let sync2 = SyncManager::new(&cl2).unwrap();
    sync2.init_cache().unwrap();
    assert!(sync2.hub_mode().is_v3(), "m2 must join the v3 hub");
    crate::reconcile::migration::establish_verified_readiness_for_test(&cl2, "sync-test").unwrap();
    let db2 = crate::db::Database::open(&cl2.join("issues.db")).unwrap();
    let w2 = crate::shared_writer::SharedWriter::new(&cl2)
        .unwrap()
        .unwrap();
    w2.create_issue(&db2, "from m2", None, "medium", None, None)
        .unwrap();

    sync1.fetch().unwrap();
    let source = crate::hub_source::RefHubSource::new(sync1.cache_path()).unwrap();
    let state = crate::compaction::reduce(&source).unwrap().state;
    assert!(
        state.issues.values().any(|i| i.title == "from m1"),
        "m1 must still see its own issue"
    );
    assert!(
        state.issues.values().any(|i| i.title == "from m2"),
        "m1 must see m2's issue after fetch"
    );
}

#[test]
fn test_v2_fetch_is_read_only_no_new_commits() {
    let (work_dir, _remote_dir) = setup_sync_env();
    let crosslink_dir = work_dir.path().join(".crosslink");
    let wp = work_dir.path();

    let cache_dir = crosslink_dir.join(HUB_CACHE_DIR);
    for args in [
        vec![
            "worktree",
            "add",
            "--orphan",
            "-b",
            "crosslink/hub",
            cache_dir.to_str().unwrap(),
        ],
        vec![
            "-C",
            cache_dir.to_str().unwrap(),
            "config",
            "user.email",
            "t@t",
        ],
        vec![
            "-C",
            cache_dir.to_str().unwrap(),
            "config",
            "user.name",
            "t",
        ],
    ] {
        Command::new("git")
            .current_dir(wp)
            .args(&args)
            .output()
            .unwrap();
    }
    std::fs::create_dir_all(cache_dir.join("issues")).unwrap();
    std::fs::write(cache_dir.join("locks.json"), "{}").unwrap();
    Command::new("git")
        .current_dir(&cache_dir)
        .args(["add", "-A"])
        .output()
        .unwrap();
    Command::new("git")
        .current_dir(&cache_dir)
        .args(["commit", "-m", "v2 init", "--no-gpg-sign"])
        .output()
        .unwrap();

    let manager = SyncManager::new(&crosslink_dir).unwrap();
    assert!(!manager.hub_mode().is_v3(), "must be a v2 hub");

    let tip_before = String::from_utf8(
        Command::new("git")
            .current_dir(&cache_dir)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();

    manager.fetch().unwrap();

    let tip_after = String::from_utf8(
        Command::new("git")
            .current_dir(&cache_dir)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    assert_eq!(
        tip_before, tip_after,
        "v2 fetch must not create any new commit on the frozen v2 branch"
    );
}

#[test]
fn test_fetch_on_initialized_cache() {
    let (work_dir, _remote_dir) = setup_sync_env();
    let crosslink_dir = work_dir.path().join(".crosslink");
    let manager = SyncManager::new(&crosslink_dir).unwrap();
    manager.init_cache().unwrap();

    manager.fetch().unwrap();
}

#[test]
fn test_fetch_v3_after_bootstrap_pushed_refs() {
    let (work_dir, _remote_dir) = setup_sync_env();
    let crosslink_dir = work_dir.path().join(".crosslink");
    let manager = SyncManager::new(&crosslink_dir).unwrap();

    manager.init_cache().unwrap();
    assert!(manager.hub_mode().is_v3());

    manager.fetch().unwrap();
}

#[test]
fn test_read_allowed_signers_no_file() {
    let dir = tempdir().unwrap();
    let crosslink_dir = dir.path().join(".crosslink");
    let cache_dir = crosslink_dir.join(HUB_CACHE_DIR);
    std::fs::create_dir_all(cache_dir.join("trust")).unwrap();

    let manager = SyncManager::new(&crosslink_dir).unwrap();

    let result = manager.read_allowed_signers();

    let _ = result;
}

fn make_agent(id: &str) -> AgentConfig {
    AgentConfig {
        agent_id: id.to_string(),
        machine_id: "test-host".to_string(),
        description: None,
        role: AgentRole::Driver,
        ssh_key_path: None,
        ssh_fingerprint: None,
        ssh_public_key: None,
    }
}

#[test]
fn test_push_heartbeat_writes_to_agent_ref() {
    let (work_dir, _remote_dir) = setup_sync_env();
    let crosslink_dir = work_dir.path().join(".crosslink");
    let manager = SyncManager::new(&crosslink_dir).unwrap();
    manager.init_cache().unwrap();
    assert!(manager.hub_mode().is_v3(), "fresh hub must bootstrap v3");

    let agent = make_agent("hb-agent");
    manager.push_heartbeat(&agent, Some(42)).unwrap();

    let beats = crate::hub_v3::read_heartbeats_from_refs(&manager.cache_dir).unwrap();
    let hb = beats
        .iter()
        .find(|(id, _)| id == "hb-agent")
        .map(|(_, hb)| hb)
        .expect("heartbeat for hb-agent must be on its agent ref");
    assert_eq!(hb.agent_id, "hb-agent");
    assert_eq!(hb.active_issue_id, Some(42));
}

#[test]
fn test_push_heartbeat_no_change_is_ok() {
    let (work_dir, _remote_dir) = setup_sync_env();
    let crosslink_dir = work_dir.path().join(".crosslink");
    let manager = SyncManager::new(&crosslink_dir).unwrap();
    manager.init_cache().unwrap();

    let agent = make_agent("hb-agent");

    manager.push_heartbeat(&agent, None).unwrap();
    manager.push_heartbeat(&agent, None).unwrap();
}

#[test]
fn test_verify_locks_signature_on_initialized_cache() {
    let (work_dir, _remote_dir) = setup_sync_env();
    let crosslink_dir = work_dir.path().join(".crosslink");
    let manager = SyncManager::new(&crosslink_dir).unwrap();
    manager.init_cache().unwrap();

    let result = manager.verify_locks_signature().unwrap();

    let _ = result;
}

#[test]
fn test_verify_locks_signature_no_commits_on_v3_hub() {
    let (work_dir, _remote_dir) = setup_sync_env();
    let crosslink_dir = work_dir.path().join(".crosslink");
    let manager = SyncManager::new(&crosslink_dir).unwrap();
    manager.init_cache().unwrap();

    let result = manager.verify_locks_signature().unwrap();
    assert!(matches!(
        result,
        crate::sync::SignatureVerification::NoCommits
    ));
}

#[test]
fn test_propagate_agent_hooks_no_src() {
    let (work_dir, _remote_dir) = setup_sync_env();
    let crosslink_dir = work_dir.path().join(".crosslink");
    let manager = SyncManager::new(&crosslink_dir).unwrap();
    manager.init_cache().unwrap();

    manager.propagate_agent_hooks().unwrap();
}

#[test]
fn test_propagate_agent_hooks_copies_files() {
    let (work_dir, _remote_dir) = setup_sync_env();
    let crosslink_dir = work_dir.path().join(".crosslink");
    let manager = SyncManager::new(&crosslink_dir).unwrap();
    manager.init_cache().unwrap();

    let hooks_src = work_dir
        .path()
        .join(".crosslink")
        .join("integrations")
        .join("hooks");
    std::fs::create_dir_all(&hooks_src).unwrap();
    std::fs::write(hooks_src.join("pre-tool-use.sh"), "#!/bin/bash\n").unwrap();

    manager.propagate_agent_hooks().unwrap();

    let hooks_dst = manager
        .cache_dir
        .join(".crosslink")
        .join("integrations")
        .join("hooks");
    assert!(hooks_dst.exists());
    assert!(hooks_dst.join("pre-tool-use.sh").exists());
}

#[test]
fn test_propagate_agent_hooks_idempotent() {
    let (work_dir, _remote_dir) = setup_sync_env();
    let crosslink_dir = work_dir.path().join(".crosslink");
    let manager = SyncManager::new(&crosslink_dir).unwrap();
    manager.init_cache().unwrap();

    let hooks_src = work_dir
        .path()
        .join(".crosslink")
        .join("integrations")
        .join("hooks");
    std::fs::create_dir_all(&hooks_src).unwrap();
    std::fs::write(hooks_src.join("hook.sh"), "#!/bin/bash\n").unwrap();

    manager.propagate_agent_hooks().unwrap();

    manager.propagate_agent_hooks().unwrap();
}

#[test]
fn test_ensure_cache_git_identity_sets_identity() {
    let (work_dir, _remote_dir) = setup_sync_env();
    let crosslink_dir = work_dir.path().join(".crosslink");
    let manager = SyncManager::new(&crosslink_dir).unwrap();
    manager.init_cache().unwrap();

    manager.ensure_cache_git_identity().unwrap();
}

#[test]
fn test_migrate_from_locks_branch_no_old_branch() {
    let (work_dir, _remote_dir) = setup_sync_env();
    let crosslink_dir = work_dir.path().join(".crosslink");
    let manager = SyncManager::new(&crosslink_dir).unwrap();

    let migrated = manager.migrate_from_locks_branch().unwrap();
    assert!(!migrated);
}

#[test]
fn test_configure_signing_no_agent_config() {
    let (work_dir, _remote_dir) = setup_sync_env();
    let crosslink_dir = work_dir.path().join(".crosslink");
    let manager = SyncManager::new(&crosslink_dir).unwrap();
    manager.init_cache().unwrap();

    manager.configure_signing(&crosslink_dir).unwrap();
}

#[test]
fn test_configure_signing_cache_not_exists() {
    let dir = tempdir().unwrap();
    let crosslink_dir = dir.path().join(".crosslink");
    std::fs::create_dir_all(&crosslink_dir).unwrap();
    let manager = SyncManager::new(&crosslink_dir).unwrap();

    manager.configure_signing(&crosslink_dir).unwrap();
}

#[test]
fn test_ensure_agent_key_published_no_cache() {
    let dir = tempdir().unwrap();
    let crosslink_dir = dir.path().join(".crosslink");
    std::fs::create_dir_all(&crosslink_dir).unwrap();
    let manager = SyncManager::new(&crosslink_dir).unwrap();

    let published = manager.ensure_agent_key_published(&crosslink_dir).unwrap();
    assert!(!published);
}

#[test]
fn test_ensure_agent_key_published_no_agent_config() {
    let (work_dir, _remote_dir) = setup_sync_env();
    let crosslink_dir = work_dir.path().join(".crosslink");
    let manager = SyncManager::new(&crosslink_dir).unwrap();
    manager.init_cache().unwrap();

    let published = manager.ensure_agent_key_published(&crosslink_dir).unwrap();
    assert!(!published);
}
