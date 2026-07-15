use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use crate::clients::FleetControlCapability;
use crate::{AuthorityActor, BlackopsdResult};
use async_trait::async_trait;
use blackops_core::{
    AgentRecord, DefinitionKey, DefinitionKind, FleetEffectKind, FollowupAgentRequest,
    InterruptAgentRequest, InvocationId, InvocationRequest, InvocationStatus, LogicalAgentStatus,
    MailboxMessageKind, OperationStatus, PollSourceSpec, ScheduleTrigger, SendMessageRequest,
    SpawnAgentRequest,
};
use bro_capabilities::{
    AgentCapability, AgentForkTurns, AgentIdentity, AgentMessageRequest, AgentSpawnRequest,
    AgentStatus, AgentSummary, AgentTarget, AgentWaitRequest, AgentWake, AtomCapability,
    AtomInvocation, AtomOutput, ExecutionCapability, ExecutionCodeMode, ExecutionDispatchContext,
    ExecutionKind, ExecutionRequest, ExecutionScope, ExecutionServiceTier, ExecutionToolPolicy,
    RecordEnvelope, RecordIngestCapability, RecordIngestRequest, WorkingSetIntent,
};
use bro_core::{AgentId, AttemptId, BroError, OperationId, Provider, SessionId, TaskId};
use bro_protocol::{
    AgentMailboxDelivery, AgentMailboxDeliveryState, AgentMailboxMessage, AgentMailboxMessageKind,
    SessionCapabilityPolicy,
};
use serde::{Deserialize, Serialize};

use crate::catalog::{
    AtomBackend, ResolvedAtomDefinition, resolve_atom_definition, validate_value,
};

const RECORD_BATCH_COUNT_LIMIT: usize = 128;
// blackbox-corpusd admits 2 MiB HTTP bodies. Keep transport headroom while
// bounding on exact compact-JSON bytes, because resolved catalog artifacts
// vary substantially in size and a count-only limit can wedge the outbox.
const RECORD_BATCH_BODY_BUDGET: usize = 2 * 1024 * 1024 - 64 * 1024;
const EMPTY_RECORD_REQUEST_BYTES: usize = br#"{"records":[]}"#.len();

#[derive(Debug, Clone)]
pub struct ExecutionProfile {
    pub provider: Provider,
    pub model: String,
}

#[derive(Clone)]
pub struct BlackopsRuntime {
    authority: AuthorityActor,
    execution: Arc<dyn ExecutionCapability>,
    control: Arc<dyn FleetControlCapability>,
    records: Arc<dyn RecordIngestCapability>,
    profile: ExecutionProfile,
    poll_client: reqwest::Client,
    build_id: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconcileReport {
    pub schedules_triggered: usize,
    pub poll_sources_fetched: usize,
    pub poll_deliveries_admitted: usize,
    pub workflow_retries_started: usize,
    pub waits_expired: usize,
    pub submitted: usize,
    pub accepted: usize,
    pub interrupted: usize,
    pub outcomes_observed: usize,
    pub terminal: usize,
    pub records_published: usize,
    #[serde(default)]
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeStatus {
    pub service: String,
    pub version: String,
    pub build_id: String,
    pub generation: u64,
    pub agents: usize,
    pub teams: usize,
    pub operations_requested: usize,
    pub operations_accepted: usize,
    pub operations_terminal: usize,
    pub pending_fleet_effects: usize,
    pub pending_records: usize,
    pub definitions: usize,
    pub invocations: usize,
    pub workflow_runs: usize,
    pub schedules: usize,
    pub poll_cursors: usize,
    pub integration_intents: usize,
    pub waits: usize,
    pub approvals: usize,
    pub whiteboards: usize,
}

impl BlackopsRuntime {
    pub async fn open(
        state_root: impl Into<std::path::PathBuf>,
        execution: Arc<dyn ExecutionCapability>,
        control: Arc<dyn FleetControlCapability>,
        records: Arc<dyn RecordIngestCapability>,
        profile: ExecutionProfile,
        build_id: impl Into<String>,
    ) -> BlackopsdResult<Self> {
        let authority = AuthorityActor::open(state_root.into()).await?;
        let poll_client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            authority,
            execution,
            control,
            records,
            profile,
            poll_client,
            build_id: build_id.into(),
        })
    }

    pub fn authority(&self) -> AuthorityActor {
        self.authority.clone()
    }

    pub fn build_id(&self) -> &str {
        &self.build_id
    }

    pub async fn status(&self) -> BlackopsdResult<RuntimeStatus> {
        let snapshot = self.authority.snapshot().await?;
        Ok(RuntimeStatus {
            service: "blackopsd".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            build_id: self.build_id.clone(),
            generation: snapshot.generation,
            agents: snapshot.agents.len(),
            teams: snapshot.teams.len(),
            operations_requested: snapshot
                .operations
                .values()
                .filter(|operation| operation.status == OperationStatus::Requested)
                .count(),
            operations_accepted: snapshot
                .operations
                .values()
                .filter(|operation| operation.status == OperationStatus::Accepted)
                .count(),
            operations_terminal: snapshot
                .operations
                .values()
                .filter(|operation| operation.status.is_terminal())
                .count(),
            pending_fleet_effects: snapshot.fleet_outbox.len(),
            pending_records: snapshot.record_outbox.len(),
            definitions: snapshot.definitions.len(),
            invocations: snapshot.invocations.len(),
            workflow_runs: snapshot.workflow_runs.len(),
            schedules: snapshot.schedules.len(),
            poll_cursors: snapshot.poll_cursors.len(),
            integration_intents: snapshot.integration_intents.len(),
            waits: snapshot.waits.len(),
            approvals: snapshot.approvals.len(),
            whiteboards: snapshot.whiteboards.len(),
        })
    }

    pub fn session_agents(
        &self,
        worker_id: impl Into<String>,
        session_id: SessionId,
        invocation_id: impl Into<String>,
    ) -> SessionAgentCapability {
        let binding = SessionAttemptBinding::local(&session_id);
        SessionAgentCapability {
            runtime: self.clone(),
            worker_id: worker_id.into(),
            session_id,
            binding,
            invocation_id: invocation_id.into(),
            inherited_capability_policy: None,
        }
    }

    pub(crate) fn session_agents_with_policy(
        &self,
        worker_id: impl Into<String>,
        session_id: SessionId,
        binding: SessionAttemptBinding,
        invocation_id: impl Into<String>,
        capability_policy: SessionCapabilityPolicy,
    ) -> SessionAgentCapability {
        SessionAgentCapability {
            runtime: self.clone(),
            worker_id: worker_id.into(),
            session_id,
            binding,
            invocation_id: invocation_id.into(),
            inherited_capability_policy: Some(capability_policy),
        }
    }

    pub fn session_atoms(
        &self,
        worker_id: impl Into<String>,
        session_id: SessionId,
        invocation_id: impl Into<String>,
    ) -> SessionAtomCapability {
        self.session_atoms_until(
            worker_id,
            session_id,
            invocation_id,
            Some(now_ms().saturating_add(30_000)),
        )
    }

    pub fn session_atoms_until(
        &self,
        worker_id: impl Into<String>,
        session_id: SessionId,
        invocation_id: impl Into<String>,
        deadline_unix_ms: Option<u64>,
    ) -> SessionAtomCapability {
        let binding = SessionAttemptBinding::local(&session_id);
        SessionAtomCapability {
            runtime: self.clone(),
            worker_id: worker_id.into(),
            session_id,
            binding,
            invocation_id: invocation_id.into(),
            deadline_unix_ms,
            inherited_capability_policy: None,
        }
    }

    pub(crate) fn session_atoms_until_with_policy(
        &self,
        worker_id: impl Into<String>,
        session_id: SessionId,
        binding: SessionAttemptBinding,
        invocation_id: impl Into<String>,
        deadline_unix_ms: Option<u64>,
        capability_policy: SessionCapabilityPolicy,
    ) -> SessionAtomCapability {
        SessionAtomCapability {
            runtime: self.clone(),
            worker_id: worker_id.into(),
            session_id,
            binding,
            invocation_id: invocation_id.into(),
            deadline_unix_ms,
            inherited_capability_policy: Some(capability_policy),
        }
    }

