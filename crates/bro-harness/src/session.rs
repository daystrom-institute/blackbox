//! Transcript persistence for `--resume`. Stores the transport-native
//! conversation snapshot plus the transport tag (so a resume can refuse a
//! transport mismatch), and a generic loop-level `side` cell.
//!
//! Two persistence planes live in one file:
//!
//! - `snapshot` — transport-native conversation state. Opaque to the loop; the
//!   transport owns its shape.
//! - `side` — transport-agnostic loop-level state that must survive
//!   `exec → resume` (the todo list, diagnostics baselines). Opaque to
//!   *session.rs*; the agent loop owns its shape. Kept a sibling of `snapshot`
//!   (not nested inside it) precisely because it is transport-independent.

use anyhow::{Context, Result};
use bro_capabilities::AgentForkTurns;
use serde_json::{Value, json};
#[cfg(test)]
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

pub struct SessionStore {
    pub id: String,
    path: PathBuf,
    /// Restored snapshot from a prior run, if resuming. `None` for fresh.
    pub restored: Option<Restored>,
}

pub struct Restored {
    pub transport: String,
    /// Model used when the session was created. On resume the daemon does not
    /// re-pass --model (it's implied by the session), so the harness falls
    /// back to this persisted value.
    pub model: Option<String>,
    /// Code-mode the session was created with. Like `model`, the daemon does
    /// not re-pass `--code-mode` on resume — the surface shape is session-
    /// intrinsic (a transcript may contain `exec` cells that depend on it), so
    /// the harness restores this value. `None` for sessions written before this
    /// field existed.
    pub code_mode: Option<String>,
    /// Service tier the session was created/resumed with. `default` is an
    /// explicit standard-routing sentinel; `priority` maps to Codex `/fast`.
    /// `None` for sessions written before this field existed.
    pub service_tier: Option<String>,
    /// Stable prompt-cache identity. Forked children keep their own wire
    /// session ID while sharing this value with their root session.
    pub prompt_cache_root: Option<String>,
    pub snapshot: Value,
    /// Loop-level side cells from the prior run (`Value::Null` if absent, e.g.
    /// sessions written before this field existed).
    pub side: Value,
}

/// Everything persisted at the end of a turn. A struct (rather than a widening
/// arg list) so future loop-level cells extend `side` without churning the
/// `save` signature.
pub struct SaveState<'a> {
    pub transport: &'a str,
    pub model: &'a str,
    pub code_mode: &'a str,
    pub service_tier: Option<&'a str>,
    pub prompt_cache_root: &'a str,
    pub snapshot: Value,
    pub side: Value,
}

pub(crate) fn sessions_dir() -> PathBuf {
    if let Ok(home) = std::env::var("BRO_HOME") {
        PathBuf::from(home).join("harness-sessions")
    } else {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".bro-harness")
            .join("sessions")
    }
}

/// Internal launch metadata for a fresh child session. These values originate
/// in blackopsd policy labels and are never inferred from model-visible text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryForkRequest {
    pub source_session: String,
    pub turns: AgentForkTurns,
}

pub fn parse_history_fork(
    source_session: Option<&str>,
    fork_turns: Option<&str>,
) -> Result<Option<HistoryForkRequest>> {
    match (source_session, fork_turns) {
        (None, None) => Ok(None),
        (Some(_), None) | (None, Some(_)) => {
            anyhow::bail!("history fork requires both source session and turn policy")
        }
        (Some(source_session), Some(raw_turns)) => {
            validate_session_reference(source_session)?;
            let turns: AgentForkTurns =
                serde_json::from_str(raw_turns).context("invalid history fork turn policy")?;
            if matches!(turns, AgentForkTurns::Recent(0)) {
                anyhow::bail!("history fork recent turn count must be positive");
            }
            Ok(Some(HistoryForkRequest {
                source_session: source_session.to_string(),
                turns,
            }))
        }
    }
}

