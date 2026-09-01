#[allow(unused_imports)]
use super::harness::{assert_stdout_contains, SmokeHarness};
use std::io::{Read, Write};
use std::net::TcpStream;
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

fn start_authed(h: &mut SmokeHarness) -> (u16, String) {
    let port = h.start_server();
    let token = h
        .server_auth_token
        .clone()
        .expect("server did not emit auth token");
    (port, token)
}

fn authed_request(
    port: u16,
    token: &str,
    method: &str,
    path: &str,
    body: Option<&str>,
) -> (u16, String) {
    http_request(port, method, path, body, Some(token))
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
fn test_server_starts_and_stops() {
    let mut h = SmokeHarness::new();
    let port = h.start_server();

    let stream = TcpStream::connect(format!("127.0.0.1:{port}"));
    assert!(stream.is_ok(), "Server should be listening on port {port}");
    drop(stream);

    h.stop_server();

    std::thread::sleep(Duration::from_millis(200));
    let stream = TcpStream::connect_timeout(
        &format!("127.0.0.1:{port}").parse().unwrap(),
        Duration::from_millis(500),
    );
    assert!(
        stream.is_err(),
        "Server should not be listening after stop_server()"
    );
}

#[test]
fn test_health_endpoint() {
    let mut h = SmokeHarness::new();
    let port = h.start_server();

    let (status, body) = http_request(port, "GET", "/api/v1/health", None, None);
    assert_eq!(status, 200, "Health endpoint should return 200");

    let json = parse_json(&body);
    assert_eq!(json["status"], "ok", "Health status should be 'ok'");
    assert!(
        json["version"].is_string(),
        "Health response should include version string"
    );
}

#[test]
fn test_api_create_issue() {
    let mut h = SmokeHarness::new();
    let (port, token) = start_authed(&mut h);

    let payload = r#"{"title": "Test issue via API", "priority": "high"}"#;
    let (status, body) = authed_request(port, &token, "POST", "/api/v1/issues", Some(payload));
    assert!(
        status == 200 || status == 201,
        "Create issue should return 200 or 201, got {status}"
    );

    let json = parse_json(&body);
    assert_eq!(json["title"], "Test issue via API");
    assert_eq!(json["priority"], "high");
    assert!(
        json["id"].as_i64().is_some(),
        "Response should include numeric id"
    );
    assert_eq!(json["status"], "open");
}

#[test]
fn test_api_get_issue() {
    let mut h = SmokeHarness::new();

    h.run_ok(&["issue", "create", "CLI-created issue", "-p", "medium"]);

    let (port, token) = start_authed(&mut h);

    let (status, body) = authed_request(port, &token, "GET", "/api/v1/issues/1", None);
    assert_eq!(status, 200, "GET issue should return 200");

    let json = parse_json(&body);
    assert_eq!(json["id"], 1);
    assert_eq!(json["title"], "CLI-created issue");
    assert_eq!(json["priority"], "medium");

    assert!(json["labels"].is_array(), "Should have labels array");
    assert!(json["comments"].is_array(), "Should have comments array");
    assert!(json["blockers"].is_array(), "Should have blockers array");
}

#[test]
fn test_api_list_issues() {
    let mut h = SmokeHarness::new();

    h.run_ok(&["issue", "create", "Issue Alpha"]);
    h.run_ok(&["issue", "create", "Issue Beta"]);
    h.run_ok(&["issue", "create", "Issue Gamma"]);

    let (port, token) = start_authed(&mut h);

    let (status, body) = authed_request(port, &token, "GET", "/api/v1/issues", None);
    assert_eq!(status, 200);

    let json = parse_json(&body);
    let items = json["items"].as_array().expect("items should be an array");
    assert_eq!(items.len(), 3, "Should have 3 issues");
    assert_eq!(json["total"], 3);

    let titles: Vec<&str> = items.iter().map(|i| i["title"].as_str().unwrap()).collect();
    assert!(titles.contains(&"Issue Alpha"));
    assert!(titles.contains(&"Issue Beta"));
    assert!(titles.contains(&"Issue Gamma"));
}

#[test]
fn test_api_update_issue() {
    let mut h = SmokeHarness::new();
    h.run_ok(&["issue", "create", "Original title"]);

    let (port, token) = start_authed(&mut h);

    let payload = r#"{"title": "Updated title", "priority": "high"}"#;
    let (status, body) = authed_request(port, &token, "PATCH", "/api/v1/issues/1", Some(payload));
    assert_eq!(status, 200, "PATCH should return 200");

    let json = parse_json(&body);
    assert_eq!(json["title"], "Updated title");
    assert_eq!(json["priority"], "high");

    let (status2, body2) = authed_request(port, &token, "GET", "/api/v1/issues/1", None);
    assert_eq!(status2, 200);
    let json2 = parse_json(&body2);
    assert_eq!(json2["title"], "Updated title");
    assert_eq!(json2["priority"], "high");
}

#[test]
fn test_api_delete_issue() {
    let mut h = SmokeHarness::new();
    h.run_ok(&["issue", "create", "Doomed issue"]);

    let (port, token) = start_authed(&mut h);

    let (status, _) = authed_request(port, &token, "GET", "/api/v1/issues/1", None);
    assert_eq!(status, 200);

    let (status, body) = authed_request(port, &token, "DELETE", "/api/v1/issues/1", None);
    assert_eq!(status, 200, "DELETE should return 200");
    let json = parse_json(&body);
    assert_eq!(json["ok"], true);

    let (status, _) = authed_request(port, &token, "GET", "/api/v1/issues/1", None);
    assert_eq!(status, 404, "Deleted issue should return 404");
}

#[test]
fn test_api_close_reopen() {
    let mut h = SmokeHarness::new();
    h.run_ok(&["issue", "create", "Close-reopen test"]);

    let (port, token) = start_authed(&mut h);

    let (status, body) = authed_request(port, &token, "POST", "/api/v1/issues/1/close", None);
    assert_eq!(status, 200, "Close should return 200");
    let json = parse_json(&body);
    assert_eq!(json["status"], "closed");

    let (_, body) = authed_request(port, &token, "GET", "/api/v1/issues/1", None);
    let json = parse_json(&body);
    assert_eq!(json["status"], "closed");

    let (status, body) = authed_request(port, &token, "POST", "/api/v1/issues/1/reopen", None);
    assert_eq!(status, 200, "Reopen should return 200");
    let json = parse_json(&body);
    assert_eq!(json["status"], "open");

    let (_, body) = authed_request(port, &token, "GET", "/api/v1/issues/1", None);
    let json = parse_json(&body);
    assert_eq!(json["status"], "open");
}

#[test]
fn test_api_404_unknown() {
    let mut h = SmokeHarness::new();
    let (port, token) = start_authed(&mut h);

    let (status, _) = authed_request(port, &token, "GET", "/api/v1/nonexistent", None);
    assert_eq!(
        status, 404,
        "Unknown API path should return 404, got {status}"
    );
}

#[test]
fn test_api_issue_not_found() {
    let mut h = SmokeHarness::new();
    let (port, token) = start_authed(&mut h);

    let (status, body) = authed_request(port, &token, "GET", "/api/v1/issues/99999", None);
    assert_eq!(status, 404, "Non-existent issue should return 404");

    let json = parse_json(&body);
    assert_eq!(json["error"], "not found");
}

#[test]
fn test_api_invalid_json() {
    let mut h = SmokeHarness::new();
    let (port, token) = start_authed(&mut h);

    let (status, _) = authed_request(
        port,
        &token,
        "POST",
        "/api/v1/issues",
        Some("this is not valid json{{{"),
    );
    assert!(
        status == 400 || status == 422,
        "Invalid JSON should return 400 or 422, got {status}"
    );
}

#[test]
fn test_api_sessions() {
    let mut h = SmokeHarness::new();
    let (port, token) = start_authed(&mut h);

    let (status, _) = authed_request(port, &token, "GET", "/api/v1/sessions/current", None);
    assert_eq!(
        status, 404,
        "No session should exist initially, got {status}"
    );

    let (status, body) = authed_request(port, &token, "POST", "/api/v1/sessions/start", Some("{}"));
    assert_eq!(status, 200, "Start session should return 200");
    let json = parse_json(&body);
    assert!(json["id"].as_i64().is_some(), "Session should have an id");
    assert!(
        json["started_at"].is_string(),
        "Session should have started_at"
    );

    let (status, body) = authed_request(port, &token, "GET", "/api/v1/sessions/current", None);
    assert_eq!(status, 200, "Current session should now exist");
    let json = parse_json(&body);
    assert!(json["id"].as_i64().is_some());

    let (status, body) = authed_request(
        port,
        &token,
        "POST",
        "/api/v1/sessions/end",
        Some(r#"{"notes": "smoke test done"}"#),
    );
    assert_eq!(status, 200, "End session should return 200");
    let json = parse_json(&body);
    assert_eq!(json["ok"], true);

    let (status, _) = authed_request(port, &token, "GET", "/api/v1/sessions/current", None);
    assert_eq!(
        status, 404,
        "After ending session, current should be 404, got {status}"
    );
}

#[test]
fn test_api_milestones() {
    let mut h = SmokeHarness::new();
    let (port, token) = start_authed(&mut h);

    let (status, body) = authed_request(port, &token, "GET", "/api/v1/milestones", None);
    assert_eq!(status, 200);
    let json = parse_json(&body);
    assert_eq!(json["total"], 0);

    let payload = r#"{"name": "v1.0", "description": "First release"}"#;
    let (status, body) = authed_request(port, &token, "POST", "/api/v1/milestones", Some(payload));
    assert_eq!(status, 200, "Create milestone should return 200");
    let created = parse_json(&body);
    assert_eq!(created["name"], "v1.0");
    assert_eq!(created["status"], "open");
    let ms_id = created["id"].as_i64().expect("Milestone should have id");

    let (status, body) = authed_request(port, &token, "GET", "/api/v1/milestones", None);
    assert_eq!(status, 200);
    let json = parse_json(&body);
    assert_eq!(json["total"], 1);

    let (status, body) = authed_request(
        port,
        &token,
        "GET",
        &format!("/api/v1/milestones/{ms_id}"),
        None,
    );
    assert_eq!(status, 200);
    let json = parse_json(&body);
    assert_eq!(json["name"], "v1.0");
    assert_eq!(json["issue_count"], 0);
    assert_eq!(json["progress_percent"], 0.0);
}

#[test]
fn test_api_search() {
    let mut h = SmokeHarness::new();

    h.run_ok(&["issue", "create", "Authentication bug fix"]);
    h.run_ok(&["issue", "create", "Dashboard layout update"]);
    h.run_ok(&["issue", "create", "Authentication refactor"]);

    let (port, token) = start_authed(&mut h);

    let (status, body) =
        authed_request(port, &token, "GET", "/api/v1/search?q=authentication", None);
    assert_eq!(status, 200);
    let json = parse_json(&body);
    let total = json["total"].as_u64().unwrap_or(0);
    assert!(
        total >= 2,
        "Search for 'authentication' should find at least 2 results, got {total}"
    );

    let items = json["items"].as_array().expect("items should be an array");

    for item in items {
        if item["kind"] == "issue" {
            let title = item["title"].as_str().unwrap_or("");
            assert!(
                title.to_lowercase().contains("authentication"),
                "Issue result should match query: {title}"
            );
        }
    }
}

#[test]
fn test_api_config() {
    let mut h = SmokeHarness::new();
    let (port, token) = start_authed(&mut h);

    let (status, body) = authed_request(port, &token, "GET", "/api/v1/config", None);
    assert_eq!(status, 200, "GET config should return 200");

    let json = parse_json(&body);

    assert!(
        json["tracking_mode"].is_string(),
        "Config should have tracking_mode"
    );
    assert!(json["remote"].is_string(), "Config should have remote");
    assert!(
        json.get("intervention_tracking").is_some(),
        "Config should have intervention_tracking"
    );
    assert!(
        json.get("auto_steal_stale_locks").is_some(),
        "Config should have auto_steal_stale_locks"
    );
    assert!(
        json.get("stale_lock_timeout_minutes").is_some(),
        "Config should have stale_lock_timeout_minutes"
    );
    assert!(
        json.get("signing_enforcement").is_some(),
        "Config should have signing_enforcement"
    );
}

#[test]
fn test_api_sync_status() {
    let mut h = SmokeHarness::new();
    let (port, token) = start_authed(&mut h);

    let (status, body) = authed_request(port, &token, "GET", "/api/v1/sync/status", None);
    assert_eq!(status, 200, "GET sync/status should return 200");

    let json = parse_json(&body);

    assert!(
        json.get("hub_initialized").is_some(),
        "Should have hub_initialized field"
    );
    assert_eq!(
        json["hub_branch"], "crosslink/checkpoint",
        "hub_branch should identify the canonical v3 checkpoint"
    );
    assert!(json.get("remote").is_some(), "Should have remote field");
    assert!(
        json.get("active_lock_count").is_some(),
        "Should have active_lock_count"
    );
    assert!(
        json.get("stale_lock_count").is_some(),
        "Should have stale_lock_count"
    );
}

#[test]
fn test_ws_connects() {
    let mut h = SmokeHarness::new();
    let port = h.start_server();

    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}"))
        .expect("Failed to connect for WebSocket test");
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(5))).ok();

    let ws_request = format!(
        "GET /ws HTTP/1.1\r\n\
Host: 127.0.0.1:{port}\r\n\
Upgrade: websocket\r\n\
Connection: Upgrade\r\n\
Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
Sec-WebSocket-Version: 13\r\n\
\r\n"
    );
    stream
        .write_all(ws_request.as_bytes())
        .expect("Failed to send WebSocket upgrade request");

    let mut buf = [0u8; 1024];
    let n = stream
        .read(&mut buf)
        .expect("Failed to read WebSocket upgrade response");
    let response = String::from_utf8_lossy(&buf[..n]);

    assert!(
        response.contains("101"),
        "WebSocket upgrade should return 101 Switching Protocols, got: {}",
        response.lines().next().unwrap_or("(empty)")
    );
    assert!(
        response.to_lowercase().contains("upgrade: websocket"),
        "Response should contain 'Upgrade: websocket' header"
    );
}

