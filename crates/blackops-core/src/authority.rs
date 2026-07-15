use std::collections::BTreeMap;
use std::str::FromStr;

use bro_capabilities::{
    AttemptOutcome, AttemptState, ExecutionAccepted, ExecutionKind, ExecutionRequest,
    MAX_RECORD_BYTES, RecordEnvelope,
};
use bro_core::{AttemptId, OperationId, SessionId, TaskId};
use chrono::{TimeZone, Utc};
use cron::Schedule;
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::model::{outcome_terminal, terminal_status};
use crate::{
    AgentId, AgentRecord, ApprovalCreateRequest, ApprovalId, ApprovalRecord,
    ApprovalResolveRequest, ApprovalStatus, BlackopsError, BlackopsResult,
    DefinitionInstallRequest, DefinitionKey, DefinitionName, FleetEffect, FleetEffectKind,
    FollowupAgentRequest, FollowupReceipt, IdentityGenerator, IntegrationIntent,
    IntegrationIntentId, IntegrationIntentResolveRequest, IntegrationIntentStatus,
    IntegrationIntentTemplate, InterruptAgentRequest, InterruptReceipt, InvocationId,
    InvocationIntent, InvocationRequest, InvocationStatus, LogicalAgentStatus, Mailbox,
    MailboxCursor, MailboxDeliveryEffect, MailboxMessage, MailboxMessageKind, MailboxRead,
    MessageDedupRecord, OperationKind, OperationRecord, OperationStatus, OperationTerminal,
    OperationalDefinition, OperationalRepository, OperationalSnapshot, OperationalWait,
    PollDeliveryRecord, PollSourceCursor, PollSourceSpec, ReconciliationTarget, RequestDedupRecord,
    ScheduleIntent, ScheduleTrigger, SendMessageRequest, SpawnAgentReceipt, SpawnAgentRequest,
    TeamAssignment, TeamCreateRequest, TeamId, TeamMember, TeamRecord, UuidIdentityGenerator,
    WaitCreateRequest, WaitId, WaitResolveRequest, WaitStatus, WebhookAdmissionRequest,
    WhiteboardCreateRequest, WhiteboardEntry, WhiteboardId, WhiteboardPutRequest, WhiteboardRecord,
    WorkflowDefinition, WorkflowNodeKind, WorkflowNodeState, WorkflowNodeStatus, WorkflowRun,
    WorkflowRunStatus, WorkflowTransition,
};

pub struct BlackopsAuthority<R, I = UuidIdentityGenerator> {
    repository: R,
    identities: I,
    snapshot: OperationalSnapshot,
}

impl<R: OperationalRepository> BlackopsAuthority<R, UuidIdentityGenerator> {
    pub fn open(repository: R) -> BlackopsResult<Self> {
        Self::open_with_identities(repository, UuidIdentityGenerator)
    }
}

impl<R: OperationalRepository, I: IdentityGenerator> BlackopsAuthority<R, I> {
    pub fn open_with_identities(repository: R, identities: I) -> BlackopsResult<Self> {
        let snapshot = repository.load()?.unwrap_or_default();
        snapshot.validate()?;
        Ok(Self {
            repository,
            identities,
            snapshot,
        })
    }

    pub fn snapshot(&self) -> OperationalSnapshot {
        self.snapshot.clone()
    }

    pub fn operation(&self, operation_id: &OperationId) -> BlackopsResult<OperationRecord> {
        self.snapshot
            .operations
            .get(operation_id)
            .cloned()
            .ok_or_else(|| BlackopsError::NotFound(format!("operation {operation_id}")))
    }

    pub fn agent(&self, agent_id: &AgentId) -> BlackopsResult<AgentRecord> {
        self.snapshot
            .agents
            .get(agent_id)
            .cloned()
            .ok_or_else(|| BlackopsError::NotFound(format!("agent {agent_id}")))
    }

    pub fn list_agents(&self) -> Vec<AgentRecord> {
        self.snapshot.agents.values().cloned().collect()
    }

    pub fn claim_terminal_agent_status(
        &mut self,
        path_prefix: &str,
        observer: &str,
    ) -> BlackopsResult<Option<AgentRecord>> {
        if observer.trim().is_empty() {
            return Err(BlackopsError::InvalidRequest(
                "terminal status observer must not be empty".into(),
            ));
        }
        let selected = self
            .snapshot
            .agents
            .values()
            .find(|agent| {
                path_in_tree(path_prefix, &agent.path)
                    && matches!(
                        agent.status,
                        LogicalAgentStatus::Completed
                            | LogicalAgentStatus::Interrupted
                            | LogicalAgentStatus::Failed
                    )
                    && !agent.terminal_status_observers.contains(observer)
            })
            .map(|agent| agent.agent_id.clone());
        let Some(agent_id) = selected else {
            return Ok(None);
        };
        let observer = observer.to_string();
        self.transact(|snapshot| {
            let agent = snapshot
                .agents
                .get_mut(&agent_id)
                .expect("terminal agent selected from snapshot");
            agent.terminal_status_observers.insert(observer);
            Ok(Some(agent.clone()))
        })
    }

    pub fn agent_by_path(&self, canonical_path: &str) -> BlackopsResult<AgentRecord> {
        let agent_id = self
            .snapshot
            .agent_paths
            .get(canonical_path)
            .ok_or_else(|| BlackopsError::NotFound(format!("agent path {canonical_path}")))?;
        self.agent(agent_id)
    }

    pub fn agent_for_session(&self, session_id: &bro_core::SessionId) -> Option<AgentRecord> {
        self.snapshot
            .agents
            .values()
            .find(|agent| agent.current_session_id.as_ref() == Some(session_id))
            .cloned()
    }

    pub fn list_teams(&self) -> Vec<TeamRecord> {
        self.snapshot.teams.values().cloned().collect()
    }

    /// Establishes the canonical logical root for one authenticated worker
    /// session. This is identity binding only and never requests execution.
    pub fn ensure_session_root(
        &mut self,
        session_id: &SessionId,
        worker_id: &str,
        task_id: &TaskId,
        attempt_id: &AttemptId,
        session_attempt_generation: u64,
        now_unix_ms: u64,
    ) -> BlackopsResult<AgentRecord> {
        if session_id.as_str().trim().is_empty()
            || worker_id.trim().is_empty()
            || task_id.as_str().trim().is_empty()
            || attempt_id.as_str().trim().is_empty()
            || session_attempt_generation == 0
        {
            return Err(BlackopsError::InvalidRequest(
                "session root authority identity and generation must be nonempty".into(),
            ));
        }
        if let Some(existing) = self.agent_for_session(session_id) {
            if let Some(bound_generation) = existing.current_session_attempt_generation {
                if session_attempt_generation < bound_generation {
                    return Err(BlackopsError::Conflict(format!(
                        "session {session_id} rejected stale attempt generation {session_attempt_generation}; current generation is {bound_generation}"
                    )));
                }
                if session_attempt_generation == bound_generation {
                    if existing.current_worker_id.as_deref() == Some(worker_id)
                        && existing.current_task_id.as_ref() == Some(task_id)
                        && existing.current_attempt_id.as_ref() == Some(attempt_id)
                    {
                        return Ok(existing);
                    }
                    return Err(BlackopsError::Conflict(format!(
                        "session {session_id} rejected identity drift at attempt generation {session_attempt_generation}"
                    )));
                }
            } else {
                if existing
                    .current_worker_id
                    .as_deref()
                    .is_some_and(|bound_worker_id| bound_worker_id != worker_id)
                    || existing
                        .current_task_id
                        .as_ref()
                        .is_some_and(|bound_task_id| bound_task_id != task_id)
                    || existing
                        .current_attempt_id
                        .as_ref()
                        .is_some_and(|bound_attempt_id| bound_attempt_id != attempt_id)
                {
                    return Err(BlackopsError::Conflict(format!(
                        "session {session_id} legacy binding differs from authenticated attempt identity"
                    )));
                }
            }
            let agent_id = existing.agent_id.clone();
            let worker_id = worker_id.to_string();
            let task_id = task_id.clone();
            let attempt_id = attempt_id.clone();
            return self.transact(|snapshot| {
                let agent = snapshot
                    .agents
                    .get_mut(&agent_id)
                    .expect("session agent exists in the same snapshot");
                agent.current_worker_id = Some(worker_id.clone());
                agent.current_task_id = Some(task_id.clone());
                agent.current_attempt_id = Some(attempt_id.clone());
                agent.current_session_attempt_generation = Some(session_attempt_generation);
                agent.updated_at_unix_ms = now_unix_ms;
                let agent = agent.clone();
                queue_record(
                    snapshot,
                    format!(
                        "blackops:agent:{agent_id}:worker-binding:{session_attempt_generation}"
                    ),
                    "agent.worker_bound",
                    Some(agent_id.to_string()),
                    serde_json::to_value(&agent)?,
                )?;
                Ok(agent)
            });
        }
        let digest = request_digest(&json!({
            "session_id": session_id,
            "worker_id": worker_id
        }))?;
        let segment = format!("session-{}", &digest[7..23]);
        let path = format!("/sessions/{segment}");
        if let Some(existing_id) = self.snapshot.agent_paths.get(&path) {
            return self.agent(existing_id);
        }
        let agent_id = AgentId::new(self.identities.next_id("agent"));
        let agent = AgentRecord {
            agent_id: agent_id.clone(),
            name: format!("session:{session_id}"),
            path,
            role: "session-root".into(),
            parent_id: None,
            children: Default::default(),
            status: LogicalAgentStatus::Running,
            terminal_status_observers: Default::default(),
            current_operation_id: None,
            current_attempt_id: Some(attempt_id.clone()),
            current_task_id: Some(task_id.clone()),
            current_session_id: Some(session_id.clone()),
            current_worker_id: Some(worker_id.to_string()),
            current_session_attempt_generation: Some(session_attempt_generation),
            created_at_unix_ms: now_unix_ms,
            updated_at_unix_ms: now_unix_ms,
        };
        self.transact(|snapshot| {
            snapshot
                .agent_paths
                .insert(agent.path.clone(), agent_id.clone());
            snapshot.agents.insert(agent_id.clone(), agent.clone());
            snapshot
                .mailboxes
                .insert(agent_id.clone(), Mailbox::default());
            queue_record(
                snapshot,
                format!("blackops:agent:{agent_id}:session-root"),
                "agent.session_root",
                Some(agent_id.to_string()),
                serde_json::to_value(&agent)?,
            )?;
            Ok(agent.clone())
        })
    }

    pub fn create_team(&mut self, request: TeamCreateRequest) -> BlackopsResult<TeamRecord> {
        let name = request.name.trim();
        if name.is_empty() {
            return Err(BlackopsError::InvalidRequest(
                "team name must not be empty".into(),
            ));
        }
        if self.snapshot.teams.values().any(|team| team.name == name) {
            return Err(BlackopsError::Conflict(format!(
                "team name {name} already exists"
            )));
        }
        let team_id = TeamId::new(self.identities.next_id("team"));
        let team = TeamRecord {
            team_id: team_id.clone(),
            name: name.to_string(),
            members: BTreeMap::new(),
            created_at_unix_ms: request.created_at_unix_ms,
            updated_at_unix_ms: request.created_at_unix_ms,
        };
        self.transact(|snapshot| {
            snapshot.teams.insert(team_id.clone(), team.clone());
            queue_record(
                snapshot,
                format!("blackops:team:{team_id}:created"),
                "team.created",
                Some(team_id.to_string()),
                serde_json::to_value(&team)?,
            )?;
            Ok(team.clone())
        })
    }

    pub fn assign_team_member(&mut self, assignment: TeamAssignment) -> BlackopsResult<TeamRecord> {
        if assignment.role.trim().is_empty() {
            return Err(BlackopsError::InvalidRequest(
                "team role must not be empty".into(),
            ));
        }
        if !self.snapshot.agents.contains_key(&assignment.agent_id) {
            return Err(BlackopsError::NotFound(format!(
                "agent {}",
                assignment.agent_id
            )));
        }
        self.transact(|snapshot| {
            let team = snapshot
                .teams
                .get_mut(&assignment.team_id)
                .ok_or_else(|| BlackopsError::NotFound(format!("team {}", assignment.team_id)))?;
            team.members.insert(
                assignment.agent_id.clone(),
                TeamMember {
                    role: assignment.role.clone(),
                    joined_at_unix_ms: assignment.assigned_at_unix_ms,
                },
            );
            team.updated_at_unix_ms = assignment.assigned_at_unix_ms;
            let team = team.clone();
            queue_record(
                snapshot,
                format!(
                    "blackops:team:{}:member:{}",
                    assignment.team_id, assignment.agent_id
                ),
                "team.member.assigned",
                Some(assignment.team_id.to_string()),
                serde_json::to_value(&assignment)?,
            )?;
            Ok(team)
        })
    }

    pub fn spawn_agent(&mut self, request: SpawnAgentRequest) -> BlackopsResult<SpawnAgentReceipt> {
        validate_execution_identity(&request.idempotency_key, &request.execution)?;
        if !matches!(request.execution.kind, ExecutionKind::Fresh { .. }) {
            return Err(BlackopsError::InvalidRequest(
                "spawn execution must be fresh".into(),
            ));
        }
        let digest = effect_request_digest(&request, &["requested_at_unix_ms"])?;
        if let Some(prior) = self.deduplicated_request(&request.idempotency_key, &digest)? {
            let agent_id = prior.agent_id.ok_or_else(|| {
                BlackopsError::InvalidState("spawn dedup record has no agent identity".into())
            })?;
            return Ok(SpawnAgentReceipt {
                agent: self.agent(&agent_id)?,
                operation: self.operation(&prior.operation_id)?,
                deduplicated: true,
            });
        }
        let segment = canonical_segment(&request.name)?;
        let parent_path = match &request.parent_id {
            Some(parent_id) => self.agent(parent_id)?.path,
            None => String::new(),
        };
        let path = if parent_path.is_empty() {
            format!("/{segment}")
        } else {
            format!("{parent_path}/{segment}")
        };
        if self.snapshot.agent_paths.contains_key(&path) {
            return Err(BlackopsError::Conflict(format!(
                "agent path {path} already exists"
            )));
        }
        if let Some(team_id) = &request.team_id
            && !self.snapshot.teams.contains_key(team_id)
        {
            return Err(BlackopsError::NotFound(format!("team {team_id}")));
        }
        let agent_id = AgentId::new(self.identities.next_id("agent"));
        let operation_id = request.execution.operation_id.clone();
        let agent = AgentRecord {
            agent_id: agent_id.clone(),
            name: request.name.trim().to_string(),
            path,
            role: request.role.clone(),
            parent_id: request.parent_id.clone(),
            children: Default::default(),
            status: LogicalAgentStatus::Requested,
            terminal_status_observers: Default::default(),
            current_operation_id: Some(operation_id.clone()),
            current_attempt_id: None,
            current_task_id: None,
            current_session_id: None,
            current_worker_id: None,
            current_session_attempt_generation: None,
            created_at_unix_ms: request.requested_at_unix_ms,
            updated_at_unix_ms: request.requested_at_unix_ms,
        };
        let mut bound_execution = request.execution.clone();
        bound_execution
            .labels
            .insert("logical_agent_id".into(), agent_id.to_string());
        bound_execution
            .labels
            .insert("logical_agent_path".into(), agent.path.clone());
        let operation = execution_operation(
            bound_execution.clone(),
            digest.clone(),
            OperationKind::SpawnAgent {
                agent_id: agent_id.clone(),
            },
            request.requested_at_unix_ms,
        );
        self.transact(|snapshot| {
            snapshot
                .agent_paths
                .insert(agent.path.clone(), agent_id.clone());
            snapshot.agents.insert(agent_id.clone(), agent.clone());
            snapshot
                .mailboxes
                .insert(agent_id.clone(), Mailbox::default());
            if let Some(parent_id) = &request.parent_id {
                snapshot
                    .agents
                    .get_mut(parent_id)
                    .expect("parent checked before transaction")
                    .children
                    .insert(agent_id.clone());
            }
            if let Some(team_id) = &request.team_id {
                snapshot
                    .teams
                    .get_mut(team_id)
                    .expect("team checked before transaction")
                    .members
                    .insert(
                        agent_id.clone(),
                        TeamMember {
                            role: request.role.clone(),
                            joined_at_unix_ms: request.requested_at_unix_ms,
                        },
                    );
            }
            install_execution_operation(
                snapshot,
                operation.clone(),
                Some(agent_id.clone()),
                bound_execution.clone(),
            );
            queue_record(
                snapshot,
                format!("blackops:agent:{agent_id}:created"),
                "agent.created",
                Some(agent_id.to_string()),
                serde_json::to_value(&agent)?,
            )?;
            queue_operation_record(snapshot, &operation, "requested")?;
            Ok(SpawnAgentReceipt {
                agent: agent.clone(),
                operation: operation.clone(),
                deduplicated: false,
            })
        })
    }

    pub fn send_message(&mut self, request: SendMessageRequest) -> BlackopsResult<MailboxMessage> {
        validate_message_request(
            &self.snapshot,
            &request.sender,
            &request.recipient,
            &request.body,
        )?;
        let digest = effect_request_digest(&request, &["created_at_unix_ms"])?;
        if let Some(prior) = self.snapshot.message_dedup.get(&request.idempotency_key) {
            if prior.request_digest != digest {
                return Err(BlackopsError::Conflict(
                    "message idempotency key was reused with a different request".into(),
                ));
            }
            return self
                .snapshot
                .mailboxes
                .get(&prior.recipient)
                .and_then(|mailbox| mailbox.messages.get(&prior.sequence))
                .cloned()
                .ok_or_else(|| {
                    BlackopsError::InvalidState(
                        "message dedup record points to a missing message".into(),
                    )
                });
        }
        let message_id = self.identities.next_id("message");
        self.transact(|snapshot| {
            let message = append_message(
                snapshot,
                &request.idempotency_key,
                &digest,
                message_id.clone(),
                request.sender.clone(),
                request.recipient.clone(),
                MailboxMessageKind::Send,
                request.body.clone(),
                None,
                request.created_at_unix_ms,
            )?;
            queue_mailbox_delivery(snapshot, &message, false)?;
            Ok(message)
        })
    }

