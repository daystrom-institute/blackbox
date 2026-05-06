use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::orchestration::providers::Provider;

use super::types::{BadgeyScope, ProposalKind, ProposalState};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CallerRef {
    pub provider: Provider,
    pub session_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PathEdge {
    pub from: String,
    pub edge: String,
    pub to: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ThreadEvent {
    Exec {
        brofile_version: String,
        scope: BadgeyScope,
        charter: String,
        provider: Provider,
        provider_session_id: String,
    },
    Turn {
        turn_id: u64,
        mode: String,
        caller: CallerRef,
        question: String,
        bundle_summary: String,
        #[serde(default)]
        refs_consumed: Vec<String>,
        #[serde(default)]
        proposals_emitted: Vec<String>,
    },
    PathCached {
        id: String,
        #[serde(default)]
        nodes: Vec<Value>,
        #[serde(default)]
        edges: Vec<PathEdge>,
        summary: String,
    },
    ScoutDispatched {
        scout_id: String,
        scout_thread_id: String,
        charters: Vec<String>,
    },
    SubbroSpawned {
        task_id: String,
        scout_id: String,
        charter: String,
    },
    ProposalEmitted {
        proposal_id: String,
        kind: ProposalKind,
        draft_ref: String,
        state: ProposalState,
    },
    ProposalApplied {
        proposal_id: String,
        artifact_ref: String,
        decide_id: String,
    },
    ProposalRejected {
        proposal_id: String,
        reason: String,
    },
    DisputeEscalated {
        subbro_results: Vec<Value>,
    },
    Dismiss {
        reason: String,
        summary: String,
    },
}

impl ThreadEvent {
    pub fn note_kind(&self) -> &'static str {
        match self {
            Self::ScoutDispatched { .. } | Self::ProposalEmitted { .. } => "followup",
            Self::ProposalApplied { .. } | Self::Dismiss { .. } => "done",
            Self::DisputeEscalated { .. } => "dispute",
            Self::Exec { .. }
            | Self::Turn { .. }
            | Self::PathCached { .. }
            | Self::SubbroSpawned { .. }
            | Self::ProposalRejected { .. } => "learned",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(event: ThreadEvent) {
        let raw = serde_json::to_string(&event).unwrap();
        let parsed: ThreadEvent = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed, event);
    }

    #[test]
    fn thread_events_round_trip_and_report_note_kind() {
        let scope = BadgeyScope {
            project_id: "project-1".to_string(),
            initial_brief: Some("brief".to_string()),
        };
        let cases = vec![
            ThreadEvent::Exec {
                brofile_version: "1".to_string(),
                scope,
                charter: "help".to_string(),
                provider: Provider::Codex,
                provider_session_id: "session-1".to_string(),
            },
            ThreadEvent::Turn {
                turn_id: 1,
                mode: "answer".to_string(),
                caller: CallerRef {
                    provider: Provider::Claude,
                    session_id: "caller-session".to_string(),
                },
                question: "why".to_string(),
                bundle_summary: "summary".to_string(),
                refs_consumed: vec!["knowledge:abc12345".to_string()],
                proposals_emitted: vec!["P-1".to_string()],
            },
            ThreadEvent::PathCached {
                id: "bg-path-1".to_string(),
                nodes: vec![serde_json::json!({"ref": "knowledge:abc12345"})],
                edges: vec![PathEdge {
                    from: "a".to_string(),
                    edge: "DERIVED_FROM".to_string(),
                    to: "b".to_string(),
                }],
                summary: "path".to_string(),
            },
            ThreadEvent::ScoutDispatched {
                scout_id: "scout-1".to_string(),
                scout_thread_id: "thread-1".to_string(),
                charters: vec!["look".to_string()],
            },
            ThreadEvent::SubbroSpawned {
                task_id: "task-1".to_string(),
                scout_id: "scout-1".to_string(),
                charter: "look".to_string(),
            },
            ThreadEvent::ProposalEmitted {
                proposal_id: "P-1".to_string(),
                kind: ProposalKind::Agent,
                draft_ref: "file.json".to_string(),
                state: ProposalState::Pending,
            },
            ThreadEvent::ProposalApplied {
                proposal_id: "P-1".to_string(),
                artifact_ref: "agent:badgey".to_string(),
                decide_id: "9a645dfd".to_string(),
            },
            ThreadEvent::ProposalRejected {
                proposal_id: "P-1".to_string(),
                reason: "not now".to_string(),
            },
            ThreadEvent::DisputeEscalated {
                subbro_results: vec![serde_json::json!({"a": 1})],
            },
            ThreadEvent::Dismiss {
                reason: "done".to_string(),
                summary: "closed".to_string(),
            },
        ];

        for event in cases {
            let kind = event.note_kind();
            assert!(["learned", "followup", "done", "dispute"].contains(&kind));
            round_trip(event);
        }
    }
}
