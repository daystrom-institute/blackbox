//! Rule-packets — compressive compilation of observations into portable theories.
//!
//! A `Packet` carries a small axiomatic theory extracted from a larger
//! observation set. The sender's LLM extracts the theory once; any number
//! of receivers evaluate it deterministically via `apply`. No LLM in the
//! receive path. Compression survives because the evaluator is a pure
//! function of `(Packet, entity) -> Vec<Prediction>`.
//!
//! See thread-0b20e854 notes `note-23128468` / `note-41493d8c` / `note-26522dd7`
//! for the empirical validation chain (E8/E9/E10/E11). Decision entry
//! `154fd624` names this primitive.
//!
//! Storage: one JSON file per packet under `<state>/packets/<scope>/<id>.json`.
//! IDs are canonical `packet-<8hex>`, matching the `note-` / `thread-` shape.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
pub use apply::*;
pub use ast::*;
pub use audit::*;
use coerce::*;
pub use compile::*;
pub use events::*;
use rmcp::schemars;
pub use scanner::*;
use serde::{Deserialize, Serialize};

// ── MCP parameter structs ─────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PacketListParams {
    /// Optional domain to filter by (e.g. "auth-matrix"). Exact match.
    #[serde(default)]
    pub domain: Option<String>,
    /// Optional scope to filter by ("global" or "project").
    #[serde(default)]
    pub scope: Option<String>,
    /// Case-insensitive substring match across packet id, domain, rule ids,
    /// classification lattice values, and rule classifications. Use when you
    /// know the concept ("breaking", "pii", "deny") but not which domain it
    /// was filed under.
    #[serde(default)]
    pub query: Option<String>,
    /// When true, return only the newest packet per domain. Useful after
    /// multiple compile iterations — `list_all` returns every revision.
    #[serde(default)]
    pub latest_per_domain: Option<bool>,
    /// Max packets to return (default: 50, max: 500).
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EventsParams {
    /// Filter by operation: `compile`, `apply`, `audit`, `gap`,
    /// `repair_candidate` (emitted by the self-heal scanner when
    /// `PACKET_SELF_HEAL_ENABLED=true`).
    #[serde(default)]
    pub op: Option<String>,
    /// Filter by packet id (canonical `packet-<8hex>` or bare hex).
    #[serde(default)]
    pub packet_id: Option<String>,
    /// Filter by outcome: `ok`, `error`, `no_match`, `low_fidelity`,
    /// `logged`.
    #[serde(default)]
    pub outcome: Option<String>,
    /// ISO 8601 lower bound; events with earlier timestamps are excluded.
    #[serde(default)]
    pub since: Option<String>,
    /// Hard cap on returned rows (default 50, max 500).
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GapParams {
    /// One-sentence description of what the author wanted to express
    /// and why the AST couldn't handle it. E.g. "wanted to flag
    /// requests exceeding 10 per minute per user; no rate/time
    /// predicate".
    pub description: String,
    /// Optional domain tag (e.g. `pr-review`, `auth`, `retry`) to
    /// cluster gaps by use case.
    #[serde(default)]
    pub domain: Option<String>,
    /// Optional sketch of the rule you would have written if the AST
    /// supported it, in free form.
    #[serde(default)]
    pub attempted_sketch: Option<String>,
    /// Optional note on what you fell back to (prose rubric, ad-hoc
    /// code, different tool, giving up).
    #[serde(default)]
    pub fallback_used: Option<String>,
    /// Optional name of the primitive you wished existed
    /// (e.g. `RateCmp`, `StringMatches`, `Within{temporal}`).
    #[serde(default)]
    pub ast_feature_requested: Option<String>,
}

// ── Packet ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Packet {
    pub id: String,
    pub domain: String,
    pub scope: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,

    /// Lookup: role-name → rank. Augments the entity at eval time when
    /// `rank_lookup_key` resolves to a string present in this table.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub rank_table: BTreeMap<String, i64>,
    /// Lookup: resource-name → threshold. Paired with `threshold_lookup_key`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub threshold_table: BTreeMap<String, i64>,
    /// Entity field whose value keys `rank_table`. Defaults to `"role"`.
    /// The resolved rank is inserted under `"role_rank"` (i.e. `<field>_rank`).
    #[serde(default = "default_rank_lookup_key")]
    pub rank_lookup_key: String,
    /// Entity field whose value keys `threshold_table`. Defaults to
    /// `"resource"`. Resolved threshold inserted under `"res_threshold"`.
    #[serde(default = "default_threshold_lookup_key")]
    pub threshold_lookup_key: String,

    /// Classification lattice — highest precedence first. Each rule's
    /// `classification` must be in this list. In `apply(mode="all")`,
    /// the aggregate verdict is the first-listed class any rule fired.
    /// Defaults to the review lattice when omitted.
    #[serde(default = "review_lattice")]
    pub classification_lattice: Vec<String>,
    /// Id-prefix → classification map for inference when a rule omits
    /// classification. Defaults to the review prefixes (`fail_*` → fail,
    /// `flag_*` → flag, `manual_*`/`review_*` → manual, `pass_*` → pass).
    #[serde(default = "review_prefix_inference")]
    pub prefix_inference: BTreeMap<String, String>,

    /// Ordered rules — first matching antecedent wins.
    pub rules: Vec<Rule>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub self_audit_fidelity: Option<f32>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub merged_from: Vec<String>,
}