    pub fn followup_agent(
        &mut self,
        request: FollowupAgentRequest,
    ) -> BlackopsResult<FollowupReceipt> {
        validate_execution_identity(&request.idempotency_key, &request.execution)?;
        validate_message_request(
            &self.snapshot,
            &request.sender,
            &request.recipient,
            &request.body,
        )?;
        let agent = self.agent(&request.recipient)?;
        let expected_session = agent.current_session_id.ok_or_else(|| {
            BlackopsError::Conflict("followup requires an accepted agent session".into())
        })?;
        match &request.execution.kind {
            ExecutionKind::MailboxResume { session_id } if session_id == &expected_session => {}
            ExecutionKind::MailboxResume { .. } => {
                return Err(BlackopsError::Conflict(
                    "followup resume session differs from the logical agent session".into(),
                ));
            }
            ExecutionKind::Fresh { .. } | ExecutionKind::Resume { .. } => {
                return Err(BlackopsError::InvalidRequest(
                    "followup execution must use a mailbox resume".into(),
                ));
            }
        }
        let digest = effect_request_digest(&request, &["requested_at_unix_ms"])?;
        if let Some(prior) = self.deduplicated_request(&request.idempotency_key, &digest)? {
            let operation = self.operation(&prior.operation_id)?;
            let sequence = match operation.kind {
                OperationKind::Followup {
                    message_sequence, ..
                } => message_sequence,
                _ => {
                    return Err(BlackopsError::InvalidState(
                        "followup dedup record points to another operation kind".into(),
                    ));
                }
            };
            let message = self.snapshot.mailboxes[&request.recipient].messages[&sequence].clone();
            return Ok(FollowupReceipt {
                message,
                operation,
                deduplicated: true,
            });
        }
        let sequence = self.snapshot.mailboxes[&request.recipient].next_sequence;
        let message_id = self.identities.next_id("message");
        let operation = execution_operation(
            request.execution.clone(),
            digest.clone(),
            OperationKind::Followup {
                agent_id: request.recipient.clone(),
                message_sequence: sequence,
            },
            request.requested_at_unix_ms,
        );
        self.transact(|snapshot| {
            let message = append_message(
                snapshot,
                &request.idempotency_key,
                &digest,
                message_id.clone(),
                request.sender.clone(),
                request.recipient.clone(),
                MailboxMessageKind::Followup,
                request.body.clone(),
                Some(operation.operation_id.clone()),
                request.requested_at_unix_ms,
            )?;
            queue_mailbox_delivery(snapshot, &message, true)?;
            install_execution_operation(
                snapshot,
                operation.clone(),
                Some(request.recipient.clone()),
                request.execution.clone(),
            );
            let agent = snapshot
                .agents
                .get_mut(&request.recipient)
                .expect("recipient checked before transaction");
            agent.status = LogicalAgentStatus::Requested;
            agent.terminal_status_observers.clear();
            agent.current_operation_id = Some(operation.operation_id.clone());
            agent.updated_at_unix_ms = request.requested_at_unix_ms;
            queue_operation_record(snapshot, &operation, "requested")?;
            Ok(FollowupReceipt {
                message,
                operation: operation.clone(),
                deduplicated: false,
            })
        })
    }

    pub fn interrupt_agent(
        &mut self,
        request: InterruptAgentRequest,
    ) -> BlackopsResult<InterruptReceipt> {
        if request.idempotency_key.trim().is_empty() {
            return Err(BlackopsError::InvalidRequest(
                "idempotency key must not be empty".into(),
            ));
        }
        let digest = effect_request_digest(&request, &["requested_at_unix_ms"])?;
        if let Some(prior) = self.deduplicated_request(&request.idempotency_key, &digest)? {
            return Ok(InterruptReceipt {
                operation: self.operation(&prior.operation_id)?,
                deduplicated: true,
            });
        }
        if self.snapshot.operations.contains_key(&request.operation_id) {
            return Err(BlackopsError::Conflict(format!(
                "operation {} already exists",
                request.operation_id
            )));
        }
        let agent = self.agent(&request.agent_id)?;
        let task_id = agent.current_task_id.ok_or_else(|| {
            BlackopsError::Conflict("agent has no accepted task to interrupt".into())
        })?;
        let operation = OperationRecord {
            operation_id: request.operation_id.clone(),
            idempotency_key: request.idempotency_key.clone(),
            request_digest: digest.clone(),
            kind: OperationKind::InterruptAgent {
                agent_id: request.agent_id.clone(),
            },
            status: OperationStatus::Requested,
            execution_request: None,
            accepted: None,
            last_attempt_state: None,
            terminal: None,
            requested_at_unix_ms: request.requested_at_unix_ms,
            updated_at_unix_ms: request.requested_at_unix_ms,
            last_error: None,
        };
        self.transact(|snapshot| {
            snapshot
                .operations
                .insert(request.operation_id.clone(), operation.clone());
            snapshot.request_dedup.insert(
                request.idempotency_key.clone(),
                RequestDedupRecord {
                    idempotency_key: request.idempotency_key.clone(),
                    request_digest: digest.clone(),
                    operation_id: request.operation_id.clone(),
                    agent_id: Some(request.agent_id.clone()),
                },
            );
            snapshot.fleet_outbox.insert(
                fleet_effect_id(&request.operation_id),
                FleetEffect {
                    effect_id: fleet_effect_id(&request.operation_id),
                    operation_id: request.operation_id.clone(),
                    effect: FleetEffectKind::InterruptTask {
                        task_id: task_id.clone(),
                    },
                    retry_count: 0,
                    last_error: None,
                },
            );
            let agent = snapshot
                .agents
                .get_mut(&request.agent_id)
                .expect("agent checked before transaction");
            agent.status = LogicalAgentStatus::InterruptRequested;
            agent.current_operation_id = Some(request.operation_id.clone());
            agent.updated_at_unix_ms = request.requested_at_unix_ms;
            queue_operation_record(snapshot, &operation, "requested")?;
            Ok(InterruptReceipt {
                operation: operation.clone(),
                deduplicated: false,
            })
        })
    }

    pub fn read_mailbox(
        &mut self,
        agent_id: &AgentId,
        cursor_name: &str,
        after_sequence: Option<u64>,
        limit: usize,
        now_unix_ms: u64,
    ) -> BlackopsResult<MailboxRead> {
        if cursor_name.trim().is_empty() {
            return Err(BlackopsError::InvalidRequest(
                "mailbox cursor name must not be empty".into(),
            ));
        }
        if limit == 0 || limit > 1_000 {
            return Err(BlackopsError::InvalidRequest(
                "mailbox read limit must be between 1 and 1000".into(),
            ));
        }
        let mailbox = self
            .snapshot
            .mailboxes
            .get(agent_id)
            .ok_or_else(|| BlackopsError::NotFound(format!("agent {agent_id}")))?;
        let durable = mailbox
            .cursors
            .get(cursor_name)
            .map_or(0, |cursor| cursor.last_sequence);
        let start = durable.max(after_sequence.unwrap_or(0));
        let messages: Vec<_> = mailbox
            .messages
            .range(start.saturating_add(1)..)
            .take(limit)
            .map(|(_, message)| message.clone())
            .collect();
        let last_sequence = messages.last().map_or(start, |message| message.sequence);
        let needs_write = mailbox
            .cursors
            .get(cursor_name)
            .is_none_or(|cursor| cursor.last_sequence != last_sequence);
        if needs_write {
            let agent_id = agent_id.clone();
            let cursor_name = cursor_name.to_string();
            self.transact(|snapshot| {
                snapshot
                    .mailboxes
                    .get_mut(&agent_id)
                    .expect("mailbox checked before transaction")
                    .cursors
                    .insert(
                        cursor_name.clone(),
                        MailboxCursor {
                            name: cursor_name,
                            last_sequence,
                            updated_at_unix_ms: now_unix_ms,
                        },
                    );
                Ok(())
            })?;
        }
        Ok(MailboxRead {
            agent_id: agent_id.clone(),
            cursor: cursor_name.to_string(),
            messages,
            last_sequence,
        })
    }

    pub fn pending_fleet_effects(&self) -> Vec<FleetEffect> {
        self.snapshot.fleet_outbox.values().cloned().collect()
    }

    pub fn pending_mailbox_delivery_effects(&self) -> Vec<MailboxDeliveryEffect> {
        self.snapshot
            .mailbox_delivery_outbox
            .values()
            .cloned()
            .collect()
    }

    /// Advance a delivery cursor only after fleetd reports durable worker
    /// admission. Gaps fail closed so a later effect cannot hide an earlier
    /// undelivered mailbox item.
    pub fn acknowledge_mailbox_delivery(
        &mut self,
        effect_id: &str,
        delivery_id: &str,
        through_sequence: u64,
        now_unix_ms: u64,
    ) -> BlackopsResult<bool> {
        let Some(effect) = self.snapshot.mailbox_delivery_outbox.get(effect_id) else {
            return Ok(false);
        };
        if effect.delivery_id != delivery_id || effect.through_sequence != through_sequence {
            return Err(BlackopsError::Conflict(
                "fleet mailbox admission receipt differs from the durable effect".into(),
            ));
        }
        let effect = effect.clone();
        self.transact(|snapshot| {
            let cursor_name = mailbox_delivery_cursor(&effect.session_id);
            let mailbox = snapshot
                .mailboxes
                .get_mut(&effect.recipient)
                .expect("delivery effect validation guarantees a mailbox");
            let prior = mailbox
                .cursors
                .get(&cursor_name)
                .map_or(0, |cursor| cursor.last_sequence);
            if through_sequence > prior.saturating_add(1) {
                return Err(BlackopsError::Conflict(format!(
                    "mailbox delivery acknowledgement skipped sequence {}",
                    prior.saturating_add(1)
                )));
            }
            if through_sequence > prior {
                mailbox.cursors.insert(
                    cursor_name.clone(),
                    MailboxCursor {
                        name: cursor_name,
                        last_sequence: through_sequence,
                        updated_at_unix_ms: now_unix_ms,
                    },
                );
            }
            snapshot.mailbox_delivery_outbox.remove(effect_id);
            if let Some(operation_id) = effect.message.operation_id.as_ref() {
                let operation = snapshot.operations.get(operation_id).ok_or_else(|| {
                    BlackopsError::InvalidState(format!(
                        "mailbox delivery references missing operation {operation_id}"
                    ))
                })?;
                match &operation.kind {
                    OperationKind::Followup {
                        agent_id,
                        message_sequence,
                    } if agent_id == &effect.recipient
                        && *message_sequence == effect.message.sequence => {}
                    _ => {
                        return Err(BlackopsError::InvalidState(
                            "mailbox delivery operation does not match its message".into(),
                        ));
                    }
                }
            }
            Ok(true)
        })
    }

    pub fn note_mailbox_delivery_error(
        &mut self,
        effect_id: &str,
        error: String,
    ) -> BlackopsResult<()> {
        if !self
            .snapshot
            .mailbox_delivery_outbox
            .contains_key(effect_id)
        {
            return Ok(());
        }
        self.transact(|snapshot| {
            let effect = snapshot
                .mailbox_delivery_outbox
                .get_mut(effect_id)
                .expect("mailbox effect checked before transaction");
            effect.retry_count = effect.retry_count.saturating_add(1);
            effect.last_error = Some(error.clone());
            Ok(())
        })
    }

    pub fn apply_execution_accepted(
        &mut self,
        operation_id: &OperationId,
        accepted: ExecutionAccepted,
        now_unix_ms: u64,
    ) -> BlackopsResult<OperationRecord> {
        if &accepted.operation_id != operation_id {
            return Err(BlackopsError::Conflict(
                "fleet acceptance belongs to another operation".into(),
            ));
        }
        let prior = self.operation(operation_id)?;
        if let Some(existing) = &prior.accepted {
            if existing.attempt_id != accepted.attempt_id
                || existing.task_id != accepted.task_id
                || existing.session_id != accepted.session_id
            {
                return Err(BlackopsError::Conflict(
                    "fleet returned a different accepted identity for an operation".into(),
                ));
            }
            return Ok(prior);
        }
        if prior.status != OperationStatus::Requested || prior.execution_request.is_none() {
            return Err(BlackopsError::Conflict(format!(
                "operation {operation_id} cannot accept an execution"
            )));
        }
        self.transact(|snapshot| {
            let operation = snapshot
                .operations
                .get_mut(operation_id)
                .expect("operation checked before transaction");
            operation.status = OperationStatus::Accepted;
            operation.accepted = Some(accepted.clone());
            operation.last_attempt_state = Some(AttemptState::Accepted);
            operation.updated_at_unix_ms = now_unix_ms;
            operation.last_error = None;
            let operation = operation.clone();
            snapshot.fleet_outbox.remove(&fleet_effect_id(operation_id));
            if let Some(agent_id) = operation.kind.agent_id() {
                let agent = snapshot.agents.get_mut(agent_id).ok_or_else(|| {
                    BlackopsError::InvalidState(format!(
                        "operation {operation_id} references missing agent {agent_id}"
                    ))
                })?;
                agent.status = LogicalAgentStatus::Running;
                agent.terminal_status_observers.clear();
                agent.current_operation_id = Some(operation_id.clone());
                if agent
                    .current_session_id
                    .as_ref()
                    .is_some_and(|session_id| session_id != &accepted.session_id)
                {
                    agent.current_session_attempt_generation = None;
                }
                agent.current_attempt_id = Some(accepted.attempt_id.clone());
                agent.current_task_id = Some(accepted.task_id.clone());
                agent.current_session_id = Some(accepted.session_id.clone());
                agent.current_worker_id = None;
                agent.updated_at_unix_ms = now_unix_ms;
                queue_undelivered_mailbox(snapshot, agent_id)?;
            }
            if let OperationKind::InvokeDefinition { invocation_id } = &operation.kind {
                let invocation = snapshot.invocations.get_mut(invocation_id).ok_or_else(|| {
                    BlackopsError::InvalidState(format!(
                        "operation {operation_id} references missing invocation {invocation_id}"
                    ))
                })?;
                invocation.status = InvocationStatus::Running;
                invocation.updated_at_unix_ms = now_unix_ms;
            }
            queue_operation_record(snapshot, &operation, "accepted")?;
            Ok(operation)
        })
    }

    pub fn apply_interrupt_accepted(
        &mut self,
        operation_id: &OperationId,
        result: Value,
        now_unix_ms: u64,
    ) -> BlackopsResult<OperationRecord> {
        let prior = self.operation(operation_id)?;
        if prior.status.is_terminal() {
            return Ok(prior);
        }
        if !matches!(prior.kind, OperationKind::InterruptAgent { .. }) {
            return Err(BlackopsError::Conflict(
                "operation is not an interrupt intent".into(),
            ));
        }
        self.transact(|snapshot| {
            let operation = snapshot
                .operations
                .get_mut(operation_id)
                .expect("operation checked before transaction");
            operation.status = OperationStatus::Interrupted;
            operation.terminal = Some(OperationTerminal {
                status: OperationStatus::Interrupted,
                result: result.clone(),
                occurred_at_unix_ms: now_unix_ms,
            });
            operation.updated_at_unix_ms = now_unix_ms;
            let operation = operation.clone();
            snapshot.fleet_outbox.remove(&fleet_effect_id(operation_id));
            if let Some(agent_id) = operation.kind.agent_id() {
                let agent = snapshot
                    .agents
                    .get_mut(agent_id)
                    .expect("interrupt operation agent must exist");
                agent.status = LogicalAgentStatus::Interrupted;
                agent.terminal_status_observers.clear();
                agent.updated_at_unix_ms = now_unix_ms;
            }
            queue_operation_record(snapshot, &operation, "terminal")?;
            Ok(operation)
        })
    }

    pub fn reconciliation_targets(&self) -> Vec<ReconciliationTarget> {
        self.snapshot
            .operations
            .values()
            .filter(|operation| operation.status == OperationStatus::Accepted)
            .filter_map(|operation| {
                operation
                    .accepted
                    .as_ref()
                    .map(|accepted| ReconciliationTarget {
                        operation_id: operation.operation_id.clone(),
                        attempt_id: accepted.attempt_id.clone(),
                    })
            })
            .collect()
    }

    pub fn observe_attempt(
        &mut self,
        operation_id: &OperationId,
        outcome: AttemptOutcome,
        now_unix_ms: u64,
    ) -> BlackopsResult<OperationRecord> {
        let prior = self.operation(operation_id)?;
        let accepted = prior.accepted.as_ref().ok_or_else(|| {
            BlackopsError::Conflict("cannot reconcile an operation before acceptance".into())
        })?;
        if accepted.attempt_id != outcome.attempt_id {
            return Err(BlackopsError::Conflict(
                "attempt outcome belongs to another accepted attempt".into(),
            ));
        }
        if prior.status.is_terminal() {
            return Ok(prior);
        }
        if prior.last_attempt_state == Some(outcome.state) && !outcome_terminal(&outcome) {
            return Ok(prior);
        }
        self.transact(|snapshot| {
            let operation = snapshot
                .operations
                .get_mut(operation_id)
                .expect("operation checked before transaction");
            operation.last_attempt_state = Some(outcome.state);
            operation.updated_at_unix_ms = now_unix_ms;
            if let Some(status) = terminal_status(outcome.state) {
                operation.status = status;
                operation.terminal = Some(OperationTerminal {
                    status,
                    result: outcome.result.clone(),
                    occurred_at_unix_ms: now_unix_ms,
                });
            }
            let operation = operation.clone();
            if let Some(agent_id) = operation.kind.agent_id() {
                let agent = snapshot
                    .agents
                    .get_mut(agent_id)
                    .expect("execution operation agent must exist");
                agent.status = match outcome.state {
                    AttemptState::Accepted | AttemptState::Running => LogicalAgentStatus::Running,
                    AttemptState::Completed => LogicalAgentStatus::Completed,
                    AttemptState::Interrupted => LogicalAgentStatus::Interrupted,
                    AttemptState::Failed | AttemptState::Lost => LogicalAgentStatus::Failed,
                };
                if matches!(
                    outcome.state,
                    AttemptState::Completed
                        | AttemptState::Interrupted
                        | AttemptState::Failed
                        | AttemptState::Lost
                ) {
                    agent.terminal_status_observers.clear();
                }
                agent.updated_at_unix_ms = now_unix_ms;
            }
            if let OperationKind::InvokeDefinition { invocation_id } = &operation.kind {
                let invocation = snapshot.invocations.get_mut(invocation_id).ok_or_else(|| {
                    BlackopsError::InvalidState(format!(
                        "operation {operation_id} references missing invocation {invocation_id}"
                    ))
                })?;
                invocation.status = match outcome.state {
                    AttemptState::Accepted | AttemptState::Running => InvocationStatus::Running,
                    AttemptState::Completed => InvocationStatus::Completed,
                    AttemptState::Interrupted | AttemptState::Failed | AttemptState::Lost => {
                        InvocationStatus::Failed
                    }
                };
                invocation.updated_at_unix_ms = now_unix_ms;
            }
            if let OperationKind::RunWorkflowNode {
                invocation_id,
                node_id,
                ..
            } = &operation.kind
            {
                match outcome.state {
                    AttemptState::Completed => complete_workflow_node(
                        snapshot,
                        invocation_id,
                        node_id,
                        outcome.result.clone(),
                        now_unix_ms,
                        true,
                    )?,
                    AttemptState::Interrupted | AttemptState::Failed | AttemptState::Lost => {
                        fail_workflow_node(
                            snapshot,
                            invocation_id,
                            node_id,
                            outcome.result.clone(),
                            now_unix_ms,
                        )?;
                    }
                    AttemptState::Accepted | AttemptState::Running => {}
                }
            }
            if operation.status.is_terminal() {
                queue_operation_record(snapshot, &operation, "terminal")?;
            }
            Ok(operation)
        })
    }

