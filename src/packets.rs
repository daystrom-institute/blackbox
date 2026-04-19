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

/// Apply modes. `First` returns the first matching rule (classification
/// use case); `All` returns all findings + aggregate verdict (review use
/// case). Typed enum instead of a `String` field per the project's
/// stringly-typed-avoidance convention — bros called this out in the
/// phase-2 review and they were right.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, strum::EnumString, strum::AsRefStr)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum ApplyMode {
    First,
    All,
}

impl Default for ApplyMode {
    fn default() -> Self {
        ApplyMode::First
    }
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ApplyParams {
    /// Packet ID. Canonical `packet-<8hex>`; bare 8-hex accepted as fallback.
    #[schemars(regex(pattern = r"^(packet-)?[0-9a-f]{8}$"))]
    pub packet_id: String,
    /// Entity to evaluate, as a flat JSON object of field → value. Rules
    /// evaluate top-to-bottom; first matching antecedent wins by default.
    /// If no rule matches, `consequent` is null.
    pub entity: serde_json::Value,
    /// `"first"` (default) returns only the first matching rule; `"all"`
    /// evaluates every rule independently and returns all findings plus
    /// an aggregate verdict (Fail > Flag > Manual > Pass > Info). Use
    /// `"all"` for review-style workflows where multiple flags should
    /// surface in a single pass.
    #[serde(default)]
    pub mode: Option<ApplyMode>,
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
/// `"ALLOW"`, `42`, `true`. PartialEq over floats uses bitwise equality
/// on f64 bits to satisfy Eq/Ord bounds; rule predicates use the
/// numeric comparison ops (`Ge`/`Gt`/`Le`/`Lt`/`GeF`/`GtF`/...) rather
/// than reaching for Eq on a float anyway.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Value {
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        use Value::*;
        match (self, other) {
            (Bool(a), Bool(b)) => a == b,
            (Int(a), Int(b)) => a == b,
            (String(a), String(b)) => a == b,
            (Float(a), Float(b)) => a.to_bits() == b.to_bits(),
            // Cross-numeric: treat 1 == 1.0 as equal to keep rule Eq
            // predicates useful when JSON round-trips widen int → float.
            (Int(a), Float(b)) | (Float(b), Int(a)) => (*a as f64) == *b,
            _ => false,
        }
    }
}

impl Value {
    fn from_json(v: &serde_json::Value) -> Option<Value> {
        match v {
            serde_json::Value::Bool(b) => Some(Value::Bool(*b)),
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Some(Value::Int(i))
                } else {
                    n.as_f64().map(Value::Float)
                }
            }
            serde_json::Value::String(s) => Some(Value::String(s.clone())),
            _ => None,
        }
    }
}

// ── Predicate AST ────────────────────────────────────────────────

/// The canonical predicate vocabulary. Rule antecedents are trees of
/// these nodes; evaluation is a pure function of `(node, entity)`. The
/// serde tag `op` matches the JSON form produced in E11.
///
/// Phase-2 additions (convergent adversarial-bro feedback from
/// thread-0b20e854):
/// - Applicability: `IsPresent`, `IsAbsent` — gate rules on whether a
///   field is even present in the entity, eliminating the
///   zero-conflated-with-null bug class.
/// - Field-vs-field comparison: `FieldEq`, `FieldGt/Ge/Lt/Le` — compare
///   two entity fields directly instead of a field against a literal.
///   Lets structural rules like "tools added > tool docs added" express
///   cleanly without hardcoding a constant.
/// - Float comparison: `GeF`, `GtF`, `LeF`, `LtF` — real-valued
///   predicates for coverage percentages, confidence scores, rates.
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
    /// `entity[field] >= value` (float)
    GeF { field: String, value: f64 },
    GtF { field: String, value: f64 },
    LeF { field: String, value: f64 },
    LtF { field: String, value: f64 },
    /// **DEPRECATED.** Applicability predicate that collapses missing
    /// and null into one state — the bugs both adversarial-review bros
    /// caught in phase-2. Retained at deserialize time so phase-2
    /// packets on disk still evaluate, but new rules should use
    /// `IsNonNull` (same semantics, honest name) instead. A future
    /// phase will delete this variant after in-flight packets are
    /// migrated; the evaluator emits a `tracing::warn!` on each use.
    IsPresent { field: String },
    /// **DEPRECATED.** Complement of `IsPresent`; fires on either
    /// missing OR null — a trap both bros flagged. Use `IsMissing`
    /// (key absent) or `IsNull` (key present, value null) for precise
    /// semantics. Deletion scheduled after phase-3 migration.
    IsAbsent { field: String },
    /// Tri-state applicability (phase-2.5, added per adversarial-review
    /// convergent critique on thread-cc7ff97d): JSON distinguishes
    /// `{}` (key missing) from `{x: null}` (key present, value null).
    /// `null` typically means "known non-applicable"; missing means
    /// "not computed / extractor failed." The tri-state predicates
    /// preserve that distinction where `IsPresent` destroys it.
    ///
    /// - `KeyExists` — key exists regardless of value (null or otherwise)
    /// - `IsNull`    — key exists AND value is the JSON `null` literal
    /// - `IsNonNull` — key exists AND value is NOT null (what the old
    ///                 `IsPresent` did)
    /// - `IsMissing` — key does not exist in the entity at all
    KeyExists { field: String },
    IsNull { field: String },
    IsNonNull { field: String },
    IsMissing { field: String },
    /// Cross-field comparison (integer). `entity[lhs_field] OP entity[rhs_field]`.
    /// Returns false when either side is missing or non-integer.
    FieldEq {
        lhs_field: String,
        rhs_field: String,
    },
    FieldGt {
        lhs_field: String,
        rhs_field: String,
    },
    FieldGe {
        lhs_field: String,
        rhs_field: String,
    },
    FieldLt {
        lhs_field: String,
        rhs_field: String,
    },
    FieldLe {
        lhs_field: String,
        rhs_field: String,
    },
    /// Common auth-style pattern: `entity[rank_field] >= entity[threshold_field]`.
    /// Field values must be integers after lookup-table resolution. Kept as a
    /// named alias for the rank-threshold idiom that predates FieldGe.
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

