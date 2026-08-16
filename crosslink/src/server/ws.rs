use std::collections::HashSet;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::IntoResponse;
use serde::Serialize;
use tokio::sync::broadcast;

use crate::server::state::AppState;
use crate::server::types::{
    WsAgentStatusEvent, WsDashboardAlertsEvent, WsDashboardProjectEvent, WsExecutionProgressEvent,
    WsHeartbeatEvent, WsIssueUpdatedEvent, WsLockChangedEvent, WsSubscribeMessage,
};

pub const BROADCAST_CAPACITY: usize = 256;

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum WsEvent {
    Heartbeat(WsHeartbeatEvent),
    AgentStatus(WsAgentStatusEvent),
    IssueUpdated(WsIssueUpdatedEvent),
    LockChanged(WsLockChangedEvent),
    ExecutionProgress(WsExecutionProgressEvent),

    DashboardProjectUpdated(WsDashboardProjectEvent),

    DashboardAlertsChanged(WsDashboardAlertsEvent),
}

impl WsEvent {
    #[must_use]
    pub const fn channel(&self) -> &'static str {
        match self {
            Self::Heartbeat(_) | Self::AgentStatus(_) => "agents",
            Self::IssueUpdated(_) => "issues",
            Self::LockChanged(_) => "locks",
            Self::ExecutionProgress(_) => "execution",
            Self::DashboardProjectUpdated(_) | Self::DashboardAlertsChanged(_) => "dashboard",
        }
    }

    #[cfg(test)]
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    pub fn to_json_value(&self) -> Result<serde_json::Value, serde_json::Error> {
        serde_json::to_value(self)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct WsEnvelope {
    pub seq: u64,

    #[serde(flatten)]
    pub data: serde_json::Value,
}

#[must_use]
pub fn channel() -> (broadcast::Sender<WsEvent>, broadcast::Receiver<WsEvent>) {
    broadcast::channel(BROADCAST_CAPACITY)
}

pub async fn ws_handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state.ws_tx))
}

