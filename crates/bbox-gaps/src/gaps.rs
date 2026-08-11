//! First-class gap-note store.
//!
//! A "gap note" is a substrate-gap field report: a record that the blackbox
//! substrate (a tool primitive, MCP surface, refactor atom, workflow shape,
//! ontology edge, runbook) is missing a reusable capability. Gap notes used to
//! ride inside the side-channel notes surface as a `blackbox.gap_note.v1` JSON
//! envelope crammed into a `kind=followup` body. They are now first-class: a
//! typed [`GapNote`] in a dedicated, repo-owned store that mirrors the
//! `.bbox/knowledge/` persistence model (one file per record, project/central
//! split, atomic-write + store-lock, watcher reload).
//!
//! Deliberately simpler than the knowledge store: NO recall telemetry sidecar,
//! NO tantivy/search indexing, NO rendered provider memory. Gaps get durable
//! persistence + live reload only.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{Context, Result};
use rmcp::schemars;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::repo_io::{GapRepoCarrier, GapRepoRead, GapRepoWrite};
use bbox_corpus_core::project_selector::project_scope_matches;

pub const GAP_NOTE_TYPE: &str = "blackbox.gap_note.v1";

// ── Vocabularies ───────────────────────────────────────────────────

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    strum::EnumString,
    strum::AsRefStr,
    strum::VariantNames,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum GapKind {
    /// Predicate the rule-packet AST cannot express.
    PacketAst,
    /// Missing CLI / shell / refactor helper.
    Tooling,
    /// Needed dispatchable agent that does not exist.
    Agent,
    /// Missing arc / wait / fork / cancel shape.
    Workflow,
    /// Language-specific refactor atom.
    RefactorPrimitive,
    /// Wrong allow/deny shape for a recurring role.
    McpSurface,
    /// Missing entity type or edge family.
    Ontology,
    /// Packet or test eval cannot reach a class of cases.
    EvalCoverage,
    /// Missing rendered guidance or runbook.
    DocsRunbook,
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Default,
    Serialize,
    Deserialize,
    strum::EnumString,
    strum::AsRefStr,
    strum::VariantNames,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum GapImpact {
    /// Nuisance, easy manual workaround.
    Low,
    /// Repeated friction or weak mechanization.
    #[default]
    Medium,
    /// Blocks useful automation or causes recurring bad agent behavior.
    High,
    /// Causes unsafe edits, data-loss risk, or unusable workflows.
    Critical,
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Default,
    Serialize,
    Deserialize,
    strum::EnumString,
    strum::AsRefStr,
    strum::VariantNames,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum BlockingLevel {
    None,
    #[default]
    WorkaroundAvailable,
    BlocksTask,
    BlocksClassOfWork,
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Default,
    Serialize,
    Deserialize,
    strum::EnumString,
    strum::AsRefStr,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum GapResolution {
    /// Reported, not triaged.
    #[default]
    Unresolved,
    /// Seen, deduped, accepted for later handling.
    Acknowledged,
    /// Implemented, rejected, superseded, or intentionally closed.
    Addressed,
}

// ── Record ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GapNote {
    /// Canonical id `gap-<8hex>` (mirrors `note-<8hex>` / `thread-<8hex>`).
    pub id: String,
    pub title: String,
    pub gap_kind: GapKind,
    pub domain: String,
    pub wanted_capability: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub missing_primitive: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_used: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<String>,
    #[serde(default)]
    pub impact: GapImpact,
    #[serde(default)]
    pub blocking_level: BlockingLevel,
    /// Stable dedupe key `<gap_kind>/<domain>/<slug>`.
    pub dedupe_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggested_owner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    /// This gap supersedes another (`gap-<8hex>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<String>,
    /// This gap was superseded by another (`gap-<8hex>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    #[serde(default)]
    pub resolution: GapResolution,
    // ── Provenance (omitted on disk for repo-owned files — location encodes
    //    scope, exactly like knowledge entries omit `project`). ──
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// Resolving authority's project id, stamped on write. Absent on rows
    /// written before the catalog cut: those stay on the path lane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    /// Transient logical write-carrier id. A managed checkout carries the
    /// repo-owned gap file while `project` remains the durable base scope.
    /// Never retained in the central store or committed record; the checkout
    /// registry reconstructs its provisional overlay after restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write_dir: Option<String>,
    /// Checkout identity for a provisional variant in a detached read view.
    /// Never persisted in either the central store or repo-owned files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provisional_checkout_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bro: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    pub created_at: String,
    /// Defaulted on deserialize (backfilled from `created_at` at repo-file
    /// load) so a committed gap file written without it — by hand, by an
    /// older producer, or by another machine — is not rejected. A rejected
    /// file is invisible to the store and was historically DELETED by the
    /// save-side purge (see `persist_repo_gap_entries`).
    #[serde(default)]
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution_note: Option<String>,
}

// ── Validation helpers ─────────────────────────────────────────────

fn variants_list(variants: &[&str]) -> String {
    variants.join(", ")
}

fn parse_gap_kind(value: &str) -> Result<GapKind> {
    GapKind::from_str(value.trim()).map_err(|_| {
        anyhow::anyhow!(
            "gap_kind must be one of: {}",
            variants_list(<GapKind as strum::VariantNames>::VARIANTS)
        )
    })
}

fn parse_impact(value: Option<&str>) -> Result<GapImpact> {
    match value.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(GapImpact::default()),
        Some(v) => GapImpact::from_str(v).map_err(|_| {
            anyhow::anyhow!(
                "impact must be one of: {}",
                variants_list(<GapImpact as strum::VariantNames>::VARIANTS)
            )
        }),
    }
}

fn parse_blocking_level(value: Option<&str>) -> Result<BlockingLevel> {
    match value.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(BlockingLevel::default()),
        Some(v) => BlockingLevel::from_str(v).map_err(|_| {
            anyhow::anyhow!(
                "blocking_level must be one of: {}",
                variants_list(<BlockingLevel as strum::VariantNames>::VARIANTS)
            )
        }),
    }
}

/// `<gap_kind>/<domain>/<slug>` — at least three non-empty slash segments.
fn validate_dedupe_key(key: &str) -> Result<()> {
    let segments: Vec<&str> = key.split('/').collect();
    if segments.len() < 3 || segments.iter().any(|s| s.trim().is_empty()) {
        anyhow::bail!("dedupe_key must use `<gap_kind>/<domain>/<slug>` (3+ non-empty segments)");
    }
    Ok(())
}

fn slugify(s: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in s.trim().to_lowercase().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            last_dash = false;
        } else if !last_dash && !out.is_empty() {
            out.push('-');
            last_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        out.push_str("gap");
    }
    out
}

fn str_field(object: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    object
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

impl GapNote {
    /// Build a [`GapNote`] from a `blackbox.gap_note.v1` JSON envelope. Used by
    /// the spool importer and `bbox_packet_gap` (the programmatic producers).
    /// `now`/`id` are supplied by the caller. A missing `dedupe_key` is derived
    /// from `<gap_kind>/<domain>/<slug(title)>` rather than rejected, so older
    /// host-dropped envelopes still ingest.
    pub fn from_envelope(value: &Value, id: String, now: String) -> Result<Self> {
        let object = value
            .as_object()
            .context("gap envelope must be a JSON object")?;
        if object.get("type").and_then(Value::as_str) != Some(GAP_NOTE_TYPE) {
            anyhow::bail!("gap envelope must have type={GAP_NOTE_TYPE}");
        }
        let title = str_field(object, "title").context("gap envelope missing `title`")?;
        let gap_kind_raw =
            str_field(object, "gap_kind").context("gap envelope missing `gap_kind`")?;
        let gap_kind = parse_gap_kind(&gap_kind_raw)?;
        let domain = str_field(object, "domain").context("gap envelope missing `domain`")?;
        let wanted_capability = str_field(object, "wanted_capability")
            .context("gap envelope missing `wanted_capability`")?;
        let dedupe_key = match str_field(object, "dedupe_key") {
            Some(key) => {
                validate_dedupe_key(&key)?;
                key
            }
            None => format!(
                "{}/{}/{}",
                gap_kind.as_ref(),
                slugify(&domain),
                slugify(&title)
            ),
        };
        let evidence = object
            .get("evidence")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        Ok(Self {
            id,
            title,
            gap_kind,
            domain,
            wanted_capability,
            missing_primitive: str_field(object, "missing_primitive"),
            fallback_used: str_field(object, "fallback_used"),
            evidence,
            impact: parse_impact(object.get("impact").and_then(Value::as_str))?,
            blocking_level: parse_blocking_level(
                object.get("blocking_level").and_then(Value::as_str),
            )?,
            dedupe_key,
            suggested_owner: str_field(object, "suggested_owner"),
            notes: str_field(object, "notes"),
            supersedes: None,
            superseded_by: None,
            resolution: GapResolution::Unresolved,
            project: None,
            project_id: None,
            write_dir: None,
            provisional_checkout_id: None,
            task_id: None,
            session_id: None,
            provider: None,
            bro: None,
            thread_id: None,
            created_at: now.clone(),
            updated_at: now,
            resolved_at: None,
            resolution_note: None,
        })
    }

    fn matches_id(&self, needle: &str) -> bool {
        let needle = needle.trim();
        self.id == needle || self.id.strip_prefix("gap-") == Some(needle)
    }
}

// ── MCP parameter structs ──────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GapFileParams {
    /// One-line summary of the missing capability.
    pub title: String,
    /// One of: packet_ast, tooling, agent, workflow, refactor_primitive,
    /// mcp_surface, ontology, eval_coverage, docs_runbook.
    pub gap_kind: String,
    /// Subsystem / domain tag (free text), e.g. `review-policy`.
    pub domain: String,
    /// What capability you wanted that the substrate could not provide.
    pub wanted_capability: String,
    /// Stable dedupe key `<gap_kind>/<domain>/<slug>`. Boring and stable so a
    /// recurrence reuses the same key.
    pub dedupe_key: String,
    /// One of: low, medium, high, critical (default: medium).
    #[serde(default)]
    pub impact: Option<String>,
    /// One of: none, workaround_available, blocks_task, blocks_class_of_work.
    #[serde(default)]
    pub blocking_level: Option<String>,
    /// The specific primitive/surface that was missing.
    #[serde(default)]
    pub missing_primitive: Option<String>,
    /// What you did instead (the manual workaround).
    #[serde(default)]
    pub fallback_used: Option<String>,
    /// Evidence refs (file:line, packet-event ids, thread ids, ...).
    #[serde(default)]
    pub evidence: Option<Vec<String>>,
    /// Who should own the fix (default: blackbox).
    #[serde(default)]
    pub suggested_owner: Option<String>,
    /// Free-text addendum.
    #[serde(default)]
    pub notes: Option<String>,
    /// Storage scope: `project` (default → in-repo `.bbox/gaps/`) or `global`
    /// (→ central host store, for cross-project substrate gaps).
    #[serde(default)]
    pub scope: Option<String>,
    /// Project path (resolved by the tool layer). Owns the gap when
    /// scope=project; ignored when scope=global.
    #[serde(default)]
    pub project: Option<String>,
    /// Internal logical write-carrier id set by the MCP adapter for managed
    /// fleet worktrees. Not accepted from clients and omitted from the schema.
    #[serde(skip)]
    #[schemars(skip)]
    pub write_dir: Option<String>,
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub bro: Option<String>,
    #[serde(default)]
    #[schemars(regex(pattern = r"^(thread-)?[0-9a-f]{8}$"))]
    pub thread_id: Option<String>,
    /// File a fresh occurrence even when an open gap with the same dedupe_key
    /// exists (recurrence tally). Default false → dedupes to the existing gap.
    #[serde(default)]
    pub allow_recurrence: Option<bool>,
    /// Internal, not part of the MCP schema: the resolving authority's
    /// project id. Set by the daemon adapter from the resolver, never
    /// accepted from the wire, so identity cannot be caller-asserted.
    #[serde(skip)]
    pub project_id: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GapListParams {
    /// Exact gap id `gap-<8hex>` (bare 8-hex suffix accepted).
    #[serde(default)]
    #[schemars(regex(pattern = r"^(gap-)?[0-9a-f]{8}$"))]
    pub id: Option<String>,
    #[serde(default)]
    pub gap_kind: Option<String>,
    #[serde(default)]
    pub domain: Option<String>,
    #[serde(default)]
    pub impact: Option<String>,
    #[serde(default)]
    pub blocking_level: Option<String>,
    #[serde(default)]
    pub dedupe_key: Option<String>,
    /// Filter by resolution: unresolved, acknowledged, addressed.
    #[serde(default)]
    pub resolution: Option<String>,
    /// Restrict to one project's gaps. Accepts an absolute project path
    /// (e.g. `/home/user/repos/my-app`), a project_id, or a registered
    /// project alias; an unresolvable value keeps literal substring-filter
    /// semantics.
    #[serde(default)]
    pub project: Option<String>,
    /// Provisional visibility policy: published, own, or all.
    #[serde(default)]
    pub provisional: Option<String>,
    /// Free-text substring over title/domain/wanted_capability.
    #[serde(default)]
    pub query: Option<String>,
    /// ISO 8601: only gaps created at or after this timestamp.
    #[serde(default)]
    pub since: Option<String>,
    #[serde(default)]
    pub limit: Option<u64>,
    /// Include addressed gaps (default: false for lists, true for exact id).
    #[serde(default)]
    pub include_addressed: Option<bool>,
    /// Emit machine-readable JSON records instead of the rendered text view.
    #[serde(default)]
    pub json: Option<bool>,
    /// Project id from the resolver. When both this and a row carry an
    /// id, the id decides and the path predicate is not consulted.
    #[serde(default)]
    pub project_id: Option<String>,
    /// Internal, not part of the MCP schema: historical path keys the
    /// host-local `LegacyPathBinding` ledger maps to this query's project
    /// (plan §8.2 catalog-mode arm), so path-only rows written before
    /// attachment relocation stay visible. Empty on the bridge, which has no
    /// ledger. Set by the daemon adapter, never accepted from the wire.
    #[serde(skip)]
    #[schemars(skip)]
    pub project_ledger_paths: Vec<String>,
}

#[derive(Debug, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GapResolveParams {
    /// Gap id `gap-<8hex>` (bare 8-hex suffix accepted).
    #[schemars(regex(pattern = r"^(gap-)?[0-9a-f]{8}$"))]
    pub id: String,
    /// One of: unresolved, acknowledged, addressed.
    pub resolution: String,
    /// Optional resolution text (terminal reason).
    #[serde(default)]
    pub note: Option<String>,
    /// Mark this gap superseded by another (`gap-<8hex>`); wires the structured
    /// supersedes/superseded_by link on both records.
    #[serde(default)]
    #[schemars(regex(pattern = r"^(gap-)?[0-9a-f]{8}$"))]
    pub superseded_by: Option<String>,
    /// Session cwd / worktree path for WRITE-TARGETING only: when this
    /// resolves to a recognized worktree of the gap's own project, the
    /// rewritten repo-owned gap file lands in that worktree (the session
    /// commits it; the branch carries it). The gap's durable project scope
    /// never changes. Absent → the file is rewritten where it lives today
    /// (the base checkout). Ignored for global-scope gaps.
    #[serde(default)]
    pub project: Option<String>,
    /// Internal logical write-carrier id resolved by the MCP adapter from
    /// `project`. Not accepted from clients and omitted from the tool schema.
    #[serde(skip)]
    #[schemars(skip)]
    pub write_dir: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GapUpdateParams {
    /// Gap id `gap-<8hex>` (bare 8-hex suffix accepted).
    #[schemars(regex(pattern = r"^(gap-)?[0-9a-f]{8}$"))]
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub domain: Option<String>,
    #[serde(default)]
    pub wanted_capability: Option<String>,
    #[serde(default)]
    pub impact: Option<String>,
    #[serde(default)]
    pub blocking_level: Option<String>,
    #[serde(default)]
    pub missing_primitive: Option<String>,
    #[serde(default)]
    pub fallback_used: Option<String>,
    #[serde(default)]
    pub evidence: Option<Vec<String>>,
    #[serde(default)]
    pub suggested_owner: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
    /// Session cwd / worktree path for WRITE-TARGETING only: when this
    /// resolves to a recognized worktree of the gap's own project, the
    /// rewritten repo-owned gap file lands in that worktree (the session
    /// commits it; the branch carries it). The gap's durable project scope
    /// never changes. Absent → the file is rewritten where it lives today
    /// (the base checkout). Ignored for global-scope gaps.
    #[serde(default)]
    pub project: Option<String>,
    /// Internal logical write-carrier id resolved by the MCP adapter from
    /// `project`. Not accepted from clients and omitted from the tool schema.
    #[serde(skip)]
    #[schemars(skip)]
    pub write_dir: Option<String>,
}

// ── Persistence ────────────────────────────────────────────────────
//
// Mirrors the repo-owned `.bbox/knowledge/` model: project-scoped gaps live one
// file per record under `<project>/.bbox/gaps/gap-<8hex>.json` and travel with
// the checkout; global gaps stay in the central host store. The on-disk file
// omits the `project` field — location encodes scope.
//
// Collision note: `gap_spool` owns `<project>/.bbox/gaps/inbox/` (drop folder,
// with `imported/`/`rejected/` nested under it). The loader reads top-level
// `*.json` only (non-recursive `read_dir` + extension filter naturally skips the
// `inbox/` subdir), so durable gaps and the spool never alias.

fn repo_gaps_dir(project_dir: &Path) -> PathBuf {
    project_dir.join(".bbox").join("gaps")
}

const MAX_LIVE_GAP_FILE_BYTES: usize = 2 * 1024 * 1024;

/// The exact bytes a committed `.bbox/gaps/<id>.json` file carries.
///
/// Gap-side twin of `committed_knowledge_entry_bytes`, and one owner for
/// the same reason: accepted publication hashes these bytes exactly
/// (D-014), so a fixture that encodes them differently produces generation
/// hashes over bytes no writer would commit.
pub fn committed_gap_note_bytes(entry: &GapNote) -> Result<Vec<u8>> {
    let mut on_disk = entry.clone();
    on_disk.project = None;
    on_disk.write_dir = None;
    on_disk.provisional_checkout_id = None;
    bbox_corpus_core::json_store::to_vec_pretty_newline(&on_disk)
}

fn validate_repo_gap_id(id: &str) -> Result<()> {
    let mut components = Path::new(id).components();
    let Some(std::path::Component::Normal(name)) = components.next() else {
        anyhow::bail!("gap id is not a confined basename: {id:?}");
    };
    if components.next().is_some() || name.to_str() != Some(id) {
        anyhow::bail!("gap id is not a confined basename: {id:?}");
    }
    Ok(())
}

fn validate_repo_gap_filename(path: &Path, id: &str) -> Result<()> {
    validate_repo_gap_id(id)?;
    let expected = format!("{id}.json");
    if path.file_name().and_then(|name| name.to_str()) != Some(expected.as_str()) {
        anyhow::bail!(
            "repo-owned gap filename/id mismatch: {} contains id {id}",
            path.display()
        );
    }
    Ok(())
}

fn read_live_gap_file(path: &Path) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspecting repo-owned gap file {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        anyhow::bail!(
            "repo-owned gap is not a regular non-symlink file: {}",
            path.display()
        );
    }
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("opening repo-owned gap file {}", path.display()))?;
    if !file.metadata()?.file_type().is_file() {
        anyhow::bail!("repo-owned gap is not a regular file: {}", path.display());
    }
    let mut bytes = Vec::new();
    file.by_ref()
        .take((MAX_LIVE_GAP_FILE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_LIVE_GAP_FILE_BYTES {
        anyhow::bail!(
            "repo-owned gap exceeds {} bytes: {}",
            MAX_LIVE_GAP_FILE_BYTES,
            path.display()
        );
    }
    Ok(bytes)
}

/// A project is "repo-owned" for gaps once its `.bbox/gaps/` dir exists — via a
/// clone that carries it, `bbox_project_init`, the spool dropping a file, or the
/// first project-scoped `bbox_gap`.
fn project_is_repo_owned(project_dir: &Path) -> bool {
    repo_gaps_dir(project_dir).is_dir()
}

/// Load every project-scoped gap committed under `<project>/.bbox/gaps/`,
/// stamping each with `project = project_dir` (absent on disk). Top-level
/// `*.json` only; tolerant skip-and-continue per file.
fn load_repo_gap_entries(project_dir: &Path, durable_project: &str) -> Result<Vec<GapNote>> {
    let git_root = bbox_corpus_core::git::git_root_for_path(project_dir);
    let transaction_root = git_root.as_deref().unwrap_or(project_dir);
    if bbox_corpus_core::transaction::has_pending_transaction(transaction_root) {
        tracing::debug!(
            project = %project_dir.display(),
            "gaps load skipped while a repo-owned transaction is pending"
        );
        return Ok(Vec::new());
    }
    let dir = repo_gaps_dir(project_dir);
    let directory = match bbox_corpus_core::json_store::NofollowDirectory::open_existing(&dir) {
        Ok(Some(directory)) => directory,
        Ok(None) => return Ok(Vec::new()),
        Err(error) => {
            tracing::warn!(
                "gaps load: refusing unsafe directory {}: {error:#}",
                dir.display()
            );
            return Ok(Vec::new());
        }
    };
    let mut out = Vec::new();
    let mut skipped = 0usize;
    let read_dir = match fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(e) => {
            tracing::warn!("gaps load: cannot read {}: {e}", dir.display());
            return Ok(Vec::new());
        }
    };
    for de in read_dir {
        let path = match de {
            Ok(de) => de.path(),
            Err(e) => {
                tracing::warn!("gaps load: unreadable dir entry in {}: {e}", dir.display());
                skipped += 1;
                continue;
            }
        };
        // Non-recursive: the `inbox/` subdir (spool-owned) has no `.json`
        // extension and is skipped here.
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let raw = match read_live_gap_file(&path) {
            Ok(raw) => raw,
            Err(e) => {
                tracing::warn!("gaps load: skipping unreadable {}: {e}", path.display());
                skipped += 1;
                continue;
            }
        };
        let mut entry: GapNote = match serde_json::from_slice(&raw) {
            Ok(entry) => entry,
            Err(e) => {
                tracing::warn!("gaps load: skipping unparseable {}: {e}", path.display());
                skipped += 1;
                continue;
            }
        };
        if let Err(error) = validate_repo_gap_filename(&path, &entry.id) {
            tracing::warn!(
                "gaps load: skipping unsafe repo-owned entry {}: {error}",
                path.display()
            );
            skipped += 1;
            continue;
        }
        entry.project = Some(durable_project.to_string());
        // Committed records never carry a write redirect — the file's
        // location IS the carrier. Clearing here also makes loading a base
        // root's file the merge-observation signal that drops a retained
        // redirect (the load overwrites the central copy by id).
        entry.write_dir = None;
        entry.provisional_checkout_id = None;
        if entry.updated_at.is_empty() {
            entry.updated_at = entry.created_at.clone();
        }
        out.push(entry);
    }
    if skipped > 0 {
        tracing::warn!(
            "gaps load: {} loaded={} skipped={}",
            dir.display(),
            out.len(),
            skipped
        );
    }
    if let Err(error) = directory.ensure_still_current() {
        tracing::warn!(
            "gaps load: directory changed during read {}; discarding snapshot: {error:#}",
            dir.display()
        );
        return Ok(Vec::new());
    }
    Ok(out)
}