// ── Severity ──────────────────────────────────────────────────────

/// First-class rule severity. Lives on the `Rule` so the engine can
/// aggregate, sort, and threshold mechanically instead of parsing
/// severity out of the consequent string.
///
/// Aggregation precedence (used by `bbox_apply` mode="all" verdict):
/// Fail > Flag > Manual > Pass > Info.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, strum::EnumString, strum::AsRefStr)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum Severity {
    Fail,
    Flag,
    Manual,
    Pass,
    Info,
}

impl Severity {
    /// Ordering for aggregate verdict computation. Higher rank wins.
    fn rank(self) -> u8 {
        match self {
            Severity::Fail => 5,
            Severity::Flag => 4,
            Severity::Manual => 3,
            Severity::Pass => 2,
            Severity::Info => 1,
        }
    }
}

impl Default for Severity {
    fn default() -> Self {
        Severity::Info
    }
}

/// Infer a severity from the rule ID when the caller didn't specify one.
/// Prefix convention makes rule ordering auditable: `fail_*`, `flag_*`,
/// `manual_*`, `pass_*` map to matching severities. Unrecognized prefixes
/// fall through to `Info`.
fn infer_severity_from_id(id: &str) -> Severity {
    if id.starts_with("fail_") || id.starts_with("fail-") {
        Severity::Fail
    } else if id.starts_with("flag_") || id.starts_with("flag-") {
        Severity::Flag
    } else if id.starts_with("manual_")
        || id.starts_with("manual-")
        || id.starts_with("review_")
        || id.starts_with("review-")
    {
        Severity::Manual
    } else if id.starts_with("pass_") || id.starts_with("pass-") {
        Severity::Pass
    } else {
        Severity::Info
    }
}

// ── Emit (rule firing semantics in apply_all) ────────────────────

/// How a rule participates in `apply_all` evaluation. Addresses the
/// phase-2 bug where a `pass_all_clean` with `{op: True}` fired
/// alongside real findings — `Fallback` rules are evaluated only when
/// no `Independent` rule fired, which is the correct semantics for a
/// catchall that should disappear when real findings exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, strum::EnumString, strum::AsRefStr)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum Emit {
    /// Default: rule fires whenever its antecedent matches.
    Independent,
    /// Catchall / default-case rule. In `apply_all`, fires ONLY when no
    /// `Independent` rule fired. In `apply_first`, behaves like any
    /// other rule (first-match-wins ordering still applies).
    Fallback,
}

impl Default for Emit {
    fn default() -> Self {
        Emit::Independent
    }
}

// ── Rule ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rule {
    pub id: String,
    pub antecedent: Predicate,
    pub consequent: Value,
    /// First-class severity. If omitted at compile time, inferred from the
    /// id prefix (`fail_*` → Fail, `flag_*` → Flag, `manual_*`/`review_*` →
    /// Manual, `pass_*` → Pass, otherwise Info). Inference runs only when
    /// the caller's input lacked a severity field; explicit `severity:
    /// "info"` is preserved. This is what Codex's phase-2 review caught
    /// as a bug — the v3 compile loop upgraded every Info to the prefix-
    /// inferred value, erasing explicit "info". Phase-2.5 fix: RuleInput
    /// carries `Option<Severity>` and inference only runs on `None`.
    #[serde(default)]
    pub severity: Severity,
    /// Firing semantics in `apply_all`. Default: Independent. Set to
    /// Fallback on catchall rules (e.g. `pass_all_clean`) so they only
    /// emit findings when no real rule matched.
    #[serde(default)]
    pub emit: Emit,
    #[serde(default = "default_confidence")]
    pub confidence: f32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provenance: Vec<String>,
}

/// Rules-as-authored. Uses `Option<Severity>` so we can distinguish
/// "caller said nothing" (infer from id) from "caller said Info"
/// (preserve). Converted to `Rule` in `compile`.
#[derive(Debug, Clone, Deserialize)]
struct RuleInput {
    id: String,
    antecedent: Predicate,
    consequent: Value,
    #[serde(default)]
    severity: Option<Severity>,
    #[serde(default)]
    emit: Option<Emit>,
    #[serde(default = "default_confidence")]
    confidence: f32,
    #[serde(default)]
    provenance: Vec<String>,
}

