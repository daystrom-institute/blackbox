//! `FleetOrchestrator` — the daemon-free façade the `bro fleet` cockpit drives.
//!
//! The cockpit links the `blackbox` lib and spawns top-level entrypoint agents
//! **in-process** — no HTTP to a running `blackboxd` (design
//! `design/orchestration/fleet-tui.md` §3). This façade owns the three plain
//! values `spawn_task` needs — a `TaskStore`, a tail `broadcast::Sender`, and a
//! `store_dir` — and hands the cockpit a single `dispatch` entry point plus a
//! tail subscription. Ownership is clean: the cockpit owns exactly the tasks it
//! spawned (it keeps the returned `Arc<Task>` handles), so the façade stays
//! intentionally thin.
//!
//! Net-new item 7 in the design's "what needs to be added" list. The keystone
//! bidirectional control protocol (persistent stdin, `control_request`,
//! `/compact`) is **not** here — that is harness + dispatch-seam work tracked
//! separately. v1 dispatch reuses the existing one-shot `build_exec_args` /
//! `spawn_task` path so the cockpit shell is buildable and runnable today; live
//! steering lands once the bidirectional seam exists.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::RwLock;
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::sync::{Mutex as AsyncMutex, broadcast};

use super::providers::ExecOpts;
use super::{Task, TaskStore, spawn_task, spawn_task_interactive};

// Re-export the consumer-facing types so the `bro fleet` cockpit depends only
// on `blackbox::fleet::*` and never reaches into the crate-private
// `orchestration` module directly. `Task` itself is NOT re-exported — the
// cockpit handles agents through the opaque [`AgentHandle`] and reads state via
// [`TaskSnapshot`], so the crate-private `TaskInner` (and its private-typed
// fields) never leak into the public API.
pub use super::providers::Provider;
pub use super::tail::TailEvent;
pub use super::TaskStatus;

/// What to dispatch as a new top-level entrypoint agent. The cockpit's
/// composer fills this in; cwd/model are optional and resolved per dispatch
/// (no stickiness on the agent itself — provider is fixed at spawn, §4).
#[derive(Debug, Clone)]
pub struct DispatchSpec {
    pub provider: Provider,
    pub prompt: String,
    pub cwd: Option<String>,
    pub model: Option<String>,
    /// Extra env overrides for the child (e.g. MCP injection wiring). The
    /// cockpit's TUI-local config (§5.2) feeds this; `None` for a bare launch.
    pub env_overrides: Option<HashMap<String, String>>,
}

impl DispatchSpec {
    pub fn new(provider: Provider, prompt: impl Into<String>) -> Self {
        Self {
            provider,
            prompt: prompt.into(),
            cwd: None,
            model: None,
            env_overrides: None,
        }
    }
}

/// Opaque handle to a dispatched entrypoint agent. Wraps the live task; the
/// cockpit holds these (it owns exactly what it spawned) and reads state
/// through [`AgentHandle::snapshot`] without touching crate-private internals.
///
/// For bidi-capable providers the handle also holds the child's writable stdin
/// (behind an async mutex so `&self` steer calls serialize), the channel for
/// driving the persistent session per fleet-tui.md §1.
#[derive(Clone)]
pub struct AgentHandle {
    task: Arc<Task>,
    stdin: Option<Arc<AsyncMutex<tokio::process::ChildStdin>>>,
}

impl AgentHandle {
    pub fn id(&self) -> String {
        self.task.id()
    }

    /// True when this agent runs a persistent bidirectional session and can be
    /// steered (user-turns / control_requests). False for one-shot providers
    /// (Codex et al., §2.1) — steering those is unsupported.
    pub fn can_steer(&self) -> bool {
        self.stdin.is_some()
    }

    /// Write one NDJSON control-plane line to the session's stdin.
    async fn write_line(&self, line: String) -> anyhow::Result<()> {
        let Some(stdin) = &self.stdin else {
            anyhow::bail!("agent is not an interactive session — cannot steer");
        };
        let mut guard = stdin.lock().await;
        guard.write_all(line.as_bytes()).await?;
        guard.write_all(b"\n").await?;
        guard.flush().await?;
        Ok(())
    }

    /// Send a user-turn message (a steer / reply) into the live session (§1.1).
    /// Queues at the agent's next turn boundary if a turn is in flight.
    pub async fn send_user_turn(&self, text: &str) -> anyhow::Result<()> {
        self.write_line(user_turn_ndjson(text)).await
    }

    /// `control_request{interrupt}` — cancel the running turn (§1.1, `Esc`).
    pub async fn interrupt(&self) -> anyhow::Result<()> {
        self.write_line(control_ndjson("interrupt", serde_json::Map::new()))
            .await
    }

    /// `control_request{set_model}` — switch the model for subsequent turns.
    pub async fn set_model(&self, model: &str) -> anyhow::Result<()> {
        let mut extra = serde_json::Map::new();
        extra.insert("model".into(), Value::String(model.to_string()));
        self.write_line(control_ndjson("set_model", extra)).await
    }