/// Persist `entries` (all owned by `project_dir`) one file per gap under
/// `<project>/.bbox/gaps/`, with the `project` field cleared. `purge` deletes
/// committed files whose gap has been reassigned away from this dir
/// (generation semantics for write_dir migrations / project moves).
///
/// `known_ids` is the set of gap ids successfully loaded from this concrete
/// carrier. A file whose id was not accepted from this carrier is never
/// deleted: it arrived out-of-band (git pull, peer commit, hand-authoring),
/// failed to deserialize, or collided with another scope at load. In every
/// case the in-memory set is not authoritative for it, and deleting it would
/// destroy committed repo-owned gap state (this exact clobber shipped once;
/// see gap-1f3894cc).
///
/// `redirected_ids` are gaps whose durable project IS this dir but whose
/// rewrite was redirected into a worktree this save (session write-
/// targeting). Their committed base-checkout files must survive the purge
/// untouched: redirection is not reassignment — the worktree branch carries
/// the new copy and the merge (not the daemon) updates the base.
fn persist_repo_gap_entries(
    project_dir: &Path,
    entries: &[&GapNote],
    purge: bool,
    known_ids: &BTreeSet<&str>,
    redirected_ids: &BTreeSet<&str>,
) -> Result<()> {
    let checkout_dir = match bbox_corpus_core::git::git_root_for_path(project_dir) {
        Some(root) => root,
        None => project_dir.canonicalize().with_context(|| {
            format!(
                "resolving non-git gap transaction root at {}",
                project_dir.display()
            )
        })?,
    };
    bbox_corpus_core::transaction::apply_planned_transaction(&checkout_dir, || {
        use bbox_corpus_core::transaction::TransactionWrite;

        let dir = repo_gaps_dir(project_dir);
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let mut writes = Vec::new();
        if purge {
            let keep: BTreeSet<&str> = entries.iter().map(|entry| entry.id.as_str()).collect();
            for directory_entry in
                fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))?
            {
                let path = directory_entry?.path();
                if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                    continue;
                }
                let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                    tracing::warn!(
                        "gaps save: keeping non-UTF-8 on-disk gap file {}; refusing to purge",
                        path.display()
                    );
                    continue;
                };
                if keep.contains(stem) || redirected_ids.contains(stem) {
                    continue;
                }
                if !known_ids.contains(stem) {
                    tracing::warn!(
                        "gaps save: keeping unknown on-disk gap file {}; id not in store \
                         (out-of-band file or load-time rejection); refusing to purge",
                        path.display()
                    );
                    continue;
                }
                writes.push(TransactionWrite {
                    target: path,
                    new_bytes: None,
                });
            }
        }

        for entry in entries {
            validate_repo_gap_id(&entry.id)?;
            let path = dir.join(format!("{}.json", entry.id));
            let new_bytes = committed_gap_note_bytes(entry)?;
            let unchanged = match fs::symlink_metadata(&path) {
                Ok(metadata)
                    if metadata.file_type().is_file() && !metadata.file_type().is_symlink() =>
                {
                    read_live_gap_file(&path)? == new_bytes
                }
                Ok(_) => {
                    anyhow::bail!(
                        "refusing to overwrite non-regular or symlink gap file {}",
                        path.display()
                    );
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("inspecting gap file {}", path.display()));
                }
            };
            if !unchanged {
                writes.push(TransactionWrite {
                    target: path,
                    new_bytes: Some(new_bytes),
                });
            }
        }
        Ok(writes)
    })?;
    Ok(())
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GapStoreData {
    pub version: u32,
    pub gaps: Vec<GapNote>,
}

#[derive(Debug, Clone, Default)]
pub struct GapViewMetadata {
    /// Response-local reference into the containing view's `built_from`
    /// table. Detached read-view metadata only, never durable gap provenance.
    pub built_from_ref: Option<String>,
    /// Explicit label for an unstamped compatibility row.
    pub compatibility_lane: Option<String>,
}

impl GapStoreData {
    fn new() -> Self {
        Self {
            version: 1,
            gaps: Vec::new(),
        }
    }
}

/// Capture central gap rows that still carry a literal project selector.
/// Missing and malformed stores are returned as typed source states.
pub fn capture_project_catalog_owner_snapshot(
    store_path: &Path,
    limits: bbox_corpus_core::project_catalog_snapshot::OwnerSnapshotLimitsV1,
) -> std::result::Result<
    bbox_corpus_core::project_catalog_snapshot::OwnerSnapshotV1,
    bbox_corpus_core::project_catalog_snapshot::OwnerSnapshotError,
> {
    use bbox_corpus_core::project_catalog_snapshot::{
        LegacyProjectSelectorKindV1, OwnerSnapshotRowV1, capture_json_owner,
    };

    capture_json_owner(store_path, "gap", "gap:central-json", limits, |bytes| {
        let store: GapStoreData = serde_json::from_slice(bytes).map_err(|_| ())?;
        Ok(store
            .gaps
            .into_iter()
            .filter_map(|gap| {
                let selector = gap.project?.trim().to_string();
                (!selector.is_empty()).then(|| {
                    OwnerSnapshotRowV1::legacy_selector(
                        gap.id,
                        LegacyProjectSelectorKindV1::Project,
                        selector,
                    )
                })
            })
            .collect())
    })
}

/// Stamp one central gap row with its stable project id, the write-back inverse of
/// [`capture_project_catalog_owner_snapshot`]. Idempotent: a row already
/// carrying this exact id reports `AlreadyStamped` without writing.
pub fn stamp_project_catalog_owner_row(
    store_path: &Path,
    source_row_id: &str,
    expected_members: &bbox_corpus_core::project_catalog_snapshot::LegacySelectorMembersV1,
    project_id: &str,
    limits: bbox_corpus_core::project_catalog_snapshot::OwnerSnapshotLimitsV1,
) -> std::result::Result<
    bbox_corpus_core::project_catalog_snapshot::OwnerRowStampOutcomeV1,
    bbox_corpus_core::project_catalog_snapshot::OwnerRowStampError,
> {
    bbox_corpus_core::project_catalog_snapshot::ensure_singleton_member_evidence(
        source_row_id,
        expected_members,
    )?;
    use bbox_corpus_core::project_catalog_snapshot::{stamp_json_array_row, stamp_json_owner_row};

    stamp_json_owner_row(store_path, "gap", "gap:central-json", limits, |bytes| {
        stamp_json_array_row(bytes, "gaps", "id", source_row_id, project_id)
    })
}

/// Read the stable project ids of MANY central gap rows, the VERIFY half of
/// [`stamp_project_catalog_owner_row`].
///
/// Read-only by construction: the backfill's verify proves that the rows an
/// applied plan claims to have stamped really carry the project id the ledger
/// binds them to, and a verify that could write would be proving its own work.
/// Batched over the whole requested set, so verifying this owner costs ONE
/// locked capture and answers every row from ONE durable snapshot.
pub fn read_project_catalog_owner_rows(
    store_path: &Path,
    rows: &bbox_corpus_core::project_catalog_snapshot::OwnerRowRequestV1,
    limits: bbox_corpus_core::project_catalog_snapshot::OwnerSnapshotLimitsV1,
) -> std::result::Result<
    bbox_corpus_core::project_catalog_snapshot::OwnerRowBatchV1,
    bbox_corpus_core::project_catalog_snapshot::OwnerRowStampError,