    pub fn note_fleet_effect_error(
        &mut self,
        effect_id: &str,
        error: String,
        now_unix_ms: u64,
    ) -> BlackopsResult<()> {
        if !self.snapshot.fleet_outbox.contains_key(effect_id) {
            return Ok(());
        }
        self.transact(|snapshot| {
            let effect = snapshot
                .fleet_outbox
                .get_mut(effect_id)
                .expect("effect checked before transaction");
            effect.retry_count = effect.retry_count.saturating_add(1);
            effect.last_error = Some(error.clone());
            if let Some(operation) = snapshot.operations.get_mut(&effect.operation_id) {
                operation.last_error = Some(error);
                operation.updated_at_unix_ms = now_unix_ms;
            }
            Ok(())
        })
    }

    pub fn install_definition(
        &mut self,
        request: DefinitionInstallRequest,
    ) -> BlackopsResult<OperationalDefinition> {
        if request.name.trim().is_empty() || request.version.trim().is_empty() {
            return Err(BlackopsError::InvalidRequest(
                "definition name and version must not be empty".into(),
            ));
        }
        if request.kind == crate::DefinitionKind::Workflow {
            let workflow: WorkflowDefinition = serde_json::from_value(request.body.clone())
                .map_err(|error| {
                    BlackopsError::InvalidRequest(format!(
                        "workflow definition body is invalid: {error}"
                    ))
                })?;
            workflow.validate()?;
        }
        let key = DefinitionKey {
            kind: request.kind,
            name: request.name.clone(),
            version: request.version.clone(),
        };
        let content_digest = request_digest(&json!({
            "input_contract": request.input_contract,
            "body": request.body
        }))?;
        if let Some(existing) = self.snapshot.definitions.get(&key) {
            if existing.content_digest != content_digest {
                return Err(BlackopsError::Conflict(format!(
                    "definition {}:{} version {} is immutable",
                    key.kind, key.name, key.version
                )));
            }
            return Ok(existing.clone());
        }
        let definition = OperationalDefinition {
            key: key.clone(),
            input_contract: request.input_contract,
            body: request.body,
            content_digest,
            created_at_unix_ms: request.created_at_unix_ms,
        };
        self.transact(|snapshot| {
            snapshot.definitions.insert(key.clone(), definition.clone());
            if request.activate {
                snapshot.active_definitions.insert(
                    DefinitionName {
                        kind: key.kind,
                        name: key.name.clone(),
                    },
                    key.version.clone(),
                );
            }
            queue_record(
                snapshot,
                format!(
                    "blackops:definition:{}:{}:{}",
                    key.kind, key.name, key.version
                ),
                "definition.installed",
                Some(format!("{}:{}", key.kind, key.name)),
                serde_json::to_value(&definition)?,
            )?;
            Ok(definition.clone())
        })
    }

    pub fn list_definitions(&self) -> Vec<OperationalDefinition> {
        self.snapshot.definitions.values().cloned().collect()
    }

    pub fn list_invocations(&self) -> Vec<InvocationIntent> {
        self.snapshot.invocations.values().cloned().collect()
    }

    pub fn invocation(&self, invocation_id: &InvocationId) -> BlackopsResult<InvocationIntent> {
        self.snapshot
            .invocations
            .get(invocation_id)
            .cloned()
            .ok_or_else(|| BlackopsError::NotFound(format!("invocation {invocation_id}")))
    }

    pub fn request_invocation(
        &mut self,
        request: InvocationRequest,
    ) -> BlackopsResult<InvocationIntent> {
        if request.definition.kind == crate::DefinitionKind::Workflow {
            return self.request_workflow_invocation(request);
        }
        if let Some(existing) = self.snapshot.invocations.get(&request.invocation_id) {
            let requested_operation_id = request
                .execution
                .as_ref()
                .map(|execution| &execution.operation_id);
            let execution_matches = match (&existing.operation_id, &request.execution) {
                (None, None) => true,
                (Some(operation_id), Some(execution)) => {
                    self.snapshot
                        .operations
                        .get(operation_id)
                        .and_then(|operation| operation.execution_request.as_ref())
                        == Some(execution)
                }
                _ => false,
            };
            if existing.definition == request.definition
                && existing.input == request.input
                && existing.operation_id.as_ref() == requested_operation_id
                && execution_matches
            {
                return Ok(existing.clone());
            }
            return Err(BlackopsError::Conflict(format!(
                "invocation {} already exists with different intent",
                request.invocation_id
            )));
        }
        if !self.snapshot.definitions.contains_key(&request.definition) {
            return Err(BlackopsError::NotFound(format!(
                "definition {}:{}:{}",
                request.definition.kind, request.definition.name, request.definition.version
            )));
        }
        if let Some(execution) = &request.execution {
            validate_execution_identity(&execution.idempotency_key, execution)?;
            if self
                .snapshot
                .operations
                .contains_key(&execution.operation_id)
                || self
                    .snapshot
                    .request_dedup
                    .contains_key(&execution.idempotency_key)
            {
                return Err(BlackopsError::Conflict(
                    "invocation execution identity is already in use".into(),
                ));
            }
        }
        let invocation = InvocationIntent {
            invocation_id: request.invocation_id.clone(),
            definition: request.definition.clone(),
            input: request.input.clone(),
            status: InvocationStatus::Requested,
            operation_id: request
                .execution
                .as_ref()
                .map(|execution| execution.operation_id.clone()),
            output: None,
            requested_at_unix_ms: request.requested_at_unix_ms,
            updated_at_unix_ms: request.requested_at_unix_ms,
        };
        self.transact(|snapshot| {
            snapshot
                .invocations
                .insert(request.invocation_id.clone(), invocation.clone());
            if let Some(execution) = &request.execution {
                let digest = request_digest(&request)?;
                let operation = execution_operation(
                    execution.clone(),
                    digest,
                    OperationKind::InvokeDefinition {
                        invocation_id: request.invocation_id.clone(),
                    },
                    request.requested_at_unix_ms,
                );
                install_execution_operation(snapshot, operation.clone(), None, execution.clone());
                queue_operation_record(snapshot, &operation, "requested")?;
            }
            queue_record(
                snapshot,
                format!("blackops:invocation:{}:requested", request.invocation_id),
                "invocation.requested",
                Some(request.invocation_id.to_string()),
                serde_json::to_value(&invocation)?,
            )?;
            Ok(invocation.clone())
        })
    }

    /// Commit a no-fleet definition invocation and its output in one durable
    /// authority transaction. Retries with the same exact intent and output
    /// are idempotent; an identity collision fails closed.
    pub fn complete_local_invocation(
        &mut self,
        request: InvocationRequest,
        output: Value,
    ) -> BlackopsResult<InvocationIntent> {
        if request.definition.kind != crate::DefinitionKind::Atom || request.execution.is_some() {
            return Err(BlackopsError::InvalidRequest(
                "local completion requires an atom invocation without a fleet execution".into(),
            ));
        }
        if let Some(existing) = self.snapshot.invocations.get(&request.invocation_id) {
            if existing.definition == request.definition
                && existing.input == request.input
                && existing.status == InvocationStatus::Completed
                && existing.operation_id.is_none()
                && existing.output.as_ref() == Some(&output)
            {
                return Ok(existing.clone());
            }
            return Err(BlackopsError::Conflict(format!(
                "invocation {} already exists with different local intent",
                request.invocation_id
            )));
        }
        if !self.snapshot.definitions.contains_key(&request.definition) {
            return Err(BlackopsError::NotFound(format!(
                "definition {}:{}:{}",
                request.definition.kind, request.definition.name, request.definition.version
            )));
        }
        let invocation = InvocationIntent {
            invocation_id: request.invocation_id,
            definition: request.definition,
            input: request.input,
            status: InvocationStatus::Completed,
            operation_id: None,
            output: Some(output),
            requested_at_unix_ms: request.requested_at_unix_ms,
            updated_at_unix_ms: request.requested_at_unix_ms,
        };
        self.transact(|snapshot| {
            snapshot
                .invocations
                .insert(invocation.invocation_id.clone(), invocation.clone());
            queue_record(
                snapshot,
                format!("blackops:invocation:{}:completed", invocation.invocation_id),
                "invocation.completed",
                Some(invocation.invocation_id.to_string()),
                serde_json::to_value(&invocation)?,
            )?;
            Ok(invocation.clone())
        })
    }

    fn request_workflow_invocation(
        &mut self,
        request: InvocationRequest,
    ) -> BlackopsResult<InvocationIntent> {
        if request.execution.is_some() {
            return Err(BlackopsError::InvalidRequest(
                "workflow invocation execution is defined by versioned nodes".into(),
            ));
        }
        if let Some(existing) = self.snapshot.invocations.get(&request.invocation_id) {
            if existing.definition == request.definition
                && existing.input == request.input
                && self
                    .snapshot
                    .workflow_runs
                    .contains_key(&request.invocation_id)
            {
                return Ok(existing.clone());
            }
            return Err(BlackopsError::Conflict(format!(
                "workflow invocation {} already exists with different intent",
                request.invocation_id
            )));
        }
        let definition = self
            .snapshot
            .definitions
            .get(&request.definition)
            .ok_or_else(|| BlackopsError::NotFound("workflow definition does not exist".into()))?;
        let workflow = parse_workflow_definition(definition)?;
        let invocation = InvocationIntent {
            invocation_id: request.invocation_id.clone(),
            definition: request.definition.clone(),
            input: request.input.clone(),
            status: InvocationStatus::Running,
            operation_id: None,
            output: None,
            requested_at_unix_ms: request.requested_at_unix_ms,
            updated_at_unix_ms: request.requested_at_unix_ms,
        };
        let run = WorkflowRun {
            invocation_id: request.invocation_id.clone(),
            definition: request.definition.clone(),
            input: request.input,
            status: WorkflowRunStatus::Running,
            current_node: workflow.start,
            nodes: BTreeMap::new(),
            outputs: BTreeMap::new(),
            waiting_on: None,
            integration_intent_ids: Vec::new(),
            created_at_unix_ms: request.requested_at_unix_ms,
            updated_at_unix_ms: request.requested_at_unix_ms,
        };
        self.transact(|snapshot| {
            snapshot
                .invocations
                .insert(invocation.invocation_id.clone(), invocation.clone());
            snapshot
                .workflow_runs
                .insert(run.invocation_id.clone(), run.clone());
            queue_record(
                snapshot,
                format!("blackops:invocation:{}:requested", invocation.invocation_id),
                "invocation.requested",
                Some(invocation.invocation_id.to_string()),
                serde_json::to_value(&invocation)?,
            )?;
            advance_workflow(
                snapshot,
                &invocation.invocation_id,
                request.requested_at_unix_ms,
            )?;
            Ok(snapshot.invocations[&invocation.invocation_id].clone())
        })
    }

    pub fn workflow_run(&self, invocation_id: &InvocationId) -> BlackopsResult<WorkflowRun> {
        self.snapshot
            .workflow_runs
            .get(invocation_id)
            .cloned()
            .ok_or_else(|| BlackopsError::NotFound(format!("workflow run {invocation_id}")))
    }

    pub fn list_workflow_runs(&self) -> Vec<WorkflowRun> {
        self.snapshot.workflow_runs.values().cloned().collect()
    }

    pub fn trigger_due_workflow_retries(
        &mut self,
        now_unix_ms: u64,
        limit: usize,
    ) -> BlackopsResult<usize> {
        if limit == 0 || limit > 64 {
            return Err(BlackopsError::InvalidRequest(
                "workflow retry limit must be between 1 and 64".into(),
            ));
        }
        let due: Vec<_> = self
            .snapshot
            .workflow_runs
            .values()
            .filter(|run| run.status == WorkflowRunStatus::RetryScheduled)
            .filter_map(|run| {
                run.nodes
                    .get(&run.current_node)
                    .filter(|node| {
                        node.status == WorkflowNodeStatus::RetryScheduled
                            && node
                                .next_attempt_unix_ms
                                .is_some_and(|due| due <= now_unix_ms)
                    })
                    .map(|_| run.invocation_id.clone())
            })
            .take(limit)
            .collect();
        if due.is_empty() {
            return Ok(0);
        }
        self.transact(|snapshot| {
            for invocation_id in &due {
                advance_workflow(snapshot, invocation_id, now_unix_ms)?;
            }
            Ok(due.len())
        })
    }

    pub fn list_integration_intents(&self) -> Vec<IntegrationIntent> {
        self.snapshot
            .integration_intents
            .values()
            .cloned()
            .collect()
    }

    pub fn resolve_integration_intent(
        &mut self,
        intent_id: &IntegrationIntentId,
        request: IntegrationIntentResolveRequest,
    ) -> BlackopsResult<IntegrationIntent> {
        if request.status == IntegrationIntentStatus::Requested {
            return Err(BlackopsError::InvalidRequest(
                "integration intent resolution must be terminal".into(),
            ));
        }
        let prior = self
            .snapshot
            .integration_intents
            .get(intent_id)
            .ok_or_else(|| BlackopsError::NotFound(format!("integration intent {intent_id}")))?;
        if prior.status != IntegrationIntentStatus::Requested {
            if prior.status == request.status && prior.result.as_ref() == Some(&request.result) {
                return Ok(prior.clone());
            }
            return Err(BlackopsError::Conflict(format!(
                "integration intent {intent_id} is already terminal"
            )));
        }
        let intent_id = intent_id.clone();
        self.transact(|snapshot| {
            let intent = snapshot
                .integration_intents
                .get_mut(&intent_id)
                .expect("integration intent checked");
            intent.status = request.status;
            intent.result = Some(request.result.clone());
            intent.updated_at_unix_ms = request.resolved_at_unix_ms;
            let intent = intent.clone();
            queue_record(
                snapshot,
                format!("blackops:integration:{intent_id}:terminal"),
                "integration.terminal",
                Some(intent_id.to_string()),
                serde_json::to_value(&intent)?,
            )?;
            Ok(intent)
        })
    }

    pub fn put_schedule(&mut self, mut schedule: ScheduleIntent) -> BlackopsResult<ScheduleIntent> {
        if schedule.name.trim().is_empty() {
            return Err(BlackopsError::InvalidRequest(
                "schedule name must not be empty".into(),
            ));
        }
        if !self
            .snapshot
            .definitions
            .contains_key(&schedule.invocation.definition)
        {
            return Err(BlackopsError::NotFound(
                "schedule definition does not exist".into(),
            ));
        }
        if schedule.invocation.definition.kind == crate::DefinitionKind::Atom
            && schedule.invocation.execution.is_none()
        {
            return Err(BlackopsError::InvalidRequest(
                "atom schedule invocation requires an execution template".into(),
            ));
        }
        if schedule.invocation.definition.kind == crate::DefinitionKind::Workflow
            && schedule.invocation.execution.is_some()
        {
            return Err(BlackopsError::InvalidRequest(
                "workflow schedule execution is defined by versioned nodes".into(),
            ));
        }
        match &schedule.trigger {
            ScheduleTrigger::At { unix_ms } => {
                schedule.next_due_unix_ms.get_or_insert(*unix_ms);
            }
            ScheduleTrigger::Interval {
                every_ms,
                anchor_unix_ms,
            } => {
                if *every_ms == 0 {
                    return Err(BlackopsError::InvalidRequest(
                        "interval cadence must be nonzero".into(),
                    ));
                }
                schedule.next_due_unix_ms.get_or_insert(*anchor_unix_ms);
            }
            ScheduleTrigger::Cron { expression } => {
                Schedule::from_str(expression).map_err(|error| {
                    BlackopsError::InvalidRequest(format!("invalid cron expression: {error}"))
                })?;
                if schedule.next_due_unix_ms.is_none() {
                    schedule.next_due_unix_ms =
                        next_cron_due(expression, schedule.updated_at_unix_ms)?;
                }
            }
            ScheduleTrigger::Webhook { path } => {
                if !path.starts_with('/') || path.contains("..") {
                    return Err(BlackopsError::InvalidRequest(
                        "webhook path must be absolute and must not contain parent traversal"
                            .into(),
                    ));
                }
                schedule.next_due_unix_ms = None;
            }
            ScheduleTrigger::Poll { every_ms, source } => {
                if *every_ms == 0 {
                    return Err(BlackopsError::InvalidRequest(
                        "poll cadence must be nonzero".into(),
                    ));
                }
                validate_poll_source(source)?;
                schedule
                    .next_due_unix_ms
                    .get_or_insert(schedule.updated_at_unix_ms.saturating_add(*every_ms));
            }
        }
        if let Some(existing) = self.snapshot.schedules.get(&schedule.schedule_id) {
            if existing == &schedule {
                return Ok(existing.clone());
            }
            schedule.created_at_unix_ms = existing.created_at_unix_ms;
            schedule.generation = existing.generation.saturating_add(1);
        } else if schedule.generation == 0 {
            schedule.generation = 1;
        }
        let schedule_id = schedule.schedule_id.clone();
        self.transact(|snapshot| {
            snapshot
                .schedules
                .insert(schedule_id.clone(), schedule.clone());
            queue_record(
                snapshot,
                format!(
                    "blackops:schedule:{schedule_id}:generation:{}",
                    schedule.generation
                ),
                "schedule.intent",
                Some(schedule_id.to_string()),
                serde_json::to_value(&schedule)?,
            )?;
            Ok(schedule.clone())
        })
    }

    pub fn list_schedules(&self) -> Vec<ScheduleIntent> {
        self.snapshot.schedules.values().cloned().collect()
    }

    pub fn due_schedules(&self, now_unix_ms: u64) -> Vec<ScheduleIntent> {
        self.snapshot
            .schedules
            .values()
            .filter(|schedule| {
                schedule.enabled
                    && schedule
                        .next_due_unix_ms
                        .is_some_and(|due| due <= now_unix_ms)
            })
            .cloned()
            .collect()
    }

    pub fn due_poll_schedules(
        &self,
        now_unix_ms: u64,
        limit: usize,
    ) -> BlackopsResult<Vec<ScheduleIntent>> {
        if limit == 0 || limit > 16 {
            return Err(BlackopsError::InvalidRequest(
                "poll schedule limit must be between 1 and 16".into(),
            ));
        }
        Ok(self
            .snapshot
            .schedules
            .values()
            .filter(|schedule| {
                schedule.enabled
                    && matches!(&schedule.trigger, ScheduleTrigger::Poll { .. })
                    && schedule
                        .next_due_unix_ms
                        .is_some_and(|due| due <= now_unix_ms)
            })
            .take(limit)
            .cloned()
            .collect())
    }

