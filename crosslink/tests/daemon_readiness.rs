use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

const CLI_PROCESS_TIMEOUT: Duration = Duration::from_secs(15);
const DAEMON_PROCESS_TIMEOUT: Duration = Duration::from_secs(15);

fn run(directory: &Path, program: &str, arguments: &[&str]) -> Output {
    let output = Command::new(program)
        .current_dir(directory)
        .args(arguments)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{program} {arguments:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn wait_output(mut child: Child, timeout: Duration) -> Output {
    let deadline = Instant::now() + timeout;
    loop {
        if child.try_wait().unwrap().is_some() {
            return child.wait_with_output().unwrap();
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let output = child.wait_with_output().unwrap();
            panic!(
                "crosslink process exceeded {timeout:?}: stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn crosslink(directory: &Path, arguments: &[&str]) -> Output {
    wait_output(
        Command::new(env!("CARGO_BIN_EXE_crosslink"))
            .current_dir(directory)
            .args(arguments)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
        CLI_PROCESS_TIMEOUT,
    )
}

fn parse(output: &Output) -> Value {
    let value: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "invalid daemon JSON: {error}; stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    });
    let mut fields = value
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    fields.sort();
    assert_eq!(
        fields,
        [
            "attempt_id",
            "daemon_epoch",
            "daemon_pid",
            "evidence_path",
            "evidence_sha256",
            "generation_id",
            "protocol_version",
            "ready",
            "reason",
            "repository_id",
            "running",
            "schema_version",
            "state",
            "updated_at",
        ]
    );
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["protocol_version"], 1);
    value
}

fn file_snapshot(path: &Path) -> Vec<(String, Vec<u8>)> {
    fn collect(root: &Path, path: &Path, files: &mut Vec<(String, Vec<u8>)>) {
        let mut entries = std::fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        entries.sort();
        for entry in entries {
            if entry.is_dir() {
                collect(root, &entry, files);
            } else if entry.is_file() {
                files.push((
                    entry.strip_prefix(root).unwrap().display().to_string(),
                    std::fs::read(entry).unwrap(),
                ));
            }
        }
    }
    let mut files = Vec::new();
    collect(path, path, &mut files);
    files
}

fn repository() -> (tempfile::TempDir, tempfile::TempDir) {
    let remote = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    run(remote.path(), "git", &["init", "--bare", "-b", "main"]);
    run(work.path(), "git", &["init", "-b", "main"]);
    run(
        work.path(),
        "git",
        &["config", "user.email", "test@example.invalid"],
    );
    run(work.path(), "git", &["config", "user.name", "Test"]);
    std::fs::write(work.path().join("README.md"), "test").unwrap();
    run(work.path(), "git", &["add", "README.md"]);
    run(
        work.path(),
        "git",
        &["commit", "-m", "initial", "--no-gpg-sign"],
    );
    run(
        work.path(),
        "git",
        &["remote", "add", "origin", remote.path().to_str().unwrap()],
    );
    run(work.path(), "git", &["push", "-u", "origin", "main"]);
    let init = crosslink(
        work.path(),
        &["init", "--defaults", "--skip-cpitd", "--skip-signing"],
    );
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    (work, remote)
}

fn fresh_local_repository() -> tempfile::TempDir {
    let work = tempfile::tempdir().unwrap();
    run(work.path(), "git", &["init", "-b", "main"]);
    run(
        work.path(),
        "git",
        &["config", "user.email", "test@example.invalid"],
    );
    run(work.path(), "git", &["config", "user.name", "Test"]);
    std::fs::write(work.path().join("README.md"), "test").unwrap();
    run(work.path(), "git", &["add", "README.md"]);
    run(
        work.path(),
        "git",
        &["commit", "-m", "initial", "--no-gpg-sign"],
    );
    let crosslink = work.path().join(".crosslink");
    std::fs::create_dir(&crosslink).unwrap();
    std::fs::write(crosslink.join("hook-config.json"), "{}").unwrap();
    crosslink::identity::AgentConfig::init(&crosslink, "local-agent", None).unwrap();
    work
}

fn ensure_child(directory: &Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_crosslink"))
        .current_dir(directory)
        .args(["--json", "daemon", "ensure", "--wait-ready"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

#[cfg(unix)]
fn ensure_child_in_own_group(directory: &Path) -> Child {
    use std::os::unix::process::CommandExt;

    let mut command = Command::new(env!("CARGO_BIN_EXE_crosslink"));
    command
        .current_dir(directory)
        .args(["--json", "daemon", "ensure", "--wait-ready"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    command.spawn().unwrap()
}

struct DaemonCleanup {
    directory: PathBuf,
}

#[test]
fn fresh_hook_initialized_local_repository_bootstraps_to_ready() {
    let work = fresh_local_repository();
    let _cleanup = DaemonCleanup::new(work.path());
    let ready = wait_output(ensure_child(work.path()), DAEMON_PROCESS_TIMEOUT);
    assert!(
        ready.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&ready.stdout),
        String::from_utf8_lossy(&ready.stderr)
    );
    let ready = parse(&ready);
    assert_eq!(ready["ready"], true);
    assert!(matches!(
        ready["state"].as_str(),
        Some("ready_current" | "ready_migrated" | "ready_adopted")
    ));
    assert!(work.path().join(".crosslink/issues.db").is_file());
    assert!(work.path().join(".crosslink/.hub-cache").is_dir());
}

#[cfg(unix)]
#[test]
fn daemon_survives_launcher_process_group_cleanup() {
    let (work, _remote) = repository();
    let _cleanup = DaemonCleanup::new(work.path());
    let ensure = ensure_child_in_own_group(work.path());
    let launcher_group = ensure.id();
    let ready = wait_output(ensure, DAEMON_PROCESS_TIMEOUT);
    assert!(
        ready.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&ready.stdout),
        String::from_utf8_lossy(&ready.stderr)
    );
    let ready = parse(&ready);
    let group = format!("-{launcher_group}");
    let _ = Command::new("kill")
        .args(["-TERM", "--", &group])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    thread::sleep(Duration::from_millis(100));
    let status = crosslink(work.path(), &["--json", "daemon", "status"]);
    assert!(status.status.success());
    let status = parse(&status);
    assert_eq!(status["running"], true);
    assert_eq!(status["daemon_pid"], ready["daemon_pid"]);
    let created = crosslink(
        work.path(),
        &["issue", "create", "launcher cleanup survived", "--json"],
    );
    assert!(
        created.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&created.stdout),
        String::from_utf8_lossy(&created.stderr)
    );
}

impl DaemonCleanup {
    fn new(directory: &Path) -> Self {
        Self {
            directory: directory.to_path_buf(),
        }
    }
}

impl Drop for DaemonCleanup {
    fn drop(&mut self) {
        let cleanup = |arguments: &[&str]| -> Option<Output> {
            let mut child = Command::new(env!("CARGO_BIN_EXE_crosslink"))
                .current_dir(&self.directory)
                .args(arguments)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .ok()?;
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if child.try_wait().ok()?.is_some() {
                    return child.wait_with_output().ok();
                }
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    return child.wait_with_output().ok();
                }
                thread::sleep(Duration::from_millis(20));
            }
        };
        let _ = cleanup(&["daemon", "stop"]);
        let still_running = cleanup(&["--json", "daemon", "status"])
            .and_then(|output| serde_json::from_slice::<Value>(&output.stdout).ok())
            .is_some_and(|status| status["running"] == true);
        if still_running {
            let _ = cleanup(&["daemon", "stop"]);
        }
    }
}

#[test]
fn concurrent_ensure_converges_and_offline_exit_is_structured() {
    let (work, remote) = repository();
    let _cleanup = DaemonCleanup::new(work.path());
    let offline = remote
        .path()
        .parent()
        .unwrap()
        .join(format!("crosslink-offline-{}", std::process::id()));
    std::fs::rename(remote.path(), &offline).unwrap();
    let waiting_a = ensure_child(work.path());
    let waiting_b = ensure_child(work.path());
    let waiting_a = wait_output(waiting_a, DAEMON_PROCESS_TIMEOUT);
    let waiting_b = wait_output(waiting_b, DAEMON_PROCESS_TIMEOUT);
    assert_eq!(waiting_a.status.code(), Some(20));
    assert_eq!(waiting_b.status.code(), Some(20));
    let waiting_a = parse(&waiting_a);
    let waiting_b = parse(&waiting_b);
    assert_eq!(waiting_a["state"], "waiting_for_remote");
    assert_eq!(waiting_a["ready"], false);
    assert_eq!(waiting_a["daemon_epoch"], waiting_b["daemon_epoch"]);
    assert_eq!(waiting_a["daemon_pid"], waiting_b["daemon_pid"]);
    let waiting_evidence = PathBuf::from(waiting_a["evidence_path"].as_str().unwrap());
    assert!(waiting_evidence.is_file());
    let refs_before = run(
        work.path(),
        "git",
        &["for-each-ref", "--format=%(refname) %(objectname)"],
    )
    .stdout;
    let database_before = std::fs::read(work.path().join(".crosslink/issues.db")).unwrap();
    let session_before = std::fs::read(work.path().join(".crosslink/session.json")).ok();
    let diagnostic = crosslink(work.path(), &["--json", "issue", "list"]);
    assert!(
        diagnostic.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&diagnostic.stdout),
        String::from_utf8_lossy(&diagnostic.stderr)
    );
    let mutation = crosslink(work.path(), &["issue", "create", "must remain blocked"]);
    assert!(!mutation.status.success());
    assert!(String::from_utf8_lossy(&mutation.stderr).contains("waiting_for_remote"));
    let refs_after = run(
        work.path(),
        "git",
        &["for-each-ref", "--format=%(refname) %(objectname)"],
    )
    .stdout;
    assert_eq!(refs_after, refs_before);
    assert_eq!(
        std::fs::read(work.path().join(".crosslink/issues.db")).unwrap(),
        database_before
    );
    assert_eq!(
        std::fs::read(work.path().join(".crosslink/session.json")).ok(),
        session_before
    );
    std::fs::rename(&offline, remote.path()).unwrap();
    let recovery_deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let recovery = wait_output(ensure_child(work.path()), DAEMON_PROCESS_TIMEOUT);
        if recovery.status.success() {
            break;
        }
        assert_eq!(recovery.status.code(), Some(20));
        assert!(
            Instant::now() < recovery_deadline,
            "daemon did not recover after remote restore"
        );
        thread::sleep(Duration::from_millis(50));
    }
    let ready_a = ensure_child(work.path());
    let ready_b = ensure_child(work.path());
    let ready_a = wait_output(ready_a, DAEMON_PROCESS_TIMEOUT);
    let ready_b = wait_output(ready_b, DAEMON_PROCESS_TIMEOUT);
    assert!(
        ready_a.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&ready_a.stdout),
        String::from_utf8_lossy(&ready_a.stderr)
    );
    assert!(
        ready_b.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&ready_b.stdout),
        String::from_utf8_lossy(&ready_b.stderr)
    );
    let ready_a = parse(&ready_a);
    let ready_b = parse(&ready_b);
    assert_eq!(ready_a["ready"], true);
    assert_eq!(ready_a["daemon_epoch"], ready_b["daemon_epoch"]);
    assert_eq!(ready_a["daemon_pid"], ready_b["daemon_pid"]);
    assert!(waiting_evidence.is_file());
    let malformed = work
        .path()
        .join(".crosslink/readiness/99999999999999999999-corrupt.json");
    std::fs::write(&malformed, b"{").unwrap();
    let status = crosslink(work.path(), &["--json", "daemon", "status"]);
    assert!(!status.status.success());
    let status = parse(&status);
    assert!(status["state"].is_null());
    assert_eq!(status["ready"], false);
    assert_eq!(status["running"], false);
    assert!(status["reason"]
        .as_str()
        .is_some_and(|value| value.contains("parsing")));
    std::fs::remove_file(malformed).unwrap();
}