> {
    bbox_corpus_core::project_catalog_snapshot::ensure_singleton_member_evidence_batch(rows)?;
    let source_row_ids = &rows
        .keys()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    use bbox_corpus_core::project_catalog_snapshot::{
        read_json_array_rows_project_id, read_json_owner_rows,
    };

    read_json_owner_rows(store_path, "gap", "gap:central-json", limits, |bytes| {
        read_json_array_rows_project_id(bytes, "gaps", "id", source_row_ids)
    })
}

pub struct GapStore {
    store_path: PathBuf,
    data: GapStoreData,
    project_carriers: Vec<GapRepoCarrier>,
    repo_read: Option<Arc<dyn GapRepoRead>>,
    repo_write: Option<Arc<dyn GapRepoWrite>>,
    repo_owned_projects: BTreeSet<String>,
    repo_owned_carriers: BTreeSet<String>,
    /// Successfully loaded ids by concrete carrier. Generation purge may
    /// remove only these files, so malformed, unsafe, symlinked, or
    /// cross-project-shadowed records remain protected even if another scope
    /// happens to use the same logical id.
    repo_loaded_ids: BTreeMap<String, BTreeSet<String>>,
    /// Store-layer enforcement for the monotonic path-authority cut.
    path_fallback_cut: bool,
    view_metadata: BTreeMap<String, GapViewMetadata>,
}

impl GapStore {
    fn carrier_for_project(&self, project: &str) -> Option<&GapRepoCarrier> {
        self.project_carriers
            .iter()
            .find(|carrier| carrier.project == project)
    }

    fn carrier_for_write(&self, project: &str, carrier_id: &str) -> Result<GapRepoCarrier> {
        if let Some(base) = self
            .carrier_for_project(project)
            .filter(|base| base.carrier_id == carrier_id)
        {
            return Ok(base.clone());
        }
        GapRepoCarrier::new(project, carrier_id)
    }

    fn with_repo_read<T>(
        &self,
        carrier: &GapRepoCarrier,
        mut operation: impl FnMut(&Path) -> Result<T>,
    ) -> Result<T> {
        let authority = self.repo_read.as_ref().with_context(|| {
            format!(
                "gap repository read authority is unavailable for carrier {}",
                carrier.carrier_id
            )
        })?;
        let mut result = None;
        authority.with_read(carrier, &mut |root| {
            result = Some(operation(root)?);
            Ok(())
        })?;
        result.with_context(|| {
            format!(
                "gap repository read authority did not invoke operation for carrier {}",
                carrier.carrier_id
            )
        })
    }

    fn with_repo_write<T>(
        &self,
        carrier: &GapRepoCarrier,
        mut operation: impl FnMut(&Path) -> Result<T>,
    ) -> Result<T> {
        let authority = self.repo_write.as_ref().with_context(|| {
            format!(
                "gap repository write authority is unavailable for carrier {}",
                carrier.carrier_id
            )
        })?;
        let mut result = None;
        authority.with_write(carrier, &mut |root| {
            result = Some(operation(root)?);
            Ok(())
        })?;
        result.with_context(|| {
            format!(
                "gap repository write authority did not invoke operation for carrier {}",
                carrier.carrier_id
            )
        })
    }

    pub fn open(store_path: &Path) -> Result<Self> {
        let mut s = Self {
            store_path: store_path.to_path_buf(),
            data: GapStoreData::new(),
            project_carriers: Vec::new(),
            repo_read: None,
            repo_write: None,
            repo_owned_projects: BTreeSet::new(),
            repo_owned_carriers: BTreeSet::new(),
            repo_loaded_ids: BTreeMap::new(),
            path_fallback_cut: false,
            view_metadata: BTreeMap::new(),
        };
        s.reload()?;
        Ok(s)
    }

    /// Install repository I/O authorities and logical published carriers, then
    /// reload so their committed gaps are immediately visible.
    pub fn configure_repo_io(
        &mut self,
        read: Arc<dyn GapRepoRead>,
        write: Arc<dyn GapRepoWrite>,
        carriers: Vec<GapRepoCarrier>,
    ) -> Result<()> {
        self.repo_read = Some(read);
        self.repo_write = Some(write);
        self.project_carriers = carriers;
        self.reload()
    }

    #[cfg(test)]
    pub fn set_project_roots(&mut self, roots: Vec<PathBuf>) -> Result<()> {
        use crate::repo_io::test_support::TestGapRepoIo;

        let carriers = roots
            .into_iter()
            .map(|root| {
                let project = root.to_string_lossy().into_owned();
                let carrier = GapRepoCarrier::new(project.clone(), project)?;
                Ok((carrier, root))
            })
            .collect::<Result<Vec<_>>>()?;
        let io = Arc::new(TestGapRepoIo::default());
        io.replace(&carriers);
        self.repo_read = Some(io.clone());
        self.repo_write = Some(io);
        self.project_carriers = carriers.into_iter().map(|(carrier, _)| carrier).collect();
        self.reload()
    }

    pub fn set_path_fallback_cut(&mut self, cut: bool) {
        self.path_fallback_cut = cut;
    }

    pub fn reload(&mut self) -> Result<()> {
        self.view_metadata.clear();
        if self.store_path.exists() {
            let raw = fs::read_to_string(&self.store_path)
                .with_context(|| format!("reading {}", self.store_path.display()))?;
            self.data = serde_json::from_str(&raw)
                .with_context(|| format!("parsing {}", self.store_path.display()))?;
        } else {
            self.data = GapStoreData::new();
        }
        self.repo_owned_projects.clear();
        self.repo_owned_carriers.clear();
        self.repo_loaded_ids.clear();
        self.load_project_entries()?;
        Ok(())
    }

    fn load_project_entries(&mut self) -> Result<()> {
        let carriers = self.project_carriers.clone();
        for carrier in &carriers {
            let (repo_owned, entries) = self.with_repo_read(carrier, |root| {
                Ok((
                    project_is_repo_owned(root),
                    load_repo_gap_entries(root, &carrier.project)?,
                ))
            })?;
            if repo_owned {
                self.repo_owned_projects.insert(carrier.project.clone());
                self.repo_owned_carriers.insert(carrier.carrier_id.clone());
            }
            for entry in entries {
                let loaded_id = entry.id.clone();
                let mut accepted = false;
                if let Some(existing) = self.data.gaps.iter_mut().find(|g| g.id == entry.id) {
                    if existing.project.as_deref() == Some(carrier.project.as_str()) {
                        *existing = entry;
                        accepted = true;
                    } else {
                        tracing::warn!(
                            id = %entry.id,
                            project = %carrier.project,
                            existing_project = ?existing.project,
                            "gaps load: refusing cross-project gap id shadow"
                        );
                    }
                } else {
                    self.data.gaps.push(entry);
                    accepted = true;
                }
                if accepted {
                    self.repo_loaded_ids
                        .entry(carrier.carrier_id.clone())
                        .or_default()
                        .insert(loaded_id);
                }
            }
        }
        Ok(())
    }

    /// Seed checkout-local variants before a mutation. Reload reconstructs the
    /// published/base store first; this replaces those records with the
    /// session checkout's own files so successive updates never overwrite an
    /// earlier provisional edit with stale published bytes.
    fn seed_checkout_entries(
        &mut self,
        durable_project: Option<&str>,
        write_carrier_id: Option<&str>,
    ) -> Result<()> {
        let (Some(durable_project), Some(write_carrier_id)) = (durable_project, write_carrier_id)
        else {
            return Ok(());
        };
        if self
            .carrier_for_project(durable_project)
            .is_some_and(|base| base.carrier_id == write_carrier_id)
        {
            return Ok(());
        }
        let carrier = self.carrier_for_write(durable_project, write_carrier_id)?;
        let (repo_owned, entries) = self.with_repo_read(&carrier, |root| {
            Ok((
                project_is_repo_owned(root),
                load_repo_gap_entries(root, durable_project)?,
            ))
        })?;
        if repo_owned {
            self.repo_owned_carriers.insert(carrier.carrier_id.clone());
        }
        for mut gap in entries {
            gap.project = Some(durable_project.to_string());
            gap.write_dir = Some(write_carrier_id.to_string());
            gap.provisional_checkout_id = None;
            if let Some(existing) = self.data.gaps.iter_mut().find(|item| item.id == gap.id) {
                *existing = gap;
            } else {
                self.data.gaps.push(gap);
            }
        }
        Ok(())
    }

    /// Ensure the selected logical carrier has a repo-owned gap directory.
    /// Returns `None` when a legacy project has no configured carrier and must
    /// remain central-owned.
    fn ensure_repo_owned_carrier(
        &mut self,
        project: &str,
        write_carrier_id: Option<&str>,
    ) -> Result<Option<GapRepoCarrier>> {
        let carrier = match write_carrier_id {
            Some(carrier_id) => self.carrier_for_write(project, carrier_id)?,
            None => {
                let Some(carrier) = self.carrier_for_project(project).cloned() else {
                    return Ok(None);
                };
                carrier
            }
        };
        self.with_repo_write(&carrier, |root| {
            fs::create_dir_all(repo_gaps_dir(root)).with_context(|| {
                format!(
                    "creating repo-owned gaps for carrier {}",
                    carrier.carrier_id
                )
            })
        })?;
        self.repo_owned_carriers.insert(carrier.carrier_id.clone());
        if self
            .carrier_for_project(project)
            .is_some_and(|base| base.carrier_id == carrier.carrier_id)
        {
            self.repo_owned_projects.insert(project.to_string());
        }
        Ok(Some(carrier))
    }

    fn save(&self) -> Result<()> {
        // Central store owns only global (project-less or not-yet-repo-owned)
        // gaps; project-scoped gaps are written under their owning repo's
        // `.bbox/gaps/`. A project whose repo isn't present falls back to
        // central so the gap is never dropped.
        let mut central = GapStoreData {
            version: self.data.version,
            gaps: Vec::new(),
        };
        let mut by_project: BTreeMap<GapRepoCarrier, Vec<&GapNote>> = BTreeMap::new();
        // Per durable-project dir: ids whose rewrite is targeted into a
        // checkout (`write_dir != project`). Their committed base files are
        // protected from generation purge while the checkout carries the
        // provisional variant.
        let mut redirected: BTreeMap<GapRepoCarrier, BTreeSet<&str>> = BTreeMap::new();
        for g in &self.data.gaps {
            match g.project.as_deref() {
                Some(project) if !project.is_empty() => {
                    let base = self.carrier_for_project(project).cloned();
                    match g.write_dir.as_deref().filter(|id| !id.is_empty()) {
                        // The checkout file is the only provisional carrier.
                        // Registry discovery reconstructs its overlay after a
                        // restart, so the central store never retains a copy.
                        Some(write_carrier_id)
                            if self.repo_owned_carriers.contains(write_carrier_id) =>
                        {
                            let carrier = self.carrier_for_write(project, write_carrier_id)?;
                            by_project.entry(carrier).or_default().push(g);
                            if let Some(base) = base
                                && base.carrier_id != write_carrier_id
                            {
                                redirected.entry(base).or_default().insert(g.id.as_str());
                            }
                        }
                        Some(write_carrier_id) => tracing::warn!(
                            gap = %g.id,
                            target = write_carrier_id,
                            "dropping legacy central gap redirect whose checkout is gone"
                        ),
                        None if self.repo_owned_projects.contains(project) => {
                            if let Some(base) = base {
                                by_project.entry(base).or_default().push(g);
                            } else {
                                central.gaps.push(g.clone());
                            }
                        }
                        None => central.gaps.push(g.clone()),
                    }
                }
                _ => central.gaps.push(g.clone()),
            }
        }
        for gap in &mut central.gaps {
            gap.write_dir = None;
            gap.provisional_checkout_id = None;
        }
        bbox_corpus_core::json_store::atomic_write_json_locked(&self.store_path, &central)?;
        let loaded = self
            .project_carriers
            .iter()
            .map(|carrier| carrier.carrier_id.as_str())
            .collect::<BTreeSet<_>>();
        let no_redirects = BTreeSet::new();
        let no_loaded_ids = BTreeSet::new();
        for (carrier, entries) in &by_project {
            let purge = loaded.contains(carrier.carrier_id.as_str());
            let redirected_ids = redirected.get(carrier).unwrap_or(&no_redirects);
            let known_ids = self
                .repo_loaded_ids
                .get(&carrier.carrier_id)
                .unwrap_or(&no_loaded_ids)
                .iter()
                .map(String::as_str)
                .collect::<BTreeSet<_>>();
            self.with_repo_write(carrier, |root| {
                persist_repo_gap_entries(root, entries, purge, &known_ids, redirected_ids)
            })?;
        }
        Ok(())
    }

    fn is_checkout_redirect(&self, project: Option<&str>, write_carrier_id: Option<&str>) -> bool {
        let (Some(project), Some(write_carrier_id)) = (project, write_carrier_id) else {
            return false;
        };
        self.carrier_for_project(project)
            .is_none_or(|base| base.carrier_id != write_carrier_id)
    }

    fn now_iso() -> String {
        bbox_util::util::now_iso()
    }

    fn gen_id() -> String {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        use std::time::SystemTime;
        let mut h = DefaultHasher::new();
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .hash(&mut h);
        std::process::id().hash(&mut h);
        format!("gap-{:08x}", h.finish() as u32)
    }

    /// Re-key central rows on project rename (phase-2 §8.4): the same
    /// coverage knowledge/threads/notes/pins already had; repo-owned gap
    /// files travel with their checkout and are not touched here.
    pub fn rename_project_refs(
        &mut self,
        old_project: &str,
        new_project: &str,
    ) -> anyhow::Result<usize> {
        let mut updated = 0usize;
        for gap in &mut self.data.gaps {
            if gap.project.as_deref() == Some(old_project) {
                gap.project = Some(new_project.to_string());
                updated += 1;
            }
        }
        Ok(updated)
    }

    /// Immutable slice of all stored gaps — used by cross-store aggregators
    /// (inbox) that can't go through the MCP layer.
    pub fn all(&self) -> &[GapNote] {
        &self.data.gaps
    }

    /// Read-only store assembled by the daemon from pinned published trees and
    /// selected checkout overlays.
    pub fn detached_view(
        gaps: Vec<GapNote>,
        view_metadata: BTreeMap<String, GapViewMetadata>,
    ) -> Self {
        Self {
            store_path: PathBuf::new(),
            data: GapStoreData { version: 1, gaps },
            project_carriers: Vec::new(),
            repo_read: None,
            repo_write: None,
            repo_owned_projects: BTreeSet::new(),
            repo_owned_carriers: BTreeSet::new(),
            repo_loaded_ids: BTreeMap::new(),
            path_fallback_cut: true,
            view_metadata,
        }
    }

    pub fn view_metadata(&self, id: &str) -> Option<&GapViewMetadata> {
        self.view_metadata.get(id)
    }

    /// Project records still relying on the host-local central path key.
    /// The schema-epoch cut cannot retire that fallback while any remain.
    pub fn legacy_path_scoped_entry_count(&self) -> Result<usize> {
        let mut ids = self
            .data
            .gaps
            .iter()
            .filter(|gap| {
                gap.project
                    .as_deref()
                    .is_some_and(|project| !self.repo_owned_projects.contains(project))
            })
            .map(|gap| gap.id.clone())
            .collect::<BTreeSet<_>>();
        if self.store_path.is_file() {
            let raw: GapStoreData = serde_json::from_slice(&fs::read(&self.store_path)?)?;
            ids.extend(
                raw.gaps
                    .into_iter()
                    .filter(|gap| gap.project.is_some())
                    .map(|gap| gap.id),
            );
        }
        Ok(ids.len())
    }

