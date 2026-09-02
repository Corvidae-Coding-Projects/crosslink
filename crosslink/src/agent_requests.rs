use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RequestKind {
    Kill,
    Pause,
    Resume,
    Reprioritise,
}

impl RequestKind {
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "kill" => Ok(Self::Kill),
            "pause" => Ok(Self::Pause),
            "resume" => Ok(Self::Resume),
            "reprioritise" | "reprioritize" => Ok(Self::Reprioritise),
            other => anyhow::bail!(
                "unknown request kind '{other}' (expected kill|pause|resume|reprioritise)"
            ),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RequestSubject {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub issue_id: Option<i64>,
}

impl RequestSubject {
    pub const fn is_empty(&self) -> bool {
        self.issue_id.is_none()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentRequest {
    pub request_id: String,
    pub kind: RequestKind,
    #[serde(default, skip_serializing_if = "RequestSubject::is_empty")]
    pub subject: RequestSubject,

    pub requested_by: String,
    pub requested_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentRequestAck {
    pub request_id: String,
    pub ack_at: String,

    pub acted: bool,

    pub result: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RequestWithAck {
    pub request: AgentRequest,
    pub ack: Option<AgentRequestAck>,
}

pub fn requests_dir(agent_id: &str) -> PathBuf {
    PathBuf::from("agents").join(agent_id).join("requests")
}

pub fn scan(cache_dir: &Path, agent_id: &str) -> Result<Vec<RequestWithAck>> {
    let dir = cache_dir.join(requests_dir(agent_id));
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut out = Vec::new();
    let entries = std::fs::read_dir(&dir).with_context(|| format!("read_dir {}", dir.display()))?;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };

        if name.to_ascii_lowercase().ends_with(".ack.json") {
            continue;
        }
        if !path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("json"))
        {
            continue;
        }
        let raw =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let request: AgentRequest =
            serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;

        let ack_file = dir.join(format!("{}.ack.json", request.request_id));
        let ack = if ack_file.exists() {
            let ack_raw = std::fs::read_to_string(&ack_file)
                .with_context(|| format!("read {}", ack_file.display()))?;
            Some(
                serde_json::from_str::<AgentRequestAck>(&ack_raw)
                    .with_context(|| format!("parse {}", ack_file.display()))?,
            )
        } else {
            None
        };

        out.push(RequestWithAck { request, ack });
    }

    out.sort_by(|a, b| a.request.request_id.cmp(&b.request.request_id));
    Ok(out)
}

pub fn new_request_id() -> String {
    ulid::Ulid::new().to_string()
}

pub mod poll {
    use super::*;
    use crate::agent_flags;
    use crate::application::CommandService;
    use crate::shared_writer::PushOutcome;

    #[derive(Debug, Clone, Default)]
    pub struct PollResult {
        pub acted: Vec<PollAction>,

        pub skipped_existing_ack: usize,
    }

    #[derive(Debug, Clone)]
    pub struct PollAction {
        pub request_id: String,
        pub kind: RequestKind,

        pub acted: bool,
        pub result: String,
        pub push_outcome: PushOutcome,
    }

    pub fn process_pending(
        commands: &(impl CommandService + ?Sized),
        crosslink_dir: &std::path::Path,
        agent_id: &str,
    ) -> Result<PollResult> {
        let sync = crate::sync::SyncManager::new(crosslink_dir)?;
        if sync.hub_mode().is_v3() {
            return process_pending_v3(commands, crosslink_dir, agent_id, sync.cache_path());
        }

        let entries = scan(sync.cache_path(), agent_id)?;
        let mut result = PollResult::default();

        for row in entries {
            if row.ack.is_some() {
                result.skipped_existing_ack += 1;
                continue;
            }
            let (acted, summary) = apply_request(crosslink_dir, &row.request)
                .unwrap_or_else(|e| (false, format!("error applying request: {e}")));

            let ack = AgentRequestAck {
                request_id: row.request.request_id.clone(),
                ack_at: chrono::Utc::now().to_rfc3339(),
                acted,
                result: summary.clone(),
                notes: None,
            };
            let push_outcome = commands.write_agent_ack(agent_id, &ack)?;

            result.acted.push(PollAction {
                request_id: row.request.request_id,
                kind: row.request.kind,
                acted,
                result: summary,
                push_outcome,
            });
        }

        Ok(result)
    }