impl RuleInput {
    fn materialize(self) -> Rule {
        let severity = self
            .severity
            .unwrap_or_else(|| infer_severity_from_id(&self.id));
        Rule {
            id: self.id,
            antecedent: self.antecedent,
            consequent: self.consequent,
            severity,
            emit: self.emit.unwrap_or_default(),
            confidence: self.confidence,
            provenance: self.provenance,
        }
    }
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
    /// Severity of the rule that fired. Lets callers group/filter findings
    /// mechanically (especially in `apply_all` mode where multiple rules
    /// fire simultaneously and a reviewer wants all FAILs first).
    #[serde(default)]
    pub severity: Severity,
}

/// Result of `bbox_apply` in mode="all" — every rule whose antecedent
/// holds emits a finding. `verdict` is the aggregate severity computed
/// from the findings via Fail > Flag > Manual > Pass > Info precedence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplyAllResult {
    pub packet_id: String,
    pub findings: Vec<Prediction>,
    /// Aggregate verdict. `null` when no rule matched at all.
    pub verdict: Option<Severity>,
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

fn entity_f64(entity: &serde_json::Map<String, serde_json::Value>, field: &str) -> Option<f64> {
    entity.get(field).and_then(|v| v.as_f64())
}

fn entity_has(entity: &serde_json::Map<String, serde_json::Value>, field: &str) -> bool {
    // `IsPresent` semantics: the field exists in the map AND is not the JSON
    // null literal. Collapses missing and null — preserved for backward
    // compat with phase-2 packets. Rules that need to distinguish should
    // use the tri-state predicates (`KeyExists`, `IsNull`, `IsNonNull`,
    // `IsMissing`) instead.
    match entity.get(field) {
        None => false,
        Some(serde_json::Value::Null) => false,
        Some(_) => true,
    }
}

fn entity_key_exists(entity: &serde_json::Map<String, serde_json::Value>, field: &str) -> bool {
    entity.contains_key(field)
}

fn entity_is_null(entity: &serde_json::Map<String, serde_json::Value>, field: &str) -> bool {
    matches!(entity.get(field), Some(serde_json::Value::Null))
}

/// Evaluate a predicate against a resolved entity. Pure function — no
/// I/O, no LLM, no side effects. Cross-field and applicability
/// predicates return `false` on missing / malformed inputs rather than
/// panicking or erroring; the caller can audit applicability via
/// `IsPresent` / `IsAbsent` guards in composite rules.
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
        Predicate::GeF { field, value } => {
            entity_f64(entity, field).map(|v| v >= *value).unwrap_or(false)
        }
        Predicate::GtF { field, value } => {
            entity_f64(entity, field).map(|v| v > *value).unwrap_or(false)
        }
        Predicate::LeF { field, value } => {
            entity_f64(entity, field).map(|v| v <= *value).unwrap_or(false)
        }
        Predicate::LtF { field, value } => {
            entity_f64(entity, field).map(|v| v < *value).unwrap_or(false)
        }
        Predicate::IsPresent { field } => {
            tracing::warn!(
                field,
                "packets: IsPresent is deprecated (collapses missing+null); use IsNonNull for same semantics"
            );
            entity_has(entity, field)
        }
        Predicate::IsAbsent { field } => {
            tracing::warn!(
                field,
                "packets: IsAbsent is deprecated (fires on missing OR null — ambiguous); use IsMissing or IsNull for precise semantics"
            );
            !entity_has(entity, field)
        }
        Predicate::KeyExists { field } => entity_key_exists(entity, field),
        Predicate::IsNull { field } => entity_is_null(entity, field),
        Predicate::IsNonNull { field } => entity_has(entity, field),
        Predicate::IsMissing { field } => !entity_key_exists(entity, field),
        Predicate::FieldEq { lhs_field, rhs_field } => {
            match (entity_get(entity, lhs_field), entity_get(entity, rhs_field)) {
                (Some(l), Some(r)) => l == r,
                _ => false,
            }
        }
        Predicate::FieldGt { lhs_field, rhs_field } => {
            match (entity_int(entity, lhs_field), entity_int(entity, rhs_field)) {
                (Some(l), Some(r)) => l > r,
                _ => false,
            }
        }
        Predicate::FieldGe { lhs_field, rhs_field } => {
            match (entity_int(entity, lhs_field), entity_int(entity, rhs_field)) {
                (Some(l), Some(r)) => l >= r,
                _ => false,
            }
        }
        Predicate::FieldLt { lhs_field, rhs_field } => {
            match (entity_int(entity, lhs_field), entity_int(entity, rhs_field)) {
                (Some(l), Some(r)) => l < r,
                _ => false,
            }
        }
        Predicate::FieldLe { lhs_field, rhs_field } => {
            match (entity_int(entity, lhs_field), entity_int(entity, rhs_field)) {
                (Some(l), Some(r)) => l <= r,
                _ => false,
            }
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
                severity: rule.severity,
            });
        }
    }
    None
}

