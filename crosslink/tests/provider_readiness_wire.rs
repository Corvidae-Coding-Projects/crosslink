use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::Command;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn python() -> &'static str {
    if cfg!(windows) {
        "python"
    } else {
        "python3"
    }
}

fn validate(value: &Value) -> (bool, Option<String>) {
    let code = r#"
import json, sys
sys.path.insert(0, sys.argv[1])
from hook_protocol import validate_readiness_envelope
valid, reason = validate_readiness_envelope(json.loads(sys.argv[2]))
print(json.dumps([valid, reason]))
"#;
    let output = Command::new(python())
        .args([
            "-c",
            code,
            root().join("resources/agent/hooks").to_str().unwrap(),
            &value.to_string(),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn envelope(state: Option<&str>, ready: bool, running: bool) -> Value {
    let evidence = matches!(state, Some("waiting_for_remote" | "blocked_corrupt"));
    json!({
        "schema_version": 1,
        "protocol_version": 1,
        "state": state,
        "ready": ready,
        "running": running,
        "repository_id": running.then_some("repository"),
        "daemon_epoch": running.then_some("epoch"),
        "daemon_pid": running.then_some(42),
        "attempt_id": running.then_some("attempt"),
        "generation_id": ready.then_some("generation"),
        "updated_at": running.then_some("2026-09-01T00:00:00Z"),
        "reason": evidence.then_some("not ready"),
        "evidence_path": evidence.then_some("evidence.json"),
        "evidence_sha256": evidence.then_some("0".repeat(64)),
    })
}

#[test]
fn provider_wire_accepts_the_shared_terminal_and_daemon_liveness_envelopes() {
    for (state, ready) in [
        ("ready_current", true),
        ("ready_migrated", true),
        ("ready_adopted", true),
        ("waiting_for_remote", false),
        ("blocked_corrupt", false),
    ] {
        assert_eq!(validate(&envelope(Some(state), ready, true)), (true, None));
    }
    assert_eq!(validate(&envelope(None, false, true)), (true, None));
    assert_eq!(validate(&envelope(None, false, false)), (true, None));
    let mut error = envelope(None, false, false);
    error["reason"] = Value::String("status failed".to_string());
    assert_eq!(validate(&error), (true, None));
}

#[test]
fn provider_wire_rejects_unknown_fields_versions_and_inconsistent_states() {
    let mut cases = Vec::new();
    let mut unknown = envelope(Some("ready_current"), true, true);
    unknown["unexpected"] = Value::Bool(true);
    cases.push(unknown);
    let mut version = envelope(Some("ready_current"), true, true);
    version["protocol_version"] = Value::from(2);
    cases.push(version);
    let mut inconsistent = envelope(Some("waiting_for_remote"), false, true);
    inconsistent["ready"] = Value::Bool(true);
    cases.push(inconsistent);
    let mut digest = envelope(Some("blocked_corrupt"), false, true);
    digest["evidence_sha256"] = Value::String("ABC".to_string());
    cases.push(digest);
    let mut stopped = envelope(None, false, false);
    stopped["daemon_pid"] = Value::from(42);
    cases.push(stopped);
    for value in cases {
        let (valid, reason) = validate(&value);
        assert!(!valid, "{value}");
        assert!(reason.is_some());
    }
}

#[test]
fn both_managed_hooks_use_the_shared_validator() {
    for script in ["session-start.py", "work-check.py"] {
        let body =
            std::fs::read_to_string(root().join("resources/agent/hooks").join(script)).unwrap();
        assert!(body.contains("validate_readiness_envelope"), "{script}");
    }
}

#[cfg(windows)]
mod windows {
    use super::*;
    use std::io::Write;
    use std::process::{Child, Output, Stdio};
    use std::time::{Duration, Instant};

    fn copy_directory(source: &std::path::Path, destination: &std::path::Path) {
        std::fs::create_dir_all(destination).unwrap();
        for entry in std::fs::read_dir(source).unwrap() {
            let entry = entry.unwrap();
            let destination = destination.join(entry.file_name());
            if entry.path().is_dir() {
                copy_directory(&entry.path(), &destination);
            } else {
                std::fs::copy(entry.path(), destination).unwrap();
            }
        }
    }

    fn wait_output(mut child: Child) -> Output {
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            if child.try_wait().unwrap().is_some() {
                return child.wait_with_output().unwrap();
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let output = child.wait_with_output().unwrap();
                panic!(
                    "provider wrapper timed out: stdout={} stderr={}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn wrapper(provider: &str, script: &str) -> String {
        let path = root().join(match provider {
            "claude" => "resources/providers/claude/settings.json",
            "codex" => "resources/providers/codex/hooks.json",
            _ => unreachable!(),
        });
        let value: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        value["hooks"]
            .as_object()
            .unwrap()
            .values()
            .flat_map(|groups| groups.as_array().unwrap())
            .flat_map(|group| group["hooks"].as_array().unwrap())
            .find(|hook| {
                hook["commandWindows"]
                    .as_str()
                    .is_some_and(|command| command.contains(script))
            })
            .and_then(|hook| hook["commandWindows"].as_str())
            .unwrap()
            .to_string()
    }

    fn fixture() -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        assert!(Command::new("git")
            .current_dir(directory.path())
            .args(["init", "-b", "main"])
            .status()
            .unwrap()
            .success());
        let crosslink = directory.path().join(".crosslink");
        let hooks = crosslink.join("integrations/hooks");
        copy_directory(&root().join("resources/agent/hooks"), &hooks);
        let fake = directory.path().join("crosslink-test.cmd");
        std::fs::write(
            &fake,
            "@echo off\r\necho %CROSSLINK_TEST_RESPONSE%\r\nexit /b %CROSSLINK_TEST_EXIT%\r\n",
        )
        .unwrap();
        std::fs::write(
            crosslink.join("hook-config.json"),
            serde_json::to_vec(&json!({"crosslink_binary": fake})).unwrap(),
        )
        .unwrap();
        directory
    }

    fn execute(
        directory: &std::path::Path,
        command: &str,
        response: &str,
        exit_code: i32,
        payload: &Value,
    ) -> Output {
        let mut child = Command::new("cmd")
            .current_dir(directory)
            .args(["/D", "/S", "/C", command])
            .env("CROSSLINK_TEST_RESPONSE", response)
            .env("CROSSLINK_TEST_EXIT", exit_code.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        wait_output(child)
    }

    #[test]
    fn claude_and_codex_windows_wrappers_propagate_nonready_hook_exits() {
        let directory = fixture();
        let cases = [
            ("{".to_string(), 1),
            (
                envelope(Some("waiting_for_remote"), false, true).to_string(),
                20,
            ),
            (
                envelope(Some("blocked_corrupt"), false, true).to_string(),
                21,
            ),
        ];
        for provider in ["claude", "codex"] {
            for script in ["work-check.py", "session-start.py"] {
                let command = wrapper(provider, script);
                assert!(command.contains("exit $LASTEXITCODE"));
                for (response, exit_code) in &cases {
                    let payload = if script == "work-check.py" {
                        json!({"tool_name":"Bash","tool_input":{"command":"git commit -m test"}})
                    } else {
                        json!({"source":"startup"})
                    };
                    let output =
                        execute(directory.path(), &command, response, *exit_code, &payload);
                    assert_eq!(
                        output.status.code(),
                        Some(2),
                        "{provider} {script} response={response}: stdout={} stderr={}",
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr)
                    );
                }
            }
        }
    }
}
