//! Bro council — TUI-driven multi-peer chat backend.
//!
//! A council is a conversational coordination log bound to a team:
//! the user posts turns, every team member receives an inbox envelope,
//! a per-(council × bro) drain worker dispatches each envelope serially
//! via `bro_exec` (first turn) / `bro_resume` (subsequent), the reply
//! is appended as a new post, and `@-mentions` cascade through the
//! roster with relay-depth + dedupe + fanout caps.
//!
//! Boundary vs whiteboard: council = conversational coordination log,
//! whiteboard = structured decision artifact. If a council reaches a
//! crystallized claim worth a durable record, the user (or a designated
//! bro) posts it to a whiteboard separately. Council does not supersede
//! whiteboard; they compose.

pub mod charter;
pub mod drain;
pub mod envelope;
pub mod http;
pub mod post;
pub mod session;

pub use envelope::{EnvelopeStatus, InboxEnvelope};
pub use post::{CouncilPost, ReplyScope};
pub use session::{CouncilSession, CouncilStatus};

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, anyhow};
use parking_lot::{Mutex, RwLock};
use serde::Serialize;
use tokio::sync::{Notify, broadcast};
use tokio_util::sync::CancellationToken;

use crate::orchestration::team::{Team, load_all_teams};
use crate::server::state::SharedState;

const COUNCIL_EVENT_BUFFER: usize = 256;

// ── Registry ──────────────────────────────────────────────────────────

pub type SharedRegistry = Arc<CouncilRegistry>;

#[derive(Default)]
pub struct CouncilRegistry {
    councils: RwLock<HashMap<String, Arc<CouncilState>>>,
    storage_dir: RwLock<Option<PathBuf>>,
    events: RwLock<Option<broadcast::Sender<CouncilEvent>>>,
}

