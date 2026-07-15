use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use bro_capabilities::{
    AttemptOutcome, AttemptState, ExecutionAccepted, ExecutionCodeMode, ExecutionDispatchContext,
    ExecutionRequest, ExecutionServiceTier, ExecutionToolPolicy, RecordEnvelope, WorkingSetIntent,
};
use bro_core::{AgentId, AttemptId, OperationId, Provider, SessionId, TaskId};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{BlackopsError, BlackopsResult};

pub const CURRENT_SNAPSHOT_FORMAT: u32 = 1;

macro_rules! id_type {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

id_type!(TeamId);
id_type!(InvocationId);
id_type!(ScheduleId);
id_type!(WaitId);
id_type!(ApprovalId);
id_type!(WhiteboardId);
id_type!(IntegrationIntentId);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OperationalSnapshot {
    pub format_version: u32,
    pub generation: u64,
    pub next_record_cursor: u64,
    #[serde(default)]
    pub operations: BTreeMap<OperationId, OperationRecord>,
    #[serde(default)]
    pub request_dedup: BTreeMap<String, RequestDedupRecord>,
    #[serde(default)]
    pub fleet_outbox: BTreeMap<String, FleetEffect>,
    #[serde(default)]
    pub agents: BTreeMap<AgentId, AgentRecord>,
    #[serde(default)]
    pub agent_paths: BTreeMap<String, AgentId>,
    #[serde(default)]
    pub teams: BTreeMap<TeamId, TeamRecord>,
    #[serde(default)]
    pub mailboxes: BTreeMap<AgentId, Mailbox>,
    #[serde(default)]
    pub message_dedup: BTreeMap<String, MessageDedupRecord>,
    /// Mailbox items awaiting durable worker admission, keyed by stable effect
    /// identity. Delivery acknowledgement, not an HTTP response, advances the
    /// corresponding mailbox cursor.
    #[serde(default)]
    pub mailbox_delivery_outbox: BTreeMap<String, MailboxDeliveryEffect>,
    #[serde(default, with = "map_as_entries")]
    pub definitions: BTreeMap<DefinitionKey, OperationalDefinition>,
    #[serde(default, with = "map_as_entries")]
    pub active_definitions: BTreeMap<DefinitionName, String>,
    #[serde(default)]
    pub invocations: BTreeMap<InvocationId, InvocationIntent>,
    #[serde(default)]
    pub workflow_runs: BTreeMap<InvocationId, WorkflowRun>,
    #[serde(default)]
    pub schedules: BTreeMap<ScheduleId, ScheduleIntent>,
    #[serde(default)]
    pub poll_cursors: BTreeMap<ScheduleId, PollSourceCursor>,
    #[serde(default)]
    pub waits: BTreeMap<WaitId, OperationalWait>,
    #[serde(default)]
    pub approvals: BTreeMap<ApprovalId, ApprovalRecord>,
    #[serde(default)]
    pub whiteboards: BTreeMap<WhiteboardId, WhiteboardRecord>,
    #[serde(default)]
    pub integration_intents: BTreeMap<IntegrationIntentId, IntegrationIntent>,
    #[serde(default)]
    pub record_outbox: BTreeMap<String, RecordEnvelope>,
}

mod map_as_entries {
    use std::collections::BTreeMap;
    use std::fmt;
    use std::marker::PhantomData;

    use serde::de::{IgnoredAny, MapAccess, SeqAccess, Visitor};
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<K, V, S>(map: &BTreeMap<K, V>, serializer: S) -> Result<S::Ok, S::Error>
    where
        K: Serialize,
        V: Serialize,
        S: Serializer,
    {
        serializer.collect_seq(map.iter())
    }

    pub fn deserialize<'de, K, V, D>(deserializer: D) -> Result<BTreeMap<K, V>, D::Error>
    where
        K: Deserialize<'de> + Ord,
        V: Deserialize<'de>,
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(EntriesVisitor(PhantomData))
    }

    struct EntriesVisitor<K, V>(PhantomData<(K, V)>);

    impl<'de, K, V> Visitor<'de> for EntriesVisitor<K, V>
    where
        K: Deserialize<'de> + Ord,
        V: Deserialize<'de>,
    {
        type Value = BTreeMap<K, V>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a sequence of key-value entries or a legacy empty object")
        }

        fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            let mut map = BTreeMap::new();
            while let Some((key, value)) = sequence.next_element()? {
                if map.insert(key, value).is_some() {
                    return Err(serde::de::Error::custom("duplicate durable map key"));
                }
            }
            Ok(map)
        }

        fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
        where
            A: MapAccess<'de>,
        {
            if map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {
                return Err(serde::de::Error::custom(
                    "legacy object form is only valid when empty",
                ));
            }
            Ok(BTreeMap::new())
        }
    }
}