/// Load a fork source strictly. Resume remains backward-compatible when a
/// requested session is absent, but a fork must never silently become empty.
#[allow(clippy::disallowed_methods)]
pub fn load_fork_source(session_id: &str) -> Result<Restored> {
    validate_session_reference(session_id)?;
    let live_fork = sessions_dir().join(format!("{session_id}.fork.json"));
    match std::fs::read_to_string(&live_fork) {
        Ok(raw) => return parse_restored(&raw).context("parse live fork source session"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("read live fork source session"),
    }
    let primary = sessions_dir().join(format!("{session_id}.json"));
    match std::fs::read_to_string(&primary) {
        Ok(raw) => parse_restored(&raw).context("parse fork source session"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let legacy = legacy_sessions_dir().join(format!("{session_id}.json"));
            let raw = std::fs::read_to_string(&legacy)
                .with_context(|| format!("fork source session '{session_id}' does not exist"))?;
            parse_restored(&raw).context("parse legacy fork source session")
        }
        Err(error) => Err(error).context("read fork source session"),
    }
}

fn validate_session_reference(session_id: &str) -> Result<()> {
    if session_id.is_empty()
        || session_id == "."
        || session_id == ".."
        || session_id.contains('/')
        || session_id.contains('\\')
    {
        anyhow::bail!("invalid fork source session reference");
    }
    Ok(())
}

fn parse_restored(raw: &str) -> Result<Restored> {
    let value: Value = serde_json::from_str(raw)?;
    let transport = value
        .get("transport")
        .and_then(Value::as_str)
        .filter(|transport| !transport.is_empty())
        .context("fork source has no transport")?;
    let snapshot = value
        .get("snapshot")
        .cloned()
        .context("fork source has no snapshot")?;
    Ok(Restored {
        transport: transport.to_string(),
        model: value
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_string),
        code_mode: value
            .get("code_mode")
            .and_then(Value::as_str)
            .map(str::to_string),
        service_tier: value
            .get("service_tier")
            .and_then(Value::as_str)
            .map(str::to_string),
        prompt_cache_root: value
            .get("prompt_cache_root")
            .and_then(Value::as_str)
            .map(str::to_string),
        snapshot,
        side: value.get("side").cloned().unwrap_or(Value::Null),
    })
}

/// Select conversation history for a fresh child. Model-visible World State
/// fragments are removed from every policy so the child rebuilds them from its
/// own environment and capability set. Side-state is never part of this API.
pub fn fork_conversation_snapshot(
    transport: &str,
    snapshot: &Value,
    turns: &AgentForkTurns,
) -> Result<Value> {
    let (items, responses_object) = match transport {
        "anthropic" | "openai-chat" => (
            snapshot
                .as_array()
                .context("fork source snapshot must be a message array")?,
            false,
        ),
        "openai-responses" => {
            if let Some(items) = snapshot.as_array() {
                (items, false)
            } else {
                (
                    snapshot
                        .get("input")
                        .and_then(Value::as_array)
                        .context("fork source Responses snapshot must contain input[]")?,
                    true,
                )
            }
        }
        other => anyhow::bail!("unsupported fork source transport '{other}'"),
    };

    validate_snapshot_items(transport, items)?;
    let conversation: Vec<Value> = items
        .iter()
        .map(sanitize_conversation_item)
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect();
    let selected = match turns {
        AgentForkTurns::None => Vec::new(),
        AgentForkTurns::All => conversation,
        AgentForkTurns::Recent(count) => {
            let user_turns: Vec<usize> = conversation
                .iter()
                .enumerate()
                .filter_map(|(index, item)| is_genuine_user_turn(item).then_some(index))
                .collect();
            match user_turns.len().checked_sub(*count as usize) {
                Some(offset) => conversation[user_turns[offset]..].to_vec(),
                None => match user_turns.first() {
                    Some(first) => conversation[*first..].to_vec(),
                    None => Vec::new(),
                },
            }
        }
    };

    if transport == "openai-responses" && responses_object {
        Ok(json!({"input": selected, "ambient_hash": Value::Null}))
    } else {
        Ok(Value::Array(selected))
    }
}

fn validate_snapshot_items(transport: &str, items: &[Value]) -> Result<()> {
    for item in items {
        let object = item
            .as_object()
            .context("fork source snapshot contains a non-object item")?;
        if transport == "openai-responses" {
            let item_type = object
                .get("type")
                .and_then(Value::as_str)
                .context("Responses fork item has no type")?;
            if item_type == "message" {
                validate_message_role(item)?;
            }
        } else {
            validate_message_role(item)?;
        }
    }
    Ok(())
}