/// Canonical serialization of a packet's durable content, with the volatile
/// identity/bookkeeping fields (`id`, `created_at`, `updated_at`,
/// `self_audit_fidelity`) removed. Two packets with equal fingerprints carry
/// the same rules, tables, lattice, and scope — they are interchangeable for
/// evaluation. `superseded_by`/`merged_from` are deliberately retained: a
/// superseded packet is not a duplicate of its live sibling.
pub fn content_fingerprint(packet: &Packet) -> String {
    let mut v = serde_json::to_value(packet).unwrap_or_default();
    if let Some(obj) = v.as_object_mut() {
        obj.remove("id");
        obj.remove("created_at");
        obj.remove("updated_at");
        obj.remove("self_audit_fidelity");
    }
    v.to_string()
}

/// Result of [`Packets::compile_idempotent`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompileOutcome {
    /// A new packet was written; carries the new packet id.
    Created(String),
    /// An identical-content packet already existed; carries its id.
    UnchangedExisting(String),
}

/// Report from [`Packets::gc_duplicate_packets`].
#[derive(Debug, Clone, Serialize)]
pub struct PacketGcReport {
    pub applied: bool,
    /// Total packets scanned across both scopes.
    pub scanned: usize,
    /// Duplicate copies deleted (or that would be deleted in dry-run).
    pub deleted: usize,
    /// Duplicates kept because another packet's `Apply` references them.
    pub protected_by_refs: usize,
    /// Lock files without a backing packet json swept up.
    pub orphan_locks_removed: usize,
    /// Deleted-duplicate counts by domain.
    pub per_domain: BTreeMap<String, usize>,
}

