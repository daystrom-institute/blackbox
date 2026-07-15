//! Session-bound logical agent and mailbox capability contract.

use async_trait::async_trait;
use bro_core::{AgentId, AttemptId};
use serde::{Deserialize, Serialize};

use crate::CapabilityResult;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case", tag = "kind", content = "turns")]
pub enum AgentForkTurns {
    None,
    #[default]
    All,
    Recent(u32),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSpawnRequest {
    pub task_name: String,
    pub message: String,
    #[serde(default)]
    pub fork_turns: AgentForkTurns,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentIdentity {
    pub agent_id: AgentId,
    pub canonical_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentTarget {
    pub canonical_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentMessageRequest {
    pub target: AgentTarget,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Initializing,
    Running,
    Idle,
    Interrupted,
    Completed,
    Errored,
    Evicted,
    NotFound,
}

impl AgentStatus {
    pub fn is_addressable(self) -> bool {
        matches!(
            self,
            Self::Initializing
                | Self::Running
                | Self::Idle
                | Self::Interrupted
                | Self::Completed
                | Self::Errored
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSummary {
    pub identity: AgentIdentity,
    pub status: AgentStatus,
    pub last_attempt_id: Option<AttemptId>,
    pub unavailable_cause: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentWaitRequest {
    pub timeout_ms: Option<u64>,
    pub path_prefix: Option<String>,
    pub after_mailbox_sequence: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "reason")]
pub enum AgentWake {
    MailboxChanged { through_sequence: u64 },
    DescendantStatus { agent: AgentSummary },
    UserSteer,
    Timeout,
}

#[async_trait]
pub trait AgentCapability: Send + Sync {
    async fn spawn(&self, request: AgentSpawnRequest) -> CapabilityResult<AgentIdentity>;

    async fn spawn_for_invocation(
        &self,
        invocation_id: &str,
        request: AgentSpawnRequest,
    ) -> CapabilityResult<AgentIdentity> {
        let _ = invocation_id;
        self.spawn(request).await
    }

    async fn send_message(&self, request: AgentMessageRequest) -> CapabilityResult<()>;

    async fn send_message_for_invocation(
        &self,
        invocation_id: &str,
        request: AgentMessageRequest,
    ) -> CapabilityResult<()> {
        let _ = invocation_id;
        self.send_message(request).await
    }

    async fn followup(&self, request: AgentMessageRequest) -> CapabilityResult<()>;

    async fn followup_for_invocation(
        &self,
        invocation_id: &str,
        request: AgentMessageRequest,
    ) -> CapabilityResult<()> {
        let _ = invocation_id;
        self.followup(request).await
    }

    async fn interrupt(&self, target: AgentTarget) -> CapabilityResult<AgentStatus>;

    async fn interrupt_for_invocation(
        &self,
        invocation_id: &str,
        target: AgentTarget,
    ) -> CapabilityResult<AgentStatus> {
        let _ = invocation_id;
        self.interrupt(target).await
    }

    async fn status(&self, target: AgentTarget) -> CapabilityResult<AgentSummary>;

    async fn list(&self, prefix: Option<String>) -> CapabilityResult<Vec<AgentSummary>>;

    async fn wait(&self, request: AgentWaitRequest) -> CapabilityResult<AgentWake>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fork_policy_is_explicit_and_round_trips() {
        for policy in [
            AgentForkTurns::None,
            AgentForkTurns::All,
            AgentForkTurns::Recent(3),
        ] {
            let value = serde_json::to_value(&policy).unwrap();
            assert_eq!(
                serde_json::from_value::<AgentForkTurns>(value).unwrap(),
                policy
            );
        }
    }

    #[test]
    fn list_summary_carries_no_mailbox_payload() {
        let summary = AgentSummary {
            identity: AgentIdentity {
                agent_id: AgentId::new("agent-1"),
                canonical_path: "/root/reviewer".into(),
            },
            status: AgentStatus::Idle,
            last_attempt_id: None,
            unavailable_cause: None,
        };
        let value = serde_json::to_value(summary).unwrap();
        assert!(value.get("message").is_none());
        assert!(value.get("mailbox").is_none());
    }
}