    pub async fn drive_once(&self) -> ReconcileReport {
        let mut report = ReconcileReport::default();
        let retry_now = now_ms();
        match self
            .authority
            .call(move |authority| authority.trigger_due_workflow_retries(retry_now, 64))
            .await
        {
            Ok(started) => report.workflow_retries_started = started,
            Err(error) => report.errors.push(error.to_string()),
        }
        let poll_now = now_ms();
        let due_polls = match self
            .authority
            .call(move |authority| authority.due_poll_schedules(poll_now, 8))
            .await
        {
            Ok(schedules) => schedules,
            Err(error) => {
                report.errors.push(error.to_string());
                Vec::new()
            }
        };
        for schedule in due_polls {
            let source = match &schedule.trigger {
                ScheduleTrigger::Poll { source, .. } => source,
                _ => continue,
            };
            match self.fetch_poll_source(source).await {
                Ok(response) => {
                    report.poll_sources_fetched += 1;
                    let schedule_id = schedule.schedule_id.clone();
                    let generation = schedule.generation;
                    let Some(due_at_unix_ms) = schedule.next_due_unix_ms else {
                        report.errors.push(format!(
                            "poll source {} has no due occurrence",
                            schedule.schedule_id
                        ));
                        continue;
                    };
                    match self
                        .authority
                        .call(move |authority| {
                            authority.admit_poll_response(
                                &schedule_id,
                                generation,
                                due_at_unix_ms,
                                response,
                                now_ms(),
                            )
                        })
                        .await
                    {
                        Ok(invocations) => {
                            report.poll_deliveries_admitted += invocations.len();
                        }
                        Err(error) => report.errors.push(error.to_string()),
                    }
                }
                Err(error) => report.errors.push(format!(
                    "poll source {} failed: {error}",
                    schedule.schedule_id
                )),
            }
        }
        let schedule_now = now_ms();
        match self
            .authority
            .call(move |authority| authority.trigger_due_schedules(schedule_now, 64))
            .await
        {
            Ok(invocations) => report.schedules_triggered = invocations.len(),
            Err(error) => report.errors.push(error.to_string()),
        }
        let wait_now = now_ms();
        match self
            .authority
            .call(move |authority| authority.expire_waits(wait_now, 256))
            .await
        {
            Ok(expired) => report.waits_expired = expired,
            Err(error) => report.errors.push(error.to_string()),
        }
        let mailbox_effects = match self
            .authority
            .call(|authority| Ok(authority.pending_mailbox_delivery_effects()))
            .await
        {
            Ok(effects) => effects,
            Err(error) => {
                report.errors.push(error.to_string());
                Vec::new()
            }
        };
        for effect in mailbox_effects {
            let delivery = AgentMailboxDelivery {
                delivery_id: effect.delivery_id.clone(),
                target_agent_id: effect.recipient.clone(),
                canonical_target: effect.canonical_target.clone(),
                session_id: effect.session_id.clone(),
                through_sequence: effect.through_sequence,
                wake: effect.wake,
                messages: vec![AgentMailboxMessage {
                    message_id: effect.message.message_id.clone(),
                    sequence: effect.message.sequence,
                    sender: effect.message.sender.clone(),
                    recipient: effect.message.recipient.clone(),
                    kind: match effect.message.kind {
                        MailboxMessageKind::Send => AgentMailboxMessageKind::Send,
                        MailboxMessageKind::Followup => AgentMailboxMessageKind::Followup,
                        MailboxMessageKind::System => AgentMailboxMessageKind::System,
                    },
                    body: effect.message.body.clone(),
                    created_at_unix_ms: effect.message.created_at_unix_ms,
                }],
            };
            let effect_id = effect.effect_id.clone();
            match self.control.deliver_agent_mailbox(delivery).await {
                Ok(receipt)
                    if receipt.delivery_id == effect.delivery_id
                        && receipt.target_agent_id == effect.recipient
                        && receipt.canonical_target == effect.canonical_target
                        && receipt.session_id == effect.session_id
                        && receipt.through_sequence == effect.through_sequence
                        && receipt.state == AgentMailboxDeliveryState::Admitted =>
                {
                    let delivery_id = receipt.delivery_id;
                    let through_sequence = receipt.through_sequence;
                    if let Err(error) = self
                        .authority
                        .call(move |authority| {
                            authority.acknowledge_mailbox_delivery(
                                &effect_id,
                                &delivery_id,
                                through_sequence,
                                now_ms(),
                            )
                        })
                        .await
                    {
                        report.errors.push(error.to_string());
                    }
                }
                Ok(receipt)
                    if receipt.delivery_id == effect.delivery_id
                        && receipt.target_agent_id == effect.recipient
                        && receipt.canonical_target == effect.canonical_target
                        && receipt.session_id == effect.session_id
                        && receipt.through_sequence == effect.through_sequence
                        && receipt.state == AgentMailboxDeliveryState::Pending => {}
                Ok(receipt) => {
                    let error = if receipt.state == AgentMailboxDeliveryState::Rejected {
                        receipt
                            .error
                            .unwrap_or_else(|| "worker rejected mailbox delivery".into())
                    } else {
                        "fleet mailbox receipt differs from the durable delivery effect".into()
                    };
                    let error_for_state = error.clone();
                    let _ = self
                        .authority
                        .call(move |authority| {
                            authority.note_mailbox_delivery_error(&effect_id, error_for_state)
                        })
                        .await;
                    report.errors.push(error);
                }
                Err(error) => {
                    let rendered = format!("{}: {}", error.code, error.message);
                    let error_for_state = rendered.clone();
                    let _ = self
                        .authority
                        .call(move |authority| {
                            authority.note_mailbox_delivery_error(&effect_id, error_for_state)
                        })
                        .await;
                    report.errors.push(rendered);
                }
            }
        }
        let effects = match self
            .authority
            .call(|authority| Ok(authority.pending_fleet_effects()))
            .await
        {
            Ok(effects) => effects,
            Err(error) => {
                report.errors.push(error.to_string());
                return report;
            }
        };
        for effect in effects {
            report.submitted += 1;
            let operation_id = effect.operation_id.clone();
            let effect_id = effect.effect_id.clone();
            let result = match effect.effect {
                FleetEffectKind::RequestExecution { request } => {
                    match self.execution.request_execution(*request).await {
                        Ok(accepted) => {
                            let applied = self
                                .authority
                                .call(move |authority| {
                                    authority.apply_execution_accepted(
                                        &operation_id,
                                        accepted,
                                        now_ms(),
                                    )
                                })
                                .await;
                            match applied {
                                Ok(_) => {
                                    report.accepted += 1;
                                    Ok(())
                                }
                                Err(error) => Err(error.to_string()),
                            }
                        }
                        Err(error) => Err(format!("{}: {}", error.code, error.message)),
                    }
                }
                FleetEffectKind::InterruptTask { task_id } => {
                    match self.control.interrupt_task(task_id).await {
                        Ok(result) => {
                            let applied = self
                                .authority
                                .call(move |authority| {
                                    authority.apply_interrupt_accepted(
                                        &operation_id,
                                        result,
                                        now_ms(),
                                    )
                                })
                                .await;
                            match applied {
                                Ok(_) => {
                                    report.interrupted += 1;
                                    Ok(())
                                }
                                Err(error) => Err(error.to_string()),
                            }
                        }
                        Err(error) => Err(format!("{}: {}", error.code, error.message)),
                    }
                }
            };
            if let Err(error) = result {
                let error_for_state = error.clone();
                let _ = self
                    .authority
                    .call(move |authority| {
                        authority.note_fleet_effect_error(&effect_id, error_for_state, now_ms())
                    })
                    .await;
                report.errors.push(error);
            }
        }

        let targets = match self
            .authority
            .call(|authority| Ok(authority.reconciliation_targets()))
            .await
        {
            Ok(targets) => targets,
            Err(error) => {
                report.errors.push(error.to_string());
                return report;
            }
        };
        for target in targets {
            match self.execution.attempt_outcome(target.attempt_id).await {
                Ok(outcome) => {
                    let terminal = matches!(
                        outcome.state,
                        bro_capabilities::AttemptState::Completed
                            | bro_capabilities::AttemptState::Failed
                            | bro_capabilities::AttemptState::Interrupted
                            | bro_capabilities::AttemptState::Lost
                    );
                    let operation_id = target.operation_id;
                    match self
                        .authority
                        .call(move |authority| {
                            authority.observe_attempt(&operation_id, outcome, now_ms())
                        })
                        .await
                    {
                        Ok(_) => {
                            report.outcomes_observed += 1;
                            if terminal {
                                report.terminal += 1;
                            }
                        }
                        Err(error) => report.errors.push(error.to_string()),
                    }
                }
                Err(error) => report
                    .errors
                    .push(format!("{}: {}", error.code, error.message)),
            }
        }

        let records = match self
            .authority
            .call(|authority| Ok(authority.pending_records(RECORD_BATCH_COUNT_LIMIT)))
            .await
        {
            Ok(records) => records,
            Err(error) => {
                report.errors.push(error.to_string());
                return report;
            }
        };
        let records = match bounded_record_batch(records) {
            Ok(records) => records,
            Err(error) => {
                report.errors.push(error);
                return report;
            }
        };
        if !records.is_empty() {
            let record_ids: Vec<_> = records
                .iter()
                .map(|record| record.record_id.clone())
                .collect();
            match self
                .records
                .ingest_records(RecordIngestRequest { records })
                .await
            {
                Ok(receipt) if receipt.accepted + receipt.deduplicated == record_ids.len() => {
                    match self
                        .authority
                        .call(move |authority| authority.acknowledge_records(&record_ids))
                        .await
                    {
                        Ok(count) => report.records_published += count,
                        Err(error) => report.errors.push(error.to_string()),
                    }
                }
                Ok(receipt) => report.errors.push(format!(
                    "record ingest receipt covered {} of {} records",
                    receipt.accepted + receipt.deduplicated,
                    record_ids.len()
                )),
                Err(error) => report
                    .errors
                    .push(format!("{}: {}", error.code, error.message)),
            }
        }
        report
    }

