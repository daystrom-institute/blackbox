use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use tokio::sync::{RwLock, broadcast};

use super::identity::IdentityRegistry;
use super::outbox::{OutboxCompactionReport, OutboxStore};
use super::reactions;
use super::store::{EventCompactionReport, EventStore};
use super::types::*;

const BROADCAST_CAPACITY: usize = 256;
const MAX_CAUSATION_DEPTH: usize = 4;

pub type SharedEventHub = Arc<EventHub>;

pub struct EventHub {
    tx: broadcast::Sender<SystemEvent>,
    event_store: EventStore,
    outbox_store: OutboxStore,
    reactions: RwLock<HashMap<String, ReactionSpec>>,
    reactions_dir: PathBuf,
    process_instance_id: String,
    identity_registry: Arc<IdentityRegistry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemEventDraft {
    pub kind: SystemEventKind,
    pub producer: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal: Option<EventPrincipal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<EventSubject>,
    #[serde(default)]
    pub correlation: serde_json::Map<String, serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub causation_id: Option<String>,
    #[serde(default)]
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmitOutcome {
    pub event: SystemEvent,
    pub journal_appended: bool,
    pub outbox_enqueued: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outbox_error: Option<String>,
    pub matched_reactions: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReactionLoadWarning {
    pub source: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReactionListResult {
    pub reactions: Vec<ReactionSpec>,
    pub warnings: Vec<ReactionLoadWarning>,
}

#[derive(Debug, serde::Serialize)]
pub struct SystemEventCompactionReport {
    pub now: String,
    pub event_journal: EventCompactionReport,
    pub outbox: OutboxCompactionReport,
}

impl EventHub {
    pub fn new(
        event_store: EventStore,
        outbox_store: OutboxStore,
        reactions_dir: PathBuf,
        identity_root: PathBuf,
    ) -> Self {
        let (tx, _) = broadcast::channel(BROADCAST_CAPACITY);
        let process_instance_id = format!(
            "pid-{}-{}",
            std::process::id(),
            chrono::Utc::now().format("%Y%m%d%H%M%S")
        );
        std::fs::create_dir_all(&reactions_dir).ok();
        let identity_registry =
            Arc::new(IdentityRegistry::new(identity_root).unwrap_or_else(|e| {
                tracing::warn!("identity registry init failed, using empty: {e:#}");
                IdentityRegistry::new_empty()
            }));
        Self {
            tx,
            event_store,
            outbox_store,
            reactions: RwLock::new(HashMap::new()),
            reactions_dir,
            process_instance_id,
            identity_registry,
        }
    }

    pub fn identity_registry(&self) -> &Arc<IdentityRegistry> {
        &self.identity_registry
    }

    /// Look up a durable identity mapping. If none exists, emit `bro.identity.required`
    /// (once per daemon lifetime per key via pending dedup) and return `Ok(None)`.
    pub async fn require_identity(
        &self,
        req: &super::identity::IdentityRequest,
    ) -> Result<Option<super::identity::ExternalIdentity>> {
        let reg = &self.identity_registry;
        let subject = req.subject();
        if let Some(id) = reg.lookup(
            &req.scope,
            &req.instance,
            &subject,
            &req.provider,
            &req.model,
        ) {
            return Ok(Some(id));
        }
        if reg.is_pending(
            &req.scope,
            &req.instance,
            &subject,
            &req.provider,
            &req.model,
        ) {
            return Ok(None);
        }
        reg.mark_pending(
            &req.scope,
            &req.instance,
            &subject,
            &req.provider,
            &req.model,
        );
        let mut payload = serde_json::json!({
            "identity_scope": req.scope,
            "instance": req.instance,
            "bro": req.bro,
            "provider": req.provider,
            "model": req.model,
            "username": req.username(),
            "display_name": req.display_name(),
            "email": req.email(),
        });
        if let Some(ref effort) = req.effort {
            payload["effort"] = serde_json::Value::String(effort.clone());
        }
        let draft = SystemEventDraft {
            kind: SystemEventKind::BroIdentityRequired,
            producer: "system_events.identity".to_string(),
            project: req.project.clone(),
            principal: Some(super::types::EventPrincipal {
                kind: "bro".to_string(),
                bro: Some(req.bro.clone()),
                provider: Some(req.provider.clone()),
                model: Some(req.model.clone()),
                effort: req.effort.clone(),
            }),
            subject: Some(super::types::EventSubject {
                kind: "bro".to_string(),
                id: subject,
            }),
            correlation: Default::default(),
            causation_id: None,
            payload,
        };
        self.emit(draft).await?;
        Ok(None)
    }

    pub async fn restore_reactions_from_disk(&self) -> Vec<ReactionLoadWarning> {
        let mut warnings = Vec::new();
        for entry in load_all_reactions_raw(&self.reactions_dir) {
            match entry {
                RawReaction::Valid(spec) => {
                    let name = spec.name.clone();
                    match reactions::validate_reaction_spec(&spec) {
                        Ok(()) => {
                            tracing::info!("restoring reaction '{name}'");
                            let mut map = self.reactions.write().await;
                            map.insert(name, spec);
                        }
                        Err(e) => {
                            let source = format!("{}.json", spec.name);
                            tracing::warn!(
                                "skipping restore of reaction '{name}': validation failed: {e}"
                            );
                            warnings.push(ReactionLoadWarning {
                                source,
                                reason: format!("validation failed: {e}"),
                            });
                        }
                    }
                }
                RawReaction::Invalid { path, error } => {
                    tracing::warn!("load_all_reactions: bad spec at {path}: {error}");
                    warnings.push(ReactionLoadWarning {
                        source: path,
                        reason: format!("parse error: {error}"),
                    });
                }
            }
        }
        warnings
    }

    pub fn subscribe(&self) -> broadcast::Receiver<SystemEvent> {
        self.tx.subscribe()
    }

    pub fn event_store(&self) -> &EventStore {
        &self.event_store
    }

    pub fn outbox_store(&self) -> &OutboxStore {
        &self.outbox_store
    }

    pub fn compact_with_now(&self, now_rfc3339: &str) -> Result<SystemEventCompactionReport> {
        let event_journal = self.event_store.compact_with_now(now_rfc3339)?;
        let outbox = self.outbox_store.compact_with_now(now_rfc3339)?;
        Ok(SystemEventCompactionReport {
            now: now_rfc3339.to_string(),
            event_journal,
            outbox,
        })
    }

    pub fn reactions_dir(&self) -> &Path {
        &self.reactions_dir
    }

    pub fn process_instance_id(&self) -> &str {
        &self.process_instance_id
    }

    pub async fn install_reaction(&self, spec: ReactionSpec, replace: bool) -> Result<()> {
        reactions::validate_reaction_spec(&spec)?;
        {
            let reactions = self.reactions.read().await;
            if reactions.contains_key(&spec.name) && !replace {
                bail!(
                    "reaction '{}' already exists; set replace=true to overwrite",
                    spec.name
                );
            }
        }
        let path = self.reactions_dir.join(format!("{}.json", spec.name));
        let pretty = serde_json::to_string_pretty(&spec)?;
        std::fs::write(&path, pretty)?;
        let mut reactions = self.reactions.write().await;
        reactions.insert(spec.name.clone(), spec);
        Ok(())
    }

    pub async fn get_reaction(&self, name: &str) -> Option<ReactionSpec> {
        let reactions = self.reactions.read().await;
        reactions.get(name).cloned()
    }

    pub async fn list_reactions(&self) -> ReactionListResult {
        let reactions = self.reactions.read().await;
        let mut specs: Vec<_> = reactions.values().cloned().collect();
        specs.sort_by(|a, b| a.name.cmp(&b.name));
        ReactionListResult {
            reactions: specs,
            warnings: Vec::new(),
        }
    }

    pub async fn list_reactions_with_warnings(&self) -> ReactionListResult {
        let mut result = self.list_reactions().await;
        let disk_warnings = scan_reaction_warnings(&self.reactions_dir);
        result.warnings = disk_warnings;
        result
    }

    pub async fn emit(&self, draft: SystemEventDraft) -> Result<EmitOutcome> {
        if let Some(ref cid) = draft.causation_id {
            let chain = self.event_store.causation_chain(cid)?;
            if chain.len() >= MAX_CAUSATION_DEPTH {
                bail!(
                    "causation depth {} exceeds maximum of {} (event {})",
                    chain.len() + 1,
                    MAX_CAUSATION_DEPTH,
                    cid
                );
            }
        }

        let event = SystemEvent {
            id: new_event_id(),
            kind: draft.kind,
            occurred_at: crate::util::now_iso(),
            producer: draft.producer,
            project: draft.project,
            principal: draft.principal,
            subject: draft.subject,
            correlation: draft.correlation,
            causation_id: draft.causation_id,
            payload: draft.payload,
        };

        let envelope = JournalEnvelope::wrap(event.clone());
        if let Err(e) = self.event_store.append(&envelope) {
            bail!("journal append failed: {e:#}");
        }

        let reactions = self.reactions.read().await;
        let matched: Vec<&ReactionSpec> = reactions
            .values()
            .filter(|r| r.enabled && r.event_kinds.iter().any(|k| k == event.kind.to_wire()))
            .collect();
        let matched_count = matched.len();

        let mut outbox_enqueued = true;
        let mut outbox_error: Option<String> = None;

        let event_roots = build_event_roots(&event);

        let mut sorted = matched;
        sorted.sort_by(|a, b| a.name.cmp(&b.name));
        for reaction in sorted {
            let rendered_key = match reaction.idempotency_key.as_deref() {
                Some(tpl) => {
                    match super::template::render_template_with_roots(
                        tpl,
                        &event_roots,
                        super::template::UnresolvedPolicy::HardError,
                    ) {
                        Ok(k) => Some(k),
                        Err(e) => {
                            outbox_enqueued = false;
                            outbox_error = Some(format!(
                                "idempotency key render failed for '{}': {e:#}",
                                reaction.name
                            ));
                            break;
                        }
                    }
                }
                None => None,
            };
            if let Err(e) = self
                .outbox_store
                .create_record(&event.id, &reaction.name, rendered_key)
            {
                outbox_enqueued = false;
                outbox_error = Some(format!("{e:#}"));
                break;
            }
        }

        let _ = self.tx.send(event.clone());

        Ok(EmitOutcome {
            event,
            journal_appended: true,
            outbox_enqueued,
            outbox_error,
            matched_reactions: matched_count,
        })
    }

    pub fn list_events(
        &self,
        limit: Option<usize>,
        kind_filter: Option<&str>,
        producer_filter: Option<&str>,
        project_filter: Option<&str>,
    ) -> Result<Vec<SystemEvent>> {
        let envelopes = self.event_store.load_all()?;
        let mut events: Vec<SystemEvent> = envelopes
            .into_iter()
            .map(|e| e.event)
            .filter(|e| kind_filter.map(|k| e.kind.to_wire() == k).unwrap_or(true))
            .filter(|e| producer_filter.map(|p| e.producer == p).unwrap_or(true))
            .filter(|e| {
                project_filter
                    .map(|p| e.project.as_deref() == Some(p))
                    .unwrap_or(true)
            })
            .collect();
        events.reverse();
        if let Some(limit) = limit {
            events.truncate(limit);
        }
        Ok(events)
    }

    pub fn open_event(&self, event_id: &str) -> Result<Option<SystemEvent>> {
        let envelopes = self.event_store.load_all()?;
        let event = envelopes
            .into_iter()
            .find(|e| e.event.id == event_id)
            .map(|e| e.event);
        Ok(event)
    }

    pub fn causation_chain_for(&self, event_id: &str) -> Result<Vec<SystemEvent>> {
        self.event_store.causation_chain(event_id)
    }

    pub fn derived_events(&self, event_id: &str) -> Result<Vec<SystemEvent>> {
        let envelopes = self.event_store.load_all()?;
        let derived: Vec<SystemEvent> = envelopes
            .into_iter()
            .filter_map(|e| {
                if e.event.causation_id.as_deref() == Some(event_id) {
                    Some(e.event)
                } else {
                    None
                }
            })
            .collect();
        Ok(derived)
    }
}

fn build_event_roots(event: &SystemEvent) -> serde_json::Map<String, serde_json::Value> {
    let mut roots = serde_json::Map::new();
    roots.insert(
        "event".to_string(),
        serde_json::to_value(event).unwrap_or_default(),
    );
    roots
}

enum RawReaction {
    Valid(ReactionSpec),
    Invalid { path: String, error: String },
}

fn load_all_reactions_raw(dir: &Path) -> Vec<RawReaction> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension() != Some(std::ffi::OsStr::new("json")) {
            continue;
        }
        let path_display = path.display().to_string();
        if let Ok(bytes) = std::fs::read(&path) {
            match serde_json::from_slice::<ReactionSpec>(&bytes) {
                Ok(spec) => out.push(RawReaction::Valid(spec)),
                Err(e) => out.push(RawReaction::Invalid {
                    path: path_display,
                    error: e.to_string(),
                }),
            }
        }
    }
    out
}

pub fn load_all_reactions(dir: &Path) -> Vec<ReactionSpec> {
    load_all_reactions_raw(dir)
        .into_iter()
        .filter_map(|entry| match entry {
            RawReaction::Valid(spec) => Some(spec),
            RawReaction::Invalid { path, error } => {
                tracing::warn!("load_all_reactions: bad spec at {path}: {error}");
                None
            }
        })
        .collect()
}

fn scan_reaction_warnings(dir: &Path) -> Vec<ReactionLoadWarning> {
    load_all_reactions_raw(dir)
        .into_iter()
        .filter_map(|entry| match entry {
            RawReaction::Valid(spec) => {
                let source = format!("{}.json", spec.name);
                match reactions::validate_reaction_spec(&spec) {
                    Ok(()) => None,
                    Err(e) => Some(ReactionLoadWarning {
                        source,
                        reason: format!("validation failed: {e}"),
                    }),
                }
            }
            RawReaction::Invalid { path, error } => Some(ReactionLoadWarning {
                source: path,
                reason: format!("parse error: {error}"),
            }),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn test_hub() -> (SharedEventHub, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let es = EventStore::new_at(dir.path().join("journal"));
        let os = OutboxStore::new(dir.path().join("outbox")).unwrap();
        let rd = dir.path().join("reactions");
        let id = dir.path().join("identities");
        (Arc::new(EventHub::new(es, os, rd, id)), dir)
    }

    fn simple_draft(producer: &str) -> SystemEventDraft {
        SystemEventDraft {
            kind: SystemEventKind::TaskStarted,
            producer: producer.to_string(),
            project: None,
            principal: None,
            subject: None,
            correlation: serde_json::Map::new(),
            causation_id: None,
            payload: json!({}),
        }
    }

    #[tokio::test]
    async fn emit_fills_id_time_and_appends_journal() {
        let (hub, _dir) = test_hub();
        let draft = simple_draft("test-producer");
        let outcome = hub.emit(draft).await.unwrap();
        assert!(outcome.event.id.starts_with("evt-"));
        assert!(!outcome.event.occurred_at.is_empty());
        assert!(outcome.journal_appended);
        assert!(outcome.outbox_enqueued);
        assert_eq!(outcome.matched_reactions, 0);

        let events = hub.list_events(None, None, None, None).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, outcome.event.id);
    }

    #[tokio::test]
    async fn live_subscriber_receives_event() {
        let (hub, _dir) = test_hub();
        let mut rx = hub.subscribe();
        let draft = simple_draft("broadcaster");
        let outcome = hub.emit(draft).await.unwrap();

        let received = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(received.id, outcome.event.id);
        assert_eq!(received.producer, "broadcaster");
    }

    #[tokio::test]
    async fn causation_depth_rejects_depth_5() {
        let (hub, _dir) = test_hub();

        let mut prev_id = None;
        for depth in 0..4 {
            let draft = SystemEventDraft {
                kind: SystemEventKind::TaskStarted,
                producer: format!("chain-{depth}"),
                project: None,
                principal: None,
                subject: None,
                correlation: serde_json::Map::new(),
                causation_id: prev_id,
                payload: json!({}),
            };
            let outcome = hub.emit(draft).await.unwrap();
            prev_id = Some(outcome.event.id);
        }

        let over_depth = SystemEventDraft {
            kind: SystemEventKind::TaskCompleted,
            producer: "chain-overflow".to_string(),
            project: None,
            principal: None,
            subject: None,
            correlation: serde_json::Map::new(),
            causation_id: prev_id,
            payload: json!({}),
        };
        let result = hub.emit(over_depth).await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("causation depth"),
            "expected causation depth error, got: {err}"
        );
    }

    #[tokio::test]
    async fn concurrent_emits_preserve_events_and_unique_ids() {
        let (hub, _dir) = test_hub();
        let n = 20;
        let mut handles = Vec::new();
        for i in 0..n {
            let h = Arc::clone(&hub);
            handles.push(tokio::spawn(async move {
                h.emit(SystemEventDraft {
                    kind: SystemEventKind::TaskStarted,
                    producer: format!("concurrent-{i}"),
                    project: None,
                    principal: None,
                    subject: None,
                    correlation: serde_json::Map::new(),
                    causation_id: None,
                    payload: json!({"i": i}),
                })
                .await
                .unwrap()
            }));
        }
        let mut ids = std::collections::HashSet::new();
        for h in handles {
            let outcome = h.await.unwrap();
            assert!(ids.insert(outcome.event.id), "duplicate event id");
        }
        assert_eq!(ids.len(), n);

        let events = hub.list_events(None, None, None, None).unwrap();
        assert_eq!(events.len(), n);
    }

    #[tokio::test]
    async fn emit_matches_reactions_and_creates_outbox() {
        let (hub, _dir) = test_hub();
        hub.install_reaction(
            ReactionSpec {
                contract: "reaction/v1".to_string(),
                name: "react-completed".to_string(),
                version: 1,
                enabled: true,
                event_kinds: vec!["task.completed".to_string()],
                when: None,
                idempotency_key: Some("key:${event.id}".to_string()),
                action: ReactionAction::EmitEvent { args: json!({}) },
                retry: RetryPolicy::default(),
                on_failure: FailurePolicy::DeadLetter,
            },
            false,
        )
        .await
        .unwrap();

        let draft = SystemEventDraft {
            kind: SystemEventKind::TaskCompleted,
            producer: "test".to_string(),
            project: None,
            principal: None,
            subject: None,
            correlation: serde_json::Map::new(),
            causation_id: None,
            payload: json!({}),
        };
        let outcome = hub.emit(draft).await.unwrap();
        assert_eq!(outcome.matched_reactions, 1);
        assert!(outcome.outbox_enqueued);

        let records = hub.outbox_store().load_all().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].event_id, outcome.event.id);
        assert_eq!(records[0].reaction_name, "react-completed");
    }

