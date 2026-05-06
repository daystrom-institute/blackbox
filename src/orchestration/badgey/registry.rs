use std::collections::HashMap;

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

use crate::orchestration::providers::Provider;

use super::types::{now_rfc3339, BadgeyId, BadgeyScope};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BadgeyInstance {
    pub id: BadgeyId,
    pub scope: BadgeyScope,
    pub provider: Provider,
    /// Provider-owned session id observed from the underlying exec
    /// result. Badgey never generates this value.
    pub provider_session_id: String,
    pub thread_of_record_id: String,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dismissed_at: Option<String>,
}

impl BadgeyInstance {
    pub fn new(
        id: BadgeyId,
        scope: BadgeyScope,
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
    NotFound { id: BadgeyId },
    AlreadyExists { id: BadgeyId },
    Dismissed { id: BadgeyId },
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound { id } => write!(f, "badgey instance not found: {id}"),
            Self::AlreadyExists { id } => write!(f, "badgey instance already exists: {id}"),
            Self::Dismissed { id } => write!(f, "badgey instance dismissed: {id}"),
        }
    }
}

impl std::error::Error for RegistryError {}

#[derive(Default)]
pub struct BadgeyRegistry {
    instances: RwLock<HashMap<BadgeyId, BadgeyInstance>>,
}

impl BadgeyRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, instance: BadgeyInstance) -> Result<(), RegistryError> {
        let mut instances = self.instances.write();
        if instances.contains_key(&instance.id) {
            return Err(RegistryError::AlreadyExists {
                id: instance.id.clone(),
            });
        }
        instances.insert(instance.id.clone(), instance);
        Ok(())
    }

    pub fn get(&self, id: &BadgeyId) -> Result<BadgeyInstance, RegistryError> {
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

    pub fn get_including_dismissed(&self, id: &BadgeyId) -> Result<BadgeyInstance, RegistryError> {
        self.instances
            .read()
            .get(id)
            .cloned()
            .ok_or_else(|| RegistryError::NotFound { id: id.clone() })
    }

    pub fn dismiss(&self, id: &BadgeyId) -> Result<BadgeyInstance, RegistryError> {
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
        Ok(instance.clone())
    }

    pub fn update_provider_session_id(
        &self,
        id: &BadgeyId,
        observed_provider_session_id: String,
    ) -> Result<BadgeyInstance, RegistryError> {
        let mut instances = self.instances.write();
        let instance = instances
            .get_mut(id)
            .ok_or_else(|| RegistryError::NotFound { id: id.clone() })?;
        if instance.is_dismissed() {
            return Err(RegistryError::Dismissed { id: id.clone() });
        }
        instance.provider_session_id = observed_provider_session_id;
        instance.updated_at = now_rfc3339();
        Ok(instance.clone())
    }

    pub fn list(&self) -> Vec<BadgeyInstance> {
        let mut instances: Vec<_> = self.instances.read().values().cloned().collect();
        instances.sort_by(|a, b| a.id.cmp(&b.id));
        instances
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    #[test]
    fn register_lookup_dismiss_marks_instance_unavailable() {
        let registry = BadgeyRegistry::new();
        let id = BadgeyId::from_str("bg-3f7a91c4-91ff04cc").unwrap();
        let instance = BadgeyInstance::new(
            id.clone(),
            BadgeyScope {
                project_id: "proj".to_string(),
                initial_brief: Some("brief".to_string()),
            },
            Provider::Codex,
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
        assert!(registry
            .get_including_dismissed(&id)
            .unwrap()
            .is_dismissed());
    }

    #[test]
    fn updates_observed_provider_session_id() {
        let registry = BadgeyRegistry::new();
        let id = BadgeyId::from_str("bg-3f7a91c4-91ff04cc").unwrap();
        let instance = BadgeyInstance::new(
            id.clone(),
            BadgeyScope {
                project_id: "proj".to_string(),
                initial_brief: None,
            },
            Provider::Gemini,
            "pending".to_string(),
            "thread-1".to_string(),
        );
        registry.register(instance).unwrap();
        let updated = registry
            .update_provider_session_id(&id, "gemini-session-1".to_string())
            .unwrap();
        assert_eq!(updated.provider_session_id, "gemini-session-1");
    }
}
