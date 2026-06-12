use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

use crate::orchestration::providers::Provider;

use super::queue::{PendingTurn, QueueError, QueuePermit, QueueStatus, ResumeQueue};
use super::types::{ConsultantId, ConsultantScope, now_rfc3339};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConsultantInstance {
    pub id: ConsultantId,
    pub scope: ConsultantScope,
    pub provider: Provider,
    /// Provider-owned session id observed from the underlying exec
    /// result. The consultant runtime never generates this value.
    pub provider_session_id: String,
    pub thread_of_record_id: String,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dismissed_at: Option<String>,
}

impl ConsultantInstance {
    pub fn new(
        id: ConsultantId,
        scope: ConsultantScope,
        provider: Provider,
        provider_session_id: String,
        thread_of_record_id: String,
    ) -> Self {
        let now = now_rfc3339();
        Self {
            id,
            scope,
            provider,
            provider_session_id,
            thread_of_record_id,
            created_at: now.clone(),
            updated_at: now,
            dismissed_at: None,
        }
    }

    pub fn is_dismissed(&self) -> bool {
        self.dismissed_at.is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryError {
    NotFound { id: ConsultantId },
    AlreadyExists { id: ConsultantId },
    Dismissed { id: ConsultantId },
    QueueRejected { id: ConsultantId, err: QueueError },
}

// Error text intentionally keeps the legacy "badgey" wording until the
// consumer-descriptor phase parameterizes per-consumer wording.
impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound { id } => write!(f, "badgey instance not found: {id}"),
            Self::AlreadyExists { id } => write!(f, "badgey instance already exists: {id}"),
            Self::Dismissed { id } => write!(f, "badgey instance dismissed: {id}"),
            Self::QueueRejected { id, err } => write!(f, "badgey instance {id}: {err}"),
        }
    }
}

impl std::error::Error for RegistryError {}

#[derive(Default)]
pub struct ConsultantRegistry {
    instances: RwLock<HashMap<ConsultantId, ConsultantInstance>>,
    queues: RwLock<HashMap<ConsultantId, Arc<ResumeQueue>>>,
}