    /// `/compact` — an in-stream slash command delivered as a user turn; the
    /// agent emits a `compact_boundary` in response (§1.1, §2.4).
    pub async fn compact(&self) -> anyhow::Result<()> {
        self.send_user_turn("/compact").await
    }

    /// Point-in-time copy of the agent's live state, read under one lock.
    pub fn snapshot(&self) -> TaskSnapshot {
        let inner = self.task.inner.lock();
        TaskSnapshot {
            status: inner.status,
            provider: inner.provider,
            session_id: inner.session_id.clone(),
            last_assistant_message: inner.last_assistant_message.clone(),
            report_message: inner.report.as_ref().map(|r| r.message.clone()),
            report_needs: inner.report.as_ref().and_then(|r| r.needs.clone()),
            cost_usd: inner.cost_usd,
            num_turns: inner.num_turns,
            started_at: inner.started_at,
            cwd: inner.cwd.clone(),
            stderr: inner.stderr.clone(),
            model: model_from_events(&inner.events),
        }
    }
}

/// Read-only snapshot of a dispatched agent's live state — the cockpit's window
/// into a task without naming the crate-private `Task`/`TaskInner`.
#[derive(Debug, Clone)]
pub struct TaskSnapshot {
    pub status: TaskStatus,
    pub provider: Provider,
    pub session_id: String,
    pub last_assistant_message: Option<String>,
    pub report_message: Option<String>,
    pub report_needs: Option<String>,
    pub cost_usd: Option<f64>,
    pub num_turns: Option<u64>,
    pub started_at: u64,
    pub cwd: Option<String>,
    pub stderr: String,
    pub model: Option<String>,
}

/// Best-effort model id from an `init`/assistant event in the stream-json buffer.
fn model_from_events(events: &[serde_json::Value]) -> Option<String> {
    events.iter().find_map(|e| {
        e.get("model")
            .or_else(|| e.get("message").and_then(|m| m.get("model")))
            .and_then(|m| m.as_str())
            .map(|s| s.to_string())
    })
}

/// Daemon-free orchestration core for the fleet cockpit. Holds the `TaskStore`,
/// the tail broadcast channel, and the on-disk `store_dir` that `spawn_task`
/// persists task state into.
pub struct FleetOrchestrator {
    task_store: Arc<RwLock<TaskStore>>,
    tail_tx: broadcast::Sender<TailEvent>,
    store_dir: PathBuf,
}

impl FleetOrchestrator {
    /// Construct over an explicit `store_dir`. Tests and embedders use this;
    /// the cockpit normally goes through [`FleetOrchestrator::from_config`].
    pub fn new(store_dir: PathBuf) -> Self {
        let (tail_tx, _rx) = broadcast::channel(1024);
        Self {
            task_store: Arc::new(RwLock::new(TaskStore::new())),
            tail_tx,
            store_dir,
        }
    }

    /// Build from the resolved blackbox config, reusing the daemon's
    /// orchestration `store_dir` (`paths.bro_home`) so persisted/resumable
    /// sessions land in the same place the daemon would have written them —
    /// without putting the daemon in the execution path.
    pub fn from_config() -> anyhow::Result<Self> {
        let cfg = crate::config::load()?;
        Ok(Self::new(cfg.paths.bro_home))
    }

    /// Subscribe to the tail stream. Each call returns an independent receiver;
    /// the cockpit forwards these into its (sync) TUI loop the same way
    /// `council_tui` forwards SSE signals.
    pub fn subscribe(&self) -> broadcast::Receiver<TailEvent> {
        self.tail_tx.subscribe()
    }

    /// Handles to every task this orchestrator has spawned. The cockpit
    /// normally keeps the handles returned by [`dispatch`] directly (it owns
    /// exactly what it spawned), so this is a convenience for
    /// recovery/enumeration paths.
    pub fn tasks(&self) -> Vec<AgentHandle> {
        self.task_store
            .read()
            .all_tasks()
            .into_iter()
            .map(|task| AgentHandle { task, stdin: None })
            .collect()
    }

    pub fn store_dir(&self) -> &std::path::Path {
        &self.store_dir
    }

