use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SupervisionAttachment {
    pub supervision_run_id: String,
    pub primary_invocation_id: String,
    pub primary_task_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classifier_invocation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advisor_invocation_id: Option<String>,
    pub attempt: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InvocationStatus {
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    TimedOut,
    Expired,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AtomHandle {
    Profile {
        provider: String,
        session_id: String,
        project_dir: Option<String>,
        task_id: String,
    },
    Workflow {
        workflow_ref: String,
        arc_id: Option<String>,
        root_task_id: Option<String>,
    },
    Deterministic {
        runner: String,
    },
    Adapter {
        adapter_name: String,
        adapter_handle: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EffectsObserved {
    pub writes_files: Option<bool>,
    pub dispatches_runs: Option<u64>,
    pub uses_network: Option<bool>,
    pub violations: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct InvocationCost {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub dispatched_runs: Option<u64>,
    pub wall_time_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputShapeStatus {
    pub valid: Option<bool>,
    pub schema_ref: String,
    pub errors: Vec<String>,
}

impl Default for OutputShapeStatus {
    fn default() -> Self {
        Self {
            valid: None,
            schema_ref: "outputs.schema".to_string(),
            errors: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct InvocationLimits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub writes_files: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatches_runs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_depth: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uses_network: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AtomInvocation {
    pub invocation_id: String,
    pub atom_ref: String,
    pub parent_invocation_id: Option<String>,
    pub owners: HashSet<String>,
    pub handle: AtomHandle,
    pub status: InvocationStatus,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub input_digest: Option<String>,
    pub output_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_shape: Option<OutputShapeStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_limits: Option<InvocationLimits>,
    pub summary: Option<String>,
    pub effects_observed: EffectsObserved,
    pub cost: InvocationCost,
    pub children: Vec<String>,
    pub errors: Vec<String>,
    pub artifacts: Vec<String>,
}

impl AtomInvocation {
    pub fn task_id(&self) -> Option<String> {
        match &self.handle {
            AtomHandle::Profile { task_id, .. } => Some(task_id.clone()),
            AtomHandle::Workflow {
                root_task_id: Some(task_id),
                ..
            } => Some(task_id.clone()),
            _ => None,
        }
    }
}

impl AtomInvocation {
    pub fn new_profile(
        invocation_id: String,
        atom_ref: String,
        parent_invocation_id: Option<String>,
        owner: String,
        provider: String,
        session_id: String,
        project_dir: Option<String>,
        task_id: String,
    ) -> Self {
        let mut owners = HashSet::new();
        owners.insert(owner);
        Self {
            invocation_id,
            atom_ref,
            parent_invocation_id,
            owners,
            handle: AtomHandle::Profile {
                provider,
                session_id,
                project_dir,
                task_id,
            },
            status: InvocationStatus::Running,
            started_at: now_iso(),
            ended_at: None,
            input_digest: None,
            output_digest: None,
            output_shape: None,
            effective_limits: None,
            summary: None,
            effects_observed: EffectsObserved {
                dispatches_runs: Some(1),
                ..EffectsObserved::default()
            },
            cost: InvocationCost {
                dispatched_runs: Some(1),
                ..InvocationCost::default()
            },
            children: Vec::new(),
            errors: Vec::new(),
            artifacts: Vec::new(),
        }
    }

    pub fn new_workflow(
        invocation_id: String,
        atom_ref: String,
        parent_invocation_id: Option<String>,
        owner: String,
        workflow_ref: String,
        arc_id: Option<String>,
        root_task_id: Option<String>,
    ) -> Self {
        let mut owners = HashSet::new();
        owners.insert(owner);
        Self {
            invocation_id,
            atom_ref,
            parent_invocation_id,
            owners,
            handle: AtomHandle::Workflow {
                workflow_ref,
                arc_id,
                root_task_id,
            },
            status: InvocationStatus::Running,
            started_at: now_iso(),
            ended_at: None,
            input_digest: None,
            output_digest: None,
            output_shape: None,
            effective_limits: None,
            summary: None,
            effects_observed: EffectsObserved {
                dispatches_runs: Some(1),
                ..EffectsObserved::default()
            },
            cost: InvocationCost {
                dispatched_runs: Some(1),
                ..InvocationCost::default()
            },
            children: Vec::new(),
            errors: Vec::new(),
            artifacts: Vec::new(),
        }
    }

    pub fn is_owner(&self, identity: &str) -> bool {
        self.owners.contains(identity)
    }

    pub fn add_owner(&mut self, identity: &str) {
        self.owners.insert(identity.to_string());
    }

    pub fn is_resumable(&self) -> bool {
        matches!(self.handle, AtomHandle::Profile { .. })
            && matches!(
                self.status,
                InvocationStatus::Running
                    | InvocationStatus::Succeeded
                    | InvocationStatus::Failed
                    | InvocationStatus::TimedOut
            )
    }

    pub fn to_trace_envelope(&self) -> serde_json::Value {
        let state_str = match &self.status {
            InvocationStatus::Queued => "queued",
            InvocationStatus::Running => "running",
            InvocationStatus::Succeeded => "succeeded",
            InvocationStatus::Failed => "failed",
            InvocationStatus::Cancelled => "cancelled",
            InvocationStatus::TimedOut => "timed_out",
            InvocationStatus::Expired => "expired",
        };
        let impl_kind = match &self.handle {
            AtomHandle::Profile { .. } => "profile",
            AtomHandle::Workflow { .. } => "workflow",
            AtomHandle::Deterministic { .. } => "deterministic",
            AtomHandle::Adapter { .. } => "adapter",
        };
        serde_json::json!({
            "invocation_id": self.invocation_id,
            "parent_invocation_id": self.parent_invocation_id,
            "atom_ref": self.atom_ref,
            "implementation_kind": impl_kind,
            "state": state_str,
            "started_at": self.started_at,
            "ended_at": self.ended_at,
            "input_digest": self.input_digest,
            "output_digest": self.output_digest,
            "output_shape": self.output_shape.clone().unwrap_or_default(),
            "summary": self.summary,
            "decision_points": [],
            "children": self.children,
            "effective_limits": self.effective_limits,
            "effects_observed": self.effects_observed,
            "cost": self.cost,
            "artifacts": self.artifacts,
            "errors": self.errors,
        })
    }
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

pub struct InvocationStore {
    invocations: HashMap<String, AtomInvocation>,
    attachments: HashMap<String, Vec<SupervisionAttachment>>,
    path: PathBuf,
}

impl InvocationStore {
    pub fn new(path: PathBuf) -> Self {
        let mut store = Self {
            invocations: HashMap::new(),
            attachments: HashMap::new(),
            path,
        };
        store.load();
        store
    }

    pub fn insert(&mut self, inv: AtomInvocation) {
        self.invocations.insert(inv.invocation_id.clone(), inv);
        let _ = self.persist();
    }

    pub fn get(&self, id: &str) -> Option<&AtomInvocation> {
        self.invocations.get(id)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut AtomInvocation> {
        self.invocations.get_mut(id)
    }

    pub fn update(&mut self, inv: AtomInvocation) {
        self.invocations.insert(inv.invocation_id.clone(), inv);
        let _ = self.persist();
    }

    pub fn insert_attachment(&mut self, attachment: SupervisionAttachment) {
        let primary_id = attachment.primary_invocation_id.clone();
        let bucket = self.attachments.entry(primary_id).or_default();
        if let Some(existing) = bucket
            .iter_mut()
            .find(|attached| attached.attempt == attachment.attempt)
        {
            *existing = attachment;
        } else {
            bucket.push(attachment);
            bucket.sort_by_key(|a| a.attempt);
        }
        let _ = self.persist();
    }

    pub fn attachments_for_primary(
        &self,
        primary_invocation_id: &str,
    ) -> Vec<SupervisionAttachment> {
        self.attachments
            .get(primary_invocation_id)
            .cloned()
            .unwrap_or_default()
    }

    pub fn attachment_for_primary_attempt(
        &self,
        primary_invocation_id: &str,
        attempt: u64,
    ) -> Option<SupervisionAttachment> {
        self.attachments
            .get(primary_invocation_id)?
            .iter()
            .find(|attached| attached.attempt == attempt)
            .cloned()
    }

    pub fn latest_attachment_for_primary(
        &self,
        primary_invocation_id: &str,
    ) -> Option<SupervisionAttachment> {
        self.attachments
            .get(primary_invocation_id)?
            .iter()
            .max_by_key(|attached| attached.attempt)
            .cloned()
    }

    fn load(&mut self) {
        if !self.path.exists() {
            return;
        }
        if let Ok(data) = std::fs::read_to_string(&self.path) {
            #[derive(Deserialize)]
            struct PersistedInvocationStore {
                invocations: HashMap<String, AtomInvocation>,
                #[serde(default)]
                attachments: HashMap<String, Vec<SupervisionAttachment>>,
            }
            if let Ok(loaded) = serde_json::from_str::<PersistedInvocationStore>(&data) {
                self.invocations = loaded.invocations;
                self.attachments = loaded.attachments;
                return;
            }
            if let Ok(loaded) = serde_json::from_str::<HashMap<String, AtomInvocation>>(&data) {
                self.invocations = loaded;
                self.persist().ok();
            }
        }
    }

    pub fn persist(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self.path.with_extension("tmp");
        #[derive(Serialize)]
        struct PersistedInvocationStore {
            invocations: HashMap<String, AtomInvocation>,
            attachments: HashMap<String, Vec<SupervisionAttachment>>,
        }
        let data = serde_json::to_string(&PersistedInvocationStore {
            invocations: self.invocations.clone(),
            attachments: self.attachments.clone(),
        })?;
        std::fs::write(&tmp, &data)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

pub type SharedInvocationStore = Arc<RwLock<InvocationStore>>;

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_invocation() -> AtomInvocation {
        AtomInvocation::new_profile(
            "inv-001".into(),
            "atom:test@v1".into(),
            None,
            "operator:claude".into(),
            "claude".into(),
            "ses-123".into(),
            Some("/tmp/project".into()),
            "task-456".into(),
        )
    }

    #[test]
    fn new_profile_invocation_has_correct_defaults() {
        let inv = sample_invocation();
        assert_eq!(inv.status, InvocationStatus::Running);
        assert!(inv.is_owner("operator:claude"));
        assert!(!inv.is_owner("operator:other"));
        assert!(inv.children.is_empty());
        assert!(inv.errors.is_empty());
        assert!(inv.is_resumable());
    }

    #[test]
    fn add_owner_grants_access() {
        let mut inv = sample_invocation();
        inv.add_owner("operator:other");
        assert!(inv.is_owner("operator:other"));
        assert!(inv.is_owner("operator:claude"));
    }

    #[test]
    fn deterministic_not_resumable() {
        let mut inv = sample_invocation();
        inv.handle = AtomHandle::Deterministic {
            runner: "test-runner".into(),
        };
        assert!(!inv.is_resumable());
    }

    #[test]
    fn trace_envelope_shape() {
        let inv = sample_invocation();
        let env = inv.to_trace_envelope();
        assert_eq!(env["invocation_id"], "inv-001");
        assert_eq!(env["atom_ref"], "atom:test@v1");
        assert_eq!(env["state"], "running");
        assert_eq!(env["implementation_kind"], "profile");
        assert!(env["started_at"].is_string());
        assert!(env["ended_at"].is_null());
        assert_eq!(env["output_shape"]["schema_ref"], "outputs.schema");
        assert!(env["decision_points"].is_array());
        assert_eq!(env["cost"]["dispatched_runs"], 1);
    }

    #[test]
    fn store_insert_get_update() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("invocations.json");
        let mut store = InvocationStore::new(path.clone());
        let inv = sample_invocation();
        store.insert(inv.clone());
        assert!(store.get("inv-001").is_some());

        let store2 = InvocationStore::new(path);
        assert!(store2.get("inv-001").is_some());
        assert_eq!(store2.get("inv-001").unwrap().atom_ref, "atom:test@v1");
    }

    #[test]
    fn attachment_records_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("invocations.json");
        let mut store = InvocationStore::new(path.clone());
        let mut inv = sample_invocation();
        inv.invocation_id = "parent-1".into();
        store.insert(inv);
        store.insert_attachment(SupervisionAttachment {
            supervision_run_id: "run-1".into(),
            primary_invocation_id: "parent-1".into(),
            primary_task_id: "task-1".into(),
            classifier_invocation_id: Some("classifier-1".into()),
            advisor_invocation_id: None,
            attempt: 1,
        });

        let loaded = InvocationStore::new(path);
        let attachment = loaded
            .attachment_for_primary_attempt("parent-1", 1)
            .expect("attachment should be loaded");
        assert_eq!(attachment.primary_task_id, "task-1");
        assert_eq!(
            attachment.classifier_invocation_id.as_deref(),
            Some("classifier-1")
        );
    }

    #[test]
    fn latest_attachment_respects_attempt_number() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("invocations.json");
        let mut store = InvocationStore::new(path);
        store.insert_attachment(SupervisionAttachment {
            supervision_run_id: "run-1".into(),
            primary_invocation_id: "primary-1".into(),
            primary_task_id: "task-1".into(),
            classifier_invocation_id: Some("classifier-1".into()),
            advisor_invocation_id: None,
            attempt: 1,
        });
        store.insert_attachment(SupervisionAttachment {
            supervision_run_id: "run-1".into(),
            primary_invocation_id: "primary-1".into(),
            primary_task_id: "task-2".into(),
            classifier_invocation_id: Some("classifier-1".into()),
            advisor_invocation_id: None,
            attempt: 2,
        });

        let latest = store.latest_attachment_for_primary("primary-1").unwrap();
        assert_eq!(latest.attempt, 2);
    }

    #[test]
    fn ownership_denial() {
        let mut inv = sample_invocation();
        assert!(!inv.is_owner("unknown"));
        inv.status = InvocationStatus::Succeeded;
        inv.add_owner("operator:other");
        assert!(inv.is_owner("operator:other"));
    }

    #[test]
    fn status_lifecycle() {
        let mut inv = sample_invocation();
        assert_eq!(inv.status, InvocationStatus::Running);
        inv.status = InvocationStatus::Succeeded;
        inv.ended_at = Some(now_iso());
        inv.summary = Some("completed".into());
        assert_eq!(inv.status, InvocationStatus::Succeeded);
        assert!(inv.ended_at.is_some());
    }
}