    #[tokio::test]
    async fn list_respects_filters() {
        let (hub, _dir) = test_hub();
        for (producer, kind) in &[
            ("alpha", SystemEventKind::TaskStarted),
            ("beta", SystemEventKind::TaskCompleted),
            ("alpha", SystemEventKind::TaskFailed),
        ] {
            hub.emit(SystemEventDraft {
                kind: kind.clone(),
                producer: producer.to_string(),
                project: Some("/repo/test".to_string()),
                principal: None,
                subject: None,
                correlation: serde_json::Map::new(),
                causation_id: None,
                payload: json!({}),
            })
            .await
            .unwrap();
        }

        let all = hub.list_events(None, None, None, None).unwrap();
        assert_eq!(all.len(), 3);

        let alpha = hub.list_events(None, None, Some("alpha"), None).unwrap();
        assert_eq!(alpha.len(), 2);

        let started = hub
            .list_events(None, Some("task.started"), None, None)
            .unwrap();
        assert_eq!(started.len(), 1);
        assert_eq!(started[0].producer, "alpha");

        let limited = hub.list_events(Some(2), None, None, None).unwrap();
        assert_eq!(limited.len(), 2);
    }

    #[tokio::test]
    async fn list_does_not_leak_secret_values() {
        let (hub, _dir) = test_hub();
        hub.emit(SystemEventDraft {
            kind: SystemEventKind::TaskStarted,
            producer: "test".to_string(),
            project: None,
            principal: None,
            subject: None,
            correlation: serde_json::Map::new(),
            causation_id: None,
            payload: json!({
                "token": "secret:forgejo-admin-token",
                "api_key": "should-not-appear"
            }),
        })
        .await
        .unwrap();

        let events = hub.list_events(None, None, None, None).unwrap();
        assert_eq!(events.len(), 1);
        let payload_str = serde_json::to_string(&events[0].payload).unwrap();
        assert!(
            payload_str.contains("secret:forgejo-admin-token"),
            "secret ref should be preserved as-is in payload"
        );
        assert!(
            !payload_str.contains("resolved_secret_value"),
            "resolved secret values must not appear"
        );
    }