impl Default for OperationalSnapshot {
    fn default() -> Self {
        Self {
            format_version: CURRENT_SNAPSHOT_FORMAT,
            generation: 0,
            next_record_cursor: 1,
            operations: BTreeMap::new(),
            request_dedup: BTreeMap::new(),
            fleet_outbox: BTreeMap::new(),
            agents: BTreeMap::new(),
            agent_paths: BTreeMap::new(),
            teams: BTreeMap::new(),
            mailboxes: BTreeMap::new(),
            message_dedup: BTreeMap::new(),
            mailbox_delivery_outbox: BTreeMap::new(),
            definitions: BTreeMap::new(),
            active_definitions: BTreeMap::new(),
            invocations: BTreeMap::new(),
            workflow_runs: BTreeMap::new(),
            schedules: BTreeMap::new(),
            poll_cursors: BTreeMap::new(),
            waits: BTreeMap::new(),
            approvals: BTreeMap::new(),
            whiteboards: BTreeMap::new(),
            integration_intents: BTreeMap::new(),
            record_outbox: BTreeMap::new(),
        }
    }
}

impl OperationalSnapshot {
    pub fn validate(&self) -> BlackopsResult<()> {
        if self.format_version != CURRENT_SNAPSHOT_FORMAT {
            return Err(BlackopsError::UnsupportedSnapshotFormat {
                found: self.format_version,
                expected: CURRENT_SNAPSHOT_FORMAT,
            });
        }
        if self.next_record_cursor == 0 {
            return Err(BlackopsError::InvalidState(
                "record cursor must be one-based".into(),
            ));
        }
        for (operation_id, operation) in &self.operations {
            if operation_id != &operation.operation_id {
                return Err(BlackopsError::InvalidState(format!(
                    "operation map key {operation_id} differs from its record"
                )));
            }
            operation.validate()?;
        }
        for (key, dedup) in &self.request_dedup {
            if key != &dedup.idempotency_key || !self.operations.contains_key(&dedup.operation_id) {
                return Err(BlackopsError::InvalidState(format!(
                    "request dedup entry {key} is not backed by its operation"
                )));
            }
        }
        for (effect_id, effect) in &self.fleet_outbox {
            if effect_id != &effect.effect_id || !self.operations.contains_key(&effect.operation_id)
            {
                return Err(BlackopsError::InvalidState(format!(
                    "fleet effect {effect_id} is not backed by its operation"
                )));
            }
            if self.operations[&effect.operation_id].status.is_terminal() {
                return Err(BlackopsError::InvalidState(format!(
                    "terminal operation {} retains a fleet effect",
                    effect.operation_id
                )));
            }
        }
        if self.agent_paths.len() != self.agents.len() {
            return Err(BlackopsError::InvalidState(
                "agent path index does not match agent membership".into(),
            ));
        }
        for (agent_id, agent) in &self.agents {
            if agent_id != &agent.agent_id
                || self.agent_paths.get(&agent.path) != Some(agent_id)
                || !self.mailboxes.contains_key(agent_id)
            {
                return Err(BlackopsError::InvalidState(format!(
                    "agent {agent_id} identity, path, or mailbox is inconsistent"
                )));
            }
            if let Some(parent_id) = &agent.parent_id {
                let parent = self.agents.get(parent_id).ok_or_else(|| {
                    BlackopsError::InvalidState(format!(
                        "agent {agent_id} references missing parent {parent_id}"
                    ))
                })?;
                if !parent.children.contains(agent_id) {
                    return Err(BlackopsError::InvalidState(format!(
                        "agent {agent_id} parent edge is not reciprocal"
                    )));
                }
            }
            for child_id in &agent.children {
                if self
                    .agents
                    .get(child_id)
                    .and_then(|child| child.parent_id.as_ref())
                    != Some(agent_id)
                {
                    return Err(BlackopsError::InvalidState(format!(
                        "agent {agent_id} child edge to {child_id} is not reciprocal"
                    )));
                }
            }
            if let Some(operation_id) = &agent.current_operation_id
                && !self.operations.contains_key(operation_id)
            {
                return Err(BlackopsError::InvalidState(format!(
                    "agent {agent_id} references missing operation {operation_id}"
                )));
            }
            if agent
                .current_worker_id
                .as_deref()
                .is_some_and(str::is_empty)
                || agent.current_session_attempt_generation == Some(0)
                || (agent.current_session_attempt_generation.is_some()
                    && (agent.current_attempt_id.is_none()
                        || agent.current_task_id.is_none()
                        || agent.current_session_id.is_none()))
                || (agent.current_worker_id.is_some() && agent.current_session_id.is_none())
            {
                return Err(BlackopsError::InvalidState(format!(
                    "agent {agent_id} has an invalid worker/session attempt binding"
                )));
            }
        }
        for (agent_id, mailbox) in &self.mailboxes {
            if !self.agents.contains_key(agent_id) {
                return Err(BlackopsError::InvalidState(format!(
                    "mailbox exists for missing agent {agent_id}"
                )));
            }
            mailbox.validate(agent_id)?;
        }
        for (key, dedup) in &self.message_dedup {
            let mailbox = self.mailboxes.get(&dedup.recipient).ok_or_else(|| {
                BlackopsError::InvalidState(format!("message dedup {key} has no mailbox"))
            })?;
            if key != &dedup.idempotency_key
                || mailbox
                    .messages
                    .get(&dedup.sequence)
                    .map(|message| &message.message_id)
                    != Some(&dedup.message_id)
            {
                return Err(BlackopsError::InvalidState(format!(
                    "message dedup entry {key} is inconsistent"
                )));
            }
        }
        for (effect_id, effect) in &self.mailbox_delivery_outbox {
            if effect_id != &effect.effect_id
                || effect.delivery_id != effect.effect_id
                || effect.through_sequence != effect.message.sequence
                || effect.message.recipient != effect.recipient
                || self
                    .mailboxes
                    .get(&effect.recipient)
                    .and_then(|mailbox| mailbox.messages.get(&effect.message.sequence))
                    != Some(&effect.message)
                || self.agents.get(&effect.recipient).is_none_or(|agent| {
                    agent.path != effect.canonical_target
                        || agent.current_session_id.as_ref() != Some(&effect.session_id)
                })
            {
                return Err(BlackopsError::InvalidState(format!(
                    "mailbox delivery effect {effect_id} is inconsistent"
                )));
            }
        }
        for (team_id, team) in &self.teams {
            if team_id != &team.team_id {
                return Err(BlackopsError::InvalidState(format!(
                    "team map key {team_id} differs from its record"
                )));
            }
            for agent_id in team.members.keys() {
                if !self.agents.contains_key(agent_id) {
                    return Err(BlackopsError::InvalidState(format!(
                        "team {team_id} references missing agent {agent_id}"
                    )));
                }
            }
        }
        for (key, definition) in &self.definitions {
            if key != &definition.key {
                return Err(BlackopsError::InvalidState(format!(
                    "definition key {}:{} differs from its record",
                    key.name, key.version
                )));
            }
        }
        for (name, version) in &self.active_definitions {
            let key = DefinitionKey {
                kind: name.kind,
                name: name.name.clone(),
                version: version.clone(),
            };
            if !self.definitions.contains_key(&key) {
                return Err(BlackopsError::InvalidState(format!(
                    "active definition {}:{} points to missing version {version}",
                    name.kind, name.name
                )));
            }
        }
        for (invocation_id, invocation) in &self.invocations {
            if invocation_id != &invocation.invocation_id {
                return Err(BlackopsError::InvalidState(format!(
                    "invocation map key {invocation_id} differs from its record"
                )));
            }
            if !self.definitions.contains_key(&invocation.definition) {
                return Err(BlackopsError::InvalidState(format!(
                    "invocation {} references a missing definition",
                    invocation.invocation_id
                )));
            }
            if let Some(operation_id) = &invocation.operation_id
                && !matches!(
                    self.operations.get(operation_id).map(|operation| &operation.kind),
                    Some(OperationKind::InvokeDefinition {
                        invocation_id: linked
                    }) | Some(OperationKind::RunWorkflowNode {
                        invocation_id: linked,
                        ..
                    }) if linked == invocation_id
                )
            {
                return Err(BlackopsError::InvalidState(format!(
                    "invocation {invocation_id} references an inconsistent operation"
                )));
            }
        }
        for (invocation_id, run) in &self.workflow_runs {
            if invocation_id != &run.invocation_id
                || self
                    .invocations
                    .get(invocation_id)
                    .map(|invocation| &invocation.definition)
                    != Some(&run.definition)
                || run.definition.kind != DefinitionKind::Workflow
            {
                return Err(BlackopsError::InvalidState(format!(
                    "workflow run {invocation_id} has inconsistent identity"
                )));
            }
            if let Some(wait_id) = &run.waiting_on
                && !self.waits.contains_key(wait_id)
            {
                return Err(BlackopsError::InvalidState(format!(
                    "workflow run {invocation_id} references missing wait {wait_id}"
                )));
            }
            for (node_id, node) in &run.nodes {
                if node_id != &node.node_id {
                    return Err(BlackopsError::InvalidState(format!(
                        "workflow run {invocation_id} has inconsistent node state"
                    )));
                }
            }
        }
        for (schedule_id, schedule) in &self.schedules {
            if schedule_id != &schedule.schedule_id {
                return Err(BlackopsError::InvalidState(format!(
                    "schedule map key {schedule_id} differs from its record"
                )));
            }
            if !self
                .definitions
                .contains_key(&schedule.invocation.definition)
            {
                return Err(BlackopsError::InvalidState(format!(
                    "schedule {} references a missing definition",
                    schedule.schedule_id
                )));
            }
        }
        for (schedule_id, cursor) in &self.poll_cursors {
            if schedule_id != &cursor.schedule_id
                || !matches!(
                    self.schedules
                        .get(schedule_id)
                        .map(|schedule| &schedule.trigger),
                    Some(ScheduleTrigger::Poll { .. })
                )
            {
                return Err(BlackopsError::InvalidState(format!(
                    "poll cursor {schedule_id} has no matching poll schedule"
                )));
            }
        }
        for (wait_id, wait) in &self.waits {
            if wait_id != &wait.wait_id {
                return Err(BlackopsError::InvalidState(format!(
                    "wait map key {wait_id} differs from its record"
                )));
            }
        }
        for (approval_id, approval) in &self.approvals {
            if approval_id != &approval.approval_id {
                return Err(BlackopsError::InvalidState(format!(
                    "approval map key {approval_id} differs from its record"
                )));
            }
        }
        for (board_id, board) in &self.whiteboards {
            if board_id != &board.whiteboard_id {
                return Err(BlackopsError::InvalidState(format!(
                    "whiteboard map key {board_id} differs from its record"
                )));
            }
            for (key, entry) in &board.entries {
                if key != &entry.key || entry.revision == 0 {
                    return Err(BlackopsError::InvalidState(format!(
                        "whiteboard {board_id} contains an invalid entry"
                    )));
                }
            }
        }
        for (intent_id, intent) in &self.integration_intents {
            if intent_id != &intent.integration_intent_id
                || !self.invocations.contains_key(&intent.invocation_id)
            {
                return Err(BlackopsError::InvalidState(format!(
                    "integration intent {intent_id} has inconsistent identity"
                )));
            }
        }
        for (record_id, record) in &self.record_outbox {
            if record_id != &record.record_id || record.producer != "blackopsd" {
                return Err(BlackopsError::InvalidState(format!(
                    "record outbox entry {record_id} has inconsistent identity"
                )));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequestDedupRecord {
    pub idempotency_key: String,
    pub request_digest: String,
    pub operation_id: OperationId,
    pub agent_id: Option<AgentId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationStatus {
    Requested,
    Accepted,
    Completed,
    Failed,
    Interrupted,
    Lost,
}

impl OperationStatus {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Interrupted | Self::Lost
        )
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum OperationKind {
    SpawnAgent {
        agent_id: AgentId,
    },
    Followup {
        agent_id: AgentId,
        message_sequence: u64,
    },
    InterruptAgent {
        agent_id: AgentId,
    },
    InvokeDefinition {
        invocation_id: InvocationId,
    },
    RunWorkflowNode {
        invocation_id: InvocationId,
        node_id: String,
        generation: u32,
    },
}

impl OperationKind {
    pub fn agent_id(&self) -> Option<&AgentId> {
        match self {
            Self::SpawnAgent { agent_id }
            | Self::Followup { agent_id, .. }
            | Self::InterruptAgent { agent_id } => Some(agent_id),
            Self::InvokeDefinition { .. } | Self::RunWorkflowNode { .. } => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OperationTerminal {
    pub status: OperationStatus,
    #[serde(default)]
    pub result: Value,
    pub occurred_at_unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OperationRecord {
    pub operation_id: OperationId,
    pub idempotency_key: String,
    pub request_digest: String,
    pub kind: OperationKind,
    pub status: OperationStatus,
    pub execution_request: Option<ExecutionRequest>,
    pub accepted: Option<ExecutionAccepted>,
    pub last_attempt_state: Option<AttemptState>,
    pub terminal: Option<OperationTerminal>,
    pub requested_at_unix_ms: u64,
    pub updated_at_unix_ms: u64,
    pub last_error: Option<String>,
}

impl OperationRecord {
    fn validate(&self) -> BlackopsResult<()> {
        if self.idempotency_key.is_empty() || self.request_digest.is_empty() {
            return Err(BlackopsError::InvalidState(format!(
                "operation {} has empty idempotency data",
                self.operation_id
            )));
        }
        if self.status == OperationStatus::Requested
            && (self.accepted.is_some() || self.terminal.is_some())
        {
            return Err(BlackopsError::InvalidState(format!(
                "requested operation {} has accepted or terminal state",
                self.operation_id
            )));
        }
        if self.status == OperationStatus::Accepted
            && (self.accepted.is_none() || self.terminal.is_some())
        {
            return Err(BlackopsError::InvalidState(format!(
                "accepted operation {} has incoherent state",
                self.operation_id
            )));
        }
        if self.status.is_terminal()
            && self.terminal.as_ref().map(|terminal| terminal.status) != Some(self.status)
        {
            return Err(BlackopsError::InvalidState(format!(
                "terminal operation {} has incoherent terminal payload",
                self.operation_id
            )));
        }
        if let Some(request) = &self.execution_request
            && (request.operation_id != self.operation_id
                || request.idempotency_key != self.idempotency_key)
        {
            return Err(BlackopsError::InvalidState(format!(
                "operation {} differs from its execution request identity",
                self.operation_id
            )));
        }
        if let Some(accepted) = &self.accepted
            && accepted.operation_id != self.operation_id
        {
            return Err(BlackopsError::InvalidState(format!(
                "operation {} differs from its accepted execution identity",
                self.operation_id
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FleetEffect {
    pub effect_id: String,
    pub operation_id: OperationId,
    pub effect: FleetEffectKind,
    pub retry_count: u32,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum FleetEffectKind {
    RequestExecution { request: Box<ExecutionRequest> },
    InterruptTask { task_id: TaskId },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogicalAgentStatus {
    Requested,
    Ready,
    Running,
    InterruptRequested,
    Interrupted,
    Completed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentRecord {
    pub agent_id: AgentId,
    pub name: String,
    pub path: String,
    pub role: String,
    pub parent_id: Option<AgentId>,
    #[serde(default)]
    pub children: BTreeSet<AgentId>,
    pub status: LogicalAgentStatus,
    #[serde(default)]
    pub terminal_status_observers: BTreeSet<String>,
    pub current_operation_id: Option<OperationId>,
    pub current_attempt_id: Option<AttemptId>,
    pub current_task_id: Option<TaskId>,
    pub current_session_id: Option<SessionId>,
    #[serde(default)]
    pub current_worker_id: Option<String>,
    #[serde(default)]
    pub current_session_attempt_generation: Option<u64>,
    pub created_at_unix_ms: u64,
    pub updated_at_unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamMember {
    pub role: String,
    pub joined_at_unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamRecord {
    pub team_id: TeamId,
    pub name: String,
    #[serde(default)]
    pub members: BTreeMap<AgentId, TeamMember>,
    pub created_at_unix_ms: u64,
    pub updated_at_unix_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MailboxMessageKind {
    Send,
    Followup,
    System,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailboxMessage {
    pub message_id: String,
    pub sequence: u64,
    pub sender: Option<AgentId>,
    pub recipient: AgentId,
    pub kind: MailboxMessageKind,
    pub body: String,
    pub operation_id: Option<OperationId>,
    pub created_at_unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailboxCursor {
    pub name: String,
    pub last_sequence: u64,
    pub updated_at_unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mailbox {
    pub next_sequence: u64,
    #[serde(default)]
    pub messages: BTreeMap<u64, MailboxMessage>,
    #[serde(default)]
    pub cursors: BTreeMap<String, MailboxCursor>,
}

impl Default for Mailbox {
    fn default() -> Self {
        Self {
            next_sequence: 1,
            messages: BTreeMap::new(),
            cursors: BTreeMap::new(),
        }
    }
}

impl Mailbox {
    fn validate(&self, agent_id: &AgentId) -> BlackopsResult<()> {
        if self.next_sequence == 0 {
            return Err(BlackopsError::InvalidState(format!(
                "mailbox {agent_id} has a zero next sequence"
            )));
        }
        for (sequence, message) in &self.messages {
            if *sequence != message.sequence || &message.recipient != agent_id {
                return Err(BlackopsError::InvalidState(format!(
                    "mailbox {agent_id} contains an invalid message sequence"
                )));
            }
        }
        let last = self.next_sequence.saturating_sub(1);
        for (name, cursor) in &self.cursors {
            if name != &cursor.name || cursor.last_sequence > last {
                return Err(BlackopsError::InvalidState(format!(
                    "mailbox {agent_id} cursor {name} is invalid"
                )));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageDedupRecord {
    pub idempotency_key: String,
    pub request_digest: String,
    pub recipient: AgentId,
    pub sequence: u64,
    pub message_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailboxRead {
    pub agent_id: AgentId,
    pub cursor: String,
    pub messages: Vec<MailboxMessage>,
    pub last_sequence: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailboxDeliveryEffect {
    pub effect_id: String,
    pub delivery_id: String,
    pub recipient: AgentId,
    pub canonical_target: String,
    pub session_id: SessionId,
    pub through_sequence: u64,
    pub wake: bool,
    pub message: MailboxMessage,
    pub retry_count: u32,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DefinitionKind {
    Atom,
    Workflow,
}

impl fmt::Display for DefinitionKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Atom => "atom",
            Self::Workflow => "workflow",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct DefinitionName {
    pub kind: DefinitionKind,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct DefinitionKey {
    pub kind: DefinitionKind,
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OperationalDefinition {
    pub key: DefinitionKey,
    pub input_contract: Value,
    pub body: Value,
    pub content_digest: String,
    pub created_at_unix_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvocationStatus {
    Requested,
    Running,
    Completed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InvocationIntent {
    pub invocation_id: InvocationId,
    pub definition: DefinitionKey,
    pub input: Value,
    pub status: InvocationStatus,
    pub operation_id: Option<OperationId>,
    /// Terminal output for definitions completed inside blackopsd without a
    /// fleet attempt, such as deterministic and adapter atoms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<Value>,
    pub requested_at_unix_ms: u64,
    pub updated_at_unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InvocationTemplate {
    pub definition: DefinitionKey,
    pub input: Value,
    #[serde(default)]
    pub execution: Option<ScheduledExecution>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScheduledExecution {
    pub provider: Provider,
    pub model: String,
    pub prompt: String,
    pub effort: Option<String>,
    #[serde(default)]
    pub service_tier: ExecutionServiceTier,
    pub code_mode: Option<ExecutionCodeMode>,
    pub dispatch_context: Option<ExecutionDispatchContext>,
    pub working_set: WorkingSetIntent,
    #[serde(default)]
    pub shell_env: BTreeMap<String, String>,
    #[serde(default)]
    pub tool_policy: ExecutionToolPolicy,
    pub system_prompt: Option<String>,
    pub output_schema: Option<Value>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
}

pub const WORKFLOW_SCHEMA_VERSION: u32 = 1;
pub const WORKFLOW_NODE_CONTRACT_VERSION: u32 = 1;
pub const POLL_SOURCE_CONTRACT_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowDefinition {
    pub schema_version: u32,
    pub start: String,
    pub nodes: BTreeMap<String, WorkflowNodeDefinition>,
    #[serde(default)]
    pub terminal_intents: Vec<IntegrationIntentTemplate>,
}

impl WorkflowDefinition {
    pub fn validate(&self) -> BlackopsResult<()> {
        if self.schema_version != WORKFLOW_SCHEMA_VERSION {
            return Err(BlackopsError::InvalidRequest(format!(
                "workflow schema version {} is unsupported",
                self.schema_version
            )));
        }
        if self.nodes.is_empty() || self.nodes.len() > 256 || !self.nodes.contains_key(&self.start)
        {
            return Err(BlackopsError::InvalidRequest(
                "workflow must contain 1 to 256 nodes and a valid start node".into(),
            ));
        }
        for (node_id, node) in &self.nodes {
            if node_id.trim().is_empty() || node_id.len() > 128 {
                return Err(BlackopsError::InvalidRequest(
                    "workflow node IDs must contain 1 to 128 bytes".into(),
                ));
            }
            if node.contract_version != WORKFLOW_NODE_CONTRACT_VERSION {
                return Err(BlackopsError::InvalidRequest(format!(
                    "workflow node {node_id} contract version {} is unsupported",
                    node.contract_version
                )));
            }
            if node.retry.max_retries > 16 || node.retry.backoff_ms > 86_400_000 {
                return Err(BlackopsError::InvalidRequest(format!(
                    "workflow node {node_id} retry policy exceeds bounds"
                )));
            }
            if let WorkflowTransition::Goto { to } = &node.transition
                && !self.nodes.contains_key(to)
            {
                return Err(BlackopsError::InvalidRequest(format!(
                    "workflow node {node_id} targets missing node {to}"
                )));
            }
            match &node.kind {
                WorkflowNodeKind::Execution { execution, .. } => {
                    if execution.model.trim().is_empty() || execution.prompt.trim().is_empty() {
                        return Err(BlackopsError::InvalidRequest(format!(
                            "workflow execution node {node_id} requires model and prompt"
                        )));
                    }
                }
                WorkflowNodeKind::Wait {
                    topic, timeout_ms, ..
                } => {
                    if topic.trim().is_empty() || timeout_ms == &Some(0) {
                        return Err(BlackopsError::InvalidRequest(format!(
                            "workflow wait node {node_id} has an invalid topic or timeout"
                        )));
                    }
                }
                WorkflowNodeKind::Integration { intent } => {
                    validate_integration_template(node_id, intent)?;
                }
            }
        }
        for intent in &self.terminal_intents {
            validate_integration_template("terminal", intent)?;
        }
        Ok(())
    }
}

fn validate_integration_template(
    owner: &str,
    intent: &IntegrationIntentTemplate,
) -> BlackopsResult<()> {
    if intent.target.trim().is_empty() || intent.target.len() > 2_048 {
        return Err(BlackopsError::InvalidRequest(format!(
            "workflow {owner} integration target must contain 1 to 2048 bytes"
        )));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowNodeDefinition {
    pub contract_version: u32,
    pub kind: WorkflowNodeKind,
    pub transition: WorkflowTransition,
    #[serde(default)]
    pub retry: WorkflowRetryPolicy,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkflowNodeKind {
    Execution {
        execution: Box<ScheduledExecution>,
        #[serde(default)]
        input: Value,
    },
    Wait {
        topic: String,
        #[serde(default)]
        selector: Value,
        timeout_ms: Option<u64>,
    },
    Integration {
        intent: IntegrationIntentTemplate,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkflowTransition {
    Goto { to: String },
    Terminal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct WorkflowRetryPolicy {
    #[serde(default)]
    pub max_retries: u32,
    #[serde(default)]
    pub backoff_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowRunStatus {
    Running,
    Waiting,
    RetryScheduled,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowNodeStatus {
    Pending,
    Running,
    Waiting,
    RetryScheduled,
    Completed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowNodeState {
    pub node_id: String,
    pub generation: u32,
    pub status: WorkflowNodeStatus,
    pub operation_id: Option<OperationId>,
    pub wait_id: Option<WaitId>,
    pub next_attempt_unix_ms: Option<u64>,
    pub output: Option<Value>,
    pub last_error: Option<Value>,
    pub updated_at_unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowRun {
    pub invocation_id: InvocationId,
    pub definition: DefinitionKey,
    pub input: Value,
    pub status: WorkflowRunStatus,
    pub current_node: String,
    #[serde(default)]
    pub nodes: BTreeMap<String, WorkflowNodeState>,
    #[serde(default)]
    pub outputs: BTreeMap<String, Value>,
    pub waiting_on: Option<WaitId>,
    #[serde(default)]
    pub integration_intent_ids: Vec<IntegrationIntentId>,
    pub created_at_unix_ms: u64,
    pub updated_at_unix_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationIntentKind {
    Publish,
    Integrate,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IntegrationIntentTemplate {
    pub kind: IntegrationIntentKind,
    pub target: String,
    #[serde(default)]
    pub payload: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationIntentStatus {
    Requested,
    Completed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IntegrationIntent {
    pub integration_intent_id: IntegrationIntentId,
    pub invocation_id: InvocationId,
    pub node_id: Option<String>,
    pub kind: IntegrationIntentKind,
    pub target: String,
    pub payload: Value,
    pub status: IntegrationIntentStatus,
    pub result: Option<Value>,
    pub created_at_unix_ms: u64,
    pub updated_at_unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IntegrationIntentResolveRequest {
    pub status: IntegrationIntentStatus,
    #[serde(default)]
    pub result: Value,
    pub resolved_at_unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PollSourceSpec {
    pub contract_version: u32,
    pub url: String,
    #[serde(default = "default_poll_method")]
    pub method: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    pub body: Option<Value>,
    #[serde(default = "default_poll_timeout_ms")]
    pub timeout_ms: u64,
    pub items_pointer: Option<String>,
    pub delivery_id_pointer: Option<String>,
    #[serde(default = "default_poll_max_items")]
    pub max_items: usize,
}

fn default_poll_method() -> String {
    "GET".into()
}

fn default_poll_timeout_ms() -> u64 {
    10_000
}

fn default_poll_max_items() -> usize {
    64
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PollDeliveryRecord {
    pub delivery_id: String,
    pub payload_digest: String,
    pub observed_sequence: u64,
    pub observed_at_unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PollSourceCursor {
    pub schedule_id: ScheduleId,
    pub schedule_generation: u64,
    pub fetch_sequence: u64,
    #[serde(default)]
    pub deliveries: BTreeMap<String, PollDeliveryRecord>,
    pub updated_at_unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum ScheduleTrigger {
    At {
        unix_ms: u64,
    },
    Interval {
        every_ms: u64,
        anchor_unix_ms: u64,
    },
    Cron {
        expression: String,
    },
    Webhook {
        path: String,
    },
    Poll {
        every_ms: u64,
        source: PollSourceSpec,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScheduleIntent {
    pub schedule_id: ScheduleId,
    pub name: String,
    pub invocation: InvocationTemplate,
    pub trigger: ScheduleTrigger,
    pub enabled: bool,
    pub next_due_unix_ms: Option<u64>,
    pub generation: u64,
    pub created_at_unix_ms: u64,
    pub updated_at_unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WebhookAdmissionRequest {
    pub delivery_id: String,
    #[serde(default)]
    pub payload: Value,
    pub occurred_at_unix_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitStatus {
    Pending,
    Satisfied,
    Cancelled,
    TimedOut,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OperationalWait {
    pub wait_id: WaitId,
    pub topic: String,
    pub selector: Value,
    pub status: WaitStatus,
    pub deadline_unix_ms: Option<u64>,
    pub resolution: Option<Value>,
    pub created_at_unix_ms: u64,
    pub updated_at_unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WaitCreateRequest {
    pub wait_id: WaitId,
    pub topic: String,
    pub selector: Value,
    pub deadline_unix_ms: Option<u64>,
    pub created_at_unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WaitResolveRequest {
    pub status: WaitStatus,
    pub resolution: Value,
    pub resolved_at_unix_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalStatus {
    Pending,
    Approved,
    Rejected,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApprovalRecord {
    pub approval_id: ApprovalId,
    pub requested_by: String,
    pub action: Value,
    pub status: ApprovalStatus,
    pub decision: Option<Value>,
    pub created_at_unix_ms: u64,
    pub resolved_at_unix_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApprovalCreateRequest {
    pub approval_id: ApprovalId,
    pub requested_by: String,
    pub action: Value,
    pub created_at_unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApprovalResolveRequest {
    pub status: ApprovalStatus,
    pub decision: Value,
    pub resolved_at_unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WhiteboardRecord {
    pub whiteboard_id: WhiteboardId,
    pub name: String,
    pub generation: u64,
    #[serde(default)]
    pub entries: BTreeMap<String, WhiteboardEntry>,
    pub created_at_unix_ms: u64,
    pub updated_at_unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WhiteboardEntry {
    pub key: String,
    pub value: Value,
    pub revision: u64,
    pub updated_by: String,
    pub updated_at_unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WhiteboardCreateRequest {
    pub whiteboard_id: WhiteboardId,
    pub name: String,
    pub created_at_unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WhiteboardPutRequest {
    pub value: Value,
    pub expected_revision: Option<u64>,
    pub updated_by: String,
    pub updated_at_unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpawnAgentRequest {
    pub idempotency_key: String,
    pub name: String,
    pub role: String,
    pub parent_id: Option<AgentId>,
    pub team_id: Option<TeamId>,
    pub execution: ExecutionRequest,
    pub requested_at_unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpawnAgentReceipt {
    pub agent: AgentRecord,
    pub operation: OperationRecord,
    pub deduplicated: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SendMessageRequest {
    pub idempotency_key: String,
    pub sender: Option<AgentId>,
    pub recipient: AgentId,
    pub body: String,
    pub created_at_unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FollowupAgentRequest {
    pub idempotency_key: String,
    pub sender: Option<AgentId>,
    pub recipient: AgentId,
    pub body: String,
    pub execution: ExecutionRequest,
    pub requested_at_unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FollowupReceipt {
    pub message: MailboxMessage,
    pub operation: OperationRecord,
    pub deduplicated: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InterruptAgentRequest {
    pub operation_id: OperationId,
    pub idempotency_key: String,
    pub agent_id: AgentId,
    pub requested_at_unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InterruptReceipt {
    pub operation: OperationRecord,
    pub deduplicated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamCreateRequest {
    pub name: String,
    pub created_at_unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamAssignment {
    pub team_id: TeamId,
    pub agent_id: AgentId,
    pub role: String,
    pub assigned_at_unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DefinitionInstallRequest {
    pub kind: DefinitionKind,
    pub name: String,
    pub version: String,
    pub input_contract: Value,
    pub body: Value,
    pub activate: bool,
    pub created_at_unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InvocationRequest {
    pub invocation_id: InvocationId,
    pub definition: DefinitionKey,
    pub input: Value,
    pub execution: Option<ExecutionRequest>,
    pub requested_at_unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReconciliationTarget {
    pub operation_id: OperationId,
    pub attempt_id: AttemptId,
}

pub(crate) fn terminal_status(state: AttemptState) -> Option<OperationStatus> {
    match state {
        AttemptState::Completed => Some(OperationStatus::Completed),
        AttemptState::Failed => Some(OperationStatus::Failed),
        AttemptState::Interrupted => Some(OperationStatus::Interrupted),
        AttemptState::Lost => Some(OperationStatus::Lost),
        AttemptState::Accepted | AttemptState::Running => None,
    }
}

pub(crate) fn outcome_terminal(outcome: &AttemptOutcome) -> bool {
    terminal_status(outcome.state).is_some()
}