fn slugify(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars().take(48) {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_end_matches('-').to_string()
}

fn default_rank_lookup_key() -> String {
    "role".to_string()
}

fn default_threshold_lookup_key() -> String {
    "resource".to_string()
}

/// Store handle. One `Packets` per daemon; constructs/lists from the
/// state directory directly (one file per packet, unlike notes which
/// share a single JSON).
pub struct Packets {
    packets_dir: PathBuf,
    /// Monotonic mutation counter. Bumped on every durable store change
    /// (save/remove/gc) so read-side caches — notably the MCP wire head's
    /// surface-decision cache — can validate without rescanning the
    /// directory. Starts at 1 so a zero-initialized cache slot is always
    /// treated as stale. Interior-atomic because mutators take `&self`
    /// behind the SharedState read lock.
    generation: AtomicU64,
}

impl Packets {
    pub fn open(packets_dir: &Path) -> Result<Self> {
        let actual_dir = packets_dir.to_path_buf();

        fs::create_dir_all(&actual_dir)
            .with_context(|| format!("creating {}", actual_dir.display()))?;
        Ok(Self {
            packets_dir: actual_dir,
            generation: AtomicU64::new(1),
        })
    }

    /// Current mutation generation. Equal values across two reads guarantee
    /// no packet was saved or removed through this handle in between.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    fn bump_generation(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
    }

    fn gen_id() -> String {
        use std::time::SystemTime;
        let d = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let hash = d.as_nanos() ^ 0x9e3779b97f4a7c15;
        format!("packet-{:08x}", hash as u32)
    }

    fn now_iso() -> String {
        chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    }

    /// Append one event to the log. Best-effort: errors are logged
    /// via tracing but never propagate, so event-log I/O can never
    /// break a compile/apply/audit operation.
    fn append_event(&self, event: &PacketEvent) {
        let path = events_log_path(&self.packets_dir);
        if let Some(parent) = path.parent() {
            if let Err(e) = fs::create_dir_all(parent) {
                tracing::warn!(error = %e, "packet event log: create_dir_all failed");
                return;
            }
        }
        let line = match serde_json::to_string(event) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "packet event log: serialize failed");
                return;
            }
        };
        if let Err(e) = append_line(&path, &line) {
            tracing::warn!(error = %e, "packet event log: append failed");
        }
    }

    /// Read events from the log, newest first, with optional filters.
    pub fn list_events(
        &self,
        op: Option<&str>,
        packet_id: Option<&str>,
        outcome: Option<&str>,
        since: Option<&str>,
        limit: usize,
    ) -> Result<Vec<PacketEvent>> {
        let path = events_log_path(&self.packets_dir);
        if !path.exists() {
            return Ok(Vec::new());
        }
        let raw =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let mut events: Vec<PacketEvent> = Vec::new();
        for line in raw.lines().rev() {
            if events.len() >= limit {
                break;
            }
            if line.trim().is_empty() {
                continue;
            }
            let ev: PacketEvent = match serde_json::from_str(line) {
                Ok(e) => e,
                Err(_) => continue, // skip malformed lines silently
            };
            if let Some(o) = op {
                if ev.op != o {
                    continue;
                }
            }
            if let Some(id) = packet_id {
                let want = normalize_id(id);
                match &ev.packet_id {
                    Some(p) if p == &want => {}
                    _ => continue,
                }
            }
            if let Some(oc) = outcome {
                if ev.outcome != oc {
                    continue;
                }
            }
            if let Some(s) = since {
                if ev.timestamp.as_str() < s {
                    continue;
                }
            }
            events.push(ev);
        }
        Ok(events)
    }

    pub fn log_gap(
        &self,
        description: &str,
        domain: Option<&str>,
        attempted_sketch: Option<&str>,
        fallback_used: Option<&str>,
        ast_feature_requested: Option<&str>,
    ) -> Result<PacketEvent> {
        if description.trim().is_empty() {
            anyhow::bail!("'description' is required and cannot be empty");
        }
        let mut details = serde_json::Map::new();
        details.insert(
            "description".into(),
            serde_json::Value::String(description.to_string()),
        );
        if let Some(v) = attempted_sketch {
            details.insert(
                "attempted_sketch".into(),
                serde_json::Value::String(v.into()),
            );
        }
        if let Some(v) = fallback_used {
            details.insert("fallback_used".into(), serde_json::Value::String(v.into()));
        }
        if let Some(v) = ast_feature_requested {
            details.insert(
                "ast_feature_requested".into(),
                serde_json::Value::String(v.into()),
            );
        }
        let mut ev =
            PacketEvent::now("gap", "logged").with_details(serde_json::Value::Object(details));
        if let Some(d) = domain {
            ev = ev.with_domain(d);
        }
        self.append_event(&ev);
        Ok(ev)
    }

    pub fn gap_dedupe_key(
        domain: Option<&str>,
        ast_feature_requested: Option<&str>,
        description: &str,
    ) -> String {
        let slug = match ast_feature_requested {
            Some(f) if !f.trim().is_empty() => f.trim().to_string(),
            _ => slugify(description),
        };
        format!("packet_ast/{}/{}", domain.unwrap_or("unknown"), slug)
    }

    pub fn build_gap_note_body(ev: &PacketEvent, params: &GapParams, dedupe_key: &str) -> String {
        let body = serde_json::json!({
            "type": "blackbox.gap_note.v1",
            "title": format!("Packet AST gap: {}",
                ev.domain.as_deref()
                    .or(params.ast_feature_requested.as_deref())
                    .unwrap_or("unknown")),
            "gap_kind": "packet_ast",
            "domain": ev.domain.as_deref().unwrap_or("unknown"),
            "wanted_capability": params.description,
            "missing_primitive": params.ast_feature_requested,
            "fallback_used": params.fallback_used,
            "impact": "medium",
            "blocking_level": "workaround_available",
            "evidence": [format!("packet-event:gap:{}", ev.timestamp)],
            "dedupe_key": dedupe_key,
            "notes": params.attempted_sketch,
        });
        body.to_string()
    }

    /// Emit the companion gap note for a packet-authoring gap into the
    /// first-class gap store. Global-scoped (substrate gaps aren't repo-owned);
    /// `GapStore::ingest` handles open-duplicate dedupe by `dedupe_key`.
    pub fn emit_companion_gap_note(
        gaps_lock: &parking_lot::RwLock<crate::gaps::GapStore>,
        ev: &PacketEvent,
        params: &GapParams,
    ) -> Option<String> {
        let dedupe_key = Self::gap_dedupe_key(
            ev.domain.as_deref(),
            params.ast_feature_requested.as_deref(),
            &params.description,
        );

        let body = Self::build_gap_note_body(ev, params, &dedupe_key);
        let value: serde_json::Value = match serde_json::from_str(&body) {
            Ok(v) => v,
            Err(e) => return Some(format!("companion gap note build failed: {e:#}")),
        };
        let gap = match crate::gaps::GapNote::from_envelope(
            &value,
            String::new(),
            crate::util::now_iso(),
        ) {
            Ok(g) => g,
            Err(e) => return Some(format!("companion gap note build failed: {e:#}")),
        };

        match gaps_lock.write().ingest(gap) {
            Ok(_) => None,
            Err(e) => Some(format!("companion gap note failed: {e:#}")),
        }
    }

    fn save_packet(&self, packet: &Packet) -> Result<()> {
        let path = packet_path(&self.packets_dir, &packet.scope, &packet.id);
        let result = crate::json_store::with_store_lock(&path, || {
            crate::json_store::atomic_write_json_locked(&path, packet)
        });
        if result.is_ok() {
            self.bump_generation();
        }
        result
    }

    /// Search both scopes for a packet by canonical ID or bare suffix.
    pub fn load(&self, id: &str) -> Result<Packet> {
        // Domain-shaped reference: `domain:<domain-name>` resolves to
        // the most-recently-compiled packet matching that domain.
        // Lets workflow + webhook specs reference packets by stable
        // human name without pinning to a specific compile hash —
        // recompiling the routing rules doesn't break the consumers.
        if let Some(domain) = id.strip_prefix("domain:") {
            let mut matches: Vec<Packet> = self
                .list_all()
                .unwrap_or_default()
                .into_iter()
                .filter(|p| p.domain == domain)
                .collect();
            if matches.is_empty() {
                anyhow::bail!(
                    "no packet found for domain '{domain}' (try `bbox_packet_list(query=\"{domain}\")`)"
                );
            }
            matches.sort_by(|a, b| b.created_at.cmp(&a.created_at));
            return Ok(matches.into_iter().next().unwrap());
        }
        let needle = normalize_id(id);
        for scope in &["global", "project"] {
            let path = packet_path(&self.packets_dir, scope, &needle);
            if path.exists() {
                let raw = fs::read_to_string(&path)
                    .with_context(|| format!("reading {}", path.display()))?;
                let packet: Packet = serde_json::from_str(&raw)
                    .with_context(|| format!("parsing {}", path.display()))?;
                return Ok(packet);
            }
        }
        anyhow::bail!(
            "Packet not found: {id} (expected `packet-<8hex>` or `domain:<name>`, e.g. `packet-a1b2c3d4` or `domain:webhook-routing/forgejo`)"
        )
    }

    /// Load the latest packet for a given domain, with project-scoped override.
    ///
    /// Project-scoped packets take precedence: if a project is specified and
    /// a packet with matching domain and scope="project" exists for that project,
    /// it is returned. Otherwise, falls back to the latest global packet with
    /// the matching domain.
    pub fn load_latest_by_domain(
        &self,
        domain: &str,
        project: Option<&str>,
    ) -> Result<Option<Packet>> {
        let all = self.list_all()?;

        // Collect matching packets
        let mut matches: Vec<&Packet> = all.iter().filter(|p| p.domain == domain).collect();

        if matches.is_empty() {
            return Ok(None);
        }

        // Sort by created_at descending (newest first)
        matches.sort_by(|a, b| b.created_at.cmp(&a.created_at));

        // If project specified, look for project-scoped packet first
        if let Some(proj) = project {
            for packet in &matches {
                if packet.scope == "project" && packet.project.as_deref() == Some(proj) {
                    return Ok(Some((*packet).clone()));
                }
            }
        }

        // Fall back to global packet or first project packet without project match
        for packet in &matches {
            if packet.scope == "global" {
                return Ok(Some((*packet).clone()));
            }
        }

        // Return the newest matching packet regardless of scope
        Ok(Some((*matches[0]).clone()))
    }

    pub fn list_all(&self) -> Result<Vec<Packet>> {
        let mut out = Vec::new();
        for scope in &["global", "project"] {
            let dir = scope_dir(&self.packets_dir, scope);
            if !dir.exists() {
                continue;
            }
            for entry in fs::read_dir(&dir)? {
                let entry = entry?;
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) != Some("json") {
                    continue;
                }
                if let Ok(raw) = fs::read_to_string(&path) {
                    if let Ok(packet) = serde_json::from_str::<Packet>(&raw) {
                        out.push(packet);
                    }
                }
            }
        }
        out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(out)
    }

    pub fn rename_project_refs(&self, old_project: &str, new_project: &str) -> Result<usize> {
        let mut updated = 0usize;
        for mut packet in self.list_all()? {
            if packet.project.as_deref() == Some(old_project) {
                packet.project = Some(new_project.to_string());
                self.save_packet(&packet)?;
                updated += 1;
            }
        }
        Ok(updated)
    }

    pub fn remove_domain(&self, domain: &str) -> Result<usize> {
        let packets: Vec<Packet> = self
            .list_all()?
            .into_iter()
            .filter(|packet| packet.domain == domain)
            .collect();
        let mut removed = 0usize;
        for packet in packets {
            let path = packet_path(&self.packets_dir, &packet.scope, &packet.id);
            if path.exists() {
                fs::remove_file(&path)
                    .with_context(|| format!("removing packet {}", path.display()))?;
                removed += 1;
            }
        }
        if removed > 0 {
            self.bump_generation();
        }
        Ok(removed)
    }

    // ── bbox_compile (create) ──────────────────────────────────────

    pub fn compile(&self, p: &CompileParams) -> Result<String> {
        let result = self.compile_inner(p);
        // Log the outcome regardless of success/failure. Event-log
        // I/O errors are swallowed inside append_event so they can't
        // mask the real result.
        match &result {
            Ok(packet) => self.log_compile_ok(packet),
            Err(e) => {
                let details = serde_json::json!({
                    "error": format!("{e:#}"),
                });
                let mut ev = PacketEvent::now("compile", "error").with_details(details);
                if !p.domain.trim().is_empty() {
                    ev = ev.with_domain(p.domain.clone());
                }
                self.append_event(&ev);
            }
        }
        result.map(|packet| {
            format!(
                "Packet {} compiled (domain={}, scope={}, rules={})",
                packet.id,
                packet.domain,
                packet.scope,
                packet.rules.len()
            )
        })
    }

    fn log_compile_ok(&self, packet: &Packet) {
        let mut refs = Vec::new();
        for rule in &packet.rules {
            collect_apply_refs(&rule.antecedent, &mut refs);
        }
        refs.sort();
        refs.dedup();
        let details = serde_json::json!({
            "rules_count": packet.rules.len(),
            "lattice_size": packet.classification_lattice.len(),
            "lattice": packet.classification_lattice,
            "referenced_packets": refs,
            "scope": packet.scope,
        });
        self.append_event(
            &PacketEvent::now("compile", "ok")
                .with_packet_id(packet.id.clone())
                .with_domain(packet.domain.clone())
                .with_details(details),
        );
    }

    /// Compile, but skip the write when an identical-content packet already
    /// exists for the same domain/scope/project. This is the boot-restore
    /// entry point: `restore_runtime_artifacts_from_catalog` re-compiles
    /// every active packet artifact on every daemon start, and the
    /// unconditional `compile` minted a fresh packet id each time — 33
    /// artifacts × N restarts grew the store to thousands of byte-identical
    /// files (see thread-935b467d).
    pub fn compile_idempotent(&self, p: &CompileParams) -> Result<CompileOutcome> {
        let packet = self.build_packet(p)?;
        if let Some(existing_id) = self.find_identical(&packet)? {
            return Ok(CompileOutcome::UnchangedExisting(existing_id));
        }
        self.save_packet(&packet)?;
        self.log_compile_ok(&packet);
        Ok(CompileOutcome::Created(packet.id))
    }

    /// Newest existing packet whose durable content matches `candidate`
    /// (same domain/scope/project, equal [`content_fingerprint`]).
    fn find_identical(&self, candidate: &Packet) -> Result<Option<String>> {
        let fp = content_fingerprint(candidate);
        // list_all is newest-first; first hit is the newest identical copy.
        for existing in self.list_all()? {
            if existing.domain == candidate.domain
                && existing.scope == candidate.scope
                && existing.project == candidate.project
                && content_fingerprint(&existing) == fp
            {
                return Ok(Some(existing.id));
            }
        }
        Ok(None)
    }

    fn compile_inner(&self, p: &CompileParams) -> Result<Packet> {
        let packet = self.build_packet(p)?;
        self.save_packet(&packet)?;
        Ok(packet)
    }

    fn build_packet(&self, p: &CompileParams) -> Result<Packet> {
        if p.domain.trim().is_empty() {
            anyhow::bail!("'domain' is required and cannot be empty");
        }

        // Resolve the classification lattice + prefix inference map.
        // Callers override per-domain; default is the review lattice.
        let lattice = p
            .classification_lattice
            .clone()
            .unwrap_or_else(review_lattice);
        if lattice.is_empty() {
            anyhow::bail!("'classification_lattice' cannot be empty");
        }
        let prefix_inference = p
            .prefix_inference
            .clone()
            .unwrap_or_else(review_prefix_inference);

        // Deserialize into RuleInput (classification is Option<String>);
        // materialize validates each rule's classification is in the
        // lattice, inferring from id prefix when the caller omitted it.
        // Explicit classification beats inferred.
        //
        // Unwrap stringified-JSON here so Codex-style first-attempts
        // (stringified arrays) succeed without a retry cycle.
        let mut rules_v = p.rules.clone();
        unwrap_jsonish(&mut rules_v);
        let inputs: Vec<RuleInput> = serde_json::from_value(rules_v)
            .context("'rules' must be a JSON array of {id, antecedent, consequent, classification?, emit?, confidence?, provenance?} objects")?;

        if inputs.is_empty() {
            anyhow::bail!("'rules' cannot be empty — at least one rule required");
        }

        let rules: Vec<Rule> = inputs
            .into_iter()
            .map(|ri| ri.materialize(&lattice, &prefix_inference))
            .collect::<Result<Vec<_>>>()?;

        // Phase-5 composition: verify every `Apply{packet_id}` reference
        // resolves to an existing packet. Catches typos and stale IDs at
        // compile time rather than silent false-returns at eval time.
        // Also check the referenced packet's classification lattice
        // contains every element of `expect` — otherwise the Apply can
        // never match.
        let mut refs = Vec::new();
        for rule in &rules {
            collect_apply_refs(&rule.antecedent, &mut refs);
        }
        for packet_id in &refs {
            let normalized = normalize_id(packet_id);
            let sub = self.load(&normalized).with_context(|| {
                format!(
                    "Apply references packet '{packet_id}' which is not in the store. \
                     Compile the referenced packet first, or check the ID."
                )
            })?;
            // Check expect values are in the sub-packet's lattice.
            check_apply_expect_against_lattice(
                &rule_antecedents_referencing(&rules, packet_id),
                &sub,
            )?;
        }

        let rank_table: BTreeMap<String, i64> = match &p.rank_table {
            Some(v) => {
                let mut vv = v.clone();
                unwrap_jsonish(&mut vv);
                serde_json::from_value(vv).context(
                    "'rank_table' must be an object mapping string keys to integer values",
                )?
            }
            None => BTreeMap::new(),
        };
        let threshold_table: BTreeMap<String, i64> = match &p.threshold_table {
            Some(v) => {
                let mut vv = v.clone();
                unwrap_jsonish(&mut vv);
                serde_json::from_value(vv).context(
                    "'threshold_table' must be an object mapping string keys to integer values",
                )?
            }
            None => BTreeMap::new(),
        };

        let scope = p.scope.as_deref().unwrap_or("global");
        if scope != "global" && scope != "project" {
            anyhow::bail!("scope must be 'global' or 'project'");
        }

        let now = Self::now_iso();
        let id = Self::gen_id();

        let packet = Packet {
            id: id.clone(),
            domain: p.domain.clone(),
            scope: scope.to_string(),
            project: p.project.clone(),
            rank_table,
            threshold_table,
            rank_lookup_key: p
                .rank_lookup_key
                .clone()
                .unwrap_or_else(default_rank_lookup_key),
            threshold_lookup_key: p
                .threshold_lookup_key
                .clone()
                .unwrap_or_else(default_threshold_lookup_key),
            classification_lattice: lattice,
            prefix_inference,
            rules,
            source_ids: p.source_ids.clone().unwrap_or_default(),
            self_audit_fidelity: None,
            created_at: now.clone(),
            updated_at: now,
            superseded_by: None,
            merged_from: Vec::new(),
        };

        Ok(packet)
    }

    /// Dry-run or apply duplicate-packet garbage collection.
    ///
    /// Groups packets by (scope, project, domain, content fingerprint) and
    /// keeps the newest of each group; older byte-identical copies are
    /// deletion candidates. A candidate referenced by any other packet's
    /// `Apply{packet_id}` antecedent is protected (kept) so composition
    /// references can never dangle. Sibling `.json.lock` files are removed
    /// with their packet, and orphaned lock files (packet json already gone,
    /// e.g. from `remove_domain`) are swept opportunistically.
    pub fn gc_duplicate_packets(&self, apply: bool) -> Result<PacketGcReport> {
        let all = self.list_all()?;

        // Ids referenced from any packet's Apply antecedents are protected.
        let mut referenced: HashSet<String> = HashSet::new();
        for packet in &all {
            let mut refs = Vec::new();
            for rule in &packet.rules {
                collect_apply_refs(&rule.antecedent, &mut refs);
            }
            for r in refs {
                referenced.insert(normalize_id(&r));
            }
        }

        // list_all is newest-first; the first packet seen per group is the
        // keeper, the rest are duplicate candidates.
        let mut keepers: HashMap<(String, Option<String>, String, String), String> = HashMap::new();
        let mut candidates: Vec<&Packet> = Vec::new();
        for packet in &all {
            let key = (
                packet.scope.clone(),
                packet.project.clone(),
                packet.domain.clone(),
                content_fingerprint(packet),
            );
            match keepers.entry(key) {
                std::collections::hash_map::Entry::Vacant(v) => {
                    v.insert(packet.id.clone());
                }
                std::collections::hash_map::Entry::Occupied(_) => candidates.push(packet),
            }
        }

        let mut deleted = 0usize;
        let mut protected_by_refs = 0usize;
        let mut per_domain: BTreeMap<String, usize> = BTreeMap::new();
        for packet in &candidates {
            if referenced.contains(&normalize_id(&packet.id)) {
                protected_by_refs += 1;
                continue;
            }
            if apply {
                let path = packet_path(&self.packets_dir, &packet.scope, &packet.id);
                if path.exists() {
                    fs::remove_file(&path)
                        .with_context(|| format!("removing packet {}", path.display()))?;
                }
                let lock_path = path.with_extension("json.lock");
                if lock_path.exists() {
                    let _ = fs::remove_file(&lock_path);
                }
            }
            deleted += 1;
            *per_domain.entry(packet.domain.clone()).or_insert(0) += 1;
        }

        // Sweep lock files whose packet json no longer exists.
        let mut orphan_locks_removed = 0usize;
        for scope in &["global", "project"] {
            let dir = scope_dir(&self.packets_dir, scope);
            if !dir.exists() {
                continue;
            }
            for entry in fs::read_dir(&dir)? {
                let path = entry?.path();
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                let Some(stem) = name.strip_suffix(".json.lock") else {
                    continue;
                };
                if !dir.join(format!("{stem}.json")).exists() {
                    if apply {
                        let _ = fs::remove_file(&path);
                    }
                    orphan_locks_removed += 1;
                }
            }
        }

        if apply && deleted > 0 {
            self.bump_generation();
            self.append_event(
                &PacketEvent::now("gc", "ok").with_details(serde_json::json!({
                    "deleted": deleted,
                    "duplicate_groups": per_domain.len(),
                    "protected_by_refs": protected_by_refs,
                    "orphan_locks_removed": orphan_locks_removed,
                })),
            );
        }

        Ok(PacketGcReport {
            applied: apply,
            scanned: all.len(),
            deleted,
            protected_by_refs,
            orphan_locks_removed,
            per_domain,
        })
    }

    // ── bbox_apply ─────────────────────────────────────────────────

    pub fn apply_tool(&self, p: &ApplyParams) -> Result<String> {
        let packet = match self.load(&p.packet_id) {
            Ok(pk) => pk,
            Err(e) => {
                self.append_event(
                    &PacketEvent::now("apply", "error")
                        .with_packet_id(normalize_id(&p.packet_id))
                        .with_details(serde_json::json!({"error": format!("{e:#}")})),
                );
                return Err(e);
            }
        };
        // Absorb stringified-JSON first-attempts from agents whose
        // clients serialize structured params as strings (see E12).
        let mut entity = p.entity.clone();
        unwrap_jsonish(&mut entity);
        let mode = p.mode.unwrap_or_default();
        match mode {
            ApplyMode::First => match apply_with(&packet, &entity, self) {
                Some(prediction) => {
                    self.append_event(
                        &PacketEvent::now("apply", "ok")
                            .with_packet_id(packet.id.clone())
                            .with_domain(packet.domain.clone())
                            .with_details(serde_json::json!({
                                "mode": "first",
                                "matched": true,
                                "rule_id": prediction.rule_id,
                                "classification": prediction.classification,
                            })),
                    );
                    Ok(serde_json::to_string_pretty(&serde_json::json!({
                        "packet_id": packet.id,
                        "mode": mode,
                        "match": true,
                        "rule_id": prediction.rule_id,
                        "classification": prediction.classification,
                        "consequent": prediction.consequent,
                        "confidence": prediction.confidence,
                    }))?)
                }
                None => {
                    self.append_event(
                        &PacketEvent::now("apply", "no_match")
                            .with_packet_id(packet.id.clone())
                            .with_domain(packet.domain.clone())
                            .with_details(serde_json::json!({"mode": "first"})),
                    );
                    Ok(serde_json::to_string_pretty(&serde_json::json!({
                        "packet_id": packet.id,
                        "mode": mode,
                        "match": false,
                        "consequent": serde_json::Value::Null,
                        "note": "no rule's antecedent matched the entity",
                    }))?)
                }
            },
            ApplyMode::All => {
                let result = apply_all_with(&packet, &entity, self);
                let outcome = if result.verdict.is_some() {
                    "ok"
                } else {
                    "no_match"
                };
                self.append_event(
                    &PacketEvent::now("apply", outcome)
                        .with_packet_id(packet.id.clone())
                        .with_domain(packet.domain.clone())
                        .with_details(serde_json::json!({
                            "mode": "all",
                            "verdict": result.verdict,
                            "finding_count": result.findings.len(),
                        })),
                );
                Ok(serde_json::to_string_pretty(&serde_json::json!({
                    "packet_id": result.packet_id,
                    "mode": mode,
                    "findings": result.findings,
                    "verdict": result.verdict,
                    "finding_count": result.findings.len(),
                }))?)
            }
        }
    }

    // ── bbox_audit ─────────────────────────────────────────────────

    pub fn audit_tool(&self, p: &AuditParams) -> Result<String> {
        let packet = match self.load(&p.packet_id) {
            Ok(pk) => pk,
            Err(e) => {
                self.append_event(
                    &PacketEvent::now("audit", "error")
                        .with_packet_id(normalize_id(&p.packet_id))
                        .with_details(serde_json::json!({"error": format!("{e:#}")})),
                );
                return Err(e);
            }
        };
        // Absorb stringified-JSON first-attempts — see apply_tool.
        let mut dataset = p.dataset.clone();
        unwrap_jsonish(&mut dataset);
        let mode = p.mode.unwrap_or_default();
        match mode {
            ApplyMode::First => {
                let report = verify_with(&packet, &dataset, self)?;
                let outcome = if report.fidelity >= 1.0 {
                    "ok"
                } else {
                    "low_fidelity"
                };
                self.append_event(
                    &PacketEvent::now("audit", outcome)
                        .with_packet_id(packet.id.clone())
                        .with_domain(packet.domain.clone())
                        .with_details(serde_json::json!({
                            "mode": "first",
                            "total": report.total,
                            "correct": report.correct,
                            "fidelity": report.fidelity,
                            "mismatch_count": report.mismatches.len(),
                            "uncovered_count": report.uncovered.len(),
                        })),
                );
                Ok(serde_json::to_string_pretty(&serde_json::json!({
                    "packet_id": packet.id,
                    "mode": mode,
                    "total": report.total,
                    "correct": report.correct,
                    "fidelity": report.fidelity,
                    "mismatches": report.mismatches,
                    "uncovered_count": report.uncovered.len(),
                }))?)
            }
            ApplyMode::All => {
                let report = verify_all_with(&packet, &dataset, self)?;
                let outcome = if report.fidelity >= 1.0 {
                    "ok"
                } else {
                    "low_fidelity"
                };
                self.append_event(
                    &PacketEvent::now("audit", outcome)
                        .with_packet_id(packet.id.clone())
                        .with_domain(packet.domain.clone())
                        .with_details(serde_json::json!({
                            "mode": "all",
                            "total": report.total,
                            "correct": report.correct,
                            "fidelity": report.fidelity,
                            "mismatch_count": report.mismatches.len(),
                        })),
                );
                Ok(serde_json::to_string_pretty(&serde_json::json!({
                    "packet_id": packet.id,
                    "mode": mode,
                    "total": report.total,
                    "correct": report.correct,
                    "fidelity": report.fidelity,
                    "mismatches": report.mismatches,
                }))?)
            }
        }
    }
}