    /// Open (non-addressed) gap with a matching `dedupe_key` in the same scope.
    fn open_duplicate(&self, dedupe_key: &str, project: Option<&str>) -> Option<&GapNote> {
        self.data.gaps.iter().find(|g| {
            g.resolution != GapResolution::Addressed
                && g.dedupe_key == dedupe_key
                && g.project.as_deref() == project
        })
    }

    // ── bbox_gap (file) ────────────────────────────────────────────

    /// File a gap. The `project` field on `p` is the already-resolved owning
    /// path (or None for global). Returns the gap id (existing id on a dedupe
    /// hit). The boolean is true when a NEW gap was created.
    pub fn file(&mut self, p: &GapFileParams) -> Result<(String, bool)> {
        let path = self.store_path.clone();
        bbox_corpus_core::json_store::with_store_lock(&path, || {
            self.reload()?;
            let result = self.file_locked(p);
            let reloaded = self
                .is_checkout_redirect(p.project.as_deref(), p.write_dir.as_deref())
                .then(|| self.reload());
            match result {
                Ok(value) => {
                    if let Some(reloaded) = reloaded {
                        reloaded?;
                    }
                    Ok(value)
                }
                Err(err) => {
                    if reloaded.is_none() {
                        let _ = self.reload();
                    }
                    Err(err)
                }
            }
        })
    }

    fn file_locked(&mut self, p: &GapFileParams) -> Result<(String, bool)> {
        if p.title.trim().is_empty() {
            anyhow::bail!("'title' is required and cannot be empty");
        }
        let gap_kind = parse_gap_kind(&p.gap_kind)?;
        if p.domain.trim().is_empty() {
            anyhow::bail!("'domain' is required and cannot be empty");
        }
        if p.wanted_capability.trim().is_empty() {
            anyhow::bail!("'wanted_capability' is required and cannot be empty");
        }
        validate_dedupe_key(&p.dedupe_key)?;
        let impact = parse_impact(p.impact.as_deref())?;
        let blocking_level = parse_blocking_level(p.blocking_level.as_deref())?;

        // `scope=global` wins over any supplied/defaulted `project`: both the
        // durable project AND the worktree write target are dropped, so an
        // ambient-defaulted `project` can never project-scope (or mkdir
        // `.bbox/gaps/` for) a global filing.
        let scope = p.scope.as_deref().unwrap_or("project");
        let (project, write_dir) = match scope {
            "global" => (None, None),
            "project" => (
                p.project.clone().filter(|s| !s.trim().is_empty()),
                p.write_dir.clone(),
            ),
            other => anyhow::bail!("scope must be `project` or `global`, got `{other}`"),
        };
        if self.path_fallback_cut
            && scope == "project"
            && (project.is_none() || write_dir.is_none())
        {
            anyhow::bail!(
                "path-scoped project fallback is retired; project gap writes require checkout authority"
            );
        }
        self.seed_checkout_entries(project.as_deref(), write_dir.as_deref())?;

        let allow_recurrence = p.allow_recurrence.unwrap_or(false);
        if !allow_recurrence {
            if let Some(existing) = self.open_duplicate(&p.dedupe_key, project.as_deref()) {
                return Ok((existing.id.clone(), false));
            }
        }

        if let Some(project) = project.as_deref() {
            self.ensure_repo_owned_carrier(project, write_dir.as_deref())?;
        }

        let now = Self::now_iso();
        let gap = GapNote {
            id: Self::gen_id(),
            title: p.title.trim().to_string(),
            gap_kind,
            domain: p.domain.trim().to_string(),
            wanted_capability: p.wanted_capability.trim().to_string(),
            missing_primitive: p.missing_primitive.clone().filter(|s| !s.trim().is_empty()),
            fallback_used: p.fallback_used.clone().filter(|s| !s.trim().is_empty()),
            evidence: p.evidence.clone().unwrap_or_default(),
            impact,
            blocking_level,
            dedupe_key: p.dedupe_key.trim().to_string(),
            suggested_owner: p.suggested_owner.clone().filter(|s| !s.trim().is_empty()),
            notes: p.notes.clone().filter(|s| !s.trim().is_empty()),
            supersedes: None,
            superseded_by: None,
            resolution: GapResolution::Unresolved,
            project,
            project_id: p.project_id.clone(),
            write_dir,
            provisional_checkout_id: None,
            task_id: p.task_id.clone(),
            session_id: p.session_id.clone(),
            provider: p.provider.clone(),
            bro: p.bro.clone(),
            thread_id: p.thread_id.clone(),
            created_at: now.clone(),
            updated_at: now,
            resolved_at: None,
            resolution_note: None,
        };
        let id = gap.id.clone();
        self.data.gaps.push(gap);
        self.save()?;
        Ok((id, true))
    }

    /// Ingest a pre-built [`GapNote`] (spool / packet producers). Honors the
    /// same open-duplicate dedupe by key+scope. Returns (id, created).
    pub fn ingest(&mut self, gap: GapNote) -> Result<(String, bool)> {
        self.ingest_with_carrier(gap, None)
    }

    /// Ingest through an explicitly selected repository carrier. Checkout
    /// spool imports use this path so reading from a worktree can never write
    /// the resulting gap into the selected base checkout.
    pub fn ingest_with_carrier(
        &mut self,
        mut gap: GapNote,
        write_carrier_id: Option<&str>,
    ) -> Result<(String, bool)> {
        let path = self.store_path.clone();
        bbox_corpus_core::json_store::with_store_lock(&path, || {
            self.reload()?;
            if self.path_fallback_cut && gap.project.is_some() && write_carrier_id.is_none() {
                anyhow::bail!(
                    "path-scoped project fallback is retired; project gap ingestion requires checkout authority"
                );
            }
            if let Some(project) = gap.project.clone() {
                self.seed_checkout_entries(Some(&project), write_carrier_id)?;
                self.ensure_repo_owned_carrier(&project, write_carrier_id)?;
                gap.write_dir = write_carrier_id.map(str::to_owned);
            }
            if let Some(existing) = self.open_duplicate(&gap.dedupe_key, gap.project.as_deref()) {
                return Ok((existing.id.clone(), false));
            }
            if gap.id.is_empty() {
                gap.id = Self::gen_id();
            }
            let id = gap.id.clone();
            self.data.gaps.push(gap);
            self.save()?;
            Ok((id, true))
        })
    }

    /// Ingest a spool entry while the caller holds mutation authority for the
    /// exact carrier and `root`. This targeted path never reloads or saves a
    /// different carrier, so a checkout inbox cannot mutate the selected base
    /// repository and nested authority acquisition is unnecessary.
    pub fn ingest_authorized_carrier(
        &mut self,
        mut gap: GapNote,
        carrier: &GapRepoCarrier,
        root: &Path,
    ) -> Result<(String, bool)> {
        let project = gap
            .project
            .as_deref()
            .context("authorized repository gap ingestion requires a project")?;
        if project != carrier.project {
            anyhow::bail!("authorized gap carrier does not match the gap project");
        }
        for mut existing in load_repo_gap_entries(root, project)? {
            existing.project = Some(project.to_string());
            existing.write_dir = Some(carrier.carrier_id.clone());
            if let Some(current) = self.data.gaps.iter_mut().find(|gap| gap.id == existing.id) {
                *current = existing;
            } else {
                self.data.gaps.push(existing);
            }
        }
        if let Some(existing) = self.open_duplicate(&gap.dedupe_key, Some(project)) {
            return Ok((existing.id.clone(), false));
        }
        if gap.id.is_empty() {
            gap.id = Self::gen_id();
        }
        gap.write_dir = Some(carrier.carrier_id.clone());
        let id = gap.id.clone();
        let known_ids = BTreeSet::from([id.as_str()]);
        persist_repo_gap_entries(root, &[&gap], false, &known_ids, &BTreeSet::new())?;
        self.repo_owned_carriers.insert(carrier.carrier_id.clone());
        self.data.gaps.push(gap);
        Ok((id, true))
    }

    // ── bbox_gap_resolve ───────────────────────────────────────────

    /// Apply adapter-resolved write-targeting to a gap a mutation is about to
    /// rewrite: when the session's `project` resolved to a recognized worktree
    /// (`write_dir`) of the gap's OWN project (`resolved_base`), the rewritten
    /// repo-owned file lands in that worktree instead of the base checkout.
    ///
    /// Write REDIRECTION only — mirror of the knowledge lane: the durable
    /// `project` field never changes here and the daemon never commits. The
    /// session commits the rewritten file, the branch carries it, and
    /// discarding the branch discards the resolution (correct: resolutions
    /// cite branch work). Until the branch merges, the base checkout keeps
    /// its committed copy untouched and the daemon's loaded view reflects it.
    ///
    /// No-op for global gaps (`project=None`), gaps owned by a different
    /// project than the resolved base, or when no write target was resolved.
    fn prepare_write_target(
        &mut self,
        owner: Option<&str>,
        resolved_base: Option<&str>,
        write_carrier_id: Option<&str>,
    ) -> Result<Option<String>> {
        let (Some(owner), Some(base), Some(write_carrier_id)) =
            (owner, resolved_base, write_carrier_id)
        else {
            return Ok(None);
        };
        if owner != base {
            return Ok(None);
        }
        self.ensure_repo_owned_carrier(owner, Some(write_carrier_id))?;
        Ok(Some(write_carrier_id.to_string()))
    }

    fn validate_mutation_authority(
        gap: &GapNote,
        project: Option<&str>,
        write_dir: Option<&str>,
        path_fallback_cut: bool,
    ) -> Result<()> {
        let Some(owner) = gap.project.as_deref() else {
            return Ok(());
        };
        if let Some(project) = project
            && !Self::project_authority_matches_owner(project, owner)
        {
            anyhow::bail!(
                "gap {} belongs to project {owner}; supplied checkout authority is for {project}",
                gap.id
            );
        }
        if path_fallback_cut && (project != Some(owner) || write_dir.is_none()) {
            anyhow::bail!(
                "path-scoped project fallback is retired; project gap mutation requires matching checkout authority"
            );
        }
        Ok(())
    }

    fn project_authority_matches_owner(project: &str, owner: &str) -> bool {
        project == owner
    }

    pub fn resolve(&mut self, p: &GapResolveParams) -> Result<String> {
        let path = self.store_path.clone();
        bbox_corpus_core::json_store::with_store_lock(&path, || {
            self.reload()?;
            self.seed_checkout_entries(p.project.as_deref(), p.write_dir.as_deref())?;
            let result = self.resolve_locked(p);
            let reloaded = self
                .is_checkout_redirect(p.project.as_deref(), p.write_dir.as_deref())
                .then(|| self.reload());
            match result {
                Ok(value) => {
                    if let Some(reloaded) = reloaded {
                        reloaded?;
                    }
                    Ok(value)
                }
                Err(err) => {
                    if reloaded.is_none() {
                        let _ = self.reload();
                    }
                    Err(err)
                }
            }
        })
    }

    fn resolve_locked(&mut self, p: &GapResolveParams) -> Result<String> {
        let resolution = GapResolution::from_str(p.resolution.trim()).map_err(|_| {
            anyhow::anyhow!(
                "Unknown resolution: {}. Use: unresolved, acknowledged, addressed",
                p.resolution
            )
        })?;

        // Canonical superseded_by id, if any.
        let superseded_by = p.superseded_by.as_deref().map(|s| {
            let s = s.trim();
            if s.starts_with("gap-") {
                s.to_string()
            } else {
                format!("gap-{s}")
            }
        });
        // Validate the supersessor exists and is covered by the same authority
        // before mutating either side.
        if let Some(by) = superseded_by.as_deref() {
            let other = self
                .data
                .gaps
                .iter()
                .find(|g| g.matches_id(by))
                .with_context(|| format!("superseded_by gap not found: {by}"))?;
            Self::validate_mutation_authority(
                other,
                p.project.as_deref(),
                p.write_dir.as_deref(),
                self.path_fallback_cut,
            )?;
        }

        let now = Self::now_iso();
        let idx = self
            .data
            .gaps
            .iter()
            .position(|g| g.matches_id(&p.id))
            .with_context(|| {
                format!(
                    "Gap not found: {} (expected `gap-<8hex>`, e.g. `gap-a1b2c3d4`)",
                    p.id
                )
            })?;

        Self::validate_mutation_authority(
            &self.data.gaps[idx],
            p.project.as_deref(),
            p.write_dir.as_deref(),
            self.path_fallback_cut,
        )?;

        let resolved_id = self.data.gaps[idx].id.clone();
        let resolved_owner = self.data.gaps[idx].project.clone();
        let resolved_target = self.prepare_write_target(
            resolved_owner.as_deref(),
            p.project.as_deref(),
            p.write_dir.as_deref(),
        )?;
        let supersessor_target = if let Some(by) = superseded_by.as_deref() {
            let owner = self
                .data
                .gaps
                .iter()
                .find(|gap| gap.matches_id(by))
                .and_then(|gap| gap.project.clone());
            self.prepare_write_target(
                owner.as_deref(),
                p.project.as_deref(),
                p.write_dir.as_deref(),
            )?
        } else {
            None
        };
        {
            let gap = &mut self.data.gaps[idx];
            gap.resolution = resolution;
            gap.updated_at = now.clone();
            gap.resolved_at = if matches!(resolution, GapResolution::Unresolved) {
                None
            } else {
                Some(now.clone())
            };
            if let Some(txt) = p.note.as_deref() {
                gap.resolution_note = Some(txt.to_string());
            }
            if let Some(by) = &superseded_by {
                gap.superseded_by = Some(by.clone());
            }
            if resolved_target.is_some() {
                gap.write_dir = resolved_target;
            }
        }
        // Wire the reverse link on the supersessor. Its file is rewritten by
        // this mutation too, so it honors the same session write target.
        if let Some(by) = &superseded_by {
            if let Some(other) = self.data.gaps.iter_mut().find(|g| g.matches_id(by)) {
                other.supersedes = Some(resolved_id.clone());
                other.updated_at = now;
                if supersessor_target.is_some() {
                    other.write_dir = supersessor_target;
                }
            }
        }

        self.save()?;
        Ok(format!("Gap {resolved_id} → {}", resolution.as_ref()))
    }

    // ── bbox_gap_update ────────────────────────────────────────────

    pub fn update(&mut self, p: &GapUpdateParams) -> Result<String> {
        let path = self.store_path.clone();
        bbox_corpus_core::json_store::with_store_lock(&path, || {
            self.reload()?;
            self.seed_checkout_entries(p.project.as_deref(), p.write_dir.as_deref())?;
            let result = self.update_locked(p);
            let reloaded = self
                .is_checkout_redirect(p.project.as_deref(), p.write_dir.as_deref())
                .then(|| self.reload());
            match result {
                Ok(value) => {
                    if let Some(reloaded) = reloaded {
                        reloaded?;
                    }
                    Ok(value)
                }
                Err(err) => {
                    if reloaded.is_none() {
                        let _ = self.reload();
                    }
                    Err(err)
                }
            }
        })
    }