fn validate_message_role(item: &Value) -> Result<()> {
    match item.get("role").and_then(Value::as_str) {
        Some("user" | "assistant" | "tool" | "system" | "developer") => Ok(()),
        _ => anyhow::bail!("fork source message has an invalid role"),
    }
}

fn sanitize_conversation_item(item: &Value) -> Result<Option<Value>> {
    match item.get("role").and_then(Value::as_str) {
        Some("developer" | "system") => return Ok(None),
        Some("user") => {}
        _ => return Ok(Some(item.clone())),
    }

    let mut sanitized = item.clone();
    let Some(content) = sanitized.get_mut("content") else {
        return Ok(Some(sanitized));
    };
    match content {
        Value::String(text) => match strip_leading_world_state(text)? {
            Some(remaining) => *text = remaining,
            None => return Ok(None),
        },
        Value::Array(blocks) => {
            let mut retained = Vec::with_capacity(blocks.len());
            for mut block in std::mem::take(blocks) {
                if matches!(
                    block.get("type").and_then(Value::as_str),
                    Some("text" | "input_text")
                ) && let Some(text) = block.get("text").and_then(Value::as_str)
                {
                    if let Some(remaining) = strip_leading_world_state(text)? {
                        block["text"] = Value::String(remaining);
                        retained.push(block);
                    }
                } else {
                    retained.push(block);
                }
            }
            if retained.is_empty() {
                return Ok(None);
            }
            *blocks = retained;
        }
        Value::Null => {}
        _ => anyhow::bail!("fork source user content has an invalid shape"),
    }
    Ok(Some(sanitized))
}

fn is_genuine_user_turn(item: &Value) -> bool {
    item.get("role").and_then(Value::as_str) == Some("user")
        && user_text_blocks(item)
            .into_iter()
            .any(|text| !is_harness_synthetic_user_text(text))
}

fn is_harness_synthetic_user_text(text: &str) -> bool {
    let text = text.trim_start();
    text.starts_with("[Earlier conversation compacted to a summary]")
        || text.starts_with("Your previous response contained no visible output")
}