/// Evaluate every rule independently against an entity. Returns all
/// findings in packet-declared order plus an aggregate verdict. This is
/// the right semantic for review workflows where multiple FLAGs should
/// surface in a single pass — see the adversarial-review notes on
/// thread-0b20e854 and thread-cc7ff97d for the motivating critiques.
///
/// Two-pass semantics (phase-2.5 addition per Codex's proposed
/// mechanism): evaluate `Independent` rules first; evaluate `Fallback`
/// rules only when no `Independent` rule matched. This fixes the
/// pass_all_clean-fires-alongside-FAILs bug without special-casing the
/// severity enum. A catchall PASS rule authored as `emit: "fallback"`
/// now vanishes automatically when any real finding exists.
pub fn apply_all(packet: &Packet, entity: &serde_json::Value) -> ApplyAllResult {
    let entity_obj = match entity.as_object() {
        Some(o) => o,
        None => {
            return ApplyAllResult {
                packet_id: packet.id.clone(),
                findings: Vec::new(),
                verdict: None,
            };
        }
    };
    let resolved = resolve_entity(packet, entity_obj);

    let independent_findings: Vec<Prediction> = packet
        .rules
        .iter()
        .filter(|rule| rule.emit == Emit::Independent)
        .filter(|rule| eval_predicate(&rule.antecedent, &resolved))
        .map(|rule| Prediction {
            rule_id: rule.id.clone(),
            consequent: rule.consequent.clone(),
            confidence: rule.confidence,
            severity: rule.severity,
        })
        .collect();

    // Fallback rules fire only when NO Independent rule fired. This is
    // what makes a `pass_all_clean` catchall do the right thing in
    // mode="all" without polluting the findings list when real issues
    // surfaced.
    let fallback_findings: Vec<Prediction> = if independent_findings.is_empty() {
        packet
            .rules
            .iter()
            .filter(|rule| rule.emit == Emit::Fallback)
            .filter(|rule| eval_predicate(&rule.antecedent, &resolved))
            .map(|rule| Prediction {
                rule_id: rule.id.clone(),
                consequent: rule.consequent.clone(),
                confidence: rule.confidence,
                severity: rule.severity,
            })
            .collect()
    } else {
        Vec::new()
    };

    let findings: Vec<Prediction> = independent_findings
        .into_iter()
        .chain(fallback_findings)
        .collect();

    let verdict = findings
        .iter()
        .map(|p| p.severity)
        .max_by_key(|s| s.rank());

    ApplyAllResult {
        packet_id: packet.id.clone(),
        findings,
        verdict,
    }
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

        // Deserialize into RuleInput (severity is `Option<Severity>` here),
        // then materialize into Rule — inferring severity from the id
        // prefix ONLY when the caller omitted it. Explicit `severity:
        // "info"` is preserved, which fixes the phase-2 bug where the
        // compile loop upgraded every Info to the prefix-inferred value.
        let inputs: Vec<RuleInput> = serde_json::from_value(p.rules.clone())
            .context("'rules' must be a JSON array of {id, antecedent, consequent, severity?, emit?, confidence?, provenance?} objects")?;

        if inputs.is_empty() {
            anyhow::bail!("'rules' cannot be empty — at least one rule required");
        }

        let rules: Vec<Rule> = inputs.into_iter().map(RuleInput::materialize).collect();

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
        let mode = p.mode.unwrap_or_default();
        match mode {
            ApplyMode::First => match apply(&packet, &p.entity) {
                Some(prediction) => Ok(serde_json::to_string_pretty(&serde_json::json!({
                    "packet_id": packet.id,
                    "mode": mode,
                    "match": true,
                    "rule_id": prediction.rule_id,
                    "severity": prediction.severity,
                    "consequent": prediction.consequent,
                    "confidence": prediction.confidence,
                }))?),
                None => Ok(serde_json::to_string_pretty(&serde_json::json!({
                    "packet_id": packet.id,
                    "mode": mode,
                    "match": false,
                    "consequent": serde_json::Value::Null,
                    "note": "no rule's antecedent matched the entity",
                }))?),
            },
            ApplyMode::All => {
                let result = apply_all(&packet, &p.entity);
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
                severity: Severity::Info,
                emit: Emit::Independent,
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
                severity: Severity::Info,
                emit: Emit::Independent,
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
                severity: Severity::Info,
                emit: Emit::Independent,
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
                severity: Severity::Info,
                emit: Emit::Independent,
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
                severity: Severity::Info,
                emit: Emit::Independent,
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
                severity: Severity::Info,
                emit: Emit::Independent,
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
                severity: Severity::Info,
                emit: Emit::Independent,
                confidence: 1.0,
                provenance: vec![],
            },
            // Catch-all deny
            Rule {
                id: "default_deny".into(),
                antecedent: Predicate::AlwaysTrue {},
                consequent: Value::String("DENY".into()),
                severity: Severity::Info,
                emit: Emit::Independent,
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
            mode: None,
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
                mode: None,
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

    // ── Phase 2 tests: applicability, field-vs-field, float, severity, evaluate-all ──

    fn bare_packet(rules: Vec<Rule>) -> Packet {
        let now = Packets::now_iso();
        Packet {
            id: "packet-phase2t".into(),
            domain: "phase2-test".into(),
            scope: "global".into(),
            project: None,
            rank_table: BTreeMap::new(),
            threshold_table: BTreeMap::new(),
            rank_lookup_key: "role".into(),
            threshold_lookup_key: "resource".into(),
            rules,
            source_ids: vec![],
            self_audit_fidelity: None,
            created_at: now.clone(),
            updated_at: now,
            superseded_by: None,
            merged_from: vec![],
        }
    }

    fn rule(id: &str, antecedent: Predicate, consequent: &str, sev: Severity) -> Rule {
        Rule {
            id: id.into(),
            antecedent,
            consequent: Value::String(consequent.into()),
            severity: sev,
            emit: Emit::Independent,
            confidence: 1.0,
            provenance: vec![],
        }
    }

    fn fallback_rule(id: &str, antecedent: Predicate, consequent: &str, sev: Severity) -> Rule {
        Rule {
            id: id.into(),
            antecedent,
            consequent: Value::String(consequent.into()),
            severity: sev,
            emit: Emit::Fallback,
            confidence: 1.0,
            provenance: vec![],
        }
    }

    #[test]
    fn is_present_discriminates_from_zero() {
        // The decisive phase-2 bug: rule said `Lt(tool_docs, 3)` fires on
        // a "clean" entity where no docs were added AND no tools were
        // added. IsPresent lets rules gate on applicability.
        let p = bare_packet(vec![rule(
            "fail_undocumented",
            Predicate::All {
                args: vec![
                    Predicate::IsPresent { field: "mcp_tools_added".into() },
                    Predicate::Gt { field: "mcp_tools_added".into(), value: 0 },
                    Predicate::FieldLt {
                        lhs_field: "tool_docs_stanzas_added".into(),
                        rhs_field: "mcp_tools_added".into(),
                    },
                ],
            },
            "FAIL",
            Severity::Fail,
        )]);

        // No tools added — rule must NOT fire even though
        // tool_docs_stanzas_added=0 < some-constant.
        let clean = json!({
            "mcp_tools_added": 0,
            "tool_docs_stanzas_added": 0,
        });
        assert!(apply(&p, &clean).is_none(), "clean entity must not fire");

        // Tools added with too few docs — rule fires.
        let undoc = json!({
            "mcp_tools_added": 3,
            "tool_docs_stanzas_added": 1,
        });
        let pred = apply(&p, &undoc).expect("should fire on undoc");
        assert_eq!(pred.rule_id, "fail_undocumented");

        // Tools added, docs match — no fire.
        let ok = json!({
            "mcp_tools_added": 3,
            "tool_docs_stanzas_added": 3,
        });
        assert!(apply(&p, &ok).is_none(), "fully documented must not fire");
    }

    #[test]
    fn is_absent_and_is_present_treat_null_as_absent() {
        let present_rule = Predicate::IsPresent { field: "x".into() };
        let absent_rule = Predicate::IsAbsent { field: "x".into() };

        let p = bare_packet(vec![
            rule("flag_present", present_rule, "HAS_X", Severity::Flag),
        ]);

        // Field missing: not present.
        assert!(apply(&p, &json!({})).is_none());
        // Field null: treated as absent (signal from data source).
        assert!(apply(&p, &json!({"x": null})).is_none());
        // Field with real value: present.
        assert!(apply(&p, &json!({"x": 42})).is_some());
        assert!(apply(&p, &json!({"x": "foo"})).is_some());
        assert!(apply(&p, &json!({"x": false})).is_some()); // bool false is still present

        // IsAbsent is the exact complement
        let ap = bare_packet(vec![rule("flag_absent", absent_rule, "NO_X", Severity::Flag)]);
        assert!(apply(&ap, &json!({})).is_some(), "missing field → absent → fire");
        assert!(apply(&ap, &json!({"x": null})).is_some(), "null → absent → fire");
        assert!(apply(&ap, &json!({"x": 1})).is_none(), "present → no fire");
    }

    #[test]
    fn field_comparisons_work_across_all_ops() {
        let cases: &[(Predicate, &str, serde_json::Value, bool)] = &[
            (Predicate::FieldEq { lhs_field: "a".into(), rhs_field: "b".into() },
                "eq-hit", json!({"a": 5, "b": 5}), true),
            (Predicate::FieldEq { lhs_field: "a".into(), rhs_field: "b".into() },
                "eq-miss", json!({"a": 5, "b": 6}), false),
            (Predicate::FieldGt { lhs_field: "a".into(), rhs_field: "b".into() },
                "gt-hit", json!({"a": 10, "b": 5}), true),
            (Predicate::FieldGt { lhs_field: "a".into(), rhs_field: "b".into() },
                "gt-eq", json!({"a": 5, "b": 5}), false),
            (Predicate::FieldGe { lhs_field: "a".into(), rhs_field: "b".into() },
                "ge-eq", json!({"a": 5, "b": 5}), true),
            (Predicate::FieldLt { lhs_field: "a".into(), rhs_field: "b".into() },
                "lt-hit", json!({"a": 1, "b": 5}), true),
            (Predicate::FieldLe { lhs_field: "a".into(), rhs_field: "b".into() },
                "le-eq", json!({"a": 5, "b": 5}), true),
            // Missing field → false (no panic)
            (Predicate::FieldGt { lhs_field: "a".into(), rhs_field: "b".into() },
                "missing-a", json!({"b": 5}), false),
        ];

        for (pred, label, entity, expect_hit) in cases {
            let p = bare_packet(vec![rule(label, pred.clone(), "HIT", Severity::Info)]);
            let fired = apply(&p, entity).is_some();
            assert_eq!(fired, *expect_hit, "case `{label}` failed; pred={pred:?}, entity={entity}");
        }
    }

    #[test]
    fn float_comparisons_work() {
        let p = bare_packet(vec![
            rule("fail_low_coverage", Predicate::LtF { field: "coverage_pct".into(), value: 80.0 },
                 "FAIL: coverage below 80%", Severity::Fail),
            rule("pass_high_coverage", Predicate::GeF { field: "coverage_pct".into(), value: 95.0 },
                 "PASS: coverage above 95%", Severity::Pass),
        ]);

        let low = apply(&p, &json!({"coverage_pct": 73.5})).unwrap();
        assert_eq!(low.rule_id, "fail_low_coverage");

        let mid = apply(&p, &json!({"coverage_pct": 85.0}));
        assert!(mid.is_none(), "mid coverage should match neither rule");

        let high = apply(&p, &json!({"coverage_pct": 96.0})).unwrap();
        assert_eq!(high.rule_id, "pass_high_coverage");
    }

    #[test]
    fn severity_infers_from_id_prefix() {
        assert_eq!(infer_severity_from_id("fail_warnings"), Severity::Fail);
        assert_eq!(infer_severity_from_id("flag_readonly_fs"), Severity::Flag);
        assert_eq!(infer_severity_from_id("manual_review_security"), Severity::Manual);
        assert_eq!(infer_severity_from_id("review_contract"), Severity::Manual);
        assert_eq!(infer_severity_from_id("pass_all_clean"), Severity::Pass);
        assert_eq!(infer_severity_from_id("miscellaneous"), Severity::Info);
    }

    #[test]
    fn severity_serde_lowercase() {
        // Round-trip through JSON.
        let s = Severity::Fail;
        let json = serde_json::to_value(&s).unwrap();
        assert_eq!(json, json!("fail"));
        let back: Severity = serde_json::from_value(json).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn rule_deserializes_without_severity_for_backcompat() {
        // Old packets on disk lack the severity field. They must still
        // deserialize with severity=Info default. Old packet-890e057d
        // from thread-0b20e854 was produced before severity existed.
        let old_json = json!({
            "id": "fail_warnings",
            "antecedent": {"op": "Gt", "field": "new_warnings", "value": 0},
            "consequent": "FAIL",
            "confidence": 1.0
        });
        let r: Rule = serde_json::from_value(old_json).unwrap();
        assert_eq!(r.severity, Severity::Info); // default
        assert_eq!(r.id, "fail_warnings");
    }

    #[test]
    fn compile_infers_severity_from_id() {
        let (_dir, store) = tmp_packets();
        let params = CompileParams {
            domain: "severity-test".into(),
            rules: json!([
                {"id": "fail_a", "antecedent": {"op": "True"}, "consequent": "FAIL"},
                {"id": "flag_b", "antecedent": {"op": "True"}, "consequent": "FLAG"},
                {"id": "manual_c", "antecedent": {"op": "True"}, "consequent": "MANUAL"},
                {"id": "pass_d", "antecedent": {"op": "True"}, "consequent": "PASS"},
                // Explicit severity survives — even though id prefix would say Info
                {"id": "misc_e", "severity": "fail", "antecedent": {"op": "True"}, "consequent": "EXPLICIT"},
            ]),
            rank_table: None,
            threshold_table: None,
            rank_lookup_key: None,
            threshold_lookup_key: None,
            source_ids: None,
            scope: Some("global".into()),
            project: None,
        };
        store.compile(&params).unwrap();

        let all = store.list_all().unwrap();
        let packet = &all[0];
        let severities: Vec<Severity> = packet.rules.iter().map(|r| r.severity).collect();
        assert_eq!(
            severities,
            vec![
                Severity::Fail,   // inferred from fail_
                Severity::Flag,   // inferred from flag_
                Severity::Manual, // inferred from manual_
                Severity::Pass,   // inferred from pass_
                Severity::Fail,   // explicit "fail" beats id-prefix inference
            ]
        );
    }

    #[test]
    fn apply_all_returns_every_matching_rule() {
        // The critical phase-2 semantic: apply_all evaluates every rule,
        // returns all findings, computes aggregate verdict. This is what
        // the bros called for in thread-0b20e854.
        let p = bare_packet(vec![
            rule("fail_a", Predicate::AlwaysTrue {}, "FAIL: always", Severity::Fail),
            rule("flag_b", Predicate::AlwaysTrue {}, "FLAG: always", Severity::Flag),
            rule("flag_c", Predicate::Eq { field: "x".into(), value: Value::Int(1) },
                 "FLAG: on x=1", Severity::Flag),
            rule("pass_d", Predicate::AlwaysFalse {}, "PASS: never", Severity::Pass),
        ]);

        let result = apply_all(&p, &json!({"x": 1}));
        let fired: Vec<&str> = result.findings.iter().map(|f| f.rule_id.as_str()).collect();
        assert_eq!(fired, vec!["fail_a", "flag_b", "flag_c"], "every matching rule should appear");
        assert_eq!(result.verdict, Some(Severity::Fail), "verdict = highest severity that fired");

        // Entity where only the false rule fires → no findings, no verdict
        let empty = apply_all(&p, &json!({"x": 99}));
        let fired2: Vec<&str> = empty.findings.iter().map(|f| f.rule_id.as_str()).collect();
        assert_eq!(fired2, vec!["fail_a", "flag_b"]); // unconditional rules still fire
    }

    #[test]
    fn apply_all_verdict_follows_severity_precedence() {
        // Fail > Flag > Manual > Pass > Info
        let fail_p = bare_packet(vec![
            rule("pass_x", Predicate::AlwaysTrue {}, "PASS", Severity::Pass),
            rule("manual_y", Predicate::AlwaysTrue {}, "MANUAL", Severity::Manual),
            rule("flag_z", Predicate::AlwaysTrue {}, "FLAG", Severity::Flag),
        ]);
        assert_eq!(apply_all(&fail_p, &json!({})).verdict, Some(Severity::Flag));

        let with_fail = bare_packet(vec![
            rule("pass_x", Predicate::AlwaysTrue {}, "PASS", Severity::Pass),
            rule("fail_y", Predicate::AlwaysTrue {}, "FAIL", Severity::Fail),
            rule("info_z", Predicate::AlwaysTrue {}, "INFO", Severity::Info),
        ]);
        assert_eq!(apply_all(&with_fail, &json!({})).verdict, Some(Severity::Fail));

        // Nothing fires
        let nothing = bare_packet(vec![
            rule("fail_never", Predicate::AlwaysFalse {}, "NOPE", Severity::Fail),
        ]);
        assert_eq!(apply_all(&nothing, &json!({})).verdict, None);
    }

    #[test]
    fn apply_tool_all_mode_returns_aggregate() {
        let (_dir, store) = tmp_packets();
        let params = CompileParams {
            domain: "all-mode-test".into(),
            rules: json!([
                {"id": "flag_a", "antecedent": {"op": "True"}, "consequent": "FLAG_A"},
                {"id": "flag_b", "antecedent": {"op": "True"}, "consequent": "FLAG_B"},
                {"id": "pass_c", "antecedent": {"op": "True"}, "consequent": "PASS"},
            ]),
            rank_table: None,
            threshold_table: None,
            rank_lookup_key: None,
            threshold_lookup_key: None,
            source_ids: None,
            scope: Some("global".into()),
            project: None,
        };
        store.compile(&params).unwrap();
        let all = store.list_all().unwrap();
        let packet = &all[0];

        let out = store
            .apply_tool(&ApplyParams {
                packet_id: packet.id.clone(),
                entity: json!({}),
                mode: Some(ApplyMode::All),
            })
            .unwrap();
        assert!(out.contains("\"mode\": \"all\""));
        assert!(out.contains("\"verdict\": \"flag\""));
        assert!(out.contains("\"finding_count\": 3"));
        assert!(out.contains("flag_a"));
        assert!(out.contains("flag_b"));
        assert!(out.contains("pass_c"));
    }

    #[test]
    fn apply_mode_deserializes_invalid_string_as_error() {
        // Phase-2.5: mode is now a typed enum, so invalid mode strings
        // fail at JSON deserialization rather than reaching apply_tool.
        let bad = json!({"packet_id": "packet-deadbeef", "entity": {}, "mode": "nonsense"});
        let res: std::result::Result<ApplyParams, _> = serde_json::from_value(bad);
        assert!(res.is_err(), "invalid mode string should fail deserialization");
    }

    #[test]
    fn value_eq_across_int_and_float() {
        // JSON serde can widen ints to floats on round-trip. Rules
        // authored as `Eq{value: 5}` must still match `entity.x = 5.0`.
        assert_eq!(Value::Int(5), Value::Float(5.0));
        assert_eq!(Value::Float(5.0), Value::Int(5));
        assert_ne!(Value::Int(5), Value::Int(6));
        assert_ne!(Value::Float(5.0), Value::Float(6.0));
    }

    // ── Phase 2.5 tests (convergent adversarial-review fixes) ──

    #[test]
    fn tri_state_applicability_discriminates_null_vs_missing() {
        let missing = json!({});                      // no key
        let nulled = json!({"x": serde_json::Value::Null}); // key present, value null
        let real = json!({"x": 42});                  // key present, real value

        let key_exists = bare_packet(vec![rule("flag_ke", Predicate::KeyExists { field: "x".into() }, "KE", Severity::Flag)]);
        let is_null = bare_packet(vec![rule("flag_null", Predicate::IsNull { field: "x".into() }, "NULL", Severity::Flag)]);
        let is_non_null = bare_packet(vec![rule("flag_nn", Predicate::IsNonNull { field: "x".into() }, "NN", Severity::Flag)]);
        let is_missing = bare_packet(vec![rule("flag_miss", Predicate::IsMissing { field: "x".into() }, "M", Severity::Flag)]);

        // KeyExists: fires when key exists regardless of value
        assert!(apply(&key_exists, &missing).is_none());
        assert!(apply(&key_exists, &nulled).is_some());
        assert!(apply(&key_exists, &real).is_some());

        // IsNull: ONLY when key exists AND value is null
        assert!(apply(&is_null, &missing).is_none());
        assert!(apply(&is_null, &nulled).is_some());
        assert!(apply(&is_null, &real).is_none());

        // IsNonNull: fires when key exists with a non-null value
        assert!(apply(&is_non_null, &missing).is_none());
        assert!(apply(&is_non_null, &nulled).is_none());
        assert!(apply(&is_non_null, &real).is_some());

        // IsMissing: fires ONLY when key absent
        assert!(apply(&is_missing, &missing).is_some());
        assert!(apply(&is_missing, &nulled).is_none());
        assert!(apply(&is_missing, &real).is_none());
    }

    #[test]
    fn severity_info_explicitly_preserved_over_prefix_inference() {
        // The phase-2 bug Codex caught: compile loop upgraded every Info
        // from the id prefix, so explicit `severity: "info"` was erased.
        let (_dir, store) = tmp_packets();
        let params = CompileParams {
            domain: "severity-preserve".into(),
            rules: json!([
                // Prefix says FAIL, but caller EXPLICITLY says Info — must preserve.
                {"id": "fail_x", "severity": "info", "antecedent": {"op": "True"}, "consequent": "X"},
                // No severity declared — infer from prefix.
                {"id": "fail_y", "antecedent": {"op": "True"}, "consequent": "Y"},
            ]),
            rank_table: None,
            threshold_table: None,
            rank_lookup_key: None,
            threshold_lookup_key: None,
            source_ids: None,
            scope: Some("global".into()),
            project: None,
        };
        store.compile(&params).unwrap();
        let packet = &store.list_all().unwrap()[0];
        assert_eq!(packet.rules[0].severity, Severity::Info, "explicit info must survive prefix inference");
        assert_eq!(packet.rules[1].severity, Severity::Fail, "no severity declared → infer from prefix");
    }

    #[test]
    fn fallback_rules_suppressed_when_independent_fires() {
        // Phase-2.5d: Fallback rules fire ONLY when no Independent rule fired.
        // This is how pass_all_clean ought to behave: disappear when real
        // findings exist, present when nothing else has anything to say.
        let p = bare_packet(vec![
            rule("flag_x", Predicate::Eq { field: "trigger".into(), value: Value::Bool(true) }, "FLAG", Severity::Flag),
            fallback_rule("pass_catchall", Predicate::AlwaysTrue {}, "PASS", Severity::Pass),
        ]);

        // Trigger fires — fallback is suppressed
        let result = apply_all(&p, &json!({"trigger": true}));
        let fired: Vec<&str> = result.findings.iter().map(|f| f.rule_id.as_str()).collect();
        assert_eq!(fired, vec!["flag_x"], "fallback must be suppressed when Independent fires");
        assert_eq!(result.verdict, Some(Severity::Flag));

        // No trigger — fallback fires
        let result = apply_all(&p, &json!({"trigger": false}));
        let fired: Vec<&str> = result.findings.iter().map(|f| f.rule_id.as_str()).collect();
        assert_eq!(fired, vec!["pass_catchall"], "fallback fires when no Independent matched");
        assert_eq!(result.verdict, Some(Severity::Pass));
    }

    #[test]
    fn fallback_ignored_in_first_mode() {
        // In apply (mode="first"), emit is irrelevant — first-match-wins
        // applies regardless. Fallback rules can still fire.
        let p = bare_packet(vec![
            rule("flag_x", Predicate::Eq { field: "a".into(), value: Value::Int(1) }, "FLAG_X", Severity::Flag),
            fallback_rule("pass_catchall", Predicate::AlwaysTrue {}, "PASS", Severity::Pass),
        ]);

        // When flag_x matches, first-match-wins picks it
        let pred = apply(&p, &json!({"a": 1})).unwrap();
        assert_eq!(pred.rule_id, "flag_x");

        // When flag_x doesn't match, pass_catchall (fallback) fires since we
        // still walk the rule list top-to-bottom.
        let pred = apply(&p, &json!({"a": 99})).unwrap();
        assert_eq!(pred.rule_id, "pass_catchall");
    }

    #[test]
    fn apply_mode_enum_serde_lowercase() {
        assert_eq!(serde_json::to_value(&ApplyMode::First).unwrap(), json!("first"));
        assert_eq!(serde_json::to_value(&ApplyMode::All).unwrap(), json!("all"));
        let m: ApplyMode = serde_json::from_value(json!("all")).unwrap();
        assert_eq!(m, ApplyMode::All);
    }

    #[test]
    fn emit_default_is_independent() {
        // Rule authored without `emit:` field gets Independent.
        let rule_json = json!({
            "id": "fail_x",
            "antecedent": {"op": "True"},
            "consequent": "X",
        });
        let ri: RuleInput = serde_json::from_value(rule_json).unwrap();
        let r = ri.materialize();
        assert_eq!(r.emit, Emit::Independent);
    }

    #[test]
    fn emit_fallback_deserializes() {
        let rule_json = json!({
            "id": "pass_clean",
            "antecedent": {"op": "True"},
            "consequent": "PASS",
            "emit": "fallback",
        });
        let ri: RuleInput = serde_json::from_value(rule_json).unwrap();
        let r = ri.materialize();
        assert_eq!(r.emit, Emit::Fallback);
    }

    #[test]
    fn old_packets_without_emit_default_independent() {
        // Backward compat: packets compiled before 2.5 lack `emit` on rules.
        // They must deserialize with Emit::Independent default.
        let old_rule = json!({
            "id": "fail_x",
            "antecedent": {"op": "True"},
            "consequent": "X",
            "severity": "fail",
            "confidence": 1.0
        });
        let r: Rule = serde_json::from_value(old_rule).unwrap();
        assert_eq!(r.emit, Emit::Independent);
    }
}
