//! Whiteboard primitive — structured multi-agent deliberation as a
//! first-class engine concept.
//!
//! A board is a typed deliberation log: posts (proposals / claims /
//! concerns), annotations (challenge / corroborate / resolve / validate),
//! and votes, advanced through phases (blind → read → validate →
//! debate → resolve → archived) by a facilitator role.
//!
//! Three audiences share one surface:
//!
//! - **In-workflow ensemble specialists.** When an ensemble dispatches
//!   against a node with `board: "<id>"` set, each member's STRICT-JSON
//!   output is auto-posted to the board (engine-driven).
//! - **In-workflow facilitator (single bro).** Drives phase
//!   transitions from inside the workflow, same way any other actor
//!   produces output.
//! - **External agents — operator's Claude session, dispatched help,
//!   eventually humans through slack/ntfy adapters.** Read board state
//!   via `whiteboard_state`, post via `whiteboard_post`, vote via
//!   `whiteboard_vote`, transition via `whiteboard_transition`. Same
//!   tools the engine calls internally.
//!
//! Resume mechanism: `whiteboard_transition` emits a `board-transitioned`
//! signal correlated to `(board_id, target_phase)` through the engine's
//! shared `dispatch_routed_event` pipeline. A `wait` node with
//! `target_phase: "<phase>"` resolves on that signal — same machinery
//! webhook ingress uses.
//!
//! Storage: file-per-board JSON under `$store_dir/whiteboards/<id>.json`,
//! atomic write via tempfile + rename. Concurrency mediated by a
//! `parking_lot::RwLock` per board (single-daemon assumption — the
//! lockfile dance phaser does for Node IPC isn't needed here).

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsStr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Result, anyhow, bail};
use chrono::Utc;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

// ── Phase + roles ──────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Blind,
    Read,
    Validate,
    Debate,
    Resolve,
    Archived,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Blind => "blind",
            Self::Read => "read",
            Self::Validate => "validate",
            Self::Debate => "debate",
            Self::Resolve => "resolve",
            Self::Archived => "archived",
        }
    }

    /// Next phase in the canonical sequence. Validate is optional —
    /// `read → debate` is also legal (skip validate).
    pub fn canonical_next(self) -> Option<Phase> {
        match self {
            Self::Blind => Some(Self::Read),
            Self::Read => Some(Self::Validate),
            Self::Validate => Some(Self::Debate),
            Self::Debate => Some(Self::Resolve),
            Self::Resolve => Some(Self::Archived),
            Self::Archived => None,
        }
    }

    /// True if `target` is a legal direct transition from `self`.
    /// Allows the canonical step OR `read → debate` (skip validate).
    pub fn allows_transition_to(self, target: Phase) -> bool {
        if self.canonical_next() == Some(target) {
            return true;
        }
        matches!((self, target), (Self::Read, Self::Debate))
    }

    pub fn allows_post(self) -> bool {
        matches!(self, Self::Blind)
    }

    pub fn allows_annotate(self) -> bool {
        matches!(self, Self::Validate | Self::Debate)
    }

    pub fn allows_vote(self) -> bool {
        matches!(self, Self::Debate)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Specialist,
    Facilitator,
    Operator,
}

impl Role {
    pub fn can_transition(self) -> bool {
        matches!(self, Self::Facilitator | Self::Operator)
    }
}

