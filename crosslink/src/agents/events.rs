use serde::{Deserialize, Serialize};
use std::path::Path;

use super::AgentProvider;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeEventKind {
    SessionStarted,
    TurnStarted,
    TurnCompleted,
    TurnFailed,
    Waiting,
    TimedOut,
    AssistantMessage,
    ToolCall,
    FileChange,
    McpCall,
    WebSearch,
    PlanUpdate,
    Error,
    Unknown,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeUsage {
    pub input_tokens: i64,
    pub cached_input_tokens: Option<i64>,
    pub output_tokens: i64,
    pub reasoning_output_tokens: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeEvent {
    pub provider: AgentProvider,
    pub kind: RuntimeEventKind,
    pub session_id: Option<String>,
    pub item_id: Option<String>,
    pub status: Option<String>,
    pub text: Option<String>,
    pub command: Option<String>,
    pub paths: Vec<String>,
    pub usage: Option<RuntimeUsage>,
    pub raw: serde_json::Value,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuntimeSnapshot {
    pub status: Option<String>,
    pub session_id: Option<String>,
    pub last_message: Option<String>,
}

#[must_use]
pub fn runtime_provider(worktree_dir: &Path) -> AgentProvider {
    std::fs::read_to_string(worktree_dir.join(".kickoff-metadata.json"))
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|metadata| {
            metadata
                .get("provider")
                .and_then(serde_json::Value::as_str)
                .map(ToOwned::to_owned)
        })
        .and_then(|provider| provider.parse().ok())
        .unwrap_or(AgentProvider::Claude)
}

#[must_use]
pub fn runtime_snapshot(worktree_dir: &Path) -> RuntimeSnapshot {
    let provider = runtime_provider(worktree_dir);
    let mut snapshot = RuntimeSnapshot::default();
    let path = worktree_dir.join(".crosslink/runtime/agent-events.jsonl");
    let Ok(raw) = std::fs::read_to_string(path) else {
        return snapshot;
    };
    for line in raw.lines() {
        let Ok(event) = parse_jsonl_event(provider, line) else {
            continue;
        };
        if event.session_id.is_some() {
            snapshot.session_id = event.session_id;
        }
        if event.text.is_some() {
            snapshot.last_message = event.text;
        }
        snapshot.status = match event.kind {
            RuntimeEventKind::SessionStarted | RuntimeEventKind::TurnStarted => {
                Some("running".to_string())
            }
            RuntimeEventKind::Waiting => Some("waiting".to_string()),
            RuntimeEventKind::TurnCompleted => Some("done".to_string()),
            RuntimeEventKind::TurnFailed | RuntimeEventKind::Error => Some("failed".to_string()),
            RuntimeEventKind::TimedOut => Some("timed-out".to_string()),
            _ => snapshot.status,
        };
    }
    snapshot
}

fn str_field(value: &serde_json::Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(ToString::to_string)
}

fn item_kind(item_type: Option<&str>) -> RuntimeEventKind {
    match item_type {
        Some("agent_message") => RuntimeEventKind::AssistantMessage,
        Some("command_execution") => RuntimeEventKind::ToolCall,
        Some("file_change") => RuntimeEventKind::FileChange,
        Some("mcp_tool_call") => RuntimeEventKind::McpCall,
        Some("web_search") => RuntimeEventKind::WebSearch,
        Some("plan_update") => RuntimeEventKind::PlanUpdate,
        _ => RuntimeEventKind::Unknown,
    }
}

fn claude_content(raw: &serde_json::Value) -> Option<&serde_json::Value> {
    raw.get("message")
        .and_then(|message| message.get("content"))
        .and_then(serde_json::Value::as_array)
        .and_then(|content| content.last())
}

fn tool_event_kind(name: Option<&str>) -> RuntimeEventKind {
    match name.unwrap_or_default() {
        "Write" | "Edit" | "apply_patch" => RuntimeEventKind::FileChange,
        "WebSearch" | "WebFetch" | "web_search" => RuntimeEventKind::WebSearch,
        name if name.starts_with("mcp__") => RuntimeEventKind::McpCall,
        _ => RuntimeEventKind::ToolCall,
    }
}

pub fn parse_jsonl_event(provider: AgentProvider, line: &str) -> serde_json::Result<RuntimeEvent> {
    let raw: serde_json::Value = serde_json::from_str(line)?;
    let event_type = raw.get("type").and_then(serde_json::Value::as_str);
    let item = raw.get("item").unwrap_or(&serde_json::Value::Null);
    let claude_block = claude_content(&raw).unwrap_or(&serde_json::Value::Null);
    let item_type = item.get("type").and_then(serde_json::Value::as_str);
    let claude_block_type = claude_block.get("type").and_then(serde_json::Value::as_str);
    let kind = match event_type {
        Some("thread.started" | "system") => RuntimeEventKind::SessionStarted,
        Some("turn.started") => RuntimeEventKind::TurnStarted,
        Some("result")
            if raw.get("is_error").and_then(serde_json::Value::as_bool) == Some(true) =>
        {
            RuntimeEventKind::TurnFailed
        }
        Some("turn.completed" | "result") => RuntimeEventKind::TurnCompleted,
        Some("turn.failed") => RuntimeEventKind::TurnFailed,
        Some("turn.waiting") => RuntimeEventKind::Waiting,
        Some("turn.timed_out") => RuntimeEventKind::TimedOut,
        Some("error") => RuntimeEventKind::Error,
        Some("item.started" | "item.updated" | "item.completed") => item_kind(item_type),
        Some("assistant") if claude_block_type == Some("tool_use") => {
            tool_event_kind(claude_block.get("name").and_then(serde_json::Value::as_str))
        }
        Some("assistant") => RuntimeEventKind::AssistantMessage,
        _ => RuntimeEventKind::Unknown,
    };
    let usage_value = raw
        .get("usage")
        .or_else(|| raw.get("result").and_then(|v| v.get("usage")));
    let usage = usage_value.map(|value| RuntimeUsage {
        input_tokens: value
            .get("input_tokens")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0),
        cached_input_tokens: value
            .get("cached_input_tokens")
            .or_else(|| value.get("cache_read_input_tokens"))
            .and_then(serde_json::Value::as_i64),
        output_tokens: value
            .get("output_tokens")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0),
        reasoning_output_tokens: value
            .get("reasoning_output_tokens")
            .and_then(serde_json::Value::as_i64),
    });
    let mut paths: Vec<String> = item
        .get("changes")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|change| str_field(change, "path"))
        .collect();
    if let Some(path) = claude_block
        .get("input")
        .and_then(|input| input.get("file_path"))
        .and_then(serde_json::Value::as_str)
    {
        paths.push(path.to_string());
    }

    let claude_text = claude_block
        .get("text")
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned);
    let claude_command = claude_block
        .get("input")
        .and_then(|input| input.get("command"))
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned);

    Ok(RuntimeEvent {
        provider,
        kind,
        session_id: str_field(&raw, "thread_id").or_else(|| str_field(&raw, "session_id")),
        item_id: str_field(item, "id"),
        status: str_field(item, "status").or_else(|| str_field(&raw, "status")),
        text: str_field(item, "text")
            .or(claude_text)
            .or_else(|| str_field(&raw, "message"))
            .or_else(|| str_field(&raw, "result")),
        command: str_field(item, "command").or(claude_command),
        paths,
        usage,
        raw,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_codex_usage_and_items() {
        let event = parse_jsonl_event(
            AgentProvider::Codex,
            r#"{"type":"turn.completed","usage":{"input_tokens":24763,"cached_input_tokens":24448,"output_tokens":122,"reasoning_output_tokens":7}}"#,
        )
        .unwrap();
        assert_eq!(event.kind, RuntimeEventKind::TurnCompleted);
        assert_eq!(event.usage.unwrap().reasoning_output_tokens, Some(7));

        let item = parse_jsonl_event(
            AgentProvider::Codex,
            r#"{"type":"item.completed","item":{"id":"item_3","type":"web_search","status":"completed"}}"#,
        )
        .unwrap();
        assert_eq!(item.kind, RuntimeEventKind::WebSearch);
    }

    #[test]
    fn parses_claude_result_into_same_lifecycle() {
        let event = parse_jsonl_event(
            AgentProvider::Claude,
            r#"{"type":"result","session_id":"abc","result":"done","usage":{"input_tokens":10,"output_tokens":3}}"#,
        )
        .unwrap();
        assert_eq!(event.kind, RuntimeEventKind::TurnCompleted);
        assert_eq!(event.session_id.as_deref(), Some("abc"));
        assert_eq!(event.text.as_deref(), Some("done"));
    }

    #[test]
    fn recorded_provider_fixtures_cover_the_shared_lifecycle() {
        let fixtures = [
            (
                AgentProvider::Claude,
                include_str!("../../resources/agent/events/claude.jsonl"),
            ),
            (
                AgentProvider::Codex,
                include_str!("../../resources/agent/events/codex.jsonl"),
            ),
        ];
        for (provider, fixture) in fixtures {
            let kinds: Vec<_> = fixture
                .lines()
                .map(|line| parse_jsonl_event(provider, line).unwrap().kind)
                .collect();
            for expected in [
                RuntimeEventKind::SessionStarted,
                RuntimeEventKind::AssistantMessage,
                RuntimeEventKind::ToolCall,
                RuntimeEventKind::FileChange,
                RuntimeEventKind::WebSearch,
                RuntimeEventKind::McpCall,
                RuntimeEventKind::TurnCompleted,
                RuntimeEventKind::TurnFailed,
                RuntimeEventKind::Waiting,
                RuntimeEventKind::TimedOut,
            ] {
                assert!(
                    kinds.contains(&expected),
                    "{provider}: missing {expected:?}"
                );
            }
        }
    }
}
