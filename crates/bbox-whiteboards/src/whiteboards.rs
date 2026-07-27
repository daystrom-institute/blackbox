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
use std::path::{Path, PathBuf};
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
    /// Allows the canonical step, OR `read → debate` (skip validate when
    /// no validator round is run), OR `validate → resolve` (skip debate on
    /// the conflict-free path — validation alone is dispositive).
    pub fn allows_transition_to(self, target: Phase) -> bool {
        if self.canonical_next() == Some(target) {
            return true;
        }
        matches!(
            (self, target),
            (Self::Read, Self::Debate) | (Self::Validate, Self::Resolve)
        )
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
    /// Resolving authority's project id, stamped on write. Absent on rows
    /// written before the catalog cut: those stay on the path lane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
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

    /// Validation standing of a single post, derived from the validator's
    /// `Validation` annotations against it. Precedence (bridgecrew exclusion
    /// teeth): any `refuted` → `Excluded`; else any `inconclusive` with no
    /// `confirmed` → `Inconclusive`; else ≥1 `confirmed` → `Confirmed`; else
    /// (no validation at all) → `Unvalidated`.
    pub fn post_standing(&self, post_id: &str) -> PostStanding {
        let mut confirmed = false;
        let mut inconclusive = false;
        for a in &self.annotations {
            if a.post_id != post_id || a.annotation_type != AnnotationType::Validation {
                continue;
            }
            match a.result {
                Some(ValidationResult::Refuted) => return PostStanding::Excluded,
                Some(ValidationResult::Confirmed) => confirmed = true,
                Some(ValidationResult::Inconclusive) => inconclusive = true,
                None => {}
            }
        }
        if confirmed {
            PostStanding::Confirmed
        } else if inconclusive {
            PostStanding::Inconclusive
        } else {
            PostStanding::Unvalidated
        }
    }

    /// Count of posts that received NO cross-agent challenge or corroborate.
    /// Self-annotation doesn't count (an author cannot review their own post).
    /// A non-zero value means the panel left posts unscrutinised — the gate
    /// uses this to reject the "zero challenges trivially ready" degenerate.
    pub fn unreviewed_post_count(&self) -> u32 {
        self.posts
            .iter()
            .filter(|p| {
                !self.annotations.iter().any(|a| {
                    a.post_id == p.id
                        && a.agent != p.agent
                        && matches!(
                            a.annotation_type,
                            AnnotationType::Challenge | AnnotationType::Corroborate
                        )
                })
            })
            .count() as u32
    }

    /// Validator-driven partition of posts into surviving vs excluded, plus
    /// per-standing counts and review coverage. Refuted posts are excluded
    /// from the correction plan; everything else survives (inconclusive is
    /// flagged/severity-capped downstream, unvalidated proceeds with a warning).
    pub fn validation_summary(&self) -> ValidationSummary {
        let mut out = ValidationSummary::default();
        for p in &self.posts {
            match self.post_standing(&p.id) {
                PostStanding::Excluded => {
                    out.excluded_post_ids.push(p.id.clone());
                    out.refuted_count += 1;
                }
                PostStanding::Confirmed => {
                    out.surviving_post_ids.push(p.id.clone());
                    out.confirmed_count += 1;
                }
                PostStanding::Inconclusive => {
                    out.surviving_post_ids.push(p.id.clone());
                    out.inconclusive_count += 1;
                }
                PostStanding::Unvalidated => {
                    out.surviving_post_ids.push(p.id.clone());
                    out.unvalidated_count += 1;
                }
            }
        }
        out.unreviewed_post_count = self.unreviewed_post_count();
        out
    }
}

/// Validation standing of a single post (see `Board::post_standing`).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PostStanding {
    Confirmed,
    Inconclusive,
    Unvalidated,
    /// Validator refuted the claim — excluded from the correction plan.
    Excluded,
}

/// Board-level validation partition + review coverage, surfaced by
/// `whiteboard_summarize` for the gate packet and the plan-writer.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ValidationSummary {
    pub surviving_post_ids: Vec<String>,
    pub excluded_post_ids: Vec<String>,
    pub confirmed_count: u32,
    pub refuted_count: u32,
    pub inconclusive_count: u32,
    pub unvalidated_count: u32,
    pub unreviewed_post_count: u32,
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

/// Capture persisted boards that retain a legacy literal project selector.
/// This does not initialize a [`WhiteboardRegistry`] or create its directory.
pub fn capture_project_catalog_owner_snapshot(
    storage_dir: &std::path::Path,
    limits: bbox_corpus_core::project_catalog_snapshot::OwnerSnapshotLimitsV1,
) -> std::result::Result<
    bbox_corpus_core::project_catalog_snapshot::OwnerSnapshotV1,
    bbox_corpus_core::project_catalog_snapshot::OwnerSnapshotError,