    #[tokio::test]
    async fn restore_loads_valid_specs_async() {
        let (hub, _dir) = test_hub();
        let spec = ReactionSpec {
            contract: "reaction/v1".to_string(),
            name: "restore-valid".to_string(),
            version: 1,
            enabled: true,
            event_kinds: vec!["task.started".to_string()],
            when: None,
            idempotency_key: None,
            action: ReactionAction::EmitEvent { args: json!({}) },
            retry: RetryPolicy::default(),
            on_failure: FailurePolicy::DeadLetter,
        };
        let path = hub.reactions_dir().join("restore-valid.json");
        std::fs::write(&path, serde_json::to_string_pretty(&spec).unwrap()).unwrap();

        let warnings = hub.restore_reactions_from_disk().await;
        assert!(warnings.is_empty(), "expected no warnings: {warnings:?}");

        let loaded = hub.get_reaction("restore-valid").await;
        assert!(loaded.is_some());
        assert_eq!(loaded.unwrap().name, "restore-valid");
    }

    #[tokio::test]
    async fn restore_reports_invalid_json_as_warning() {
        let (hub, _dir) = test_hub();
        let path = hub.reactions_dir().join("bad-json.json");
        std::fs::write(&path, "this is not json").unwrap();

        let warnings = hub.restore_reactions_from_disk().await;
        assert_eq!(warnings.len(), 1);
        assert!(
            warnings[0].reason.contains("parse error"),
            "expected parse error: {:?}",
            warnings[0]
        );

        let loaded = hub.get_reaction("bad-json").await;
        assert!(loaded.is_none());
    }

