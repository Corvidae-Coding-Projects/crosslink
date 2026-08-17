use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub const ORCHESTRATOR_DIR: &str = "orchestrator";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmTask {
    pub title: String,
    pub description: String,

    #[serde(default)]
    pub complexity_hours: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmStage {
    pub title: String,
    pub description: String,
    #[serde(default)]
    pub tasks: Vec<LlmTask>,

    #[serde(default)]
    pub depends_on: Vec<String>,

    #[serde(default = "default_agent_count")]
    pub agent_count: usize,
    #[serde(default)]
    pub complexity_hours: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmPhase {
    pub title: String,
    #[serde(default)]
    pub description: String,
    pub stages: Vec<LlmStage>,

    #[serde(default)]
    pub gate_criteria: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmDecomposeResponse {
    pub phases: Vec<LlmPhase>,

    #[serde(default)]
    pub estimated_hours: f64,
}

const fn default_agent_count() -> usize {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrchestratorTask {
    pub id: String,
    pub title: String,
    pub description: String,

    pub complexity_hours: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrchestratorStage {
    pub id: String,
    pub title: String,
    pub description: String,
    pub tasks: Vec<OrchestratorTask>,

    pub depends_on: Vec<String>,

    pub agent_count: usize,
    pub complexity_hours: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrchestratorPhase {
    pub id: String,
    pub title: String,
    pub description: String,
    pub stages: Vec<OrchestratorStage>,

    pub gate_criteria: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrchestratorPlan {
    pub id: String,
    pub document_slug: String,
    pub phases: Vec<OrchestratorPhase>,
    pub created_at: DateTime<Utc>,
    pub total_stages: usize,
    pub estimated_hours: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredPlan {
    pub plan: OrchestratorPlan,

    pub source_document: String,

    pub stored_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn llm_response_deserializes_minimal() {
        let json = r#"{
            "phases": [
                {
                    "title": "Phase 1",
                    "stages": [
                        {
                            "title": "Stage A",
                            "description": "Do stuff"
                        }
                    ]
                }
            ]
        }"#;
        let resp: LlmDecomposeResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.phases.len(), 1);
        assert_eq!(resp.phases[0].stages.len(), 1);
        assert_eq!(resp.phases[0].stages[0].agent_count, 1);
        assert!(resp.phases[0].stages[0].depends_on.is_empty());
    }

    #[test]
    fn llm_response_deserializes_full() {
        let json = r#"{
            "phases": [
                {
                    "title": "Phase 1",
                    "description": "First phase",
                    "stages": [
                        {
                            "title": "Stage A",
                            "description": "First stage",
                            "tasks": [
                                {"title": "Task 1", "description": "Do thing", "complexity_hours": 1.5}
                            ],
                            "depends_on": [],
                            "agent_count": 2,
                            "complexity_hours": 3.0
                        },
                        {
                            "title": "Stage B",
                            "description": "Second stage",
                            "tasks": [],
                            "depends_on": ["Stage A"],
                            "agent_count": 1,
                            "complexity_hours": 2.0
                        }
                    ],
                    "gate_criteria": ["All tests pass"]
                }
            ],
            "estimated_hours": 5.0
        }"#;
        let resp: LlmDecomposeResponse = serde_json::from_str(json).unwrap();
        assert!((resp.estimated_hours - 5.0).abs() < f64::EPSILON);
        assert_eq!(resp.phases[0].stages[0].agent_count, 2);
        assert_eq!(resp.phases[0].stages[1].depends_on, vec!["Stage A"]);
        assert_eq!(
            resp.phases[0].gate_criteria,
            vec!["All tests pass".to_string()]
        );
    }

    #[test]
    fn stored_plan_round_trip() {
        let plan = StoredPlan {
            plan: OrchestratorPlan {
                id: "test-plan".to_string(),
                document_slug: "test-doc".to_string(),
                phases: vec![],
                created_at: Utc::now(),
                total_stages: 0,
                estimated_hours: 0.0,
            },
            source_document: "# Test Doc\n\nHello".to_string(),
            stored_at: Utc::now(),
        };
        let json = serde_json::to_string(&plan).unwrap();
        let parsed: StoredPlan = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.plan.id, "test-plan");
        assert_eq!(parsed.source_document, "# Test Doc\n\nHello");
    }
}