// ── Posts / annotations / votes ────────────────────────────────────

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PostType {
    Proposal,
    Claim,
    Concern,
    Informational,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Critical,
    High,
    Medium,
    Low,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Post {
    pub id: String,
    pub agent: String,
    #[serde(rename = "type")]
    pub post_type: PostType,
    pub title: String,
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_location: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity: Option<Severity>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub finding_refs: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cascade_targets: Vec<String>,
    pub posted_at: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AnnotationType {
    Challenge,
    Corroborate,
    Resolve,
    Validation,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ValidationResult {
    Confirmed,
    Refuted,
    Inconclusive,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Annotation {
    pub id: String,
    pub post_id: String,
    pub agent: String,
    #[serde(rename = "type")]
    pub annotation_type: AnnotationType,
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<ValidationResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolves: Option<String>,
    pub posted_at: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VoteValue {
    Accept,
    Reject,
    Defer,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Vote {
    pub post_id: String,
    pub agent: String,
    pub vote: VoteValue,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub at: String,
}

// ── Agent registration ─────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    pub role: Role,
    /// Free-form domain hint ("security", "perf", "design", …).
    /// Workflow-level concept; the engine doesn't enforce semantics.
    pub domain: String,
    pub registered_at: String,
}

// ── Phase history ──────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhaseEvent {
    pub phase: Phase,
    pub by: String,
    pub at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

// ── Board ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Board {
    pub id: String,
    /// Free-form topic — typically "<workflow-name>: <issue title>" or
    /// similar. Used for inbox surfacing.
    pub topic: String,
    pub project: String,
    pub created_at: String,
    pub phase: Phase,
    pub phase_history: Vec<PhaseEvent>,
    pub agents: BTreeMap<String, Agent>,
    pub posts: Vec<Post>,
    pub annotations: Vec<Annotation>,
    pub votes: Vec<Vote>,
    /// Optional arc thread ID this board is bound to. When set, the
    /// engine knows which arc to resume on phase transitions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arc_thread_id: Option<String>,
}

impl Board {
    fn next_post_id(&self) -> String {
        format!("post-{:03}", self.posts.len() + 1)
    }

    fn next_annotation_id(&self) -> String {
        format!("ann-{:03}", self.annotations.len() + 1)
    }

    /// True if the board is ready to advance from its current phase
    /// (advisory — facilitator still owns the actual transition).
    /// Mirrors phaser's auto-transition heuristic.
    pub fn ready_for_transition(&self, phase_age_secs: i64) -> bool {
        match self.phase {
            Phase::Blind => {
                let specialists: Vec<&str> = self
                    .agents
                    .iter()
                    .filter(|(_, a)| a.role == Role::Specialist)
                    .map(|(n, _)| n.as_str())
                    .collect();
                if specialists.is_empty() {
                    return false;
                }
                let posted: std::collections::HashSet<&str> =
                    self.posts.iter().map(|p| p.agent.as_str()).collect();
                specialists.iter().all(|n| posted.contains(n))
            }
            Phase::Debate => {
                let challenges: Vec<&Annotation> = self
                    .annotations
                    .iter()
                    .filter(|a| a.annotation_type == AnnotationType::Challenge)
                    .collect();
                if challenges.is_empty() {
                    return phase_age_secs > 60;
                }
                let resolved: std::collections::HashSet<&str> = self
                    .annotations
                    .iter()
                    .filter(|a| a.annotation_type == AnnotationType::Resolve)
                    .filter_map(|a| a.resolves.as_deref())
                    .collect();
                challenges.iter().all(|c| resolved.contains(c.id.as_str()))
            }
            _ => false,
        }
    }

    /// Vote tally per post.
    pub fn vote_tally(&self) -> BTreeMap<String, VoteCounts> {
        let mut tallies: BTreeMap<String, VoteCounts> = BTreeMap::new();
        for v in &self.votes {
            let entry = tallies.entry(v.post_id.clone()).or_default();
            match v.vote {
                VoteValue::Accept => entry.accept += 1,
                VoteValue::Reject => entry.reject += 1,
                VoteValue::Defer => entry.defer += 1,
            }
        }
        tallies
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VoteCounts {
    pub accept: u32,
    pub reject: u32,
    pub defer: u32,
}

impl VoteCounts {
    #[allow(dead_code)]
    pub fn total(&self) -> u32 {
        self.accept + self.reject + self.defer
    }
}

// ── Conflict detection ─────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Conflict {
    DirectOverlap {
        posts: Vec<String>,
        target_file: String,
        target_location: String,
    },
    CascadeCollision {
        posts: Vec<String>,
        cascade_file: String,
    },
    SeverityDisagreement {
        posts: Vec<String>,
        finding_ref: String,
        severities: BTreeMap<String, String>,
    },
}

pub fn detect_conflicts(board: &Board) -> Vec<Conflict> {
    let mut out = Vec::new();
    let mut by_target: HashMap<&str, Vec<&Post>> = HashMap::new();
    let mut by_ref: HashMap<&str, Vec<&Post>> = HashMap::new();

    for p in &board.posts {
        if let Some(t) = &p.target_file {
            by_target.entry(t.as_str()).or_default().push(p);
        }
        for r in &p.finding_refs {
            by_ref.entry(r.as_str()).or_default().push(p);
        }
    }

    // Direct overlap: same target_file + identical target_location.
    for (file, posts) in &by_target {
        for i in 0..posts.len() {
            for j in (i + 1)..posts.len() {
                let a = posts[i];
                let b = posts[j];
                if let (Some(la), Some(lb)) = (&a.target_location, &b.target_location) {
                    if la == lb {
                        out.push(Conflict::DirectOverlap {
                            posts: vec![a.id.clone(), b.id.clone()],
                            target_file: (*file).to_string(),
                            target_location: la.clone(),
                        });
                    }
                }
            }
        }
    }

    // Cascade collision: post A cascades to post B's direct target.
    for p in &board.posts {
        for cascade in &p.cascade_targets {
            if let Some(targets) = by_target.get(cascade.as_str()) {
                for other in targets {
                    if other.id == p.id {
                        continue;
                    }
                    out.push(Conflict::CascadeCollision {
                        posts: vec![p.id.clone(), other.id.clone()],
                        cascade_file: cascade.clone(),
                    });
                }
            }
        }
    }

    // Severity disagreement: same finding_ref, distinct severities.
    for (r, posts) in &by_ref {
        let mut sev_by_post: BTreeMap<String, String> = BTreeMap::new();
        for p in posts {
            if let Some(s) = p.severity {
                sev_by_post.insert(p.id.clone(), severity_str(s).to_string());
            }
        }
        let distinct: std::collections::HashSet<&str> =
            sev_by_post.values().map(String::as_str).collect();
        if distinct.len() > 1 {
            out.push(Conflict::SeverityDisagreement {
                posts: posts.iter().map(|p| p.id.clone()).collect(),
                finding_ref: (*r).to_string(),
                severities: sev_by_post,
            });
        }
    }

    out
}

fn severity_str(s: Severity) -> &'static str {
    match s {
        Severity::Critical => "critical",
        Severity::High => "high",
        Severity::Medium => "medium",
        Severity::Low => "low",
    }
}

// ── Filtered view (per-agent, per-phase) ───────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct BoardView {
    pub id: String,
    pub topic: String,
    pub phase: Phase,
    pub phase_age_secs: i64,
    pub phase_history: Vec<PhaseEvent>,
    pub agents: BTreeMap<String, Agent>,
    pub posts: Vec<Post>,
    pub annotations: Vec<Annotation>,
    pub votes: Vec<Vote>,
    pub post_count: usize,
    pub annotation_count: usize,
    pub vote_count: usize,
    pub ready_for_transition: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase_age_warning: Option<String>,
}

pub fn filter_for_agent(board: &Board, agent_name: &str) -> Result<BoardView> {
    let agent = board.agents.get(agent_name).ok_or_else(|| {
        anyhow!(
            "agent '{agent_name}' is not registered on board '{}'",
            board.id
        )
    })?;

    let phase_started = board
        .phase_history
        .last()
        .map(|h| h.at.as_str())
        .unwrap_or(&board.created_at);
    let phase_age_secs = age_secs(phase_started);

    // Posts visibility: blind hides others, every later phase reveals all.
    let posts: Vec<Post> = if board.phase == Phase::Blind {
        board
            .posts
            .iter()
            .filter(|p| p.agent == agent_name)
            .cloned()
            .collect()
    } else {
        board.posts.clone()
    };

    // Annotation visibility: validate and resolve are open; debate
    // surfaces all to facilitator/operator, restricted to own + related
    // posts for specialists.
    let annotations: Vec<Annotation> = match board.phase {
        Phase::Blind | Phase::Read => Vec::new(),
        Phase::Validate | Phase::Resolve | Phase::Archived => board.annotations.clone(),
        Phase::Debate => {
            if matches!(agent.role, Role::Facilitator | Role::Operator) {
                board.annotations.clone()
            } else {
                let own_post_ids: std::collections::HashSet<&str> = board
                    .posts
                    .iter()
                    .filter(|p| p.agent == agent_name)
                    .map(|p| p.id.as_str())
                    .collect();
                board
                    .annotations
                    .iter()
                    .filter(|a| a.agent == agent_name || own_post_ids.contains(a.post_id.as_str()))
                    .cloned()
                    .collect()
            }
        }
    };

    let votes: Vec<Vote> = if board.phase == Phase::Resolve
        || matches!(agent.role, Role::Facilitator | Role::Operator)
    {
        board.votes.clone()
    } else if board.phase == Phase::Debate {
        board
            .votes
            .iter()
            .filter(|v| v.agent == agent_name)
            .cloned()
            .collect()
    } else {
        Vec::new()
    };

    let warning = if phase_age_secs > 5 * 60 {
        Some(format!(
            "Phase '{}' has been active for {} minutes.",
            board.phase.as_str(),
            phase_age_secs / 60
        ))
    } else {
        None
    };

    Ok(BoardView {
        id: board.id.clone(),
        topic: board.topic.clone(),
        phase: board.phase,
        phase_age_secs,
        phase_history: board.phase_history.clone(),
        agents: board.agents.clone(),
        posts,
        annotations,
        votes,
        post_count: board.posts.len(),
        annotation_count: board.annotations.len(),
        vote_count: board.votes.len(),
        ready_for_transition: board.ready_for_transition(phase_age_secs),
        phase_age_warning: warning,
    })
}

fn age_secs(iso: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(iso)
        .map(|t| (Utc::now() - t.with_timezone(&Utc)).num_seconds())
        .unwrap_or(0)
}

// ── Registry: in-memory + disk-backed ──────────────────────────────

#[derive(Default)]
pub struct WhiteboardRegistry {
    boards: RwLock<HashMap<String, Arc<RwLock<Board>>>>,
    storage_dir: RwLock<Option<PathBuf>>,
}

pub type SharedRegistry = Arc<WhiteboardRegistry>;

impl WhiteboardRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the base directory boards are persisted under. Idempotent.
    /// Loads any existing boards from disk on first call.
    pub fn set_storage_dir(&self, dir: PathBuf) -> Result<()> {
        let mut slot = self.storage_dir.write();
        if slot.is_some() {
            return Ok(());
        }
        std::fs::create_dir_all(&dir)
            .map_err(|e| anyhow!("create whiteboard storage dir {}: {e}", dir.display()))?;
        // Restore boards from disk.
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension() != Some(OsStr::new("json")) {
                    continue;
                }
                if let Ok(bytes) = std::fs::read(&path) {
                    match serde_json::from_slice::<Board>(&bytes) {
                        Ok(board) => {
                            let id = board.id.clone();
                            self.boards.write().insert(id, Arc::new(RwLock::new(board)));
                        }
                        Err(e) => {
                            tracing::warn!("whiteboards: bad spec at {}: {e}", path.display())
                        }
                    }
                }
            }
        }
        *slot = Some(dir);
        Ok(())
    }

    fn board_path(&self, id: &str) -> Option<PathBuf> {
        self.storage_dir
            .read()
            .as_ref()
            .map(|d| d.join(format!("{id}.json")))
    }

    fn persist(&self, id: &str, board: &Board) -> Result<()> {
        let Some(path) = self.board_path(id) else {
            return Ok(()); // Storage not configured (test mode).
        };
        let body = serde_json::to_string_pretty(board)
            .map_err(|e| anyhow!("serialize board {id}: {e}"))?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, body).map_err(|e| anyhow!("write {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, &path)
            .map_err(|e| anyhow!("rename {} → {}: {e}", tmp.display(), path.display()))?;
        Ok(())
    }

    pub fn list_ids(&self) -> Vec<String> {
        self.boards.read().keys().cloned().collect()
    }

    pub fn get(&self, id: &str) -> Option<Arc<RwLock<Board>>> {
        self.boards.read().get(id).cloned()
    }

    pub fn rename_project_refs(&self, old_project: &str, new_project: &str) -> Result<usize> {
        let boards = self.boards.read().values().cloned().collect::<Vec<_>>();
        let mut updated = 0usize;
        for board_lock in boards {
            let mut board = board_lock.write();
            if board.project == old_project {
                board.project = new_project.to_string();
                self.persist(&board.id, &board)?;
                updated += 1;
            }
        }
        Ok(updated)
    }

    /// Open a new board. Returns the board id (which the caller chose).
    /// Errors if the id is already in use.
    pub fn open(
        &self,
        id: &str,
        topic: &str,
        project: &str,
        arc_thread_id: Option<&str>,
        opener: &str,
    ) -> Result<()> {
        let mut boards = self.boards.write();
        if boards.contains_key(id) {
            bail!("whiteboard '{id}' already exists");
        }
        let now = Utc::now().to_rfc3339();
        let board = Board {
            id: id.to_string(),
            topic: topic.to_string(),
            project: project.to_string(),
            created_at: now.clone(),
            phase: Phase::Blind,
            phase_history: vec![PhaseEvent {
                phase: Phase::Blind,
                by: opener.to_string(),
                at: now,
                summary: None,
            }],
            agents: BTreeMap::new(),
            posts: Vec::new(),
            annotations: Vec::new(),
            votes: Vec::new(),
            arc_thread_id: arc_thread_id.map(str::to_string),
        };
        self.persist(id, &board)?;
        boards.insert(id.to_string(), Arc::new(RwLock::new(board)));
        Ok(())
    }

    pub fn register(&self, id: &str, agent_name: &str, role: Role, domain: &str) -> Result<()> {
        let board_arc = self
            .boards
            .read()
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow!("whiteboard '{id}' does not exist"))?;
        let mut board = board_arc.write();
        if board.phase == Phase::Resolve || board.phase == Phase::Archived {
            bail!(
                "cannot register on board '{id}': phase is '{}'",
                board.phase.as_str()
            );
        }
        if board.agents.contains_key(agent_name) {
            // Idempotent — existing registration is fine.
            return Ok(());
        }
        let now = Utc::now().to_rfc3339();
        board.agents.insert(
            agent_name.to_string(),
            Agent {
                role,
                domain: domain.to_string(),
                registered_at: now,
            },
        );
        self.persist(id, &board)?;
        Ok(())
    }

    pub fn post(
        &self,
        id: &str,
        agent_name: &str,
        post_type: PostType,
        title: &str,
        body: &str,
        target_file: Option<&str>,
        target_location: Option<&str>,
        severity: Option<Severity>,
        finding_refs: Vec<String>,
        cascade_targets: Vec<String>,
    ) -> Result<String> {
        let board_arc = self
            .boards
            .read()
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow!("whiteboard '{id}' does not exist"))?;
        let mut board = board_arc.write();
        if !board.phase.allows_post() {
            bail!(
                "cannot post on board '{id}': phase is '{}', posting only in 'blind'",
                board.phase.as_str()
            );
        }
        if !board.agents.contains_key(agent_name) {
            bail!("agent '{agent_name}' is not registered on board '{id}'");
        }
        let post = Post {
            id: board.next_post_id(),
            agent: agent_name.to_string(),
            post_type,
            title: title.to_string(),
            body: body.to_string(),
            target_file: target_file.map(str::to_string),
            target_location: target_location.map(str::to_string),
            severity,
            finding_refs,
            cascade_targets,
            posted_at: Utc::now().to_rfc3339(),
        };
        let post_id = post.id.clone();
        board.posts.push(post);
        self.persist(id, &board)?;
        Ok(post_id)
    }

    pub fn annotate(
        &self,
        id: &str,
        agent_name: &str,
        post_id: &str,
        annotation_type: AnnotationType,
        body: &str,
        result: Option<ValidationResult>,
        resolves: Option<&str>,
    ) -> Result<String> {
        let board_arc = self
            .boards
            .read()
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow!("whiteboard '{id}' does not exist"))?;
        let mut board = board_arc.write();
        if !board.phase.allows_annotate() {
            bail!(
                "cannot annotate: phase is '{}', annotations only in 'validate' / 'debate'",
                board.phase.as_str()
            );
        }
        // Phase-specific annotation type allow-list.
        match (board.phase, annotation_type) {
            (Phase::Validate, AnnotationType::Validation) => {}
            (Phase::Debate, AnnotationType::Challenge)
            | (Phase::Debate, AnnotationType::Corroborate)
            | (Phase::Debate, AnnotationType::Resolve) => {}
            (phase, kind) => bail!(
                "annotation type '{:?}' not allowed in phase '{}'",
                kind,
                phase.as_str()
            ),
        }
        if !board.agents.contains_key(agent_name) {
            bail!("agent '{agent_name}' is not registered on board '{id}'");
        }
        let target_post = board.posts.iter().find(|p| p.id == post_id);
        let post = match target_post {
            Some(p) => p,
            None => bail!("post '{post_id}' does not exist on board '{id}'"),
        };
        if post.agent == agent_name {
            bail!("agent '{agent_name}' cannot annotate own post '{post_id}'");
        }
        if annotation_type == AnnotationType::Resolve {
            let r = resolves.ok_or_else(|| {
                anyhow!("'resolve' annotations must reference a challenge via 'resolves'")
            })?;
            let challenge = board
                .annotations
                .iter()
                .find(|a| a.id == r)
                .ok_or_else(|| anyhow!("referenced annotation '{r}' does not exist"))?;
            if challenge.annotation_type != AnnotationType::Challenge {
                bail!("referenced annotation '{r}' is not a challenge");
            }
        }
        if annotation_type == AnnotationType::Validation && result.is_none() {
            bail!("'validation' annotations require a 'result' (confirmed/refuted/inconclusive)");
        }
        let ann = Annotation {
            id: board.next_annotation_id(),
            post_id: post_id.to_string(),
            agent: agent_name.to_string(),
            annotation_type,
            body: body.to_string(),
            result,
            resolves: resolves.map(str::to_string),
            posted_at: Utc::now().to_rfc3339(),
        };
        let ann_id = ann.id.clone();
        board.annotations.push(ann);
        self.persist(id, &board)?;
        Ok(ann_id)
    }

    pub fn vote(
        &self,
        id: &str,
        agent_name: &str,
        post_id: &str,
        vote: VoteValue,
        reason: Option<&str>,
    ) -> Result<bool> {
        let board_arc = self
            .boards
            .read()
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow!("whiteboard '{id}' does not exist"))?;
        let mut board = board_arc.write();
        if !board.phase.allows_vote() {
            bail!(
                "cannot vote: phase is '{}', voting only in 'debate'",
                board.phase.as_str()
            );
        }
        if !board.agents.contains_key(agent_name) {
            bail!("agent '{agent_name}' is not registered on board '{id}'");
        }
        if !board.posts.iter().any(|p| p.id == post_id) {
            bail!("post '{post_id}' does not exist on board '{id}'");
        }
        let now = Utc::now().to_rfc3339();
        let mut replaced = false;
        if let Some(existing) = board
            .votes
            .iter_mut()
            .find(|v| v.post_id == post_id && v.agent == agent_name)
        {
            existing.vote = vote;
            existing.reason = reason.map(str::to_string);
            existing.at = now;
            replaced = true;
        } else {
            board.votes.push(Vote {
                post_id: post_id.to_string(),
                agent: agent_name.to_string(),
                vote,
                reason: reason.map(str::to_string),
                at: now,
            });
        }
        self.persist(id, &board)?;
        Ok(replaced)
    }

    /// Transition the board to a new phase. Returns the (from, to)
    /// pair so the caller can fire the routed signal.
    pub fn transition(
        &self,
        id: &str,
        agent_name: &str,
        target: Phase,
        summary: Option<&str>,
    ) -> Result<(Phase, Phase)> {
        let board_arc = self
            .boards
            .read()
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow!("whiteboard '{id}' does not exist"))?;
        let mut board = board_arc.write();
        let agent = board
            .agents
            .get(agent_name)
            .ok_or_else(|| anyhow!("agent '{agent_name}' is not registered on board '{id}'"))?;
        if !agent.role.can_transition() {
            bail!(
                "agent '{agent_name}' has role {:?} — only facilitator or operator can transition",
                agent.role
            );
        }
        let from = board.phase;
        if !from.allows_transition_to(target) {
            bail!(
                "illegal phase transition: '{}' → '{}'",
                from.as_str(),
                target.as_str()
            );
        }
        board.phase = target;
        board.phase_history.push(PhaseEvent {
            phase: target,
            by: agent_name.to_string(),
            at: Utc::now().to_rfc3339(),
            summary: summary.map(str::to_string),
        });
        self.persist(id, &board)?;
        Ok((from, target))
    }

    pub fn archive(&self, id: &str, agent_name: &str) -> Result<ArchiveSummary> {
        let board_arc = self
            .boards
            .read()
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow!("whiteboard '{id}' does not exist"))?;
        let board = board_arc.read();
        if !board.agents.contains_key(agent_name) {
            bail!("agent '{agent_name}' is not registered on board '{id}'");
        }
        if board.phase != Phase::Resolve {
            bail!(
                "cannot archive: phase is '{}', archive only allowed in 'resolve'",
                board.phase.as_str()
            );
        }
        let mut posts_by_type: BTreeMap<String, u32> = BTreeMap::new();
        for p in &board.posts {
            let key = post_type_str(p.post_type).to_string();
            *posts_by_type.entry(key).or_insert(0) += 1;
        }
        let mut tally = VoteCounts::default();
        for v in &board.votes {
            match v.vote {
                VoteValue::Accept => tally.accept += 1,
                VoteValue::Reject => tally.reject += 1,
                VoteValue::Defer => tally.defer += 1,
            }
        }
        let challenges = board
            .annotations
            .iter()
            .filter(|a| a.annotation_type == AnnotationType::Challenge)
            .count();
        let resolved: std::collections::HashSet<&str> = board
            .annotations
            .iter()
            .filter(|a| a.annotation_type == AnnotationType::Resolve)
            .filter_map(|a| a.resolves.as_deref())
            .collect();
        let unresolved_challenges = board
            .annotations
            .iter()
            .filter(|a| a.annotation_type == AnnotationType::Challenge)
            .filter(|c| !resolved.contains(c.id.as_str()))
            .count();
        let summary = ArchiveSummary {
            board_id: id.to_string(),
            total_posts: board.posts.len(),
            posts_by_type,
            total_annotations: board.annotations.len(),
            total_votes: board.votes.len(),
            vote_tally: tally,
            total_challenges: challenges,
            unresolved_challenges,
            agent_count: board.agents.len(),
            phase_count: board.phase_history.len(),
        };
        drop(board);
        // Move archive to disk under archive/, drop in-memory entry.
        let board_arc = self.boards.write().remove(id);
        if let Some(arc) = board_arc {
            let final_board = {
                let mut b = arc.write();
                b.phase = Phase::Archived;
                b.phase_history.push(PhaseEvent {
                    phase: Phase::Archived,
                    by: agent_name.to_string(),
                    at: Utc::now().to_rfc3339(),
                    summary: None,
                });
                b.clone()
            };
            if let Some(active) = self.board_path(id) {
                if let Some(parent) = active.parent() {
                    let archive_dir = parent.join("archive");
                    let _ = std::fs::create_dir_all(&archive_dir);
                    let archive_path = archive_dir.join(format!("{id}.json"));
                    if let Ok(body) = serde_json::to_string_pretty(&final_board) {
                        let _ = std::fs::write(&archive_path, body);
                    }
                }
                let _ = std::fs::remove_file(&active);
            }
        }
        Ok(summary)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ArchiveSummary {
    pub board_id: String,
    pub total_posts: usize,
    pub posts_by_type: BTreeMap<String, u32>,
    pub total_annotations: usize,
    pub total_votes: usize,
    pub vote_tally: VoteCounts,
    pub total_challenges: usize,
    pub unresolved_challenges: usize,
    pub agent_count: usize,
    pub phase_count: usize,
}