    fn process_pending_v3(
        commands: &(impl CommandService + ?Sized),
        crosslink_dir: &std::path::Path,
        agent_id: &str,
        cache_dir: &std::path::Path,
    ) -> Result<PollResult> {
        let pending = crate::hub_v3::poll_requests_for_agent(cache_dir, agent_id)?;
        let mut result = PollResult::default();

        for (_driver_id, request) in pending {
            let (acted, summary) = apply_request(crosslink_dir, &request)
                .unwrap_or_else(|e| (false, format!("error applying request: {e}")));

            let ack = AgentRequestAck {
                request_id: request.request_id.clone(),
                ack_at: chrono::Utc::now().to_rfc3339(),
                acted,
                result: summary.clone(),
                notes: None,
            };
            let push_outcome = commands.write_agent_ack(agent_id, &ack)?;

            result.acted.push(PollAction {
                request_id: request.request_id,
                kind: request.kind,
                acted,
                result: summary,
                push_outcome,
            });
        }

        Ok(result)
    }

    fn apply_request(
        crosslink_dir: &std::path::Path,
        req: &AgentRequest,
    ) -> Result<(bool, String)> {
        match req.kind {
            RequestKind::Pause => {
                if agent_flags::is_paused(crosslink_dir) {
                    Ok((false, "already paused".into()))
                } else {
                    agent_flags::set_paused(crosslink_dir)?;
                    Ok((true, "paused".into()))
                }
            }
            RequestKind::Resume => {
                if agent_flags::is_paused(crosslink_dir) {
                    agent_flags::clear_paused(crosslink_dir)?;
                    Ok((true, "resumed".into()))
                } else {
                    Ok((false, "already running".into()))
                }
            }
            RequestKind::Kill => {
                if agent_flags::should_exit(crosslink_dir) {
                    Ok((false, "already flagged for exit".into()))
                } else {
                    agent_flags::set_kill(crosslink_dir)?;
                    Ok((true, "exit requested".into()))
                }
            }
            RequestKind::Reprioritise => {
                let Some(issue_id) = req.subject.issue_id else {
                    return Ok((
                        false,
                        "reprioritise request missing subject.issue_id".into(),
                    ));
                };
                agent_flags::set_reprioritise_hint(
                    crosslink_dir,
                    &agent_flags::ReprioritiseHint {
                        issue_id,
                        from_request_id: req.request_id.clone(),
                    },
                )?;
                Ok((true, format!("reprioritise hint → #{issue_id}")))
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::application::{Command, CommandResult};
        use tempfile::tempdir;

        struct RejectingCommands;

        impl CommandService for RejectingCommands {
            fn execute(&self, _command: Command) -> Result<CommandResult> {
                anyhow::bail!("acknowledgement rejected")
            }
        }

        fn make_req(kind: RequestKind, issue_id: Option<i64>) -> AgentRequest {
            AgentRequest {
                request_id: new_request_id(),
                kind,
                subject: RequestSubject { issue_id },
                requested_by: "SHA256:test".into(),
                requested_at: chrono::Utc::now().to_rfc3339(),
                reason: None,
            }
        }

        #[test]
        fn test_apply_pause_toggles_flag() {
            let dir = tempdir().unwrap();
            let (acted, summary) =
                apply_request(dir.path(), &make_req(RequestKind::Pause, None)).unwrap();
            assert!(acted);
            assert!(summary.contains("paused"));
            assert!(agent_flags::is_paused(dir.path()));

            let (acted2, summary2) =
                apply_request(dir.path(), &make_req(RequestKind::Pause, None)).unwrap();
            assert!(!acted2);
            assert!(summary2.contains("already"));
        }

        #[test]
        fn test_apply_resume_clears_flag() {
            let dir = tempdir().unwrap();
            agent_flags::set_paused(dir.path()).unwrap();
            let (acted, _) =
                apply_request(dir.path(), &make_req(RequestKind::Resume, None)).unwrap();
            assert!(acted);
            assert!(!agent_flags::is_paused(dir.path()));
        }

        #[test]
        fn test_apply_kill_sets_flag() {
            let dir = tempdir().unwrap();
            let (acted, _) = apply_request(dir.path(), &make_req(RequestKind::Kill, None)).unwrap();
            assert!(acted);
            assert!(agent_flags::should_exit(dir.path()));
        }

        #[test]
        fn test_apply_reprioritise_requires_issue_id() {
            let dir = tempdir().unwrap();
            let (acted, summary) =
                apply_request(dir.path(), &make_req(RequestKind::Reprioritise, None)).unwrap();
            assert!(!acted);
            assert!(summary.contains("missing"));

            let (acted_ok, _) =
                apply_request(dir.path(), &make_req(RequestKind::Reprioritise, Some(7))).unwrap();
            assert!(acted_ok);
            let hint = agent_flags::read_reprioritise_hint(dir.path())
                .unwrap()
                .unwrap();
            assert_eq!(hint.issue_id, 7);
        }

        #[test]
        fn process_pending_propagates_acknowledgement_failure() {
            let directory = tempdir().unwrap();
            let crosslink_dir = directory.path().join(".crosslink");
            let request_dir = crosslink_dir
                .join(".hub-cache")
                .join(requests_dir("agent-x"));
            std::fs::create_dir_all(&request_dir).unwrap();
            let request = make_req(RequestKind::Pause, None);
            std::fs::write(
                request_dir.join(format!("{}.json", request.request_id)),
                serde_json::to_vec(&request).unwrap(),
            )
            .unwrap();

            let error = process_pending(&RejectingCommands, &crosslink_dir, "agent-x")
                .expect_err("acknowledgement failure must not become local-only success");

            assert!(error.to_string().contains("acknowledgement rejected"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_requestkind_roundtrip() {
        for (s, k) in [
            ("kill", RequestKind::Kill),
            ("pause", RequestKind::Pause),
            ("resume", RequestKind::Resume),
            ("reprioritise", RequestKind::Reprioritise),
            ("reprioritize", RequestKind::Reprioritise),
        ] {
            assert_eq!(RequestKind::parse(s).unwrap(), k);
        }
        assert!(RequestKind::parse("bogus").is_err());
    }

    #[test]
    fn test_scan_missing_dir_returns_empty() {
        let dir = tempdir().unwrap();
        let out = scan(dir.path(), "agent-x").unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn test_scan_pairs_requests_with_acks() {
        let dir = tempdir().unwrap();
        let req_dir = dir.path().join(requests_dir("agent-x"));
        std::fs::create_dir_all(&req_dir).unwrap();

        let r1 = AgentRequest {
            request_id: "01HXY000000000000000000001".into(),
            kind: RequestKind::Pause,
            subject: RequestSubject { issue_id: Some(42) },
            requested_by: "SHA256:driver".into(),
            requested_at: "2026-04-20T18:30:00Z".into(),
            reason: Some("stuck".into()),
        };
        std::fs::write(
            req_dir.join(format!("{}.json", r1.request_id)),
            serde_json::to_string(&r1).unwrap(),
        )
        .unwrap();

        let r2 = AgentRequest {
            request_id: "01HXY000000000000000000000".into(),
            kind: RequestKind::Kill,
            subject: RequestSubject::default(),
            requested_by: "SHA256:driver".into(),
            requested_at: "2026-04-20T18:20:00Z".into(),
            reason: None,
        };
        std::fs::write(
            req_dir.join(format!("{}.json", r2.request_id)),
            serde_json::to_string(&r2).unwrap(),
        )
        .unwrap();
        let ack = AgentRequestAck {
            request_id: r2.request_id.clone(),
            ack_at: "2026-04-20T18:20:05Z".into(),
            acted: true,
            result: "killed".into(),
            notes: None,
        };
        std::fs::write(
            req_dir.join(format!("{}.ack.json", r2.request_id)),
            serde_json::to_string(&ack).unwrap(),
        )
        .unwrap();

        let out = scan(dir.path(), "agent-x").unwrap();
        assert_eq!(out.len(), 2);

        assert_eq!(out[0].request.request_id, r2.request_id);
        assert!(out[0].ack.as_ref().unwrap().acted);
        assert_eq!(out[1].request.request_id, r1.request_id);
        assert!(out[1].ack.is_none());
    }

    #[test]
    fn test_new_request_id_is_unique_and_sortable() {
        let a = new_request_id();
        let b = new_request_id();
        assert_ne!(a, b);

        assert_eq!(a.len(), 26);
        assert_eq!(b.len(), 26);
    }
}
