//! Durable arc checkpoints - the crash-resume substrate for workflow
//! arcs.
//!
//! One JSON file per top-level arc under `<store_dir>/arcs/`, written
//! atomically (tmp + rename) at every node boundary and at Wait
//! registration, removed when the arc reaches a terminal state. On
//! daemon boot, `load_all` returns the surviving checkpoints so the
//! rehydration pass can re-park Wait-suspended arcs and mark
//! mid-dispatch arcs interrupted.
//!
//! Scope (v1): only top-level arcs checkpoint (composition_depth == 0,
//! no parent). A sub-workflow in flight at crash time surfaces as its
//! parent's `running` checkpoint, which rehydration marks interrupted
//! rather than silently re-running non-idempotent node bodies.
//!
//! Durability claim: restart-safety, not power-loss safety. Files are
//! flushed and renamed but parent-directory fsync is best-effort.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::io::AsyncWriteExt;

use super::Workflow;
use super::context::ArcContext;

/// Bump when the checkpoint shape changes incompatibly. Loaders skip
/// (and log) checkpoints from a different schema version rather than
/// guessing.
pub const ARC_CHECKPOINT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArcCheckpointStatus {
    /// Arc was between node boundaries (or inside a node body) when
    /// this checkpoint was written. Not safely resumable: node bodies
    /// are not idempotent, so rehydration marks these interrupted.
    Running,
    /// Arc was parked on a Wait node with its registrations already
    /// computed. Safely resumable: re-entering the wait node re-derives
    /// the same correlations from the restored context and the
    /// system-events catch-up replays anything that arrived while the
    /// daemon was down.
    Waiting,
    /// Stamped by rehydration when a non-resumable checkpoint is found
    /// at boot. Kept on disk for operator triage; cleared when the arc
    /// is resumed manually or removed.
    Interrupted,
}

/// Serialized runner state - everything `WorkflowRunner` needs to
/// reconstruct itself minus live handles (in-flight fork tasks, notify
/// slots, cancel tokens). `in_flight_nodes` records the *names* of
/// live fork dispatches so rehydration can refuse to resume an arc
/// whose forked work died with the process.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArcCheckpoint {
    pub schema_version: u32,
    pub arc_id: String,
    /// Full workflow spec embedded at arc start. Registry drift between
    /// install and resume cannot change a running arc's program.
    pub workflow: Workflow,
    pub project_dir: Option<String>,
    pub ctx: ArcContext,
    pub node_outputs: HashMap<String, String>,
    pub actor_sessions: HashMap<String, String>,
    pub ensemble_sessions: HashMap<String, HashMap<String, String>>,
    pub visit_counts: HashMap<String, u32>,
    pub last_verdict: Option<String>,
    /// Steps consumed so far; resume continues the max_steps budget
    /// rather than resetting it.
    pub steps: usize,
    pub max_steps: usize,
    /// The node the arc will run next (status Running) or is parked on
    /// (status Waiting).
    pub current_node: String,
    pub status: ArcCheckpointStatus,
    pub in_flight_nodes: Vec<String>,
    pub arc_thread_id: Option<String>,
    pub saved_at: String,
}

/// Registered-wait descriptor derivable from a Waiting checkpoint.
/// The in-memory WaitStore is rebuilt by re-entering the wait node on
/// resume, so this is observational (peek/status surfaces), not a
/// second source of truth.
#[derive(Debug, Clone, Serialize)]
pub struct CheckpointWaitView {
    pub arc_id: String,
    pub node: String,
    pub correlation: Map<String, Value>,
}

/// Disk store for arc checkpoints. All I/O is async; callers hold no
/// locks across these awaits. Per-arc write ordering is guaranteed by
/// the engine (checkpoints are awaited inline in the single arc-runner
/// future), so concurrent writers per file cannot happen.
#[derive(Debug)]
pub struct ArcStore {
    dir: PathBuf,
}

impl ArcStore {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    fn path_for(&self, arc_id: &str) -> PathBuf {
        // Arc ids are engine-minted (`arc-<uuid>`), but harden against
        // path separators anyway since ids also arrive via resume.
        let safe: String = arc_id
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
            .collect();
        self.dir.join(format!("{safe}.json"))
    }

    pub async fn save(&self, cp: &ArcCheckpoint) -> Result<()> {
        tokio::fs::create_dir_all(&self.dir)
            .await
            .with_context(|| format!("create arc checkpoint dir {:?}", self.dir))?;
        let path = self.path_for(&cp.arc_id);
        let tmp = path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(cp).context("serialize arc checkpoint")?;
        let mut f = tokio::fs::File::create(&tmp)
            .await
            .with_context(|| format!("create {tmp:?}"))?;
        f.write_all(&bytes).await?;
        f.sync_all().await?;
        drop(f);
        tokio::fs::rename(&tmp, &path)
            .await
            .with_context(|| format!("publish {path:?}"))?;
        Ok(())
    }