    #[tokio::test]
    async fn restore_reports_validation_failure_as_warning() {
        let (hub, _dir) = test_hub();
        let mut spec = ReactionSpec {
            contract: "reaction/v1".to_string(),
            name: "bad-contract".to_string(),
            version: 1,
            enabled: true,
            event_kinds: vec!["task.started".to_string()],
            when: None,
            idempotency_key: None,
            action: ReactionAction::EmitEvent { args: json!({}) },
            retry: RetryPolicy::default(),
            on_failure: FailurePolicy::DeadLetter,
        };
        spec.contract = "wrong/v1".to_string();
        let path = hub.reactions_dir().join("bad-contract.json");
        std::fs::write(&path, serde_json::to_string_pretty(&spec).unwrap()).unwrap();

        let warnings = hub.restore_reactions_from_disk().await;
        assert_eq!(warnings.len(), 1);
        assert!(
            warnings[0].reason.contains("validation failed"),
            "expected validation failure: {:?}",
            warnings[0]
        );

        let loaded = hub.get_reaction("bad-contract").await;
        assert!(loaded.is_none());
    }

    #[tokio::test]
    async fn emit_stores_rendered_idempotency_key() {
        let (hub, _dir) = test_hub();
        hub.install_reaction(
            ReactionSpec {
                contract: "reaction/v1".to_string(),
                name: "render-key-test".to_string(),
                version: 1,
                enabled: true,
                event_kinds: vec!["task.completed".to_string()],
                when: None,
                idempotency_key: Some("key:${event.id}".to_string()),
                action: ReactionAction::EmitEvent { args: json!({}) },
                retry: RetryPolicy::default(),
                on_failure: FailurePolicy::DeadLetter,
            },
            false,
        )
        .await
        .unwrap();

        let outcome = hub
            .emit(SystemEventDraft {
                kind: SystemEventKind::TaskCompleted,
                producer: "key-render".to_string(),
                project: None,
                principal: None,
                subject: None,
                correlation: serde_json::Map::new(),
                causation_id: None,
                payload: json!({}),
            })
            .await
            .unwrap();

        let records = hub.outbox_store().load_all().unwrap();
        assert_eq!(records.len(), 1);
        let rendered_key = records[0].idempotency_key.as_ref().unwrap();
        assert!(
            rendered_key.starts_with("key:evt-"),
            "expected rendered key like 'key:evt-...'; got: {rendered_key}"
        );
        assert!(
            !rendered_key.contains("${"),
            "key must not contain unrendered template: {rendered_key}"
        );
        assert!(
            rendered_key.contains(&outcome.event.id),
            "rendered key must contain the event id: {rendered_key}"
        );
    }

