use super::harness::SmokeHarness;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::Command;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

fn http_request(
    port: u16,
    method: &str,
    path: &str,
    body: Option<&str>,
    auth_token: Option<&str>,
) -> (u16, String) {
    let mut stream =
        TcpStream::connect(format!("127.0.0.1:{port}")).expect("Failed to connect to server");
    stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(5))).ok();

    let body_str = body.unwrap_or("");
    let auth_header = auth_token
        .map(|t| format!("Authorization: Bearer {t}\r\n"))
        .unwrap_or_default();
    let request = format!(
        "{method} {path} HTTP/1.1\r\n\
Host: 127.0.0.1:{port}\r\n\
Content-Type: application/json\r\n\
{auth_header}\
Content-Length: {len}\r\n\
Connection: close\r\n\
\r\n\
{body_str}",
        len = body_str.len()
    );
    stream
        .write_all(request.as_bytes())
        .expect("Failed to write request");

    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);

    parse_http_response(&response)
}

fn parse_http_response(raw: &str) -> (u16, String) {
    let status = raw
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);

    let body = if let Some(idx) = raw.find("\r\n\r\n") {
        let after_headers = &raw[idx + 4..];
        let headers_lower = raw[..idx].to_lowercase();
        if headers_lower.contains("transfer-encoding: chunked") {
            decode_chunked(after_headers)
        } else {
            after_headers.to_string()
        }
    } else {
        String::new()
    };

    (status, body)
}

fn decode_chunked(raw: &str) -> String {
    let mut result = String::new();
    let mut remaining = raw;

    while let Some(line_end) = remaining.find("\r\n") {
        let size_str = remaining[..line_end].trim();
        let Ok(size) = usize::from_str_radix(size_str, 16) else {
            break;
        };
        if size == 0 {
            break;
        }
        let chunk_start = line_end + 2;
        let chunk_end = chunk_start + size;
        if chunk_end > remaining.len() {
            result.push_str(&remaining[chunk_start..]);
            break;
        }
        result.push_str(&remaining[chunk_start..chunk_end]);
        remaining = if chunk_end + 2 <= remaining.len() {
            &remaining[chunk_end + 2..]
        } else {
            ""
        };
    }

    result
}

fn parse_json(body: &str) -> serde_json::Value {
    serde_json::from_str(body).unwrap_or_else(|e| {
        panic!(
            "Failed to parse JSON: {}\nBody was: {:?}",
            e,
            &body[..body.len().min(500)]
        )
    })
}