    pub async fn remove(&self, arc_id: &str) -> Result<()> {
        let path = self.path_for(arc_id);
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("remove {path:?}")),
        }
    }

    /// Load every parseable checkpoint. Malformed or version-mismatched
    /// files are skipped with a warning - a corrupt checkpoint must not
    /// block boot, and the arc thread still carries the audit trail.
    pub async fn load_all(&self) -> Vec<ArcCheckpoint> {
        let mut out = Vec::new();
        let mut entries = match tokio::fs::read_dir(&self.dir).await {
            Ok(e) => e,
            Err(_) => return out,
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let bytes = match tokio::fs::read(&path).await {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!("arc checkpoint {path:?} unreadable: {e}");
                    continue;
                }
            };
            match serde_json::from_slice::<ArcCheckpoint>(&bytes) {
                Ok(cp) if cp.schema_version == ARC_CHECKPOINT_SCHEMA_VERSION => out.push(cp),
                Ok(cp) => {
                    tracing::warn!(
                        "arc checkpoint {path:?} has schema_version {} (want {}); skipping",
                        cp.schema_version,
                        ARC_CHECKPOINT_SCHEMA_VERSION
                    );
                }
                Err(e) => {
                    tracing::warn!("arc checkpoint {path:?} unparseable: {e}");
                }
            }
        }
        out
    }

    /// Stamp a checkpoint interrupted in place (rehydration found it
    /// non-resumable). Keeps the file for operator triage.
    pub async fn mark_interrupted(&self, arc_id: &str) -> Result<()> {
        let path = self.path_for(arc_id);
        let bytes = tokio::fs::read(&path)
            .await
            .with_context(|| format!("read {path:?}"))?;
        let mut cp: ArcCheckpoint =
            serde_json::from_slice(&bytes).context("parse arc checkpoint")?;
        cp.status = ArcCheckpointStatus::Interrupted;
        cp.saved_at = crate::util::now_iso();
        self.save(&cp).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::context::ArcMeta;

    fn minimal_workflow() -> Workflow {
        serde_json::from_value(serde_json::json!({
            "name": "cp-test",
            "version": 1,
            "start": "A",
            "actors": {},
            "nodes": {
                "A": {"actor": "", "next": {"type": "terminal"}}
            }
        }))
        .expect("minimal workflow parses")
    }

    fn checkpoint(arc_id: &str) -> ArcCheckpoint {
        ArcCheckpoint {
            schema_version: ARC_CHECKPOINT_SCHEMA_VERSION,
            arc_id: arc_id.into(),
            workflow: minimal_workflow(),
            project_dir: None,
            ctx: ArcContext::new(ArcMeta {
                arc_id: arc_id.into(),
                workflow_name: "cp-test".into(),
                workflow_version: 1,
                started_at: crate::util::now_iso(),
                project_dir: None,
                worktree: None,
                arc_outcome: None,
                parent_arc_id: None,
                composition_depth: 0,
            }),
            node_outputs: HashMap::new(),
            actor_sessions: HashMap::new(),
            ensemble_sessions: HashMap::new(),
            visit_counts: HashMap::new(),
            last_verdict: None,
            steps: 1,
            max_steps: 50,
            current_node: "A".into(),
            status: ArcCheckpointStatus::Waiting,
            in_flight_nodes: Vec::new(),
            arc_thread_id: Some("thread-00000000".into()),
            saved_at: crate::util::now_iso(),
        }
    }

    #[tokio::test]
    async fn save_load_remove_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let store = ArcStore::new(root.join("arcs"));
        let cp = checkpoint("arc-roundtrip");
        store.save(&cp).await.unwrap();
        let loaded = store.load_all().await;
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].arc_id, "arc-roundtrip");
        assert_eq!(loaded[0].status, ArcCheckpointStatus::Waiting);
        assert_eq!(loaded[0].workflow.name, "cp-test");
        store.remove("arc-roundtrip").await.unwrap();
        assert!(store.load_all().await.is_empty());
        // Second remove is a no-op, not an error.
        store.remove("arc-roundtrip").await.unwrap();
    }

    #[tokio::test]
    async fn load_all_skips_garbage_and_wrong_versions() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let store = ArcStore::new(root.join("arcs"));
        store.save(&checkpoint("arc-good")).await.unwrap();
        tokio::fs::write(root.join("arcs").join("junk.json"), b"{not json")
            .await
            .unwrap();
        let mut old = checkpoint("arc-old");
        old.schema_version = ARC_CHECKPOINT_SCHEMA_VERSION + 1;
        store.save(&old).await.unwrap();
        let loaded = store.load_all().await;
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].arc_id, "arc-good");
    }

    #[tokio::test]
    async fn mark_interrupted_rewrites_status() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let store = ArcStore::new(root.join("arcs"));
        let mut cp = checkpoint("arc-int");
        cp.status = ArcCheckpointStatus::Running;
        store.save(&cp).await.unwrap();
        store.mark_interrupted("arc-int").await.unwrap();
        let loaded = store.load_all().await;
        assert_eq!(loaded[0].status, ArcCheckpointStatus::Interrupted);
    }
}