impl ConsultantRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, instance: ConsultantInstance) -> Result<(), RegistryError> {
        let mut instances = self.instances.write();
        if instances.contains_key(&instance.id) {
            return Err(RegistryError::AlreadyExists {
                id: instance.id.clone(),
            });
        }
        let id = instance.id.clone();
        instances.insert(id.clone(), instance);
        self.queues
            .write()
            .entry(id)
            .or_insert_with(|| Arc::new(ResumeQueue::default()));
        Ok(())
    }

    pub fn get(&self, id: &ConsultantId) -> Result<ConsultantInstance, RegistryError> {
        let instances = self.instances.read();
        let instance = instances
            .get(id)
            .cloned()
            .ok_or_else(|| RegistryError::NotFound { id: id.clone() })?;
        if instance.is_dismissed() {
            return Err(RegistryError::Dismissed { id: id.clone() });
        }
        Ok(instance)
    }

    pub fn get_including_dismissed(&self, id: &ConsultantId) -> Result<ConsultantInstance, RegistryError> {
        self.instances
            .read()
            .get(id)
            .cloned()
            .ok_or_else(|| RegistryError::NotFound { id: id.clone() })
    }

    pub fn dismiss(&self, id: &ConsultantId) -> Result<ConsultantInstance, RegistryError> {
        let mut instances = self.instances.write();
        let instance = instances
            .get_mut(id)
            .ok_or_else(|| RegistryError::NotFound { id: id.clone() })?;
        if instance.is_dismissed() {
            return Err(RegistryError::Dismissed { id: id.clone() });
        }
        let now = now_rfc3339();
        instance.dismissed_at = Some(now.clone());
        instance.updated_at = now;
        if let Some(queue) = self.queues.read().get(id) {
            queue.close_and_clear();
        }
        Ok(instance.clone())
    }

    pub fn list(&self) -> Vec<ConsultantInstance> {
        let mut instances: Vec<_> = self.instances.read().values().cloned().collect();
        instances.sort_by(|a, b| a.id.cmp(&b.id));
        instances
    }

    pub fn enqueue_resume(&self, id: &ConsultantId, turn: PendingTurn) -> Result<usize, RegistryError> {
        let queue = self.queue_for_active_instance(id)?;
        queue
            .enqueue(turn)
            .map_err(|err| RegistryError::QueueRejected {
                id: id.clone(),
                err,
            })
    }

    pub fn enqueue_priority_resume(
        &self,
        id: &ConsultantId,
        turn: PendingTurn,
    ) -> Result<usize, RegistryError> {
        let queue = self.queue_for_active_instance(id)?;
        queue
            .enqueue_priority(turn)
            .map_err(|err| RegistryError::QueueRejected {
                id: id.clone(),
                err,
            })
    }

    pub fn pop_next_resume(&self, id: &ConsultantId) -> Result<Option<PendingTurn>, RegistryError> {
        Ok(self.queue_for_active_instance(id)?.pop_next())
    }

    pub async fn wait_for_resume_turn(
        &self,
        id: &ConsultantId,
        turn_id: &str,
    ) -> Result<QueuePermit, RegistryError> {
        let queue = self.queue_for_active_instance(id)?;
        queue
            .wait_until_turn(turn_id)
            .await
            .map_err(|err| RegistryError::QueueRejected {
                id: id.clone(),
                err,
            })
    }

    pub fn queue_status(&self, id: &ConsultantId) -> Result<QueueStatus, RegistryError> {
        Ok(self.queue_for_existing_instance(id)?.status())
    }

    fn queue_for_active_instance(&self, id: &ConsultantId) -> Result<Arc<ResumeQueue>, RegistryError> {
        self.get(id)?;
        self.queue_for_existing_instance(id)
    }

    fn queue_for_existing_instance(
        &self,
        id: &ConsultantId,
    ) -> Result<Arc<ResumeQueue>, RegistryError> {
        self.queues
            .read()
            .get(id)
            .cloned()
            .ok_or_else(|| RegistryError::NotFound { id: id.clone() })
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    #[test]
    fn register_lookup_dismiss_marks_instance_unavailable() {
        let registry = ConsultantRegistry::new();
        let id = ConsultantId::from_str("bg-3f7a91c4-91ff04cc").unwrap();
        let instance = ConsultantInstance::new(
            id.clone(),
            ConsultantScope {
                project_id: "proj".to_string(),
                initial_brief: Some("brief".to_string()),
            },
            Provider::Brodex,
            "provider-session".to_string(),
            "thread-1".to_string(),
        );
        registry.register(instance).unwrap();
        assert_eq!(
            registry.get(&id).unwrap().provider_session_id,
            "provider-session"
        );
        registry.dismiss(&id).unwrap();
        assert!(matches!(
            registry.get(&id),
            Err(RegistryError::Dismissed { .. })
        ));
        assert!(
            registry
                .get_including_dismissed(&id)
                .unwrap()
                .is_dismissed()
        );
    }

    #[test]
    fn registry_queue_serializes_resumes_and_closes_on_dismiss() {
        let registry = ConsultantRegistry::new();
        let id = ConsultantId::from_str("bg-3f7a91c4-91ff04cc").unwrap();
        registry
            .register(ConsultantInstance::new(
                id.clone(),
                ConsultantScope {
                    project_id: "proj".to_string(),
                    initial_brief: None,
                },
                Provider::Brodex,
                "session-1".to_string(),
                "thread-1".to_string(),
            ))
            .unwrap();

        registry
            .enqueue_resume(
                &id,
                PendingTurn {
                    turn_id: "turn-1".to_string(),
                    prompt: "first".to_string(),
                },
            )
            .unwrap();
        registry
            .enqueue_priority_resume(
                &id,
                PendingTurn {
                    turn_id: "dismiss".to_string(),
                    prompt: "dismiss".to_string(),
                },
            )
            .unwrap();
        assert_eq!(registry.queue_status(&id).unwrap().depth, 2);
        assert_eq!(
            registry.pop_next_resume(&id).unwrap().unwrap().turn_id,
            "dismiss"
        );
        registry.dismiss(&id).unwrap();
        assert!(matches!(
            registry.enqueue_resume(
                &id,
                PendingTurn {
                    turn_id: "turn-2".to_string(),
                    prompt: "second".to_string(),
                },
            ),
            Err(RegistryError::Dismissed { .. })
        ));
        assert!(registry.queue_status(&id).unwrap().closed);
    }
}