fn post_type_str(t: PostType) -> &'static str {
    match t {
        PostType::Proposal => "proposal",
        PostType::Claim => "claim",
        PostType::Concern => "concern",
        PostType::Informational => "informational",
    }
}

// ── Template-scope helpers (board.* in ArcContext) ─────────────────

/// Flatten a board into a `Value` map for ArcContext template
/// rendering and gate-packet entity evaluation. Exposes phase,
/// counts, vote tally, and an array of posts/annotations.
#[allow(dead_code)]
pub fn board_template_scope(board: &Board) -> Value {
    let mut out = Map::new();
    out.insert("id".into(), Value::String(board.id.clone()));
    out.insert("topic".into(), Value::String(board.topic.clone()));
    out.insert("phase".into(), Value::String(board.phase.as_str().into()));
    out.insert(
        "post_count".into(),
        Value::Number((board.posts.len() as u64).into()),
    );
    out.insert(
        "annotation_count".into(),
        Value::Number((board.annotations.len() as u64).into()),
    );
    out.insert(
        "vote_count".into(),
        Value::Number((board.votes.len() as u64).into()),
    );
    let tallies = board.vote_tally();
    let mut tally_map = Map::new();
    for (post_id, c) in &tallies {
        tally_map.insert(
            post_id.clone(),
            serde_json::json!({
                "accept": c.accept,
                "reject": c.reject,
                "defer": c.defer,
                "total": c.total(),
            }),
        );
    }
    out.insert("vote_tally".into(), Value::Object(tally_map));
    let posts_json = serde_json::to_value(&board.posts).unwrap_or(Value::Null);
    out.insert("posts".into(), posts_json);
    let ann_json = serde_json::to_value(&board.annotations).unwrap_or(Value::Null);
    out.insert("annotations".into(), ann_json);
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_registry() -> WhiteboardRegistry {
        WhiteboardRegistry::new()
    }

    #[test]
    fn open_register_post_is_idempotent_register() {
        let r = fresh_registry();
        r.open("b1", "topic", "/proj", None, "alice").unwrap();
        r.register("b1", "alice", Role::Facilitator, "ops").unwrap();
        // Idempotent re-registration is OK.
        r.register("b1", "alice", Role::Facilitator, "ops").unwrap();
        let b = r.get("b1").unwrap();
        assert_eq!(b.read().agents.len(), 1);
    }

    #[test]
    fn post_only_in_blind() {
        let r = fresh_registry();
        r.open("b1", "topic", "/proj", None, "alice").unwrap();
        r.register("b1", "alice", Role::Facilitator, "ops").unwrap();
        r.register("b1", "bob", Role::Specialist, "security")
            .unwrap();
        r.post(
            "b1",
            "bob",
            PostType::Claim,
            "title",
            "body",
            None,
            None,
            None,
            vec![],
            vec![],
        )
        .unwrap();
        // Transition to read; posting should now fail.
        r.transition("b1", "alice", Phase::Read, None).unwrap();
        let err = r
            .post(
                "b1",
                "bob",
                PostType::Claim,
                "x",
                "y",
                None,
                None,
                None,
                vec![],
                vec![],
            )
            .unwrap_err();
        assert!(err.to_string().contains("phase is 'read'"));
    }

    #[test]
    fn transition_validates_role_and_sequence() {
        let r = fresh_registry();
        r.open("b1", "topic", "/proj", None, "alice").unwrap();
        r.register("b1", "alice", Role::Facilitator, "ops").unwrap();
        r.register("b1", "bob", Role::Specialist, "security")
            .unwrap();
        // Specialist cannot transition.
        let err = r.transition("b1", "bob", Phase::Read, None).unwrap_err();
        assert!(err.to_string().contains("only facilitator"));
        // Facilitator can advance read → debate (skip validate).
        r.transition("b1", "alice", Phase::Read, None).unwrap();
        r.transition("b1", "alice", Phase::Debate, None).unwrap();
        // But not blind → debate (illegal skip).
        r.open("b2", "t", "/p", None, "a").unwrap();
        r.register("b2", "a", Role::Facilitator, "ops").unwrap();
        let err = r.transition("b2", "a", Phase::Debate, None).unwrap_err();
        assert!(err.to_string().contains("illegal phase transition"));
    }

    #[test]
    fn ready_for_transition_blind_when_all_specialists_posted() {
        let r = fresh_registry();
        r.open("b1", "topic", "/proj", None, "alice").unwrap();
        r.register("b1", "alice", Role::Facilitator, "ops").unwrap();
        r.register("b1", "s1", Role::Specialist, "security")
            .unwrap();
        r.register("b1", "s2", Role::Specialist, "perf").unwrap();
        let arc = r.get("b1").unwrap();
        assert!(!arc.read().ready_for_transition(0));
        r.post(
            "b1",
            "s1",
            PostType::Claim,
            "t1",
            "b1",
            None,
            None,
            None,
            vec![],
            vec![],
        )
        .unwrap();
        assert!(!arc.read().ready_for_transition(0));
        r.post(
            "b1",
            "s2",
            PostType::Claim,
            "t2",
            "b2",
            None,
            None,
            None,
            vec![],
            vec![],
        )
        .unwrap();
        assert!(arc.read().ready_for_transition(0));
    }

    #[test]
    fn vote_replace_semantics() {
        let r = fresh_registry();
        r.open("b1", "topic", "/proj", None, "f").unwrap();
        r.register("b1", "f", Role::Facilitator, "ops").unwrap();
        r.register("b1", "s", Role::Specialist, "security").unwrap();
        r.post(
            "b1",
            "s",
            PostType::Proposal,
            "t",
            "b",
            None,
            None,
            None,
            vec![],
            vec![],
        )
        .unwrap();
        r.transition("b1", "f", Phase::Read, None).unwrap();
        r.transition("b1", "f", Phase::Debate, None).unwrap();
        let replaced = r
            .vote("b1", "f", "post-001", VoteValue::Accept, None)
            .unwrap();
        assert!(!replaced);
        let replaced = r
            .vote("b1", "f", "post-001", VoteValue::Reject, Some("changed"))
            .unwrap();
        assert!(replaced);
        let arc = r.get("b1").unwrap();
        let board = arc.read();
        assert_eq!(board.votes.len(), 1);
        assert_eq!(board.votes[0].vote, VoteValue::Reject);
    }

    #[test]
    fn detect_severity_disagreement() {
        let r = fresh_registry();
        r.open("b1", "topic", "/proj", None, "f").unwrap();
        r.register("b1", "f", Role::Facilitator, "ops").unwrap();
        r.register("b1", "s1", Role::Specialist, "x").unwrap();
        r.register("b1", "s2", Role::Specialist, "y").unwrap();
        r.post(
            "b1",
            "s1",
            PostType::Concern,
            "t1",
            "b1",
            None,
            None,
            Some(Severity::Critical),
            vec!["RUSTSEC-2026-0001".into()],
            vec![],
        )
        .unwrap();
        r.post(
            "b1",
            "s2",
            PostType::Concern,
            "t2",
            "b2",
            None,
            None,
            Some(Severity::Low),
            vec!["RUSTSEC-2026-0001".into()],
            vec![],
        )
        .unwrap();
        let arc = r.get("b1").unwrap();
        let conflicts = detect_conflicts(&arc.read());
        assert!(
            conflicts
                .iter()
                .any(|c| matches!(c, Conflict::SeverityDisagreement { .. }))
        );
    }

    #[test]
    fn cannot_annotate_own_post() {
        let r = fresh_registry();
        r.open("b1", "topic", "/proj", None, "f").unwrap();
        r.register("b1", "f", Role::Facilitator, "ops").unwrap();
        r.register("b1", "s", Role::Specialist, "x").unwrap();
        r.post(
            "b1",
            "s",
            PostType::Claim,
            "t",
            "b",
            None,
            None,
            None,
            vec![],
            vec![],
        )
        .unwrap();
        r.transition("b1", "f", Phase::Read, None).unwrap();
        r.transition("b1", "f", Phase::Debate, None).unwrap();
        let err = r
            .annotate(
                "b1",
                "s",
                "post-001",
                AnnotationType::Challenge,
                "self-criticize?",
                None,
                None,
            )
            .unwrap_err();
        assert!(err.to_string().contains("cannot annotate own post"));
    }

    #[test]
    fn template_scope_exposes_phase_and_counts() {
        let r = fresh_registry();
        r.open("b1", "topic", "/proj", None, "f").unwrap();
        r.register("b1", "f", Role::Facilitator, "ops").unwrap();
        r.register("b1", "s", Role::Specialist, "x").unwrap();
        r.post(
            "b1",
            "s",
            PostType::Claim,
            "t",
            "b",
            None,
            None,
            None,
            vec![],
            vec![],
        )
        .unwrap();
        let arc = r.get("b1").unwrap();
        let scope = board_template_scope(&arc.read());
        assert_eq!(scope.get("phase").unwrap().as_str(), Some("blind"));
        assert_eq!(scope.get("post_count").unwrap().as_u64(), Some(1));
    }
}
