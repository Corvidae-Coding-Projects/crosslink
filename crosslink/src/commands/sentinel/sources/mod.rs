use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub mod ci;
pub mod cpitd;
pub mod github;
pub mod internal;
pub mod maintenance;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceKind {
    GitHub,
    Internal,
    CI,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SignalKind {
    LabelAdded,
    StaleIssue,
    CIFailure,

    CodeClone,
}

#[derive(Debug, Clone)]
pub struct Signal {
    pub source: SourceKind,
    pub kind: SignalKind,

    pub reference: String,
    pub title: String,
    pub body: String,
    pub metadata: serde_json::Value,
    pub detected_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignalDecision {
    New,

    Escalate,

    Skip(&'static str),
}

pub trait Source {
    fn name(&self) -> &'static str;

    fn poll(&mut self) -> Result<Vec<Signal>>;
}