> {
    use bbox_corpus_core::project_catalog_snapshot::{
        LegacyProjectSelectorKindV1, OwnerSnapshotRowV1, OwnerSnapshotStateV1,
        build_owner_snapshot, capture_stable_regular_tree_nofollow, corrupt_owner_snapshot,
        finalize_owner_snapshot, missing_owner_snapshot, owner_subsource, sha256_hex,
        stable_subsource_id,
    };

    match std::fs::symlink_metadata(storage_dir) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return missing_owner_snapshot("whiteboard", "whiteboard:root", limits);
        }
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        _ => {
            return corrupt_owner_snapshot(
                "whiteboard",
                "whiteboard:root",
                "owner_tree_unsafe",
                limits,
            );
        }
    }
    let captures =
        match capture_stable_regular_tree_nofollow(storage_dir, "whiteboard", limits, |relative| {
            relative
                .extension()
                .and_then(|extension| extension.to_str())
                == Some("json")
        }) {
            Ok(captures) => captures,
            Err(error) => {
                return corrupt_owner_snapshot("whiteboard", "whiteboard:root", error.code, limits);
            }
        };
    if captures.is_empty() {
        let state = OwnerSnapshotStateV1::Present {
            content_sha256: sha256_hex(b""),
            byte_len: 0,
        };
        return build_owner_snapshot(
            "whiteboard",
            vec![owner_subsource("whiteboard:root", state, &[])],
            Vec::new(),
            limits,
        );
    }
    let mut rows = Vec::new();
    let mut subsources = Vec::new();
    for (relative, captured) in captures {
        let subsource_id = stable_subsource_id("whiteboard", &relative);
        let Some(bytes) = captured.bytes else {
            return corrupt_owner_snapshot(
                "whiteboard",
                &subsource_id,
                "owner_source_unreadable",
                limits,
            );
        };
        let board: Board = match serde_json::from_slice(&bytes) {
            Ok(board) => board,
            Err(_) => {
                return corrupt_owner_snapshot(
                    "whiteboard",
                    &subsource_id,
                    "owner_source_invalid",
                    limits,
                );
            }
        };
        let mut subsource_rows = Vec::new();
        if let Some(project_id) = board
            .project_id
            .as_deref()
            .map(str::trim)
            .filter(|project_id| !project_id.is_empty())
        {
            subsource_rows.push(OwnerSnapshotRowV1::inventory_target(
                format!("{}:target", board.id),
                project_id,
                sha256_hex(&bytes),
            ));
        }
        let selector = board.project.trim().to_string();
        if !selector.is_empty() {
            subsource_rows.push(OwnerSnapshotRowV1::legacy_selector(
                board.id,
                LegacyProjectSelectorKindV1::Project,
                selector,
            ));
        }
        subsources.push(owner_subsource(
            subsource_id,
            captured.state,
            &subsource_rows,
        ));
        rows.extend(subsource_rows);
    }
    finalize_owner_snapshot("whiteboard", "whiteboard:root", subsources, rows, limits)
}