#[test]
fn test_api_create_issue_with_description() {
    let mut h = SmokeHarness::new();
    let (port, token) = start_authed(&mut h);

    let payload =
        r#"{"title": "Described issue", "description": "This is the details", "priority": "low"}"#;
    let (status, body) = authed_request(port, &token, "POST", "/api/v1/issues", Some(payload));
    assert!(status == 200 || status == 201);

    let json = parse_json(&body);
    assert_eq!(json["title"], "Described issue");
    assert_eq!(json["description"], "This is the details");
    assert_eq!(json["priority"], "low");
}

#[test]
fn test_api_create_issue_default_priority() {
    let mut h = SmokeHarness::new();
    let (port, token) = start_authed(&mut h);

    let payload = r#"{"title": "Default priority issue"}"#;
    let (status, body) = authed_request(port, &token, "POST", "/api/v1/issues", Some(payload));
    assert!(status == 200 || status == 201);

    let json = parse_json(&body);
    assert_eq!(
        json["priority"], "medium",
        "Default priority should be 'medium'"
    );
}

#[test]
fn test_api_update_nonexistent_issue() {
    let mut h = SmokeHarness::new();
    let (port, token) = start_authed(&mut h);

    let payload = r#"{"title": "New title"}"#;
    let (status, _) = authed_request(port, &token, "PATCH", "/api/v1/issues/99999", Some(payload));
    assert_eq!(status, 404, "Updating non-existent issue should return 404");
}