impl Packets {
    /// One scanner pass. Reads the recent event log, flags candidates,
    /// writes one `op="repair_candidate"` event per candidate. Returns
    /// the list of candidates flagged this tick.
    ///
    /// Called on a timer by the main-process background task when
    /// `PACKET_SELF_HEAL_ENABLED=true`. Also exposed for direct
    /// invocation from tests / operators.
    pub fn scanner_step(&self, config: &ScannerConfig) -> Result<Vec<RepairCandidate>> {
        let now = crate::util::now_iso();
        self.scanner_step_at(config, &now)
    }

    fn scanner_step_at(
        &self,
        config: &ScannerConfig,
        now_iso: &str,
    ) -> Result<Vec<RepairCandidate>> {
        // Pull a generous slice of the event log. The default 50 is too
        // small for aggregation; 10_000 keeps I/O bounded while giving
        // enough history to compute rates over a 24h window even on
        // busy installations.
        let events = self.list_events(None, None, None, None, 10_000)?;
        let candidates = find_repair_candidates(&events, config, now_iso);

        for cand in &candidates {
            let mut details = serde_json::Map::new();
            details.insert("reasons".into(), serde_json::json!(cand.reasons));
            details.insert("apply_count".into(), serde_json::json!(cand.apply_count));
            details.insert(
                "no_match_count".into(),
                serde_json::json!(cand.no_match_count),
            );
            if let Some(r) = cand.no_match_rate {
                details.insert("no_match_rate".into(), serde_json::json!(r));
            }
            if let Some(f) = cand.fidelity {
                details.insert("fidelity".into(), serde_json::json!(f));
            }
            if let Some(ts) = &cand.last_audit_timestamp {
                details.insert("last_audit_timestamp".into(), serde_json::json!(ts));
            }
            details.insert(
                "window_hours".into(),
                serde_json::json!(config.window.as_secs() / 3600),
            );

            let mut ev = PacketEvent::now("repair_candidate", "flagged")
                .with_packet_id(cand.packet_id.clone());
            if let Some(d) = &cand.domain {
                ev = ev.with_domain(d.clone());
            }
            ev = ev.with_details(serde_json::Value::Object(details));
            self.append_event(&ev);
        }
        Ok(candidates)
    }
}