    pub fn admit_poll_response(
        &mut self,
        schedule_id: &crate::ScheduleId,
        generation: u64,
        expected_due_at_unix_ms: u64,
        response: Value,
        now_unix_ms: u64,
    ) -> BlackopsResult<Vec<InvocationIntent>> {
        let schedule = self
            .snapshot
            .schedules
            .get(schedule_id)
            .cloned()
            .ok_or_else(|| BlackopsError::NotFound(format!("schedule {schedule_id}")))?;
        if !schedule.enabled || schedule.generation != generation {
            return Err(BlackopsError::Conflict(format!(
                "poll schedule {schedule_id} generation changed"
            )));
        }
        let (every_ms, source) = match &schedule.trigger {
            ScheduleTrigger::Poll { every_ms, source } => (*every_ms, source.clone()),
            _ => {
                return Err(BlackopsError::InvalidRequest(
                    "schedule is not a poll source".into(),
                ));
            }
        };
        let due_at = schedule.next_due_unix_ms.ok_or_else(|| {
            BlackopsError::InvalidState("enabled poll schedule has no next due time".into())
        })?;
        // The due timestamp is the fetch occurrence token. Concurrent or delayed admissions
        // cannot advance a newer occurrence even when the generation is unchanged.
        if due_at != expected_due_at_unix_ms {
            return Err(BlackopsError::Conflict(format!(
                "poll schedule {schedule_id} due occurrence changed"
            )));
        }
        if due_at > now_unix_ms {
            return Err(BlackopsError::Conflict(format!(
                "poll schedule {schedule_id} is no longer due"
            )));
        }
        let selected = match source.items_pointer.as_deref() {
            Some(pointer) => response.pointer(pointer).cloned().ok_or_else(|| {
                BlackopsError::InvalidRequest(format!(
                    "poll items pointer {pointer} did not resolve"
                ))
            })?,
            None => response,
        };
        let items = match selected {
            Value::Array(items) => items,
            Value::Null => Vec::new(),
            item => vec![item],
        };
        if items.len() > source.max_items {
            return Err(BlackopsError::InvalidRequest(format!(
                "poll response contains {} items, exceeding configured max {}",
                items.len(),
                source.max_items
            )));
        }
        let mut deliveries = Vec::new();
        for item in items {
            let source_identity = match source.delivery_id_pointer.as_deref() {
                Some(pointer) => poll_identity_value(item.pointer(pointer).ok_or_else(|| {
                    BlackopsError::InvalidRequest(format!(
                        "poll delivery pointer {pointer} did not resolve"
                    ))
                })?)?,
                None => request_digest(&item)?,
            };
            let delivery_id = request_digest(&json!({
                "schedule_id": schedule_id,
                "generation": generation,
                "source_identity": source_identity,
            }))?;
            let payload_digest = request_digest(&item)?;
            deliveries.push((delivery_id, payload_digest, item));
        }
        let schedule_id = schedule_id.clone();
        self.transact(|snapshot| {
            let sequence = snapshot
                .poll_cursors
                .get(&schedule_id)
                .filter(|cursor| cursor.schedule_generation == generation)
                .map_or(1, |cursor| cursor.fetch_sequence.saturating_add(1));
            let cursor = snapshot
                .poll_cursors
                .entry(schedule_id.clone())
                .or_insert_with(|| PollSourceCursor {
                    schedule_id: schedule_id.clone(),
                    schedule_generation: generation,
                    fetch_sequence: 0,
                    deliveries: BTreeMap::new(),
                    updated_at_unix_ms: now_unix_ms,
                });
            if cursor.schedule_generation != generation {
                *cursor = PollSourceCursor {
                    schedule_id: schedule_id.clone(),
                    schedule_generation: generation,
                    fetch_sequence: 0,
                    deliveries: BTreeMap::new(),
                    updated_at_unix_ms: now_unix_ms,
                };
            }
            let mut fresh = Vec::new();
            for (delivery_id, payload_digest, item) in &deliveries {
                if let Some(prior) = cursor.deliveries.get(delivery_id) {
                    if prior.payload_digest != *payload_digest {
                        return Err(BlackopsError::Conflict(format!(
                            "poll delivery {delivery_id} changed payload"
                        )));
                    }
                    continue;
                }
                cursor.deliveries.insert(
                    delivery_id.clone(),
                    PollDeliveryRecord {
                        delivery_id: delivery_id.clone(),
                        payload_digest: payload_digest.clone(),
                        observed_sequence: sequence,
                        observed_at_unix_ms: now_unix_ms,
                    },
                );
                fresh.push((delivery_id.clone(), item.clone()));
            }
            cursor.fetch_sequence = sequence;
            cursor.updated_at_unix_ms = now_unix_ms;
            while cursor.deliveries.len() > 4_096 {
                let oldest = cursor
                    .deliveries
                    .values()
                    .min_by_key(|delivery| delivery.observed_sequence)
                    .map(|delivery| delivery.delivery_id.clone())
                    .expect("nonempty delivery cursor");
                cursor.deliveries.remove(&oldest);
            }
            let next_due = next_due_after_fire(&schedule, due_at, now_unix_ms)?
                .unwrap_or_else(|| now_unix_ms.saturating_add(every_ms));
            let durable_schedule = snapshot
                .schedules
                .get_mut(&schedule_id)
                .expect("poll schedule checked");
            durable_schedule.next_due_unix_ms = Some(next_due);
            durable_schedule.updated_at_unix_ms = now_unix_ms;

            let mut invocations = Vec::new();
            for (delivery_id, item) in fresh {
                let occurrence = format!("poll-{delivery_id}");
                let input = json!({
                    "configured": schedule.invocation.input,
                    "poll": item,
                    "delivery_id": delivery_id,
                    "fetch_sequence": sequence,
                });
                let invocation = install_scheduled_invocation(
                    snapshot,
                    &schedule,
                    &occurrence,
                    input,
                    now_unix_ms,
                )?;
                queue_record(
                    snapshot,
                    format!("blackops:poll:{schedule_id}:delivery:{delivery_id}"),
                    "poll.delivery.admitted",
                    Some(schedule_id.to_string()),
                    json!({
                        "schedule_id": schedule_id,
                        "generation": generation,
                        "delivery_id": delivery_id,
                        "invocation_id": invocation.invocation_id,
                        "fetch_sequence": sequence,
                    }),
                )?;
                invocations.push(invocation);
            }
            Ok(invocations)
        })
    }

    pub fn trigger_due_schedules(
        &mut self,
        now_unix_ms: u64,
        limit: usize,
    ) -> BlackopsResult<Vec<InvocationIntent>> {
        if limit == 0 || limit > 64 {
            return Err(BlackopsError::InvalidRequest(
                "schedule trigger limit must be between 1 and 64".into(),
            ));
        }
        let due: Vec<_> = self
            .snapshot
            .schedules
            .values()
            .filter_map(|schedule| {
                (schedule.enabled && !matches!(&schedule.trigger, ScheduleTrigger::Poll { .. }))
                    .then_some(schedule.next_due_unix_ms)
                    .flatten()
                    .filter(|due| *due <= now_unix_ms)
                    .map(|due| (schedule.schedule_id.clone(), due))
            })
            .take(limit)
            .collect();
        if due.is_empty() {
            return Ok(Vec::new());
        }
        self.transact(|snapshot| {
            let mut invocations = Vec::new();
            for (schedule_id, due_at) in due {
                let schedule = snapshot
                    .schedules
                    .get(&schedule_id)
                    .cloned()
                    .ok_or_else(|| {
                        BlackopsError::InvalidState(format!(
                            "due schedule {schedule_id} disappeared"
                        ))
                    })?;
                if !schedule.enabled || schedule.next_due_unix_ms != Some(due_at) {
                    continue;
                }
                let input = schedule.invocation.input.clone();
                let invocation = install_scheduled_invocation(
                    snapshot,
                    &schedule,
                    &format!("due-{due_at}"),
                    input,
                    now_unix_ms,
                )?;
                let next_due = next_due_after_fire(&schedule, due_at, now_unix_ms)?;
                let durable = snapshot
                    .schedules
                    .get_mut(&schedule_id)
                    .expect("schedule cloned from this snapshot");
                durable.next_due_unix_ms = next_due;
                durable.updated_at_unix_ms = now_unix_ms;
                if matches!(durable.trigger, ScheduleTrigger::At { .. }) {
                    durable.enabled = false;
                }
                invocations.push(invocation);
            }
            Ok(invocations)
        })
    }

    pub fn admit_webhook(
        &mut self,
        path: &str,
        request: WebhookAdmissionRequest,
    ) -> BlackopsResult<Vec<InvocationIntent>> {
        if request.delivery_id.trim().is_empty() {
            return Err(BlackopsError::InvalidRequest(
                "webhook delivery ID must not be empty".into(),
            ));
        }
        let schedules: Vec<_> = self
            .snapshot
            .schedules
            .values()
            .filter(|schedule| {
                schedule.enabled
                    && matches!(
                        &schedule.trigger,
                        ScheduleTrigger::Webhook { path: expected } if expected == path
                    )
            })
            .cloned()
            .collect();
        if schedules.len() > 64 {
            return Err(BlackopsError::Conflict(
                "webhook matches more than the bounded 64 schedules".into(),
            ));
        }
        if schedules.is_empty() {
            return Ok(Vec::new());
        }
        self.transact(|snapshot| {
            let mut invocations = Vec::new();
            for schedule in &schedules {
                let digest = request_digest(&json!({
                    "schedule_id": schedule.schedule_id.clone(),
                    "generation": schedule.generation,
                    "delivery_id": request.delivery_id.clone()
                }))?;
                let occurrence = format!("webhook-{}", &digest[7..31]);
                let input = json!({
                    "configured": schedule.invocation.input.clone(),
                    "webhook": request.payload.clone(),
                    "delivery_id": request.delivery_id.clone()
                });
                invocations.push(install_scheduled_invocation(
                    snapshot,
                    schedule,
                    &occurrence,
                    input,
                    request.occurred_at_unix_ms,
                )?);
            }
            Ok(invocations)
        })
    }

    pub fn create_wait(&mut self, request: WaitCreateRequest) -> BlackopsResult<OperationalWait> {
        if request.topic.trim().is_empty() {
            return Err(BlackopsError::InvalidRequest(
                "wait topic must not be empty".into(),
            ));
        }
        let wait = OperationalWait {
            wait_id: request.wait_id.clone(),
            topic: request.topic,
            selector: request.selector,
            status: WaitStatus::Pending,
            deadline_unix_ms: request.deadline_unix_ms,
            resolution: None,
            created_at_unix_ms: request.created_at_unix_ms,
            updated_at_unix_ms: request.created_at_unix_ms,
        };
        if let Some(existing) = self.snapshot.waits.get(&request.wait_id) {
            if existing == &wait {
                return Ok(existing.clone());
            }
            return Err(BlackopsError::Conflict(format!(
                "wait {} already exists with different intent",
                request.wait_id
            )));
        }
        self.transact(|snapshot| {
            snapshot.waits.insert(wait.wait_id.clone(), wait.clone());
            queue_record(
                snapshot,
                format!("blackops:wait:{}:created", wait.wait_id),
                "wait.created",
                Some(wait.wait_id.to_string()),
                serde_json::to_value(&wait)?,
            )?;
            Ok(wait.clone())
        })
    }

    pub fn resolve_wait(
        &mut self,
        wait_id: &WaitId,
        request: WaitResolveRequest,
    ) -> BlackopsResult<OperationalWait> {
        if request.status == WaitStatus::Pending {
            return Err(BlackopsError::InvalidRequest(
                "wait resolution must be terminal".into(),
            ));
        }
        let prior = self
            .snapshot
            .waits
            .get(wait_id)
            .ok_or_else(|| BlackopsError::NotFound(format!("wait {wait_id}")))?;
        if prior.status != WaitStatus::Pending {
            if prior.status == request.status
                && prior.resolution.as_ref() == Some(&request.resolution)
            {
                return Ok(prior.clone());
            }
            return Err(BlackopsError::Conflict(format!(
                "wait {wait_id} is already terminal"
            )));
        }
        let wait_id = wait_id.clone();
        self.transact(|snapshot| {
            let wait = snapshot.waits.get_mut(&wait_id).expect("wait checked");
            wait.status = request.status;
            wait.resolution = Some(request.resolution.clone());
            wait.updated_at_unix_ms = request.resolved_at_unix_ms;
            let wait = wait.clone();
            queue_record(
                snapshot,
                format!("blackops:wait:{wait_id}:terminal"),
                "wait.terminal",
                Some(wait_id.to_string()),
                serde_json::to_value(&wait)?,
            )?;
            resume_workflow_wait(
                snapshot,
                &wait_id,
                request.status,
                request.resolution.clone(),
                request.resolved_at_unix_ms,
            )?;
            Ok(wait)
        })
    }

    pub fn expire_waits(&mut self, now_unix_ms: u64, limit: usize) -> BlackopsResult<usize> {
        let expired: Vec<_> = self
            .snapshot
            .waits
            .values()
            .filter(|wait| {
                wait.status == WaitStatus::Pending
                    && wait
                        .deadline_unix_ms
                        .is_some_and(|deadline| deadline <= now_unix_ms)
            })
            .map(|wait| wait.wait_id.clone())
            .take(limit.min(256))
            .collect();
        if expired.is_empty() {
            return Ok(0);
        }
        self.transact(|snapshot| {
            for wait_id in &expired {
                let wait = snapshot.waits.get_mut(wait_id).expect("wait selected");
                wait.status = WaitStatus::TimedOut;
                wait.resolution = Some(json!({"reason": "deadline_elapsed"}));
                wait.updated_at_unix_ms = now_unix_ms;
                let wait = wait.clone();
                queue_record(
                    snapshot,
                    format!("blackops:wait:{wait_id}:terminal"),
                    "wait.terminal",
                    Some(wait_id.to_string()),
                    serde_json::to_value(wait)?,
                )?;
                resume_workflow_wait(
                    snapshot,
                    wait_id,
                    WaitStatus::TimedOut,
                    json!({"reason": "deadline_elapsed"}),
                    now_unix_ms,
                )?;
            }
            Ok(expired.len())
        })
    }

    pub fn list_waits(&self) -> Vec<OperationalWait> {
        self.snapshot.waits.values().cloned().collect()
    }

    pub fn request_approval(
        &mut self,
        request: ApprovalCreateRequest,
    ) -> BlackopsResult<ApprovalRecord> {
        if request.requested_by.trim().is_empty() {
            return Err(BlackopsError::InvalidRequest(
                "approval requester must not be empty".into(),
            ));
        }
        let approval = ApprovalRecord {
            approval_id: request.approval_id.clone(),
            requested_by: request.requested_by,
            action: request.action,
            status: ApprovalStatus::Pending,
            decision: None,
            created_at_unix_ms: request.created_at_unix_ms,
            resolved_at_unix_ms: None,
        };
        if let Some(existing) = self.snapshot.approvals.get(&approval.approval_id) {
            if existing == &approval {
                return Ok(existing.clone());
            }
            return Err(BlackopsError::Conflict(format!(
                "approval {} already exists with different intent",
                approval.approval_id
            )));
        }
        self.transact(|snapshot| {
            snapshot
                .approvals
                .insert(approval.approval_id.clone(), approval.clone());
            queue_record(
                snapshot,
                format!("blackops:approval:{}:requested", approval.approval_id),
                "approval.requested",
                Some(approval.approval_id.to_string()),
                serde_json::to_value(&approval)?,
            )?;
            Ok(approval.clone())
        })
    }

    pub fn resolve_approval(
        &mut self,
        approval_id: &ApprovalId,
        request: ApprovalResolveRequest,
    ) -> BlackopsResult<ApprovalRecord> {
        if request.status == ApprovalStatus::Pending {
            return Err(BlackopsError::InvalidRequest(
                "approval resolution must be terminal".into(),
            ));
        }
        let prior = self
            .snapshot
            .approvals
            .get(approval_id)
            .ok_or_else(|| BlackopsError::NotFound(format!("approval {approval_id}")))?;
        if prior.status != ApprovalStatus::Pending {
            if prior.status == request.status && prior.decision.as_ref() == Some(&request.decision)
            {
                return Ok(prior.clone());
            }
            return Err(BlackopsError::Conflict(format!(
                "approval {approval_id} is already terminal"
            )));
        }
        let approval_id = approval_id.clone();
        self.transact(|snapshot| {
            let approval = snapshot
                .approvals
                .get_mut(&approval_id)
                .expect("approval checked");
            approval.status = request.status;
            approval.decision = Some(request.decision.clone());
            approval.resolved_at_unix_ms = Some(request.resolved_at_unix_ms);
            let approval = approval.clone();
            queue_record(
                snapshot,
                format!("blackops:approval:{approval_id}:terminal"),
                "approval.terminal",
                Some(approval_id.to_string()),
                serde_json::to_value(&approval)?,
            )?;
            Ok(approval)
        })
    }

    pub fn list_approvals(&self) -> Vec<ApprovalRecord> {
        self.snapshot.approvals.values().cloned().collect()
    }

    pub fn create_whiteboard(
        &mut self,
        request: WhiteboardCreateRequest,
    ) -> BlackopsResult<WhiteboardRecord> {
        if request.name.trim().is_empty() {
            return Err(BlackopsError::InvalidRequest(
                "whiteboard name must not be empty".into(),
            ));
        }
        let board = WhiteboardRecord {
            whiteboard_id: request.whiteboard_id.clone(),
            name: request.name,
            generation: 1,
            entries: BTreeMap::new(),
            created_at_unix_ms: request.created_at_unix_ms,
            updated_at_unix_ms: request.created_at_unix_ms,
        };
        if let Some(existing) = self.snapshot.whiteboards.get(&board.whiteboard_id) {
            if existing.name == board.name {
                return Ok(existing.clone());
            }
            return Err(BlackopsError::Conflict(format!(
                "whiteboard {} already exists with another name",
                board.whiteboard_id
            )));
        }
        self.transact(|snapshot| {
            snapshot
                .whiteboards
                .insert(board.whiteboard_id.clone(), board.clone());
            queue_record(
                snapshot,
                format!("blackops:whiteboard:{}:created", board.whiteboard_id),
                "whiteboard.created",
                Some(board.whiteboard_id.to_string()),
                serde_json::to_value(&board)?,
            )?;
            Ok(board.clone())
        })
    }