    #[tokio::test]
    async fn emit_partial_success_on_idempotency_render_failure() {
        let (hub, _dir) = test_hub();
        hub.install_reaction(
            ReactionSpec {
                contract: "reaction/v1".to_string(),
                name: "bad-key-reaction".to_string(),
                version: 1,
                enabled: true,
                event_kinds: vec!["task.completed".to_string()],
                when: None,
                idempotency_key: Some("${event.nonexistent.deep.field}".to_string()),
                action: ReactionAction::EmitEvent { args: json!({}) },
                retry: RetryPolicy::default(),
                on_failure: FailurePolicy::DeadLetter,
            },
            false,
        )
        .await
        .unwrap();

        let outcome = hub
            .emit(SystemEventDraft {
                kind: SystemEventKind::TaskCompleted,
                producer: "bad-key".to_string(),
                project: None,
                principal: None,
                subject: None,
                correlation: serde_json::Map::new(),
                causation_id: None,
                payload: json!({}),
            })
            .await
            .unwrap();

        assert!(
            outcome.journal_appended,
            "journal should be appended even when idempotency render fails"
        );
        assert!(
            !outcome.outbox_enqueued,
            "outbox should not be enqueued when render fails"
        );
        assert!(
            outcome.outbox_error.is_some(),
            "outbox error should describe the render failure"
        );
        let err = outcome.outbox_error.unwrap();
        assert!(
            err.contains("idempotency key render failed"),
            "error should mention render failure: {err}"
        );

        let records = hub.outbox_store().load_all().unwrap();
        assert!(
            records.is_empty(),
            "no outbox records should be created on render failure"
        );
    }