// ── Tests ─────────────────────────────────────────────────────────

mod apply;
mod ast;
mod audit;
mod coerce;
mod compile;
#[cfg(test)]
mod e8_tests;
mod events;
mod scanner;
#[cfg(test)]
mod test_support;

#[cfg(test)]
mod store_tests {
    use serde_json::json;

    use super::CompileParams;
    use super::test_support::tmp_packets;

    #[test]
    fn load_resolves_domain_prefix_to_latest() {
        let (_dir, store) = tmp_packets();
        // Compile two packets in the same domain.
        let p1 = CompileParams {
            domain: "demo/routing".into(),
            rules: json!([{
                "id": "fail_default",
                "antecedent": {"op": "True"},
                "consequent": "REJECT"
            }]),
            classification_lattice: None,
            prefix_inference: None,
            rank_table: None,
            threshold_table: None,
            rank_lookup_key: None,
            threshold_lookup_key: None,
            source_ids: None,
            scope: Some("global".into()),
            project: None,
        };
        let msg1 = store.compile(&p1).unwrap();
        // Slight delay so created_at differs reliably.
        std::thread::sleep(std::time::Duration::from_millis(20));
        let msg2 = store.compile(&p1).unwrap();
        let id1 = msg1.split_whitespace().nth(1).unwrap();
        let id2 = msg2.split_whitespace().nth(1).unwrap();
        assert_ne!(id1, id2, "two compiles of same domain -> distinct ids");
        // domain: prefix resolves to the latest.
        let resolved = store.load("domain:demo/routing").unwrap();
        assert_eq!(resolved.id, id2);
        // Bare id still works.
        let resolved_old = store.load(id1).unwrap();
        assert_eq!(resolved_old.id, id1);
        // Unknown domain errors clearly.
        let err = store.load("domain:does-not-exist").unwrap_err().to_string();
        assert!(err.contains("no packet found for domain"), "got: {err}");
    }
}