    fn update_locked(&mut self, p: &GapUpdateParams) -> Result<String> {
        // Validate enum patches before locating the record.
        let impact = p
            .impact
            .as_deref()
            .map(|v| parse_impact(Some(v)))
            .transpose()?;
        let blocking_level = p
            .blocking_level
            .as_deref()
            .map(|v| parse_blocking_level(Some(v)))
            .transpose()?;

        let gap_index = self
            .data
            .gaps
            .iter()
            .position(|g| g.matches_id(&p.id))
            .with_context(|| {
                format!(
                    "Gap not found: {} (expected `gap-<8hex>`, e.g. `gap-a1b2c3d4`)",
                    p.id
                )
            })?;
        Self::validate_mutation_authority(
            &self.data.gaps[gap_index],
            p.project.as_deref(),
            p.write_dir.as_deref(),
            self.path_fallback_cut,
        )?;
        let owner = self.data.gaps[gap_index].project.clone();
        let write_target = self.prepare_write_target(
            owner.as_deref(),
            p.project.as_deref(),
            p.write_dir.as_deref(),
        )?;
        let gap = self
            .data
            .gaps
            .get_mut(gap_index)
            .expect("gap index was resolved above");

        if let Some(v) = p.title.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            gap.title = v.to_string();
        }
        if let Some(v) = p.domain.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            gap.domain = v.to_string();
        }
        if let Some(v) = p
            .wanted_capability
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            gap.wanted_capability = v.to_string();
        }
        if let Some(v) = impact {
            gap.impact = v;
        }
        if let Some(v) = blocking_level {
            gap.blocking_level = v;
        }
        if let Some(v) = &p.missing_primitive {
            gap.missing_primitive = Some(v.clone()).filter(|s| !s.trim().is_empty());
        }
        if let Some(v) = &p.fallback_used {
            gap.fallback_used = Some(v.clone()).filter(|s| !s.trim().is_empty());
        }
        if let Some(v) = &p.evidence {
            gap.evidence = v.clone();
        }
        if let Some(v) = &p.suggested_owner {
            gap.suggested_owner = Some(v.clone()).filter(|s| !s.trim().is_empty());
        }
        if let Some(v) = &p.notes {
            gap.notes = Some(v.clone()).filter(|s| !s.trim().is_empty());
        }
        gap.updated_at = Self::now_iso();
        if write_target.is_some() {
            gap.write_dir = write_target;
        }
        let id = gap.id.clone();
        self.save()?;
        Ok(format!("Gap {id} updated"))
    }

    // ── bbox_gaps (list / filter) ──────────────────────────────────

    /// Filtered, newest-first view. Returns owned clones so callers can render
    /// or serialize without holding the borrow.
    pub fn query(&self, p: &GapListParams) -> Vec<GapNote> {
        let gap_kind = p
            .gap_kind
            .as_deref()
            .and_then(|v| GapKind::from_str(v.trim()).ok());
        let impact = p
            .impact
            .as_deref()
            .and_then(|v| GapImpact::from_str(v.trim()).ok());
        let blocking = p
            .blocking_level
            .as_deref()
            .and_then(|v| BlockingLevel::from_str(v.trim()).ok());
        let resolution = p
            .resolution
            .as_deref()
            .and_then(|v| GapResolution::from_str(v.trim()).ok());
        let include_addressed = p.include_addressed.unwrap_or(p.id.is_some());
        let id_needle = p.id.as_deref().map(|s| {
            s.trim()
                .strip_prefix("gap-")
                .unwrap_or(s.trim())
                .to_ascii_lowercase()
        });
        let query_lower = p.query.as_deref().map(|s| s.to_lowercase());
        let project_lower = p.project.as_deref().map(|s| s.to_lowercase());
        let project_id_filter = p.project_id.as_deref();
        let ledger_lower: Vec<String> = p
            .project_ledger_paths
            .iter()
            .map(|path| path.to_lowercase())
            .collect();
        let dedupe_lower = p.dedupe_key.as_deref().map(|s| s.to_lowercase());

        let mut out: Vec<GapNote> = self
            .data
            .gaps
            .iter()
            .filter(|g| {
                if let Some(needle) = &id_needle {
                    if g.id
                        .strip_prefix("gap-")
                        .unwrap_or(&g.id)
                        .to_ascii_lowercase()
                        != *needle
                    {
                        return false;
                    }
                }
                if let Some(k) = gap_kind {
                    if g.gap_kind != k {
                        return false;
                    }
                }
                if let Some(i) = impact {
                    if g.impact != i {
                        return false;
                    }
                }
                if let Some(b) = blocking {
                    if g.blocking_level != b {
                        return false;
                    }
                }
                if let Some(r) = resolution {
                    if g.resolution != r {
                        return false;
                    }
                } else if !include_addressed && g.resolution == GapResolution::Addressed {
                    return false;
                }
                if let Some(d) = &dedupe_lower {
                    if !g.dedupe_key.to_lowercase().contains(d) {
                        return false;
                    }
                }
                if let Some(dom) = &p.domain {
                    if !g.domain.eq_ignore_ascii_case(dom.trim()) {
                        return false;
                    }
                }
                // Dual-read (plan §8.2): ids on both sides decide, whatever the
                // paths say; either side missing an id keeps the path predicate.
                // The ledger arm is catalog-mode only and matches a path-only
                // row still keyed under a historical path of this project.
                if let Some(pl) = &project_lower
                    && !project_scope_matches(g.project_id.as_deref(), project_id_filter, || {
                        let row_project = g.project.as_deref().unwrap_or("").to_lowercase();
                        row_project.contains(pl)
                            || ledger_lower
                                .iter()
                                .any(|historical| row_project.contains(historical.as_str()))
                    })
                {
                    return false;
                }
                if let Some(q) = &query_lower {
                    let hay = format!(
                        "{} {} {}",
                        g.title.to_lowercase(),
                        g.domain.to_lowercase(),
                        g.wanted_capability.to_lowercase()
                    );
                    if !hay.contains(q) {
                        return false;
                    }
                }
                if let Some(since) = p.since.as_deref() {
                    if g.created_at.as_str() < since {
                        return false;
                    }
                }
                true
            })
            .cloned()
            .collect();

        out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        let limit = p.limit.unwrap_or(50).max(1) as usize;
        out.truncate(limit);
        out
    }

    pub fn list_rendered(&self, p: &GapListParams) -> Result<String> {
        // Surface enum-filter typos loudly rather than silently matching nothing.
        if let Some(v) = p.gap_kind.as_deref() {
            parse_gap_kind(v)?;
        }
        if let Some(v) = p.impact.as_deref() {
            parse_impact(Some(v))?;
        }
        if let Some(v) = p.blocking_level.as_deref() {
            parse_blocking_level(Some(v))?;
        }
        if let Some(v) = p.resolution.as_deref() {
            GapResolution::from_str(v.trim())
                .map_err(|_| anyhow::anyhow!("Unknown resolution filter: {v}"))?;
        }

        let results = self.query(p);
        if p.json.unwrap_or(false) {
            let rows = results
                .into_iter()
                .map(|gap| {
                    let metadata = self.view_metadata(&gap.id);
                    GapResponseRow {
                        gap,
                        built_from_ref: metadata.and_then(|row| row.built_from_ref.clone()),
                        compatibility_lane: metadata.and_then(|row| row.compatibility_lane.clone()),
                    }
                })
                .collect::<Vec<_>>();
            return Ok(serde_json::to_string_pretty(&rows)?);
        }
        if results.is_empty() {
            return Ok("No gaps found.".to_string());
        }
        let mut out = format!("{} gap(s)\n\n", results.len());
        for g in &results {
            let scope = g.project.as_deref().map_or("global".to_string(), |p| {
                p.rsplit('/').next().unwrap_or(p).to_string()
            });
            let provisional = g
                .provisional_checkout_id
                .as_deref()
                .map(|checkout| format!("  checkout={checkout}"))
                .unwrap_or_default();
            let built_from = self
                .view_metadata(&g.id)
                .map(|metadata| {
                    match (
                        metadata.built_from_ref.as_deref(),
                        metadata.compatibility_lane.as_deref(),
                    ) {
                        (Some(reference), _) => format!("  built_from={reference}"),
                        (None, Some(lane)) => format!("  built_from={lane}"),
                        (None, None) => String::new(),
                    }
                })
                .unwrap_or_default();
            out.push_str(&format!(
                "{id}  [{kind}/{impact}/{res}]  {ts}  scope={scope}{provisional}{built_from}  dedupe={dedupe}\n  {title}\n  want: {want}\n",
                id = g.id,
                kind = g.gap_kind.as_ref(),
                impact = g.impact.as_ref(),
                res = g.resolution.as_ref(),
                ts = g.created_at,
                dedupe = g.dedupe_key,
                title = g.title,
                want = g.wanted_capability,
            ));
            if let Some(by) = &g.superseded_by {
                out.push_str(&format!("  superseded_by: {by}\n"));
            }
            if let Some(rn) = &g.resolution_note {
                out.push_str(&format!("  ↳ {rn}\n"));
            }
            out.push('\n');
        }
        Ok(out)
    }
}

#[derive(Serialize)]
struct GapResponseRow {
    #[serde(flatten)]
    gap: GapNote,
    #[serde(skip_serializing_if = "Option::is_none")]
    built_from_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    compatibility_lane: Option<String>,
}