    async fn fetch_poll_source(
        &self,
        source: &PollSourceSpec,
    ) -> Result<serde_json::Value, String> {
        const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
        let method = reqwest::Method::from_bytes(source.method.as_bytes())
            .map_err(|error| format!("invalid method: {error}"))?;
        let mut request = self
            .poll_client
            .request(method, &source.url)
            .timeout(Duration::from_millis(source.timeout_ms))
            .header(reqwest::header::ACCEPT, "application/json");
        for (name, value) in &source.headers {
            request = request.header(name, value);
        }
        if let Some(body) = &source.body {
            request = request.json(body);
        }
        let mut response = request.send().await.map_err(|error| error.to_string())?;
        if !response.status().is_success() {
            return Err(format!("HTTP {}", response.status()));
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
        {
            return Err("response exceeds 1 MiB".into());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|error| error.to_string())? {
            if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                return Err("response exceeds 1 MiB".into());
            }
            bytes.extend_from_slice(&chunk);
        }
        if bytes.is_empty() {
            return Ok(serde_json::Value::Null);
        }
        serde_json::from_slice(&bytes).map_err(|error| format!("response is not JSON: {error}"))
    }
}

fn bounded_record_batch(mut records: Vec<RecordEnvelope>) -> Result<Vec<RecordEnvelope>, String> {
    let mut body_bytes = EMPTY_RECORD_REQUEST_BYTES;
    let mut selected = 0usize;
    for record in records.iter().take(RECORD_BATCH_COUNT_LIMIT) {
        let record_bytes = serde_json::to_vec(record)
            .map_err(|error| format!("serializing record {} failed: {error}", record.record_id))?
            .len();
        let separator_bytes = usize::from(selected > 0);
        let candidate_bytes = body_bytes
            .saturating_add(separator_bytes)
            .saturating_add(record_bytes);
        if candidate_bytes > RECORD_BATCH_BODY_BUDGET {
            if selected == 0 {
                return Err(format!(
                    "record {} needs {candidate_bytes} request bytes; the bounded corpus transport budget is {RECORD_BATCH_BODY_BUDGET}",
                    record.record_id
                ));
            }
            break;
        }
        body_bytes = candidate_bytes;
        selected += 1;
    }
    records.truncate(selected);
    Ok(records)
}

#[derive(Clone)]
pub(crate) struct SessionAttemptBinding {
    task_id: TaskId,
    attempt_id: AttemptId,
    generation: u64,
}

impl SessionAttemptBinding {
    pub(crate) fn authenticated(task_id: TaskId, attempt_id: AttemptId, generation: u64) -> Self {
        Self {
            task_id,
            attempt_id,
            generation,
        }
    }

    fn local(session_id: &SessionId) -> Self {
        Self {
            task_id: TaskId::new(format!("blackops-local-task:{session_id}")),
            attempt_id: AttemptId::new(format!("blackops-local-attempt:{session_id}")),
            generation: 1,
        }
    }
}

#[derive(Clone)]
pub struct SessionAtomCapability {
    runtime: BlackopsRuntime,
    worker_id: String,
    session_id: SessionId,
    binding: SessionAttemptBinding,
    invocation_id: String,
    deadline_unix_ms: Option<u64>,
    inherited_capability_policy: Option<SessionCapabilityPolicy>,
}

#[derive(Debug, Deserialize)]
struct CatalogBrofile {
    name: String,
    provider: Provider,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    effort: Option<String>,
    #[serde(default)]
    lens: Option<String>,
    #[serde(default)]
    filters: Option<CatalogToolFilters>,
    #[serde(default)]
    code_mode: Option<ExecutionCodeMode>,
    #[serde(default)]
    service_tier: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct CatalogToolFilters {
    #[serde(default)]
    allow: Vec<String>,
    #[serde(default)]
    disallow: Vec<String>,
    #[serde(default)]
    allowed_remote_operations: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    allowed_atom_refs: Vec<bro_core::AtomRef>,
}

#[async_trait]
impl AtomCapability for SessionAtomCapability {
    async fn invoke_atom(&self, invocation: AtomInvocation) -> Result<AtomOutput, BroError> {
        self.invoke_atom_inner(invocation).await
    }

