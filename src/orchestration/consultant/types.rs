use std::fmt;
use std::str::FromStr;

use serde::de::{Error as DeError, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use uuid::Uuid;

pub fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ConsultantId(String);

impl ConsultantId {
    pub fn new() -> Self {
        let raw = Uuid::new_v4().simple().to_string();
        Self(format!("bg-{}-{}", &raw[..8], &raw[8..16]))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for ConsultantId {
    fn default() -> Self {
        Self::new()
    }
}

// Error/display text below intentionally keeps the legacy "badgey" wording
// until the consumer-descriptor phase parameterizes id prefix and wording
// per consumer (see design/orchestration/agents/consultant-runtime.md §5).
impl fmt::Display for ConsultantId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for ConsultantId {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let parts: Vec<_> = value.split('-').collect();
        if parts.len() != 3
            || parts[0] != "bg"
            || parts[1].len() != 8
            || parts[2].len() != 8
            || !parts[1].chars().all(|c| c.is_ascii_hexdigit())
            || !parts[2].chars().all(|c| c.is_ascii_hexdigit())
        {
            return Err(format!(
                "invalid badgey id '{value}', expected bg-<8hex>-<8hex>"
            ));
        }
        Ok(Self(value.to_ascii_lowercase()))
    }
}

impl Serialize for ConsultantId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ConsultantId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Self::from_str(&s).map_err(D::Error::custom)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConsultantScope {
    pub project_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_brief: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProposalKind {
    Workflow,
    Packet,
    Brofile,
    Lens,
    Agent,
    RedispatchTask,
    ArtifactPromotion,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProposalState {
    Pending,
    Applying,
    Applied,
    Failed,
}

impl ProposalState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Applied | Self::Failed)
    }

    pub fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Pending, Self::Applying)
                | (Self::Pending, Self::Failed)
                | (Self::Applying, Self::Applied)
                | (Self::Applying, Self::Failed)
                | (Self::Failed, Self::Applying)
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProposalEvent {
    pub at: String,
    pub from: ProposalState,
    pub to: ProposalState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConsultantProposal {
    pub id: String,
    pub instance_id: ConsultantId,
    pub kind: ProposalKind,
    pub state: ProposalState,
    pub draft: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_task_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<ProposalEvent>,
}

impl ConsultantProposal {
    pub fn new(
        id: String,
        instance_id: ConsultantId,
        kind: ProposalKind,
        draft: Value,
        idempotency_key: Option<String>,
    ) -> Self {
        let now = now_rfc3339();
        Self {
            id,
            instance_id,
            kind,
            state: ProposalState::Pending,
            draft,
            idempotency_key,
            applied_task_id: None,
            created_at: now.clone(),
            updated_at: now,
            events: Vec::new(),
        }
    }

    pub fn is_terminal(&self) -> bool {
        self.state.is_terminal()
    }

    pub fn can_transition_to(&self, next: ProposalState) -> bool {
        self.state.can_transition_to(next)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ActionId(Uuid);

impl ActionId {
    pub fn new_v4() -> Self {
        Self(Uuid::new_v4())
    }

    pub fn as_uuid(&self) -> Uuid {
        self.0
    }
}

impl fmt::Display for ActionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for ActionId {
    type Err = uuid::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Uuid::parse_str(value).map(Self)
    }
}

impl Serialize for ActionId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

struct ActionIdVisitor;

impl<'de> Visitor<'de> for ActionIdVisitor {
    type Value = ActionId;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a UUID string")
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: DeError,
    {
        ActionId::from_str(value).map_err(E::custom)
    }
}

impl<'de> Deserialize<'de> for ActionId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_str(ActionIdVisitor)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ActionJournalState {
    Seen,
    Dispatching { task_id: String },
    Completed { result_ref: String },
    Failed { reason: String },
}

impl ActionJournalState {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed { .. } | Self::Failed { .. })
    }

    pub fn can_transition_to(&self, next: &Self) -> bool {
        matches!(
            (self, next),
            (Self::Seen, Self::Dispatching { .. })
                | (Self::Seen, Self::Completed { .. })
                | (Self::Seen, Self::Failed { .. })
                | (Self::Dispatching { .. }, Self::Completed { .. })
                | (Self::Dispatching { .. }, Self::Failed { .. })
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActionJournalEvent {
    pub at: String,
    pub from: ActionJournalState,
    pub to: ActionJournalState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ActionJournalEntry {
    pub action_id: ActionId,
    pub action_kind: String,
    pub body: Value,
    pub state: ActionJournalState,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<ActionJournalEvent>,
}

impl ActionJournalEntry {
    pub fn new(action_id: ActionId, action_kind: String, body: Value) -> Self {
        let now = now_rfc3339();
        Self {
            action_id,
            action_kind,
            body,
            state: ActionJournalState::Seen,
            created_at: now.clone(),
            updated_at: now,
            events: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consultant_id_round_trips() {
        let id = ConsultantId::from_str("bg-3f7a91c4-91ff04cc").unwrap();
        assert_eq!(id.to_string(), "bg-3f7a91c4-91ff04cc");
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(serde_json::from_str::<ConsultantId>(&json).unwrap(), id);
        assert!(ConsultantId::from_str("bg-nope").is_err());
    }

    #[test]
    fn proposal_state_rejects_invalid_shortcuts() {
        assert!(ProposalState::Pending.can_transition_to(ProposalState::Applying));
        assert!(!ProposalState::Pending.can_transition_to(ProposalState::Applied));
        assert!(!ProposalState::Applied.can_transition_to(ProposalState::Applying));
    }

    #[test]
    fn proposal_and_action_journal_serde_round_trip() {
        let proposal = ConsultantProposal::new(
            "P-1".to_string(),
            ConsultantId::from_str("bg-3f7a91c4-91ff04cc").unwrap(),
            ProposalKind::Agent,
            serde_json::json!({"name": "badgey"}),
            Some("idem-1".to_string()),
        );
        let raw = serde_json::to_string(&proposal).unwrap();
        assert_eq!(
            serde_json::from_str::<ConsultantProposal>(&raw).unwrap(),
            proposal
        );

        let state = ActionJournalState::Dispatching {
            task_id: "task-123".to_string(),
        };
        let raw = serde_json::to_string(&state).unwrap();
        assert_eq!(
            serde_json::from_str::<ActionJournalState>(&raw).unwrap(),
            state
        );
    }
}