/// Remove persisted boards owned by one project. Missing stores are empty;
/// malformed or unsafe entries refuse instead of being treated as absent.
pub fn discharge_project_catalog_rows(
    storage_dir: &Path,
    project_id: &str,
    selectors: &[String],
) -> Result<usize> {
    let metadata = match std::fs::symlink_metadata(storage_dir) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("whiteboard store root is not a safe directory");
    }
    let mut removals = Vec::new();
    for entry in std::fs::read_dir(storage_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() || entry.path().extension() != Some(OsStr::new("json")) {
            bail!("whiteboard store contains a non-canonical entry");
        }
        let board: Board = serde_json::from_slice(&std::fs::read(entry.path())?)?;
        if board.project_id.as_deref() == Some(project_id)
            || selectors.iter().any(|selector| selector == &board.project)
        {
            removals.push(entry.path());
        }
    }
    for path in &removals {
        std::fs::remove_file(path)?;
    }
    if !removals.is_empty() {
        std::fs::File::open(storage_dir)?.sync_all()?;
    }
    Ok(removals.len())
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
        project_id: Option<&str>,
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
            project_id: project_id.map(str::to_string),
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
        if post.agent == agent_name && annotation_type != AnnotationType::Resolve {
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
            if challenge.agent == agent_name {
                bail!("agent '{agent_name}' cannot resolve own challenge '{r}'");
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

    /// Archive the board. Normally legal only from the `resolve` phase.
    /// `force=true` archives from ANY phase — the abandon path for boards
    /// stranded mid-phase by a failed arc (gap-0301dc75) — and requires a
    /// facilitator/operator role since it is a phase transition in effect.
    pub fn archive(&self, id: &str, agent_name: &str, force: bool) -> Result<ArchiveSummary> {
        let board_arc = self
            .boards
            .read()
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow!("whiteboard '{id}' does not exist"))?;
        let board = board_arc.read();
        let agent = board
            .agents
            .get(agent_name)
            .ok_or_else(|| anyhow!("agent '{agent_name}' is not registered on board '{id}'"))?;
        let from_phase = board.phase;
        if force {
            if !agent.role.can_transition() {
                bail!(
                    "agent '{agent_name}' has role {:?} — only facilitator or operator can force-archive",
                    agent.role
                );
            }
        } else if from_phase != Phase::Resolve {
            bail!(
                "cannot archive: phase is '{}', archive only allowed in 'resolve' (pass force=true to abandon a stranded board)",
                from_phase.as_str()
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
                    summary: (force && from_phase != Phase::Resolve)
                        .then(|| format!("force-archived from phase '{}'", from_phase.as_str())),
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

// ── Structured board actions (engine auto-apply) ───────────────────
//
// The typed contract for engine-driven board mutation: a dispatched
// agent returns STRICT JSON describing what it wants to do on the
// board, and the caller (the workflow engine's `board` node binding)
// parses + applies it through the same registry methods the
// `whiteboard_*` MCP tools use — every phase/role/reference check
// holds identically. This closes the silent-failure mode where an LLM
// writes a beautiful deliberation turn but forgets the tool call
// (gap-7fbefe13).

/// One board mutation. Tagged by `action`; field names and enum
/// spellings match the `whiteboard_*` tool surface exactly, so a
/// prompt can describe either interface with the same vocabulary.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum BoardAction {
    Post {
        #[serde(rename = "type")]
        post_type: PostType,
        title: String,
        body: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target_file: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target_location: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        severity: Option<Severity>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        finding_refs: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        cascade_targets: Vec<String>,
    },
    Annotate {
        post_id: String,
        #[serde(rename = "type")]
        annotation_type: AnnotationType,
        body: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<ValidationResult>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resolves: Option<String>,
    },
    Vote {
        post_id: String,
        vote: VoteValue,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// Explicit abstention — lets an agent return STRICT JSON even
    /// when it has nothing to put on the board this turn.
    #[serde(alias = "noop")]
    None,
}

impl BoardAction {
    /// Short human label for event logs.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Post { .. } => "post",
            Self::Annotate { .. } => "annotate",
            Self::Vote { .. } => "vote",
            Self::None => "none",
        }
    }
}

/// One parsed item: an action plus an optional agent override. When
/// `agent_name` is absent the caller supplies its own attribution
/// (the workflow engine uses the ensemble member name).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BoardItem {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
    #[serde(flatten)]
    pub action: BoardAction,
}

/// Result of applying one [`BoardAction`], for event logging.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "applied", rename_all = "snake_case")]
pub enum AppliedAction {
    Post { post_id: String },
    Annotate { annotation_id: String },
    Vote { replaced: bool },
    None,
}

/// Parse an agent's raw final output into board items.
///
/// The contract is STRICT JSON — a single object or an array of
/// objects, optionally wrapped in a markdown code fence. Live runs
/// show real agents drift anyway (prose preambles, provider tool-call
/// echoes around the answer), so when the whole payload fails to
/// parse, a salvage pass tries every fenced code block and then the
/// outermost bracket-delimited spans. Salvage candidates must match
/// the action schema (the tagged `action` field), so unrelated JSON
/// embedded in the output — tool inputs, state dumps — cannot
/// false-positive into board mutations. Output with no schema-valid
/// JSON anywhere is an error: a loud skip beats silently applying
/// half an answer.
pub fn parse_board_actions(raw: &str) -> Result<Vec<BoardItem>> {
    let trimmed = raw.trim();
    let primary_err = match parse_board_actions_strict(strip_code_fence(trimmed)) {
        Ok(items) => return Ok(items),
        Err(e) => e,
    };
    // Salvage 1: every fenced block in the output, first schema-valid
    // one wins.
    for block in fenced_blocks(trimmed) {
        if let Ok(items) = parse_board_actions_strict(block.trim()) {
            return Ok(items);
        }
    }
    // Salvage 2: outermost bracket-delimited spans (array preferred —
    // the documented contract shape — then object).
    for (open, close) in [('[', ']'), ('{', '}')] {
        if let (Some(start), Some(end)) = (trimmed.find(open), trimmed.rfind(close)) {
            if start < end {
                if let Ok(items) = parse_board_actions_strict(&trimmed[start..=end]) {
                    return Ok(items);
                }
            }
        }
    }
    Err(primary_err)
}

fn parse_board_actions_strict(inner: &str) -> Result<Vec<BoardItem>> {
    let value: Value = serde_json::from_str(inner)
        .map_err(|e| anyhow!("board action output is not valid JSON: {e}"))?;
    let items: Vec<BoardItem> = match value {
        Value::Array(_) => serde_json::from_value(value)
            .map_err(|e| anyhow!("board action array did not match the action schema: {e}"))?,
        Value::Object(_) => vec![
            serde_json::from_value(value)
                .map_err(|e| anyhow!("board action did not match the action schema: {e}"))?,
        ],
        other => bail!(
            "board action output must be a JSON object or array, got {}",
            match other {
                Value::String(_) => "a string",
                Value::Number(_) => "a number",
                Value::Bool(_) => "a bool",
                Value::Null => "null",
                _ => "unsupported JSON",
            }
        ),
    };
    Ok(items)
}

/// Iterate the contents of every ``` fenced block in `s`, tolerating a
/// language tag on each opening fence.
fn fenced_blocks(s: &str) -> Vec<&str> {
    let mut blocks = Vec::new();
    let mut rest = s;
    while let Some(open) = rest.find("```") {
        let after_open = &rest[open + 3..];
        let Some(tag_end) = after_open.find('\n') else {
            break;
        };
        let body = &after_open[tag_end + 1..];
        let Some(close) = body.find("```") else {
            break;
        };
        blocks.push(&body[..close]);
        rest = &body[close + 3..];
    }
    blocks
}

/// Strip one enclosing markdown code fence if present. Tolerates a
/// language tag on the opening fence. Returns the input unchanged
/// when it is not fenced.
fn strip_code_fence(s: &str) -> &str {
    let Some(rest) = s.strip_prefix("```") else {
        return s;
    };
    // Drop the language tag line (may be empty).
    let body = match rest.split_once('\n') {
        Some((_tag, body)) => body,
        None => return s,
    };
    match body.rfind("```") {
        Some(idx) => body[..idx].trim(),
        None => s,
    }
}

impl WhiteboardRegistry {
    /// Apply one parsed action as `agent_name`. Routes through the
    /// same `post` / `annotate` / `vote` methods the MCP tools call,
    /// so phase legality, registration, and reference checks are
    /// identical to the tool surface.
    pub fn apply_action(
        &self,
        board_id: &str,
        agent_name: &str,
        action: &BoardAction,
    ) -> Result<AppliedAction> {
        match action {
            BoardAction::Post {
                post_type,
                title,
                body,
                target_file,
                target_location,
                severity,
                finding_refs,
                cascade_targets,
            } => {
                let post_id = self.post(
                    board_id,
                    agent_name,
                    *post_type,
                    title,
                    body,
                    target_file.as_deref(),
                    target_location.as_deref(),
                    *severity,
                    finding_refs.clone(),
                    cascade_targets.clone(),
                )?;
                Ok(AppliedAction::Post { post_id })
            }
            BoardAction::Annotate {
                post_id,
                annotation_type,
                body,
                result,
                resolves,
            } => {
                let annotation_id = self.annotate(
                    board_id,
                    agent_name,
                    post_id,
                    *annotation_type,
                    body,
                    *result,
                    resolves.as_deref(),
                )?;
                Ok(AppliedAction::Annotate { annotation_id })
            }
            BoardAction::Vote {
                post_id,
                vote,
                reason,
            } => {
                let replaced =
                    self.vote(board_id, agent_name, post_id, *vote, reason.as_deref())?;
                Ok(AppliedAction::Vote { replaced })
            }
            BoardAction::None => Ok(AppliedAction::None),
        }
    }
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
        r.open("b1", "topic", "/proj", None, None, "alice").unwrap();
        r.register("b1", "alice", Role::Facilitator, "ops").unwrap();
        // Idempotent re-registration is OK.
        r.register("b1", "alice", Role::Facilitator, "ops").unwrap();
        let b = r.get("b1").unwrap();
        assert_eq!(b.read().agents.len(), 1);
    }

    #[test]
    fn post_only_in_blind() {
        let r = fresh_registry();
        r.open("b1", "topic", "/proj", None, None, "alice").unwrap();
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
        r.open("b1", "topic", "/proj", None, None, "alice").unwrap();
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
        r.open("b2", "t", "/p", None, None, "a").unwrap();
        r.register("b2", "a", Role::Facilitator, "ops").unwrap();
        let err = r.transition("b2", "a", Phase::Debate, None).unwrap_err();
        assert!(err.to_string().contains("illegal phase transition"));
    }

    #[test]
    fn archive_requires_resolve_unless_forced() {
        let r = fresh_registry();
        r.open("b1", "topic", "/proj", None, None, "alice").unwrap();
        r.register("b1", "alice", Role::Facilitator, "ops").unwrap();
        r.register("b1", "bob", Role::Specialist, "security")
            .unwrap();
        r.transition("b1", "alice", Phase::Read, None).unwrap();
        r.transition("b1", "alice", Phase::Validate, None).unwrap();
        // Non-force archive outside resolve is refused.
        let err = r.archive("b1", "alice", false).unwrap_err();
        assert!(
            err.to_string()
                .contains("archive only allowed in 'resolve'")
        );
        // Specialists cannot force-archive.
        let err = r.archive("b1", "bob", true).unwrap_err();
        assert!(err.to_string().contains("can force-archive"));
        // Facilitator force-archives the stranded board; the archived
        // phase event records the phase it was abandoned from.
        let arc = r.get("b1").unwrap();
        r.archive("b1", "alice", true).unwrap();
        assert!(r.get("b1").is_none());
        let board = arc.read();
        assert_eq!(board.phase, Phase::Archived);
        let last = board.phase_history.last().unwrap();
        assert_eq!(
            last.summary.as_deref(),
            Some("force-archived from phase 'validate'")
        );
    }

    #[test]
    fn parse_board_actions_accepts_object_array_and_fences() {
        // Single object.
        let items = parse_board_actions(
            r#"{"action":"post","type":"claim","title":"t","body":"b","severity":"high"}"#,
        )
        .unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].action.kind(), "post");
        assert!(items[0].agent_name.is_none());

        // Array with agent override + vote + abstention.
        let items = parse_board_actions(
            r#"[
                {"agent_name":"security","action":"vote","post_id":"post-1","vote":"accept","reason":"solid"},
                {"action":"annotate","post_id":"post-2","type":"corroborate","body":"agreed"},
                {"action":"none"}
            ]"#,
        )
        .unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].agent_name.as_deref(), Some("security"));
        assert_eq!(items[0].action.kind(), "vote");
        assert_eq!(items[1].action.kind(), "annotate");
        assert_eq!(items[2].action, BoardAction::None);

        // Fenced with a language tag.
        let items = parse_board_actions(
            "```json\n{\"action\":\"vote\",\"post_id\":\"p\",\"vote\":\"defer\"}\n```",
        )
        .unwrap();
        assert_eq!(items[0].action.kind(), "vote");

        // noop alias.
        let items = parse_board_actions(r#"{"action":"noop"}"#).unwrap();
        assert_eq!(items[0].action, BoardAction::None);
    }

    #[test]
    fn parse_board_actions_rejects_prose_and_bad_schema() {
        // Pure prose with no schema-valid JSON anywhere.
        let err = parse_board_actions("Deliberation reviewed. Both peer concerns are sound.")
            .unwrap_err();
        assert!(err.to_string().contains("not valid JSON"), "{err}");
        // Unknown action tag.
        let err = parse_board_actions(r#"{"action":"shout","body":"x"}"#).unwrap_err();
        assert!(err.to_string().contains("action schema"), "{err}");
        // Non-object/array JSON.
        let err = parse_board_actions("42").unwrap_err();
        assert!(err.to_string().contains("object or array"), "{err}");
    }

    #[test]
    fn parse_board_actions_salvages_answer_from_drifted_output() {
        // Prose preamble around a schema-valid answer — the exact drift
        // shape observed live (2026-07-14 run 2): the model narrates,
        // then answers. Bracket-span salvage recovers the votes.
        let items = parse_board_actions(
            "I reviewed the board carefully. My votes:\n\n[{\"action\":\"vote\",\"post_id\":\"post-1\",\"vote\":\"accept\",\"reason\":\"solid\"}]\n\nThat concludes my assessment.",
        )
        .unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].action.kind(), "vote");

        // Provider tool-call echoes: an unrelated fenced JSON blob
        // precedes the fenced answer. The tool input lacks the action
        // tag so it cannot false-positive; the real answer is found.
        let items = parse_board_actions(
            "**Tool: web_search**\n\n**Input:**\n```json\n{\"location\":\"us\",\"query\":\"config schema\"}\n```\n\nBased on that:\n\n```json\n[{\"action\":\"vote\",\"post_id\":\"post-2\",\"vote\":\"defer\"}]\n```\n",
        )
        .unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].action,
            BoardAction::Vote {
                post_id: "post-2".into(),
                vote: VoteValue::Defer,
                reason: None,
            }
        );

        // Unrelated JSON alone (no schema-valid answer anywhere) still
        // errors — salvage must not apply arbitrary JSON to the board.
        let err = parse_board_actions(
            "**Input:**\n```json\n{\"location\":\"us\",\"query\":\"x\"}\n```\nno answer given",
        )
        .unwrap_err();
        assert!(err.to_string().contains("not valid JSON"), "{err}");
    }

    #[test]
    fn apply_action_enforces_same_phase_rules_as_tools() {
        let r = fresh_registry();
        r.open("b1", "topic", "/proj", None, None, "alice").unwrap();
        r.register("b1", "alice", Role::Facilitator, "ops").unwrap();
        r.register("b1", "bob", Role::Specialist, "security")
            .unwrap();

        // Post lands in blind.
        let applied = r
            .apply_action(
                "b1",
                "bob",
                &BoardAction::Post {
                    post_type: PostType::Claim,
                    title: "t".into(),
                    body: "b".into(),
                    target_file: None,
                    target_location: None,
                    severity: None,
                    finding_refs: vec![],
                    cascade_targets: vec![],
                },
            )
            .unwrap();
        let post_id = match applied {
            AppliedAction::Post { post_id } => post_id,
            other => panic!("expected post, got {other:?}"),
        };

        // Vote outside debate is refused with the registry's own error.
        let err = r
            .apply_action(
                "b1",
                "bob",
                &BoardAction::Vote {
                    post_id: post_id.clone(),
                    vote: VoteValue::Accept,
                    reason: None,
                },
            )
            .unwrap_err();
        assert!(err.to_string().contains("voting only in 'debate'"), "{err}");

        // Advance to debate; vote applies, re-vote replaces.
        r.transition("b1", "alice", Phase::Read, None).unwrap();
        r.transition("b1", "alice", Phase::Debate, None).unwrap();
        let v1 = r
            .apply_action(
                "b1",
                "bob",
                &BoardAction::Vote {
                    post_id: post_id.clone(),
                    vote: VoteValue::Accept,
                    reason: Some("initial".into()),
                },
            )
            .unwrap();
        assert_eq!(v1, AppliedAction::Vote { replaced: false });
        let v2 = r
            .apply_action(
                "b1",
                "bob",
                &BoardAction::Vote {
                    post_id,
                    vote: VoteValue::Reject,
                    reason: Some("changed my mind".into()),
                },
            )
            .unwrap();
        assert_eq!(v2, AppliedAction::Vote { replaced: true });

        // Abstention is a clean no-op.
        assert_eq!(
            r.apply_action("b1", "bob", &BoardAction::None).unwrap(),
            AppliedAction::None
        );
    }

    #[test]
    fn archive_from_resolve_needs_no_force() {
        let r = fresh_registry();
        r.open("b1", "topic", "/proj", None, None, "alice").unwrap();
        r.register("b1", "alice", Role::Facilitator, "ops").unwrap();
        r.transition("b1", "alice", Phase::Read, None).unwrap();
        r.transition("b1", "alice", Phase::Validate, None).unwrap();
        r.transition("b1", "alice", Phase::Resolve, None).unwrap();
        let arc = r.get("b1").unwrap();
        r.archive("b1", "alice", false).unwrap();
        assert!(r.get("b1").is_none());
        // A normal resolve-phase archive carries no force annotation.
        let board = arc.read();
        assert_eq!(board.phase_history.last().unwrap().summary, None);
    }

    #[test]
    fn ready_for_transition_blind_when_all_specialists_posted() {
        let r = fresh_registry();
        r.open("b1", "topic", "/proj", None, None, "alice").unwrap();
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
        r.open("b1", "topic", "/proj", None, None, "f").unwrap();
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
        r.open("b1", "topic", "/proj", None, None, "f").unwrap();
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
        r.open("b1", "topic", "/proj", None, None, "f").unwrap();
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
    fn post_owner_can_resolve_external_challenge_on_own_post() {
        let r = fresh_registry();
        r.open("b1", "topic", "/proj", None, None, "f").unwrap();
        r.register("b1", "f", Role::Facilitator, "ops").unwrap();
        r.register("b1", "owner", Role::Specialist, "x").unwrap();
        r.register("b1", "reviewer", Role::Specialist, "y").unwrap();
        r.post(
            "b1",
            "owner",
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
        let challenge = r
            .annotate(
                "b1",
                "reviewer",
                "post-001",
                AnnotationType::Challenge,
                "missing proof",
                None,
                None,
            )
            .unwrap();
        r.annotate(
            "b1",
            "owner",
            "post-001",
            AnnotationType::Resolve,
            "proof added",
            None,
            Some(&challenge),
        )
        .unwrap();
        let arc = r.get("b1").unwrap();
        assert!(arc.read().ready_for_transition(0));
    }

    #[test]
    fn agent_cannot_resolve_own_challenge() {
        let r = fresh_registry();
        r.open("b1", "topic", "/proj", None, None, "f").unwrap();
        r.register("b1", "f", Role::Facilitator, "ops").unwrap();
        r.register("b1", "owner", Role::Specialist, "x").unwrap();
        r.register("b1", "reviewer", Role::Specialist, "y").unwrap();
        r.post(
            "b1",
            "owner",
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
        let challenge = r
            .annotate(
                "b1",
                "reviewer",
                "post-001",
                AnnotationType::Challenge,
                "missing proof",
                None,
                None,
            )
            .unwrap();
        let err = r
            .annotate(
                "b1",
                "reviewer",
                "post-001",
                AnnotationType::Resolve,
                "withdrawn",
                None,
                Some(&challenge),
            )
            .unwrap_err();
        assert!(err.to_string().contains("cannot resolve own challenge"));
    }

    #[test]
    fn template_scope_exposes_phase_and_counts() {
        let r = fresh_registry();
        r.open("b1", "topic", "/proj", None, None, "f").unwrap();
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

    // ── Phase 1: validator exclusion teeth + review coverage ───────────

    /// Board with three lens posts (post-001..003 by l1/l2/l3) plus a
    /// validator `v`, advanced to the validate phase.
    fn board_in_validate() -> WhiteboardRegistry {
        let r = fresh_registry();
        r.open("b1", "topic", "/proj", None, None, "fac").unwrap();
        r.register("b1", "fac", Role::Facilitator, "ops").unwrap();
        r.register("b1", "v", Role::Specialist, "validator")
            .unwrap();
        for lens in ["l1", "l2", "l3"] {
            r.register("b1", lens, Role::Specialist, "lens").unwrap();
            r.post(
                "b1",
                lens,
                PostType::Claim,
                lens,
                "body",
                None,
                None,
                None,
                vec![],
                vec![],
            )
            .unwrap();
        }
        r.transition("b1", "fac", Phase::Read, None).unwrap();
        r.transition("b1", "fac", Phase::Validate, None).unwrap();
        r
    }

    #[test]
    fn post_standing_precedence() {
        let r = board_in_validate();
        r.annotate(
            "b1",
            "v",
            "post-001",
            AnnotationType::Validation,
            "ok",
            Some(ValidationResult::Confirmed),
            None,
        )
        .unwrap();
        r.annotate(
            "b1",
            "v",
            "post-002",
            AnnotationType::Validation,
            "nope",
            Some(ValidationResult::Refuted),
            None,
        )
        .unwrap();
        let arc = r.get("b1").unwrap();
        let b = arc.read();
        assert_eq!(b.post_standing("post-001"), PostStanding::Confirmed);
        assert_eq!(b.post_standing("post-002"), PostStanding::Excluded);
        // No validation at all → unvalidated (survives with a warning).
        assert_eq!(b.post_standing("post-003"), PostStanding::Unvalidated);
    }

    #[test]
    fn post_standing_refuted_wins_and_confirmed_beats_inconclusive() {
        let r = board_in_validate();
        r.register("b1", "v2", Role::Specialist, "validator")
            .unwrap();
        // post-001: confirmed + refuted → Excluded (refuted is dispositive).
        r.annotate(
            "b1",
            "v",
            "post-001",
            AnnotationType::Validation,
            "c",
            Some(ValidationResult::Confirmed),
            None,
        )
        .unwrap();
        r.annotate(
            "b1",
            "v2",
            "post-001",
            AnnotationType::Validation,
            "r",
            Some(ValidationResult::Refuted),
            None,
        )
        .unwrap();
        // post-002: confirmed + inconclusive → Confirmed.
        r.annotate(
            "b1",
            "v",
            "post-002",
            AnnotationType::Validation,
            "c",
            Some(ValidationResult::Confirmed),
            None,
        )
        .unwrap();
        r.annotate(
            "b1",
            "v2",
            "post-002",
            AnnotationType::Validation,
            "i",
            Some(ValidationResult::Inconclusive),
            None,
        )
        .unwrap();
        // post-003: inconclusive only → Inconclusive.
        r.annotate(
            "b1",
            "v",
            "post-003",
            AnnotationType::Validation,
            "i",
            Some(ValidationResult::Inconclusive),
            None,
        )
        .unwrap();
        let arc = r.get("b1").unwrap();
        let b = arc.read();
        assert_eq!(b.post_standing("post-001"), PostStanding::Excluded);
        assert_eq!(b.post_standing("post-002"), PostStanding::Confirmed);
        assert_eq!(b.post_standing("post-003"), PostStanding::Inconclusive);
    }

    #[test]
    fn validation_summary_partitions_surviving_and_excluded() {
        let r = board_in_validate();
        r.annotate(
            "b1",
            "v",
            "post-001",
            AnnotationType::Validation,
            "c",
            Some(ValidationResult::Confirmed),
            None,
        )
        .unwrap();
        r.annotate(
            "b1",
            "v",
            "post-002",
            AnnotationType::Validation,
            "r",
            Some(ValidationResult::Refuted),
            None,
        )
        .unwrap();
        // post-003 left unvalidated.
        let arc = r.get("b1").unwrap();
        let vs = arc.read().validation_summary();
        assert_eq!(vs.excluded_post_ids, vec!["post-002".to_string()]);
        assert_eq!(
            vs.surviving_post_ids,
            vec!["post-001".to_string(), "post-003".to_string()]
        );
        assert_eq!(vs.confirmed_count, 1);
        assert_eq!(vs.refuted_count, 1);
        assert_eq!(vs.unvalidated_count, 1);
        assert_eq!(vs.inconclusive_count, 0);
    }

    #[test]
    fn unreviewed_post_count_requires_cross_agent_review() {
        let r = board_in_validate();
        // Conflict path: validate → debate (canonical). Self-annotation never counts.
        r.transition("b1", "fac", Phase::Debate, None).unwrap();
        let arc = r.get("b1").unwrap();
        assert_eq!(arc.read().unreviewed_post_count(), 3);
        // l2 challenges post-001, l3 corroborates post-002 → only post-003 unreviewed.
        r.annotate(
            "b1",
            "l2",
            "post-001",
            AnnotationType::Challenge,
            "disagree",
            None,
            None,
        )
        .unwrap();
        r.annotate(
            "b1",
            "l3",
            "post-002",
            AnnotationType::Corroborate,
            "agree",
            None,
            None,
        )
        .unwrap();
        let arc = r.get("b1").unwrap();
        assert_eq!(arc.read().unreviewed_post_count(), 1);
    }

    #[test]
    fn validate_to_resolve_skip_is_legal() {
        let r = board_in_validate();
        // Conflict-free path skips debate entirely.
        r.transition("b1", "fac", Phase::Resolve, None).unwrap();
        let arc = r.get("b1").unwrap();
        assert_eq!(arc.read().phase, Phase::Resolve);
    }

    #[test]
    fn board_without_project_id_decodes_and_round_trips() {
        let legacy = serde_json::json!({
            "id": "b-legacy",
            "topic": "t",
            "project": "/repo/old",
            "created_at": "2026-07-24T00:00:00Z",
            "phase": "blind",
            "phase_history": [],
            "agents": {},
            "posts": [],
            "annotations": [],
            "votes": []
        });
        let board: Board = serde_json::from_value(legacy).unwrap();
        assert_eq!(board.project_id, None);
        assert!(
            serde_json::to_value(&board)
                .unwrap()
                .get("project_id")
                .is_none()
        );
    }

    #[test]
    fn opened_board_carries_the_stamped_project_id() {
        let r = WhiteboardRegistry::new();
        r.open(
            "b-stamped",
            "topic",
            "/repo/x",
            Some("abc12345"),
            None,
            "alice",
        )
        .unwrap();

        let board = r.get("b-stamped").unwrap();
        assert_eq!(board.read().project_id.as_deref(), Some("abc12345"));
        // A board opened without a stamped id stays on the path lane.
        r.open("b-legacy-open", "topic", "/repo/x", None, None, "alice")
            .unwrap();
        assert_eq!(r.get("b-legacy-open").unwrap().read().project_id, None);
    }

    #[test]
    fn retirement_discharge_removes_only_owned_boards_and_is_idempotent() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap().join("whiteboards");
        let registry = WhiteboardRegistry::new();
        registry.set_storage_dir(root.clone()).unwrap();
        registry
            .open(
                "owned",
                "topic",
                "/repo/a",
                Some("project-a"),
                None,
                "alice",
            )
            .unwrap();
        registry
            .open(
                "other",
                "topic",
                "/repo/b",
                Some("project-b"),
                None,
                "alice",
            )
            .unwrap();

        assert_eq!(
            discharge_project_catalog_rows(&root, "project-a", &["/repo/a".into()]).unwrap(),
            1
        );
        assert_eq!(
            discharge_project_catalog_rows(&root, "project-a", &["/repo/a".into()]).unwrap(),
            0
        );
        assert!(!root.join("owned.json").exists());
        assert!(root.join("other.json").is_file());
    }
}