fn user_text_blocks(item: &Value) -> Vec<&str> {
    match item.get("content") {
        Some(Value::String(text)) => vec![text.as_str()],
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|block| {
                matches!(
                    block.get("type").and_then(Value::as_str),
                    Some("text" | "input_text")
                )
                .then(|| block.get("text").and_then(Value::as_str))
                .flatten()
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn strip_leading_world_state(text: &str) -> Result<Option<String>> {
    let mut remaining = text.trim_start();
    loop {
        let closing = if remaining.starts_with("# AGENTS.md instructions for ") {
            Some("</INSTRUCTIONS>")
        } else if remaining.starts_with("<project_instructions_update>") {
            Some("</project_instructions_update>")
        } else if remaining.starts_with("<bbox_scope>") {
            Some("</bbox_scope>")
        } else if remaining.starts_with("<bbox_pins>") {
            Some("</bbox_pins>")
        } else if remaining.starts_with("<environment_context>") {
            Some("</environment_context>")
        } else if remaining.starts_with("<environment_context_update>") {
            Some("</environment_context_update>")
        } else {
            None
        };
        let Some(closing) = closing else {
            break;
        };
        let end = remaining
            .find(closing)
            .with_context(|| format!("unterminated World State fragment ending with {closing}"))?
            + closing.len();
        remaining = remaining[end..].trim_start();
    }
    Ok((!remaining.is_empty()).then(|| remaining.to_string()))
}

/// Legacy sessions directory (~/.bro-harness/sessions) used as a resume
/// fallback when `BRO_HOME` is set and the session file is absent from the
/// BRO_HOME-based dir.
fn legacy_sessions_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".bro-harness")
        .join("sessions")
}

static NONCE: AtomicU64 = AtomicU64::new(0);

/// Atomically write `contents` to `path` using tmp+rename, matching the
/// daemon's `json_store::atomic_write_json_locked` idiom. A crash mid-write
/// leaves at most a stale `.tmp` file; the target is never partially written.
// callers wrap session persists in spawn_blocking (wave 6b).
#[allow(clippy::disallowed_methods)]
pub fn write_atomic(path: &std::path::Path, contents: &str) -> Result<()> {
    use std::io::Write as _;

    let pid = std::process::id();
    let nonce = NONCE.fetch_add(1, Ordering::SeqCst);
    let tmp_path = path.with_extension(format!("json.{pid}.{nonce}.tmp"));

    if let Some(parent) = tmp_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options
        .open(&tmp_path)
        .with_context(|| format!("open session tmp {}", tmp_path.display()))?;
    file.write_all(contents.as_bytes())
        .with_context(|| format!("write session tmp {}", tmp_path.display()))?;
    file.sync_all()
        .with_context(|| format!("sync session tmp {}", tmp_path.display()))?;
    drop(file);

    std::fs::rename(&tmp_path, path).with_context(|| {
        format!(
            "rename session tmp {} → {}",
            tmp_path.display(),
            path.display()
        )
    })?;
    if let Some(parent) = path.parent() {
        std::fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .with_context(|| format!("sync session directory {}", parent.display()))?;
    }

    Ok(())
}

impl SessionStore {
    #[cfg(test)]
    pub(crate) fn at_path_for_test(path: PathBuf) -> Self {
        Self {
            id: "test-session".to_string(),
            path,
            restored: None,
        }
    }

    // one-time session open/resume, before the loop serves turns.
    #[allow(clippy::disallowed_methods)]
    pub fn open(session_id: Option<&str>, resume: Option<&str>) -> Result<Self> {
        let dir = sessions_dir();
        std::fs::create_dir_all(&dir).context("create sessions dir")?;

        if let Some(rid) = resume {
            let path = dir.join(format!("{rid}.json"));
            let restored = match std::fs::read_to_string(&path) {
                Ok(s) => {
                    let v: Value = serde_json::from_str(&s).context("parse resumed session")?;
                    Some(Restored {
                        transport: v["transport"].as_str().unwrap_or_default().to_string(),
                        model: v["model"].as_str().map(str::to_string),
                        code_mode: v["code_mode"].as_str().map(str::to_string),
                        service_tier: v["service_tier"].as_str().map(str::to_string),
                        prompt_cache_root: v["prompt_cache_root"].as_str().map(str::to_string),
                        snapshot: v["snapshot"].clone(),
                        side: v.get("side").cloned().unwrap_or(Value::Null),
                    })
                }
                Err(_) => {
                    // Fall back to legacy ~/.bro-harness/sessions dir when the
                    // session file is absent in the BRO_HOME-based dir, so
                    // every pre-existing session stays resumable.
                    let legacy_path = legacy_sessions_dir().join(format!("{rid}.json"));
                    match std::fs::read_to_string(&legacy_path) {
                        Ok(s) => {
                            let v: Value =
                                serde_json::from_str(&s).context("parse resumed legacy session")?;
                            Some(Restored {
                                transport: v["transport"].as_str().unwrap_or_default().to_string(),
                                model: v["model"].as_str().map(str::to_string),
                                code_mode: v["code_mode"].as_str().map(str::to_string),
                                service_tier: v["service_tier"].as_str().map(str::to_string),
                                prompt_cache_root: v["prompt_cache_root"]
                                    .as_str()
                                    .map(str::to_string),
                                snapshot: v["snapshot"].clone(),
                                side: v.get("side").cloned().unwrap_or(Value::Null),
                            })
                        }
                        Err(_) => None, // absent in both dirs → start clean
                    }
                }
            };
            return Ok(Self {
                id: rid.to_string(),
                path,
                restored,
            });
        }

        let id = match session_id {
            Some(s) if !s.is_empty() && s != "pending" => s.to_string(),
            _ => uuid::Uuid::new_v4().to_string(),
        };
        let path = dir.join(format!("{id}.json"));
        Ok(Self {
            id,
            path,
            restored: None,
        })
    }

    pub fn save(&self, state: &SaveState) -> Result<()> {
        let body = serde_json::to_string(&json!({
            "transport": state.transport,
            "model": state.model,
            "code_mode": state.code_mode,
            "service_tier": state.service_tier,
            "prompt_cache_root": state.prompt_cache_root,
            "snapshot": state.snapshot,
            "side": state.side,
        }))
        .context("serialize session")?;
        write_atomic(&self.path, &body).context("write session")?;
        Ok(())
    }

    /// The filesystem path this store writes to.
    pub fn store_path(&self) -> &PathBuf {
        &self.path
    }

    /// Atomic handoff read by a fresh child launched during the current parent
    /// turn, before the normal end-of-turn session commit is available.
    pub fn fork_source_path(&self) -> PathBuf {
        self.path.with_extension("fork.json")
    }
}

#[cfg(test)]
// Filesystem fixtures intentionally exercise durable session migration and recovery.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use bro_protocol::SERVICE_TIER_PRIORITY;

    /// A unique, hermetic session dir under the OS temp dir — no `tempfile`
    /// dep, no process-global env mutation (so it can't race the bin's other
    /// tests). Caller removes it.
    fn unique_dir(tag: &str) -> PathBuf {
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("bro-harness-test-{tag}-{pid}-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Build a store directly against `dir`, mirroring `open`'s fresh-id path
    /// without depending on `sessions_dir()` / env.
    fn store_in(dir: &Path, id: &str) -> SessionStore {
        SessionStore {
            id: id.to_string(),
            path: dir.join(format!("{id}.json")),
            restored: None,
        }
    }

    /// Mirror `open`'s resume path against an explicit `dir`.
    fn resume_in(dir: &Path, id: &str) -> SessionStore {
        let path = dir.join(format!("{id}.json"));
        let restored = std::fs::read_to_string(&path).ok().map(|s| {
            let v: Value = serde_json::from_str(&s).unwrap();
            Restored {
                transport: v["transport"].as_str().unwrap_or_default().to_string(),
                model: v["model"].as_str().map(str::to_string),
                code_mode: v["code_mode"].as_str().map(str::to_string),
                service_tier: v["service_tier"].as_str().map(str::to_string),
                prompt_cache_root: v["prompt_cache_root"].as_str().map(str::to_string),
                snapshot: v["snapshot"].clone(),
                side: v.get("side").cloned().unwrap_or(Value::Null),
            }
        });
        SessionStore {
            id: id.to_string(),
            path,
            restored,
        }
    }

    #[test]
    fn side_cell_round_trips_through_save_and_resume() {
        let dir = unique_dir("side");
        let store = store_in(&dir, "sess-1");
        store
            .save(&SaveState {
                transport: "anthropic",
                model: "m",
                code_mode: "only",
                service_tier: Some(SERVICE_TIER_PRIORITY),
                prompt_cache_root: "cache-root-1",
                snapshot: json!({"msgs": 1}),
                side: json!({"todos": []}),
            })
            .unwrap();

        let r = resume_in(&dir, "sess-1").restored.expect("restored");
        assert_eq!(r.transport, "anthropic");
        assert_eq!(r.model.as_deref(), Some("m"));
        assert_eq!(r.code_mode.as_deref(), Some("only"));
        assert_eq!(r.service_tier.as_deref(), Some(SERVICE_TIER_PRIORITY));
        assert_eq!(r.prompt_cache_root.as_deref(), Some("cache-root-1"));
        assert_eq!(r.snapshot, json!({"msgs": 1}));
        assert_eq!(r.side["todos"], json!([]));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn legacy_session_without_side_restores_as_null() {
        let dir = unique_dir("legacy");
        std::fs::write(
            dir.join("old.json"),
            r#"{"transport":"anthropic","model":"m","snapshot":{"x":1}}"#,
        )
        .unwrap();

        let r = resume_in(&dir, "old").restored.expect("restored");
        assert_eq!(r.snapshot, json!({"x": 1}));
        assert_eq!(r.side, Value::Null);
        // A session written before code_mode existed restores it as absent.
        assert_eq!(r.code_mode, None);
        // A session written before service_tier existed restores it as absent.
        assert_eq!(r.service_tier, None);
        assert_eq!(r.prompt_cache_root, None);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn recent_fork_keeps_exact_genuine_user_turn_suffix_and_following_items() {
        let context = json!({
            "role": "user",
            "content": [{"type":"text", "text":"<environment_context>old</environment_context>"}],
        });
        let turn_1 = json!({"role":"user", "content":[{"type":"text", "text":"turn one"}]});
        let answer_1 = json!({"role":"assistant", "content":[{"type":"text", "text":"one"}]});
        let turn_2 = json!({"role":"user", "content":[{"type":"text", "text":"turn two"}]});
        let tool_call = json!({
            "role":"assistant",
            "content":[{"type":"tool_use", "id":"call-1", "name":"read", "input":{}}],
        });
        let tool_result = json!({
            "role":"user",
            "content":[{"type":"tool_result", "tool_use_id":"call-1", "content":"ok"}],
        });
        let answer_2 = json!({"role":"assistant", "content":[{"type":"text", "text":"two"}]});
        let delta = json!({
            "role": "user",
            "content": [{"type":"text", "text":"<environment_context_update>new</environment_context_update>"}],
        });
        let turn_3 = json!({"role":"user", "content":[{"type":"text", "text":"turn three"}]});
        let answer_3 = json!({"role":"assistant", "content":[{"type":"text", "text":"three"}]});
        let source = json!([
            context,
            turn_1,
            answer_1,
            turn_2,
            tool_call,
            tool_result,
            answer_2,
            delta,
            turn_3,
            answer_3,
        ]);

        let forked =
            fork_conversation_snapshot("anthropic", &source, &AgentForkTurns::Recent(2)).unwrap();

        assert_eq!(
            forked,
            json!([turn_2, tool_call, tool_result, answer_2, turn_3, answer_3])
        );
    }

    #[test]
    fn fork_none_is_empty_and_all_rebuilds_responses_world_state() {
        let source = json!({
            "input": [
                {"type":"message", "role":"developer", "content":[{"type":"input_text", "text":"stale tools"}]},
                {"type":"message", "role":"user", "content":[
                    {"type":"input_text", "text":"# AGENTS.md instructions for /old\n\n<INSTRUCTIONS>\nstale\n</INSTRUCTIONS>"},
                    {"type":"input_text", "text":"<bbox_scope>old</bbox_scope>"},
                    {"type":"input_text", "text":"do work"}
                ]},
                {"type":"message", "role":"assistant", "content":[{"type":"output_text", "text":"done"}]}
            ],
            "ambient_hash": 42,
        });

        assert_eq!(
            fork_conversation_snapshot("openai-responses", &source, &AgentForkTurns::None).unwrap(),
            json!({"input": [], "ambient_hash": Value::Null})
        );
        assert_eq!(
            fork_conversation_snapshot("openai-responses", &source, &AgentForkTurns::All).unwrap(),
            json!({
                "input": [
                    {"type":"message", "role":"user", "content":[
                        {"type":"input_text", "text":"do work"}
                    ]},
                    {"type":"message", "role":"assistant", "content":[{"type":"output_text", "text":"done"}]}
                ],
                "ambient_hash": Value::Null,
            })
        );
    }

    #[test]
    fn chat_fork_strips_collapsed_world_state_prefix_but_preserves_task() {
        let source = json!([
            {
                "role":"user",
                "content":"# AGENTS.md instructions for /old\n\n<INSTRUCTIONS>\nstale\n</INSTRUCTIONS>\n\n<bbox_scope>old</bbox_scope>\n\n<environment_context>\n  <cwd>/old</cwd>\n</environment_context>\n\nkeep this task"
            },
            {"role":"assistant", "content":"kept answer"}
        ]);

        assert_eq!(
            fork_conversation_snapshot("openai-chat", &source, &AgentForkTurns::All).unwrap(),
            json!([
                {"role":"user", "content":"keep this task"},
                {"role":"assistant", "content":"kept answer"}
            ])
        );
    }

    #[test]
    fn fork_metadata_and_snapshot_validation_fail_closed() {
        assert!(parse_history_fork(Some("source"), None).is_err());
        assert!(parse_history_fork(Some("../source"), Some(r#"{"kind":"all"}"#)).is_err());
        assert!(
            parse_history_fork(Some("source"), Some(r#"{"kind":"recent","turns":0}"#)).is_err()
        );
        assert!(fork_conversation_snapshot("anthropic", &json!({}), &AgentForkTurns::All).is_err());
        assert!(fork_conversation_snapshot("unknown", &json!([]), &AgentForkTurns::All).is_err());
    }

    // -----------------------------------------------------------------
    // write_atomic
    // -----------------------------------------------------------------

    #[test]
    fn write_atomic_tmp_rename() {
        let dir = unique_dir("atomic");
        let path = dir.join("session.json");

        write_atomic(&path, r#"{"transport":"test","snapshot":{"n":1}}"#).unwrap();

        // Target exists with the right content.
        let s = std::fs::read_to_string(&path).unwrap();
        assert!(s.contains("\"transport\":\"test\""));
        assert!(s.contains("\"n\":1"));

        // No stray tmp file left behind.
        let tmp_suffix = ".tmp";
        let has_tmp = std::fs::read_dir(&dir)
            .ok()
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok())
                    .any(|e| e.file_name().to_string_lossy().ends_with(tmp_suffix))
            })
            .unwrap_or(false);
        assert!(!has_tmp, "tmp file left behind after write_atomic");

        // A second write succeeds (different nonce, no collision).
        write_atomic(&path, r#"{"transport":"test","snapshot":{"n":2}}"#).unwrap();
        let s2 = std::fs::read_to_string(&path).unwrap();
        assert!(s2.contains("\"n\":2"));

        std::fs::remove_dir_all(&dir).ok();
    }

    // -----------------------------------------------------------------
    // sessions_dir / BRO_HOME
    // -----------------------------------------------------------------

    /// Restore an env var on drop.
    struct EnvGuard {
        key: &'static str,
        prior: Option<String>,
    }

    impl EnvGuard {
        fn push(key: &'static str, value: &str) -> Self {
            let prior = std::env::var(key).ok();
            unsafe { std::env::set_var(key, value) }
            EnvGuard { key, prior }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.prior {
                Some(v) => unsafe { std::env::set_var(self.key, v) },
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
    }

    #[test]
    fn sessions_dir_honors_bro_home() {
        let dir = unique_dir("bro-home");
        let _guard = EnvGuard::push("BRO_HOME", &dir.to_string_lossy());

        // open (fresh) creates the sessions dir under BRO_HOME/harness-sessions
        let store = SessionStore::open(None, None).unwrap();
        let sp = store.store_path();
        let expected_dir = dir.join("harness-sessions");
        assert!(
            sp.starts_with(&expected_dir),
            "store path {sp:?} should start with {expected_dir:?}"
        );
        // The dir was created.
        assert!(expected_dir.exists());

        // Write through the atomic path; file lands in the right place.
        store
            .save(&SaveState {
                transport: "t",
                model: "m",
                code_mode: "only",
                service_tier: None,
                prompt_cache_root: "sess-root",
                snapshot: json!({"x": 1}),
                side: Value::Null,
            })
            .unwrap();
        assert!(sp.exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resume_falls_back_to_legacy_dir() {
        let new_dir = unique_dir("new");
        let legacy_base = unique_dir("legacy-home");

        // Simulate legacy ~/.bro-harness/sessions with a session file.
        let legacy_sessions = legacy_base.join(".bro-harness").join("sessions");
        std::fs::create_dir_all(&legacy_sessions).unwrap();
        std::fs::write(
            legacy_sessions.join("old-session.json"),
            r#"{"transport":"anthropic","model":"legacy-m","code_mode":"only","snapshot":{"msgs":1},"side":null}"#,
        )
        .unwrap();

        // Point HOME at the legacy base and BRO_HOME at the new dir.
        let _home_guard = EnvGuard::push("HOME", &legacy_base.to_string_lossy());
        let _bro_guard = EnvGuard::push("BRO_HOME", &new_dir.to_string_lossy());

        // Resume — file is absent from new dir, must fall back to legacy.
        let store = SessionStore::open(None, Some("old-session")).unwrap();
        let r = store.restored.as_ref().expect("resumed from legacy dir");
        assert_eq!(r.transport, "anthropic");
        assert_eq!(r.model.as_deref(), Some("legacy-m"));
        assert_eq!(r.snapshot, json!({"msgs": 1}));

        // The store path still points to the new dir for future writes.
        let expected_new_dir = new_dir.join("harness-sessions");
        assert!(
            store.store_path().starts_with(&expected_new_dir),
            "store path {:?} should be under {:?}",
            store.store_path(),
            expected_new_dir
        );

        std::fs::remove_dir_all(&new_dir).ok();
        std::fs::remove_dir_all(&legacy_base).ok();
    }
}