    async fn invoke_atom_for_invocation(
        &self,
        invocation_id: &str,
        invocation: AtomInvocation,
    ) -> Result<AtomOutput, BroError> {
        let mut scoped = self.clone();
        scoped.invocation_id = invocation_id.to_owned();
        scoped.invoke_atom_inner(invocation).await
    }
}

impl SessionAtomCapability {
    async fn invoke_atom_inner(&self, invocation: AtomInvocation) -> Result<AtomOutput, BroError> {
        if self
            .inherited_capability_policy
            .as_ref()
            .is_some_and(|policy| !policy.allows_atom_ref(&invocation.atom))
        {
            return Err(BroError::new(
                "atom.unauthorized_ref",
                format!(
                    "session capability policy does not grant atom {}",
                    invocation.atom
                ),
            ));
        }
        let atom_ref = invocation.atom.as_str();
        let reference = atom_ref.strip_prefix("atom:").ok_or_else(|| {
            BroError::new(
                "atom.invalid_ref",
                "atom ref must use the atom:<name>@<version> form",
            )
        })?;
        let (name, version) = reference.rsplit_once('@').ok_or_else(|| {
            BroError::new(
                "atom.invalid_ref",
                "atom ref must contain an exact @version",
            )
        })?;
        if name.trim().is_empty() || version.trim().is_empty() {
            return Err(BroError::new(
                "atom.invalid_ref",
                "atom name and version must not be empty",
            ));
        }
        let definition_key = DefinitionKey {
            kind: DefinitionKind::Atom,
            name: name.to_string(),
            version: version.to_string(),
        };
        let lookup_key = definition_key.clone();
        let definition = self
            .runtime
            .authority
            .call(move |authority| {
                authority
                    .list_definitions()
                    .into_iter()
                    .find(|definition| definition.key == lookup_key)
                    .ok_or_else(|| {
                        blackops_core::BlackopsError::NotFound(format!(
                            "atom definition {}:{}",
                            lookup_key.name, lookup_key.version
                        ))
                    })
            })
            .await
            .map_err(atom_service_error)?;
        let stable = format!("atom:{}:{}", self.session_id, self.invocation_id);
        let operation_id = OperationId::new(format!("operation-{stable}"));
        let invocation_id = InvocationId::new(format!("invocation-{stable}"));
        let mut labels = BTreeMap::new();
        labels.insert("atom_ref".into(), invocation.atom.to_string());
        labels.insert("worker_id".into(), self.worker_id.clone());
        labels.insert("session_id".into(), self.session_id.to_string());
        labels.insert("provider_invocation_id".into(), self.invocation_id.clone());
        let resolved = resolve_atom_definition(&definition).ok();
        let input = if invocation.input_json.is_null() {
            serde_json::json!({})
        } else {
            invocation.input_json.clone()
        };
        if let Some(resolved) = &resolved {
            validate_value(resolved.input_schema.as_ref(), &input)
                .map_err(|message| BroError::new("atom.schema_validation_failed", message))?;
            match &resolved.backend {
                AtomBackend::Deterministic { runner } => {
                    let output = run_deterministic_atom(runner, &input)?;
                    return self
                        .complete_local_atom(
                            invocation_id,
                            definition_key,
                            input,
                            output,
                            resolved.output_schema.as_ref(),
                        )
                        .await;
                }
                AtomBackend::Adapter { adapter_name } => {
                    let output = run_adapter_atom(adapter_name, &input)?;
                    return self
                        .complete_local_atom(
                            invocation_id,
                            definition_key,
                            input,
                            output,
                            resolved.output_schema.as_ref(),
                        )
                        .await;
                }
                AtomBackend::Workflow {
                    workflow_ref,
                    workflow,
                } => {
                    let output = self
                        .execute_catalog_workflow(workflow_ref, workflow, &input, resolved)
                        .await?;
                    return self
                        .complete_local_atom(
                            invocation_id,
                            definition_key,
                            input,
                            output,
                            resolved.output_schema.as_ref(),
                        )
                        .await;
                }
                AtomBackend::Consultant { consumer } => {
                    let output = self.execute_consultant(consumer, &input).await?;
                    return self
                        .complete_local_atom(
                            invocation_id,
                            definition_key,
                            input,
                            output,
                            resolved.output_schema.as_ref(),
                        )
                        .await;
                }
                AtomBackend::Profile { .. } => {}
            }
        }
        let input_json = serde_json::to_string(&input)
            .map_err(|error| BroError::new("atom.invalid_input", error.to_string()))?;
        let mut execution = self.execution_for_atom(
            resolved.as_ref(),
            operation_id,
            stable.clone(),
            &input,
            &input_json,
            labels,
            &definition,
            &invocation,
        )?;
        if let Some(policy) = &self.inherited_capability_policy {
            narrow_remote_authority(&mut execution.tool_policy, policy);
        }
        let request = InvocationRequest {
            invocation_id: invocation_id.clone(),
            definition: definition_key,
            input,
            execution: Some(execution),
            requested_at_unix_ms: now_ms(),
        };
        let intent = self
            .runtime
            .authority
            .call(move |authority| authority.request_invocation(request))
            .await
            .map_err(atom_service_error)?;
        let deadline = self
            .deadline_unix_ms
            .unwrap_or_else(|| now_ms().saturating_add(30_000))
            .min(now_ms().saturating_add(300_000));
        loop {
            let invocation_id = intent.invocation_id.clone();
            let (invocation, operation) = self
                .runtime
                .authority
                .call(move |authority| {
                    let invocation = authority.invocation(&invocation_id)?;
                    let operation = invocation
                        .operation_id
                        .as_ref()
                        .map(|operation_id| authority.operation(operation_id))
                        .transpose()?;
                    Ok((invocation, operation))
                })
                .await
                .map_err(atom_service_error)?;
            match invocation.status {
                InvocationStatus::Completed => {
                    let result = invocation
                        .output
                        .or_else(|| {
                            operation
                                .and_then(|operation| operation.terminal)
                                .map(|terminal| terminal.result)
                        })
                        .ok_or_else(|| {
                            BroError::new(
                                "atom.invalid_state",
                                "completed atom invocation has no terminal output",
                            )
                        })?;
                    return Ok(AtomOutput {
                        output_json: result,
                    });
                }
                InvocationStatus::Failed => {
                    let result = operation
                        .and_then(|operation| operation.terminal)
                        .map(|terminal| terminal.result)
                        .unwrap_or(serde_json::Value::Null);
                    return Err(BroError::new(
                        "atom.execution_failed",
                        format!("atom execution failed: {result}"),
                    ));
                }
                InvocationStatus::Requested | InvocationStatus::Running => {}
            }
            let now = now_ms();
            if now >= deadline {
                return Err(BroError::new(
                    "atom.deadline_exceeded",
                    format!("atom invocation {} is still running", intent.invocation_id),
                ));
            }
            let remaining = Duration::from_millis(deadline.saturating_sub(now).max(1));
            if tokio::time::timeout(remaining, self.runtime.drive_once())
                .await
                .is_err()
            {
                return Err(BroError::new(
                    "atom.deadline_exceeded",
                    format!("atom invocation {} is still running", intent.invocation_id),
                ));
            }
            tokio::time::sleep(Duration::from_millis(25).min(remaining)).await;
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn execution_for_atom(
        &self,
        resolved: Option<&ResolvedAtomDefinition>,
        operation_id: OperationId,
        idempotency_key: String,
        input: &serde_json::Value,
        input_json: &str,
        labels: BTreeMap<String, String>,
        definition: &blackops_core::OperationalDefinition,
        invocation: &AtomInvocation,
    ) -> Result<ExecutionRequest, BroError> {
        if let Some(ResolvedAtomDefinition {
            backend:
                AtomBackend::Profile {
                    brofile_ref,
                    brofile,
                },
            prompt_template,
            output_schema,
            ..
        }) = resolved
        {
            let profile: CatalogBrofile =
                serde_json::from_value(brofile.clone()).map_err(|error| {
                    BroError::new(
                        "atom.invalid_profile",
                        format!("{brofile_ref} is invalid: {error}"),
                    )
                })?;
            if !profile.provider.is_dispatchable() {
                return Err(BroError::new(
                    "atom.invalid_profile",
                    format!("{brofile_ref} selects a non-dispatchable provider"),
                ));
            }
            let prompt = prompt_template
                .as_deref()
                .map(|template| expand_atom_template(template, input))
                .unwrap_or_else(|| input_json.to_string());
            let filters = profile.filters.unwrap_or_default();
            let project_dir = input
                .get("project_dir")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned);
            let working_set = project_dir
                .as_ref()
                .map(|cwd| WorkingSetIntent::Existing {
                    cwd: cwd.clone(),
                    managed_worktree: false,
                })
                .unwrap_or(WorkingSetIntent::Scratch);
            let service_tier = match profile.service_tier.as_deref() {
                None | Some("default") => ExecutionServiceTier::Default,
                Some("priority") => ExecutionServiceTier::Priority,
                Some("flex") => ExecutionServiceTier::Flex,
                Some(other) => {
                    return Err(BroError::new(
                        "atom.invalid_profile",
                        format!("{brofile_ref} has unsupported service tier {other}"),
                    ));
                }
            };
            return Ok(ExecutionRequest {
                operation_id,
                idempotency_key,
                kind: ExecutionKind::Fresh { prompt },
                provider: profile.provider,
                model: profile
                    .model
                    .unwrap_or_else(|| self.runtime.profile.model.clone()),
                effort: profile.effort,
                service_tier,
                code_mode: profile.code_mode,
                dispatch_context: Some(ExecutionDispatchContext {
                    persona: Some(profile.name),
                    directives: Vec::new(),
                    scope: ExecutionScope {
                        project: project_dir,
                        root_session_id: Some(self.session_id.clone()),
                        ..ExecutionScope::default()
                    },
                    pins: None,
                }),
                working_set,
                shell_env: BTreeMap::new(),
                tool_policy: ExecutionToolPolicy {
                    allow_tools: filters.allow,
                    deny_tools: filters.disallow,
                    allowed_remote_operations: filters.allowed_remote_operations,
                    allowed_atom_refs: filters.allowed_atom_refs,
                    ..ExecutionToolPolicy::default()
                },
                system_prompt: profile.lens,
                output_schema: output_schema.clone(),
                labels,
            });
        }

        let configured_prompt = definition
            .body
            .get("prompt")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| format!("Execute the catalog atom {}", invocation.atom));
        Ok(ExecutionRequest {
            operation_id,
            idempotency_key,
            kind: ExecutionKind::Fresh {
                prompt: format!("{configured_prompt}\n\nAtom input:\n{input_json}"),
            },
            provider: self.runtime.profile.provider,
            model: self.runtime.profile.model.clone(),
            effort: None,
            service_tier: ExecutionServiceTier::Default,
            code_mode: None,
            dispatch_context: None,
            working_set: WorkingSetIntent::Scratch,
            shell_env: BTreeMap::new(),
            tool_policy: ExecutionToolPolicy::default(),
            system_prompt: definition
                .body
                .get("system_prompt")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            output_schema: definition.body.get("output_schema").cloned(),
            labels,
        })
    }

