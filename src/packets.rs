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

use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rmcp::schemars;
use serde::{Deserialize, Serialize};

// ── MCP parameter structs ─────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CompileParams {
    /// Short domain label (e.g. "auth-matrix", "retry-policy"). Used for
    /// filtering and display; free-form.
    pub domain: String,
    /// Rules serialized as a JSON array. Each rule is
    /// `{ id, antecedent: <Predicate>, consequent: <Value>, confidence?: f32,
    ///   provenance?: [string] }`. Predicate AST operators are documented
    /// in the module-level doc comment for this tool.
    pub rules: serde_json::Value,
    /// Named lookup table: role → rank. Optional; used by
    /// `RankGeFieldThreshold` predicates. Entity must carry the role key in
    /// the field named by `rank_lookup_key` (default: `"role"`).
    #[serde(default)]
    pub rank_table: Option<serde_json::Value>,
    /// Named lookup table: resource → threshold. Optional; paired with
    /// `threshold_lookup_key` (default: `"resource"`).
    #[serde(default)]
    pub threshold_table: Option<serde_json::Value>,
    /// Entity field whose value indexes `rank_table`. Default: `"role"`.
    #[serde(default)]
    pub rank_lookup_key: Option<String>,
    /// Entity field whose value indexes `threshold_table`. Default: `"resource"`.
    #[serde(default)]
    pub threshold_lookup_key: Option<String>,
    /// Stable source references (e.g. knowledge entry IDs, transcript spans).
    /// Not validated — free-form provenance strings.
    #[serde(default)]
    pub source_ids: Option<Vec<String>>,
    /// global or project (default: global)
    #[serde(default)]
    pub scope: Option<String>,
    /// Project path for project-scoped packets
    #[serde(default)]
    pub project: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ApplyParams {
    /// Packet ID. Canonical `packet-<8hex>`; bare 8-hex accepted as fallback.
    #[schemars(regex(pattern = r"^(packet-)?[0-9a-f]{8}$"))]
    pub packet_id: String,
    /// Entity to evaluate, as a flat JSON object of field → value. Rules
    /// evaluate top-to-bottom; first matching antecedent wins. If no rule
    /// matches, `consequent` is null.
    pub entity: serde_json::Value,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AuditParams {
    #[schemars(regex(pattern = r"^(packet-)?[0-9a-f]{8}$"))]
    pub packet_id: String,
    /// JSON array of `{entity, expected}` pairs. Evaluator compares each
    /// entity's predicted consequent to `expected`. Report lists mismatches
    /// and returns a fidelity ratio.
    pub dataset: serde_json::Value,
}

// ── Value (consequent + entity field type) ───────────────────────

/// Field values. Serialized untagged so packets read cleanly:
/// `"ALLOW"`, `42`, `true`. Ordering is defined for BTreeMap storage
/// of lookup tables only.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Value {
    Bool(bool),
    Int(i64),
    String(String),
}

impl Value {
    fn from_json(v: &serde_json::Value) -> Option<Value> {
        match v {
            serde_json::Value::Bool(b) => Some(Value::Bool(*b)),
            serde_json::Value::Number(n) => n.as_i64().map(Value::Int),
            serde_json::Value::String(s) => Some(Value::String(s.clone())),
            _ => None,
        }
    }
}

// ── Predicate AST ────────────────────────────────────────────────

/// The canonical predicate vocabulary. Rule antecedents are trees of
/// these nodes; evaluation is a pure function of `(node, entity)`. The
/// serde tag `op` matches the JSON form produced in E11.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op")]
pub enum Predicate {
    /// `entity[field] == value`
    Eq { field: String, value: Value },
    /// `entity[field] >= value` (integer)
    Ge { field: String, value: i64 },
    Gt { field: String, value: i64 },
    Le { field: String, value: i64 },
    Lt { field: String, value: i64 },
    /// Common auth-style pattern: `entity[rank_field] >= entity[threshold_field]`.
    /// Field values must be integers after lookup-table resolution.
    RankGeFieldThreshold {
        rank_field: String,
        threshold_field: String,
    },
    All { args: Vec<Predicate> },
    Any { args: Vec<Predicate> },
    Not { arg: Box<Predicate> },
    #[serde(rename = "True")]
    AlwaysTrue {},
    #[serde(rename = "False")]
    AlwaysFalse {},
}