    pub fn put_whiteboard_entry(
        &mut self,
        board_id: &WhiteboardId,
        key: &str,
        request: WhiteboardPutRequest,
    ) -> BlackopsResult<WhiteboardRecord> {
        if key.trim().is_empty() || key.len() > 256 {
            return Err(BlackopsError::InvalidRequest(
                "whiteboard entry key must contain 1 to 256 bytes".into(),
            ));
        }
        let board = self
            .snapshot
            .whiteboards
            .get(board_id)
            .ok_or_else(|| BlackopsError::NotFound(format!("whiteboard {board_id}")))?;
        let found_revision = board.entries.get(key).map(|entry| entry.revision);
        if request.expected_revision != found_revision {
            return Err(BlackopsError::Conflict(format!(
                "whiteboard entry revision changed: expected {:?}, found {:?}",
                request.expected_revision, found_revision
            )));
        }
        let board_id = board_id.clone();
        let key = key.to_string();
        self.transact(|snapshot| {
            let board = snapshot
                .whiteboards
                .get_mut(&board_id)
                .expect("whiteboard checked");
            let revision = board
                .entries
                .get(&key)
                .map_or(1, |entry| entry.revision.saturating_add(1));
            board.entries.insert(
                key.clone(),
                WhiteboardEntry {
                    key: key.clone(),
                    value: request.value.clone(),
                    revision,
                    updated_by: request.updated_by.clone(),
                    updated_at_unix_ms: request.updated_at_unix_ms,
                },
            );
            board.generation = board.generation.saturating_add(1);
            board.updated_at_unix_ms = request.updated_at_unix_ms;
            let board = board.clone();
            queue_record(
                snapshot,
                format!("blackops:whiteboard:{board_id}:entry:{key}:revision:{revision}"),
                "whiteboard.entry",
                Some(board_id.to_string()),
                serde_json::to_value(&board.entries[&key])?,
            )?;
            Ok(board)
        })
    }

    pub fn list_whiteboards(&self) -> Vec<WhiteboardRecord> {
        self.snapshot.whiteboards.values().cloned().collect()
    }

    pub fn whiteboard(&self, board_id: &WhiteboardId) -> BlackopsResult<WhiteboardRecord> {
        self.snapshot
            .whiteboards
            .get(board_id)
            .cloned()
            .ok_or_else(|| BlackopsError::NotFound(format!("whiteboard {board_id}")))
    }

    pub fn pending_records(&self, limit: usize) -> Vec<RecordEnvelope> {
        let mut records: Vec<_> = self.snapshot.record_outbox.values().cloned().collect();
        records.sort_by_key(|record| record.cursor.parse::<u64>().unwrap_or(u64::MAX));
        records.truncate(limit);
        records
    }

    pub fn acknowledge_records(&mut self, record_ids: &[String]) -> BlackopsResult<usize> {
        let existing = record_ids
            .iter()
            .filter(|record_id| self.snapshot.record_outbox.contains_key(*record_id))
            .count();
        if existing == 0 {
            return Ok(0);
        }
        self.transact(|snapshot| {
            for record_id in record_ids {
                snapshot.record_outbox.remove(record_id);
            }
            Ok(existing)
        })
    }

    fn deduplicated_request(
        &self,
        idempotency_key: &str,
        digest: &str,
    ) -> BlackopsResult<Option<RequestDedupRecord>> {
        let Some(prior) = self.snapshot.request_dedup.get(idempotency_key) else {
            return Ok(None);
        };
        if prior.request_digest != digest {
            return Err(BlackopsError::Conflict(
                "operation idempotency key was reused with a different request".into(),
            ));
        }
        Ok(Some(prior.clone()))
    }

    fn transact<T>(
        &mut self,
        mutate: impl FnOnce(&mut OperationalSnapshot) -> BlackopsResult<T>,
    ) -> BlackopsResult<T> {
        let expected_generation = self.snapshot.generation;
        let mut next = self.snapshot.clone();
        let value = mutate(&mut next)?;
        next.generation = expected_generation.saturating_add(1);
        next.validate()?;
        self.repository
            .compare_and_swap(expected_generation, &next)?;
        self.snapshot = next;
        Ok(value)
    }
}

fn parse_workflow_definition(
    definition: &OperationalDefinition,
) -> BlackopsResult<WorkflowDefinition> {
    if definition.key.kind != crate::DefinitionKind::Workflow {
        return Err(BlackopsError::InvalidRequest(
            "definition is not a workflow".into(),
        ));
    }
    let workflow: WorkflowDefinition =
        serde_json::from_value(definition.body.clone()).map_err(|error| {
            BlackopsError::InvalidState(format!("stored workflow is invalid: {error}"))
        })?;
    workflow.validate()?;
    Ok(workflow)
}

fn advance_workflow(
    snapshot: &mut OperationalSnapshot,
    invocation_id: &InvocationId,
    now_unix_ms: u64,
) -> BlackopsResult<()> {
    for _ in 0..=256 {
        let run = snapshot
            .workflow_runs
            .get(invocation_id)
            .cloned()
            .ok_or_else(|| {
                BlackopsError::InvalidState(format!("missing workflow {invocation_id}"))
            })?;
        if matches!(
            run.status,
            WorkflowRunStatus::Completed | WorkflowRunStatus::Failed
        ) {
            return Ok(());
        }
        let definition = snapshot
            .definitions
            .get(&run.definition)
            .cloned()
            .ok_or_else(|| BlackopsError::InvalidState("workflow definition disappeared".into()))?;
        let workflow = parse_workflow_definition(&definition)?;
        let node_id = run.current_node.clone();
        let node = workflow.nodes.get(&node_id).cloned().ok_or_else(|| {
            BlackopsError::InvalidState(format!("workflow node {node_id} disappeared"))
        })?;
        let state = run
            .nodes
            .get(&node_id)
            .cloned()
            .unwrap_or(WorkflowNodeState {
                node_id: node_id.clone(),
                generation: 0,
                status: WorkflowNodeStatus::Pending,
                operation_id: None,
                wait_id: None,
                next_attempt_unix_ms: None,
                output: None,
                last_error: None,
                updated_at_unix_ms: now_unix_ms,
            });
        if matches!(
            state.status,
            WorkflowNodeStatus::Running | WorkflowNodeStatus::Waiting
        ) {
            return Ok(());
        }
        if state.status == WorkflowNodeStatus::RetryScheduled
            && state
                .next_attempt_unix_ms
                .is_some_and(|due| due > now_unix_ms)
        {
            return Ok(());
        }
        match &node.kind {
            WorkflowNodeKind::Execution { execution, input } => {
                // A node keeps one durable operation identity across retries. The
                // generation scopes the concrete fleet request and immutable records.
                let operation_identity = request_digest(&json!({
                    "invocation_id": invocation_id,
                    "node_id": node_id,
                }))?;
                let operation_id = OperationId::new(format!("workflow-node-{operation_identity}"));
                let idempotency_key = format!(
                    "workflow:{invocation_id}:node:{node_id}:generation:{}",
                    state.generation
                );
                let valid_retry = snapshot.operations.get(&operation_id).is_some_and(|prior| {
                    matches!(
                        &prior.kind,
                        OperationKind::RunWorkflowNode {
                            invocation_id: prior_invocation,
                            node_id: prior_node,
                            generation: prior_generation,
                        } if prior_invocation == invocation_id
                            && prior_node == &node_id
                            && prior_generation.saturating_add(1) == state.generation
                    ) && prior.status.is_terminal()
                        && state.status == WorkflowNodeStatus::RetryScheduled
                });
                if snapshot.operations.contains_key(&operation_id) && !valid_retry {
                    return Err(BlackopsError::Conflict(format!(
                        "workflow node execution identity {operation_id} is already in use"
                    )));
                }
                if state.generation > 0 && !valid_retry {
                    return Err(BlackopsError::InvalidState(format!(
                        "workflow node {node_id} retry has no terminal prior generation"
                    )));
                }
                if snapshot.request_dedup.contains_key(&idempotency_key) {
                    return Err(BlackopsError::Conflict(format!(
                        "workflow node generation {} was already requested",
                        state.generation
                    )));
                }
                let execution_input = json!({
                    "workflow_input": run.input,
                    "node_input": input,
                    "outputs": run.outputs,
                });
                let input_json = serde_json::to_string(&execution_input)?;
                let mut labels = execution.labels.clone();
                labels.insert("workflow_invocation_id".into(), invocation_id.to_string());
                labels.insert("workflow_node_id".into(), node_id.clone());
                labels.insert("workflow_generation".into(), state.generation.to_string());
                let request = ExecutionRequest {
                    operation_id: operation_id.clone(),
                    idempotency_key,
                    kind: ExecutionKind::Fresh {
                        prompt: format!(
                            "{}\n\nWorkflow node input:\n{input_json}",
                            execution.prompt
                        ),
                    },
                    provider: execution.provider,
                    model: execution.model.clone(),
                    effort: execution.effort.clone(),
                    service_tier: execution.service_tier,
                    code_mode: execution.code_mode,
                    dispatch_context: execution.dispatch_context.clone(),
                    working_set: execution.working_set.clone(),
                    shell_env: execution.shell_env.clone(),
                    tool_policy: execution.tool_policy.clone(),
                    system_prompt: execution.system_prompt.clone(),
                    output_schema: execution.output_schema.clone(),
                    labels,
                };
                let digest = request_digest(&json!({
                    "invocation_id": invocation_id,
                    "node_id": node_id,
                    "generation": state.generation,
                    "request": request,
                }))?;
                let operation = execution_operation(
                    request.clone(),
                    digest,
                    OperationKind::RunWorkflowNode {
                        invocation_id: invocation_id.clone(),
                        node_id: node_id.clone(),
                        generation: state.generation,
                    },
                    now_unix_ms,
                );
                install_execution_operation(snapshot, operation.clone(), None, request);
                let run = snapshot
                    .workflow_runs
                    .get_mut(invocation_id)
                    .expect("workflow checked");
                run.status = WorkflowRunStatus::Running;
                run.waiting_on = None;
                run.updated_at_unix_ms = now_unix_ms;
                run.nodes.insert(
                    node_id.clone(),
                    WorkflowNodeState {
                        node_id: node_id.clone(),
                        generation: state.generation,
                        status: WorkflowNodeStatus::Running,
                        operation_id: Some(operation_id.clone()),
                        wait_id: None,
                        next_attempt_unix_ms: None,
                        output: None,
                        last_error: state.last_error,
                        updated_at_unix_ms: now_unix_ms,
                    },
                );
                let invocation = snapshot
                    .invocations
                    .get_mut(invocation_id)
                    .expect("workflow invocation checked");
                invocation.status = InvocationStatus::Running;
                invocation.operation_id = Some(operation_id);
                invocation.updated_at_unix_ms = now_unix_ms;
                queue_operation_record(snapshot, &operation, "requested")?;
                queue_record(
                    snapshot,
                    format!(
                        "blackops:workflow:{invocation_id}:node:{node_id}:generation:{}:started",
                        state.generation
                    ),
                    "workflow.node.started",
                    Some(invocation_id.to_string()),
                    json!({"node_id": node_id, "generation": state.generation}),
                )?;
                return Ok(());
            }
            WorkflowNodeKind::Wait {
                topic,
                selector,
                timeout_ms,
            } => {
                let wait_id = WaitId::new(format!(
                    "workflow-{invocation_id}-{node_id}-g{}",
                    state.generation
                ));
                let wait = OperationalWait {
                    wait_id: wait_id.clone(),
                    topic: topic.clone(),
                    selector: selector.clone(),
                    status: WaitStatus::Pending,
                    deadline_unix_ms: timeout_ms.map(|timeout| now_unix_ms.saturating_add(timeout)),
                    resolution: None,
                    created_at_unix_ms: now_unix_ms,
                    updated_at_unix_ms: now_unix_ms,
                };
                if let Some(existing) = snapshot.waits.get(&wait_id)
                    && existing != &wait
                {
                    return Err(BlackopsError::Conflict(format!(
                        "workflow wait {wait_id} already exists with different intent"
                    )));
                }
                snapshot.waits.insert(wait_id.clone(), wait.clone());
                let run = snapshot
                    .workflow_runs
                    .get_mut(invocation_id)
                    .expect("workflow checked");
                run.status = WorkflowRunStatus::Waiting;
                run.waiting_on = Some(wait_id.clone());
                run.updated_at_unix_ms = now_unix_ms;
                run.nodes.insert(
                    node_id.clone(),
                    WorkflowNodeState {
                        node_id: node_id.clone(),
                        generation: state.generation,
                        status: WorkflowNodeStatus::Waiting,
                        operation_id: None,
                        wait_id: Some(wait_id.clone()),
                        next_attempt_unix_ms: None,
                        output: None,
                        last_error: state.last_error,
                        updated_at_unix_ms: now_unix_ms,
                    },
                );
                queue_record(
                    snapshot,
                    format!("blackops:wait:{wait_id}:created"),
                    "wait.created",
                    Some(wait_id.to_string()),
                    serde_json::to_value(&wait)?,
                )?;
                queue_record(
                    snapshot,
                    format!(
                        "blackops:workflow:{invocation_id}:node:{node_id}:generation:{}:waiting",
                        state.generation
                    ),
                    "workflow.node.waiting",
                    Some(invocation_id.to_string()),
                    json!({"node_id": node_id, "generation": state.generation, "wait_id": wait_id}),
                )?;
                return Ok(());
            }
            WorkflowNodeKind::Integration { intent } => {
                let created = create_integration_intent(
                    snapshot,
                    invocation_id,
                    Some(&node_id),
                    state.generation,
                    0,
                    intent,
                    now_unix_ms,
                )?;
                complete_workflow_node(
                    snapshot,
                    invocation_id,
                    &node_id,
                    json!({"integration_intent_id": created.integration_intent_id}),
                    now_unix_ms,
                    false,
                )?;
            }
        }
    }
    Err(BlackopsError::InvalidState(
        "workflow exceeded 256 immediate transitions without suspension".into(),
    ))
}

fn complete_workflow_node(
    snapshot: &mut OperationalSnapshot,
    invocation_id: &InvocationId,
    node_id: &str,
    output: Value,
    now_unix_ms: u64,
    advance: bool,
) -> BlackopsResult<()> {
    let run = snapshot
        .workflow_runs
        .get(invocation_id)
        .cloned()
        .ok_or_else(|| BlackopsError::InvalidState("workflow run disappeared".into()))?;
    let definition = snapshot
        .definitions
        .get(&run.definition)
        .cloned()
        .ok_or_else(|| BlackopsError::InvalidState("workflow definition disappeared".into()))?;
    let workflow = parse_workflow_definition(&definition)?;
    let node = workflow.nodes.get(node_id).ok_or_else(|| {
        BlackopsError::InvalidState(format!("workflow node {node_id} disappeared"))
    })?;
    let generation = run.nodes.get(node_id).map_or(0, |state| state.generation);
    let terminal = matches!(node.transition, WorkflowTransition::Terminal);
    {
        let run = snapshot
            .workflow_runs
            .get_mut(invocation_id)
            .expect("workflow checked");
        let state = run
            .nodes
            .entry(node_id.to_string())
            .or_insert(WorkflowNodeState {
                node_id: node_id.to_string(),
                generation,
                status: WorkflowNodeStatus::Pending,
                operation_id: None,
                wait_id: None,
                next_attempt_unix_ms: None,
                output: None,
                last_error: None,
                updated_at_unix_ms: now_unix_ms,
            });
        state.status = WorkflowNodeStatus::Completed;
        state.output = Some(output.clone());
        state.next_attempt_unix_ms = None;
        state.updated_at_unix_ms = now_unix_ms;
        run.outputs.insert(node_id.to_string(), output.clone());
        run.waiting_on = None;
        run.updated_at_unix_ms = now_unix_ms;
        match &node.transition {
            WorkflowTransition::Goto { to } => {
                run.current_node = to.clone();
                run.status = WorkflowRunStatus::Running;
            }
            WorkflowTransition::Terminal => {
                run.status = WorkflowRunStatus::Completed;
            }
        }
    }
    queue_record(
        snapshot,
        format!(
            "blackops:workflow:{invocation_id}:node:{node_id}:generation:{generation}:completed"
        ),
        "workflow.node.completed",
        Some(invocation_id.to_string()),
        json!({"node_id": node_id, "generation": generation, "output": output}),
    )?;
    if terminal {
        let invocation = snapshot
            .invocations
            .get_mut(invocation_id)
            .expect("workflow invocation checked");
        invocation.status = InvocationStatus::Completed;
        invocation.updated_at_unix_ms = now_unix_ms;
        for (index, intent) in workflow.terminal_intents.iter().enumerate() {
            create_integration_intent(
                snapshot,
                invocation_id,
                None,
                generation,
                index,
                intent,
                now_unix_ms,
            )?;
        }
        let run = snapshot.workflow_runs[invocation_id].clone();
        queue_record(
            snapshot,
            format!("blackops:workflow:{invocation_id}:terminal"),
            "workflow.terminal",
            Some(invocation_id.to_string()),
            serde_json::to_value(run)?,
        )?;
        return Ok(());
    }
    if advance {
        advance_workflow(snapshot, invocation_id, now_unix_ms)?;
    }
    Ok(())
}

fn fail_workflow_node(
    snapshot: &mut OperationalSnapshot,
    invocation_id: &InvocationId,
    node_id: &str,
    error: Value,
    now_unix_ms: u64,
) -> BlackopsResult<()> {
    let run = snapshot
        .workflow_runs
        .get(invocation_id)
        .cloned()
        .ok_or_else(|| BlackopsError::InvalidState("workflow run disappeared".into()))?;
    let workflow =
        parse_workflow_definition(snapshot.definitions.get(&run.definition).ok_or_else(|| {
            BlackopsError::InvalidState("workflow definition disappeared".into())
        })?)?;
    let node = workflow.nodes.get(node_id).ok_or_else(|| {
        BlackopsError::InvalidState(format!("workflow node {node_id} disappeared"))
    })?;
    let generation = run.nodes.get(node_id).map_or(0, |state| state.generation);
    let retry = generation < node.retry.max_retries;
    let next_generation = generation.saturating_add(u32::from(retry));
    let retry_at = retry.then(|| {
        now_unix_ms.saturating_add(
            node.retry
                .backoff_ms
                .saturating_mul(u64::from(next_generation.max(1))),
        )
    });
    {
        let run = snapshot
            .workflow_runs
            .get_mut(invocation_id)
            .expect("workflow checked");
        let state = run
            .nodes
            .entry(node_id.to_string())
            .or_insert(WorkflowNodeState {
                node_id: node_id.to_string(),
                generation,
                status: WorkflowNodeStatus::Pending,
                operation_id: None,
                wait_id: None,
                next_attempt_unix_ms: None,
                output: None,
                last_error: None,
                updated_at_unix_ms: now_unix_ms,
            });
        state.generation = next_generation;
        state.status = if retry {
            WorkflowNodeStatus::RetryScheduled
        } else {
            WorkflowNodeStatus::Failed
        };
        state.wait_id = None;
        state.next_attempt_unix_ms = retry_at;
        state.last_error = Some(error.clone());
        state.updated_at_unix_ms = now_unix_ms;
        run.waiting_on = None;
        run.status = if retry {
            WorkflowRunStatus::RetryScheduled
        } else {
            WorkflowRunStatus::Failed
        };
        run.updated_at_unix_ms = now_unix_ms;
    }
    let invocation = snapshot
        .invocations
        .get_mut(invocation_id)
        .expect("workflow invocation checked");
    invocation.status = if retry {
        InvocationStatus::Running
    } else {
        InvocationStatus::Failed
    };
    invocation.updated_at_unix_ms = now_unix_ms;
    if retry {
        queue_record(
            snapshot,
            format!(
                "blackops:workflow:{invocation_id}:node:{node_id}:generation:{next_generation}:retry"
            ),
            "workflow.node.retry_scheduled",
            Some(invocation_id.to_string()),
            json!({
                "node_id": node_id,
                "generation": next_generation,
                "retry_at_unix_ms": retry_at,
                "error": error,
            }),
        )?;
    } else {
        queue_record(
            snapshot,
            format!("blackops:workflow:{invocation_id}:terminal"),
            "workflow.terminal",
            Some(invocation_id.to_string()),
            json!({"status": "failed", "node_id": node_id, "error": error}),
        )?;
    }
    Ok(())
}