    async fn complete_local_atom(
        &self,
        invocation_id: InvocationId,
        definition: DefinitionKey,
        input: serde_json::Value,
        output: serde_json::Value,
        output_schema: Option<&serde_json::Value>,
    ) -> Result<AtomOutput, BroError> {
        validate_value(output_schema, &output)
            .map_err(|message| BroError::new("atom.invalid_output", message))?;
        let output_for_store = output.clone();
        self.runtime
            .authority
            .call(move |authority| {
                authority.complete_local_invocation(
                    InvocationRequest {
                        invocation_id,
                        definition,
                        input,
                        execution: None,
                        requested_at_unix_ms: now_ms(),
                    },
                    output_for_store,
                )
            })
            .await
            .map_err(atom_service_error)?;
        Ok(AtomOutput {
            output_json: output,
        })
    }

    async fn execute_catalog_workflow(
        &self,
        workflow_ref: &str,
        workflow: &serde_json::Value,
        input: &serde_json::Value,
        definition: &ResolvedAtomDefinition,
    ) -> Result<serde_json::Value, BroError> {
        let bindings = workflow
            .get("atom_bindings")
            .and_then(serde_json::Value::as_object)
            .ok_or_else(|| {
                BroError::new(
                    "atom.unsupported_workflow",
                    format!("{workflow_ref} is not an atom-binding workflow"),
                )
            })?;
        let nodes = workflow
            .get("nodes")
            .and_then(serde_json::Value::as_object)
            .ok_or_else(|| BroError::new("atom.invalid_workflow", "workflow nodes are missing"))?;
        let mut node_id = workflow
            .get("start")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| BroError::new("atom.invalid_workflow", "workflow start is missing"))?
            .to_owned();
        let mut context = serde_json::json!({"vars": input, "nodes": {}});
        for step in 0..256_u16 {
            let node = nodes.get(&node_id).ok_or_else(|| {
                BroError::new(
                    "atom.invalid_workflow",
                    format!("workflow node {node_id} does not exist"),
                )
            })?;
            let binding_name = node
                .get("atom")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    BroError::new(
                        "atom.unsupported_workflow",
                        format!("workflow node {node_id} is not an atom node"),
                    )
                })?;
            let atom_ref = bindings
                .get(binding_name)
                .and_then(|binding| binding.get("atom_ref"))
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    BroError::new(
                        "atom.invalid_workflow",
                        format!("workflow binding {binding_name} has no atom_ref"),
                    )
                })?;
            ensure_workflow_child_allowed(definition.composition.as_ref(), atom_ref)?;
            let args = interpolate_workflow_value(
                node.get("atom_args").unwrap_or(&serde_json::Value::Null),
                &context,
            );
            let child_invocation_id = format!(
                "{}:workflow:{workflow_ref}:{step}:{node_id}",
                self.invocation_id
            );
            let child = match &self.inherited_capability_policy {
                Some(policy) => self.runtime.session_atoms_until_with_policy(
                    self.worker_id.clone(),
                    self.session_id.clone(),
                    self.binding.clone(),
                    child_invocation_id,
                    self.deadline_unix_ms,
                    policy.clone(),
                ),
                None => self.runtime.session_atoms_until(
                    self.worker_id.clone(),
                    self.session_id.clone(),
                    child_invocation_id,
                    self.deadline_unix_ms,
                ),
            };
            let parsed_ref = bro_core::AtomRef::new(atom_ref);
            let last_output = child
                .invoke_atom(AtomInvocation {
                    atom: parsed_ref,
                    input_json: args,
                })
                .await?
                .output_json;
            if let Some(node_outputs) = context
                .get_mut("nodes")
                .and_then(serde_json::Value::as_object_mut)
            {
                node_outputs.insert(node_id.clone(), last_output.clone());
            }
            let next = node.get("next").ok_or_else(|| {
                BroError::new("atom.invalid_workflow", "workflow node has no transition")
            })?;
            match next.get("type").and_then(serde_json::Value::as_str) {
                Some("terminal") => return Ok(last_output),
                Some("goto") => {
                    node_id = next
                        .get("to")
                        .and_then(serde_json::Value::as_str)
                        .ok_or_else(|| {
                            BroError::new("atom.invalid_workflow", "goto transition has no target")
                        })?
                        .to_owned();
                }
                _ => {
                    return Err(BroError::new(
                        "atom.unsupported_workflow",
                        "atom-binding workflow supports only goto and terminal transitions",
                    ));
                }
            }
        }
        Err(BroError::new(
            "atom.workflow_limit_exceeded",
            format!("{workflow_ref} exceeded 256 node executions"),
        ))
    }

    async fn execute_consultant(
        &self,
        consumer: &str,
        input: &serde_json::Value,
    ) -> Result<serde_json::Value, BroError> {
        if consumer != "badgey" {
            return Err(BroError::new(
                "atom.unknown_consultant",
                format!("no durable consultant consumer is registered as {consumer}"),
            ));
        }
        let requested = input
            .get("consultant_id")
            .and_then(serde_json::Value::as_str);
        let prompt = input.get("prompt").and_then(serde_json::Value::as_str);
        let brief = input.get("brief").and_then(serde_json::Value::as_str);
        let agent_invocation = format!("{}:consultant:{consumer}", self.invocation_id);
        let agents = match &self.inherited_capability_policy {
            Some(policy) => self.runtime.session_agents_with_policy(
                self.worker_id.clone(),
                self.session_id.clone(),
                self.binding.clone(),
                agent_invocation.clone(),
                policy.clone(),
            ),
            None => self.runtime.session_agents(
                self.worker_id.clone(),
                self.session_id.clone(),
                agent_invocation.clone(),
            ),
        };
        let identity = if let Some(canonical_path) = requested {
            let prompt = prompt.ok_or_else(|| {
                BroError::new(
                    "atom.invalid_input",
                    "a consultant turn with consultant_id requires prompt",
                )
            })?;
            let target = AgentTarget {
                canonical_path: canonical_path.to_owned(),
            };
            agents
                .followup_for_invocation(
                    &agent_invocation,
                    AgentMessageRequest {
                        target: target.clone(),
                        message: prompt.to_owned(),
                    },
                )
                .await?;
            agents.status(target).await?.identity
        } else {
            let initial = brief.or(prompt).ok_or_else(|| {
                BroError::new(
                    "atom.invalid_input",
                    "a new consultant requires brief or prompt",
                )
            })?;
            let suffix = self
                .invocation_id
                .chars()
                .filter(|character| character.is_ascii_alphanumeric())
                .rev()
                .take(16)
                .collect::<String>()
                .chars()
                .rev()
                .collect::<String>();
            agents
                .spawn_for_invocation(
                    &agent_invocation,
                    AgentSpawnRequest {
                        task_name: format!("badgey-{suffix}"),
                        message: format!(
                            "Act as the durable Badgey consultant for one bounded turn. {initial}"
                        ),
                        fork_turns: AgentForkTurns::All,
                    },
                )
                .await?
        };

        let timeout_ms = input
            .get("timeout_seconds")
            .and_then(serde_json::Value::as_f64)
            .filter(|seconds| seconds.is_finite() && *seconds > 0.0)
            .map(|seconds| (seconds * 1_000.0).min(300_000.0) as u64)
            .unwrap_or(30_000);
        let deadline = self
            .deadline_unix_ms
            .unwrap_or_else(|| now_ms().saturating_add(timeout_ms))
            .min(now_ms().saturating_add(timeout_ms));
        loop {
            self.runtime.drive_once().await;
            let snapshot = self
                .runtime
                .authority
                .snapshot()
                .await
                .map_err(service_error)?;
            let agent = snapshot.agents.get(&identity.agent_id).ok_or_else(|| {
                BroError::new(
                    "atom.consultant_lost",
                    format!("consultant {} disappeared", identity.canonical_path),
                )
            })?;
            if matches!(
                agent.status,
                LogicalAgentStatus::Completed
                    | LogicalAgentStatus::Failed
                    | LogicalAgentStatus::Interrupted
            ) {
                let operation = agent
                    .current_operation_id
                    .as_ref()
                    .and_then(|operation_id| snapshot.operations.get(operation_id))
                    .ok_or_else(|| {
                        BroError::new(
                            "atom.consultant_invalid_state",
                            "terminal consultant has no operation",
                        )
                    })?;
                let accepted = operation.accepted.as_ref().ok_or_else(|| {
                    BroError::new(
                        "atom.consultant_invalid_state",
                        "terminal consultant has no accepted attempt",
                    )
                })?;
                if agent.status != LogicalAgentStatus::Completed {
                    return Err(BroError::new(
                        "atom.consultant_failed",
                        format!(
                            "consultant {} ended as {:?}",
                            identity.canonical_path, agent.status
                        ),
                    ));
                }
                return Ok(serde_json::json!({
                    "consultant_id": identity.canonical_path,
                    "badgey_id": identity.agent_id,
                    "task_id": accepted.task_id,
                    "session_id": accepted.session_id,
                    "provider": operation
                        .execution_request
                        .as_ref()
                        .map(|request| request.provider.as_str()),
                    "thread_id": identity.canonical_path,
                    "status": "completed",
                    "result": operation
                        .terminal
                        .as_ref()
                        .map(|terminal| terminal.result.clone())
                        .unwrap_or(serde_json::Value::Null),
                    "actions": [],
                    "resolved_brofile": "blackops:consultant/badgey@v1"
                }));
            }
            if now_ms() >= deadline {
                return Err(BroError::new(
                    "atom.deadline_exceeded",
                    format!("consultant {} is still running", identity.canonical_path),
                ));
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}

fn run_deterministic_atom(
    runner: &str,
    input: &serde_json::Value,
) -> Result<serde_json::Value, BroError> {
    match runner {
        "echo" => Ok(serde_json::json!({"echo": input})),
        "noop" => Ok(serde_json::json!({"noop": true})),
        "validate-schema" | "refactor-plan-validate" => Ok(serde_json::json!({
            "valid": input.is_object(),
            "type": if input.is_object() {
                "object"
            } else if input.is_array() {
                "array"
            } else if input.is_string() {
                "string"
            } else {
                "other"
            },
            "input": input
        })),
        other => Err(BroError::new(
            "atom.unknown_runner",
            format!("no deterministic blackops runner is registered as {other}"),
        )),
    }
}

fn run_adapter_atom(
    adapter_name: &str,
    input: &serde_json::Value,
) -> Result<serde_json::Value, BroError> {
    match adapter_name {
        "badgey" => Ok(serde_json::json!({
            "adapter": "badgey",
            "accepted": true,
            "input": input
        })),
        other => Err(BroError::new(
            "atom.unknown_adapter",
            format!("no blackops adapter is registered as {other}"),
        )),
    }
}

fn expand_atom_template(template: &str, input: &serde_json::Value) -> String {
    let mut result = template.to_owned();
    if let Some(input) = input.as_object() {
        for (key, value) in input {
            let replacement = value
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| value.to_string());
            result = result.replace(&format!("{{{{{key}}}}}"), &replacement);
        }
    }
    result
}

fn ensure_workflow_child_allowed(
    composition: Option<&serde_json::Value>,
    atom_ref: &str,
) -> Result<(), BroError> {
    let policy = composition
        .and_then(|composition| composition.get("may_invoke_atoms"))
        .ok_or_else(|| {
            BroError::new(
                "atom.composition_denied",
                "workflow atom has no child-composition policy",
            )
        })?;
    match policy.get("kind").and_then(serde_json::Value::as_str) {
        Some("any") => Ok(()),
        Some("allowed")
            if policy
                .get("atoms")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|atoms| atoms.iter().any(|allowed| allowed == atom_ref)) =>
        {
            Ok(())
        }
        _ => Err(BroError::new(
            "atom.composition_denied",
            format!("workflow atom is not permitted to invoke {atom_ref}"),
        )),
    }
}