/// Emit the companion gap note for a packet-authoring gap into the
/// first-class gap store. Global-scoped (substrate gaps aren't repo-owned);
/// `GapStore::ingest` handles open-duplicate dedupe by `dedupe_key`.
/// Lives here (not in bbox-packets) so the leaf packets crate stays free of
/// gap-store coupling; the body composition stays in `Packets`.
pub fn emit_companion_packet_gap_note(
    gaps_lock: &parking_lot::RwLock<GapStore>,
    ev: &bbox_packets::PacketEvent,
    params: &bbox_packets::GapParams,
) -> Option<String> {
    use bbox_packets::Packets;

    let dedupe_key = Packets::gap_dedupe_key(
        ev.domain.as_deref(),
        params.ast_feature_requested.as_deref(),
        &params.description,
    );

    let body = Packets::build_gap_note_body(ev, params, &dedupe_key);
    let value: serde_json::Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => return Some(format!("companion gap note build failed: {e:#}")),
    };
    let gap = match GapNote::from_envelope(&value, String::new(), bbox_util::util::now_iso()) {
        Ok(g) => g,
        Err(e) => return Some(format!("companion gap note build failed: {e:#}")),
    };

    match gaps_lock.write().ingest(gap) {
        Ok(_) => None,
        Err(e) => Some(format!("companion gap note failed: {e:#}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::tempdir;

    struct CountingGapRepoIo {
        root: PathBuf,
        reads: AtomicUsize,
        writes: AtomicUsize,
        deny_writes: bool,
    }

    impl GapRepoRead for CountingGapRepoIo {
        fn with_read(
            &self,
            _carrier: &GapRepoCarrier,
            operation: &mut dyn FnMut(&Path) -> Result<()>,
        ) -> Result<()> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            operation(&self.root)
        }
    }

    impl GapRepoWrite for CountingGapRepoIo {
        fn with_write(
            &self,
            _carrier: &GapRepoCarrier,
            operation: &mut dyn FnMut(&Path) -> Result<()>,
        ) -> Result<()> {
            self.writes.fetch_add(1, Ordering::SeqCst);
            if self.deny_writes {
                anyhow::bail!("test gap write authority denied");
            }
            operation(&self.root)
        }
    }

    fn file_params(title: &str, dedupe: &str) -> GapFileParams {
        GapFileParams {
            title: title.into(),
            gap_kind: "tooling".into(),
            domain: "test-domain".into(),
            wanted_capability: "do the thing".into(),
            dedupe_key: dedupe.into(),
            impact: Some("high".into()),
            blocking_level: None,
            missing_primitive: None,
            fallback_used: None,
            evidence: None,
            suggested_owner: None,
            notes: None,
            scope: Some("global".into()),
            project: None,
            project_id: None,
            write_dir: None,
            task_id: None,
            session_id: None,
            provider: None,
            bro: None,
            thread_id: None,
            allow_recurrence: None,
        }
    }

    #[test]
    fn detached_gap_view_exposes_response_ref_in_text_and_json() {
        let dir = tempdir().unwrap();
        let mut durable = GapStore::open(&dir.path().join("gaps.json")).unwrap();
        let (id, _) = durable
            .file(&file_params(
                "Stamped gap",
                "tooling/test-domain/stamped-gap",
            ))
            .unwrap();
        let gap = durable.all()[0].clone();
        let view = GapStore::detached_view(
            vec![gap],
            BTreeMap::from([(
                id,
                GapViewMetadata {
                    built_from_ref: Some("built_from_0".into()),
                    compatibility_lane: None,
                },
            )]),
        );

        let text = view.list_rendered(&GapListParams::default()).unwrap();
        assert!(text.contains("built_from=built_from_0"), "{text}");

        let json = view
            .list_rendered(&GapListParams {
                json: Some(true),
                ..Default::default()
            })
            .unwrap();
        let rows: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(rows[0]["built_from_ref"], "built_from_0");
    }

    #[test]
    fn file_and_query_roundtrip() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let mut store = GapStore::open(&root.join("gaps.json")).unwrap();
        let (id, created) = store
            .file(&file_params("Need a latch", "tooling/test-domain/latch"))
            .unwrap();
        assert!(created);
        assert!(id.starts_with("gap-"));

        let listed = store.query(&GapListParams::default());
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].impact, GapImpact::High);
        assert_eq!(listed[0].gap_kind, GapKind::Tooling);
    }

    #[test]
    fn legacy_path_count_reads_the_persisted_central_store() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let store_path = root.join("gaps.json");
        let project = root.join("legacy-project");
        fs::create_dir_all(&project).unwrap();
        let mut store = GapStore::open(&store_path).unwrap();
        store
            .file(&file_params("legacy", "tooling/test-domain/legacy"))
            .unwrap();
        store.data.gaps[0].project = Some(project.to_string_lossy().into_owned());
        store.save().unwrap();
        assert_eq!(store.legacy_path_scoped_entry_count().unwrap(), 1);

        store.set_project_roots(vec![project.clone()]).unwrap();
        fs::create_dir_all(project.join(".bbox/gaps")).unwrap();
        store.reload().unwrap();
        store.save().unwrap();
        assert_eq!(store.legacy_path_scoped_entry_count().unwrap(), 0);
    }

    #[test]
    fn path_cut_rejects_low_level_project_write_before_mutating() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let project = root.join("project");
        fs::create_dir_all(&project).unwrap();
        let mut store = GapStore::open(&root.join("gaps.json")).unwrap();
        store.set_path_fallback_cut(true);
        let mut params = file_params("blocked", "tooling/test-domain/path-cut");
        params.scope = Some("project".into());
        params.project = Some(project.to_string_lossy().into_owned());
        let err = store.file(&params).unwrap_err();
        assert!(err.to_string().contains("checkout authority"));
        assert!(store.all().is_empty());
    }

    #[test]
    fn project_gap_creation_requires_repository_write_authority() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let project = "project:test".to_string();
        let carrier = GapRepoCarrier::new(&project, "checkout:test").unwrap();
        let io = Arc::new(CountingGapRepoIo {
            root: root.clone(),
            reads: AtomicUsize::new(0),
            writes: AtomicUsize::new(0),
            deny_writes: true,
        });
        let mut store = GapStore::open(&root.join("gaps.json")).unwrap();
        store
            .configure_repo_io(io.clone(), io.clone(), vec![carrier])
            .unwrap();
        let mut params = file_params("blocked", "tooling/test-domain/repo-write");
        params.scope = Some("project".into());
        params.project = Some(project);

        let error = store.file(&params).unwrap_err();

        assert!(error.to_string().contains("write authority denied"));
        assert_eq!(io.writes.load(Ordering::SeqCst), 1);
        assert!(io.reads.load(Ordering::SeqCst) >= 1);
        assert!(store.all().is_empty());
    }

    #[test]
    fn dedupes_open_gap_by_key() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let mut store = GapStore::open(&root.join("gaps.json")).unwrap();
        let (id1, c1) = store
            .file(&file_params("first", "tooling/test-domain/x"))
            .unwrap();
        assert!(c1);
        let (id2, c2) = store
            .file(&file_params("second", "tooling/test-domain/x"))
            .unwrap();
        assert!(!c2, "same dedupe_key should not create a second open gap");
        assert_eq!(id1, id2);
        assert_eq!(store.all().len(), 1);
    }

    #[test]
    fn recurrence_allowed_with_flag() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let mut store = GapStore::open(&root.join("gaps.json")).unwrap();
        store
            .file(&file_params("first", "tooling/test-domain/x"))
            .unwrap();
        let mut p = file_params("again", "tooling/test-domain/x");
        p.allow_recurrence = Some(true);
        let (_id, created) = store.file(&p).unwrap();
        assert!(created);
        assert_eq!(store.all().len(), 2);
    }

    fn project_params(title: &str, dedupe: &str, root: &Path) -> GapFileParams {
        let mut p = file_params(title, dedupe);
        p.scope = Some("project".into());
        p.project = Some(root.to_string_lossy().to_string());
        p
    }

    /// Incident repro (gap-1f3894cc): a committed repo gap file written
    /// without `updated_at` (hand-authored / older producer / other machine)
    /// must load — backfilled from `created_at` — and must survive a
    /// project-scoped save instead of being purged as unknown.
    #[test]
    fn repo_gap_file_without_updated_at_loads_and_survives_save() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let gaps_dir = root.join(".bbox/gaps");
        fs::create_dir_all(&gaps_dir).unwrap();
        let foreign = gaps_dir.join("gap-deadbeef.json");
        fs::write(
            &foreign,
            r#"{
  "id": "gap-deadbeef",
  "title": "Peer-committed gap",
  "gap_kind": "tooling",
  "domain": "test-domain",
  "wanted_capability": "survive the persister",
  "impact": "medium",
  "blocking_level": "none",
  "dedupe_key": "tooling/test-domain/peer-committed",
  "resolution": "unresolved",
  "created_at": "2026-06-11T16:33:52Z"
}"#,
        )
        .unwrap();

        let central = tempdir().unwrap();
        let mut store = GapStore::open(&central.path().join("gaps.json")).unwrap();
        store.set_project_roots(vec![root.clone()]).unwrap();

        let loaded = store
            .all()
            .iter()
            .find(|g| g.id == "gap-deadbeef")
            .expect("repo gap file missing updated_at should load");
        assert_eq!(loaded.updated_at, "2026-06-11T16:33:52Z");

        store
            .file(&project_params(
                "fresh gap",
                "tooling/test-domain/fresh",
                &root,
            ))
            .unwrap();
        assert!(
            foreign.exists(),
            "project-scoped save must not purge the peer-committed gap file"
        );
        let fresh = fs::read_dir(&gaps_dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name().and_then(|name| name.to_str()) != Some("gap-deadbeef.json")
            })
            .expect("fresh project gap file should be written");
        let fresh_text = fs::read_to_string(fresh).unwrap();
        assert!(fresh_text.ends_with('\n'));
        assert!(!fresh_text.ends_with("\n\n"));
    }

    /// A repo gap file the store cannot parse must never be deleted by the
    /// save-side purge: skipped-at-load means the in-memory set is not
    /// authoritative for it.
    #[test]
    fn purge_keeps_files_unknown_to_store() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let gaps_dir = root.join(".bbox/gaps");
        fs::create_dir_all(&gaps_dir).unwrap();
        let broken = gaps_dir.join("gap-feedface.json");
        fs::write(&broken, r#"{"id": "gap-feedface"}"#).unwrap();

        let central = tempdir().unwrap();
        let mut store = GapStore::open(&central.path().join("gaps.json")).unwrap();
        store.set_project_roots(vec![root.clone()]).unwrap();
        assert!(
            !store.all().iter().any(|g| g.id == "gap-feedface"),
            "unparseable file should be skipped at load"
        );

        store
            .file(&project_params(
                "fresh",
                "tooling/test-domain/fresh2",
                &root,
            ))
            .unwrap();
        assert!(
            broken.exists(),
            "purge must keep files whose id the store does not hold"
        );
    }

    #[test]
    fn repo_gap_id_collision_cannot_shadow_or_delete_another_project() {
        let dir_a = tempdir().unwrap();
        let root_a = dir_a.path().canonicalize().unwrap();
        let dir_b = tempdir().unwrap();
        let root_b = dir_b.path().canonicalize().unwrap();
        fs::create_dir_all(repo_gaps_dir(&root_a)).unwrap();
        fs::create_dir_all(repo_gaps_dir(&root_b)).unwrap();

        let seed_dir = tempdir().unwrap();
        let mut seed = GapStore::open(&seed_dir.path().join("gaps.json")).unwrap();
        seed.file(&file_params(
            "seed",
            "tooling/test-domain/cross-project-collision",
        ))
        .unwrap();
        let mut first = seed.all()[0].clone();
        first.id = "gap-shared-id".into();
        first.title = "first project truth".into();
        let mut collision = first.clone();
        collision.title = "must not shadow".into();
        let mut stayer = first.clone();
        stayer.id = "gap-stayer".into();
        stayer.title = "keeps second-project purge active".into();

        let first_path = repo_gaps_dir(&root_a).join("gap-shared-id.json");
        let collision_path = repo_gaps_dir(&root_b).join("gap-shared-id.json");
        let stayer_path = repo_gaps_dir(&root_b).join("gap-stayer.json");
        fs::write(&first_path, committed_gap_note_bytes(&first).unwrap()).unwrap();
        fs::write(
            &collision_path,
            committed_gap_note_bytes(&collision).unwrap(),
        )
        .unwrap();
        fs::write(&stayer_path, committed_gap_note_bytes(&stayer).unwrap()).unwrap();

        let central = tempdir().unwrap();
        let mut store = GapStore::open(&central.path().join("gaps.json")).unwrap();
        store
            .set_project_roots(vec![root_a.clone(), root_b.clone()])
            .unwrap();

        let visible = store
            .all()
            .iter()
            .find(|gap| gap.id == "gap-shared-id")
            .unwrap();
        assert_eq!(visible.title, "first project truth");
        assert_eq!(visible.project.as_deref(), root_a.to_str());

        store
            .data
            .gaps
            .iter_mut()
            .find(|gap| gap.id == "gap-stayer")
            .unwrap()
            .notes = Some("trigger save".into());
        store.save().unwrap();

        assert!(first_path.exists());
        assert!(
            collision_path.exists(),
            "cross-project collision rejected at load must remain purge-protected"
        );
        assert!(stayer_path.exists());
    }

    #[test]
    fn repo_gap_loader_rejects_mismatched_and_traversal_ids() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::create_dir_all(repo_gaps_dir(&root)).unwrap();
        let central = tempdir().unwrap();
        let mut seed = GapStore::open(&central.path().join("gaps.json")).unwrap();
        seed.file(&file_params(
            "unsafe id",
            "tooling/test-domain/unsafe-gap-id",
        ))
        .unwrap();
        let mut unsafe_gap = seed.all()[0].clone();
        unsafe_gap.id = "../escape".into();
        unsafe_gap.project = None;
        fs::write(
            repo_gaps_dir(&root).join("safe-name.json"),
            serde_json::to_vec(&unsafe_gap).unwrap(),
        )
        .unwrap();

        let loaded = load_repo_gap_entries(&root, root.to_string_lossy().as_ref()).unwrap();
        assert!(loaded.is_empty(), "unsafe id must not enter the gap store");

        let known_ids = BTreeSet::from([unsafe_gap.id.as_str()]);
        let error =
            persist_repo_gap_entries(&root, &[&unsafe_gap], false, &known_ids, &BTreeSet::new())
                .unwrap_err();
        assert!(error.to_string().contains("confined basename"));
        assert!(!root.join(".bbox/escape.json").exists());
    }

    #[cfg(unix)]
    #[test]
    fn repo_gap_loader_rejects_symlinked_files() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let outside = tempdir().unwrap();
        let central = tempdir().unwrap();
        let mut seed = GapStore::open(&central.path().join("gaps.json")).unwrap();
        seed.file(&file_params("linked", "tooling/test-domain/symlink-gap"))
            .unwrap();
        let mut linked = seed.all()[0].clone();
        linked.id = "gap-linked".into();
        linked.project = None;
        let target = outside.path().join("gap-linked.json");
        fs::write(&target, serde_json::to_vec(&linked).unwrap()).unwrap();
        fs::create_dir_all(repo_gaps_dir(&root)).unwrap();
        symlink(&target, repo_gaps_dir(&root).join("gap-linked.json")).unwrap();

        let loaded = load_repo_gap_entries(&root, root.to_string_lossy().as_ref()).unwrap();
        assert!(loaded.is_empty(), "symlinked gap must not load");
    }

    #[cfg(unix)]
    #[test]
    fn repo_gap_loader_rejects_symlinked_directory() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let outside = tempdir().unwrap();
        let central = tempdir().unwrap();
        let mut seed = GapStore::open(&central.path().join("gaps.json")).unwrap();
        seed.file(&file_params(
            "linked",
            "tooling/test-domain/symlink-gap-directory",
        ))
        .unwrap();
        let mut linked = seed.all()[0].clone();
        linked.id = "gap-linked".into();
        linked.project = None;
        fs::write(
            outside.path().join("gap-linked.json"),
            serde_json::to_vec(&linked).unwrap(),
        )
        .unwrap();
        fs::create_dir_all(root.join(".bbox")).unwrap();
        symlink(outside.path(), repo_gaps_dir(&root)).unwrap();

        let loaded = load_repo_gap_entries(&root, root.to_string_lossy().as_ref()).unwrap();
        assert!(loaded.is_empty(), "symlinked gap directory must not load");
    }

    /// Generation purge still reaps a file whose gap the store has
    /// affirmatively reassigned to a different repo dir (write_dir
    /// migration / project move).
    #[test]
    fn purge_removes_reassigned_files() {
        let dir_a = tempdir().unwrap();
        let root_a = dir_a.path().canonicalize().unwrap();
        let dir_b = tempdir().unwrap();
        let root_b = dir_b.path().canonicalize().unwrap();
        fs::create_dir_all(root_b.join(".bbox/gaps")).unwrap();

        let central = tempdir().unwrap();
        let mut store = GapStore::open(&central.path().join("gaps.json")).unwrap();
        store
            .set_project_roots(vec![root_a.clone(), root_b.clone()])
            .unwrap();

        let (id, _) = store
            .file(&project_params(
                "mover",
                "tooling/test-domain/mover",
                &root_a,
            ))
            .unwrap();
        // A second gap keeps root_a in the save's write set: purge only runs
        // for dirs being written (a fully-vacated dir is left untouched).
        store
            .file(&project_params(
                "stayer",
                "tooling/test-domain/stayer",
                &root_a,
            ))
            .unwrap();
        let file_a = root_a.join(".bbox/gaps").join(format!("{id}.json"));
        assert!(file_a.exists());

        let entry = store.data.gaps.iter_mut().find(|g| g.id == id).unwrap();
        entry.project = Some(root_b.to_string_lossy().to_string());
        store.save().unwrap();

        assert!(
            !file_a.exists(),
            "reassigned gap's old file should be purged"
        );
        assert!(
            root_b
                .join(".bbox/gaps")
                .join(format!("{id}.json"))
                .exists()
        );
    }

    /// Write redirection (resolve/update from a worktree session) must leave
    /// the base checkout's committed copy untouched: redirection is not
    /// reassignment, so the generation purge skips the redirected id even
    /// while it reaps genuinely reassigned files (gap-b94129ba; complements
    /// the a9b00cf known_ids guard).
    #[test]
    fn resolve_write_redirection_writes_worktree_and_keeps_base_copy() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let wt_dir = tempdir().unwrap();
        let wt = wt_dir.path().canonicalize().unwrap();

        let central = tempdir().unwrap();
        let mut store = GapStore::open(&central.path().join("gaps.json")).unwrap();
        store.set_project_roots(vec![root.clone()]).unwrap();

        let (id, _) = store
            .file(&project_params(
                "redirected",
                "tooling/test-domain/redirected",
                &root,
            ))
            .unwrap();
        // A second base-owned gap keeps `root` in the save's write set so the
        // generation purge actually runs there.
        store
            .file(&project_params(
                "stayer",
                "tooling/test-domain/stayer2",
                &root,
            ))
            .unwrap();
        let base_file = root.join(".bbox/gaps").join(format!("{id}.json"));
        assert!(base_file.exists());
        let base_before = fs::read_to_string(&base_file).unwrap();

        store
            .resolve(&GapResolveParams {
                id: id.clone(),
                resolution: "addressed".into(),
                note: Some("done on branch".into()),
                project: Some(root.to_string_lossy().into_owned()),
                write_dir: Some(wt.to_string_lossy().into_owned()),
                ..Default::default()
            })
            .unwrap();

        let wt_file = wt.join(".bbox/gaps").join(format!("{id}.json"));
        assert!(
            wt_file.exists(),
            "redirected resolve must write the gap file into the worktree"
        );
        assert!(fs::read_to_string(&wt_file).unwrap().contains("addressed"));
        assert!(
            base_file.exists(),
            "redirected gap's base copy must survive the generation purge"
        );
        assert_eq!(
            fs::read_to_string(&base_file).unwrap(),
            base_before,
            "base checkout copy must be byte-for-byte untouched"
        );
    }

    /// Checkout-targeted gaps never leak into the host-global store. Registry
    /// overlay reconstruction owns restart visibility; after merge the base
    /// project loader observes the same committed record normally.
    #[test]
    fn redirected_gap_stays_out_of_central_and_loads_after_base_merge() {
        let base_dir = tempdir().unwrap();
        let root = base_dir.path().canonicalize().unwrap();
        let wt_dir = tempdir().unwrap();
        let wt = wt_dir.path().canonicalize().unwrap();
        fs::create_dir_all(root.join(".bbox/gaps")).unwrap();
        fs::create_dir_all(wt.join(".bbox/gaps")).unwrap();
        let central = tempdir().unwrap();
        let store_path = central.path().join("gaps.json");

        let mut store = GapStore::open(&store_path).unwrap();
        store.set_project_roots(vec![root.clone()]).unwrap();
        let mut p = project_params("survivor", "tooling/test-domain/survivor", &root);
        p.write_dir = Some(wt.to_string_lossy().into_owned());
        let (id, created) = store.file(&p).unwrap();
        assert!(created);
        let wt_file = wt.join(".bbox/gaps").join(format!("{id}.json"));
        let base_file = root.join(".bbox/gaps").join(format!("{id}.json"));
        assert!(wt_file.exists(), "worktree carries the redirected gap");
        assert!(!base_file.exists(), "base checkout must stay untouched");
        drop(store);

        let central_raw = fs::read_to_string(&store_path).unwrap();
        assert!(
            !central_raw.contains(&id),
            "checkout variant must not be retained centrally"
        );

        // A bare store reopen cannot see checkout-private bytes. The daemon's
        // registry overlay supplies that view instead.
        let mut store = GapStore::open(&store_path).unwrap();
        store.set_project_roots(vec![root.clone()]).unwrap();
        assert!(!store.data.gaps.iter().any(|gap| gap.id == id));

        // Merge lands: base root now carries the committed file.
        fs::copy(&wt_file, &base_file).unwrap();
        store.reload().unwrap();
        let g = store.data.gaps.iter().find(|g| g.id == id).unwrap();
        assert!(
            g.write_dir.is_none(),
            "observing the base file must drop the redirect"
        );
        assert!(base_file.exists());
    }

    /// A checkout removed before merging drops its provisional bytes with the
    /// branch checkout. The daemon neither retains a host-global copy nor
    /// falls back to rewriting the base checkout.
    #[test]
    fn dead_worktree_redirect_drops_without_writing_base_or_central() {
        let base_dir = tempdir().unwrap();
        let root = base_dir.path().canonicalize().unwrap();
        let wt_dir = tempdir().unwrap();
        let wt = wt_dir.path().canonicalize().unwrap();
        fs::create_dir_all(root.join(".bbox/gaps")).unwrap();
        fs::create_dir_all(wt.join(".bbox/gaps")).unwrap();
        let central = tempdir().unwrap();
        let store_path = central.path().join("gaps.json");

        let mut store = GapStore::open(&store_path).unwrap();
        store.set_project_roots(vec![root.clone()]).unwrap();
        let mut p = project_params("orphaned", "tooling/test-domain/orphaned", &root);
        p.write_dir = Some(wt.to_string_lossy().into_owned());
        let (id, _) = store.file(&p).unwrap();

        // Worktree removed before the branch merged.
        drop(wt_dir);
        store.save().unwrap();

        let base_file = root.join(".bbox/gaps").join(format!("{id}.json"));
        assert!(
            !base_file.exists(),
            "dead-worktree redirect must not fall back to a base-checkout write"
        );
        let central_raw = fs::read_to_string(&store_path).unwrap();
        assert!(
            !central_raw.contains(&id),
            "dead checkout variant must not remain centrally retained"
        );
    }

    #[test]
    fn supersession_uses_one_shared_repo_transaction() {
        let root_dir = tempdir().unwrap();
        let root = root_dir.path().canonicalize().unwrap();
        fs::create_dir_all(root.join(".bbox/gaps")).unwrap();
        let central = tempdir().unwrap();
        let mut store = GapStore::open(&central.path().join("gaps.json")).unwrap();
        store.set_project_roots(vec![root.clone()]).unwrap();
        let (old_id, _) = store
            .file(&project_params(
                "old",
                "tooling/test-domain/transaction-old",
                &root,
            ))
            .unwrap();
        let (new_id, _) = store
            .file(&project_params(
                "new",
                "tooling/test-domain/transaction-new",
                &root,
            ))
            .unwrap();

        let completed = root.join(".bbox/local/knowledge-transactions/completed");
        fs::remove_dir_all(&completed).unwrap();
        fs::create_dir_all(&completed).unwrap();
        store
            .resolve(&GapResolveParams {
                id: old_id,
                resolution: "addressed".into(),
                superseded_by: Some(new_id),
                ..Default::default()
            })
            .unwrap();

        let manifests = fs::read_dir(&completed)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("json"))
            .collect::<Vec<_>>();
        assert_eq!(manifests.len(), 1);
        let manifest: bbox_corpus_core::transaction::RepoTransactionManifest =
            serde_json::from_slice(&fs::read(&manifests[0]).unwrap()).unwrap();
        assert_eq!(manifest.files.len(), 2);
        assert!(
            manifest
                .files
                .iter()
                .all(|file| file.relative_path.starts_with(".bbox/gaps/"))
        );
    }

    #[test]
    fn monorepo_gap_transaction_is_anchored_at_checkout_root() {
        let root_dir = tempdir().unwrap();
        let root = root_dir.path().canonicalize().unwrap();
        let status = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&root)
            .status()
            .unwrap();
        assert!(status.success());
        let project = root.join("services/api");
        fs::create_dir_all(&project).unwrap();

        let central = tempdir().unwrap();
        let mut store = GapStore::open(&central.path().join("gaps.json")).unwrap();
        store.set_project_roots(vec![project.clone()]).unwrap();
        store
            .file(&project_params(
                "monorepo gap",
                "tooling/test-domain/monorepo-transaction",
                &project,
            ))
            .unwrap();

        let completed = root.join(".bbox/local/knowledge-transactions/completed");
        assert!(completed.is_dir());
        assert!(
            !project.join(".bbox/local/knowledge-transactions").exists(),
            "subproject must not create a second transaction lane"
        );
        let manifest_path = fs::read_dir(completed)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let manifest: bbox_corpus_core::transaction::RepoTransactionManifest =
            serde_json::from_slice(&fs::read(manifest_path).unwrap()).unwrap();
        assert!(
            manifest
                .files
                .iter()
                .all(|file| { file.relative_path.starts_with("services/api/.bbox/gaps/") })
        );
    }

    #[test]
    fn monorepo_gap_loader_checks_checkout_root_for_pending_transaction() {
        let root_dir = tempdir().unwrap();
        let root = root_dir.path().canonicalize().unwrap();
        let status = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&root)
            .status()
            .unwrap();
        assert!(status.success());
        let project = root.join("services/api");
        let gaps_dir = project.join(".bbox/gaps");
        fs::create_dir_all(&gaps_dir).unwrap();
        fs::write(
            gaps_dir.join("gap-deadbeef.json"),
            r#"{
  "id": "gap-deadbeef",
  "title": "Pending transaction gap",
  "gap_kind": "tooling",
  "domain": "test-domain",
  "wanted_capability": "remain hidden during the transaction",
  "impact": "medium",
  "blocking_level": "none",
  "dedupe_key": "tooling/test-domain/pending-transaction",
  "resolution": "unresolved",
  "created_at": "2026-07-21T00:00:00Z"
}"#,
        )
        .unwrap();
        let pending = root.join(".bbox/local/knowledge-transactions/pending.json");
        fs::create_dir_all(pending.parent().unwrap()).unwrap();
        fs::write(&pending, "{}\n").unwrap();
        assert!(
            !project.join(".bbox/local/knowledge-transactions").exists(),
            "subproject must use the checkout transaction lane"
        );

        let central = tempdir().unwrap();
        let mut store = GapStore::open(&central.path().join("gaps.json")).unwrap();
        store.set_project_roots(vec![project]).unwrap();

        assert!(
            !store.all().iter().any(|gap| gap.id == "gap-deadbeef"),
            "loader must not observe a subproject gap while the checkout transaction is pending"
        );
    }

    #[test]
    fn cut_rejects_authority_from_a_different_project() {
        let root_a_dir = tempdir().unwrap();
        let root_a = root_a_dir.path().canonicalize().unwrap();
        let root_b_dir = tempdir().unwrap();
        let root_b = root_b_dir.path().canonicalize().unwrap();
        let central = tempdir().unwrap();
        let mut store = GapStore::open(&central.path().join("gaps.json")).unwrap();
        store
            .set_project_roots(vec![root_a.clone(), root_b.clone()])
            .unwrap();
        let (id, _) = store
            .file(&project_params(
                "owned by a",
                "tooling/test-domain/project-authority",
                &root_a,
            ))
            .unwrap();
        let before = fs::read(root_a.join(".bbox/gaps").join(format!("{id}.json"))).unwrap();
        store.set_path_fallback_cut(true);

        let err = store
            .update(&GapUpdateParams {
                id: id.clone(),
                title: Some("mutated from b".into()),
                project: Some(root_b.to_string_lossy().into_owned()),
                write_dir: Some(root_b.to_string_lossy().into_owned()),
                ..Default::default()
            })
            .unwrap_err();
        assert!(err.to_string().contains("belongs to project"));
        assert_eq!(
            fs::read(root_a.join(".bbox/gaps").join(format!("{id}.json"))).unwrap(),
            before
        );
    }

    #[test]
    fn supersession_requires_authority_for_both_project_records() {
        let root_a_dir = tempdir().unwrap();
        let root_a = root_a_dir.path().canonicalize().unwrap();
        let root_b_dir = tempdir().unwrap();
        let root_b = root_b_dir.path().canonicalize().unwrap();
        let central = tempdir().unwrap();
        let mut store = GapStore::open(&central.path().join("gaps.json")).unwrap();
        store
            .set_project_roots(vec![root_a.clone(), root_b.clone()])
            .unwrap();
        let (old_id, _) = store
            .file(&project_params(
                "old a",
                "tooling/test-domain/cross-supersession-a",
                &root_a,
            ))
            .unwrap();
        let (new_id, _) = store
            .file(&project_params(
                "new b",
                "tooling/test-domain/cross-supersession-b",
                &root_b,
            ))
            .unwrap();
        store.set_path_fallback_cut(true);

        let err = store
            .resolve(&GapResolveParams {
                id: old_id.clone(),
                resolution: "addressed".into(),
                superseded_by: Some(new_id.clone()),
                project: Some(root_a.to_string_lossy().into_owned()),
                write_dir: Some(root_a.to_string_lossy().into_owned()),
                ..Default::default()
            })
            .unwrap_err();
        assert!(err.to_string().contains("belongs to project"));
        assert_eq!(
            store
                .all()
                .iter()
                .find(|gap| gap.id == old_id)
                .unwrap()
                .resolution,
            GapResolution::Unresolved
        );
        assert!(
            store
                .all()
                .iter()
                .find(|gap| gap.id == new_id)
                .unwrap()
                .supersedes
                .is_none()
        );
    }

    #[test]
    fn rejects_bad_dedupe_key() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let mut store = GapStore::open(&root.join("gaps.json")).unwrap();
        let err = store
            .file(&file_params("x", "too/short"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("dedupe_key"));
    }

    #[test]
    fn rejects_unknown_gap_kind() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let mut store = GapStore::open(&root.join("gaps.json")).unwrap();
        let mut p = file_params("x", "tooling/d/s");
        p.gap_kind = "nonsense".into();
        let err = store.file(&p).unwrap_err().to_string();
        assert!(err.contains("gap_kind must be one of"));
    }

    #[test]
    fn resolve_wires_supersession_both_ways() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let mut store = GapStore::open(&root.join("gaps.json")).unwrap();
        let (old_id, _) = store
            .file(&file_params("old", "tooling/test-domain/a"))
            .unwrap();
        let (new_id, _) = store
            .file(&file_params("new", "tooling/test-domain/b"))
            .unwrap();

        store
            .resolve(&GapResolveParams {
                id: old_id.clone(),
                resolution: "addressed".into(),
                note: Some("rolled into successor".into()),
                superseded_by: Some(new_id.clone()),
                ..Default::default()
            })
            .unwrap();

        let old = store.all().iter().find(|g| g.id == old_id).unwrap();
        let new = store.all().iter().find(|g| g.id == new_id).unwrap();
        assert_eq!(old.resolution, GapResolution::Addressed);
        assert_eq!(old.superseded_by.as_deref(), Some(new_id.as_str()));
        assert_eq!(new.supersedes.as_deref(), Some(old_id.as_str()));
    }

    #[test]
    fn update_edits_in_place() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let mut store = GapStore::open(&root.join("gaps.json")).unwrap();
        let (id, _) = store
            .file(&file_params("orig", "tooling/test-domain/a"))
            .unwrap();
        store
            .update(&GapUpdateParams {
                id: id.clone(),
                title: Some("revised title".into()),
                impact: Some("critical".into()),
                ..Default::default()
            })
            .unwrap();
        let g = store.all().iter().find(|g| g.id == id).unwrap();
        assert_eq!(g.title, "revised title");
        assert_eq!(g.impact, GapImpact::Critical);
    }

    #[test]
    fn project_scoped_gap_lands_in_repo() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let project = root.join("project");
        fs::create_dir_all(&project).unwrap();
        let mut store = GapStore::open(&root.join("gaps.json")).unwrap();
        store.set_project_roots(vec![project.clone()]).unwrap();

        let mut p = file_params("repo gap", "tooling/test-domain/repo");
        p.scope = Some("project".into());
        p.project = Some(project.to_string_lossy().to_string());
        let (id, created) = store.file(&p).unwrap();
        assert!(created);

        // Lands as a top-level file under <project>/.bbox/gaps/.
        let on_disk = project
            .join(".bbox")
            .join("gaps")
            .join(format!("{id}.json"));
        assert!(
            on_disk.exists(),
            "gap should be committed in-repo at {on_disk:?}"
        );
        // The on-disk file omits `project` (location encodes scope).
        let raw = fs::read_to_string(&on_disk).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(v.get("project").is_none());
        // Central store should NOT carry the repo-owned gap.
        let central = fs::read_to_string(root.join("gaps.json")).unwrap();
        let cv: GapStoreData = serde_json::from_str(&central).unwrap();
        assert!(cv.gaps.is_empty(), "repo-owned gap must not stay central");
    }

    #[test]
    fn ingest_from_envelope_derives_dedupe_when_absent() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let mut store = GapStore::open(&root.join("gaps.json")).unwrap();
        let env = serde_json::json!({
            "type": GAP_NOTE_TYPE,
            "title": "Need a Foo Hook",
            "gap_kind": "workflow",
            "domain": "ingest test",
            "wanted_capability": "hook the foo"
        });
        let gap = GapNote::from_envelope(&env, GapStore::gen_id(), GapStore::now_iso()).unwrap();
        assert_eq!(gap.dedupe_key, "workflow/ingest-test/need-a-foo-hook");
        let (_id, created) = store.ingest(gap).unwrap();
        assert!(created);
    }

    #[test]
    fn loader_ignores_spool_inbox_subdir() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let project = root.join("project");
        let gaps_dir = project.join(".bbox").join("gaps");
        let inbox = gaps_dir.join("inbox");
        fs::create_dir_all(&inbox).unwrap();
        // A spool drop file under inbox/ must NOT be loaded as a durable gap.
        fs::write(
            inbox.join("dropped.json"),
            serde_json::json!({"type": GAP_NOTE_TYPE, "title": "x"}).to_string(),
        )
        .unwrap();
        let mut store = GapStore::open(&root.join("gaps.json")).unwrap();
        store.set_project_roots(vec![project.clone()]).unwrap();
        assert!(
            store.all().is_empty(),
            "inbox/ files are spool-owned, not durable gaps"
        );
    }

    // ── Dual-read (plan §8.2) ────────────────────────────────────────────

    fn dual_read_gap(id: &str, project: &str, project_id: Option<&str>) -> GapNote {
        GapNote {
            id: id.into(),
            title: "dual read gap".into(),
            gap_kind: GapKind::Tooling,
            domain: "dual-read".into(),
            wanted_capability: "identity-first visibility".into(),
            missing_primitive: None,
            fallback_used: None,
            evidence: Vec::new(),
            impact: GapImpact::Medium,
            blocking_level: BlockingLevel::None,
            dedupe_key: format!("dual-read-{id}"),
            suggested_owner: None,
            notes: None,
            supersedes: None,
            superseded_by: None,
            resolution: GapResolution::Unresolved,
            project: Some(project.into()),
            project_id: project_id.map(str::to_string),
            write_dir: None,
            provisional_checkout_id: None,
            task_id: None,
            session_id: None,
            provider: None,
            bro: None,
            thread_id: None,
            created_at: "2026-07-24T00:00:00Z".into(),
            updated_at: "2026-07-24T00:00:00Z".into(),
            resolved_at: None,
            resolution_note: None,
        }
    }

    #[test]
    fn gap_row_without_project_id_decodes_and_round_trips() {
        let legacy = serde_json::json!({
            "id": "gap-legacy1",
            "title": "t",
            "gap_kind": "tooling",
            "domain": "d",
            "wanted_capability": "w",
            "impact": "medium",
            "blocking_level": "none",
            "dedupe_key": "k",
            "resolution": "unresolved",
            "project": "/repo/old",
            "created_at": "2026-07-24T00:00:00Z"
        });
        let gap: GapNote = serde_json::from_value(legacy).unwrap();
        assert_eq!(gap.project_id, None);
        assert!(
            serde_json::to_value(&gap)
                .unwrap()
                .get("project_id")
                .is_none()
        );

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gaps.json");
        let mut store = GapStore::open(&path).unwrap();
        store.data.gaps.push(gap);
        std::fs::write(&path, serde_json::to_string(&store.data).unwrap()).unwrap();
        let reopened = GapStore::open(&path).unwrap();
        assert_eq!(reopened.data.gaps.len(), 1);
        assert_eq!(reopened.data.gaps[0].project_id, None);
    }

    #[test]
    fn gap_project_id_match_wins_over_a_different_path() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = GapStore::open(&dir.path().join("gaps.json")).unwrap();
        store
            .data
            .gaps
            .push(dual_read_gap("gap-aaaaaaaa", "/repo/old", Some("abc12345")));

        let hits = store.query(&GapListParams {
            project: Some("/repo/relocated".into()),
            project_id: Some("abc12345".into()),
            ..Default::default()
        });
        assert_eq!(hits.len(), 1, "id arm must match: {hits:?}");
    }

    #[test]
    fn gap_without_ids_falls_back_to_the_exact_path_arm() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = GapStore::open(&dir.path().join("gaps.json")).unwrap();
        store
            .data
            .gaps
            .push(dual_read_gap("gap-bbbbbbbb", "/repo/old", None));

        let miss = store.query(&GapListParams {
            project: Some("/repo/relocated".into()),
            project_id: Some("abc12345".into()),
            ..Default::default()
        });
        assert!(miss.is_empty(), "path arm must decide: {miss:?}");

        let hit = store.query(&GapListParams {
            project: Some("/repo/old".into()),
            ..Default::default()
        });
        assert_eq!(hit.len(), 1, "path arm must match");
    }

    #[test]
    fn gap_mismatched_ids_hide_the_row_at_the_same_path() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = GapStore::open(&dir.path().join("gaps.json")).unwrap();
        store
            .data
            .gaps
            .push(dual_read_gap("gap-cccccccc", "/repo/old", Some("abc12345")));

        // Same path key, different ids: the id decides against the row, so a
        // path reused after a retire-and-add cannot leak the old rows.
        let hits = store.query(&GapListParams {
            project: Some("/repo/old".into()),
            project_id: Some("def67890".into()),
            ..Default::default()
        });
        assert!(hits.is_empty(), "id mismatch must hide: {hits:?}");
    }

    #[test]
    fn gap_ledger_paths_match_a_path_only_row_under_a_historical_path() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = GapStore::open(&dir.path().join("gaps.json")).unwrap();
        store
            .data
            .gaps
            .push(dual_read_gap("gap-dddddddd", "/repo/old", None));

        // Catalog-mode ledger arm: the relocated project queries by its
        // current key, and the ledger's historical key still reaches the row.
        let hit = store.query(&GapListParams {
            project: Some("/repo/relocated".into()),
            project_ledger_paths: vec!["/repo/old".into()],
            ..Default::default()
        });
        assert_eq!(hit.len(), 1, "ledger arm must match: {hit:?}");

        // Bridge mode carries no ledger paths, so the historical row stays
        // invisible to the relocated key.
        let miss = store.query(&GapListParams {
            project: Some("/repo/relocated".into()),
            ..Default::default()
        });
        assert!(miss.is_empty(), "no ledger path must not match: {miss:?}");
    }
}