#[test]
fn daemon_status_outside_a_repository_returns_the_closed_error_envelope() {
    let directory = tempfile::tempdir().unwrap();
    let status = crosslink(directory.path(), &["--json", "daemon", "status"]);
    assert!(!status.status.success());
    let status = parse(&status);
    assert!(status["state"].is_null());
    assert_eq!(status["ready"], false);
    assert_eq!(status["running"], false);
    assert!(status["repository_id"].is_null());
    assert!(status["reason"]
        .as_str()
        .is_some_and(|reason| reason.contains("Not a crosslink repository")));
}

#[test]
fn corrupt_projection_exit_is_structured_and_daemon_remains_reportable() {
    let (work, _remote) = repository();
    let _cleanup = DaemonCleanup::new(work.path());
    let projection = work.path().join(".crosslink/issues.db");
    std::fs::write(&projection, b"truncated sqlite").unwrap();
    let blocked = wait_output(ensure_child(work.path()), DAEMON_PROCESS_TIMEOUT);
    assert_eq!(blocked.status.code(), Some(21));
    let blocked = parse(&blocked);
    assert_eq!(blocked["state"], "blocked_corrupt");
    assert_eq!(blocked["ready"], false);
    assert_eq!(blocked["running"], true);
    let evidence = PathBuf::from(blocked["evidence_path"].as_str().unwrap());
    assert!(evidence.is_file());
    let status = crosslink(work.path(), &["--json", "daemon", "status"]);
    assert!(status.status.success());
    let status = parse(&status);
    assert_eq!(status["state"], "blocked_corrupt");
    assert_eq!(std::fs::read(projection).unwrap(), b"truncated sqlite");
}