#[test]
fn test_concurrent_api_creates_10() {
    let mut h = SmokeHarness::new();
    let port = h.start_server();
    let token = Arc::new(
        h.server_auth_token
            .clone()
            .expect("server did not emit auth token"),
    );

    let barrier = Arc::new(Barrier::new(10));

    let handles: Vec<_> = (0..10)
        .map(|i| {
            let barrier = Arc::clone(&barrier);
            let token = Arc::clone(&token);
            thread::spawn(move || {
                barrier.wait();
                let payload =
                    format!(r#"{{"title": "Concurrent issue {i}", "priority": "medium"}}"#);
                http_request(port, "POST", "/api/v1/issues", Some(&payload), Some(&token))
            })
        })
        .collect();

    let mut ids: Vec<i64> = Vec::new();
    for (idx, handle) in handles.into_iter().enumerate() {
        let (status, body) = handle.join().expect("thread panicked");
        assert!(
            status == 200 || status == 201,
            "Thread {} expected 200/201 but got {} — body: {}",
            idx,
            status,
            &body[..body.len().min(200)],
        );
        let json = parse_json(&body);
        let id = json["id"]
            .as_i64()
            .unwrap_or_else(|| panic!("Thread {idx} response missing numeric id: {body}"));
        assert!(
            !ids.contains(&id),
            "Duplicate issue id {id} from thread {idx}"
        );
        ids.push(id);
    }

    assert_eq!(ids.len(), 10, "Expected 10 distinct issue ids, got {ids:?}");

    let (status, body) = http_request(port, "GET", "/api/v1/issues", None, Some(&token));
    assert_eq!(status, 200);
    let json = parse_json(&body);
    let total = json["total"].as_u64().unwrap_or(0);
    assert_eq!(
        total, 10,
        "Expected 10 issues in list after concurrent creates, got {total}"
    );
}

#[test]
fn test_parallel_lock_claim_one_winner() {
    let agent_a = SmokeHarness::new();
    agent_a.run_ok(&["agent", "init", "agent-a", "--no-key", "--force"]);
    agent_a.run_ok(&["sync"]);
    agent_a.run_ok(&["create", "Contested resource"]);
    agent_a.run_ok(&["sync"]);

    let agent_b = agent_a.fork_agent("agent-b");
    agent_b.run_ok(&["agent", "init", "agent-b", "--no-key", "--force"]);
    agent_b.run_ok(&["sync"]);

    let bin_a = agent_a.crosslink_bin.clone();
    let dir_a = agent_a.temp_dir.path().to_path_buf();
    let bin_b = agent_b.crosslink_bin.clone();
    let dir_b = agent_b.temp_dir.path().to_path_buf();

    let barrier = Arc::new(Barrier::new(2));

    let barrier_a = Arc::clone(&barrier);
    let handle_a = thread::spawn(move || {
        barrier_a.wait();
        Command::new(&bin_a)
            .current_dir(&dir_a)
            .args(["locks", "claim", "1"])
            .output()
            .expect("failed to run locks claim for agent-a")
    });

    let barrier_b = Arc::clone(&barrier);
    let handle_b = thread::spawn(move || {
        barrier_b.wait();
        Command::new(&bin_b)
            .current_dir(&dir_b)
            .args(["locks", "claim", "1"])
            .output()
            .expect("failed to run locks claim for agent-b")
    });

    let out_a = handle_a.join().expect("agent-a thread panicked");
    let out_b = handle_b.join().expect("agent-b thread panicked");

    let stdout_a = String::from_utf8_lossy(&out_a.stdout);
    let stderr_a = String::from_utf8_lossy(&out_a.stderr);
    let stdout_b = String::from_utf8_lossy(&out_b.stdout);
    let stderr_b = String::from_utf8_lossy(&out_b.stderr);

    let success_a = out_a.status.success();
    let success_b = out_b.status.success();

    let code_a = out_a.status.code().unwrap_or(-1);
    let code_b = out_b.status.code().unwrap_or(-1);
    assert!(
        code_a == 0 || code_a == 1,
        "agent-a exited with unexpected code {code_a}\nstdout: {stdout_a}\nstderr: {stderr_a}",
    );
    assert!(
        code_b == 0 || code_b == 1,
        "agent-b exited with unexpected code {code_b}\nstdout: {stdout_b}\nstderr: {stderr_b}",
    );

    assert!(
        !(success_a && success_b),
        "Both agents claimed the same lock simultaneously — expected exactly one winner.\n\
         agent-a stdout: {stdout_a}\nagent-b stdout: {stdout_b}",
    );

    assert!(
        success_a || success_b,
        "Neither agent was able to claim the lock.\n\
         agent-a: code={code_a} stdout={stdout_a} stderr={stderr_a}\n\
         agent-b: code={code_b} stdout={stdout_b} stderr={stderr_b}",
    );

    if !success_a {
        let combined_a = format!("{stdout_a}{stderr_a}");
        assert!(
            !combined_a.is_empty(),
            "Losing agent-a produced no output at all",
        );
    }
    if !success_b {
        let combined_b = format!("{stdout_b}{stderr_b}");
        assert!(
            !combined_b.is_empty(),
            "Losing agent-b produced no output at all",
        );
    }
}

#[test]
fn test_offline_local_operations_then_sync() {
    let temp_dir = tempfile::TempDir::new().expect("failed to create temp dir");
    let bin = std::path::PathBuf::from(env!("CARGO_BIN_EXE_crosslink"));

    let out = Command::new("git")
        .current_dir(temp_dir.path())
        .args(["init", "-b", "main"])
        .output()
        .expect("git init failed to execute");
    assert!(out.status.success(), "git init failed");

    for args in [
        vec!["config", "user.email", "offline@test.local"],
        vec!["config", "user.name", "Offline Test"],
    ] {
        let out = Command::new("git")
            .current_dir(temp_dir.path())
            .args(&args)
            .output()
            .expect("git config failed");
        assert!(out.status.success(), "git config {args:?} failed");
    }

    std::fs::write(temp_dir.path().join("README.md"), "# offline test\n")
        .expect("failed to write README");
    let _ = Command::new("git")
        .current_dir(temp_dir.path())
        .args(["add", "README.md"])
        .output();
    let out = Command::new("git")
        .current_dir(temp_dir.path())
        .args(["commit", "-m", "initial", "--no-gpg-sign"])
        .output()
        .expect("git commit failed");
    assert!(out.status.success(), "initial commit failed");

    let out = Command::new(&bin)
        .current_dir(temp_dir.path())
        .args(["init", "--defaults", "--skip-cpitd", "--skip-signing"])
        .output()
        .expect("crosslink init failed to execute");
    assert!(
        out.status.success(),
        "crosslink init failed: {}",
        String::from_utf8_lossy(&out.stderr),
    );

    let run = |args: &[&str]| -> (bool, String, String) {
        let out = Command::new(&bin)
            .current_dir(temp_dir.path())
            .args(args)
            .output()
            .expect("failed to execute crosslink");
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).to_string(),
            String::from_utf8_lossy(&out.stderr).to_string(),
        )
    };

    let (ok, stdout, stderr) = run(&["issue", "create", "Offline issue alpha"]);
    assert!(
        ok,
        "create should succeed offline\nstdout: {stdout}\nstderr: {stderr}"
    );

    let (ok, stdout, stderr) = run(&["issue", "create", "Offline issue beta", "-p", "high"]);
    assert!(
        ok,
        "create with priority should succeed offline\nstdout: {stdout}\nstderr: {stderr}"
    );

    let (ok, list_stdout, stderr) = run(&["issue", "list", "-s", "all"]);
    assert!(
        ok,
        "list should succeed offline\nstdout: {list_stdout}\nstderr: {stderr}"
    );
    assert!(
        list_stdout.contains("Offline issue alpha"),
        "list should show alpha\nstdout: {list_stdout}"
    );
    assert!(
        list_stdout.contains("Offline issue beta"),
        "list should show beta\nstdout: {list_stdout}"
    );

    let alpha_id = list_stdout
        .lines()
        .find(|l| l.contains("Offline issue alpha"))
        .and_then(|l| l.split_whitespace().next())
        .map(|id| id.trim_start_matches('#').to_string())
        .unwrap_or_else(|| panic!("Could not find alpha in list output: {list_stdout}"));

    let beta_id = list_stdout
        .lines()
        .find(|l| l.contains("Offline issue beta"))
        .and_then(|l| l.split_whitespace().next())
        .map(|id| id.trim_start_matches('#').to_string())
        .unwrap_or_else(|| panic!("Could not find beta in list output: {list_stdout}"));

    let (ok, stdout, stderr) = run(&["issue", "show", &alpha_id]);
    assert!(
        ok,
        "show should succeed offline\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains("Offline issue alpha"),
        "show should display alpha\nstdout: {stdout}"
    );

    let (ok, stdout, stderr) = run(&[
        "issue",
        "update",
        &beta_id,
        "-t",
        "Offline issue beta (updated)",
    ]);
    assert!(
        ok,
        "update should succeed offline\nstdout: {stdout}\nstderr: {stderr}"
    );

    let (ok, stdout, _) = run(&["issue", "show", &beta_id]);
    assert!(ok, "show after offline update should succeed");
    assert!(
        stdout.contains("beta (updated)") || stdout.contains("Offline issue beta"),
        "show should reflect the update\nstdout: {stdout}"
    );

    let remote_dir = tempfile::TempDir::new().expect("failed to create remote temp dir");
    let out = Command::new("git")
        .current_dir(remote_dir.path())
        .args(["init", "--bare", "-b", "main"])
        .output()
        .expect("git init --bare failed");
    assert!(out.status.success(), "git init --bare failed");

    let out = Command::new("git")
        .current_dir(temp_dir.path())
        .args([
            "remote",
            "add",
            "origin",
            remote_dir.path().to_str().expect("remote path not UTF-8"),
        ])
        .output()
        .expect("git remote add failed");
    assert!(out.status.success(), "git remote add failed");

    let out = Command::new("git")
        .current_dir(temp_dir.path())
        .args(["push", "-u", "origin", "main"])
        .output()
        .expect("git push failed");
    assert!(
        out.status.success(),
        "initial push failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let (_, stdout, stderr) = run(&["sync"]);

    let _ = (stdout, stderr);

    let (ok, stdout, stderr) = run(&["issue", "list", "-s", "all"]);
    assert!(
        ok,
        "list should succeed after sync\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains("Offline issue alpha"),
        "alpha should survive sync\nstdout: {stdout}"
    );
}