fn create_integration_intent(
    snapshot: &mut OperationalSnapshot,
    invocation_id: &InvocationId,
    node_id: Option<&str>,
    generation: u32,
    ordinal: usize,
    template: &IntegrationIntentTemplate,
    now_unix_ms: u64,
) -> BlackopsResult<IntegrationIntent> {
    let owner = node_id.unwrap_or("terminal");
    let intent_id = IntegrationIntentId::new(format!(
        "workflow-{invocation_id}-{owner}-g{generation}-{ordinal}"
    ));
    if let Some(existing) = snapshot.integration_intents.get(&intent_id) {
        return Ok(existing.clone());
    }
    let run = snapshot
        .workflow_runs
        .get(invocation_id)
        .ok_or_else(|| BlackopsError::InvalidState("workflow run disappeared".into()))?;
    let intent = IntegrationIntent {
        integration_intent_id: intent_id.clone(),
        invocation_id: invocation_id.clone(),
        node_id: node_id.map(str::to_owned),
        kind: template.kind,
        target: template.target.clone(),
        payload: json!({
            "configured": template.payload,
            "workflow_input": run.input,
            "outputs": run.outputs,
        }),
        status: IntegrationIntentStatus::Requested,
        result: None,
        created_at_unix_ms: now_unix_ms,
        updated_at_unix_ms: now_unix_ms,
    };
    snapshot
        .integration_intents
        .insert(intent_id.clone(), intent.clone());
    snapshot
        .workflow_runs
        .get_mut(invocation_id)
        .expect("workflow checked")
        .integration_intent_ids
        .push(intent_id.clone());
    queue_record(
        snapshot,
        format!("blackops:integration:{intent_id}:requested"),
        "integration.requested",
        Some(intent_id.to_string()),
        serde_json::to_value(&intent)?,
    )?;
    Ok(intent)
}

fn resume_workflow_wait(
    snapshot: &mut OperationalSnapshot,
    wait_id: &WaitId,
    status: WaitStatus,
    resolution: Value,
    now_unix_ms: u64,
) -> BlackopsResult<()> {
    let selected = snapshot
        .workflow_runs
        .values()
        .find(|run| run.waiting_on.as_ref() == Some(wait_id))
        .map(|run| (run.invocation_id.clone(), run.current_node.clone()));
    let Some((invocation_id, node_id)) = selected else {
        return Ok(());
    };
    if status == WaitStatus::Satisfied {
        complete_workflow_node(
            snapshot,
            &invocation_id,
            &node_id,
            resolution,
            now_unix_ms,
            true,
        )
    } else {
        fail_workflow_node(
            snapshot,
            &invocation_id,
            &node_id,
            json!({"wait_status": status, "resolution": resolution}),
            now_unix_ms,
        )
    }
}

fn poll_identity_value(value: &Value) -> BlackopsResult<String> {
    let identity = match value {
        Value::String(value) => value.clone(),
        Value::Number(value) => value.to_string(),
        Value::Bool(value) => value.to_string(),
        Value::Null | Value::Array(_) | Value::Object(_) => {
            return Err(BlackopsError::InvalidRequest(
                "poll delivery identity must be a string, number, or boolean".into(),
            ));
        }
    };
    if identity.is_empty() || identity.len() > 1_024 {
        return Err(BlackopsError::InvalidRequest(
            "poll delivery identity must contain 1 to 1024 bytes".into(),
        ));
    }
    Ok(identity)
}

fn validate_poll_source(source: &PollSourceSpec) -> BlackopsResult<()> {
    if source.contract_version != crate::POLL_SOURCE_CONTRACT_VERSION {
        return Err(BlackopsError::InvalidRequest(format!(
            "poll source contract version {} is unsupported",
            source.contract_version
        )));
    }
    if source.url.is_empty() || source.url.len() > 2_048 {
        return Err(BlackopsError::InvalidRequest(
            "poll source URL must contain 1 to 2048 bytes".into(),
        ));
    }
    if !matches!(source.method.as_str(), "GET" | "POST") {
        return Err(BlackopsError::InvalidRequest(
            "poll source method must be GET or POST".into(),
        ));
    }
    if source.method == "GET" && source.body.is_some() {
        return Err(BlackopsError::InvalidRequest(
            "poll GET source must not declare a request body".into(),
        ));
    }
    if source.timeout_ms == 0 || source.timeout_ms > 30_000 {
        return Err(BlackopsError::InvalidRequest(
            "poll source timeout must be between 1 and 30000 ms".into(),
        ));
    }
    if source.max_items == 0 || source.max_items > 64 {
        return Err(BlackopsError::InvalidRequest(
            "poll source max_items must be between 1 and 64".into(),
        ));
    }
    for pointer in [&source.items_pointer, &source.delivery_id_pointer]
        .into_iter()
        .flatten()
    {
        if pointer.len() > 1_024 || (!pointer.is_empty() && !pointer.starts_with('/')) {
            return Err(BlackopsError::InvalidRequest(
                "poll source JSON pointers must be at most 1024 bytes and empty or start with '/'"
                    .into(),
            ));
        }
    }
    if source.headers.len() > 32 {
        return Err(BlackopsError::InvalidRequest(
            "poll source may declare at most 32 headers".into(),
        ));
    }
    let mut header_bytes = 0_usize;
    for (name, value) in &source.headers {
        header_bytes = header_bytes
            .saturating_add(name.len())
            .saturating_add(value.len());
        if name.is_empty()
            || name.len() > 256
            || value.len() > 8_192
            || value.contains(['\r', '\n'])
            || !name.bytes().all(|byte| {
                byte.is_ascii_alphanumeric()
                    || matches!(
                        byte,
                        b'!' | b'#'
                            | b'$'
                            | b'%'
                            | b'&'
                            | b'\''
                            | b'*'
                            | b'+'
                            | b'-'
                            | b'.'
                            | b'^'
                            | b'_'
                            | b'`'
                            | b'|'
                            | b'~'
                    )
            })
        {
            return Err(BlackopsError::InvalidRequest(
                "poll source contains an invalid or oversized header".into(),
            ));
        }
        if matches!(
            name.to_ascii_lowercase().as_str(),
            "authorization" | "proxy-authorization" | "cookie" | "set-cookie"
        ) {
            return Err(BlackopsError::InvalidRequest(
                "poll source must not persist credential-bearing headers".into(),
            ));
        }
    }
    if header_bytes > 16 * 1_024 {
        return Err(BlackopsError::InvalidRequest(
            "poll source headers exceed 16 KiB".into(),
        ));
    }
    if source
        .body
        .as_ref()
        .map(serde_json::to_vec)
        .transpose()?
        .is_some_and(|body| body.len() > 64 * 1_024)
    {
        return Err(BlackopsError::InvalidRequest(
            "poll source request body exceeds 64 KiB".into(),
        ));
    }
    let remainder = source.url.strip_prefix("http://").ok_or_else(|| {
        BlackopsError::InvalidRequest("poll source must use loopback HTTP".into())
    })?;
    let authority = remainder.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.is_empty() || authority.contains('@') {
        return Err(BlackopsError::InvalidRequest(
            "poll source URL has an invalid authority".into(),
        ));
    }
    let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
        let (host, suffix) = bracketed
            .split_once(']')
            .ok_or_else(|| BlackopsError::InvalidRequest("invalid poll IPv6 host".into()))?;
        let port = if suffix.is_empty() {
            None
        } else {
            Some(
                suffix
                    .strip_prefix(':')
                    .ok_or_else(|| {
                        BlackopsError::InvalidRequest("invalid poll IPv6 authority".into())
                    })?
                    .parse::<u16>()
                    .map_err(|_| BlackopsError::InvalidRequest("invalid poll port".into()))?,
            )
        };
        (host, port)
    } else if let Some((host, port)) = authority.rsplit_once(':') {
        if host.contains(':') {
            return Err(BlackopsError::InvalidRequest(
                "poll IPv6 hosts must be bracketed".into(),
            ));
        }
        (
            host,
            Some(
                port.parse::<u16>()
                    .map_err(|_| BlackopsError::InvalidRequest("invalid poll port".into()))?,
            ),
        )
    } else {
        (authority, None)
    };
    if port == Some(0) {
        return Err(BlackopsError::InvalidRequest(
            "poll source port must be nonzero".into(),
        ));
    }
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback());
    if !loopback {
        return Err(BlackopsError::InvalidRequest(
            "poll source must target localhost, 127.0.0.0/8, or ::1".into(),
        ));
    }
    Ok(())
}

fn next_cron_due(expression: &str, after_unix_ms: u64) -> BlackopsResult<Option<u64>> {
    let schedule = Schedule::from_str(expression).map_err(|error| {
        BlackopsError::InvalidRequest(format!("invalid cron expression: {error}"))
    })?;
    let after_unix_ms = i64::try_from(after_unix_ms)
        .map_err(|_| BlackopsError::InvalidRequest("cron timestamp exceeds i64".into()))?;
    let after = Utc
        .timestamp_millis_opt(after_unix_ms)
        .single()
        .ok_or_else(|| BlackopsError::InvalidRequest("invalid cron timestamp".into()))?;
    schedule
        .after(&after)
        .next()
        .map(|next| {
            u64::try_from(next.timestamp_millis()).map_err(|_| {
                BlackopsError::InvalidState("cron produced a timestamp before the epoch".into())
            })
        })
        .transpose()
}

fn next_due_after_fire(
    schedule: &ScheduleIntent,
    due_at_unix_ms: u64,
    now_unix_ms: u64,
) -> BlackopsResult<Option<u64>> {
    match &schedule.trigger {
        ScheduleTrigger::At { .. } | ScheduleTrigger::Webhook { .. } => Ok(None),
        ScheduleTrigger::Interval { every_ms, .. } | ScheduleTrigger::Poll { every_ms, .. } => {
            let elapsed = now_unix_ms.saturating_sub(due_at_unix_ms);
            let steps = elapsed
                .checked_div(*every_ms)
                .and_then(|steps| steps.checked_add(1))
                .ok_or_else(|| BlackopsError::InvalidState("schedule cadence overflowed".into()))?;
            let advance = every_ms
                .checked_mul(steps)
                .ok_or_else(|| BlackopsError::InvalidState("schedule cadence overflowed".into()))?;
            let next = due_at_unix_ms.checked_add(advance).ok_or_else(|| {
                BlackopsError::InvalidState("schedule timestamp overflowed".into())
            })?;
            Ok(Some(next))
        }
        ScheduleTrigger::Cron { expression } => {
            next_cron_due(expression, now_unix_ms.max(due_at_unix_ms))
        }
    }
}

fn install_scheduled_invocation(
    snapshot: &mut OperationalSnapshot,
    schedule: &ScheduleIntent,
    occurrence: &str,
    input: Value,
    now_unix_ms: u64,
) -> BlackopsResult<InvocationIntent> {
    let invocation_id = InvocationId::new(format!(
        "schedule-{}-g{}-{occurrence}",
        schedule.schedule_id, schedule.generation
    ));
    if let Some(existing) = snapshot.invocations.get(&invocation_id) {
        if existing.definition == schedule.invocation.definition && existing.input == input {
            return Ok(existing.clone());
        }
        return Err(BlackopsError::Conflict(format!(
            "scheduled invocation {invocation_id} already exists with different intent"
        )));
    }
    if schedule.invocation.definition.kind == crate::DefinitionKind::Workflow {
        let definition = snapshot
            .definitions
            .get(&schedule.invocation.definition)
            .cloned()
            .ok_or_else(|| BlackopsError::InvalidState("workflow definition disappeared".into()))?;
        let workflow = parse_workflow_definition(&definition)?;
        let invocation = InvocationIntent {
            invocation_id: invocation_id.clone(),
            definition: schedule.invocation.definition.clone(),
            input: input.clone(),
            status: InvocationStatus::Running,
            operation_id: None,
            output: None,
            requested_at_unix_ms: now_unix_ms,
            updated_at_unix_ms: now_unix_ms,
        };
        let run = WorkflowRun {
            invocation_id: invocation_id.clone(),
            definition: schedule.invocation.definition.clone(),
            input,
            status: WorkflowRunStatus::Running,
            current_node: workflow.start,
            nodes: BTreeMap::new(),
            outputs: BTreeMap::new(),
            waiting_on: None,
            integration_intent_ids: Vec::new(),
            created_at_unix_ms: now_unix_ms,
            updated_at_unix_ms: now_unix_ms,
        };
        snapshot
            .invocations
            .insert(invocation_id.clone(), invocation.clone());
        snapshot.workflow_runs.insert(invocation_id.clone(), run);
        queue_record(
            snapshot,
            format!("blackops:invocation:{invocation_id}:requested"),
            "invocation.requested",
            Some(invocation_id.to_string()),
            serde_json::to_value(&invocation)?,
        )?;
        advance_workflow(snapshot, &invocation_id, now_unix_ms)?;
        return Ok(snapshot.invocations[&invocation_id].clone());
    }
    let template = schedule.invocation.execution.as_ref().ok_or_else(|| {
        BlackopsError::InvalidState(format!(
            "schedule {} has no execution template",
            schedule.schedule_id
        ))
    })?;
    let operation_id = OperationId::new(format!(
        "invoke-{}-g{}-{occurrence}",
        schedule.schedule_id, schedule.generation
    ));
    if snapshot.operations.contains_key(&operation_id) {
        return Err(BlackopsError::Conflict(format!(
            "scheduled operation {operation_id} already exists without its invocation"
        )));
    }
    let idempotency_key = format!(
        "schedule:{}:generation:{}:{occurrence}",
        schedule.schedule_id, schedule.generation
    );
    let mut labels = template.labels.clone();
    labels.insert("schedule_id".into(), schedule.schedule_id.to_string());
    labels.insert(
        "schedule_generation".into(),
        schedule.generation.to_string(),
    );
    labels.insert("schedule_occurrence".into(), occurrence.to_string());
    let input_json = serde_json::to_string(&input)?;
    let execution = ExecutionRequest {
        operation_id: operation_id.clone(),
        idempotency_key: idempotency_key.clone(),
        kind: ExecutionKind::Fresh {
            prompt: format!("{}\n\nInvocation input:\n{input_json}", template.prompt),
        },
        provider: template.provider,
        model: template.model.clone(),
        effort: template.effort.clone(),
        service_tier: template.service_tier,
        code_mode: template.code_mode,
        dispatch_context: template.dispatch_context.clone(),
        working_set: template.working_set.clone(),
        shell_env: template.shell_env.clone(),
        tool_policy: template.tool_policy.clone(),
        system_prompt: template.system_prompt.clone(),
        output_schema: template.output_schema.clone(),
        labels,
    };
    let invocation = InvocationIntent {
        invocation_id: invocation_id.clone(),
        definition: schedule.invocation.definition.clone(),
        input,
        status: InvocationStatus::Requested,
        operation_id: Some(operation_id.clone()),
        output: None,
        requested_at_unix_ms: now_unix_ms,
        updated_at_unix_ms: now_unix_ms,
    };
    let digest = request_digest(&json!({
        "invocation": invocation,
        "execution": execution,
    }))?;
    let operation = execution_operation(
        execution.clone(),
        digest,
        OperationKind::InvokeDefinition {
            invocation_id: invocation_id.clone(),
        },
        now_unix_ms,
    );
    snapshot
        .invocations
        .insert(invocation_id.clone(), invocation.clone());
    install_execution_operation(snapshot, operation.clone(), None, execution);
    queue_operation_record(snapshot, &operation, "requested")?;
    queue_record(
        snapshot,
        format!("blackops:invocation:{invocation_id}:requested"),
        "invocation.requested",
        Some(invocation_id.to_string()),
        serde_json::to_value(&invocation)?,
    )?;
    Ok(invocation)
}

fn validate_execution_identity(
    idempotency_key: &str,
    execution: &ExecutionRequest,
) -> BlackopsResult<()> {
    if idempotency_key.trim().is_empty() || execution.idempotency_key.trim().is_empty() {
        return Err(BlackopsError::InvalidRequest(
            "idempotency key must not be empty".into(),
        ));
    }
    if execution.idempotency_key != idempotency_key {
        return Err(BlackopsError::InvalidRequest(
            "operational and execution idempotency keys must match".into(),
        ));
    }
    Ok(())
}

fn validate_message_request(
    snapshot: &OperationalSnapshot,
    sender: &Option<AgentId>,
    recipient: &AgentId,
    body: &str,
) -> BlackopsResult<()> {
    if body.trim().is_empty() {
        return Err(BlackopsError::InvalidRequest(
            "mailbox message must not be empty".into(),
        ));
    }
    if !snapshot.agents.contains_key(recipient) {
        return Err(BlackopsError::NotFound(format!("agent {recipient}")));
    }
    if let Some(sender) = sender
        && !snapshot.agents.contains_key(sender)
    {
        return Err(BlackopsError::NotFound(format!("agent {sender}")));
    }
    Ok(())
}

fn canonical_segment(name: &str) -> BlackopsResult<String> {
    let name = name.trim().to_ascii_lowercase().replace(' ', "-");
    if name.is_empty()
        || name.starts_with('.')
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(BlackopsError::InvalidRequest(
            "agent name must form a safe canonical path segment".into(),
        ));
    }
    Ok(name)
}