    fn identity_req() -> super::super::identity::IdentityRequest {
        super::super::identity::IdentityRequest {
            scope: "forgejo".to_string(),
            instance: "local-forgejo15".to_string(),
            bro: "keystone-review".to_string(),
            provider: "claude".to_string(),
            model: "haiku-4.5".to_string(),
            effort: Some("medium".to_string()),
            project: None,
        }
    }

    #[tokio::test]
    async fn require_identity_miss_emits_event_once() {
        let (hub, _dir) = test_hub();
        let req = identity_req();

        // First call: no identity, should emit bro.identity.required and return None.
        let result = hub.require_identity(&req).await.unwrap();
        assert!(result.is_none(), "expected None on first miss");

        let events = hub
            .list_events(None, Some("bro.identity.required"), None, None)
            .unwrap();
        assert_eq!(
            events.len(),
            1,
            "expected exactly one bro.identity.required event"
        );
        let payload = &events[0].payload;
        assert_eq!(payload["bro"], "keystone-review");
        assert_eq!(payload["username"], "bro-keystone-review-claude-haiku45");

        // Second call: still no identity, but pending dedup suppresses second event.
        let result2 = hub.require_identity(&req).await.unwrap();
        assert!(result2.is_none());

        let events2 = hub
            .list_events(None, Some("bro.identity.required"), None, None)
            .unwrap();
        assert_eq!(
            events2.len(),
            1,
            "repeated miss must not emit a second event"
        );
    }

