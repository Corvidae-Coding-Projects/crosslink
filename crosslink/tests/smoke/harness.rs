#![allow(dead_code)]

use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;

#[derive(Debug)]
pub struct CmdResult {
    pub success: bool,
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl CmdResult {
    pub fn stdout_contains(&self, expected: &str) -> bool {
        self.stdout.contains(expected)
    }

    pub fn stderr_contains(&self, expected: &str) -> bool {
        self.stderr.contains(expected)
    }
}

pub struct SmokeHarness {
    pub temp_dir: TempDir,
    pub crosslink_bin: PathBuf,
    server_handle: Option<Child>,
    pub server_port: Option<u16>,

    pub server_auth_token: Option<String>,
    pub agent_id: String,

    bare_remote: Option<PathBuf>,

    _remote_dir: Option<TempDir>,
}

impl SmokeHarness {
    pub fn new() -> Self {
        let temp_dir = TempDir::new().expect("failed to create temp dir");
        let remote_dir = TempDir::new().expect("failed to create remote temp dir");
        let bin = PathBuf::from(env!("CARGO_BIN_EXE_crosslink"));

        let out = Command::new("git")
            .current_dir(remote_dir.path())
            .args(["init", "--bare", "-b", "main"])
            .output()
            .expect("git init --bare failed to execute");
        assert!(out.status.success(), "git init --bare failed");

        let out = Command::new("git")
            .current_dir(temp_dir.path())
            .args(["init", "-b", "main"])
            .output()
            .expect("git init failed to execute");
        assert!(out.status.success(), "git init failed");

        for args in [
            vec!["config", "user.email", "smoke@test.local"],
            vec!["config", "user.name", "Smoke Test"],
            vec![
                "remote",
                "add",
                "origin",
                remote_dir
                    .path()
                    .to_str()
                    .expect("remote path not valid UTF-8"),
            ],
        ] {
            let out = Command::new("git")
                .current_dir(temp_dir.path())
                .args(&args)
                .output()
                .expect("git config/remote failed to execute");
            assert!(out.status.success(), "git {args:?} failed");
        }

        std::fs::write(temp_dir.path().join("README.md"), "# smoke\n")
            .expect("failed to write README.md");
        let _ = Command::new("git")
            .current_dir(temp_dir.path())
            .args(["add", "README.md"])
            .output();
        let out = Command::new("git")
            .current_dir(temp_dir.path())
            .args(["commit", "-m", "initial", "--no-gpg-sign"])
            .output()
            .expect("git commit failed to execute");
        assert!(out.status.success(), "initial git commit failed");
        let out = Command::new("git")
            .current_dir(temp_dir.path())
            .args(["push", "-u", "origin", "main"])
            .output()
            .expect("git push failed to execute");
        assert!(out.status.success(), "initial git push failed");

        let out = Command::new(&bin)
            .current_dir(temp_dir.path())
            .args(["init", "--defaults", "--skip-cpitd", "--skip-signing"])
            .output()
            .expect("crosslink init failed to execute");
        assert!(
            out.status.success(),
            "crosslink init failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );

        let bare_remote = Some(remote_dir.path().to_path_buf());

        SmokeHarness {
            temp_dir,
            crosslink_bin: bin,
            server_handle: None,
            server_port: None,
            server_auth_token: None,
            agent_id: "smoke-primary".to_string(),
            bare_remote,
            _remote_dir: Some(remote_dir),
        }
    }

    pub fn new_bare() -> Self {
        let temp_dir = TempDir::new().expect("failed to create temp dir");
        let bin = PathBuf::from(env!("CARGO_BIN_EXE_crosslink"));
        SmokeHarness {
            temp_dir,
            crosslink_bin: bin,
            server_handle: None,
            server_port: None,
            server_auth_token: None,
            agent_id: "smoke-bare".to_string(),
            bare_remote: None,
            _remote_dir: None,
        }
    }

    pub fn run(&self, args: &[&str]) -> CmdResult {
        let output = Command::new(&self.crosslink_bin)
            .current_dir(self.temp_dir.path())
            .args(args)
            .output()
            .expect("failed to execute crosslink");

        CmdResult {
            success: output.status.success(),
            exit_code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        }
    }

    pub fn run_ok(&self, args: &[&str]) -> CmdResult {
        let result = self.run(args);
        assert!(
            result.success,
            "expected crosslink {:?} to succeed but got exit code {}.\nstdout: {}\nstderr: {}",
            args, result.exit_code, result.stdout, result.stderr,
        );
        result
    }

    pub fn run_err(&self, args: &[&str]) -> CmdResult {
        let result = self.run(args);
        assert!(
            !result.success,
            "expected crosslink {:?} to fail but it succeeded.\nstdout: {}\nstderr: {}",
            args, result.stdout, result.stderr,
        );
        result
    }

    pub fn crosslink_dir(&self) -> PathBuf {
        self.temp_dir.path().join(".crosslink")
    }

    pub fn db_path(&self) -> PathBuf {
        self.crosslink_dir().join("issues.db")
    }

    pub fn start_server(&mut self) -> u16 {
        let port = {
            let listener = TcpListener::bind("127.0.0.1:0").expect("failed to bind to a free port");
            listener
                .local_addr()
                .expect("failed to get local addr")
                .port()
        };

        let mut child = Command::new(&self.crosslink_bin)
            .current_dir(self.temp_dir.path())
            .args(["serve", "--port", &port.to_string()])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn crosslink serve");

        if let Some(stdout) = child.stdout.take() {
            use std::io::BufRead;
            let reader = std::io::BufReader::new(stdout);
            let deadline = Instant::now() + Duration::from_secs(10);
            for line in reader.lines() {
                if Instant::now() > deadline {
                    break;
                }
                let Ok(line) = line else { break };
                if let Some(token) = line.trim().strip_prefix("Auth:") {
                    let token = token.trim();
                    if let Some(token) = token.strip_prefix("Bearer ") {
                        self.server_auth_token = Some(token.to_string());
                    }
                    break;
                }
            }
        }

        self.server_handle = Some(child);
        self.server_port = Some(port);

        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if Instant::now() > deadline {
                self.stop_server();
                panic!("crosslink serve did not become ready within 10 seconds on port {port}");
            }
            if std::net::TcpStream::connect_timeout(
                &format!("127.0.0.1:{port}").parse().unwrap(),
                Duration::from_millis(100),
            )
            .is_ok()
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        port
    }

    pub fn stop_server(&mut self) {
        if let Some(mut child) = self.server_handle.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.server_port = None;
    }

    pub fn fork_agent(&self, agent_id: &str) -> SmokeHarness {
        let remote_path = self
            .bare_remote
            .as_ref()
            .expect("cannot fork_agent from a bare harness (no remote)");

        let temp_dir = TempDir::new().expect("failed to create temp dir for fork");

        let out = Command::new("git")
            .current_dir(temp_dir.path())
            .args(["init", "-b", "main"])
            .output()
            .expect("git init failed");
        assert!(out.status.success(), "git init for fork failed");

        for args in [
            vec!["config", "user.email", &format!("{agent_id}@test.local")],
            vec!["config", "user.name", agent_id],
            vec![
                "remote",
                "add",
                "origin",
                remote_path.to_str().expect("remote path not valid UTF-8"),
            ],
        ] {
            let out = Command::new("git")
                .current_dir(temp_dir.path())
                .args(&args)
                .output()
                .expect("git config/remote failed");
            assert!(out.status.success(), "git {args:?} failed for fork");
        }

        let out = Command::new("git")
            .current_dir(temp_dir.path())
            .args(["fetch", "origin"])
            .output()
            .expect("git fetch failed");
        assert!(out.status.success(), "git fetch for fork failed");

        let out = Command::new("git")
            .current_dir(temp_dir.path())
            .args(["reset", "--hard", "origin/main"])
            .output()
            .expect("git reset failed");
        assert!(out.status.success(), "git reset for fork failed");

        let out = Command::new("git")
            .current_dir(temp_dir.path())
            .args(["branch", "--set-upstream-to=origin/main", "main"])
            .output()
            .expect("git branch --set-upstream-to failed");
        assert!(out.status.success(), "set upstream for fork failed");

        let bin = PathBuf::from(env!("CARGO_BIN_EXE_crosslink"));
        let out = Command::new(&bin)
            .current_dir(temp_dir.path())
            .args(["init", "--defaults", "--skip-cpitd", "--skip-signing"])
            .output()
            .expect("crosslink init failed for fork");
        assert!(
            out.status.success(),
            "crosslink init failed for fork: {}",
            String::from_utf8_lossy(&out.stderr)
        );

        SmokeHarness {
            temp_dir,
            crosslink_bin: bin,
            server_handle: None,
            server_port: None,
            server_auth_token: None,
            agent_id: agent_id.to_string(),
            bare_remote: Some(remote_path.clone()),
            _remote_dir: None,
        }
    }
}

impl Drop for SmokeHarness {
    fn drop(&mut self) {
        self.stop_server();
    }
}

pub fn assert_stdout_contains(result: &CmdResult, expected: &str) {
    assert!(
        result.stdout_contains(expected),
        "expected stdout to contain {:?} but got:\n{}",
        expected,
        result.stdout,
    );
}

pub fn assert_stderr_contains(result: &CmdResult, expected: &str) {
    assert!(
        result.stderr_contains(expected),
        "expected stderr to contain {:?} but got:\n{}",
        expected,
        result.stderr,
    );
}

pub fn assert_issue_count(harness: &SmokeHarness, status: &str, expected: usize) {
    let result = harness.run_ok(&["issue", "list", "-s", status, "--json"]);

    let parsed: serde_json::Value = serde_json::from_str(&result.stdout).unwrap_or_else(|e| {
        panic!(
            "failed to parse issue list JSON: {}\nstdout was:\n{}",
            e, result.stdout
        )
    });
    let count = parsed
        .as_array()
        .map(|a| a.len())
        .unwrap_or_else(|| panic!("expected JSON array, got: {}", result.stdout));
    assert_eq!(
        count, expected,
        "expected {} issues with status {:?}, got {}.\nJSON:\n{}",
        expected, status, count, result.stdout,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_harness_new() {
        let h = SmokeHarness::new();
        assert!(h.crosslink_dir().exists());
        assert!(h.db_path().exists());
    }

    #[test]
    fn test_harness_run_ok() {
        let h = SmokeHarness::new();
        let result = h.run_ok(&["issue", "list"]);
        assert!(result.success);
    }

    #[test]
    fn test_harness_run_err() {
        let h = SmokeHarness::new();
        let result = h.run_err(&["issue", "show", "99999"]);
        assert!(!result.success);
    }

    #[test]
    fn test_harness_bare_no_crosslink_dir() {
        let h = SmokeHarness::new_bare();
        assert!(!h.crosslink_dir().exists());
    }

    #[test]
    fn test_harness_create_and_list() {
        let h = SmokeHarness::new();
        h.run_ok(&["issue", "create", "Test issue from harness"]);
        let result = h.run_ok(&["issue", "list"]);
        assert!(result.stdout_contains("Test issue from harness"));
    }

    #[test]
    fn test_cmd_result_helpers() {
        let result = CmdResult {
            success: true,
            exit_code: 0,
            stdout: "hello world".to_string(),
            stderr: "warning: something".to_string(),
        };
        assert!(result.stdout_contains("hello"));
        assert!(!result.stdout_contains("goodbye"));
        assert!(result.stderr_contains("warning"));
        assert!(!result.stderr_contains("error"));
    }

    #[test]
    fn test_assert_stdout_contains() {
        let result = CmdResult {
            success: true,
            exit_code: 0,
            stdout: "Created issue #1".to_string(),
            stderr: String::new(),
        };
        assert_stdout_contains(&result, "Created issue");
    }

    #[test]
    fn test_fork_agent() {
        let h = SmokeHarness::new();
        let h2 = h.fork_agent("agent-b");
        assert!(h2.crosslink_dir().exists());
        assert!(h2.db_path().exists());
        assert_eq!(h2.agent_id, "agent-b");

        assert_ne!(h.temp_dir.path(), h2.temp_dir.path(),);
    }
}