/// Companion-gap-note integration tests. They live here (not in the
/// bbox-packets leaf crate) because they exercise the packets→gap-store
/// bridge, and the leaf crate must stay free of gap-store coupling.
#[cfg(test)]
mod packet_companion_tests {
    use bbox_packets::{GapParams, Packets};
    use tempfile::TempDir;

    #[test]
    fn companion_gap_note_created() {
        let dir = TempDir::new().unwrap();
        let packets = Packets::open(dir.path()).unwrap();
        let gaps_path = dir.path().join("gaps.json");
        let gaps = crate::gaps::GapStore::open(&gaps_path).unwrap();
        let gaps_lock = parking_lot::RwLock::new(gaps);

        let ev = packets
            .log_gap(
                "wanted regex matching on log messages",
                Some("auth"),
                Some("CountInWindow{...}"),
                Some("prose rubric"),
                Some("StringMatches"),
            )
            .unwrap();

        let params = GapParams {
            description: "wanted regex matching on log messages".into(),
            domain: Some("auth".into()),
            attempted_sketch: Some("CountInWindow{...}".into()),
            fallback_used: Some("prose rubric".into()),
            ast_feature_requested: Some("StringMatches".into()),
        };

        let warning = crate::gaps::emit_companion_packet_gap_note(&gaps_lock, &ev, &params);
        assert!(warning.is_none(), "should succeed without warning");

        let gaps = gaps_lock.read();
        assert_eq!(gaps.all().len(), 1);
        let gap = &gaps.all()[0];
        assert_eq!(gap.gap_kind, crate::gaps::GapKind::PacketAst);
        assert_eq!(gap.domain, "auth");
        assert_eq!(gap.dedupe_key, "packet_ast/auth/StringMatches");
        assert_eq!(
            gap.wanted_capability,
            "wanted regex matching on log messages"
        );
    }