fn path_in_tree(root: &str, candidate: &str) -> bool {
    candidate == root
        || candidate
            .strip_prefix(root)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn execution_operation(
    request: ExecutionRequest,
    digest: String,
    kind: OperationKind,
    requested_at_unix_ms: u64,
) -> OperationRecord {
    OperationRecord {
        operation_id: request.operation_id.clone(),
        idempotency_key: request.idempotency_key.clone(),
        request_digest: digest,
        kind,
        status: OperationStatus::Requested,
        execution_request: Some(request),
        accepted: None,
        last_attempt_state: None,
        terminal: None,
        requested_at_unix_ms,
        updated_at_unix_ms: requested_at_unix_ms,
        last_error: None,
    }
}

fn install_execution_operation(
    snapshot: &mut OperationalSnapshot,
    operation: OperationRecord,
    agent_id: Option<AgentId>,
    request: ExecutionRequest,
) {
    let effect_id = fleet_effect_id(&operation.operation_id);
    snapshot.request_dedup.insert(
        operation.idempotency_key.clone(),
        RequestDedupRecord {
            idempotency_key: operation.idempotency_key.clone(),
            request_digest: operation.request_digest.clone(),
            operation_id: operation.operation_id.clone(),
            agent_id,
        },
    );
    snapshot.fleet_outbox.insert(
        effect_id.clone(),
        FleetEffect {
            effect_id,
            operation_id: operation.operation_id.clone(),
            effect: FleetEffectKind::RequestExecution {
                request: Box::new(request),
            },
            retry_count: 0,
            last_error: None,
        },
    );
    snapshot
        .operations
        .insert(operation.operation_id.clone(), operation);
}

#[allow(clippy::too_many_arguments)]
fn append_message(
    snapshot: &mut OperationalSnapshot,
    idempotency_key: &str,
    digest: &str,
    message_id: String,
    sender: Option<AgentId>,
    recipient: AgentId,
    kind: MailboxMessageKind,
    body: String,
    operation_id: Option<OperationId>,
    created_at_unix_ms: u64,
) -> BlackopsResult<MailboxMessage> {
    let mailbox = snapshot
        .mailboxes
        .get_mut(&recipient)
        .expect("recipient checked before transaction");
    let sequence = mailbox.next_sequence;
    mailbox.next_sequence = mailbox.next_sequence.saturating_add(1);
    let message = MailboxMessage {
        message_id: message_id.clone(),
        sequence,
        sender,
        recipient: recipient.clone(),
        kind,
        body,
        operation_id,
        created_at_unix_ms,
    };
    mailbox.messages.insert(sequence, message.clone());
    snapshot.message_dedup.insert(
        idempotency_key.to_string(),
        MessageDedupRecord {
            idempotency_key: idempotency_key.to_string(),
            request_digest: digest.to_string(),
            recipient: recipient.clone(),
            sequence,
            message_id,
        },
    );
    queue_record(
        snapshot,
        format!("blackops:mailbox:{recipient}:{sequence}"),
        "mailbox.message",
        Some(recipient.to_string()),
        serde_json::to_value(&message)?,
    )?;
    Ok(message)
}

fn mailbox_delivery_cursor(session_id: &bro_core::SessionId) -> String {
    format!("delivery:{session_id}")
}

fn mailbox_delivery_effect_id(
    recipient: &AgentId,
    session_id: &bro_core::SessionId,
    sequence: u64,
) -> String {
    format!("mailbox:{recipient}:{session_id}:{sequence:020}")
}

fn queue_mailbox_delivery(
    snapshot: &mut OperationalSnapshot,
    message: &MailboxMessage,
    wake: bool,
) -> BlackopsResult<()> {
    let agent = snapshot.agents.get(&message.recipient).ok_or_else(|| {
        BlackopsError::InvalidState(format!(
            "mailbox message {} references a missing recipient",
            message.message_id
        ))
    })?;
    let Some(session_id) = agent.current_session_id.clone() else {
        return Ok(());
    };
    let canonical_target = agent.path.clone();
    let cursor_name = mailbox_delivery_cursor(&session_id);
    let delivered = snapshot
        .mailboxes
        .get(&message.recipient)
        .and_then(|mailbox| mailbox.cursors.get(&cursor_name))
        .map_or(0, |cursor| cursor.last_sequence);
    if message.sequence <= delivered {
        return Ok(());
    }
    let effect_id = mailbox_delivery_effect_id(&message.recipient, &session_id, message.sequence);
    snapshot
        .mailbox_delivery_outbox
        .entry(effect_id.clone())
        .or_insert_with(|| MailboxDeliveryEffect {
            effect_id: effect_id.clone(),
            delivery_id: effect_id,
            recipient: message.recipient.clone(),
            canonical_target,
            session_id,
            through_sequence: message.sequence,
            wake,
            message: message.clone(),
            retry_count: 0,
            last_error: None,
        });
    Ok(())
}

fn queue_undelivered_mailbox(
    snapshot: &mut OperationalSnapshot,
    agent_id: &AgentId,
) -> BlackopsResult<()> {
    let messages = snapshot
        .mailboxes
        .get(agent_id)
        .ok_or_else(|| BlackopsError::InvalidState(format!("agent {agent_id} has no mailbox")))?
        .messages
        .values()
        .cloned()
        .collect::<Vec<_>>();
    for message in messages {
        let wake = message.kind == MailboxMessageKind::Followup;
        queue_mailbox_delivery(snapshot, &message, wake)?;
    }
    Ok(())
}

fn fleet_effect_id(operation_id: &OperationId) -> String {
    format!("fleet:{operation_id}")
}

fn queue_operation_record(
    snapshot: &mut OperationalSnapshot,
    operation: &OperationRecord,
    transition: &str,
) -> BlackopsResult<()> {
    let record_id = match &operation.kind {
        OperationKind::RunWorkflowNode { generation, .. } => format!(
            "blackops:operation:{}:generation:{generation}:{transition}",
            operation.operation_id
        ),
        _ => format!("blackops:operation:{}:{transition}", operation.operation_id),
    };
    queue_record(
        snapshot,
        record_id,
        format!("operation.{transition}"),
        Some(operation.operation_id.to_string()),
        serde_json::to_value(operation)?,
    )
}

fn queue_record(
    snapshot: &mut OperationalSnapshot,
    record_id: String,
    kind: impl Into<String>,
    subject: Option<String>,
    payload: Value,
) -> BlackopsResult<()> {
    if let Some(existing) = snapshot.record_outbox.get(&record_id) {
        if existing.payload != payload {
            return Err(BlackopsError::Conflict(format!(
                "record ID {record_id} was reused with different payload"
            )));
        }
        return Ok(());
    }
    let cursor = snapshot.next_record_cursor;
    let record = RecordEnvelope {
        record_id: record_id.clone(),
        producer: "blackopsd".into(),
        cursor: cursor.to_string(),
        kind: kind.into(),
        occurred_at: None,
        subject,
        attributes: BTreeMap::new(),
        payload,
    };
    let record_bytes = serde_json::to_vec(&record)?.len();
    if record_bytes > MAX_RECORD_BYTES {
        return Err(BlackopsError::InvalidRequest(format!(
            "record {record_id} needs {record_bytes} bytes; the corpus record limit is {MAX_RECORD_BYTES}"
        )));
    }
    snapshot.next_record_cursor = snapshot.next_record_cursor.saturating_add(1);
    snapshot.record_outbox.insert(record_id.clone(), record);
    Ok(())
}

fn request_digest<T: Serialize>(request: &T) -> BlackopsResult<String> {
    let bytes = serde_json::to_vec(request)?;
    Ok(format!("sha256:{:x}", Sha256::digest(bytes)))
}

/// Hash the semantic effect request while excluding observation timestamps.
/// A response can be lost after commit and replayed later under the same
/// provider invocation identity; that retry time is not intent drift.
fn effect_request_digest<T: Serialize>(
    request: &T,
    volatile_fields: &[&str],
) -> BlackopsResult<String> {
    let mut value = serde_json::to_value(request)?;
    let object = value.as_object_mut().ok_or_else(|| {
        BlackopsError::InvalidState("effect request did not serialize as an object".into())
    })?;
    for field in volatile_fields {
        object.remove(*field);
    }
    request_digest(&value)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use bro_capabilities::{ExecutionServiceTier, ExecutionToolPolicy, WorkingSetIntent};
    use bro_core::{AttemptId, OperationId, Provider, SessionId, TaskId};

    use super::*;
    use crate::{
        DefinitionKind, IntegrationIntentKind, InvocationTemplate, MemoryRepository,
        PollSourceSpec, ScheduleId, ScheduleTrigger, ScheduledExecution,
        WORKFLOW_NODE_CONTRACT_VERSION, WORKFLOW_SCHEMA_VERSION, WorkflowNodeDefinition,
        WorkflowRetryPolicy,
    };

    fn execution(operation: &str, key: &str, kind: ExecutionKind) -> ExecutionRequest {
        ExecutionRequest {
            operation_id: OperationId::new(operation),
            idempotency_key: key.into(),
            kind,
            provider: Provider::Glm,
            model: "glm-test".into(),
            effort: Some("high".into()),
            service_tier: ExecutionServiceTier::Default,
            code_mode: None,
            dispatch_context: None,
            working_set: WorkingSetIntent::Scratch,
            shell_env: BTreeMap::new(),
            tool_policy: ExecutionToolPolicy::default(),
            system_prompt: None,
            output_schema: None,
            labels: BTreeMap::new(),
        }
    }

    fn spawn_request() -> SpawnAgentRequest {
        SpawnAgentRequest {
            idempotency_key: "spawn-one".into(),
            name: "Terra Ultra".into(),
            role: "implementer".into(),
            parent_id: None,
            team_id: None,
            execution: execution(
                "operation-spawn",
                "spawn-one",
                ExecutionKind::Fresh {
                    prompt: "implement the slice".into(),
                },
            ),
            requested_at_unix_ms: 10,
        }
    }

    fn scheduled_execution() -> ScheduledExecution {
        ScheduledExecution {
            provider: Provider::Glm,
            model: "glm-test".into(),
            prompt: "run the scheduled definition".into(),
            effort: Some("high".into()),
            service_tier: ExecutionServiceTier::Default,
            code_mode: None,
            dispatch_context: None,
            working_set: WorkingSetIntent::Scratch,
            shell_env: BTreeMap::new(),
            tool_policy: ExecutionToolPolicy::default(),
            system_prompt: None,
            output_schema: None,
            labels: BTreeMap::new(),
        }
    }

    fn workflow_body() -> Value {
        let nodes = BTreeMap::from([
            (
                "execute".into(),
                WorkflowNodeDefinition {
                    contract_version: WORKFLOW_NODE_CONTRACT_VERSION,
                    kind: WorkflowNodeKind::Execution {
                        execution: Box::new(scheduled_execution()),
                        input: json!({"phase": "execute"}),
                    },
                    transition: WorkflowTransition::Goto { to: "wait".into() },
                    retry: WorkflowRetryPolicy {
                        max_retries: 1,
                        backoff_ms: 10,
                    },
                },
            ),
            (
                "wait".into(),
                WorkflowNodeDefinition {
                    contract_version: WORKFLOW_NODE_CONTRACT_VERSION,
                    kind: WorkflowNodeKind::Wait {
                        topic: "operator.review".into(),
                        selector: json!({"change": 7}),
                        timeout_ms: Some(1_000),
                    },
                    transition: WorkflowTransition::Goto {
                        to: "integrate".into(),
                    },
                    retry: WorkflowRetryPolicy::default(),
                },
            ),
            (
                "integrate".into(),
                WorkflowNodeDefinition {
                    contract_version: WORKFLOW_NODE_CONTRACT_VERSION,
                    kind: WorkflowNodeKind::Integration {
                        intent: IntegrationIntentTemplate {
                            kind: IntegrationIntentKind::Integrate,
                            target: "change/7".into(),
                            payload: json!({"strategy": "verified"}),
                        },
                    },
                    transition: WorkflowTransition::Terminal,
                    retry: WorkflowRetryPolicy::default(),
                },
            ),
        ]);
        serde_json::to_value(WorkflowDefinition {
            schema_version: WORKFLOW_SCHEMA_VERSION,
            start: "execute".into(),
            nodes,
            terminal_intents: vec![IntegrationIntentTemplate {
                kind: IntegrationIntentKind::Publish,
                target: "campaign/events".into(),
                payload: json!({"event": "workflow_finished"}),
            }],
        })
        .unwrap()
    }

    #[test]
    fn duplicate_spawn_keeps_one_identity_and_one_fleet_effect() {
        let repository = Arc::new(MemoryRepository::default());
        let mut authority = BlackopsAuthority::open(repository).unwrap();
        let first = authority.spawn_agent(spawn_request()).unwrap();
        let mut replay = spawn_request();
        replay.requested_at_unix_ms = 9_999;
        let second = authority.spawn_agent(replay).unwrap();
        assert!(!first.deduplicated);
        assert!(second.deduplicated);
        assert_eq!(first.agent.agent_id, second.agent.agent_id);
        assert_eq!(authority.snapshot.agents.len(), 1);
        assert_eq!(authority.snapshot.fleet_outbox.len(), 1);
    }

    #[test]
    fn session_root_advances_only_on_a_newer_authenticated_attempt_generation() {
        let repository = Arc::new(MemoryRepository::default());
        let mut authority = BlackopsAuthority::open(repository.clone()).unwrap();
        let session_id = SessionId::new("session-root");
        let task_1 = TaskId::new("task-1");
        let attempt_1 = AttemptId::new("attempt-1");
        let first = authority
            .ensure_session_root(&session_id, "worker-1", &task_1, &attempt_1, 1, 1)
            .unwrap();
        assert_eq!(first.current_worker_id.as_deref(), Some("worker-1"));
        let repeated = authority
            .ensure_session_root(&session_id, "worker-1", &task_1, &attempt_1, 1, 2)
            .unwrap();
        assert_eq!(repeated, first);
        let task_2 = TaskId::new("task-2");
        let attempt_2 = AttemptId::new("attempt-2");
        assert!(matches!(
            authority.ensure_session_root(&session_id, "worker-2", &task_2, &attempt_2, 1, 3,),
            Err(BlackopsError::Conflict(_))
        ));
        let advanced = authority
            .ensure_session_root(&session_id, "worker-2", &task_2, &attempt_2, 2, 4)
            .unwrap();
        assert_eq!(advanced.agent_id, first.agent_id);
        assert_eq!(advanced.current_worker_id.as_deref(), Some("worker-2"));
        assert_eq!(advanced.current_task_id.as_ref(), Some(&task_2));
        assert_eq!(advanced.current_attempt_id.as_ref(), Some(&attempt_2));
        assert_eq!(advanced.current_session_attempt_generation, Some(2));
        assert!(matches!(
            authority.ensure_session_root(&session_id, "worker-1", &task_1, &attempt_1, 1, 5,),
            Err(BlackopsError::Conflict(_))
        ));
        assert_eq!(authority.snapshot.agents.len(), 1);

        drop(authority);
        let authority = BlackopsAuthority::open(repository).unwrap();
        let restored = &authority.snapshot.agents[&first.agent_id];
        assert_eq!(restored.current_worker_id.as_deref(), Some("worker-2"));
        assert_eq!(restored.current_task_id.as_ref(), Some(&task_2));
        assert_eq!(restored.current_attempt_id.as_ref(), Some(&attempt_2));
        assert_eq!(restored.current_session_attempt_generation, Some(2));
    }

    #[test]
    fn followup_admission_stays_nonterminal_until_attempt_completion() {
        let repository = Arc::new(MemoryRepository::default());
        let mut authority = BlackopsAuthority::open(repository.clone()).unwrap();
        let spawned = authority.spawn_agent(spawn_request()).unwrap();
        authority
            .apply_execution_accepted(
                &spawned.operation.operation_id,
                ExecutionAccepted {
                    operation_id: spawned.operation.operation_id.clone(),
                    attempt_id: AttemptId::new("attempt-1"),
                    task_id: TaskId::new("task-1"),
                    session_id: SessionId::new("session-1"),
                    deduplicated: false,
                },
                11,
            )
            .unwrap();
        authority
            .observe_attempt(
                &spawned.operation.operation_id,
                AttemptOutcome {
                    attempt_id: AttemptId::new("attempt-1"),
                    state: AttemptState::Completed,
                    result: json!({"ok": true}),
                },
                12,
            )
            .unwrap();
        let message = authority
            .send_message(SendMessageRequest {
                idempotency_key: "send-1".into(),
                sender: None,
                recipient: spawned.agent.agent_id.clone(),
                body: "context only".into(),
                created_at_unix_ms: 13,
            })
            .unwrap();
        assert_eq!(message.kind, MailboxMessageKind::Send);
        assert!(authority.snapshot.fleet_outbox.is_empty());
        let followup = authority
            .followup_agent(FollowupAgentRequest {
                idempotency_key: "followup-1".into(),
                sender: None,
                recipient: spawned.agent.agent_id.clone(),
                body: "continue".into(),
                execution: execution(
                    "operation-followup",
                    "followup-1",
                    ExecutionKind::MailboxResume {
                        session_id: SessionId::new("session-1"),
                    },
                ),
                requested_at_unix_ms: 14,
            })
            .unwrap();
        assert_eq!(followup.message.sequence, 2);
        assert_eq!(followup.message.kind, MailboxMessageKind::Followup);
        assert_eq!(authority.snapshot.fleet_outbox.len(), 1);
        assert_eq!(followup.operation.status, OperationStatus::Requested);
        let effects = authority.pending_mailbox_delivery_effects();
        assert_eq!(effects.len(), 2);
        assert!(!effects[0].wake);
        assert!(effects[1].wake);
        assert_eq!(
            authority.snapshot.mailboxes[&spawned.agent.agent_id]
                .cursors
                .get("delivery:session-1"),
            None
        );
        let read = authority
            .read_mailbox(&spawned.agent.agent_id, "worker", None, 10, 15)
            .unwrap();
        assert_eq!(read.messages.len(), 2);
        assert_eq!(read.last_sequence, 2);

        drop(authority);
        let mut authority = BlackopsAuthority::open(repository).unwrap();
        authority
            .apply_execution_accepted(
                &followup.operation.operation_id,
                ExecutionAccepted {
                    operation_id: followup.operation.operation_id.clone(),
                    attempt_id: AttemptId::new("attempt-2"),
                    task_id: TaskId::new("task-2"),
                    session_id: SessionId::new("session-1"),
                    deduplicated: false,
                },
                16,
            )
            .unwrap();
        let effects = authority.pending_mailbox_delivery_effects();
        assert_eq!(effects.len(), 2);
        assert!(matches!(
            authority.acknowledge_mailbox_delivery(
                &effects[1].effect_id,
                &effects[1].delivery_id,
                effects[1].through_sequence,
                17,
            ),
            Err(BlackopsError::Conflict(_))
        ));
        authority
            .acknowledge_mailbox_delivery(
                &effects[0].effect_id,
                &effects[0].delivery_id,
                effects[0].through_sequence,
                18,
            )
            .unwrap();
        authority
            .acknowledge_mailbox_delivery(
                &effects[1].effect_id,
                &effects[1].delivery_id,
                effects[1].through_sequence,
                19,
            )
            .unwrap();
        assert!(authority.snapshot.mailbox_delivery_outbox.is_empty());
        assert_eq!(
            authority.snapshot.mailboxes[&spawned.agent.agent_id].cursors["delivery:session-1"]
                .last_sequence,
            2
        );
        assert_eq!(
            authority
                .operation(&followup.operation.operation_id)
                .unwrap()
                .status,
            OperationStatus::Accepted
        );
        assert_eq!(
            authority.snapshot.agents[&spawned.agent.agent_id].status,
            LogicalAgentStatus::Running
        );

        let completed = authority
            .observe_attempt(
                &followup.operation.operation_id,
                AttemptOutcome {
                    attempt_id: AttemptId::new("attempt-2"),
                    state: AttemptState::Completed,
                    result: json!({"ok": true}),
                },
                20,
            )
            .unwrap();
        assert_eq!(completed.status, OperationStatus::Completed);
        assert_eq!(
            authority.snapshot.agents[&spawned.agent.agent_id].status,
            LogicalAgentStatus::Completed
        );
        let terminal_updated_at =
            authority.snapshot.agents[&spawned.agent.agent_id].updated_at_unix_ms;
        let repeated = authority
            .observe_attempt(
                &followup.operation.operation_id,
                AttemptOutcome {
                    attempt_id: AttemptId::new("attempt-2"),
                    state: AttemptState::Completed,
                    result: json!({"ok": true}),
                },
                21,
            )
            .unwrap();
        assert_eq!(repeated, completed);
        assert_eq!(
            authority.snapshot.agents[&spawned.agent.agent_id].updated_at_unix_ms,
            terminal_updated_at,
            "replaying a terminal attempt must not reapply the logical transition"
        );
    }

    #[test]
    fn definitions_are_immutable_and_schedule_intent_is_durable() {
        let repository = Arc::new(MemoryRepository::default());
        let mut authority = BlackopsAuthority::open(repository).unwrap();
        let definition = authority
            .install_definition(DefinitionInstallRequest {
                kind: DefinitionKind::Atom,
                name: "review-loop".into(),
                version: "1.0.0".into(),
                input_contract: json!({"type": "object"}),
                body: json!({"steps": ["implement", "review"]}),
                activate: true,
                created_at_unix_ms: 1,
            })
            .unwrap();
        let schedule = ScheduleIntent {
            schedule_id: ScheduleId::new("schedule-1"),
            name: "nightly review".into(),
            invocation: InvocationTemplate {
                definition: definition.key,
                input: json!({}),
                execution: Some(scheduled_execution()),
            },
            trigger: ScheduleTrigger::Interval {
                every_ms: 86_400_000,
                anchor_unix_ms: 0,
            },
            enabled: true,
            next_due_unix_ms: Some(100),
            generation: 1,
            created_at_unix_ms: 1,
            updated_at_unix_ms: 1,
        };
        authority.put_schedule(schedule).unwrap();
        assert_eq!(authority.due_schedules(100).len(), 1);
    }

    #[test]
    fn record_queue_accepts_exact_limit_and_rejects_one_byte_over_without_mutation() {
        let record_id = "record-boundary";
        let kind = "definition.installed";
        let empty_payload = json!({"body": ""});
        let empty_envelope = RecordEnvelope {
            record_id: record_id.into(),
            producer: "blackopsd".into(),
            cursor: "1".into(),
            kind: kind.into(),
            occurred_at: None,
            subject: None,
            attributes: BTreeMap::new(),
            payload: empty_payload,
        };
        let fixed_bytes = serde_json::to_vec(&empty_envelope).unwrap().len();
        let exact_payload_bytes = MAX_RECORD_BYTES - fixed_bytes;

        let mut exact = OperationalSnapshot::default();
        queue_record(
            &mut exact,
            record_id.into(),
            kind,
            None,
            json!({"body": "x".repeat(exact_payload_bytes)}),
        )
        .unwrap();
        assert_eq!(
            serde_json::to_vec(&exact.record_outbox[record_id])
                .unwrap()
                .len(),
            MAX_RECORD_BYTES
        );
        assert_eq!(exact.next_record_cursor, 2);

        let mut oversized = OperationalSnapshot::default();
        let before = oversized.clone();
        let error = queue_record(
            &mut oversized,
            record_id.into(),
            kind,
            None,
            json!({"body": "x".repeat(exact_payload_bytes + 1)}),
        )
        .unwrap_err();
        assert!(matches!(error, BlackopsError::InvalidRequest(_)));
        assert_eq!(oversized, before);
    }

    #[test]
    fn due_schedule_materializes_one_idempotent_execution_effect() {
        let repository = Arc::new(MemoryRepository::default());
        let mut authority = BlackopsAuthority::open(repository).unwrap();
        let definition = authority
            .install_definition(DefinitionInstallRequest {
                kind: DefinitionKind::Atom,
                name: "once".into(),
                version: "1.0.0".into(),
                input_contract: json!({"type": "object"}),
                body: json!({"steps": ["run"]}),
                activate: true,
                created_at_unix_ms: 1,
            })
            .unwrap();
        authority
            .put_schedule(ScheduleIntent {
                schedule_id: ScheduleId::new("schedule-once"),
                name: "run once".into(),
                invocation: InvocationTemplate {
                    definition: definition.key,
                    input: json!({"job": 1}),
                    execution: Some(scheduled_execution()),
                },
                trigger: ScheduleTrigger::At { unix_ms: 100 },
                enabled: true,
                next_due_unix_ms: None,
                generation: 1,
                created_at_unix_ms: 1,
                updated_at_unix_ms: 1,
            })
            .unwrap();

        let first = authority.trigger_due_schedules(100, 64).unwrap();
        let retry = authority.trigger_due_schedules(100, 64).unwrap();
        assert_eq!(first.len(), 1);
        assert!(retry.is_empty());
        assert_eq!(authority.snapshot.invocations.len(), 1);
        assert_eq!(authority.snapshot.fleet_outbox.len(), 1);
        assert_eq!(authority.snapshot.operations.len(), 1);
        let schedule = &authority.snapshot.schedules[&ScheduleId::new("schedule-once")];
        assert!(!schedule.enabled);
        assert_eq!(schedule.next_due_unix_ms, None);
    }

    #[test]
    fn webhook_delivery_is_idempotent_and_payload_conflicts_fail_closed() {
        let repository = Arc::new(MemoryRepository::default());
        let mut authority = BlackopsAuthority::open(repository).unwrap();
        let definition = authority
            .install_definition(DefinitionInstallRequest {
                kind: DefinitionKind::Atom,
                name: "incoming".into(),
                version: "1.0.0".into(),
                input_contract: json!({"type": "object"}),
                body: json!({}),
                activate: true,
                created_at_unix_ms: 1,
            })
            .unwrap();
        authority
            .put_schedule(ScheduleIntent {
                schedule_id: ScheduleId::new("hook-one"),
                name: "incoming hook".into(),
                invocation: InvocationTemplate {
                    definition: definition.key,
                    input: json!({"configured": true}),
                    execution: Some(scheduled_execution()),
                },
                trigger: ScheduleTrigger::Webhook {
                    path: "/incoming".into(),
                },
                enabled: true,
                next_due_unix_ms: None,
                generation: 1,
                created_at_unix_ms: 1,
                updated_at_unix_ms: 1,
            })
            .unwrap();
        let delivery = WebhookAdmissionRequest {
            delivery_id: "delivery-1".into(),
            payload: json!({"event": "created"}),
            occurred_at_unix_ms: 10,
        };
        let first = authority
            .admit_webhook("/incoming", delivery.clone())
            .unwrap();
        let retry = authority.admit_webhook("/incoming", delivery).unwrap();
        assert_eq!(first, retry);
        assert_eq!(authority.snapshot.invocations.len(), 1);
        assert_eq!(authority.snapshot.fleet_outbox.len(), 1);

        let conflict = authority
            .admit_webhook(
                "/incoming",
                WebhookAdmissionRequest {
                    delivery_id: "delivery-1".into(),
                    payload: json!({"event": "changed"}),
                    occurred_at_unix_ms: 11,
                },
            )
            .unwrap_err();
        assert!(matches!(conflict, BlackopsError::Conflict(_)));
    }

    #[test]
    fn workflow_retries_suspends_resumes_and_projects_terminal_intents() {
        let repository = Arc::new(MemoryRepository::default());
        let mut authority = BlackopsAuthority::open(repository).unwrap();
        let definition = authority
            .install_definition(DefinitionInstallRequest {
                kind: DefinitionKind::Workflow,
                name: "durable-review".into(),
                version: "1.0.0".into(),
                input_contract: json!({"type": "object"}),
                body: workflow_body(),
                activate: true,
                created_at_unix_ms: 1,
            })
            .unwrap();
        let invocation_id = InvocationId::new("workflow-one");
        authority
            .request_invocation(InvocationRequest {
                invocation_id: invocation_id.clone(),
                definition: definition.key,
                input: json!({"change": 7}),
                execution: None,
                requested_at_unix_ms: 10,
            })
            .unwrap();

        let first_operation = authority.snapshot.workflow_runs[&invocation_id].nodes["execute"]
            .operation_id
            .clone()
            .unwrap();
        authority
            .apply_execution_accepted(
                &first_operation,
                ExecutionAccepted {
                    operation_id: first_operation.clone(),
                    attempt_id: AttemptId::new("workflow-attempt-1"),
                    task_id: TaskId::new("workflow-task-1"),
                    session_id: SessionId::new("workflow-session-1"),
                    deduplicated: false,
                },
                11,
            )
            .unwrap();
        authority
            .observe_attempt(
                &first_operation,
                AttemptOutcome {
                    attempt_id: AttemptId::new("workflow-attempt-1"),
                    state: AttemptState::Failed,
                    result: json!({"error": "transient"}),
                },
                12,
            )
            .unwrap();
        let run = authority.workflow_run(&invocation_id).unwrap();
        assert_eq!(run.status, WorkflowRunStatus::RetryScheduled);
        assert_eq!(run.nodes["execute"].generation, 1);
        assert_eq!(
            run.nodes["execute"].operation_id.as_ref(),
            Some(&first_operation)
        );
        assert_eq!(run.nodes["execute"].next_attempt_unix_ms, Some(22));
        assert_eq!(authority.trigger_due_workflow_retries(21, 64).unwrap(), 0);
        assert_eq!(authority.trigger_due_workflow_retries(22, 64).unwrap(), 1);

        let retry_operation = authority.snapshot.workflow_runs[&invocation_id].nodes["execute"]
            .operation_id
            .clone()
            .unwrap();
        assert_eq!(retry_operation, first_operation);
        assert_eq!(
            authority.snapshot.operations[&retry_operation].kind,
            OperationKind::RunWorkflowNode {
                invocation_id: invocation_id.clone(),
                node_id: "execute".into(),
                generation: 1,
            }
        );
        authority
            .apply_execution_accepted(
                &retry_operation,
                ExecutionAccepted {
                    operation_id: retry_operation.clone(),
                    attempt_id: AttemptId::new("workflow-attempt-2"),
                    task_id: TaskId::new("workflow-task-2"),
                    session_id: SessionId::new("workflow-session-2"),
                    deduplicated: false,
                },
                23,
            )
            .unwrap();
        authority
            .observe_attempt(
                &retry_operation,
                AttemptOutcome {
                    attempt_id: AttemptId::new("workflow-attempt-2"),
                    state: AttemptState::Completed,
                    result: json!({"reviewed": true}),
                },
                24,
            )
            .unwrap();
        let waiting = authority.workflow_run(&invocation_id).unwrap();
        assert_eq!(waiting.status, WorkflowRunStatus::Waiting);
        assert_eq!(waiting.current_node, "wait");
        let wait_id = waiting.waiting_on.unwrap();
        authority
            .resolve_wait(
                &wait_id,
                WaitResolveRequest {
                    status: WaitStatus::Satisfied,
                    resolution: json!({"approved": true}),
                    resolved_at_unix_ms: 25,
                },
            )
            .unwrap();

        let terminal = authority.workflow_run(&invocation_id).unwrap();
        assert_eq!(terminal.status, WorkflowRunStatus::Completed);
        assert_eq!(
            authority.invocation(&invocation_id).unwrap().status,
            InvocationStatus::Completed
        );
        assert_eq!(terminal.integration_intent_ids.len(), 2);
        authority
            .resolve_wait(
                &wait_id,
                WaitResolveRequest {
                    status: WaitStatus::Satisfied,
                    resolution: json!({"approved": true}),
                    resolved_at_unix_ms: 25,
                },
            )
            .unwrap();
        assert_eq!(
            authority
                .workflow_run(&invocation_id)
                .unwrap()
                .integration_intent_ids
                .len(),
            2
        );
        assert!(
            authority
                .snapshot
                .record_outbox
                .values()
                .any(|record| record.kind == "workflow.terminal")
        );
        let intent_id = terminal.integration_intent_ids[0].clone();
        let resolved = authority
            .resolve_integration_intent(
                &intent_id,
                IntegrationIntentResolveRequest {
                    status: IntegrationIntentStatus::Completed,
                    result: json!({"external_id": "result-7"}),
                    resolved_at_unix_ms: 26,
                },
            )
            .unwrap();
        assert_eq!(resolved.status, IntegrationIntentStatus::Completed);
    }

    #[test]
    fn poll_cursor_deduplicates_stable_deliveries_before_materializing_effects() {
        let repository = Arc::new(MemoryRepository::default());
        let mut authority = BlackopsAuthority::open(repository).unwrap();
        let definition = authority
            .install_definition(DefinitionInstallRequest {
                kind: DefinitionKind::Atom,
                name: "poll-item".into(),
                version: "1.0.0".into(),
                input_contract: json!({"type": "object"}),
                body: json!({}),
                activate: true,
                created_at_unix_ms: 1,
            })
            .unwrap();
        let schedule_id = ScheduleId::new("poll-one");
        authority
            .put_schedule(ScheduleIntent {
                schedule_id: schedule_id.clone(),
                name: "local events".into(),
                invocation: InvocationTemplate {
                    definition: definition.key.clone(),
                    input: json!({"configured": true}),
                    execution: Some(scheduled_execution()),
                },
                trigger: ScheduleTrigger::Poll {
                    every_ms: 100,
                    source: PollSourceSpec {
                        contract_version: crate::POLL_SOURCE_CONTRACT_VERSION,
                        url: "http://127.0.0.1:7411/events".into(),
                        method: "GET".into(),
                        headers: BTreeMap::new(),
                        body: None,
                        timeout_ms: 1_000,
                        items_pointer: Some("/items".into()),
                        delivery_id_pointer: Some("/id".into()),
                        max_items: 4,
                    },
                },
                enabled: true,
                next_due_unix_ms: Some(10),
                generation: 1,
                created_at_unix_ms: 1,
                updated_at_unix_ms: 1,
            })
            .unwrap();
        let response = json!({"items": [{"id": "event-1", "value": 7}]});
        let first = authority
            .admit_poll_response(&schedule_id, 1, 10, response.clone(), 10)
            .unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(
            authority.snapshot.poll_cursors[&schedule_id].fetch_sequence,
            1
        );
        assert_eq!(
            authority.snapshot.poll_cursors[&schedule_id]
                .deliveries
                .len(),
            1
        );
        assert_eq!(authority.snapshot.fleet_outbox.len(), 1);

        let early = authority
            .admit_poll_response(&schedule_id, 1, 10, response.clone(), 11)
            .unwrap_err();
        assert!(matches!(early, BlackopsError::Conflict(_)));
        let duplicate = authority
            .admit_poll_response(&schedule_id, 1, 110, response, 110)
            .unwrap();
        assert!(duplicate.is_empty());
        assert_eq!(
            authority.snapshot.poll_cursors[&schedule_id].fetch_sequence,
            2
        );
        assert_eq!(authority.snapshot.fleet_outbox.len(), 1);

        let conflict = authority
            .admit_poll_response(
                &schedule_id,
                1,
                210,
                json!({"items": [{"id": "event-1", "value": 8}]}),
                210,
            )
            .unwrap_err();
        assert!(matches!(conflict, BlackopsError::Conflict(_)));
        assert_eq!(
            authority.snapshot.poll_cursors[&schedule_id].fetch_sequence,
            2
        );

        let remote = authority
            .put_schedule(ScheduleIntent {
                schedule_id: ScheduleId::new("remote-poll"),
                name: "remote".into(),
                invocation: InvocationTemplate {
                    definition: definition.key,
                    input: Value::Null,
                    execution: Some(scheduled_execution()),
                },
                trigger: ScheduleTrigger::Poll {
                    every_ms: 100,
                    source: PollSourceSpec {
                        contract_version: crate::POLL_SOURCE_CONTRACT_VERSION,
                        url: "http://example.com/events".into(),
                        method: "GET".into(),
                        headers: BTreeMap::new(),
                        body: None,
                        timeout_ms: 1_000,
                        items_pointer: None,
                        delivery_id_pointer: None,
                        max_items: 1,
                    },
                },
                enabled: true,
                next_due_unix_ms: Some(10),
                generation: 1,
                created_at_unix_ms: 1,
                updated_at_unix_ms: 1,
            })
            .unwrap_err();
        assert!(matches!(remote, BlackopsError::InvalidRequest(_)));
    }

    #[test]
    fn waits_approvals_and_whiteboards_are_durable_and_compare_and_swap() {
        let repository = Arc::new(MemoryRepository::default());
        let mut authority = BlackopsAuthority::open(repository).unwrap();
        authority
            .create_wait(WaitCreateRequest {
                wait_id: WaitId::new("wait-1"),
                topic: "review".into(),
                selector: json!({"change": 7}),
                deadline_unix_ms: Some(10),
                created_at_unix_ms: 1,
            })
            .unwrap();
        assert_eq!(authority.expire_waits(10, 256).unwrap(), 1);
        assert_eq!(authority.list_waits()[0].status, WaitStatus::TimedOut);

        let approval = authority
            .request_approval(ApprovalCreateRequest {
                approval_id: ApprovalId::new("approval-1"),
                requested_by: "operator".into(),
                action: json!({"deploy": true}),
                created_at_unix_ms: 2,
            })
            .unwrap();
        assert_eq!(approval.status, ApprovalStatus::Pending);
        let approval = authority
            .resolve_approval(
                &ApprovalId::new("approval-1"),
                ApprovalResolveRequest {
                    status: ApprovalStatus::Approved,
                    decision: json!({"reason": "verified"}),
                    resolved_at_unix_ms: 3,
                },
            )
            .unwrap();
        assert_eq!(approval.status, ApprovalStatus::Approved);

        authority
            .create_whiteboard(WhiteboardCreateRequest {
                whiteboard_id: WhiteboardId::new("board-1"),
                name: "campaign".into(),
                created_at_unix_ms: 4,
            })
            .unwrap();
        let board = authority
            .put_whiteboard_entry(
                &WhiteboardId::new("board-1"),
                "phase",
                WhiteboardPutRequest {
                    value: json!("implement"),
                    expected_revision: None,
                    updated_by: "agent".into(),
                    updated_at_unix_ms: 5,
                },
            )
            .unwrap();
        assert_eq!(board.entries["phase"].revision, 1);
        let stale = authority
            .put_whiteboard_entry(
                &WhiteboardId::new("board-1"),
                "phase",
                WhiteboardPutRequest {
                    value: json!("review"),
                    expected_revision: None,
                    updated_by: "agent".into(),
                    updated_at_unix_ms: 6,
                },
            )
            .unwrap_err();
        assert!(matches!(stale, BlackopsError::Conflict(_)));
    }
}