    #[tokio::test]
    async fn require_identity_returns_identity_after_upsert() {
        let (hub, _dir) = test_hub();
        let req = identity_req();

        // Miss first.
        hub.require_identity(&req).await.unwrap();

        // Simulate provisioning completing.
        let identity = super::super::identity::ExternalIdentity {
            scope: "forgejo".to_string(),
            instance: "local-forgejo15".to_string(),
            subject: "bro:keystone-review".to_string(),
            provider: "claude".to_string(),
            model: "haiku-4.5".to_string(),
            external_user_id: "42".to_string(),
            username: "bro-keystone-review-claude-haiku45".to_string(),
            token_ref: "secret:forgejo-bro-bro-keystone-review-claude-haiku45".to_string(),
            created_at: crate::util::now_iso(),
            last_verified_at: None,
        };
        hub.identity_registry().upsert(&identity).unwrap();

        // Now require_identity should find the provisioned identity.
        let found = hub.require_identity(&req).await.unwrap();
        assert!(found.is_some(), "expected identity after upsert");
        assert_eq!(
            found.unwrap().token_ref,
            "secret:forgejo-bro-bro-keystone-review-claude-haiku45"
        );

        // No new event should have been emitted.
        let events = hub
            .list_events(None, Some("bro.identity.required"), None, None)
            .unwrap();
        assert_eq!(events.len(), 1, "no new event expected after provisioning");
    }