async fn handle_socket(mut socket: WebSocket, tx: broadcast::Sender<WsEvent>) {
    let mut rx = tx.subscribe();

    let mut seq: u64 = 0;

    let mut subscribed: Option<HashSet<String>> = None;

    loop {
        tokio::select! {

            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {

                        if let Ok(sub) = serde_json::from_str::<WsSubscribeMessage>(&text) {
                            if sub.message_type == "subscribe" {
                                subscribed = Some(sub.channels.into_iter().collect());
                            }
                        }
                    }

                    Some(Ok(Message::Close(_))) | None => break,

                    _ => {}
                }
            }


            event = rx.recv() => {
                match event {
                    Ok(ev) => {


                        if let Some(ref channels) = subscribed {
                            if !channels.contains(ev.channel()) {
                                continue;
                            }
                        }

                        if let Ok(data) = ev.to_json_value() {
                            seq += 1;
                            let envelope = WsEnvelope { seq, data };
                            if let Ok(json) = serde_json::to_string(&envelope) {
                                if socket.send(Message::Text(json.into())).await.is_err() {

                                    break;
                                }
                            }
                        }
                    }



                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("ws: client lagged, {n} events dropped");
                        seq += 1;
                        let gap = serde_json::json!({
                            "seq": seq,
                            "type": "gap",
                            "dropped": n,
                        });
                        if let Ok(json) = serde_json::to_string(&gap) {
                            if socket.send(Message::Text(json.into())).await.is_err() {
                                break;
                            }
                        }
                    }

                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::types::{AgentStatus, WsAgentStatusEvent, WsHeartbeatEvent};
    use chrono::Utc;

    #[test]
    fn test_ws_event_channel_heartbeat() {
        let ev = WsEvent::Heartbeat(WsHeartbeatEvent {
            event_type: crate::server::types::WsEventType::Heartbeat,
            agent_id: "a1".to_string(),
            timestamp: Utc::now(),
            active_issue_id: None,
        });
        assert_eq!(ev.channel(), "agents");
    }

    #[test]
    fn test_ws_event_channel_agent_status() {
        let ev = WsEvent::AgentStatus(WsAgentStatusEvent {
            event_type: crate::server::types::WsEventType::AgentStatus,
            agent_id: "a1".to_string(),
            status: AgentStatus::Active,
        });
        assert_eq!(ev.channel(), "agents");
    }

    #[test]
    fn test_ws_event_to_json_heartbeat() {
        let ev = WsEvent::Heartbeat(WsHeartbeatEvent {
            event_type: crate::server::types::WsEventType::Heartbeat,
            agent_id: "worker-1".to_string(),
            timestamp: Utc::now(),
            active_issue_id: Some(42),
        });
        let json = ev.to_json().unwrap();
        assert!(json.contains("\"type\":\"heartbeat\""));
        assert!(json.contains("\"agent_id\":\"worker-1\""));
        assert!(json.contains("\"active_issue_id\":42"));
    }

    #[test]
    fn test_ws_event_to_json_agent_status() {
        let ev = WsEvent::AgentStatus(WsAgentStatusEvent {
            event_type: crate::server::types::WsEventType::AgentStatus,
            agent_id: "worker-2".to_string(),
            status: AgentStatus::Idle,
        });
        let json = ev.to_json().unwrap();
        assert!(json.contains("\"type\":\"agent_status\""));
        assert!(json.contains("\"status\":\"idle\""));
    }

    #[test]
    fn test_ws_event_to_json_value_heartbeat() {
        let ev = WsEvent::Heartbeat(WsHeartbeatEvent {
            event_type: crate::server::types::WsEventType::Heartbeat,
            agent_id: "worker-1".to_string(),
            timestamp: Utc::now(),
            active_issue_id: Some(42),
        });
        let val = ev.to_json_value().unwrap();
        assert_eq!(val["type"], "heartbeat");
        assert_eq!(val["agent_id"], "worker-1");
        assert_eq!(val["active_issue_id"], 42);
    }

    #[test]
    fn test_ws_envelope_contains_seq_and_event_fields() {
        let ev = WsEvent::AgentStatus(WsAgentStatusEvent {
            event_type: crate::server::types::WsEventType::AgentStatus,
            agent_id: "worker-1".to_string(),
            status: AgentStatus::Active,
        });
        let data = ev.to_json_value().unwrap();
        let envelope = WsEnvelope { seq: 7, data };
        let json = serde_json::to_string(&envelope).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed["seq"], 7);

        assert_eq!(parsed["type"], "agent_status");
        assert_eq!(parsed["agent_id"], "worker-1");
        assert_eq!(parsed["status"], "active");
    }

    #[test]
    fn test_ws_envelope_seq_increments() {
        let ev = WsEvent::Heartbeat(WsHeartbeatEvent {
            event_type: crate::server::types::WsEventType::Heartbeat,
            agent_id: "a1".to_string(),
            timestamp: Utc::now(),
            active_issue_id: None,
        });

        let data1 = ev.to_json_value().unwrap();
        let env1 = WsEnvelope {
            seq: 1,
            data: data1,
        };
        let data2 = ev.to_json_value().unwrap();
        let env2 = WsEnvelope {
            seq: 2,
            data: data2,
        };

        let j1: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&env1).unwrap()).unwrap();
        let j2: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&env2).unwrap()).unwrap();

        assert_eq!(j1["seq"], 1);
        assert_eq!(j2["seq"], 2);
    }

    #[test]
    fn test_broadcast_channel_capacity() {
        let (tx, rx) = channel();

        drop(rx);
        assert_eq!(tx.receiver_count(), 0);
        let _rx2 = tx.subscribe();
        assert_eq!(tx.receiver_count(), 1);
    }

    #[test]
    fn test_ws_event_channel_issue_updated() {
        let ev = WsEvent::IssueUpdated(crate::server::types::WsIssueUpdatedEvent {
            event_type: crate::server::types::WsEventType::IssueUpdated,
            issue_id: 1,
            field: "status".to_string(),
        });
        assert_eq!(ev.channel(), "issues");
    }

    #[test]
    fn test_ws_event_channel_lock_changed() {
        let ev = WsEvent::LockChanged(crate::server::types::WsLockChangedEvent {
            event_type: crate::server::types::WsEventType::LockChanged,
            issue_id: 1,
            action: crate::server::types::LockAction::Claimed,
            agent_id: "a1".to_string(),
        });
        assert_eq!(ev.channel(), "locks");
    }

    #[test]
    fn test_ws_event_channel_execution_progress() {
        let ev = WsEvent::ExecutionProgress(crate::server::types::WsExecutionProgressEvent {
            event_type: crate::server::types::WsEventType::ExecutionProgress,
            plan_id: "p1".to_string(),
            phase_id: "ph1".to_string(),
            stage_id: "s1".to_string(),
            status: crate::server::types::StageStatus::Running,
            agent_id: None,
        });
        assert_eq!(ev.channel(), "execution");
    }

    #[test]
    fn test_ws_event_to_json_issue_updated() {
        let ev = WsEvent::IssueUpdated(crate::server::types::WsIssueUpdatedEvent {
            event_type: crate::server::types::WsEventType::IssueUpdated,
            issue_id: 42,
            field: "title".to_string(),
        });
        let json = ev.to_json().unwrap();
        assert!(json.contains("\"issue_id\":42"));
        let val = ev.to_json_value().unwrap();
        assert_eq!(val["type"], "issue_updated");
    }

    #[test]
    fn test_ws_event_to_json_lock_changed() {
        let ev = WsEvent::LockChanged(crate::server::types::WsLockChangedEvent {
            event_type: crate::server::types::WsEventType::LockChanged,
            issue_id: 5,
            action: crate::server::types::LockAction::Released,
            agent_id: "bot".to_string(),
        });
        let json = ev.to_json().unwrap();
        assert!(json.contains("\"action\":\"released\""));
        let val = ev.to_json_value().unwrap();
        assert_eq!(val["type"], "lock_changed");
    }

    #[test]
    fn test_ws_event_to_json_execution_progress() {
        let ev = WsEvent::ExecutionProgress(crate::server::types::WsExecutionProgressEvent {
            event_type: crate::server::types::WsEventType::ExecutionProgress,
            plan_id: "p1".to_string(),
            phase_id: "ph1".to_string(),
            stage_id: "s1".to_string(),
            status: crate::server::types::StageStatus::Done,
            agent_id: Some("agent-x".to_string()),
        });
        let json = ev.to_json().unwrap();
        assert!(json.contains("\"status\":\"done\""));
        let val = ev.to_json_value().unwrap();
        assert_eq!(val["type"], "execution_progress");
        assert_eq!(val["agent_id"], "agent-x");
    }
}