impl CouncilRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the base directory councils are persisted under and restore
    /// any existing councils from disk into in-memory state. Workers
    /// are NOT spawned here — call `respawn_workers_after_restart`
    /// after `SharedState` is fully constructed so the dispatch path
    /// has the full state graph.
    pub fn set_storage_dir(&self, dir: PathBuf) -> Result<()> {
        {
            let slot = self.storage_dir.read();
            if slot.is_some() {
                return Ok(());
            }
        }
        fs::create_dir_all(&dir)
            .with_context(|| format!("create council storage dir {}", dir.display()))?;

        let (events_tx, _) = broadcast::channel(COUNCIL_EVENT_BUFFER);
        *self.events.write() = Some(events_tx);

        if let Ok(entries) = fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                match restore_council(&path) {
                    Ok(state) => {
                        let id = state.session.read().id.clone();
                        self.councils.write().insert(id, Arc::new(state));
                    }
                    Err(e) => {
                        tracing::warn!("council: skip restore of {}: {e:#}", path.display());
                    }
                }
            }
        }

        *self.storage_dir.write() = Some(dir);
        Ok(())
    }

    pub fn subscribe(&self) -> Option<broadcast::Receiver<CouncilEvent>> {
        self.events.read().as_ref().map(|tx| tx.subscribe())
    }

    pub fn emit(&self, ev: CouncilEvent) {
        if let Some(tx) = self.events.read().as_ref() {
            let _ = tx.send(ev);
        }
    }

    pub fn list_ids(&self) -> Vec<String> {
        self.councils.read().keys().cloned().collect()
    }

    pub fn list_summaries(&self, project: Option<&str>) -> Vec<CouncilSummary> {
        self.councils
            .read()
            .values()
            .filter_map(|c| {
                let s = c.session.read();
                if let Some(p) = project {
                    if s.project.as_deref() != Some(p) {
                        return None;
                    }
                }
                Some(CouncilSummary {
                    id: s.id.clone(),
                    team_id: s.team_id.clone(),
                    project: s.project.clone(),
                    topic: s.topic.clone(),
                    status: s.status,
                    members: s.member_sessions.keys().cloned().collect(),
                    created_at: s.created_at.clone(),
                    updated_at: s.updated_at.clone(),
                    post_count: c.posts.read().len() as u64,
                })
            })
            .collect()
    }

    pub fn get(&self, id: &str) -> Option<Arc<CouncilState>> {
        self.councils.read().get(id).cloned()
    }

    pub fn rename_project_refs(&self, old_project: &str, new_project: &str) -> Result<usize> {
        let councils = self.councils.read().values().cloned().collect::<Vec<_>>();
        let mut updated = 0usize;
        for council in councils {
            let mut session = council.session.write();
            if session.project.as_deref() == Some(old_project) {
                session.project = Some(new_project.to_string());
                session.touch();
                drop(session);
                council.persist_session()?;
                updated += 1;
            }
        }
        Ok(updated)
    }

    /// Create a new council. Validates the team exists (members are
    /// resolved at first dispatch via `resolve_brofile`, not here).
    pub fn create(
        &self,
        team_id: String,
        topic: String,
        charter: Option<String>,
        project: Option<String>,
        store_dir: &Path,
    ) -> Result<Arc<CouncilState>> {
        let teams = load_all_teams(store_dir);
        let team = teams
            .iter()
            .find(|t| t.name == team_id)
            .ok_or_else(|| anyhow!("unknown team: {team_id}"))?;
        if team.members.is_empty() {
            return Err(anyhow!("team {team_id} has no members"));
        }

        let id = format!("council-{}", &uuid::Uuid::new_v4().to_string()[..8]);
        let project_resolved = project.or_else(|| team.project_dir.clone());
        let session = CouncilSession::new(
            id.clone(),
            team_id,
            topic,
            charter.unwrap_or_else(|| charter::DEFAULT_CHARTER.to_string()),
            project_resolved,
        );

        let storage_path = self
            .storage_dir
            .read()
            .clone()
            .ok_or_else(|| anyhow!("council storage dir not set"))?
            .join(&id);
        fs::create_dir_all(&storage_path)
            .with_context(|| format!("create {}", storage_path.display()))?;
        fs::create_dir_all(storage_path.join("frames"))?;

        let state = CouncilState::new(session, storage_path)?;
        state.persist_session()?;
        state.persist_envelopes()?;

        let state_arc = Arc::new(state);
        self.councils.write().insert(id, state_arc.clone());
        Ok(state_arc)
    }

    /// Close a council. Cancels all drain workers, marks status closed,
    /// persists. In-memory state is retained so existing GET calls keep
    /// returning the transcript.
    pub fn close(&self, id: &str) -> Result<()> {
        let state = self
            .get(id)
            .ok_or_else(|| anyhow!("unknown council: {id}"))?;
        {
            let workers = state.workers.write();
            for w in workers.values() {
                w.cancel.cancel();
            }
        }
        state.workers.write().clear();
        {
            let mut s = state.session.write();
            s.status = CouncilStatus::Closed;
            s.touch();
        }
        state.persist_session()?;
        self.emit(CouncilEvent::Closed {
            council_id: id.to_string(),
        });
        Ok(())
    }

    /// Walk all restored councils and respawn drain workers for any
    /// `Queued` envelopes. Drain envelopes that were left in `Draining`
    /// at restart are reconciled by `reconcile_draining_at_restart`.
    pub fn respawn_workers_after_restart(self: &Arc<Self>, shared: Arc<SharedState>) {
        let ids: Vec<String> = self.councils.read().keys().cloned().collect();
        for id in ids {
            let Some(state) = self.get(&id) else { continue };
            if state.session.read().status == CouncilStatus::Closed {
                continue;
            }
            state.reconcile_draining_at_restart();
            let bros: Vec<String> = state
                .envelopes
                .read()
                .iter()
                .filter(|e| matches!(e.status, EnvelopeStatus::Queued))
                .map(|e| e.bro_id.clone())
                .collect::<std::collections::HashSet<_>>()
                .into_iter()
                .collect();
            for bro in bros {
                state.ensure_worker(shared.clone(), self.clone(), bro);
            }
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CouncilSummary {
    pub id: String,
    pub team_id: String,
    pub project: Option<String>,
    pub topic: String,
    pub status: CouncilStatus,
    pub members: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
    pub post_count: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CouncilEvent {
    Post {
        council_id: String,
        post: CouncilPost,
    },
    EnvelopeChanged {
        council_id: String,
        envelope_id: String,
        bro_id: String,
        status: EnvelopeStatus,
    },
    Closed {
        council_id: String,
    },
}

// ── CouncilState ──────────────────────────────────────────────────────

pub struct CouncilState {
    pub session: RwLock<CouncilSession>,
    pub envelopes: RwLock<Vec<InboxEnvelope>>,
    pub posts: RwLock<Vec<CouncilPost>>,
    pub posts_writer: Mutex<File>,
    pub next_sequence: AtomicU64,
    pub storage_path: PathBuf,
    pub workers: RwLock<HashMap<String, WorkerHandle>>,
}

pub struct WorkerHandle {
    pub notify: Arc<Notify>,
    pub cancel: CancellationToken,
}

impl CouncilState {
    fn new(session: CouncilSession, storage_path: PathBuf) -> Result<Self> {
        let posts_path = storage_path.join("posts.jsonl");
        let writer = OpenOptions::new()
            .append(true)
            .create(true)
            .open(&posts_path)
            .with_context(|| format!("open posts log {}", posts_path.display()))?;
        Ok(Self {
            session: RwLock::new(session),
            envelopes: RwLock::new(Vec::new()),
            posts: RwLock::new(Vec::new()),
            posts_writer: Mutex::new(writer),
            next_sequence: AtomicU64::new(1),
            storage_path,
            workers: RwLock::new(HashMap::new()),
        })
    }

    pub fn alloc_sequence(&self) -> u64 {
        self.next_sequence.fetch_add(1, Ordering::SeqCst)
    }

    /// Append a post to the in-memory cache + the on-disk jsonl log.
    pub fn append_post(&self, post: CouncilPost) -> Result<()> {
        let line = serde_json::to_string(&post).context("serialize post")?;
        {
            let mut w = self.posts_writer.lock();
            writeln!(*w, "{line}").context("write post line")?;
            w.flush().ok();
        }
        self.posts.write().push(post);
        Ok(())
    }

    pub fn persist_session(&self) -> Result<()> {
        let path = self.storage_path.join("session.json");
        let body = serde_json::to_string_pretty(&*self.session.read())?;
        atomic_write(&path, body.as_bytes())
    }

    pub fn persist_envelopes(&self) -> Result<()> {
        let path = self.storage_path.join("envelopes.json");
        let body = serde_json::to_string_pretty(&*self.envelopes.read())?;
        atomic_write(&path, body.as_bytes())
    }

    pub fn write_frame(&self, envelope_id: &str, body: &str) -> Result<()> {
        let path = self
            .storage_path
            .join("frames")
            .join(format!("{envelope_id}.txt"));
        atomic_write(&path, body.as_bytes())
    }

    /// Walk envelopes; any that were left mid-drain when the daemon
    /// died are decided here. If a post exists referencing the
    /// envelope, mark it done (the reply landed before crash); if the
    /// attempt budget is spent, mark failed; otherwise requeue with
    /// `attempt_count += 1`.
    pub fn reconcile_draining_at_restart(&self) {
        let max_attempts = self.session.read().config.max_attempts;
        let post_envelope_ids: std::collections::HashSet<String> = self
            .posts
            .read()
            .iter()
            .filter_map(|p| p.source_envelope_id.clone())
            .collect();

        let mut envelopes = self.envelopes.write();
        let mut changed = false;
        for env in envelopes.iter_mut() {
            if env.status != EnvelopeStatus::Draining {
                continue;
            }
            changed = true;
            if post_envelope_ids.contains(&env.id) {
                env.status = EnvelopeStatus::Done;
                env.finished_at = Some(chrono::Utc::now().to_rfc3339());
                env.lease_owner = None;
                env.lease_expires_at = None;
                continue;
            }
            env.attempt_count += 1;
            env.lease_owner = None;
            env.lease_expires_at = None;
            if env.attempt_count >= max_attempts {
                env.status = EnvelopeStatus::Failed;
                env.last_error =
                    Some("daemon restarted while draining; attempt budget exhausted".into());
                env.finished_at = Some(chrono::Utc::now().to_rfc3339());
            } else {
                env.status = EnvelopeStatus::Queued;
                env.last_error = Some("daemon restarted while draining".into());
            }
        }
        drop(envelopes);
        if changed {
            let _ = self.persist_envelopes();
        }
    }

    /// Lazily spawn a drain worker for `bro_id` if one is not already
    /// running, then notify it. Idempotent — the worker map is the
    /// authoritative liveness check.
    pub fn ensure_worker(
        self: &Arc<Self>,
        shared: Arc<SharedState>,
        registry: SharedRegistry,
        bro_id: String,
    ) {
        let mut workers = self.workers.write();
        if let Some(handle) = workers.get(&bro_id) {
            handle.notify.notify_one();
            return;
        }
        let notify = Arc::new(Notify::new());
        let cancel = CancellationToken::new();
        let handle = WorkerHandle {
            notify: notify.clone(),
            cancel: cancel.clone(),
        };
        workers.insert(bro_id.clone(), handle);
        drop(workers);

        let council = self.clone();
        tokio::spawn(async move {
            drain::drain_loop(shared, registry, council, bro_id, notify, cancel).await;
        });
    }
}

// ── Persistence helpers ───────────────────────────────────────────────

pub fn atomic_write(path: &Path, body: &[u8]) -> Result<()> {
    let tmp = path.with_extension(match path.extension() {
        Some(ext) => format!("{}.tmp", ext.to_string_lossy()),
        None => "tmp".to_string(),
    });
    fs::write(&tmp, body).with_context(|| format!("write {}", tmp.display()))?;
    fs::rename(&tmp, path)
        .with_context(|| format!("rename {} → {}", tmp.display(), path.display()))?;
    Ok(())
}

fn restore_council(dir: &Path) -> Result<CouncilState> {
    let session_path = dir.join("session.json");
    let session_bytes =
        fs::read(&session_path).with_context(|| format!("read {}", session_path.display()))?;
    let session: CouncilSession = serde_json::from_slice(&session_bytes)
        .with_context(|| format!("parse {}", session_path.display()))?;

    let envelopes_path = dir.join("envelopes.json");
    let envelopes: Vec<InboxEnvelope> = if envelopes_path.exists() {
        serde_json::from_slice(&fs::read(&envelopes_path)?)?
    } else {
        Vec::new()
    };

    let posts_path = dir.join("posts.jsonl");
    let mut posts = Vec::new();
    if posts_path.exists() {
        let f = File::open(&posts_path)?;
        for line in BufReader::new(f).lines().map_while(Result::ok) {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<CouncilPost>(&line) {
                Ok(p) => posts.push(p),
                Err(e) => tracing::warn!("council {}: skip malformed post line: {e}", session.id),
            }
        }
    }

    let writer = OpenOptions::new()
        .append(true)
        .create(true)
        .open(&posts_path)?;
    let next_seq = posts.iter().map(|p| p.sequence).max().unwrap_or(0) + 1;

    let state = CouncilState {
        session: RwLock::new(session),
        envelopes: RwLock::new(envelopes),
        posts: RwLock::new(posts),
        posts_writer: Mutex::new(writer),
        next_sequence: AtomicU64::new(next_seq),
        storage_path: dir.to_path_buf(),
        workers: RwLock::new(HashMap::new()),
    };
    Ok(state)
}

// ── User post entry point ─────────────────────────────────────────────

/// Append a user turn to the council, fan out one envelope per team
/// member, and notify (or spawn) their drain workers.
pub async fn post_user_turn(
    shared: Arc<SharedState>,
    registry: SharedRegistry,
    council_id: &str,
    sender: &str,
    body: &str,
) -> Result<u64> {
    let state = registry
        .get(council_id)
        .ok_or_else(|| anyhow!("unknown council: {council_id}"))?;

    if state.session.read().status == CouncilStatus::Closed {
        return Err(anyhow!("council {council_id} is closed"));
    }

    let team_id = state.session.read().team_id.clone();
    let teams = load_all_teams(&shared.store_dir);
    let team: &Team = teams
        .iter()
        .find(|t| t.name == team_id)
        .ok_or_else(|| anyhow!("council {council_id}: team {team_id} missing"))?;

    let seq = state.alloc_sequence();
    let post = CouncilPost::new_user(
        council_id.to_string(),
        seq,
        sender.to_string(),
        body.to_string(),
    );
    let mentions: Vec<String> = post.addressed_to.clone();
    state.append_post(post.clone())?;

    {
        let mut s = state.session.write();
        s.touch();
    }
    state.persist_session()?;

    // One envelope per team member. addressed_by_user tracks @-mentions
    // from the user; bros not mentioned still receive the turn (they
    // may riff under cosession semantics).
    let mut new_envelopes = Vec::with_capacity(team.members.len());
    for member in &team.members {
        let env_id = format!("env-{}", &uuid::Uuid::new_v4().to_string()[..8]);
        let addressed = mentions.iter().any(|m| m == &member.name);
        let env = InboxEnvelope::new_queued(
            env_id,
            council_id.to_string(),
            member.name.clone(),
            ReplyScope::Direct { seq },
            addressed,
            false,
            Some(seq),
            0,
        );
        new_envelopes.push(env);
    }
    {
        let mut envs = state.envelopes.write();
        envs.extend(new_envelopes.iter().cloned());
    }
    state.persist_envelopes()?;

    registry.emit(CouncilEvent::Post {
        council_id: council_id.to_string(),
        post,
    });
    for env in &new_envelopes {
        registry.emit(CouncilEvent::EnvelopeChanged {
            council_id: council_id.to_string(),
            envelope_id: env.id.clone(),
            bro_id: env.bro_id.clone(),
            status: env.status,
        });
    }

    for member in &team.members {
        state.ensure_worker(shared.clone(), registry.clone(), member.name.clone());
    }

    Ok(seq)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tempfile::TempDir;

    #[test]
    fn registry_storage_dir_idempotent() {
        let tmp = TempDir::new().unwrap();
        let reg = Arc::new(CouncilRegistry::new());
        reg.set_storage_dir(tmp.path().to_path_buf()).unwrap();
        // Second call is no-op
        reg.set_storage_dir(tmp.path().join("other")).unwrap();
        assert!(tmp.path().exists());
    }

    #[test]
    fn restore_round_trip_via_disk() {
        let tmp = TempDir::new().unwrap();
        let reg = Arc::new(CouncilRegistry::new());
        reg.set_storage_dir(tmp.path().to_path_buf()).unwrap();

        // Write a council manually (no team needed for restore test)
        let id = "council-feedface".to_string();
        let cdir = tmp.path().join(&id);
        std::fs::create_dir_all(cdir.join("frames")).unwrap();
        let s = CouncilSession::new(
            id.clone(),
            "team-x".into(),
            "topic".into(),
            "charter".into(),
            None,
        );
        std::fs::write(cdir.join("session.json"), serde_json::to_vec(&s).unwrap()).unwrap();
        std::fs::write(cdir.join("envelopes.json"), b"[]").unwrap();

        let reg2 = Arc::new(CouncilRegistry::new());
        reg2.set_storage_dir(tmp.path().to_path_buf()).unwrap();
        assert_eq!(reg2.list_ids(), vec![id]);
    }

    #[test]
    fn reconcile_draining_marks_done_if_post_exists() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("council-x");
        std::fs::create_dir_all(dir.join("frames")).unwrap();
        let s = CouncilSession::new(
            "council-x".into(),
            "t".into(),
            "topic".into(),
            "charter".into(),
            None,
        );
        let state = CouncilState::new(s, dir).unwrap();

        let env = InboxEnvelope::new_queued(
            "env-aa".into(),
            "council-x".into(),
            "alice".into(),
            ReplyScope::Direct { seq: 1 },
            true,
            false,
            Some(1),
            0,
        );
        let mut env_d = env.clone();
        env_d.status = EnvelopeStatus::Draining;
        state.envelopes.write().push(env_d);

        // Post that references the envelope
        let post = CouncilPost::new_bro(
            "council-x".into(),
            2,
            "alice".into(),
            "hi".into(),
            ReplyScope::Direct { seq: 1 },
            "env-aa".into(),
        );
        state.posts.write().push(post);

        state.reconcile_draining_at_restart();
        assert_eq!(state.envelopes.read()[0].status, EnvelopeStatus::Done);
    }
}