// ── Rule ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rule {
    pub id: String,
    pub antecedent: Predicate,
    pub consequent: Value,
    #[serde(default = "default_confidence")]
    pub confidence: f32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provenance: Vec<String>,
}

fn default_confidence() -> f32 {
    1.0
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

fn default_rank_lookup_key() -> String {
    "role".to_string()
}

fn default_threshold_lookup_key() -> String {
    "resource".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Prediction {
    pub rule_id: String,
    pub consequent: Value,
    pub confidence: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FidelityReport {
    pub total: usize,
    pub correct: usize,
    pub fidelity: f32,
    pub mismatches: Vec<Mismatch>,
    pub uncovered: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mismatch {
    pub entity: serde_json::Value,
    pub expected: Value,
    pub predicted: Option<Value>,
    pub rule_id: Option<String>,
}

// ── Evaluator (deterministic, no LLM) ────────────────────────────

/// Augment `entity` with fields derived from `packet.rank_table` /
/// `packet.threshold_table` lookups. Pure function over the entity
/// map; does not mutate the packet.
fn resolve_entity(packet: &Packet, entity: &serde_json::Map<String, serde_json::Value>)
    -> serde_json::Map<String, serde_json::Value>
{
    let mut resolved = entity.clone();

    // rank lookup: entity[rank_lookup_key] is a name → packet.rank_table[name] → int
    if !packet.rank_table.is_empty() {
        if let Some(serde_json::Value::String(key)) = entity.get(&packet.rank_lookup_key) {
            if let Some(rank) = packet.rank_table.get(key) {
                resolved.insert(
                    format!("{}_rank", packet.rank_lookup_key),
                    serde_json::Value::Number((*rank).into()),
                );
            }
        }
    }

    if !packet.threshold_table.is_empty() {
        if let Some(serde_json::Value::String(key)) = entity.get(&packet.threshold_lookup_key) {
            if let Some(threshold) = packet.threshold_table.get(key) {
                // Convention: res_threshold (from "resource" → "res_threshold")
                let field_name = if packet.threshold_lookup_key == "resource" {
                    "res_threshold".to_string()
                } else {
                    format!("{}_threshold", packet.threshold_lookup_key)
                };
                resolved.insert(field_name, serde_json::Value::Number((*threshold).into()));
            }
        }
    }

    resolved
}

fn entity_get(entity: &serde_json::Map<String, serde_json::Value>, field: &str) -> Option<Value> {
    entity.get(field).and_then(Value::from_json)
}

fn entity_int(entity: &serde_json::Map<String, serde_json::Value>, field: &str) -> Option<i64> {
    entity.get(field).and_then(|v| v.as_i64())
}

/// Evaluate a predicate against a resolved entity. Pure function.
fn eval_predicate(
    p: &Predicate,
    entity: &serde_json::Map<String, serde_json::Value>,
) -> bool {
    match p {
        Predicate::AlwaysTrue {} => true,
        Predicate::AlwaysFalse {} => false,
        Predicate::Eq { field, value } => entity_get(entity, field).as_ref() == Some(value),
        Predicate::Ge { field, value } => {
            entity_int(entity, field).map(|v| v >= *value).unwrap_or(false)
        }
        Predicate::Gt { field, value } => {
            entity_int(entity, field).map(|v| v > *value).unwrap_or(false)
        }
        Predicate::Le { field, value } => {
            entity_int(entity, field).map(|v| v <= *value).unwrap_or(false)
        }
        Predicate::Lt { field, value } => {
            entity_int(entity, field).map(|v| v < *value).unwrap_or(false)
        }
        Predicate::RankGeFieldThreshold {
            rank_field,
            threshold_field,
        } => {
            match (entity_int(entity, rank_field), entity_int(entity, threshold_field)) {
                (Some(r), Some(t)) => r >= t,
                _ => false,
            }
        }
        Predicate::All { args } => args.iter().all(|arg| eval_predicate(arg, entity)),
        Predicate::Any { args } => args.iter().any(|arg| eval_predicate(arg, entity)),
        Predicate::Not { arg } => !eval_predicate(arg, entity),
    }
}

/// Apply a packet to an entity. Returns the first matching rule's
/// prediction, or None when no rule matches.
pub fn apply(packet: &Packet, entity: &serde_json::Value) -> Option<Prediction> {
    let entity_obj = entity.as_object()?;
    let resolved = resolve_entity(packet, entity_obj);

    for rule in &packet.rules {
        if eval_predicate(&rule.antecedent, &resolved) {
            return Some(Prediction {
                rule_id: rule.id.clone(),
                consequent: rule.consequent.clone(),
                confidence: rule.confidence,
            });
        }
    }
    None
}

/// Apply packet to every entry in `dataset`. Dataset is a JSON array of
/// `{entity, expected}` pairs. Returns a fidelity report.
pub fn verify(packet: &Packet, dataset: &serde_json::Value) -> Result<FidelityReport> {
    let rows = dataset
        .as_array()
        .context("dataset must be a JSON array of {entity, expected} objects")?;

    let mut total = 0usize;
    let mut correct = 0usize;
    let mut mismatches = Vec::new();
    let mut uncovered = Vec::new();

    for row in rows {
        let entity = row.get("entity").cloned().unwrap_or(serde_json::Value::Null);
        let expected_json = row
            .get("expected")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let expected = match Value::from_json(&expected_json) {
            Some(v) => v,
            None => continue, // skip malformed rows
        };

        total += 1;
        match apply(packet, &entity) {
            Some(prediction) if prediction.consequent == expected => {
                correct += 1;
            }
            Some(prediction) => {
                mismatches.push(Mismatch {
                    entity: entity.clone(),
                    expected,
                    predicted: Some(prediction.consequent),
                    rule_id: Some(prediction.rule_id),
                });
            }
            None => {
                uncovered.push(entity.clone());
            }
        }
    }

    let fidelity = if total == 0 {
        0.0
    } else {
        correct as f32 / total as f32
    };

    Ok(FidelityReport {
        total,
        correct,
        fidelity,
        mismatches,
        uncovered,
    })
}

// ── Storage ───────────────────────────────────────────────────────

fn packets_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("packets")
}

fn scope_dir(state_dir: &Path, scope: &str) -> PathBuf {
    packets_dir(state_dir).join(scope)
}

fn packet_path(state_dir: &Path, scope: &str, id: &str) -> PathBuf {
    scope_dir(state_dir, scope).join(format!("{id}.json"))
}

/// Store handle. One `Packets` per daemon; constructs/lists from the
/// state directory directly (one file per packet, unlike notes which
/// share a single JSON).
pub struct Packets {
    state_dir: PathBuf,
}

impl Packets {
    pub fn open(state_dir: &Path) -> Result<Self> {
        fs::create_dir_all(packets_dir(state_dir))
            .with_context(|| format!("creating {}", packets_dir(state_dir).display()))?;
        Ok(Self {
            state_dir: state_dir.to_path_buf(),
        })
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
        crate::util::now_iso()
    }

    fn save_packet(&self, packet: &Packet) -> Result<()> {
        let dir = scope_dir(&self.state_dir, &packet.scope);
        fs::create_dir_all(&dir)
            .with_context(|| format!("creating {}", dir.display()))?;
        let path = packet_path(&self.state_dir, &packet.scope, &packet.id);
        let tmp = path.with_extension("json.tmp");
        let raw = serde_json::to_string_pretty(packet)?;
        let mut f = fs::File::create(&tmp)?;
        f.write_all(raw.as_bytes())?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Search both scopes for a packet by canonical ID or bare suffix.
    pub fn load(&self, id: &str) -> Result<Packet> {
        let needle = normalize_id(id);
        for scope in &["global", "project"] {
            let path = packet_path(&self.state_dir, scope, &needle);
            if path.exists() {
                let raw = fs::read_to_string(&path)
                    .with_context(|| format!("reading {}", path.display()))?;
                let packet: Packet = serde_json::from_str(&raw)
                    .with_context(|| format!("parsing {}", path.display()))?;
                return Ok(packet);
            }
        }
        anyhow::bail!(
            "Packet not found: {id} (expected `packet-<8hex>`, e.g. `packet-a1b2c3d4`)"
        )
    }

    pub fn list_all(&self) -> Result<Vec<Packet>> {
        let mut out = Vec::new();
        for scope in &["global", "project"] {
            let dir = scope_dir(&self.state_dir, scope);
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

    // ── bbox_compile (create) ──────────────────────────────────────

    pub fn compile(&self, p: &CompileParams) -> Result<String> {
        if p.domain.trim().is_empty() {
            anyhow::bail!("'domain' is required and cannot be empty");
        }

        let rules: Vec<Rule> = serde_json::from_value(p.rules.clone())
            .context("'rules' must be a JSON array of {id, antecedent, consequent, confidence?, provenance?} objects")?;

        if rules.is_empty() {
            anyhow::bail!("'rules' cannot be empty — at least one rule required");
        }

        let rank_table: BTreeMap<String, i64> = match &p.rank_table {
            Some(v) => serde_json::from_value(v.clone())
                .context("'rank_table' must be an object mapping string keys to integer values")?,
            None => BTreeMap::new(),
        };
        let threshold_table: BTreeMap<String, i64> = match &p.threshold_table {
            Some(v) => serde_json::from_value(v.clone())
                .context("'threshold_table' must be an object mapping string keys to integer values")?,
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
            rules,
            source_ids: p.source_ids.clone().unwrap_or_default(),
            self_audit_fidelity: None,
            created_at: now.clone(),
            updated_at: now,
            superseded_by: None,
            merged_from: Vec::new(),
        };

        self.save_packet(&packet)?;

        Ok(format!(
            "Packet {id} compiled (domain={}, scope={}, rules={})",
            packet.domain,
            packet.scope,
            packet.rules.len()
        ))
    }

    // ── bbox_apply ─────────────────────────────────────────────────

    pub fn apply_tool(&self, p: &ApplyParams) -> Result<String> {
        let packet = self.load(&p.packet_id)?;
        match apply(&packet, &p.entity) {
            Some(prediction) => Ok(serde_json::to_string_pretty(&serde_json::json!({
                "packet_id": packet.id,
                "match": true,
                "rule_id": prediction.rule_id,
                "consequent": prediction.consequent,
                "confidence": prediction.confidence,
            }))?),
            None => Ok(serde_json::to_string_pretty(&serde_json::json!({
                "packet_id": packet.id,
                "match": false,
                "consequent": serde_json::Value::Null,
                "note": "no rule's antecedent matched the entity",
            }))?),
        }
    }

    // ── bbox_audit ─────────────────────────────────────────────────

    pub fn audit_tool(&self, p: &AuditParams) -> Result<String> {
        let packet = self.load(&p.packet_id)?;
        let report = verify(&packet, &p.dataset)?;
        Ok(serde_json::to_string_pretty(&serde_json::json!({
            "packet_id": packet.id,
            "total": report.total,
            "correct": report.correct,
            "fidelity": report.fidelity,
            "mismatches": report.mismatches,
            "uncovered_count": report.uncovered.len(),
        }))?)
    }
}

/// Accept `packet-<8hex>` or bare `<8hex>`.
fn normalize_id(id: &str) -> String {
    if id.starts_with("packet-") {
        id.to_string()
    } else {
        format!("packet-{id}")
    }
}

// ── Tests ─────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn tmp_packets() -> (TempDir, Packets) {
        let dir = TempDir::new().unwrap();
        let packets = Packets::open(dir.path()).unwrap();
        (dir, packets)
    }

    /// Build the E8 authorization matrix packet (the merged Gemini-style
    /// encoding from thread-0b20e854) as a typed Rust value. This is the
    /// definitional round-trip: if this packet evaluates faithfully over
    /// the matrix, the primitive works end-to-end.
    fn e8_auth_packet() -> Packet {
        let now = Packets::now_iso();

        // Rank table: role → rank (from the E8 merged packet)
        let rank_table: BTreeMap<String, i64> = [
            ("auditor", 0),
            ("reader", 1),
            ("editor", 2),
            ("owner", 3),
            ("admin", 4),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();

        // Threshold table: resource → threshold
        let threshold_table: BTreeMap<String, i64> = [
            ("public", 1),
            ("team", 2),
            ("private", 3),
            ("billing", 3),
            ("archived", 4),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();

        // Rules: anomalies first, then generals.
        let rules = vec![
            // Anomalies (read)
            Rule {
                id: "anom_reader_get_team".into(),
                antecedent: Predicate::All {
                    args: vec![
                        Predicate::Eq {
                            field: "role".into(),
                            value: Value::String("reader".into()),
                        },
                        Predicate::Eq {
                            field: "method".into(),
                            value: Value::String("GET".into()),
                        },
                        Predicate::Eq {
                            field: "resource".into(),
                            value: Value::String("team".into()),
                        },
                    ],
                },
                consequent: Value::String("DENY".into()),
                confidence: 1.0,
                provenance: vec![],
            },
            Rule {
                id: "anom_auditor_get_private".into(),
                antecedent: Predicate::All {
                    args: vec![
                        Predicate::Eq {
                            field: "role".into(),
                            value: Value::String("auditor".into()),
                        },
                        Predicate::Eq {
                            field: "method".into(),
                            value: Value::String("GET".into()),
                        },
                        Predicate::Eq {
                            field: "resource".into(),
                            value: Value::String("private".into()),
                        },
                    ],
                },
                consequent: Value::String("DENY".into()),
                confidence: 1.0,
                provenance: vec![],
            },
            // Anomalies (write)
            Rule {
                id: "anom_admin_delete_billing".into(),
                antecedent: Predicate::All {
                    args: vec![
                        Predicate::Eq {
                            field: "role".into(),
                            value: Value::String("admin".into()),
                        },
                        Predicate::Eq {
                            field: "method".into(),
                            value: Value::String("DELETE".into()),
                        },
                        Predicate::Eq {
                            field: "resource".into(),
                            value: Value::String("billing".into()),
                        },
                    ],
                },
                consequent: Value::String("DENY".into()),
                confidence: 1.0,
                provenance: vec![],
            },
            Rule {
                id: "anom_owner_patch_public".into(),
                antecedent: Predicate::All {
                    args: vec![
                        Predicate::Eq {
                            field: "role".into(),
                            value: Value::String("owner".into()),
                        },
                        Predicate::Eq {
                            field: "method".into(),
                            value: Value::String("PATCH".into()),
                        },
                        Predicate::Eq {
                            field: "resource".into(),
                            value: Value::String("public".into()),
                        },
                    ],
                },
                consequent: Value::String("DENY".into()),
                confidence: 1.0,
                provenance: vec![],
            },
            Rule {
                id: "anom_editor_post_archived".into(),
                antecedent: Predicate::All {
                    args: vec![
                        Predicate::Eq {
                            field: "role".into(),
                            value: Value::String("editor".into()),
                        },
                        Predicate::Eq {
                            field: "method".into(),
                            value: Value::String("POST".into()),
                        },
                        Predicate::Eq {
                            field: "resource".into(),
                            value: Value::String("archived".into()),
                        },
                    ],
                },
                consequent: Value::String("ALLOW".into()),
                confidence: 1.0,
                provenance: vec![],
            },
            // GET default allow (after GET exceptions above)
            Rule {
                id: "get_default_allow".into(),
                antecedent: Predicate::Eq {
                    field: "method".into(),
                    value: Value::String("GET".into()),
                },
                consequent: Value::String("ALLOW".into()),
                confidence: 1.0,
                provenance: vec![],
            },
            // Write default: allow iff role_rank >= res_threshold
            Rule {
                id: "write_rank_ge_threshold".into(),
                antecedent: Predicate::RankGeFieldThreshold {
                    rank_field: "role_rank".into(),
                    threshold_field: "res_threshold".into(),
                },
                consequent: Value::String("ALLOW".into()),
                confidence: 1.0,
                provenance: vec![],
            },
            // Catch-all deny
            Rule {
                id: "default_deny".into(),
                antecedent: Predicate::AlwaysTrue {},
                consequent: Value::String("DENY".into()),
                confidence: 1.0,
                provenance: vec![],
            },
        ];

        Packet {
            id: "packet-e8test01".into(),
            domain: "e8-auth-matrix".into(),
            scope: "global".into(),
            project: None,
            rank_table,
            threshold_table,
            rank_lookup_key: "role".into(),
            threshold_lookup_key: "resource".into(),
            rules,
            source_ids: vec!["thread-0b20e854".into()],
            self_audit_fidelity: None,
            created_at: now.clone(),
            updated_at: now,
            superseded_by: None,
            merged_from: vec![],
        }
    }

    /// Ground truth: the 5 hidden anomalies used in the E8 matrix.
    fn ground_truth_allow(role: &str, method: &str, resource: &str) -> Value {
        // Anomaly lookup
        let anom = [
            ("reader", "GET", "team", "DENY"),
            ("auditor", "GET", "private", "DENY"),
            ("admin", "DELETE", "billing", "DENY"),
            ("owner", "PATCH", "public", "DENY"),
            ("editor", "POST", "archived", "ALLOW"),
        ];
        for (r, m, res, v) in anom {
            if role == r && method == m && resource == res {
                return Value::String(v.to_string());
            }
        }

        // Otherwise: GET default ALLOW, write → rank gate
        if method == "GET" {
            return Value::String("ALLOW".into());
        }

        let rank = match role {
            "auditor" => 0,
            "reader" => 1,
            "editor" => 2,
            "owner" => 3,
            "admin" => 4,
            _ => unreachable!(),
        };
        let threshold = match resource {
            "public" => 1,
            "team" => 2,
            "private" | "billing" => 3,
            "archived" => 4,
            _ => unreachable!(),
        };
        if rank >= threshold {
            Value::String("ALLOW".into())
        } else {
            Value::String("DENY".into())
        }
    }

    #[test]
    fn e8_packet_round_trips_full_matrix() {
        let packet = e8_auth_packet();
        let roles = ["reader", "editor", "auditor", "admin", "owner"];
        let methods = ["GET", "POST", "PUT", "DELETE", "PATCH"];
        let resources = ["public", "team", "private", "archived", "billing"];

        let mut correct = 0;
        let mut total = 0;
        let mut mismatches: Vec<String> = Vec::new();

        for role in &roles {
            for method in &methods {
                for resource in &resources {
                    let entity = json!({
                        "role": role,
                        "method": method,
                        "resource": resource,
                    });
                    let expected = ground_truth_allow(role, method, resource);
                    total += 1;
                    match apply(&packet, &entity) {
                        Some(p) if p.consequent == expected => correct += 1,
                        Some(p) => mismatches.push(format!(
                            "({role},{method},{resource}) expected={:?} got={:?} rule={}",
                            expected, p.consequent, p.rule_id
                        )),
                        None => mismatches.push(format!(
                            "({role},{method},{resource}) expected={:?} got=UNMATCHED",
                            expected
                        )),
                    }
                }
            }
        }

        assert_eq!(total, 125, "125 cells total");
        assert_eq!(
            correct, 125,
            "Expected 125/125, got {correct}/125. Mismatches:\n{}",
            mismatches.join("\n")
        );
    }

    #[test]
    fn e8_packet_extrapolates_to_new_role_and_new_resource() {
        // Same packet, but we add "contributor" and "staging" to the
        // lookup tables. The rules themselves DO NOT mention these
        // names — if they still produce correct answers, the packet
        // genuinely encoded laws rather than per-role tables.
        let mut packet = e8_auth_packet();
        packet.rank_table.insert("contributor".into(), 2);
        packet.threshold_table.insert("staging".into(), 2);

        // 15 cells mirroring the experiment's extrapolation set.
        let cases: &[(&str, &str, &str, &str)] = &[
            ("contributor", "GET", "public", "ALLOW"),
            ("contributor", "GET", "team", "ALLOW"),
            ("contributor", "POST", "team", "ALLOW"),
            ("contributor", "POST", "private", "DENY"),
            ("contributor", "DELETE", "archived", "DENY"),
            ("contributor", "POST", "billing", "DENY"),
            ("contributor", "PATCH", "public", "ALLOW"),
            ("contributor", "PUT", "team", "ALLOW"),
            ("contributor", "DELETE", "private", "DENY"),
            ("contributor", "GET", "billing", "ALLOW"),
            ("editor", "POST", "staging", "ALLOW"),
            ("reader", "DELETE", "staging", "DENY"),
            ("auditor", "GET", "staging", "ALLOW"),
            ("admin", "PATCH", "staging", "ALLOW"),
            ("contributor", "POST", "staging", "ALLOW"),
        ];

        let mut misses = Vec::new();
        for (role, method, resource, expected) in cases {
            let entity = json!({
                "role": role,
                "method": method,
                "resource": resource,
            });
            let expected_val = Value::String(expected.to_string());
            match apply(&packet, &entity) {
                Some(p) if p.consequent == expected_val => {}
                other => misses.push(format!(
                    "({role},{method},{resource}) expected={expected} got={:?}",
                    other
                )),
            }
        }

        assert!(
            misses.is_empty(),
            "Expected 15/15, misses: \n{}",
            misses.join("\n")
        );
    }

    #[test]
    fn save_load_round_trip() {
        let (_dir, store) = tmp_packets();
        let packet = e8_auth_packet();
        store.save_packet(&packet).unwrap();

        let loaded = store.load(&packet.id).unwrap();
        assert_eq!(loaded.id, packet.id);
        assert_eq!(loaded.rules.len(), packet.rules.len());
        // Evaluate a representative cell after round-trip
        let entity = json!({
            "role": "admin",
            "method": "DELETE",
            "resource": "billing",
        });
        let prediction = apply(&loaded, &entity).unwrap();
        assert_eq!(prediction.consequent, Value::String("DENY".into()));
        assert_eq!(prediction.rule_id, "anom_admin_delete_billing");
    }

    #[test]
    fn compile_tool_happy_path() {
        let (_dir, store) = tmp_packets();
        // Minimal 2-rule packet via the public MCP surface.
        let params = CompileParams {
            domain: "minimal-test".to_string(),
            rules: json!([
                {
                    "id": "always_allow_get",
                    "antecedent": {"op": "Eq", "field": "method", "value": "GET"},
                    "consequent": "ALLOW",
                    "confidence": 1.0
                },
                {
                    "id": "default_deny",
                    "antecedent": {"op": "True"},
                    "consequent": "DENY",
                    "confidence": 1.0
                }
            ]),
            rank_table: None,
            threshold_table: None,
            rank_lookup_key: None,
            threshold_lookup_key: None,
            source_ids: None,
            scope: Some("global".into()),
            project: None,
        };
        let msg = store.compile(&params).unwrap();
        assert!(msg.contains("Packet packet-"));
        assert!(msg.contains("minimal-test"));

        // Verify listing works
        let packets = store.list_all().unwrap();
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].rules.len(), 2);
    }

    #[test]
    fn apply_tool_and_audit_tool() {
        let (_dir, store) = tmp_packets();
        let packet = e8_auth_packet();
        store.save_packet(&packet).unwrap();

        // apply
        let apply_params = ApplyParams {
            packet_id: packet.id.clone(),
            entity: json!({
                "role": "reader",
                "method": "GET",
                "resource": "team",
            }),
        };
        let out = store.apply_tool(&apply_params).unwrap();
        assert!(out.contains("\"consequent\": \"DENY\""));
        assert!(out.contains("anom_reader_get_team"));

        // audit
        let dataset = json!([
            {"entity": {"role": "reader", "method": "GET", "resource": "public"}, "expected": "ALLOW"},
            {"entity": {"role": "reader", "method": "GET", "resource": "team"}, "expected": "DENY"},
            {"entity": {"role": "editor", "method": "POST", "resource": "archived"}, "expected": "ALLOW"},
            {"entity": {"role": "owner", "method": "DELETE", "resource": "billing"}, "expected": "ALLOW"},
            {"entity": {"role": "auditor", "method": "DELETE", "resource": "team"}, "expected": "DENY"},
        ]);
        let audit_params = AuditParams {
            packet_id: packet.id.clone(),
            dataset,
        };
        let report_text = store.audit_tool(&audit_params).unwrap();
        assert!(report_text.contains("\"total\": 5"));
        assert!(report_text.contains("\"correct\": 5"));
        assert!(report_text.contains("\"fidelity\": 1.0"));
    }

    #[test]
    fn audit_flags_mismatches() {
        let (_dir, store) = tmp_packets();
        let packet = e8_auth_packet();
        store.save_packet(&packet).unwrap();

        // One entry has a deliberately wrong expected value to exercise
        // the mismatch path.
        let dataset = json!([
            {"entity": {"role": "reader", "method": "GET", "resource": "public"}, "expected": "ALLOW"},
            {"entity": {"role": "reader", "method": "GET", "resource": "team"}, "expected": "ALLOW"},
        ]);
        let report_text = store
            .audit_tool(&AuditParams {
                packet_id: packet.id.clone(),
                dataset,
            })
            .unwrap();
        assert!(report_text.contains("\"total\": 2"));
        assert!(report_text.contains("\"correct\": 1"));
    }

    #[test]
    fn missing_packet_errors_clearly() {
        let (_dir, store) = tmp_packets();
        let err = store
            .apply_tool(&ApplyParams {
                packet_id: "packet-deadbeef".into(),
                entity: json!({}),
            })
            .unwrap_err()
            .to_string();
        assert!(err.contains("not found"));
    }

    #[test]
    fn predicate_serde_matches_e11_format() {
        // The E11 experiment output rules in this exact JSON shape.
        // If this round-trips, our AST is compatible with what LLMs
        // actually produce.
        let json_rule = json!({
            "op": "All",
            "args": [
                {"op": "Eq", "field": "role", "value": "admin"},
                {"op": "Eq", "field": "method", "value": "DELETE"},
                {"op": "Eq", "field": "resource", "value": "billing"}
            ]
        });
        let p: Predicate = serde_json::from_value(json_rule.clone()).unwrap();
        let back = serde_json::to_value(&p).unwrap();
        assert_eq!(back, json_rule);
    }
}