#[test]
fn diagnostic_commands_leave_waiting_and_blocked_repositories_byte_unchanged() {
    for state in [
        crosslink::reconcile::readiness::ReadinessState::WaitingForRemote,
        crosslink::reconcile::readiness::ReadinessState::BlockedCorrupt,
    ] {
        let (work, _remote) = repository();
        let crosslink_dir = work.path().join(".crosslink");
        let db = crosslink::db::Database::open(&crosslink_dir.join("issues.db")).unwrap();
        db.create_issue("diagnostic fixture", None, "medium")
            .unwrap();
        drop(db);
        crosslink::sync::SyncManager::new(&crosslink_dir)
            .unwrap()
            .init_cache()
            .unwrap();
        let readiness_dir = crosslink_dir.join("readiness");
        let _ = std::fs::remove_dir_all(&readiness_dir);
        for name in ["daemon.pid", "daemon.run.lock", "daemon.start.lock"] {
            let _ = std::fs::remove_file(crosslink_dir.join(name));
        }
        let identity = crosslink::reconcile::readiness::DaemonIdentity {
            schema_version: crosslink::reconcile::readiness::READINESS_SCHEMA_VERSION,
            repository_id: crosslink::reconcile::readiness::repository_id(&crosslink_dir).unwrap(),
            daemon_epoch: format!("diagnostic-no-write-{state:?}"),
            pid: std::process::id(),
            process_start: crosslink::reconcile::readiness::current_process_start_token().unwrap(),
        };
        crosslink::reconcile::readiness::write_daemon_identity(&crosslink_dir, &identity).unwrap();
        crosslink::reconcile::readiness::write_record(
            &crosslink_dir,
            crosslink::reconcile::readiness::ReadinessDraft {
                daemon_epoch: &identity.daemon_epoch,
                daemon_pid: identity.pid,
                attempt_id: "diagnostic-no-write",
                state,
                generation_id: None,
                reason: Some("repository is unavailable"),
            },
        )
        .unwrap();
        let import_dir = work.path().join("diagnostic-import");
        std::fs::create_dir(&import_dir).unwrap();
        std::fs::write(import_dir.join("page.md"), "# Page\n").unwrap();
        let before = file_snapshot(work.path());
        let commands = [
            (vec!["issue", "next"], None),
            (vec!["locks", "list"], None),
            (vec!["locks", "check", "1"], None),
            (vec!["agent", "status"], None),
            (vec!["prune", "--dry-run"], None),
            (vec!["kickoff", "status"], None),
            (vec!["kickoff", "list"], None),
            (vec!["kickoff", "logs", "missing-agent"], None),
            (vec!["swarm", "list"], None),
            (vec!["style", "diff"], Some("No house style configured")),
            (
                vec![
                    "knowledge",
                    "import",
                    import_dir.to_str().unwrap(),
                    "--dry-run",
                ],
                Some("Knowledge cache is unavailable"),
            ),
            (vec!["milestone", "list"], None),
        ];
        for (arguments, prerequisite_error) in commands {
            let output = crosslink(work.path(), &arguments);
            let stderr = String::from_utf8_lossy(&output.stderr);
            if let Some(expected) = prerequisite_error {
                assert!(
                    output.status.success() || stderr.contains(expected),
                    "diagnostic {arguments:?} returned an unexpected error: {stderr}"
                );
            } else {
                assert!(
                    output.status.success(),
                    "diagnostic {arguments:?} failed: {stderr}"
                );
            }
            assert_eq!(
                file_snapshot(work.path()),
                before,
                "diagnostic {arguments:?} mutated repository state in {state:?}"
            );
        }
    }
}
