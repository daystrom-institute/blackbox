//! Durable bro observations. Recording an event never starts further work.
use super::store::{EventCompactionReport, EventStore};
use super::types::*;
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::broadcast;

pub type SharedEventHub = Arc<EventHub>;

pub struct EventHub {
    tx: broadcast::Sender<SystemEvent>,
    event_store: EventStore,
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

impl EventHub {
    pub fn new(event_store: EventStore) -> Self {
        let (tx, _) = broadcast::channel(256);
        Self { tx, event_store }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<SystemEvent> {
        self.tx.subscribe()
    }

    pub fn compact_with_now(&self, now: &str) -> Result<EventCompactionReport> {
        self.event_store.compact_with_now(now)
    }

    pub async fn emit(&self, draft: SystemEventDraft) -> Result<SystemEvent> {
        let event = SystemEvent {
            id: new_event_id(),
            kind: draft.kind,
            occurred_at: bbox_util::util::now_iso(),
            producer: draft.producer,
            project: draft.project,
            principal: draft.principal,
            subject: draft.subject,
            correlation: draft.correlation,
            causation_id: draft.causation_id,
            payload: draft.payload,
        };
        if let Err(error) = self
            .event_store
            .append(&JournalEnvelope::wrap(event.clone()))
        {
            bail!("journal append failed: {error:#}");
        }
        let _ = self.tx.send(event.clone());
        Ok(event)
    }

    /// Continue strictly behind an existing matching event, so new appends cannot
    /// duplicate or displace rows between pages. A compacted/unknown cursor fails.
    pub fn list_event_page(
        &self,
        limit: Option<usize>,
        before: Option<&str>,
        kind: Option<&str>,
        producer: Option<&str>,
        project: Option<&str>,
    ) -> Result<serde_json::Value> {
        let events = self.list_events(None, kind, producer, project)?;
        let start = match before {
            Some(id) => events.iter().position(|event| event.id == id)
                .ok_or_else(|| anyhow::anyhow!("error.event_cursor_unavailable: before is absent from the filtered journal; restart without before"))? + 1,
            None => 0,
        };
        let total = events.len().saturating_sub(start);
        let limit = limit.unwrap_or(20).clamp(1, 100);
        let rows: Vec<_> = events
            .iter()
            .skip(start)
            .take(limit)
            .map(SystemEvent::summary)
            .collect();
        // Reserve continuation space before byte selection; event ids normally have
        // equal lengths, but this also covers historical/custom event identities.
        let cursor_reserve = rows
            .iter()
            .filter_map(|row| row["id"].as_str())
            .max_by_key(|id| id.len());
        let mut page = bbox_corpus_core::response_page::bound_page(
            serde_json::json!({
                "events": rows, "offset": 0, "total": total, "limit": limit,
                "count": rows.len(), "next_before": cursor_reserve,
            }),
            "events",
        )?;
        let next = if page["next_offset"].is_null() {
            serde_json::Value::Null
        } else {
            page["events"]
                .as_array()
                .and_then(|rows| rows.last())
                .and_then(|row| row.get("id"))
                .cloned()
                .unwrap_or_default()
        };
        page["next_before"] = next;
        page.as_object_mut().unwrap().remove("offset");
        page.as_object_mut().unwrap().remove("next_offset");
        Ok(page)
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

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn observations_survive_restart_without_opening_retired_automation_stores() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("reactions")).unwrap();
        let archive = root.join("reactions/old.json");
        let bytes = b"historical reaction bytes, intentionally not parsed";
        std::fs::write(&archive, bytes).unwrap();
        let hub = EventHub::new(EventStore::new_at(root.join("journal")));
        let mut live = hub.subscribe();
        let event = hub
            .emit(SystemEventDraft {
                kind: SystemEventKind::TaskStarted,
                producer: "bro".into(),
                project: None,
                principal: None,
                subject: None,
                correlation: Default::default(),
                causation_id: None,
                payload: serde_json::json!({"task_id":"fixture-task"}),
            })
            .await
            .unwrap();
        assert_eq!(live.recv().await.unwrap().id, event.id);
        drop(hub);
        let reopened = EventHub::new(EventStore::new_at(root.join("journal")));
        assert_eq!(
            reopened.open_event(&event.id).unwrap().unwrap().payload,
            event.payload
        );
        assert_eq!(std::fs::read(archive).unwrap(), bytes);
        assert!(!root.join("outbox").exists());
        assert!(!root.join("identities").exists());
    }
}