    #[test]
    fn companion_gap_note_deduplicates() {
        let dir = TempDir::new().unwrap();
        let packets = Packets::open(dir.path()).unwrap();
        let gaps_path = dir.path().join("gaps.json");
        let gaps = crate::gaps::GapStore::open(&gaps_path).unwrap();
        let gaps_lock = parking_lot::RwLock::new(gaps);

        let params = GapParams {
            description: "wanted regex".into(),
            domain: Some("auth".into()),
            attempted_sketch: None,
            fallback_used: None,
            ast_feature_requested: Some("StringMatches".into()),
        };

        let ev = packets
            .log_gap(
                "wanted regex",
                Some("auth"),
                None,
                None,
                Some("StringMatches"),
            )
            .unwrap();
        let _ = crate::gaps::emit_companion_packet_gap_note(&gaps_lock, &ev, &params);
        assert_eq!(gaps_lock.read().all().len(), 1);

        let ev2 = packets
            .log_gap(
                "wanted regex",
                Some("auth"),
                None,
                None,
                Some("StringMatches"),
            )
            .unwrap();
        let _ = crate::gaps::emit_companion_packet_gap_note(&gaps_lock, &ev2, &params);
        assert_eq!(
            gaps_lock.read().all().len(),
            1,
            "second call should not create a duplicate"
        );
    }

    #[test]
    fn companion_gap_note_deduplicates_acknowledged() {
        let dir = TempDir::new().unwrap();
        let packets = Packets::open(dir.path()).unwrap();
        let gaps_path = dir.path().join("gaps.json");
        let gaps = crate::gaps::GapStore::open(&gaps_path).unwrap();
        let gaps_lock = parking_lot::RwLock::new(gaps);

        let params = GapParams {
            description: "no rate predicate".into(),
            domain: Some("rate-limit".into()),
            attempted_sketch: None,
            fallback_used: None,
            ast_feature_requested: Some("RateCmp".into()),
        };

        let ev = packets
            .log_gap(
                "no rate predicate",
                Some("rate-limit"),
                None,
                None,
                Some("RateCmp"),
            )
            .unwrap();
        let _ = crate::gaps::emit_companion_packet_gap_note(&gaps_lock, &ev, &params);
        let gap_id = gaps_lock.read().all()[0].id.clone();

        gaps_lock
            .write()
            .resolve(&crate::gaps::GapResolveParams {
                id: gap_id,
                resolution: "acknowledged".into(),
                ..Default::default()
            })
            .unwrap();

        let ev2 = packets
            .log_gap(
                "no rate predicate",
                Some("rate-limit"),
                None,
                None,
                Some("RateCmp"),
            )
            .unwrap();
        let _ = crate::gaps::emit_companion_packet_gap_note(&gaps_lock, &ev2, &params);
        assert_eq!(
            gaps_lock.read().all().len(),
            1,
            "acknowledged gap note should block new companion"
        );
    }

    #[test]
    fn companion_gap_note_allows_after_addressed() {
        let dir = TempDir::new().unwrap();
        let packets = Packets::open(dir.path()).unwrap();
        let gaps_path = dir.path().join("gaps.json");
        let gaps = crate::gaps::GapStore::open(&gaps_path).unwrap();
        let gaps_lock = parking_lot::RwLock::new(gaps);

        let params = GapParams {
            description: "no temporal window".into(),
            domain: Some("retry".into()),
            attempted_sketch: None,
            fallback_used: None,
            ast_feature_requested: Some("Within{temporal}".into()),
        };

        let ev = packets
            .log_gap(
                "no temporal window",
                Some("retry"),
                None,
                None,
                Some("Within{temporal}"),
            )
            .unwrap();
        let _ = crate::gaps::emit_companion_packet_gap_note(&gaps_lock, &ev, &params);
        let gap_id = gaps_lock.read().all()[0].id.clone();

        gaps_lock
            .write()
            .resolve(&crate::gaps::GapResolveParams {
                id: gap_id,
                resolution: "addressed".into(),
                note: Some("implemented RateCmp".into()),
                ..Default::default()
            })
            .unwrap();

        let ev2 = packets
            .log_gap(
                "no temporal window",
                Some("retry"),
                None,
                None,
                Some("Within{temporal}"),
            )
            .unwrap();
        let _ = crate::gaps::emit_companion_packet_gap_note(&gaps_lock, &ev2, &params);
        assert_eq!(
            gaps_lock.read().all().len(),
            2,
            "addressed gap note should allow new companion"
        );
    }

    #[test]
    fn packet_event_survives_note_failure() {
        let dir = TempDir::new().unwrap();
        let packets = Packets::open(dir.path()).unwrap();
        let broken_path = dir.path().join("gaps.json");
        let gaps = crate::gaps::GapStore::open(&broken_path).unwrap();
        std::fs::create_dir(&broken_path).unwrap();
        let gaps_lock = parking_lot::RwLock::new(gaps);

        let params = GapParams {
            description: "some gap".into(),
            domain: None,
            attempted_sketch: None,
            fallback_used: None,
            ast_feature_requested: Some("Foo".into()),
        };

        let ev = packets
            .log_gap("some gap", None, None, None, Some("Foo"))
            .unwrap();

        let warning = crate::gaps::emit_companion_packet_gap_note(&gaps_lock, &ev, &params);
        assert!(
            warning.is_some(),
            "gap creation should fail on unwritable path"
        );
        assert!(warning.unwrap().contains("companion gap note failed"));

        let events = packets
            .list_events(Some("gap"), None, None, None, 10)
            .unwrap();
        assert_eq!(events.len(), 1, "packet event must survive note failure");
    }
}

// ── Project-catalog row stamping (P6-B) ────────────────────────────────────

#[cfg(test)]
mod owner_row_stamping {
    use super::*;
    use bbox_corpus_core::project_catalog_snapshot::{
        OWNER_ROW_ABSENT, OWNER_ROW_PROJECT_ID_CONFLICT, OWNER_SOURCE_MISSING,
        OwnerRowStampOutcomeV1, OwnerSnapshotLimitsV1,
    };

    /// Two gaps plus a field this binary does not model, so every test also
    /// witnesses preservation of data the compiled schema cannot see.
    fn write_fixture(store_path: &Path) {
        std::fs::write(
            store_path,
            br#"{
  "version": 1,
  "gaps": [
    {
      "id": "gap-0001",
      "project": "/legacy/path/one",
      "future_field": {"kept": true}
    },
    {
      "id": "gap-0002",
      "project": "/legacy/path/two"
    }
  ]
}
"#,
        )
        .unwrap();
    }

    fn stamp(
        store_path: &Path,
        row: &str,
        project_id: &str,
    ) -> std::result::Result<
        OwnerRowStampOutcomeV1,
        bbox_corpus_core::project_catalog_snapshot::OwnerRowStampError,
    > {
        stamp_project_catalog_owner_row(
            store_path,
            row,
            &bbox_corpus_core::project_catalog_snapshot::singleton_selector_members(row),
            project_id,
            OwnerSnapshotLimitsV1::default(),
        )
    }

    fn read_row(store_path: &Path, row: &str) -> serde_json::Value {
        let document: serde_json::Value =
            serde_json::from_slice(&std::fs::read(store_path).unwrap()).unwrap();
        document["gaps"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["id"] == row)
            .cloned()
            .unwrap()
    }

    fn fixture_store(dir: &tempfile::TempDir) -> std::path::PathBuf {
        let store_path = dir.path().canonicalize().unwrap().join("gaps.json");
        write_fixture(&store_path);
        store_path
    }

    #[test]
    fn a_fresh_row_takes_the_stamp() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = fixture_store(&dir);

        assert_eq!(
            stamp(&store_path, "gap-0001", "a1b2c3d4").unwrap(),
            OwnerRowStampOutcomeV1::Stamped
        );

        let row = read_row(&store_path, "gap-0001");
        assert_eq!(row["project_id"], "a1b2c3d4");
        // The legacy selector is RETAINED: dual-read still resolves through it
        // until the later path-fallback removal gate.
        assert_eq!(row["project"], "/legacy/path/one");
        // A field this binary does not model survives the write-back.
        assert_eq!(row["future_field"]["kept"], true);
        // Stamping one row must not touch its neighbours.
        assert!(
            read_row(&store_path, "gap-0002")
                .get("project_id")
                .is_none()
        );
    }

    /// Re-applying a torn backfill must complete, not double-write.
    #[test]
    fn restamping_the_same_id_is_an_idempotent_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = fixture_store(&dir);

        stamp(&store_path, "gap-0001", "a1b2c3d4").unwrap();
        let after_first = std::fs::read(&store_path).unwrap();

        assert_eq!(
            stamp(&store_path, "gap-0001", "a1b2c3d4").unwrap(),
            OwnerRowStampOutcomeV1::AlreadyStamped
        );
        // Byte-identical: the second stamp elided the write entirely.
        assert_eq!(std::fs::read(&store_path).unwrap(), after_first);
    }

    /// Never a silent overwrite: a row bound to another project refuses.
    #[test]
    fn a_conflicting_id_refuses_and_leaves_the_row_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = fixture_store(&dir);

        stamp(&store_path, "gap-0001", "a1b2c3d4").unwrap();
        let before = std::fs::read(&store_path).unwrap();

        let error = stamp(&store_path, "gap-0001", "99998888").unwrap_err();
        assert_eq!(error.code, OWNER_ROW_PROJECT_ID_CONFLICT);
        assert_eq!(read_row(&store_path, "gap-0001")["project_id"], "a1b2c3d4");
        assert_eq!(std::fs::read(&store_path).unwrap(), before);
    }

    /// Absence is a refusal, never a success: a resolution naming a row this
    /// store does not have must not report progress.
    #[test]
    fn an_absent_row_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = fixture_store(&dir);

        let error = stamp(&store_path, "row-does-not-exist", "a1b2c3d4").unwrap_err();
        assert_eq!(error.code, OWNER_ROW_ABSENT);
    }

    /// An absent SOURCE is likewise a refusal, and must not create a store.
    #[test]
    fn an_absent_source_refuses_without_creating_it() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().canonicalize().unwrap().join("gaps.json");

        let error = stamp(&store_path, "gap-0001", "a1b2c3d4").unwrap_err();
        assert_eq!(error.code, OWNER_SOURCE_MISSING);
        assert!(!store_path.exists());
    }
}