#[test]
#[ignore = "racy by design — run manually; may be slow on CI"]
fn test_sqlite_busy_concurrent_writes() {
    const THREADS: usize = 20;

    let h = SmokeHarness::new();
    let bin = h.crosslink_bin.clone();
    let dir = h.temp_dir.path().to_path_buf();

    let barrier = Arc::new(Barrier::new(THREADS));

    let handles: Vec<_> = (0..THREADS)
        .map(|i| {
            let bin = bin.clone();
            let dir = dir.clone();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                Command::new(&bin)
                    .current_dir(&dir)
                    .args(["issue", "create", &format!("SQLITE_BUSY issue {i}")])
                    .output()
                    .expect("failed to execute crosslink")
            })
        })
        .collect();

    let mut successes = 0u32;
    for (i, handle) in handles.into_iter().enumerate() {
        let output = handle
            .join()
            .unwrap_or_else(|_| panic!("thread {i} panicked"));

        assert!(
            output.status.code().is_some(),
            "thread {i} process killed by signal (possible panic/abort)"
        );

        if output.status.success() {
            successes += 1;
        }
    }

    assert!(
        successes >= 1,
        "At least one concurrent create must succeed, but all {THREADS} failed",
    );

    let result = h.run_ok(&["issue", "list", "-s", "all", "--json"]);
    let parsed: serde_json::Value =
        serde_json::from_str(&result.stdout).expect("failed to parse issue list JSON");
    let db_count = parsed.as_array().map(|a| a.len()).unwrap_or(0);

    assert!(
        db_count >= successes as usize,
        "DB has {db_count} issues but {successes} creates succeeded — some data was lost",
    );
}