    /// Spawn a new top-level entrypoint agent. Bidi-capable providers (Claude,
    /// GLM, DeepSeek, Brodex) launch a **persistent bidirectional session**
    /// (`--input-format stream-json --replay-user-messages`, keystone §2) with
    /// stdin kept open for steering; other providers fall back to one-shot
    /// dispatch (no steering, §2.1). Returns an [`AgentHandle`] — the cockpit
    /// holds it to read state and drive the session.
    pub fn dispatch(&self, spec: DispatchSpec) -> AgentHandle {
        let task_id = uuid::Uuid::new_v4().to_string();
        let session_id = uuid::Uuid::new_v4().to_string();

        let opts = ExecOpts {
            model: spec.model.clone(),
            effort: None,
            provider_defaults: None,
        };
        let mut args = spec.provider.build_exec_args(
            &spec.prompt,
            &session_id,
            spec.cwd.as_deref(),
            Some(&opts),
        );

        let bidi = provider_supports_bidi(spec.provider);
        if bidi {
            // The initial `-p <prompt>` from build_exec_args becomes the first
            // user turn; subsequent turns/controls ride the open stdin.
            args.push("--input-format".into());
            args.push("stream-json".into());
            args.push("--replay-user-messages".into());
        }

        // Fleet agents are entrypoint agents, not bros — no team/brofile label
        // and no `bro_report` surface (§2.2). bro_label stays None; the cockpit
        // names rows from the initial prompt (§5).
        if bidi {
            let spawned = spawn_task_interactive(
                task_id,
                spec.provider,
                args,
                session_id,
                spec.cwd,
                spec.env_overrides,
                self.store_dir.clone(),
                self.task_store.clone(),
                self.tail_tx.clone(),
                None,
                None,
                None,
            );
            AgentHandle {
                task: spawned.task,
                stdin: spawned.stdin.map(|s| Arc::new(AsyncMutex::new(s))),
            }
        } else {
            let task = spawn_task(
                task_id,
                spec.provider,
                args,
                session_id,
                spec.cwd,
                spec.env_overrides,
                self.store_dir.clone(),
                self.task_store.clone(),
                self.tail_tx.clone(),
                None,
                None,
                None,
            );
            AgentHandle { task, stdin: None }
        }
    }
}

/// Providers that speak the persistent bidirectional stream-json control
/// protocol: Claude (CLI bidi mode) and the bro-harness providers GLM /
/// DeepSeek / Brodex (§2). Others are one-shot only.
fn provider_supports_bidi(provider: Provider) -> bool {
    matches!(
        provider,
        Provider::Claude | Provider::Glm | Provider::Deepseek | Provider::Brodex
    )
}

/// One NDJSON user-turn message for the harness/Claude input stream
/// (`{"type":"user","message":{"role":"user","content":"…"}}`).
fn user_turn_ndjson(text: &str) -> String {
    serde_json::json!({
        "type": "user",
        "message": { "role": "user", "content": text },
    })
    .to_string()
}

/// One NDJSON `control_request` line with a fresh `request_id`. `extra` carries
/// subtype-specific fields (e.g. `model` for `set_model`).
fn control_ndjson(subtype: &str, extra: serde_json::Map<String, Value>) -> String {
    let mut obj = serde_json::Map::new();
    obj.insert("type".into(), Value::String("control_request".into()));
    obj.insert(
        "request_id".into(),
        Value::String(uuid::Uuid::new_v4().to_string()),
    );
    obj.insert("subtype".into(), Value::String(subtype.into()));
    obj.extend(extra);
    Value::Object(obj).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_orchestrator_has_no_tasks() {
        let orch = FleetOrchestrator::new(std::env::temp_dir().join("bbox-fleet-test"));
        assert!(orch.tasks().is_empty());
        // subscribe must yield a live receiver without a prior dispatch.
        let _rx = orch.subscribe();
    }

    #[test]
    fn bidi_capability_gate() {
        for p in [
            Provider::Claude,
            Provider::Glm,
            Provider::Deepseek,
            Provider::Brodex,
        ] {
            assert!(provider_supports_bidi(p), "{p} should be bidi-capable");
        }
        for p in [Provider::Codex, Provider::Gemini, Provider::Inception] {
            assert!(!provider_supports_bidi(p), "{p} should be one-shot");
        }
    }

    #[test]
    fn user_turn_ndjson_shape() {
        let line = user_turn_ndjson("hi there");
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["type"], "user");
        assert_eq!(v["message"]["role"], "user");
        assert_eq!(v["message"]["content"], "hi there");
        assert!(!line.contains('\n'), "NDJSON line must be single-line");
    }

    #[test]
    fn control_ndjson_shape() {
        let interrupt = control_ndjson("interrupt", serde_json::Map::new());
        let v: Value = serde_json::from_str(&interrupt).unwrap();
        assert_eq!(v["type"], "control_request");
        assert_eq!(v["subtype"], "interrupt");
        assert!(v["request_id"].as_str().is_some_and(|s| !s.is_empty()));

        let mut extra = serde_json::Map::new();
        extra.insert("model".into(), Value::String("opus".into()));
        let set_model = control_ndjson("set_model", extra);
        let v: Value = serde_json::from_str(&set_model).unwrap();
        assert_eq!(v["subtype"], "set_model");
        assert_eq!(v["model"], "opus");
    }

    #[test]
    fn dispatch_spec_builder_defaults() {
        let spec = DispatchSpec::new(Provider::Claude, "hello");
        assert_eq!(spec.prompt, "hello");
        assert!(spec.cwd.is_none());
        assert!(spec.model.is_none());
    }
}