fn interpolate_workflow_value(
    value: &serde_json::Value,
    context: &serde_json::Value,
) -> serde_json::Value {
    match value {
        serde_json::Value::String(text) => {
            if let Some(path) = text
                .strip_prefix("${")
                .and_then(|text| text.strip_suffix('}'))
                && let Some(resolved) = workflow_path(context, path)
            {
                return resolved.clone();
            }
            let mut rendered = text.clone();
            for capture in template_paths(text) {
                if let Some(resolved) = workflow_path(context, capture) {
                    let replacement = resolved
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| resolved.to_string());
                    rendered = rendered.replace(&format!("${{{capture}}}"), &replacement);
                }
            }
            serde_json::Value::String(rendered)
        }
        serde_json::Value::Array(values) => serde_json::Value::Array(
            values
                .iter()
                .map(|value| interpolate_workflow_value(value, context))
                .collect(),
        ),
        serde_json::Value::Object(values) => serde_json::Value::Object(
            values
                .iter()
                .map(|(key, value)| (key.clone(), interpolate_workflow_value(value, context)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn workflow_path<'a>(context: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    path.split('.')
        .try_fold(context, |value, segment| value.get(segment))
}

fn template_paths(text: &str) -> Vec<&str> {
    let mut paths = Vec::new();
    let mut remaining = text;
    while let Some(start) = remaining.find("${") {
        let after_start = &remaining[start + 2..];
        let Some(end) = after_start.find('}') else {
            break;
        };
        paths.push(&after_start[..end]);
        remaining = &after_start[end + 1..];
    }
    paths
}

fn execution_tool_policy_from_session(policy: &SessionCapabilityPolicy) -> ExecutionToolPolicy {
    ExecutionToolPolicy {
        allowed_remote_operations: policy
            .allowed_operations
            .iter()
            .map(|(capability, operations)| {
                (
                    capability.clone(),
                    operations.iter().cloned().collect::<Vec<_>>(),
                )
            })
            .collect(),
        allowed_atom_refs: policy.allowed_atom_refs.iter().cloned().collect(),
        ..ExecutionToolPolicy::default()
    }
}

fn narrow_remote_authority(tool_policy: &mut ExecutionToolPolicy, bound: &SessionCapabilityPolicy) {
    tool_policy
        .allowed_remote_operations
        .retain(|capability, operations| {
            operations.retain(|operation| bound.allows_operation(capability, operation));
            operations.sort();
            operations.dedup();
            !operations.is_empty()
        });
    tool_policy
        .allowed_atom_refs
        .retain(|atom_ref| bound.allows_atom_ref(atom_ref));
    tool_policy.allowed_atom_refs.sort();
    tool_policy.allowed_atom_refs.dedup();
}

#[derive(Clone)]
pub struct SessionAgentCapability {
    runtime: BlackopsRuntime,
    worker_id: String,
    session_id: SessionId,
    binding: SessionAttemptBinding,
    invocation_id: String,
    inherited_capability_policy: Option<SessionCapabilityPolicy>,
}

impl SessionAgentCapability {
    async fn ensure_root(&self) -> Result<AgentRecord, BroError> {
        let session_id = self.session_id.clone();
        let worker_id = self.worker_id.clone();
        let task_id = self.binding.task_id.clone();
        let attempt_id = self.binding.attempt_id.clone();
        let session_attempt_generation = self.binding.generation;
        self.runtime
            .authority
            .call(move |authority| {
                authority.ensure_session_root(
                    &session_id,
                    &worker_id,
                    &task_id,
                    &attempt_id,
                    session_attempt_generation,
                    now_ms(),
                )
            })
            .await
            .map_err(service_error)
    }

    fn operation_identity(&self, operation: &str) -> (OperationId, String) {
        let stable = format!(
            "agent:{}:{}:{}",
            self.session_id, self.invocation_id, operation
        );
        (OperationId::new(format!("operation-{stable}")), stable)
    }

    async fn caller_and_scope_root(&self) -> Result<(AgentRecord, AgentRecord), BroError> {
        let caller = self.ensure_root().await?;
        let snapshot = self
            .runtime
            .authority
            .snapshot()
            .await
            .map_err(service_error)?;
        let mut root = caller.clone();
        let mut remaining = snapshot.agents.len().saturating_add(1);
        while let Some(parent_id) = root.parent_id.as_ref() {
            if remaining == 0 {
                return Err(BroError::new(
                    "agent.invalid_graph",
                    "logical agent parent graph contains a cycle",
                ));
            }
            root = snapshot.agents.get(parent_id).cloned().ok_or_else(|| {
                BroError::new(
                    "agent.invalid_graph",
                    format!("logical agent parent {parent_id} is missing"),
                )
            })?;
            remaining = remaining.saturating_sub(1);
        }
        Ok((caller, root))
    }

    async fn authorize_target_path(&self, path: &str) -> Result<(), BroError> {
        let (_, root) = self.caller_and_scope_root().await?;
        if path_in_tree(&root.path, path) {
            Ok(())
        } else {
            Err(BroError::new(
                "agent.unauthorized_target",
                format!("target path {path} is outside the bound agent tree"),
            ))
        }
    }

    async fn execution_from_agent(
        &self,
        agent: Option<&AgentRecord>,
        operation_id: OperationId,
        idempotency_key: String,
        kind: ExecutionKind,
    ) -> Result<ExecutionRequest, BroError> {
        let template = if let Some(agent) = agent {
            let snapshot = self
                .runtime
                .authority
                .snapshot()
                .await
                .map_err(service_error)?;
            snapshot
                .operations
                .values()
                .filter(|operation| operation.kind.agent_id() == Some(&agent.agent_id))
                .filter_map(|operation| {
                    operation
                        .execution_request
                        .as_ref()
                        .map(|request| (operation.requested_at_unix_ms, request))
                })
                .max_by_key(|(requested_at, _)| *requested_at)
                .map(|(_, request)| request.clone())
        } else {
            None
        };
        let mut execution = match template {
            Some(mut request) => {
                request.operation_id = operation_id;
                request.idempotency_key = idempotency_key;
                request.kind = kind;
                request
            }
            None => ExecutionRequest {
                operation_id,
                idempotency_key,
                kind,
                provider: self.runtime.profile.provider,
                model: self.runtime.profile.model.clone(),
                effort: None,
                service_tier: ExecutionServiceTier::Default,
                code_mode: None,
                dispatch_context: None,
                working_set: WorkingSetIntent::Scratch,
                shell_env: BTreeMap::new(),
                tool_policy: self
                    .inherited_capability_policy
                    .as_ref()
                    .map(execution_tool_policy_from_session)
                    .unwrap_or_default(),
                system_prompt: None,
                output_schema: None,
                labels: BTreeMap::new(),
            },
        };
        if let Some(policy) = &self.inherited_capability_policy {
            narrow_remote_authority(&mut execution.tool_policy, policy);
        }
        Ok(execution)
    }

    async fn summary(&self, target: AgentTarget) -> Result<AgentSummary, BroError> {
        self.authorize_target_path(&target.canonical_path).await?;
        let snapshot = self
            .runtime
            .authority
            .snapshot()
            .await
            .map_err(service_error)?;
        match snapshot
            .agent_paths
            .get(&target.canonical_path)
            .and_then(|agent_id| snapshot.agents.get(agent_id))
        {
            Some(agent) => Ok(summary_for(&snapshot, agent)),
            None => Ok(AgentSummary {
                identity: AgentIdentity {
                    agent_id: AgentId::new(format!("missing:{}", target.canonical_path)),
                    canonical_path: target.canonical_path,
                },
                status: AgentStatus::NotFound,
                last_attempt_id: None,
                unavailable_cause: Some("logical agent path does not exist".into()),
            }),
        }
    }
}

#[async_trait]
impl AgentCapability for SessionAgentCapability {
    async fn spawn(&self, request: AgentSpawnRequest) -> Result<AgentIdentity, BroError> {
        let (parent, scope_root) = self.caller_and_scope_root().await?;
        let (operation_id, idempotency_key) = self.operation_identity("spawn");
        let mut execution = self
            .execution_from_agent(
                Some(&parent),
                operation_id,
                idempotency_key.clone(),
                ExecutionKind::Fresh {
                    prompt: request.message.clone(),
                },
            )
            .await?;
        execution.labels.insert(
            "fork_turns".into(),
            serde_json::to_string(&request.fork_turns).unwrap_or_default(),
        );
        execution
            .labels
            .insert("logical_parent".into(), parent.path.clone());
        let prompt_cache_root = scope_root.current_session_id.as_ref().ok_or_else(|| {
            BroError::new(
                "agent.invalid_graph",
                "top-level logical agent root has no bound session",
            )
        })?;
        execution
            .labels
            .insert("prompt_cache_root".into(), prompt_cache_root.to_string());
        if let Some(session_id) = parent.current_session_id.as_ref() {
            execution
                .labels
                .insert("fork_source_session".into(), session_id.to_string());
        }
        let spawn_request = SpawnAgentRequest {
            idempotency_key,
            name: request.task_name,
            role: "agent".into(),
            parent_id: Some(parent.agent_id),
            team_id: None,
            execution,
            requested_at_unix_ms: now_ms(),
        };
        let receipt = self
            .runtime
            .authority
            .call(move |authority| authority.spawn_agent(spawn_request))
            .await
            .map_err(service_error)?;
        Ok(AgentIdentity {
            agent_id: receipt.agent.agent_id,
            canonical_path: receipt.agent.path,
        })
    }

    async fn send_message(&self, request: AgentMessageRequest) -> Result<(), BroError> {
        let sender = self.ensure_root().await?;
        let path = request.target.canonical_path.clone();
        self.authorize_target_path(&path).await?;
        let recipient = self
            .runtime
            .authority
            .call(move |authority| authority.agent_by_path(&path))
            .await
            .map_err(service_error)?;
        let (_, idempotency_key) = self.operation_identity("send_message");
        let send_request = SendMessageRequest {
            idempotency_key,
            sender: Some(sender.agent_id),
            recipient: recipient.agent_id,
            body: request.message,
            created_at_unix_ms: now_ms(),
        };
        self.runtime
            .authority
            .call(move |authority| authority.send_message(send_request))
            .await
            .map_err(service_error)?;
        Ok(())
    }

    async fn followup(&self, request: AgentMessageRequest) -> Result<(), BroError> {
        let sender = self.ensure_root().await?;
        let path = request.target.canonical_path.clone();
        self.authorize_target_path(&path).await?;
        let recipient = self
            .runtime
            .authority
            .call(move |authority| authority.agent_by_path(&path))
            .await
            .map_err(service_error)?;
        let session_id = recipient.current_session_id.clone().ok_or_else(|| {
            BroError::new(
                "agent.not_resumable",
                "logical agent has no accepted session for followup",
            )
        })?;
        let (operation_id, idempotency_key) = self.operation_identity("followup");
        let execution = self
            .execution_from_agent(
                Some(&recipient),
                operation_id,
                idempotency_key.clone(),
                ExecutionKind::MailboxResume { session_id },
            )
            .await?;
        let followup_request = FollowupAgentRequest {
            idempotency_key,
            sender: Some(sender.agent_id),
            recipient: recipient.agent_id,
            body: request.message,
            execution,
            requested_at_unix_ms: now_ms(),
        };
        self.runtime
            .authority
            .call(move |authority| authority.followup_agent(followup_request))
            .await
            .map_err(service_error)?;
        Ok(())
    }

    async fn interrupt(&self, target: AgentTarget) -> Result<AgentStatus, BroError> {
        self.ensure_root().await?;
        let path = target.canonical_path;
        self.authorize_target_path(&path).await?;
        let agent = self
            .runtime
            .authority
            .call(move |authority| authority.agent_by_path(&path))
            .await
            .map_err(service_error)?;
        let (operation_id, idempotency_key) = self.operation_identity("interrupt");
        let interrupt_request = InterruptAgentRequest {
            operation_id,
            idempotency_key,
            agent_id: agent.agent_id,
            requested_at_unix_ms: now_ms(),
        };
        let agent_id = interrupt_request.agent_id.clone();
        self.runtime
            .authority
            .call(move |authority| authority.interrupt_agent(interrupt_request))
            .await
            .map_err(service_error)?;
        self.runtime.drive_once().await;
        let agent = self
            .runtime
            .authority
            .call(move |authority| authority.agent(&agent_id))
            .await
            .map_err(service_error)?;
        Ok(summary_for(
            &self
                .runtime
                .authority
                .snapshot()
                .await
                .map_err(service_error)?,
            &agent,
        )
        .status)
    }

    async fn status(&self, target: AgentTarget) -> Result<AgentSummary, BroError> {
        self.ensure_root().await?;
        self.summary(target).await
    }

    async fn list(&self, prefix: Option<String>) -> Result<Vec<AgentSummary>, BroError> {
        let (_, root) = self.caller_and_scope_root().await?;
        let prefix = prefix.unwrap_or_else(|| root.path.clone());
        if !path_in_tree(&root.path, &prefix) {
            return Err(BroError::new(
                "agent.unauthorized_target",
                format!("list prefix {prefix} is outside the bound agent tree"),
            ));
        }
        let snapshot = self
            .runtime
            .authority
            .snapshot()
            .await
            .map_err(service_error)?;
        Ok(snapshot
            .agents
            .values()
            .filter(|agent| path_in_tree(&prefix, &agent.path) && agent.agent_id != root.agent_id)
            .map(|agent| summary_for(&snapshot, agent))
            .collect())
    }

    async fn wait(&self, request: AgentWaitRequest) -> Result<AgentWake, BroError> {
        let (caller, root) = self.caller_and_scope_root().await?;
        let prefix = request.path_prefix.unwrap_or_else(|| root.path.clone());
        if !path_in_tree(&root.path, &prefix) {
            return Err(BroError::new(
                "agent.unauthorized_target",
                format!("wait prefix {prefix} is outside the bound agent tree"),
            ));
        }
        let after = request.after_mailbox_sequence.unwrap_or(0);
        let timeout = Duration::from_millis(request.timeout_ms.unwrap_or(30_000).min(300_000));
        let deadline = tokio::time::Instant::now() + timeout;
        let observer = format!("session:{}", self.session_id);
        loop {
            let terminal_prefix = prefix.clone();
            let terminal_observer = observer.clone();
            if let Some(agent) = self
                .runtime
                .authority
                .call(move |authority| {
                    authority.claim_terminal_agent_status(&terminal_prefix, &terminal_observer)
                })
                .await
                .map_err(service_error)?
            {
                let snapshot = self
                    .runtime
                    .authority
                    .snapshot()
                    .await
                    .map_err(service_error)?;
                return Ok(AgentWake::DescendantStatus {
                    agent: summary_for(&snapshot, &agent),
                });
            }
            let snapshot = self
                .runtime
                .authority
                .snapshot()
                .await
                .map_err(service_error)?;
            if snapshot
                .mailboxes
                .get(&caller.agent_id)
                .is_some_and(|mailbox| mailbox.next_sequence.saturating_sub(1) > after)
            {
                let agent_id = caller.agent_id.clone();
                let cursor_name = format!("session:{}", self.session_id);
                let read = self
                    .runtime
                    .authority
                    .call(move |authority| {
                        authority.read_mailbox(
                            &agent_id,
                            &cursor_name,
                            Some(after),
                            1_000,
                            now_ms(),
                        )
                    })
                    .await
                    .map_err(service_error)?;
                return Ok(AgentWake::MailboxChanged {
                    through_sequence: read.last_sequence,
                });
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return Ok(AgentWake::Timeout);
            }
            tokio::time::sleep((deadline - now).min(Duration::from_millis(50))).await;
        }
    }
}

fn path_in_tree(root: &str, candidate: &str) -> bool {
    candidate == root
        || candidate
            .strip_prefix(root)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn summary_for(snapshot: &blackops_core::OperationalSnapshot, agent: &AgentRecord) -> AgentSummary {
    let unavailable_cause = agent
        .current_operation_id
        .as_ref()
        .and_then(|operation_id| snapshot.operations.get(operation_id))
        .and_then(|operation| operation.last_error.clone());
    AgentSummary {
        identity: AgentIdentity {
            agent_id: agent.agent_id.clone(),
            canonical_path: agent.path.clone(),
        },
        status: match agent.status {
            LogicalAgentStatus::Requested => AgentStatus::Initializing,
            LogicalAgentStatus::Ready => AgentStatus::Idle,
            LogicalAgentStatus::Running | LogicalAgentStatus::InterruptRequested => {
                AgentStatus::Running
            }
            LogicalAgentStatus::Interrupted => AgentStatus::Interrupted,
            LogicalAgentStatus::Completed => AgentStatus::Completed,
            LogicalAgentStatus::Failed => AgentStatus::Errored,
        },
        last_attempt_id: agent.current_attempt_id.clone(),
        unavailable_cause,
    }
}

fn service_error(error: crate::BlackopsdError) -> BroError {
    let code = match &error {
        crate::BlackopsdError::Core(blackops_core::BlackopsError::NotFound(_)) => "agent.not_found",
        crate::BlackopsdError::Core(blackops_core::BlackopsError::Conflict(_)) => "agent.conflict",
        crate::BlackopsdError::Core(blackops_core::BlackopsError::InvalidRequest(_)) => {
            "agent.invalid_request"
        }
        _ => "agent.internal",
    };
    BroError::new(code, error.to_string())
}

fn atom_service_error(error: crate::BlackopsdError) -> BroError {
    let code = match &error {
        crate::BlackopsdError::Core(blackops_core::BlackopsError::NotFound(_)) => "atom.not_found",
        crate::BlackopsdError::Core(blackops_core::BlackopsError::Conflict(_)) => "atom.conflict",
        crate::BlackopsdError::Core(blackops_core::BlackopsError::InvalidRequest(_)) => {
            "atom.invalid_request"
        }
        _ => "atom.internal",
    };
    BroError::new(code, error.to_string())
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;

    #[test]
    fn record_batch_is_bounded_by_exact_request_bytes_without_reordering() {
        let payload_bytes = RECORD_BATCH_BODY_BUDGET / 3 - 1024;
        let records = (1..=4)
            .map(|cursor| RecordEnvelope {
                record_id: format!("record-{cursor}"),
                producer: "blackopsd".into(),
                cursor: cursor.to_string(),
                kind: "definition.installed".into(),
                occurred_at: None,
                subject: None,
                attributes: BTreeMap::new(),
                payload: serde_json::json!({"body": "x".repeat(payload_bytes)}),
            })
            .collect::<Vec<_>>();

        let batch = bounded_record_batch(records.clone()).unwrap();
        assert_eq!(batch.len(), 3);
        assert_eq!(
            batch
                .iter()
                .map(|record| record.record_id.as_str())
                .collect::<Vec<_>>(),
            ["record-1", "record-2", "record-3"]
        );
        let encoded = serde_json::to_vec(&RecordIngestRequest {
            records: batch.clone(),
        })
        .unwrap();
        assert!(encoded.len() <= RECORD_BATCH_BODY_BUDGET);
        let mut over_budget = batch;
        over_budget.push(records[3].clone());
        assert!(
            serde_json::to_vec(&RecordIngestRequest {
                records: over_budget
            })
            .unwrap()
            .len()
                > RECORD_BATCH_BODY_BUDGET
        );
    }

    #[test]
    fn child_execution_inherits_and_cannot_broaden_remote_authority() {
        let allowed_ref = bro_core::AtomRef::new("atom:allowed@v1");
        let bound = SessionCapabilityPolicy {
            allowed_operations: BTreeMap::from([
                ("atom".into(), BTreeSet::from(["invoke_atom".into()])),
                ("blackops.agent".into(), BTreeSet::from(["spawn".into()])),
            ]),
            allowed_atom_refs: BTreeSet::from([allowed_ref.clone()]),
        };

        let inherited = execution_tool_policy_from_session(&bound);
        assert_eq!(
            inherited.allowed_remote_operations,
            BTreeMap::from([
                ("atom".into(), vec!["invoke_atom".into()]),
                ("blackops.agent".into(), vec!["spawn".into()]),
            ])
        );
        assert_eq!(inherited.allowed_atom_refs, vec![allowed_ref.clone()]);

        let mut attempted_broadening = ExecutionToolPolicy {
            allow_tools: vec!["operator-visible-tool".into()],
            allowed_remote_operations: BTreeMap::from([
                (
                    "atom".into(),
                    vec!["invoke_atom".into(), "install_atom".into()],
                ),
                (
                    "blackops.agent".into(),
                    vec!["spawn".into(), "interrupt".into()],
                ),
                ("corpus.search".into(), vec!["search".into()]),
            ]),
            allowed_atom_refs: vec![allowed_ref.clone(), bro_core::AtomRef::new("atom:other@v1")],
            ..ExecutionToolPolicy::default()
        };
        narrow_remote_authority(&mut attempted_broadening, &bound);

        assert_eq!(
            attempted_broadening.allowed_remote_operations,
            inherited.allowed_remote_operations
        );
        assert_eq!(attempted_broadening.allowed_atom_refs, vec![allowed_ref]);
        assert_eq!(
            attempted_broadening.allow_tools,
            vec!["operator-visible-tool"]
        );
    }
}