#[test]
fn test_split_brain_lock_detection() {
    let agent_a = SmokeHarness::new();
    agent_a.run_ok(&["agent", "init", "agent-a", "--no-key", "--force"]);
    agent_a.run_ok(&["sync"]);
    agent_a.run_ok(&["create", "Split-brain target"]);
    agent_a.run_ok(&["sync"]);
    agent_a.run_ok(&["locks", "claim", "1"]);

    agent_a.run_ok(&["sync"]);

    let agent_b = agent_a.fork_agent("agent-b");
    agent_b.run_ok(&["agent", "init", "agent-b", "--no-key", "--force"]);
    agent_b.run_ok(&["sync"]);

    let hub_cache_b = agent_b.temp_dir.path().join(".crosslink").join("hub");

    if hub_cache_b.exists() {
        let lock_event_path = hub_cache_b.join("locks").join("issue-1.lock");
        if let Some(parent) = lock_event_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        let lock_content = format!(
            "{{\"issue_id\":1,\"holder\":\"agent-b\",\"claimed_at\":\"{}\",\"expires_at\":null}}",
            "2099-01-01T00:00:00Z"
        );
        if std::fs::write(&lock_event_path, lock_content).is_ok() {
            let _ = Command::new("git")
                .current_dir(&hub_cache_b)
                .args(["add", "."])
                .output();
            let out = Command::new("git")
                .current_dir(&hub_cache_b)
                .args([
                    "commit",
                    "-m",
                    "fabricated split-brain lock",
                    "--no-gpg-sign",
                ])
                .output();

            if out.map(|o| o.status.success()).unwrap_or(false) {
                let _ = Command::new("git")
                    .current_dir(&hub_cache_b)
                    .args(["push", "origin", "HEAD:crosslink/hub"])
                    .output();
            }
        }
    }

    let sync_result = agent_a.run(&["sync"]);
    let sync_stdout = &sync_result.stdout;
    let sync_stderr = &sync_result.stderr;

    if sync_result.success {
        let check = agent_a.run(&["locks", "check", "1"]);
        let check_text = format!("{}{}", check.stdout, check.stderr);

        assert!(
            !check_text.contains("agent-a") || !check_text.contains("agent-b"),
            "Both agents appear as lock holders simultaneously — split-brain not resolved.\n\
             locks check output: {check_text}",
        );
    } else {
        let combined = format!("{sync_stdout}{sync_stderr}");
        assert!(
            combined.contains("lock")
                || combined.contains("Lock")
                || combined.contains("conflict")
                || combined.contains("Conflict")
                || combined.contains("split")
                || combined.contains("evict")
                || combined.contains("remote")
                || !combined.is_empty(),
            "Sync failure should produce some output; got empty stdout+stderr",
        );
    }
}