#[test]
fn test_api_delete_nonexistent_issue() {
    let mut h = SmokeHarness::new();
    let (port, token) = start_authed(&mut h);

    let (status, _) = authed_request(port, &token, "DELETE", "/api/v1/issues/99999", None);
    assert_eq!(status, 404, "Deleting non-existent issue should return 404");
}

#[test]
fn test_api_close_nonexistent_issue() {
    let mut h = SmokeHarness::new();
    let (port, token) = start_authed(&mut h);

    let (status, _) = authed_request(port, &token, "POST", "/api/v1/issues/99999/close", None);
    assert_eq!(status, 404, "Closing non-existent issue should return 404");
}

#[test]
fn test_api_list_issues_empty() {
    let mut h = SmokeHarness::new();
    let (port, token) = start_authed(&mut h);

    let (status, body) = authed_request(port, &token, "GET", "/api/v1/issues", None);
    assert_eq!(status, 200);

    let json = parse_json(&body);
    assert_eq!(json["total"], 0);
    assert!(json["items"].as_array().unwrap().is_empty());
}

#[test]
fn test_api_issues_blocked_and_ready() {
    let mut h = SmokeHarness::new();

    h.run_ok(&["issue", "create", "Ready issue"]);
    h.run_ok(&["issue", "create", "Another ready issue"]);

    let (port, token) = start_authed(&mut h);

    let (status, body) = authed_request(port, &token, "GET", "/api/v1/issues/ready", None);
    assert_eq!(status, 200);
    let json = parse_json(&body);
    let total = json["total"].as_u64().unwrap_or(0);
    assert!(
        total >= 2,
        "Should have at least 2 ready issues, got {total}"
    );

    let (status, body) = authed_request(port, &token, "GET", "/api/v1/issues/blocked", None);
    assert_eq!(status, 200);
    let json = parse_json(&body);
    assert_eq!(json["total"], 0, "No issues should be blocked initially");
}

#[test]
fn test_api_milestone_not_found() {
    let mut h = SmokeHarness::new();
    let (port, token) = start_authed(&mut h);

    let (status, body) = authed_request(port, &token, "GET", "/api/v1/milestones/99999", None);
    assert_eq!(status, 404);

    let json = parse_json(&body);
    assert_eq!(json["error"], "not found");
}

#[test]
fn test_api_search_empty_query() {
    let mut h = SmokeHarness::new();
    let (port, token) = start_authed(&mut h);

    let (status, _) = authed_request(port, &token, "GET", "/api/v1/search?q=", None);
    assert_eq!(
        status, 400,
        "Empty search query should return 400, got {status}"
    );
}

#[test]
fn test_api_search_no_results() {
    let mut h = SmokeHarness::new();
    let (port, token) = start_authed(&mut h);

    let (status, body) =
        authed_request(port, &token, "GET", "/api/v1/search?q=xyznonexistent", None);
    assert_eq!(status, 200);
    let json = parse_json(&body);
    assert_eq!(json["total"], 0);
}
