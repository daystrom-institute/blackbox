use std::collections::HashMap;
use std::sync::Arc;

use super::types::{AgentFilterOverlay, AgentManifest, MergedFilters};

// ---------------------------------------------------------------------------
// DispatchContext
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct DispatchContext {
    pub project_dir: Option<String>,
    pub ambient: Option<String>,
    pub bro_label_prefix: Option<String>,
    pub caller_provider: Option<String>,
    pub caller_session_id: Option<String>,
}

// ---------------------------------------------------------------------------
// AgentDispatchResult
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct AgentDispatchResult {
    pub session_id: String,
    pub task_id: String,
    pub resolved_brofile: Option<String>,
    pub merged_filters: MergedFilters,
    pub degraded: bool,
}

// ---------------------------------------------------------------------------
// AgentDispatchError
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentDispatchError {
    BadInput { message: String },
    NotFound { name: String },
    AdapterFailed { message: String },
}

impl std::fmt::Display for AgentDispatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadInput { message } => write!(f, "bad input: {message}"),
            Self::NotFound { name } => write!(f, "adapter not found: {name}"),
            Self::AdapterFailed { message } => write!(f, "adapter failed: {message}"),
        }
    }
}

impl std::error::Error for AgentDispatchError {}

// ---------------------------------------------------------------------------
// AgentDispatchAdapter trait
// ---------------------------------------------------------------------------

pub trait AgentDispatchAdapter: Send + Sync {
    fn name(&self) -> &'static str;

    fn dispatch(
        &self,
        manifest: &AgentManifest,
        args: serde_json::Value,
        ctx: DispatchContext,
    ) -> Result<AgentDispatchResult, AgentDispatchError>;
}

// ---------------------------------------------------------------------------
// AgentAdapterRegistry
// ---------------------------------------------------------------------------

pub struct AgentAdapterRegistry {
    adapters: HashMap<&'static str, Arc<dyn AgentDispatchAdapter>>,
}

impl Default for AgentAdapterRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for AgentAdapterRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentAdapterRegistry")
            .field("names", &self.list_registered())
            .finish()
    }
}

impl AgentAdapterRegistry {
    pub fn new() -> Self {
        Self {
            adapters: HashMap::new(),
        }
    }

    pub fn register(&mut self, adapter: Arc<dyn AgentDispatchAdapter>) {
        self.adapters.insert(adapter.name(), adapter);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn AgentDispatchAdapter>> {
        self.adapters.get(name).cloned()
    }

    pub fn list_registered(&self) -> Vec<&'static str> {
        let mut names: Vec<_> = self.adapters.keys().copied().collect();
        names.sort();
        names
    }
}

// ---------------------------------------------------------------------------
// MergedFilters helper
// ---------------------------------------------------------------------------

impl MergedFilters {
    pub fn merge(
        base_allow: &[String],
        base_disallow: &[String],
        overlay: Option<&AgentFilterOverlay>,
    ) -> Self {
        let (mut allow, mut disallow) = (base_allow.to_vec(), base_disallow.to_vec());
        if let Some(ov) = overlay {
            allow.extend_from_slice(&ov.allow);
            disallow.extend_from_slice(&ov.disallow);
        }
        Self { allow, disallow }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    struct NoopAdapter;

    impl AgentDispatchAdapter for NoopAdapter {
        fn name(&self) -> &'static str {
            "noop"
        }

        fn dispatch(
            &self,
            _manifest: &AgentManifest,
            _args: serde_json::Value,
            _ctx: DispatchContext,
        ) -> Result<AgentDispatchResult, AgentDispatchError> {
            Ok(AgentDispatchResult {
                session_id: "noop-session".into(),
                task_id: "noop-task".into(),
                resolved_brofile: None,
                merged_filters: MergedFilters::default(),
                degraded: false,
            })
        }
    }

    #[test]
    fn noop_adapter_dispatches() {
        let adapter = NoopAdapter;
        let manifest = AgentManifest {
            description: "test".into(),
            ..Default::default()
        };
        let ctx = DispatchContext {
            project_dir: None,
            ambient: None,
            bro_label_prefix: None,
            caller_provider: None,
            caller_session_id: None,
        };
        let result = adapter
            .dispatch(&manifest, serde_json::Value::Null, ctx)
            .unwrap();
        assert_eq!(result.session_id, "noop-session");
        assert_eq!(result.task_id, "noop-task");
        assert!(!result.degraded);
    }

    #[test]
    fn registry_register_get_list() {
        let mut registry = AgentAdapterRegistry::new();
        assert!(registry.get("noop").is_none());
        assert!(registry.list_registered().is_empty());

        registry.register(Arc::new(NoopAdapter));
        assert!(registry.get("noop").is_some());
        assert_eq!(registry.list_registered(), vec!["noop"]);
    }

    #[test]
    fn registry_returns_none_for_unknown() {
        let registry = AgentAdapterRegistry::new();
        assert!(registry.get("unknown").is_none());
    }

    #[test]
    fn registry_overwrite_replaces() {
        let mut registry = AgentAdapterRegistry::new();
        registry.register(Arc::new(NoopAdapter));
        registry.register(Arc::new(NoopAdapter));
        assert_eq!(registry.list_registered().len(), 1);
    }

    #[test]
    fn dispatch_error_display() {
        let err = AgentDispatchError::BadInput {
            message: "missing field".into(),
        };
        assert_eq!(format!("{err}"), "bad input: missing field");

        let err = AgentDispatchError::NotFound {
            name: "badgey".into(),
        };
        assert_eq!(format!("{err}"), "adapter not found: badgey");

        let err = AgentDispatchError::AdapterFailed {
            message: "timeout".into(),
        };
        assert_eq!(format!("{err}"), "adapter failed: timeout");
    }

    #[test]
    fn merged_filters_merge_overlay() {
        let merged = MergedFilters::merge(
            &["mcp__blackbox__bbox_*".into()],
            &["mcp__blackbox__bro_*".into()],
            Some(&AgentFilterOverlay {
                allow: vec!["mcp__blackbox__bbox_search".into()],
                disallow: vec!["mcp__blackbox__bbox_forget".into()],
            }),
        );
        assert_eq!(
            merged.allow,
            vec![
                "mcp__blackbox__bbox_*".to_string(),
                "mcp__blackbox__bbox_search".to_string(),
            ]
        );
        assert_eq!(
            merged.disallow,
            vec![
                "mcp__blackbox__bro_*".to_string(),
                "mcp__blackbox__bbox_forget".to_string(),
            ]
        );
    }

    #[test]
    fn merged_filters_no_overlay() {
        let merged = MergedFilters::merge(&["a".into()], &["b".into()], None);
        assert_eq!(merged.allow, vec!["a".to_string()]);
        assert_eq!(merged.disallow, vec!["b".to_string()]);
    }
}