    #[tokio::test]
    async fn require_identity_upsert_clears_pending() {
        let (hub, _dir) = test_hub();
        let req = identity_req();

        hub.require_identity(&req).await.unwrap();
        assert!(hub.identity_registry().is_pending(
            "forgejo",
            "local-forgejo15",
            "bro:keystone-review",
            "claude",
            "haiku-4.5"
        ));

        let identity = super::super::identity::ExternalIdentity {
            scope: "forgejo".to_string(),
            instance: "local-forgejo15".to_string(),
            subject: "bro:keystone-review".to_string(),
            provider: "claude".to_string(),
            model: "haiku-4.5".to_string(),
            external_user_id: "99".to_string(),
            username: "bro-keystone-review-claude-haiku45".to_string(),
            token_ref: "secret:forgejo-bro-test".to_string(),
            created_at: crate::util::now_iso(),
            last_verified_at: None,
        };
        hub.identity_registry().upsert(&identity).unwrap();

        assert!(
            !hub.identity_registry().is_pending(
                "forgejo",
                "local-forgejo15",
                "bro:keystone-review",
                "claude",
                "haiku-4.5"
            ),
            "upsert must clear pending flag"
        );
    }

    #[test]
    fn username_derivation() {
        let req = super::super::identity::IdentityRequest {
            scope: "forgejo".to_string(),
            instance: "local-forgejo15".to_string(),
            bro: "keystone-review".to_string(),
            provider: "claude".to_string(),
            model: "haiku-4.5".to_string(),
            effort: Some("medium".to_string()),
            project: None,
        };
        assert_eq!(req.username(), "bro-keystone-review-claude-haiku45");
        assert_eq!(
            req.display_name(),
            "keystone-review / claude haiku-4.5 medium"
        );
        assert_eq!(req.email(), "bro-keystone-review@blackbox.local");
        assert_eq!(req.subject(), "bro:keystone-review");
    }
}
