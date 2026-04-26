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
use std::time::Duration;

use anyhow::{Context, Result};
use rmcp::schemars;
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

/// Case-insensitive substring match across the fields an agent is likely to
/// search for: packet id, domain, rule ids, rule classifications, and the
/// packet's classification lattice values. Intentionally narrow — rule
/// antecedents are structured AST, not free text, and searching inside them
/// would produce mostly noise. If an agent needs to know what a packet
/// *does*, the summary's rule-id preview + classification histogram carry
/// that signal; a full scan of nested predicate fields does not.
pub fn packet_matches_query(pkt: &Packet, query: &str) -> bool {
    let needle = query.to_lowercase();
    if pkt.id.to_lowercase().contains(&needle) {
        return true;
    }
    if pkt.domain.to_lowercase().contains(&needle) {
        return true;
    }
    for cls in &pkt.classification_lattice {
        if cls.to_lowercase().contains(&needle) {
            return true;
        }
    }
    for rule in &pkt.rules {
        if rule.id.to_lowercase().contains(&needle) {
            return true;
        }
        if rule.classification.to_lowercase().contains(&needle) {
            return true;
        }
    }
    false
}

/// Build the per-packet summary used by bbox_packet_list and bbox_knowledge's
/// packet-surfacing section. Includes a classification histogram and the
/// first few rule ids so agents can judge relevance at a glance.
pub fn packet_summary(pkt: &Packet) -> serde_json::Value {
    let mut histogram: BTreeMap<String, usize> = BTreeMap::new();
    for rule in &pkt.rules {
        *histogram.entry(rule.classification.clone()).or_insert(0) += 1;
    }
    let preview: Vec<&str> = pkt.rules.iter().take(3).map(|r| r.id.as_str()).collect();
    serde_json::json!({
        "id": pkt.id,
        "domain": pkt.domain,
        "scope": pkt.scope,
        "rules_count": pkt.rules.len(),
        "classification_histogram": histogram,
        "rule_ids_preview": preview,
        "created_at": pkt.created_at,
        "updated_at": pkt.updated_at,
    })
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CompileParams {
    /// Short domain label (e.g. "auth-matrix", "retry-policy"). Used for
    /// filtering and display; free-form.
    pub domain: String,
    /// Rules serialized as a JSON array. Each rule is
    /// `{ id, antecedent: <Predicate>, consequent: <Value>, classification?: string,
    ///   emit?: "independent"|"fallback", confidence?: f32, provenance?: [string] }`.
    /// Predicate AST operators are documented in the module-level doc comment.
    pub rules: serde_json::Value,
    /// Classification lattice, highest priority first. Defaults to the
    /// review lattice `["fail", "flag", "manual", "pass", "info"]` when
    /// omitted. Supply a domain-specific lattice for auth (`["deny","allow"]`),
    /// retry (`["dlq","fail_fast","backoff","retry","noop"]`), design-
    /// iteration (`["blocker","concern","suggestion","advantage","neutral"]`),
    /// etc. Aggregate verdicts respect this ordering.
    #[serde(default)]
    pub classification_lattice: Option<Vec<String>>,
    /// Id-prefix → classification map for inference. Defaults to the
    /// review prefix map when omitted. Keys are substrings matched at
    /// the start of the rule id (`"fail_"`, `"deny_"`, etc.).
    #[serde(default)]
    pub prefix_inference: Option<BTreeMap<String, String>>,
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
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
    strum::EnumString,
    strum::AsRefStr,
)]
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
    /// Dataset shape depends on `mode`:
    /// - `mode="first"` (default): JSON array of `{entity, expected}` pairs
    ///   where `expected` is the Value the packet's first matching rule
    ///   should emit. Matches the original audit shape.
    /// - `mode="all"`: JSON array of
    ///   `{entity, expected_verdict?: string, expected_rule_ids?: [string]}`
    ///   pairs. `expected_verdict` matches `ApplyAllResult.verdict`;
    ///   `expected_rule_ids` is compared as a SET (order-invariant) against
    ///   the rule IDs that fired. Either can be omitted if you only care
    ///   about one check; a row with both omitted trivially passes.
    pub dataset: serde_json::Value,
    /// `"first"` (default) compares single-rule consequent; `"all"`
    /// compares aggregate verdict + fired-rule-id set. Use `"all"` to
    /// validate review/design packets that rely on multi-finding shape.
    #[serde(default)]
    pub mode: Option<ApplyMode>,
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

// ── Comparison op (used by CountCmp) ─────────────────────────────

/// Comparison operator for `CountCmp`. Named `compare` (not `op`) in
/// serde to avoid colliding with the Predicate enum's `op` tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CmpOp {
    Lt,
    Le,
    Eq,
    Ge,
    Gt,
}

impl CmpOp {
    fn apply(self, lhs: usize, rhs: usize) -> bool {
        match self {
            CmpOp::Lt => lhs < rhs,
            CmpOp::Le => lhs <= rhs,
            CmpOp::Eq => lhs == rhs,
            CmpOp::Ge => lhs >= rhs,
            CmpOp::Gt => lhs > rhs,
        }
    }
}

// ── Predicate AST ────────────────────────────────────────────────

/// The canonical predicate vocabulary. Rule antecedents are trees of
/// these nodes; evaluation is a pure function of `(node, entity)`. The
/// serde tag `op` matches the JSON form produced in E11.
///
/// Additions since v1:
/// - Tri-state applicability: `KeyExists`, `IsNull`, `IsNonNull`,
///   `IsMissing` — JSON distinguishes `{}` (key missing) from
///   `{x: null}` (key present, value null). `null` typically means
///   "known non-applicable"; missing means "not computed / extractor
///   failed." Four predicates that preserve this distinction.
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
    Eq {
        field: String,
        value: Value,
    },
    /// `entity[field] >= value` (integer)
    Ge {
        field: String,
        value: i64,
    },
    Gt {
        field: String,
        value: i64,
    },
    Le {
        field: String,
        value: i64,
    },
    Lt {
        field: String,
        value: i64,
    },
    /// `entity[field] >= value` (float)
    GeF {
        field: String,
        value: f64,
    },
    GtF {
        field: String,
        value: f64,
    },
    LeF {
        field: String,
        value: f64,
    },
    LtF {
        field: String,
        value: f64,
    },
    /// Tri-state applicability. Preserves the distinction between
    /// `{}` (key missing) and `{x: null}` (key present, value null).
    ///
    /// - `KeyExists` — key exists regardless of value (null or otherwise)
    /// - `IsNull`    — key exists AND value is the JSON `null` literal
    /// - `IsNonNull` — key exists AND value is NOT null
    /// - `IsMissing` — key does not exist in the entity at all
    KeyExists {
        field: String,
    },
    IsNull {
        field: String,
    },
    IsNonNull {
        field: String,
    },
    IsMissing {
        field: String,
    },
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
    All {
        args: Vec<Predicate>,
    },
    Any {
        args: Vec<Predicate>,
    },
    Not {
        arg: Box<Predicate>,
    },
    /// **Quantified universal.** Path resolves to an array in the
    /// entity; inner predicate must hold for EVERY element. Missing or
    /// empty collection → `true` (vacuous truth). Non-array path →
    /// `false` (can't quantify over a scalar).
    ///
    /// Path syntax: simple dotted field lookup with a trailing `[*]`:
    /// `"tools[*]"`, `"config.rules[*]"`. Inside `pred`, the sub-entity
    /// IS the array element. If the element is a JSON object, its
    /// fields are directly addressable. If it's a primitive (string,
    /// int, bool), the predicate sees `{"$": element}` — address via
    /// the special field name `"$"`.
    ///
    /// No nested `ForAll` inside another `ForAll` in v1 — validated at
    /// `bbox_compile` (see `validate_predicate` below). The restriction
    /// keeps evaluator complexity bounded; revisit when a real use
    /// case demands nesting.
    ForAll {
        path: String,
        pred: Box<Predicate>,
    },
    /// **Quantified existential.** Path → array; inner predicate must
    /// hold for SOME element. Missing or empty collection → `false`.
    /// Same sub-entity shape as `ForAll`.
    Exists {
        path: String,
        pred: Box<Predicate>,
    },
    /// **Cardinality.** Compares the length of the array at `path`
    /// against `value` using `compare`. Missing path → length 0.
    /// Non-array path → length 0 (treat as "no collection present").
    /// No `where` filter in v1; if you need "count of items matching X",
    /// combine `Exists` with multiple rules or compose in the caller.
    CountCmp {
        path: String,
        compare: CmpOp,
        value: usize,
    },
    #[serde(rename = "True")]
    AlwaysTrue {},
    #[serde(rename = "False")]
    AlwaysFalse {},
    /// **Packet composition.** True iff applying the referenced packet
    /// to this entity produces a first-match verdict whose classification
    /// is in `expect`. Lets theories depend on other theories — a
    /// review packet can compose a `breaking_change` packet; an auth
    /// packet can compose a `privileged_role` packet.
    ///
    /// `entity_map` optionally rebinds caller fields before passing:
    /// `{"role": "actor_role"}` populates the sub-packet's `role`
    /// from the outer entity's `actor_role`. Unmapped fields pass
    /// through unchanged.
    ///
    /// Cycles are bounded by the depth limit
    /// [`MAX_COMPOSITION_DEPTH`]; exceeding it returns false with a
    /// warning log. Missing packets also return false with a warning.
    /// Validate at compile time via the optional resolver in
    /// [`Packets::compile`].
    Apply {
        packet_id: String,
        expect: Vec<String>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        entity_map: BTreeMap<String, String>,
    },
    /// **Substring match.** True iff `entity[field]` is a string that
    /// contains `needle`. Non-string values and missing fields return
    /// false. Combine via `Any{[Contains, Contains, ...]}` for the
    /// multi-needle idiom (the regex-alternation shape). This is the
    /// phase-6 answer to the two StringContains gap-log votes from
    /// the E10 sweep (S5 and S11).
    StringContains {
        field: String,
        needle: String,
        /// When true, both sides are lowercased before comparison.
        #[serde(default)]
        case_insensitive: bool,
    },
    /// **Integer banded range.** True iff `min <= entity[field] <= max`.
    /// Inclusive both ends. For exclusive bounds compose `All[Gt/Lt]`
    /// or use `InRangeF` with off-by-epsilon values. Missing or
    /// non-integer fields return false. Phase-6 sugar motivated by
    /// the E10 reflection where authoring `All[Gt 0, Le 5]` twice in
    /// one packet felt noisy.
    InRange {
        field: String,
        min: i64,
        max: i64,
    },
    /// **Float banded range.** True iff `min <= entity[field] <= max`.
    /// Inclusive both ends. Missing or non-numeric fields return false.
    InRangeF {
        field: String,
        min: f64,
        max: f64,
    },
    /// **Set membership.** True iff `entity[field]`'s scalar value is
    /// in `values`. Strings, ints, bools — pick a homogeneous type.
    /// Missing fields and non-scalar values return false. Common shape
    /// for "branch in {`merged`, `closed`, `superseded`}" and
    /// "last_signal.name in {`pr-merged`, `pr-feedback`}".
    In {
        field: String,
        values: Vec<Value>,
    },
    /// **Regex match.** True iff `entity[field]` is a string that
    /// matches `pattern`. Compiles the regex on every evaluation; the
    /// regex crate caches at the type level so the cost is just the
    /// hashmap lookup. Use anchors (`^`, `$`) explicitly — no implicit
    /// anchoring (matches anywhere in the string by default, like
    /// `regex::Regex::is_match`). Invalid regex evaluates to false
    /// with a warn log.
    StringMatches {
        field: String,
        pattern: String,
        #[serde(default)]
        case_insensitive: bool,
    },
    /// **Count-matching predicate.** True iff the count of sub-predicates
    /// that evaluate to true satisfies `count compare value`. Sibling to
    /// `All` (count == len) and `Any` (count >= 1) but with explicit
    /// threshold semantics. Enables "exactly K of N" and "at least K of
    /// N" shapes that All/Any can't express without enumeration.
    ///
    /// Phase-6 addition motivated by E14 composition-depth findings:
    /// three providers independently hit the "count how many composed
    /// sub-packets classified as red" limit when encoding tallying
    /// decisions over `Apply` nodes. The workaround — enumerating all
    /// C(N,K) pairwise combinations — is O(2^N) in the rule count and
    /// fails at N≥4. `CountMatches` collapses it to two rules and
    /// generalizes beyond Apply tallies to any multi-field threshold
    /// shape.
    ///
    /// Composes cleanly with `Apply` for sub-packet tallies:
    /// `CountMatches{args:[Apply{A,red}, Apply{B,red}, Apply{C,red}],
    ///               compare: ge, value: 2}` → true iff ≥2 concerns red.
    CountMatches {
        args: Vec<Predicate>,
        compare: CmpOp,
        value: usize,
    },
}

// ── Classification (user-defined per-packet lattice) ─────────────

/// Rule classification is a free-form `String` validated against the
/// packet's `classification_lattice`. Each domain declares its own
/// lattice and precedence direction, so the AST is domain-neutral
/// while the Rule/Packet layer carries the domain semantics.
///
/// Review domain: `["fail", "flag", "manual", "pass", "info"]`.
/// Auth domain: `["deny", "allow"]` — DENY wins.
/// Retry domain: `["dlq", "fail_fast", "backoff", "retry", "noop"]`.
/// Design-iteration: `["blocker", "concern", "suggestion", "advantage", "neutral"]`.
///
/// Lattice order is *highest priority first*. The aggregate verdict in
/// `apply(mode="all")` is the first-declared classification that any
/// rule fired.
///
/// The review lattice is the `unwrap_or_else` fallback in `bbox_compile`
/// when the caller omits `classification_lattice`. This is an opinion
/// not a truth: review happens to be the most common domain today.
/// Non-review callers should pass their own lattice explicitly — see
/// `sm-auth-packets`, `sm-design-packets` for canonical examples.
/// Named `review_lattice` (not the generic-sounding `default_lattice`
/// it was before phase 4) so the review privilege is visible at every
/// callsite rather than hidden behind the word "default."
pub fn review_lattice() -> Vec<String> {
    ["fail", "flag", "manual", "pass", "info"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// Review-domain id-prefix → classification map. Paired with
/// `review_lattice` as the fallback when `bbox_compile` is called
/// without explicit `prefix_inference`. Non-review callers should
/// supply their own map — see the auth/retry/design runbooks for
/// domain-appropriate prefix conventions.
pub fn review_prefix_inference() -> BTreeMap<String, String> {
    let pairs: &[(&str, &str)] = &[
        ("fail_", "fail"),
        ("fail-", "fail"),
        ("flag_", "flag"),
        ("flag-", "flag"),
        ("manual_", "manual"),
        ("manual-", "manual"),
        ("review_", "manual"),
        ("review-", "manual"),
        ("pass_", "pass"),
        ("pass-", "pass"),
    ];
    pairs
        .iter()
        .map(|(p, c)| (p.to_string(), c.to_string()))
        .collect()
}

/// Infer a classification from the rule ID using the packet's prefix
/// map. Returns `None` when no prefix matches.
///
/// **Longest-match wins.** If the map has both `fail_` → fail and
/// `fail_critical_` → blocker, a rule id `fail_critical_foo` resolves to
/// `blocker` (the longer prefix). BTreeMap iteration order would
/// otherwise resolve by lexicographic key order, which surprises users —
/// so this function explicitly picks the longest matching prefix.
/// Codex flagged this as the hidden-policy-most-likely-to-surprise in
/// phase-3 review (thread-cc7ff97d).
fn infer_classification(id: &str, prefix_map: &BTreeMap<String, String>) -> Option<String> {
    infer_classification_detailed(id, prefix_map).map(|(c, _)| c)
}

/// Variant of `infer_classification` that also returns the matching
/// prefix. Used by error messages so that a classification-lattice
/// mismatch can name the inferred prefix path explicitly, which was
/// the hidden-feature-most-likely-to-surprise per E14 (Gemini tripped
/// on a `review_` prefix auto-classifying as `manual` without seeing
/// the inference step in the error).
fn infer_classification_detailed<'a>(
    id: &str,
    prefix_map: &'a BTreeMap<String, String>,
) -> Option<(String, &'a str)> {
    let mut best: Option<(&str, &str)> = None;
    for (prefix, class) in prefix_map {
        if id.starts_with(prefix.as_str()) {
            match best {
                None => best = Some((prefix.as_str(), class.as_str())),
                Some((best_prefix, _)) if prefix.len() > best_prefix.len() => {
                    best = Some((prefix.as_str(), class.as_str()));
                }
                _ => {}
            }
        }
    }
    best.map(|(p, c)| (c.to_string(), p))
}

// ── Emit (rule firing semantics in apply_all) ────────────────────

/// How a rule participates in `apply_all` evaluation. Addresses the
/// phase-2 bug where a `pass_all_clean` with `{op: True}` fired
/// alongside real findings — `Fallback` rules are evaluated only when
/// no `Independent` rule fired, which is the correct semantics for a
/// catchall that should disappear when real findings exist.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, strum::EnumString, strum::AsRefStr,
)]
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
    /// Classification in the packet's lattice. If the caller omits it at
    /// compile time, inferred from the id prefix via the packet's
    /// `prefix_inference` map. Must be one of the values in the packet's
    /// `classification_lattice`; compile validates and rejects otherwise.
    pub classification: String,
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

/// Rules-as-authored. Uses `Option<String>` for classification so we
/// can distinguish "caller said nothing" (infer from id prefix) from
/// "caller said X" (preserve, then validate). Converted to `Rule` in
/// `compile` once the packet's lattice + prefix map are known.
#[derive(Debug, Clone, Deserialize)]
struct RuleInput {
    id: String,
    antecedent: Predicate,
    consequent: Value,
    #[serde(default)]
    classification: Option<String>,
    #[serde(default)]
    emit: Option<Emit>,
    #[serde(default = "default_confidence")]
    confidence: f32,
    #[serde(default)]
    provenance: Vec<String>,
}

/// Validate a quantified-predicate path: must be `<field>[*]` with a
/// single segment before `[*]`. Dotted paths and missing `[*]` are
/// authoring errors — reject at compile so they don't silently succeed
/// at runtime (which was the phase-4 bro critique convergent finding).
fn validate_path(path: &str, context: &str) -> Result<()> {
    let inner = path.strip_suffix("[*]").ok_or_else(|| {
        anyhow::anyhow!(
            "{}: path '{}' must end in '[*]' (phase-4 v1 path syntax)",
            context,
            path
        )
    })?;
    if inner.is_empty() {
        anyhow::bail!(
            "{}: path '{}' has empty field name before '[*]'",
            context,
            path
        );
    }
    // Dotted paths now permitted (e.g. `vars.labels[*]`,
    // `outputs.Plan.findings[*]`) since the ArcContext flatten emits
    // structured entities. Each segment must be non-empty.
    if inner.split('.').any(|seg| seg.is_empty()) {
        anyhow::bail!(
            "{}: dotted path '{}' has an empty segment",
            context,
            path
        );
    }
    if inner.contains('[') || inner.contains(']') {
        anyhow::bail!(
            "{}: path '{}' has stray brackets — use exactly one '[*]' at the end",
            context,
            path
        );
    }
    Ok(())
}

/// Walk a predicate tree and reject authoring errors:
/// - Invalid quantified-predicate paths (see `validate_path`)
/// - Nested `ForAll` inside another `ForAll` (deliberately banned in v1)
///
/// Called during `compile` so packets can't be saved with malformed
/// predicates. Runtime evaluation then trusts the tree.
fn validate_predicate(pred: &Predicate, inside_forall: bool) -> Result<()> {
    match pred {
        Predicate::ForAll { path, pred: inner } => {
            validate_path(path, "ForAll")?;
            if inside_forall {
                anyhow::bail!(
                    "ForAll nested inside ForAll at path '{}' — not supported in v1. \
                     Flatten the structure or wait for phase-next.",
                    path
                );
            }
            validate_predicate(inner, true)
        }
        Predicate::Exists { path, pred: inner } => {
            validate_path(path, "Exists")?;
            validate_predicate(inner, inside_forall)
        }
        Predicate::CountCmp { path, .. } => validate_path(path, "CountCmp"),
        Predicate::All { args } | Predicate::Any { args } => {
            for arg in args {
                validate_predicate(arg, inside_forall)?;
            }
            Ok(())
        }
        Predicate::CountMatches { args, .. } => {
            for arg in args {
                validate_predicate(arg, inside_forall)?;
            }
            Ok(())
        }
        Predicate::Not { arg } => validate_predicate(arg, inside_forall),
        Predicate::Apply {
            packet_id,
            expect,
            entity_map: _,
        } => {
            if packet_id.trim().is_empty() {
                anyhow::bail!("Apply requires non-empty 'packet_id'");
            }
            if expect.is_empty() {
                anyhow::bail!(
                    "Apply requires non-empty 'expect' — list at least one \
                     classification you want the sub-packet to produce"
                );
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Walk a predicate tree and collect every packet_id referenced via
/// `Apply`. Used by [`Packets::compile`] to verify sub-packets exist
/// at compile time rather than silently failing at eval time.
fn collect_apply_refs(pred: &Predicate, out: &mut Vec<String>) {
    match pred {
        Predicate::Apply { packet_id, .. } => out.push(packet_id.clone()),
        Predicate::All { args } | Predicate::Any { args } => {
            for a in args {
                collect_apply_refs(a, out);
            }
        }
        Predicate::CountMatches { args, .. } => {
            for a in args {
                collect_apply_refs(a, out);
            }
        }
        Predicate::Not { arg } => collect_apply_refs(arg, out),
        Predicate::ForAll { pred: inner, .. } | Predicate::Exists { pred: inner, .. } => {
            collect_apply_refs(inner, out);
        }
        _ => {}
    }
}

/// Walk every rule's antecedent and collect all `Apply` nodes that
/// reference the given packet_id. Returned references share the caller's
/// borrow so callers can inspect `expect` without cloning.
fn rule_antecedents_referencing<'a>(rules: &'a [Rule], packet_id: &str) -> Vec<&'a Predicate> {
    let mut out = Vec::new();
    for rule in rules {
        collect_apply_by_id(&rule.antecedent, packet_id, &mut out);
    }
    out
}

fn collect_apply_by_id<'a>(pred: &'a Predicate, target: &str, out: &mut Vec<&'a Predicate>) {
    match pred {
        Predicate::Apply { packet_id, .. } if packet_id == target => out.push(pred),
        Predicate::All { args } | Predicate::Any { args } => {
            for a in args {
                collect_apply_by_id(a, target, out);
            }
        }
        Predicate::CountMatches { args, .. } => {
            for a in args {
                collect_apply_by_id(a, target, out);
            }
        }
        Predicate::Not { arg } => collect_apply_by_id(arg, target, out),
        Predicate::ForAll { pred: inner, .. } | Predicate::Exists { pred: inner, .. } => {
            collect_apply_by_id(inner, target, out);
        }
        _ => {}
    }
}

/// Verify that every `Apply` node's `expect` only names classifications
/// that exist in the sub-packet's lattice. A typo here silently makes
/// the composition un-matchable; surfacing at compile time saves a
/// debugging round.
fn check_apply_expect_against_lattice(applies: &[&Predicate], sub: &Packet) -> Result<()> {
    for pred in applies {
        if let Predicate::Apply {
            packet_id, expect, ..
        } = pred
        {
            for e in expect {
                if !sub.classification_lattice.iter().any(|c| c == e) {
                    anyhow::bail!(
                        "Apply({packet_id}).expect contains '{e}', which is not in the \
                         referenced packet's lattice {:?}. Typo, or the sub-packet's \
                         lattice was changed?",
                        sub.classification_lattice
                    );
                }
            }
        }
    }
    Ok(())
}

impl RuleInput {
    fn materialize(
        self,
        lattice: &[String],
        prefix_map: &BTreeMap<String, String>,
    ) -> Result<Rule> {
        // Validate the predicate tree at compile time: reject malformed
        // quantified paths and nested-ForAll. This is the phase-4 bros'
        // convergent critique — silent failure on authoring errors was
        // the blocking issue.
        validate_predicate(&self.antecedent, false)
            .with_context(|| format!("in rule '{}'", self.id))?;

        // 1. Explicit classification wins; 2. infer from id prefix;
        // 3. fall back to the lowest-priority classification in the lattice.
        // Track the source so error messages can name the inference path
        // — the `prefix_inference` map is a real footgun when the caller
        // doesn't know about it (E14/Gemini tripped on review_ → manual).
        let (classification, inferred_prefix): (String, Option<String>) =
            if let Some(c) = self.classification.clone() {
                (c, None)
            } else if let Some((c, p)) = infer_classification_detailed(&self.id, prefix_map) {
                (c, Some(p.to_string()))
            } else if let Some(c) = lattice.last().cloned() {
                (c, None)
            } else {
                anyhow::bail!(
                    "rule '{}' has no classification and packet lattice is empty",
                    self.id
                )
            };
        if !lattice.iter().any(|c| c == &classification) {
            let source_hint = match &inferred_prefix {
                Some(prefix) => format!(
                    " (classification '{classification}' was INFERRED from id prefix \
                     '{prefix}' via the packet's prefix_inference map; set an explicit \
                     `classification` field on the rule to override, or revise the \
                     prefix_inference map / rule id)"
                ),
                None => String::new(),
            };
            anyhow::bail!(
                "rule '{}' classification '{}' is not in packet lattice {:?}{}",
                self.id,
                classification,
                lattice,
                source_hint
            );
        }
        Ok(Rule {
            id: self.id,
            antecedent: self.antecedent,
            consequent: self.consequent,
            classification,
            emit: self.emit.unwrap_or_default(),
            confidence: self.confidence,
            provenance: self.provenance,
        })
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
    /// Classification of the rule that fired. Lets callers group/filter
    /// findings mechanically — in `apply_all`, multiple rules fire and
    /// a reviewer typically wants all high-priority classifications first.
    pub classification: String,
}

/// Result of `bbox_apply` in mode="all" — every rule whose antecedent
/// holds emits a finding. `verdict` is the aggregate classification
/// computed from the findings via the packet's lattice precedence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplyAllResult {
    pub packet_id: String,
    pub findings: Vec<Prediction>,
    /// Aggregate verdict: the **highest-priority** classification in the
    /// packet's lattice that any rule fired. The lattice is ordered
    /// highest-first, so the verdict is the first lattice entry present
    /// in the findings list — independent of firing order within the
    /// rule sequence.
    ///
    /// Example: lattice `["fail", "flag", "pass"]`. If findings are
    /// `[{class: "flag"}, {class: "pass"}, {class: "flag"}]`, verdict =
    /// `"flag"` because it's the earliest lattice entry that appears.
    /// If findings only have `pass`, verdict = `"pass"`. If no finding
    /// fired, verdict = `None`.
    ///
    /// Consumers reading `verdict` without knowing the packet's lattice
    /// still get a meaningful domain-specific string (`"deny"`, `"fail"`,
    /// `"blocker"`, etc.) — the lattice knowledge is only needed for
    /// cross-packet comparison, not for acting on a single verdict.
    pub verdict: Option<String>,
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

/// Fidelity report for `audit_mode="all"`. Compares aggregate verdict
/// and the set of fired rule IDs independently; a row can fail on
/// either dimension, and the report tags which one so fixes are
/// targeted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AllModeFidelityReport {
    pub total: usize,
    pub correct: usize,
    pub fidelity: f32,
    pub mismatches: Vec<AllModeMismatch>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AllModeMismatch {
    pub entity: serde_json::Value,
    /// `"verdict"` when aggregate verdict diverged; `"rule_ids"` when
    /// fired-rule-id set diverged; `"both"` when both diverged.
    pub check: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_verdict: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual_verdict: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_rule_ids: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual_rule_ids: Option<Vec<String>>,
}

// ── Evaluator (deterministic, no LLM) ────────────────────────────

/// Augment `entity` with fields derived from `packet.rank_table` /
/// `packet.threshold_table` lookups. Pure function over the entity
/// map; does not mutate the packet.
fn resolve_entity(
    packet: &Packet,
    entity: &serde_json::Map<String, serde_json::Value>,
) -> serde_json::Map<String, serde_json::Value> {
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
    entity_get_raw(entity, field).and_then(|v| Value::from_json(&v))
}

/// Raw JSON lookup with dotted-path support. Tries literal-key first
/// (back-compat for legacy field names containing dots), then walks
/// `head.tail.tail` against the entity, descending into nested objects
/// and array indices. Used by every entity_* helper for consistent
/// path semantics across the predicate evaluator.
fn entity_get_raw(
    entity: &serde_json::Map<String, serde_json::Value>,
    field: &str,
) -> Option<serde_json::Value> {
    if let Some(v) = entity.get(field) {
        return Some(v.clone());
    }
    if !field.contains('.') {
        return None;
    }
    let parts: Vec<&str> = field.split('.').collect();
    let mut cur = entity.get(parts[0])?.clone();
    for seg in &parts[1..] {
        cur = match &cur {
            serde_json::Value::Object(m) => m.get(*seg)?.clone(),
            serde_json::Value::Array(a) => {
                let idx: usize = seg.parse().ok()?;
                a.get(idx)?.clone()
            }
            _ => return None,
        };
    }
    Some(cur)
}

fn entity_int(entity: &serde_json::Map<String, serde_json::Value>, field: &str) -> Option<i64> {
    entity_get_raw(entity, field).and_then(|v| v.as_i64())
}

fn entity_f64(entity: &serde_json::Map<String, serde_json::Value>, field: &str) -> Option<f64> {
    entity_get_raw(entity, field).and_then(|v| v.as_f64())
}

fn entity_has(entity: &serde_json::Map<String, serde_json::Value>, field: &str) -> bool {
    // Key exists AND value is non-null. Used by `IsNonNull`. Distinct
    // from `entity_key_exists` (which counts null as present).
    match entity_get_raw(entity, field) {
        None => false,
        Some(serde_json::Value::Null) => false,
        Some(_) => true,
    }
}

fn entity_key_exists(entity: &serde_json::Map<String, serde_json::Value>, field: &str) -> bool {
    if entity.contains_key(field) {
        return true;
    }
    entity_get_raw(entity, field).is_some()
}

fn entity_is_null(entity: &serde_json::Map<String, serde_json::Value>, field: &str) -> bool {
    matches!(entity_get_raw(entity, field), Some(serde_json::Value::Null))
}

/// Resolve a path like `"tools[*]"` or `"vars.labels[*]"` to the
/// backing array in the entity. Returns `None` if the path doesn't end
/// in `[*]`, the field is missing, or the value isn't an array.
///
/// Dotted-path support (added when the workflow engine started passing
/// structured ArcContext entities — `vars.labels[*]`,
/// `outputs.Plan.findings[*]`, etc.). The flat single-field form
/// keeps working unchanged.
///
/// Returned as an owned Vec because the dotted-path walk has to clone
/// to descend through nested objects; a borrow would need an arena.
fn resolve_collection(
    entity: &serde_json::Map<String, serde_json::Value>,
    path: &str,
) -> Option<Vec<serde_json::Value>> {
    let field = path.strip_suffix("[*]")?;
    let value = if field.contains('.') {
        entity_get_raw(entity, field)?
    } else {
        entity.get(field)?.clone()
    };
    match value {
        serde_json::Value::Array(a) => Some(a),
        _ => None,
    }
}

/// Depth limit for `Apply` (packet composition). Prevents unbounded
/// recursion if a packet graph contains a cycle or a very deep chain.
/// Bumps here should be paired with a concrete use case — most real
/// compositions are two or three levels deep.
pub const MAX_COMPOSITION_DEPTH: usize = 8;

/// Packet lookup for composition. The daemon's [`Packets`] store is the
/// canonical implementation; tests can use [`NoopResolver`] when the
/// predicate under test doesn't exercise `Apply`.
pub trait PacketResolver {
    fn resolve(&self, packet_id: &str) -> Option<Packet>;
}

/// Resolver that never finds a packet. Handy as a default when the
/// caller hasn't wired composition.
pub struct NoopResolver;

impl PacketResolver for NoopResolver {
    fn resolve(&self, _: &str) -> Option<Packet> {
        None
    }
}

impl PacketResolver for Packets {
    fn resolve(&self, packet_id: &str) -> Option<Packet> {
        self.load(packet_id).ok()
    }
}

/// Rebind caller fields into sub-packet field names before composition.
/// Outer entity fields specified in `entity_map` values populate sub
/// entity fields named by the keys. Unmapped fields pass through.
fn apply_entity_map(
    entity: &serde_json::Map<String, serde_json::Value>,
    entity_map: &BTreeMap<String, String>,
) -> serde_json::Map<String, serde_json::Value> {
    if entity_map.is_empty() {
        return entity.clone();
    }
    let mut out = entity.clone();
    for (sub_key, outer_key) in entity_map {
        if let Some(v) = entity.get(outer_key) {
            out.insert(sub_key.clone(), v.clone());
        }
    }
    out
}

/// Wrap a JSON value as a sub-entity map suitable for `eval_predicate`.
/// Objects pass through unchanged. Primitives become `{"$": value}` so
/// predicates inside `ForAll`/`Exists` can address them via the
/// special `"$"` field.
fn as_sub_entity(item: &serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
    match item {
        serde_json::Value::Object(obj) => obj.clone(),
        other => {
            let mut m = serde_json::Map::new();
            m.insert("$".to_string(), other.clone());
            m
        }
    }
}

/// Evaluate a predicate against a resolved entity. Pure with respect
/// to `entity` — no I/O on the entity side. Cross-field and
/// applicability predicates return `false` on missing / malformed
/// inputs rather than panicking or erroring; compose with `KeyExists`
/// / `IsMissing` / `IsNonNull` / `IsNull` inside `All` when
/// applicability guards are wanted.
///
/// `resolver` is used only by `Apply` for packet composition; other
/// arms ignore it. `depth` tracks composition depth to bound
/// recursion at [`MAX_COMPOSITION_DEPTH`]; top-level callers start at
/// 0.
fn eval_predicate(
    p: &Predicate,
    entity: &serde_json::Map<String, serde_json::Value>,
    resolver: &dyn PacketResolver,
    depth: usize,
) -> bool {
    match p {
        Predicate::AlwaysTrue {} => true,
        Predicate::AlwaysFalse {} => false,
        Predicate::Eq { field, value } => entity_get(entity, field).as_ref() == Some(value),
        Predicate::Ge { field, value } => entity_int(entity, field)
            .map(|v| v >= *value)
            .unwrap_or(false),
        Predicate::Gt { field, value } => entity_int(entity, field)
            .map(|v| v > *value)
            .unwrap_or(false),
        Predicate::Le { field, value } => entity_int(entity, field)
            .map(|v| v <= *value)
            .unwrap_or(false),
        Predicate::Lt { field, value } => entity_int(entity, field)
            .map(|v| v < *value)
            .unwrap_or(false),
        Predicate::GeF { field, value } => entity_f64(entity, field)
            .map(|v| v >= *value)
            .unwrap_or(false),
        Predicate::GtF { field, value } => entity_f64(entity, field)
            .map(|v| v > *value)
            .unwrap_or(false),
        Predicate::LeF { field, value } => entity_f64(entity, field)
            .map(|v| v <= *value)
            .unwrap_or(false),
        Predicate::LtF { field, value } => entity_f64(entity, field)
            .map(|v| v < *value)
            .unwrap_or(false),
        Predicate::KeyExists { field } => entity_key_exists(entity, field),
        Predicate::IsNull { field } => entity_is_null(entity, field),
        Predicate::IsNonNull { field } => entity_has(entity, field),
        Predicate::IsMissing { field } => !entity_key_exists(entity, field),
        Predicate::FieldEq {
            lhs_field,
            rhs_field,
        } => match (entity_get(entity, lhs_field), entity_get(entity, rhs_field)) {
            (Some(l), Some(r)) => l == r,
            _ => false,
        },
        Predicate::FieldGt {
            lhs_field,
            rhs_field,
        } => match (entity_int(entity, lhs_field), entity_int(entity, rhs_field)) {
            (Some(l), Some(r)) => l > r,
            _ => false,
        },
        Predicate::FieldGe {
            lhs_field,
            rhs_field,
        } => match (entity_int(entity, lhs_field), entity_int(entity, rhs_field)) {
            (Some(l), Some(r)) => l >= r,
            _ => false,
        },
        Predicate::FieldLt {
            lhs_field,
            rhs_field,
        } => match (entity_int(entity, lhs_field), entity_int(entity, rhs_field)) {
            (Some(l), Some(r)) => l < r,
            _ => false,
        },
        Predicate::FieldLe {
            lhs_field,
            rhs_field,
        } => match (entity_int(entity, lhs_field), entity_int(entity, rhs_field)) {
            (Some(l), Some(r)) => l <= r,
            _ => false,
        },
        Predicate::RankGeFieldThreshold {
            rank_field,
            threshold_field,
        } => {
            match (
                entity_int(entity, rank_field),
                entity_int(entity, threshold_field),
            ) {
                (Some(r), Some(t)) => r >= t,
                _ => false,
            }
        }
        Predicate::All { args } => args
            .iter()
            .all(|arg| eval_predicate(arg, entity, resolver, depth)),
        Predicate::Any { args } => args
            .iter()
            .any(|arg| eval_predicate(arg, entity, resolver, depth)),
        Predicate::Not { arg } => !eval_predicate(arg, entity, resolver, depth),
        Predicate::ForAll { path, pred } => match resolve_collection(entity, path) {
            // Vacuous truth: empty or missing collection → true. Matches
            // the standard ∀x∈∅: P(x) convention.
            None => true,
            Some(items) => items.iter().all(|item| {
                let sub = as_sub_entity(item);
                eval_predicate(pred, &sub, resolver, depth)
            }),
        },
        Predicate::Exists { path, pred } => match resolve_collection(entity, path) {
            // Empty set has no witness → ∃x∈∅: P(x) is false.
            None => false,
            Some(items) => items.iter().any(|item| {
                let sub = as_sub_entity(item);
                eval_predicate(pred, &sub, resolver, depth)
            }),
        },
        Predicate::CountCmp {
            path,
            compare,
            value,
        } => {
            let n = resolve_collection(entity, path)
                .map(|a| a.len())
                .unwrap_or(0);
            compare.apply(n, *value)
        }
        Predicate::StringContains {
            field,
            needle,
            case_insensitive,
        } => {
            let haystack = match entity.get(field) {
                Some(serde_json::Value::String(s)) => s.as_str(),
                _ => return false,
            };
            if *case_insensitive {
                haystack.to_lowercase().contains(&needle.to_lowercase())
            } else {
                haystack.contains(needle.as_str())
            }
        }
        Predicate::InRange { field, min, max } => entity_int(entity, field)
            .map(|v| v >= *min && v <= *max)
            .unwrap_or(false),
        Predicate::InRangeF { field, min, max } => entity_f64(entity, field)
            .map(|v| v >= *min && v <= *max)
            .unwrap_or(false),
        Predicate::CountMatches {
            args,
            compare,
            value,
        } => {
            let count = args
                .iter()
                .filter(|p| eval_predicate(p, entity, resolver, depth))
                .count();
            compare.apply(count, *value)
        }
        Predicate::In { field, values } => {
            let v = match entity_get_raw(entity, field) {
                Some(v) => v,
                None => return false,
            };
            let typed = match Value::from_json(&v) {
                Some(t) => t,
                None => return false,
            };
            values.iter().any(|cand| cand == &typed)
        }
        Predicate::StringMatches {
            field,
            pattern,
            case_insensitive,
        } => {
            let s = match entity_get_raw(entity, field) {
                Some(serde_json::Value::String(s)) => s,
                _ => return false,
            };
            let mut builder = regex::RegexBuilder::new(pattern);
            builder.case_insensitive(*case_insensitive);
            match builder.build() {
                Ok(re) => re.is_match(&s),
                Err(e) => {
                    tracing::warn!(
                        "StringMatches: invalid regex pattern {pattern:?}: {e}"
                    );
                    false
                }
            }
        }
        Predicate::Apply {
            packet_id,
            expect,
            entity_map,
        } => {
            if depth >= MAX_COMPOSITION_DEPTH {
                tracing::warn!(
                    packet_id = %packet_id,
                    depth,
                    "Apply: composition depth limit exceeded, returning false"
                );
                return false;
            }
            let sub = match resolver.resolve(packet_id) {
                Some(p) => p,
                None => {
                    tracing::warn!(
                        packet_id = %packet_id,
                        "Apply: referenced packet not found, returning false"
                    );
                    return false;
                }
            };
            let mapped = apply_entity_map(entity, entity_map);
            let sub_resolved = resolve_entity(&sub, &mapped);
            for rule in &sub.rules {
                if eval_predicate(&rule.antecedent, &sub_resolved, resolver, depth + 1) {
                    return expect.contains(&rule.classification);
                }
            }
            // No sub-rule fired; outer predicate can't match unless the
            // caller explicitly expected "no match" — but that's not a
            // classification value, so this is always false.
            false
        }
    }
}

/// Apply a packet to an entity without packet composition. Returns
/// the first matching rule's prediction, or None when no rule matches.
/// `Apply` predicates in the tree silently return false because there
/// is no resolver — use [`apply_with`] for composition-aware
/// evaluation.
pub fn apply(packet: &Packet, entity: &serde_json::Value) -> Option<Prediction> {
    apply_with(packet, entity, &NoopResolver)
}

/// Apply a packet with composition support. The resolver looks up
/// referenced packets for [`Predicate::Apply`] nodes. Pass the daemon's
/// [`Packets`] store here for production; tests without composition
/// can use [`NoopResolver`].
pub fn apply_with(
    packet: &Packet,
    entity: &serde_json::Value,
    resolver: &dyn PacketResolver,
) -> Option<Prediction> {
    let entity_obj = entity.as_object()?;
    let resolved = resolve_entity(packet, entity_obj);

    for rule in &packet.rules {
        if eval_predicate(&rule.antecedent, &resolved, resolver, 0) {
            return Some(Prediction {
                rule_id: rule.id.clone(),
                consequent: rule.consequent.clone(),
                confidence: rule.confidence,
                classification: rule.classification.clone(),
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
    apply_all_with(packet, entity, &NoopResolver)
}

/// Composition-aware variant of [`apply_all`]. Pass the daemon's
/// [`Packets`] store to let `Apply` predicates resolve other packets.
pub fn apply_all_with(
    packet: &Packet,
    entity: &serde_json::Value,
    resolver: &dyn PacketResolver,
) -> ApplyAllResult {
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
        .filter(|rule| eval_predicate(&rule.antecedent, &resolved, resolver, 0))
        .map(|rule| Prediction {
            rule_id: rule.id.clone(),
            consequent: rule.consequent.clone(),
            confidence: rule.confidence,
            classification: rule.classification.clone(),
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
            .filter(|rule| eval_predicate(&rule.antecedent, &resolved, resolver, 0))
            .map(|rule| Prediction {
                rule_id: rule.id.clone(),
                consequent: rule.consequent.clone(),
                confidence: rule.confidence,
                classification: rule.classification.clone(),
            })
            .collect()
    } else {
        Vec::new()
    };

    let findings: Vec<Prediction> = independent_findings
        .into_iter()
        .chain(fallback_findings)
        .collect();

    // Aggregate verdict = first classification in the lattice that any
    // rule fired. Lattice ordering IS the precedence — domain-specific.
    let verdict = packet
        .classification_lattice
        .iter()
        .find(|class| findings.iter().any(|p| &&p.classification == class))
        .cloned();

    ApplyAllResult {
        packet_id: packet.id.clone(),
        findings,
        verdict,
    }
}

/// Apply packet in `mode="all"` to every row of `dataset`. Row shape:
/// `{entity, expected_verdict?, expected_rule_ids?}`. Compares aggregate
/// verdict + fired-rule-id set independently; mismatches tag which
/// check failed so fixes are targeted.
pub fn verify_all(packet: &Packet, dataset: &serde_json::Value) -> Result<AllModeFidelityReport> {
    verify_all_with(packet, dataset, &NoopResolver)
}

/// Composition-aware variant of [`verify_all`].
pub fn verify_all_with(
    packet: &Packet,
    dataset: &serde_json::Value,
    resolver: &dyn PacketResolver,
) -> Result<AllModeFidelityReport> {
    let rows = dataset.as_array().context(
        "dataset must be a JSON array of {entity, expected_verdict?, expected_rule_ids?} objects",
    )?;

    let mut total = 0usize;
    let mut correct = 0usize;
    let mut mismatches = Vec::new();

    for row in rows {
        let entity = row
            .get("entity")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let expected_verdict: Option<String> = row
            .get("expected_verdict")
            .and_then(|v| v.as_str().map(|s| s.to_string()));
        let expected_rule_ids: Option<Vec<String>> = row
            .get("expected_rule_ids")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            });

        // Row with no expectation at all trivially passes but doesn't
        // count toward fidelity — skip.
        if expected_verdict.is_none() && expected_rule_ids.is_none() {
            continue;
        }

        total += 1;
        let result = apply_all_with(packet, &entity, resolver);
        let actual_verdict = result.verdict.clone();
        let mut actual_rule_ids: Vec<String> =
            result.findings.iter().map(|p| p.rule_id.clone()).collect();
        actual_rule_ids.sort();

        let verdict_ok = expected_verdict
            .as_ref()
            .map(|ev| actual_verdict.as_ref() == Some(ev))
            .unwrap_or(true);

        let mut expected_ids_sorted = expected_rule_ids.clone();
        if let Some(ids) = expected_ids_sorted.as_mut() {
            ids.sort();
        }
        let ids_ok = expected_ids_sorted
            .as_ref()
            .map(|eids| &actual_rule_ids == eids)
            .unwrap_or(true);

        if verdict_ok && ids_ok {
            correct += 1;
        } else {
            let check = match (verdict_ok, ids_ok) {
                (false, false) => "both",
                (false, true) => "verdict",
                (true, false) => "rule_ids",
                _ => unreachable!(),
            };
            mismatches.push(AllModeMismatch {
                entity: entity.clone(),
                check: check.to_string(),
                expected_verdict: if !verdict_ok {
                    expected_verdict.clone()
                } else {
                    None
                },
                actual_verdict: if !verdict_ok { actual_verdict } else { None },
                expected_rule_ids: if !ids_ok { expected_ids_sorted } else { None },
                actual_rule_ids: if !ids_ok { Some(actual_rule_ids) } else { None },
            });
        }
    }

    let fidelity = if total == 0 {
        0.0
    } else {
        correct as f32 / total as f32
    };

    Ok(AllModeFidelityReport {
        total,
        correct,
        fidelity,
        mismatches,
    })
}

/// Apply packet to every entry in `dataset`. Dataset is a JSON array of
/// `{entity, expected}` pairs. Returns a fidelity report.
pub fn verify(packet: &Packet, dataset: &serde_json::Value) -> Result<FidelityReport> {
    verify_with(packet, dataset, &NoopResolver)
}

/// Composition-aware variant of [`verify`].
pub fn verify_with(
    packet: &Packet,
    dataset: &serde_json::Value,
    resolver: &dyn PacketResolver,
) -> Result<FidelityReport> {
    let rows = dataset
        .as_array()
        .context("dataset must be a JSON array of {entity, expected} objects")?;

    let mut total = 0usize;
    let mut correct = 0usize;
    let mut mismatches = Vec::new();
    let mut uncovered = Vec::new();

    for row in rows {
        let entity = row
            .get("entity")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let expected_json = row
            .get("expected")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let expected = match Value::from_json(&expected_json) {
            Some(v) => v,
            None => continue, // skip malformed rows
        };

        total += 1;
        match apply_with(packet, &entity, resolver) {
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

fn events_log_path(state_dir: &Path) -> PathBuf {
    packets_dir(state_dir).join("events.jsonl")
}

/// Atomic-ish append: opens the file in append mode and writes one
/// line terminated by `\n`. Small concurrent writes on Linux are
/// atomic below PIPE_BUF (~4KiB), which covers every plausible event
/// payload. No rotation in v1 — revisit if the log grows unbounded.
fn append_line(path: &Path, line: &str) -> Result<()> {
    use std::io::Write;
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    f.write_all(line.as_bytes())?;
    f.write_all(b"\n")?;
    Ok(())
}

// ── Event log ─────────────────────────────────────────────────────

/// One entry in the packet event log. Written as a JSON line on every
/// `compile`, `apply`, `audit`, or `gap` operation. The log is the
/// discovery-layer's observability surface: compile errors and
/// `bbox_packet_gap` entries tell us which primitives are missing in
/// practice; low-fidelity audits tell us which packets drifted; no-
/// match applies tell us where catchall rules are missing.
///
/// Deliberately small and un-versioned in v1 — rotate or migrate when
/// a real use case surfaces. Query via `bbox_packet_events`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PacketEvent {
    /// ISO 8601 timestamp.
    pub timestamp: String,
    /// `compile` | `apply` | `audit` | `gap`.
    pub op: String,
    /// `ok` | `error` | `no_match` | `low_fidelity` | `logged` (for gaps).
    pub outcome: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub packet_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    /// Op-specific structured payload. Schema: compile → {rules_count,
    /// lattice_size, referenced_packets?}. apply → {mode, matched,
    /// rule_id?, verdict?}. audit → {mode, total, correct, fidelity,
    /// mismatch_count}. gap → {description, attempted_sketch?,
    /// fallback_used?, ast_feature_requested?}.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub details: serde_json::Value,
}

impl PacketEvent {
    fn now(op: &str, outcome: &str) -> Self {
        Self {
            timestamp: crate::util::now_iso(),
            op: op.to_string(),
            outcome: outcome.to_string(),
            packet_id: None,
            domain: None,
            details: serde_json::Value::Null,
        }
    }

    fn with_packet_id(mut self, id: impl Into<String>) -> Self {
        self.packet_id = Some(id.into());
        self
    }

    fn with_domain(mut self, d: impl Into<String>) -> Self {
        self.domain = Some(d.into());
        self
    }

    fn with_details(mut self, d: serde_json::Value) -> Self {
        self.details = d;
        self
    }
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

    /// Append one event to the log. Best-effort: errors are logged
    /// via tracing but never propagate, so event-log I/O can never
    /// break a compile/apply/audit operation.
    fn append_event(&self, event: &PacketEvent) {
        let path = events_log_path(&self.state_dir);
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
        let path = events_log_path(&self.state_dir);
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

    /// Record a packet-authoring gap — "I wanted to compile but the
    /// AST couldn't express this." High-signal input for prioritizing
    /// new predicate primitives.
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

    fn save_packet(&self, packet: &Packet) -> Result<()> {
        let dir = scope_dir(&self.state_dir, &packet.scope);
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
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
        anyhow::bail!("Packet not found: {id} (expected `packet-<8hex>`, e.g. `packet-a1b2c3d4`)")
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
        let result = self.compile_inner(p);
        // Log the outcome regardless of success/failure. Event-log
        // I/O errors are swallowed inside append_event so they can't
        // mask the real result.
        match &result {
            Ok(packet) => {
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
                        .with_domain(p.domain.clone())
                        .with_details(details),
                );
            }
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

    fn compile_inner(&self, p: &CompileParams) -> Result<Packet> {
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

        self.save_packet(&packet)?;

        Ok(packet)
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

/// Absorb a provider-serialization quirk observed in E12: some MCP
/// clients (notably Codex on first-attempt) pass structured params
/// as stringified JSON rather than structured arrays/objects. This
/// helper inspects a value — if it's a `String` that starts with
/// `{` or `[` and parses as JSON, it replaces the string with the
/// parsed value in place. No-op on already-structured values.
///
/// Applied at the tool boundary so the wire shape from the agent
/// doesn't need to be pixel-perfect to succeed. Trade: an agent who
/// genuinely wants a JSON-literal string as input (very unusual in
/// this surface) sees it coerced to structure. That's the right
/// trade for an AI-facing API where the first-attempt cost of retry
/// is much higher than the near-zero cost of permissive parsing.
fn unwrap_jsonish(v: &mut serde_json::Value) {
    if let serde_json::Value::String(s) = v {
        let trimmed = s.trim_start();
        if trimmed.starts_with('{') || trimmed.starts_with('[') {
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(s) {
                *v = parsed;
            }
        }
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

// ── Self-heal scanner ─────────────────────────────────────────────
//
// A periodic read-only walk over the packet event log that flags
// candidates for repair: packets whose applies miss too often (no-match
// rate above threshold) or whose audits show fidelity drift. Emits
// `op="repair_candidate"` events; does NOT auto-dispatch. An orchestrator
// or the human operator decides what to do with the signal.
//
// Off by default. Enable via `PACKET_SELF_HEAL_ENABLED=true`.
//
// This is intentionally scanner-only. A future revision may add an
// opt-in dispatcher that spawns an AST-repair agent when a candidate
// fires; that's its own feature and lives behind a separate flag.

const ENV_SELF_HEAL_ENABLED: &str = "PACKET_SELF_HEAL_ENABLED";
const ENV_SELF_HEAL_INTERVAL_SECS: &str = "PACKET_SELF_HEAL_INTERVAL_SECS";
const ENV_SELF_HEAL_WINDOW_HOURS: &str = "PACKET_SELF_HEAL_WINDOW_HOURS";
const ENV_SELF_HEAL_NO_MATCH_THRESHOLD: &str = "PACKET_SELF_HEAL_NO_MATCH_THRESHOLD";
const ENV_SELF_HEAL_FIDELITY_THRESHOLD: &str = "PACKET_SELF_HEAL_FIDELITY_THRESHOLD";
const ENV_SELF_HEAL_MIN_APPLIES: &str = "PACKET_SELF_HEAL_MIN_APPLIES";
const ENV_SELF_HEAL_COOLDOWN_HOURS: &str = "PACKET_SELF_HEAL_COOLDOWN_HOURS";

/// Default interval between scanner ticks when the env var is unset.
/// Hourly is cheap for a log-walk and matches the scale at which
/// packet behaviour drifts meaningfully.
const DEFAULT_INTERVAL_SECS: u64 = 3600;
const DEFAULT_WINDOW_HOURS: u64 = 24;
const DEFAULT_NO_MATCH_THRESHOLD: f32 = 0.2;
const DEFAULT_FIDELITY_THRESHOLD: f32 = 0.9;
const DEFAULT_MIN_APPLIES: usize = 5;
const DEFAULT_COOLDOWN_HOURS: u64 = 24;

#[derive(Debug, Clone)]
pub struct ScannerConfig {
    pub enabled: bool,
    pub interval: Duration,
    /// Trailing window over which apply / audit events are aggregated.
    pub window: Duration,
    /// Fraction of applies that must be no_match to flag (e.g. 0.2 = 20%).
    pub no_match_threshold: f32,
    /// Fidelity floor — the most recent audit below this flags the packet.
    pub fidelity_threshold: f32,
    /// Minimum apply count in the window before no_match_rate is trusted.
    /// Prevents flagging packets that have only been tried once or twice.
    pub min_apply_samples: usize,
    /// Suppression window after a candidate event fires for a packet, so
    /// the same packet isn't spammed every tick until the operator acts.
    pub repair_cooldown: Duration,
}

impl Default for ScannerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            interval: Duration::from_secs(DEFAULT_INTERVAL_SECS),
            window: Duration::from_secs(DEFAULT_WINDOW_HOURS * 3600),
            no_match_threshold: DEFAULT_NO_MATCH_THRESHOLD,
            fidelity_threshold: DEFAULT_FIDELITY_THRESHOLD,
            min_apply_samples: DEFAULT_MIN_APPLIES,
            repair_cooldown: Duration::from_secs(DEFAULT_COOLDOWN_HOURS * 3600),
        }
    }
}

impl ScannerConfig {
    pub fn from_env() -> Self {
        fn flag(name: &str) -> Option<bool> {
            std::env::var(name).ok().map(|v| {
                let v = v.to_ascii_lowercase();
                matches!(v.as_str(), "1" | "true" | "yes" | "on")
            })
        }
        fn u64_env(name: &str) -> Option<u64> {
            std::env::var(name).ok().and_then(|v| v.parse().ok())
        }
        fn f32_env(name: &str) -> Option<f32> {
            std::env::var(name).ok().and_then(|v| v.parse().ok())
        }
        fn usize_env(name: &str) -> Option<usize> {
            std::env::var(name).ok().and_then(|v| v.parse().ok())
        }

        let mut cfg = ScannerConfig::default();
        if let Some(on) = flag(ENV_SELF_HEAL_ENABLED) {
            cfg.enabled = on;
        }
        if let Some(s) = u64_env(ENV_SELF_HEAL_INTERVAL_SECS) {
            cfg.interval = Duration::from_secs(s.max(10));
        }
        if let Some(h) = u64_env(ENV_SELF_HEAL_WINDOW_HOURS) {
            cfg.window = Duration::from_secs(h.max(1) * 3600);
        }
        if let Some(t) = f32_env(ENV_SELF_HEAL_NO_MATCH_THRESHOLD) {
            cfg.no_match_threshold = t.clamp(0.0, 1.0);
        }
        if let Some(t) = f32_env(ENV_SELF_HEAL_FIDELITY_THRESHOLD) {
            cfg.fidelity_threshold = t.clamp(0.0, 1.0);
        }
        if let Some(n) = usize_env(ENV_SELF_HEAL_MIN_APPLIES) {
            cfg.min_apply_samples = n;
        }
        if let Some(h) = u64_env(ENV_SELF_HEAL_COOLDOWN_HOURS) {
            cfg.repair_cooldown = Duration::from_secs(h * 3600);
        }
        cfg
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RepairCandidate {
    pub packet_id: String,
    pub domain: Option<String>,
    /// Machine-readable reason tags: `"high_no_match_rate"`, `"low_fidelity"`.
    pub reasons: Vec<String>,
    pub apply_count: usize,
    pub no_match_count: usize,
    pub no_match_rate: Option<f32>,
    pub fidelity: Option<f32>,
    pub last_audit_timestamp: Option<String>,
}

/// Subtract `dur` from `now` in ISO-8601 lexicographic space. We use
/// string comparison on ISO-8601 timestamps (already done in
/// `list_events`), so the cutoff is a string too. Returns the cutoff
/// timestamp. On clock parse failure, returns `None` — the caller should
/// fall back to "keep everything".
fn window_cutoff(now_iso: &str, dur: Duration) -> Option<String> {
    let now = chrono::DateTime::parse_from_rfc3339(now_iso).ok()?;
    let cutoff = now.checked_sub_signed(chrono::Duration::from_std(dur).ok()?)?;
    Some(cutoff.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}

/// Pure function. Given a newest-first slice of events and a config,
/// return the packets that qualify for repair this tick. Events older
/// than the window are ignored. Packets with a recent
/// `repair_candidate` event inside the cooldown are skipped.
pub fn find_repair_candidates(
    events: &[PacketEvent],
    config: &ScannerConfig,
    now_iso: &str,
) -> Vec<RepairCandidate> {
    let cutoff = window_cutoff(now_iso, config.window);
    let cooldown_cutoff = window_cutoff(now_iso, config.repair_cooldown);

    // Aggregate per packet_id.
    struct Agg {
        domain: Option<String>,
        applies: usize,
        no_matches: usize,
        latest_fidelity: Option<f32>,
        latest_audit_ts: Option<String>,
        latest_candidate_ts: Option<String>,
    }
    let mut agg: BTreeMap<String, Agg> = BTreeMap::new();
    for ev in events {
        let Some(pid) = ev.packet_id.as_ref() else {
            continue;
        };
        let in_window = match &cutoff {
            Some(c) => ev.timestamp.as_str() >= c.as_str(),
            None => true,
        };

        let slot = agg.entry(pid.clone()).or_insert_with(|| Agg {
            domain: ev.domain.clone(),
            applies: 0,
            no_matches: 0,
            latest_fidelity: None,
            latest_audit_ts: None,
            latest_candidate_ts: None,
        });
        if slot.domain.is_none() {
            slot.domain = ev.domain.clone();
        }

        // Cooldown tracking always looks at the latest candidate
        // regardless of window, so a stale-but-recent candidate still
        // suppresses re-flag.
        if ev.op == "repair_candidate" {
            if slot
                .latest_candidate_ts
                .as_deref()
                .is_none_or(|t| t < ev.timestamp.as_str())
            {
                slot.latest_candidate_ts = Some(ev.timestamp.clone());
            }
            continue;
        }

        if !in_window {
            continue;
        }

        match ev.op.as_str() {
            "apply" => {
                slot.applies += 1;
                if ev.outcome == "no_match" {
                    slot.no_matches += 1;
                }
            }
            "audit" => {
                // Latest audit wins (events are newest-first, so first
                // audit we see for this packet is the freshest).
                if slot.latest_audit_ts.is_none() {
                    if let Some(f) = ev.details.get("fidelity").and_then(|v| v.as_f64()) {
                        slot.latest_fidelity = Some(f as f32);
                        slot.latest_audit_ts = Some(ev.timestamp.clone());
                    }
                }
            }
            _ => {}
        }
    }

    let mut out = Vec::new();
    for (pid, a) in agg {
        // Skip packets still inside cooldown.
        if let (Some(ts), Some(cd)) = (&a.latest_candidate_ts, &cooldown_cutoff) {
            if ts.as_str() >= cd.as_str() {
                continue;
            }
        }

        let no_match_rate = if a.applies >= config.min_apply_samples {
            Some(a.no_matches as f32 / a.applies as f32)
        } else {
            None
        };

        let mut reasons: Vec<String> = Vec::new();
        if let Some(rate) = no_match_rate {
            if rate >= config.no_match_threshold {
                reasons.push("high_no_match_rate".to_string());
            }
        }
        if let Some(f) = a.latest_fidelity {
            if f < config.fidelity_threshold {
                reasons.push("low_fidelity".to_string());
            }
        }
        if reasons.is_empty() {
            continue;
        }

        out.push(RepairCandidate {
            packet_id: pid,
            domain: a.domain,
            reasons,
            apply_count: a.applies,
            no_match_count: a.no_matches,
            no_match_rate,
            fidelity: a.latest_fidelity,
            last_audit_timestamp: a.latest_audit_ts,
        });
    }
    out
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
                classification: "info".to_string(),
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
                classification: "info".to_string(),
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
                classification: "info".to_string(),
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
                classification: "info".to_string(),
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
                classification: "info".to_string(),
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
                classification: "info".to_string(),
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
                classification: "info".to_string(),
                emit: Emit::Independent,
                confidence: 1.0,
                provenance: vec![],
            },
            // Catch-all deny
            Rule {
                id: "default_deny".into(),
                antecedent: Predicate::AlwaysTrue {},
                consequent: Value::String("DENY".into()),
                classification: "info".to_string(),
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
            classification_lattice: review_lattice(),
            prefix_inference: review_prefix_inference(),
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
            correct,
            125,
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
            mode: None,
        };
        let report_text = store.audit_tool(&audit_params).unwrap();
        assert!(report_text.contains("\"total\": 5"));
        assert!(report_text.contains("\"correct\": 5"));
        assert!(report_text.contains("\"fidelity\": 1.0"));
    }

    #[test]
    fn packet_list_helpers_match_and_summarize() {
        // Exercises the helpers bbox_packet_list / bbox_knowledge share:
        // packet_matches_query and packet_summary. Covers substring match
        // across id / domain / rule ids / classifications, the classification
        // histogram, the rule-id preview, and the latest-per-domain dedup
        // semantics (consumers rely on list_all being newest-first).

        fn mk_rule(id: &str, cls: &str) -> Rule {
            Rule {
                id: id.to_string(),
                antecedent: Predicate::AlwaysTrue {},
                consequent: Value::String(cls.to_uppercase()),
                classification: cls.to_string(),
                emit: Emit::Independent,
                confidence: 1.0,
                provenance: vec![],
            }
        }

        fn mk_packet(id: &str, domain: &str, created_at: &str, rules: Vec<Rule>) -> Packet {
            Packet {
                id: id.to_string(),
                domain: domain.to_string(),
                scope: "global".to_string(),
                project: None,
                rank_table: BTreeMap::new(),
                threshold_table: BTreeMap::new(),
                rank_lookup_key: default_rank_lookup_key(),
                threshold_lookup_key: default_threshold_lookup_key(),
                classification_lattice: vec![
                    "fail".to_string(),
                    "flag".to_string(),
                    "pass".to_string(),
                ],
                prefix_inference: BTreeMap::new(),
                rules,
                source_ids: vec![],
                self_audit_fidelity: None,
                created_at: created_at.to_string(),
                updated_at: created_at.to_string(),
                superseded_by: None,
                merged_from: vec![],
            }
        }

        let pr_triage_old = mk_packet(
            "packet-11111111",
            "pr-triage",
            "2026-01-01T00:00:00Z",
            vec![
                mk_rule("breaking_api_change", "fail"),
                mk_rule("missing_tests", "flag"),
            ],
        );
        let pr_triage_new = mk_packet(
            "packet-22222222",
            "pr-triage",
            "2026-04-19T00:00:00Z",
            vec![
                mk_rule("breaking_api_change", "fail"),
                mk_rule("missing_tests", "flag"),
                mk_rule("all_clean", "pass"),
            ],
        );
        let auth_matrix = mk_packet(
            "packet-33333333",
            "auth-matrix",
            "2026-02-14T00:00:00Z",
            vec![
                mk_rule("deny_reader_team", "fail"),
                mk_rule("allow_owner_any", "pass"),
            ],
        );

        // --- packet_matches_query ---
        // domain hit (case-insensitive)
        assert!(packet_matches_query(&pr_triage_new, "PR-triage"));
        // rule id hit
        assert!(packet_matches_query(&pr_triage_new, "breaking"));
        // classification lattice hit
        assert!(packet_matches_query(&pr_triage_new, "FAIL"));
        // id hit
        assert!(packet_matches_query(&auth_matrix, "33333333"));
        // miss
        assert!(!packet_matches_query(&auth_matrix, "retry"));
        // empty query degenerates to false — caller is responsible for
        // skipping the filter on empty queries; the helper just answers.
        assert!(!packet_matches_query(&auth_matrix, "zzzzz"));

        // --- packet_summary ---
        let summary = packet_summary(&pr_triage_new);
        assert_eq!(summary["id"], "packet-22222222");
        assert_eq!(summary["domain"], "pr-triage");
        assert_eq!(summary["rules_count"], 3);
        // Histogram counts by classification
        let hist = &summary["classification_histogram"];
        assert_eq!(hist["fail"], 1);
        assert_eq!(hist["flag"], 1);
        assert_eq!(hist["pass"], 1);
        // Rule-id preview capped at 3
        let preview = summary["rule_ids_preview"].as_array().unwrap();
        assert_eq!(preview.len(), 3);
        assert_eq!(preview[0], "breaking_api_change");

        // --- list_all ordering + latest_per_domain dedup ---
        // Save via a tmp store and confirm list_all returns newest-first.
        let (_dir, store) = tmp_packets();
        store.save_packet(&pr_triage_old).unwrap();
        store.save_packet(&pr_triage_new).unwrap();
        store.save_packet(&auth_matrix).unwrap();

        let listed = store.list_all().unwrap();
        assert_eq!(listed.len(), 3);
        // Newest first: pr_triage_new (Apr), auth_matrix (Feb), pr_triage_old (Jan)
        assert_eq!(listed[0].id, "packet-22222222");
        assert_eq!(listed[1].id, "packet-33333333");
        assert_eq!(listed[2].id, "packet-11111111");

        // Simulate the latest_per_domain filter used by bbox_packet_list.
        let mut seen = std::collections::HashSet::new();
        let deduped: Vec<_> = listed
            .into_iter()
            .filter(|pkt| seen.insert(pkt.domain.clone()))
            .collect();
        assert_eq!(deduped.len(), 2);
        assert_eq!(deduped[0].id, "packet-22222222");
        assert_eq!(deduped[1].id, "packet-33333333");
    }

    #[test]
    fn self_heal_scanner_flags_and_dedups() {
        // Synthetic event stream covering the three cases the scanner
        // cares about:
        //   A. high no_match rate, enough samples → flag
        //   B. low fidelity from latest audit → flag
        //   C. noisy but low-sample (below min_apply_samples) → don't flag
        //   D. recent repair_candidate within cooldown → suppress
        //
        // The scanner consumes events newest-first (matching list_events).
        // We construct timestamps explicitly so the test is independent of
        // wall clock.

        fn ev(
            op: &str,
            outcome: &str,
            pid: &str,
            ts: &str,
            details: serde_json::Value,
        ) -> PacketEvent {
            let mut e = PacketEvent::now(op, outcome);
            e.timestamp = ts.to_string();
            e.packet_id = Some(pid.to_string());
            e.domain = Some(format!("dom-{pid}"));
            e.details = details;
            e
        }

        let now = "2026-04-20T12:00:00Z";
        let in_window = "2026-04-20T06:00:00Z"; // 6h ago
        let out_of_window = "2026-04-18T00:00:00Z"; // >24h

        let mut events: Vec<PacketEvent> = Vec::new();

        // A: 8 applies in window, 4 no_match → 0.5 no_match rate
        for i in 0..4 {
            events.push(ev(
                "apply",
                "no_match",
                "packet-aaaaaaaa",
                &format!("2026-04-20T0{}:30:00Z", i + 1),
                serde_json::json!({}),
            ));
        }
        for i in 0..4 {
            events.push(ev(
                "apply",
                "ok",
                "packet-aaaaaaaa",
                &format!("2026-04-20T0{}:45:00Z", i + 1),
                serde_json::json!({"mode": "first"}),
            ));
        }

        // B: 6 applies all ok, but audit fidelity 0.7
        for i in 0..6 {
            events.push(ev(
                "apply",
                "ok",
                "packet-bbbbbbbb",
                &format!("2026-04-20T0{}:00:00Z", i + 1),
                serde_json::json!({}),
            ));
        }
        events.push(ev(
            "audit",
            "low_fidelity",
            "packet-bbbbbbbb",
            in_window,
            serde_json::json!({"fidelity": 0.7, "total": 10, "correct": 7}),
        ));

        // C: only 2 applies, both no_match — below min_apply_samples (5),
        // should NOT flag despite 100% no_match rate
        for i in 0..2 {
            events.push(ev(
                "apply",
                "no_match",
                "packet-cccccccc",
                &format!("2026-04-20T0{}:00:00Z", i + 1),
                serde_json::json!({}),
            ));
        }

        // D: same shape as A, but a repair_candidate event 2h ago
        // should suppress re-flag (cooldown default 24h).
        for i in 0..4 {
            events.push(ev(
                "apply",
                "no_match",
                "packet-dddddddd",
                &format!("2026-04-20T0{}:00:00Z", i + 1),
                serde_json::json!({}),
            ));
        }
        for i in 0..4 {
            events.push(ev(
                "apply",
                "ok",
                "packet-dddddddd",
                &format!("2026-04-20T0{}:30:00Z", i + 1),
                serde_json::json!({}),
            ));
        }
        events.push(ev(
            "repair_candidate",
            "flagged",
            "packet-dddddddd",
            "2026-04-20T10:00:00Z", // 2h ago, inside cooldown
            serde_json::json!({"reasons": ["high_no_match_rate"]}),
        ));

        // An out-of-window apply that should be ignored entirely.
        events.push(ev(
            "apply",
            "no_match",
            "packet-aaaaaaaa",
            out_of_window,
            serde_json::json!({}),
        ));

        // Sort newest-first (mirrors list_events shape).
        events.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));

        let config = ScannerConfig::default();
        let candidates = find_repair_candidates(&events, &config, now);

        // A and B fire; C suppressed by min_apply_samples; D suppressed by cooldown.
        assert_eq!(
            candidates.len(),
            2,
            "expected A and B, got: {:?}",
            candidates.iter().map(|c| &c.packet_id).collect::<Vec<_>>()
        );

        let a = candidates
            .iter()
            .find(|c| c.packet_id == "packet-aaaaaaaa")
            .expect("packet-aaaaaaaa flagged");
        assert!(a.reasons.contains(&"high_no_match_rate".to_string()));
        assert_eq!(a.apply_count, 8);
        assert_eq!(a.no_match_count, 4);
        assert_eq!(a.no_match_rate, Some(0.5));

        let b = candidates
            .iter()
            .find(|c| c.packet_id == "packet-bbbbbbbb")
            .expect("packet-bbbbbbbb flagged");
        assert!(b.reasons.contains(&"low_fidelity".to_string()));
        assert_eq!(b.fidelity, Some(0.7));

        // Confirm scanner_step persists events to the log.
        let (_dir, store) = tmp_packets();
        for e in &events {
            store.append_event(e);
        }
        let written = store.scanner_step_at(&config, now).unwrap();
        assert_eq!(written.len(), 2);
        let log = store
            .list_events(Some("repair_candidate"), None, None, None, 50)
            .unwrap();
        // Two we just wrote PLUS the synthetic one from the input stream.
        assert_eq!(log.len(), 3);
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
                mode: None,
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
            classification_lattice: review_lattice(),
            prefix_inference: review_prefix_inference(),
            rules,
            source_ids: vec![],
            self_audit_fidelity: None,
            created_at: now.clone(),
            updated_at: now,
            superseded_by: None,
            merged_from: vec![],
        }
    }

    fn rule(id: &str, antecedent: Predicate, consequent: &str, class: &str) -> Rule {
        Rule {
            id: id.into(),
            antecedent,
            consequent: Value::String(consequent.into()),
            classification: class.into(),
            emit: Emit::Independent,
            confidence: 1.0,
            provenance: vec![],
        }
    }

    fn fallback_rule(id: &str, antecedent: Predicate, consequent: &str, class: &str) -> Rule {
        Rule {
            id: id.into(),
            antecedent,
            consequent: Value::String(consequent.into()),
            classification: class.into(),
            emit: Emit::Fallback,
            confidence: 1.0,
            provenance: vec![],
        }
    }

    #[test]
    fn applicability_gate_discriminates_from_zero() {
        // The decisive phase-2 bug: rule said `Lt(tool_docs, 3)` fires on
        // a "clean" entity where no docs were added AND no tools were
        // added. The tri-state predicates let rules gate on applicability.
        let p = bare_packet(vec![rule(
            "fail_undocumented",
            Predicate::All {
                args: vec![
                    Predicate::IsNonNull {
                        field: "mcp_tools_added".into(),
                    },
                    Predicate::Gt {
                        field: "mcp_tools_added".into(),
                        value: 0,
                    },
                    Predicate::FieldLt {
                        lhs_field: "tool_docs_stanzas_added".into(),
                        rhs_field: "mcp_tools_added".into(),
                    },
                ],
            },
            "FAIL",
            "fail",
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

    // The old IsPresent/IsAbsent pair has been removed; tri-state
    // applicability is tested by `tri_state_applicability_discriminates_null_vs_missing`.

    #[test]
    fn field_comparisons_work_across_all_ops() {
        let cases: &[(Predicate, &str, serde_json::Value, bool)] = &[
            (
                Predicate::FieldEq {
                    lhs_field: "a".into(),
                    rhs_field: "b".into(),
                },
                "eq-hit",
                json!({"a": 5, "b": 5}),
                true,
            ),
            (
                Predicate::FieldEq {
                    lhs_field: "a".into(),
                    rhs_field: "b".into(),
                },
                "eq-miss",
                json!({"a": 5, "b": 6}),
                false,
            ),
            (
                Predicate::FieldGt {
                    lhs_field: "a".into(),
                    rhs_field: "b".into(),
                },
                "gt-hit",
                json!({"a": 10, "b": 5}),
                true,
            ),
            (
                Predicate::FieldGt {
                    lhs_field: "a".into(),
                    rhs_field: "b".into(),
                },
                "gt-eq",
                json!({"a": 5, "b": 5}),
                false,
            ),
            (
                Predicate::FieldGe {
                    lhs_field: "a".into(),
                    rhs_field: "b".into(),
                },
                "ge-eq",
                json!({"a": 5, "b": 5}),
                true,
            ),
            (
                Predicate::FieldLt {
                    lhs_field: "a".into(),
                    rhs_field: "b".into(),
                },
                "lt-hit",
                json!({"a": 1, "b": 5}),
                true,
            ),
            (
                Predicate::FieldLe {
                    lhs_field: "a".into(),
                    rhs_field: "b".into(),
                },
                "le-eq",
                json!({"a": 5, "b": 5}),
                true,
            ),
            // Missing field → false (no panic)
            (
                Predicate::FieldGt {
                    lhs_field: "a".into(),
                    rhs_field: "b".into(),
                },
                "missing-a",
                json!({"b": 5}),
                false,
            ),
        ];

        for (pred, label, entity, expect_hit) in cases {
            let p = bare_packet(vec![rule(label, pred.clone(), "HIT", "info")]);
            let fired = apply(&p, entity).is_some();
            assert_eq!(
                fired, *expect_hit,
                "case `{label}` failed; pred={pred:?}, entity={entity}"
            );
        }
    }

    #[test]
    fn float_comparisons_work() {
        let p = bare_packet(vec![
            rule(
                "fail_low_coverage",
                Predicate::LtF {
                    field: "coverage_pct".into(),
                    value: 80.0,
                },
                "FAIL: coverage below 80%",
                "fail",
            ),
            rule(
                "pass_high_coverage",
                Predicate::GeF {
                    field: "coverage_pct".into(),
                    value: 95.0,
                },
                "PASS: coverage above 95%",
                "pass",
            ),
        ]);

        let low = apply(&p, &json!({"coverage_pct": 73.5})).unwrap();
        assert_eq!(low.rule_id, "fail_low_coverage");

        let mid = apply(&p, &json!({"coverage_pct": 85.0}));
        assert!(mid.is_none(), "mid coverage should match neither rule");

        let high = apply(&p, &json!({"coverage_pct": 96.0})).unwrap();
        assert_eq!(high.rule_id, "pass_high_coverage");
    }

    #[test]
    fn classification_infers_from_id_prefix() {
        let map = review_prefix_inference();
        assert_eq!(
            infer_classification("fail_warnings", &map).as_deref(),
            Some("fail")
        );
        assert_eq!(
            infer_classification("flag_readonly_fs", &map).as_deref(),
            Some("flag")
        );
        assert_eq!(
            infer_classification("manual_review_security", &map).as_deref(),
            Some("manual")
        );
        assert_eq!(
            infer_classification("review_contract", &map).as_deref(),
            Some("manual")
        );
        assert_eq!(
            infer_classification("pass_all_clean", &map).as_deref(),
            Some("pass")
        );
        assert_eq!(infer_classification("miscellaneous", &map).as_deref(), None);
    }

    #[test]
    fn compile_infers_classification_from_id_prefix() {
        let (_dir, store) = tmp_packets();
        let params = CompileParams {
            domain: "classification-infer".into(),
            rules: json!([
                {"id": "fail_a", "antecedent": {"op": "True"}, "consequent": "FAIL"},
                {"id": "flag_b", "antecedent": {"op": "True"}, "consequent": "FLAG"},
                {"id": "manual_c", "antecedent": {"op": "True"}, "consequent": "MANUAL"},
                {"id": "pass_d", "antecedent": {"op": "True"}, "consequent": "PASS"},
                // Explicit classification survives — even though id prefix would say Fail
                {"id": "fail_e", "classification": "info", "antecedent": {"op": "True"}, "consequent": "EXPLICIT"},
            ]),
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
        store.compile(&params).unwrap();

        let all = store.list_all().unwrap();
        let packet = &all[0];
        let classes: Vec<&str> = packet
            .rules
            .iter()
            .map(|r| r.classification.as_str())
            .collect();
        assert_eq!(
            classes,
            vec![
                "fail",   // inferred from fail_
                "flag",   // inferred from flag_
                "manual", // inferred from manual_
                "pass",   // inferred from pass_
                "info",   // explicit "info" preserved even though fail_ prefix would infer "fail"
            ]
        );
    }

    #[test]
    fn compile_rejects_classification_not_in_lattice() {
        let (_dir, store) = tmp_packets();
        let params = CompileParams {
            domain: "bad-class".into(),
            rules: json!([
                {"id": "r1", "classification": "blocker", "antecedent": {"op": "True"}, "consequent": "X"},
            ]),
            classification_lattice: Some(vec!["fail".into(), "pass".into()]),
            prefix_inference: None,
            rank_table: None,
            threshold_table: None,
            rank_lookup_key: None,
            threshold_lookup_key: None,
            source_ids: None,
            scope: Some("global".into()),
            project: None,
        };
        let err = store.compile(&params).unwrap_err().to_string();
        assert!(err.contains("not in packet lattice"), "got: {err}");
    }

    #[test]
    fn prefix_inference_uses_longest_match() {
        // Overlapping prefixes — longer one wins (not BTreeMap iteration order).
        let mut map = BTreeMap::new();
        map.insert("fail_".into(), "fail".into());
        map.insert("fail_critical_".into(), "blocker".into());
        map.insert("flag_".into(), "flag".into());

        assert_eq!(
            infer_classification("fail_critical_foo", &map).as_deref(),
            Some("blocker"),
            "longer prefix `fail_critical_` beats shorter `fail_`"
        );
        assert_eq!(
            infer_classification("fail_normal", &map).as_deref(),
            Some("fail"),
            "only `fail_` matches — picks that"
        );
        assert_eq!(
            infer_classification("flag_readonly", &map).as_deref(),
            Some("flag"),
            "different prefix — picks the matching one"
        );
        assert_eq!(
            infer_classification("unknown_rule", &map).as_deref(),
            None,
            "no prefix match returns None"
        );
    }

    // ── Phase 4A: quantified collection predicates ──

    #[test]
    fn forall_vacuous_truth_on_empty_and_missing() {
        let pred = Predicate::ForAll {
            path: "items[*]".into(),
            pred: Box::new(Predicate::AlwaysFalse {}),
        };
        let p = bare_packet(vec![rule("flag_x", pred, "HIT", "flag")]);
        // Missing collection → vacuous true → rule fires even though inner is False.
        assert!(
            apply(&p, &json!({})).is_some(),
            "missing collection: ForAll is true vacuously"
        );
        // Empty collection → also vacuous true.
        assert!(
            apply(&p, &json!({"items": []})).is_some(),
            "empty collection: vacuous true"
        );
    }

    #[test]
    fn forall_fires_when_all_elements_satisfy() {
        // Rule: every tool must have a non-null description.
        let pred = Predicate::ForAll {
            path: "tools[*]".into(),
            pred: Box::new(Predicate::IsNonNull {
                field: "description".into(),
            }),
        };
        let p = bare_packet(vec![rule("ok_all_documented", pred, "ALL_OK", "flag")]);

        let good = json!({"tools": [
            {"name": "a", "description": "does A"},
            {"name": "b", "description": "does B"},
        ]});
        assert!(apply(&p, &good).is_some(), "all documented → rule fires");

        let bad = json!({"tools": [
            {"name": "a", "description": "does A"},
            {"name": "b"},  // missing description
        ]});
        assert!(
            apply(&p, &bad).is_none(),
            "one undocumented → rule does not fire"
        );
    }

    #[test]
    fn exists_false_on_empty_true_on_witness() {
        let pred = Predicate::Exists {
            path: "tools[*]".into(),
            pred: Box::new(Predicate::IsNonNull {
                field: "critical".into(),
            }),
        };
        let p = bare_packet(vec![rule(
            "flag_has_critical",
            pred,
            "HAS_CRITICAL",
            "flag",
        )]);

        // Empty → Exists is false → rule doesn't fire.
        assert!(apply(&p, &json!({"tools": []})).is_none());
        // No witness → false.
        assert!(apply(&p, &json!({"tools": [{"name": "a"}]})).is_none());
        // Witness present → true.
        assert!(apply(&p, &json!({"tools": [{"name": "a", "critical": true}]})).is_some());
    }

    #[test]
    fn forall_primitive_elements_wrapped_as_dollar() {
        // Scalars in the array get wrapped as {"$": value}. Predicate
        // references "$" to read them.
        let pred = Predicate::ForAll {
            path: "tags[*]".into(),
            pred: Box::new(Predicate::IsNonNull { field: "$".into() }),
        };
        let p = bare_packet(vec![rule("flag_tags_present", pred, "OK", "flag")]);

        assert!(
            apply(&p, &json!({"tags": ["a", "b", "c"]})).is_some(),
            "all non-null strings"
        );
        // Primitive null in array → IsNonNull("$") is false → ForAll fails.
        let with_null = json!({"tags": ["a", null, "c"]});
        assert!(
            apply(&p, &with_null).is_none(),
            "any null element breaks ForAll"
        );
    }

    #[test]
    fn forall_vacuous_true_when_runtime_data_not_an_array() {
        // Authoring error (dotted path, bad shape) is rejected at compile.
        // Runtime shape mismatch (entity has non-array where packet expected
        // array) is NOT an authoring error — the packet was correctly shaped,
        // the data just isn't what was expected. ForAll treats this as
        // "no elements to quantify over" → vacuous true, matching math
        // convention. Callers who want loud runtime failure should guard
        // with `IsNonNull{field}` and a separate rule.
        let pred = Predicate::ForAll {
            path: "count[*]".into(),
            pred: Box::new(Predicate::AlwaysTrue {}),
        };
        let p = bare_packet(vec![rule("flag_x", pred, "X", "flag")]);
        assert!(
            apply(&p, &json!({"count": 42})).is_some(),
            "non-array at runtime → vacuous true (not an authoring error)"
        );
    }

    #[test]
    fn count_cmp_all_ops() {
        fn probe(op: CmpOp, value: usize, arr_len: usize) -> bool {
            let pred = Predicate::CountCmp {
                path: "items[*]".into(),
                compare: op,
                value,
            };
            let p = bare_packet(vec![rule("flag_x", pred, "X", "flag")]);
            let arr: Vec<serde_json::Value> = (0..arr_len).map(|i| json!(i)).collect();
            apply(&p, &json!({"items": arr})).is_some()
        }

        // Eq
        assert!(probe(CmpOp::Eq, 3, 3));
        assert!(!probe(CmpOp::Eq, 3, 2));
        // Lt
        assert!(probe(CmpOp::Lt, 5, 3));
        assert!(!probe(CmpOp::Lt, 3, 3));
        // Le
        assert!(probe(CmpOp::Le, 3, 3));
        assert!(probe(CmpOp::Le, 5, 3));
        // Gt
        assert!(probe(CmpOp::Gt, 2, 3));
        assert!(!probe(CmpOp::Gt, 3, 3));
        // Ge
        assert!(probe(CmpOp::Ge, 3, 3));
        assert!(probe(CmpOp::Ge, 2, 3));

        // Missing path → length 0
        let pred = Predicate::CountCmp {
            path: "missing[*]".into(),
            compare: CmpOp::Eq,
            value: 0,
        };
        let p = bare_packet(vec![rule("flag_zero", pred, "X", "flag")]);
        assert!(apply(&p, &json!({})).is_some(), "missing path → count 0");
    }

    #[test]
    fn quantified_predicate_serde_round_trips() {
        // Canonical JSON shape for ForAll.
        let forall_json = json!({
            "op": "ForAll",
            "path": "tools[*]",
            "pred": {"op": "IsNonNull", "field": "description"}
        });
        let p: Predicate = serde_json::from_value(forall_json.clone()).unwrap();
        let back = serde_json::to_value(&p).unwrap();
        assert_eq!(back, forall_json);

        let count_json = json!({
            "op": "CountCmp",
            "path": "tools[*]",
            "compare": "ge",
            "value": 1
        });
        let p: Predicate = serde_json::from_value(count_json.clone()).unwrap();
        let back = serde_json::to_value(&p).unwrap();
        assert_eq!(back, count_json);
    }

    #[test]
    fn compile_accepts_dotted_paths() {
        // Workflow-engine integration accepts dotted paths in
        // quantified-predicate path expressions because ArcContext
        // entities are deeply structured (`vars.labels[*]`,
        // `outputs.Plan.findings[*]`). Empty segments still rejected.
        let (_dir, store) = tmp_packets();
        let ok_params = CompileParams {
            domain: "dotted-path-accepted".into(),
            rules: json!([{
                "id": "flag_x",
                "antecedent": {"op": "ForAll", "path": "config.rules[*]", "pred": {"op": "True"}},
                "consequent": "X"
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
        store
            .compile(&ok_params)
            .expect("dotted-path antecedent must compile");

        let bad_params = CompileParams {
            domain: "dotted-path-empty-seg".into(),
            rules: json!([{
                "id": "flag_x",
                "antecedent": {"op": "ForAll", "path": "config..rules[*]", "pred": {"op": "True"}},
                "consequent": "X"
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
        let err = format!("{:#}", store.compile(&bad_params).unwrap_err());
        assert!(
            err.contains("empty segment"),
            "empty-segment rejection missing: got {err}"
        );
    }

    #[test]
    fn compile_rejects_missing_bracket_suffix() {
        let (_dir, store) = tmp_packets();
        let params = CompileParams {
            domain: "no-bracket".into(),
            rules: json!([{
                "id": "flag_x",
                "antecedent": {"op": "ForAll", "path": "tools", "pred": {"op": "True"}},
                "consequent": "X"
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
        let err = format!("{:#}", store.compile(&params).unwrap_err());
        assert!(
            err.contains("[*]"),
            "missing [*] rejection unclear: got {err}"
        );
    }

    #[test]
    fn compile_rejects_nested_forall() {
        let (_dir, store) = tmp_packets();
        let params = CompileParams {
            domain: "nested".into(),
            rules: json!([{
                "id": "flag_x",
                "antecedent": {
                    "op": "ForAll",
                    "path": "groups[*]",
                    "pred": {
                        "op": "ForAll",
                        "path": "items[*]",
                        "pred": {"op": "True"}
                    }
                },
                "consequent": "X"
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
        let err = format!("{:#}", store.compile(&params).unwrap_err());
        assert!(
            err.contains("nested inside ForAll"),
            "nested-ForAll rejection unclear: got {err}"
        );
    }

    #[test]
    fn compile_allows_exists_inside_forall_inside_exists() {
        // The nested-ban is specifically ForAll-inside-ForAll.
        // Exists-inside-ForAll and ForAll-inside-Exists are fine.
        let (_dir, store) = tmp_packets();
        let params = CompileParams {
            domain: "mixed-quantifiers".into(),
            rules: json!([{
                "id": "flag_x",
                "antecedent": {
                    "op": "Exists",
                    "path": "groups[*]",
                    "pred": {
                        "op": "ForAll",
                        "path": "items[*]",
                        "pred": {"op": "IsNonNull", "field": "id"}
                    }
                },
                "consequent": "X"
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
        store
            .compile(&params)
            .expect("Exists over ForAll should compile");
    }

    #[test]
    fn verdict_is_highest_priority_not_firing_order() {
        // Lattice: fail > flag > pass. Findings fire in order [flag, pass, fail].
        // Verdict should be "fail" (highest priority), not "flag" (first fired).
        let p = bare_packet(vec![
            rule("flag_first", Predicate::AlwaysTrue {}, "FLAG", "flag"),
            rule("pass_second", Predicate::AlwaysTrue {}, "PASS", "pass"),
            rule("fail_third", Predicate::AlwaysTrue {}, "FAIL", "fail"),
        ]);
        let result = apply_all(&p, &json!({}));
        assert_eq!(result.findings.len(), 3);
        assert_eq!(
            result.verdict,
            Some("fail".to_string()),
            "verdict = highest-priority classification, not firing order"
        );
    }

    #[test]
    fn compile_auth_domain_lattice() {
        // Auth domain: deny wins; anomalies denoted deny_* or anom_*.
        let (_dir, store) = tmp_packets();
        let mut prefix = BTreeMap::new();
        prefix.insert("deny_".into(), "deny".into());
        prefix.insert("allow_".into(), "allow".into());
        prefix.insert("anom_".into(), "deny".into());

        let params = CompileParams {
            domain: "auth".into(),
            rules: json!([
                {"id": "anom_sensitive_resource", "antecedent": {"op": "Eq", "field": "sensitive", "value": true}, "consequent": "DENY"},
                {"id": "allow_admin", "antecedent": {"op": "Eq", "field": "role", "value": "admin"}, "consequent": "ALLOW"},
            ]),
            classification_lattice: Some(vec!["deny".into(), "allow".into()]),
            prefix_inference: Some(prefix),
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
        let classes: Vec<&str> = packet
            .rules
            .iter()
            .map(|r| r.classification.as_str())
            .collect();
        assert_eq!(
            classes,
            vec!["deny", "allow"],
            "auth prefixes inferred correctly"
        );

        // Apply in mode=all with a sensitive resource — deny wins.
        let result = apply_all(packet, &json!({"sensitive": true, "role": "admin"}));
        assert_eq!(
            result.verdict,
            Some("deny".to_string()),
            "DENY precedes ALLOW in auth lattice"
        );
    }

    #[test]
    fn apply_all_returns_every_matching_rule() {
        // The critical phase-2 semantic: apply_all evaluates every rule,
        // returns all findings, computes aggregate verdict. This is what
        // the bros called for in thread-0b20e854.
        let p = bare_packet(vec![
            rule("fail_a", Predicate::AlwaysTrue {}, "FAIL: always", "fail"),
            rule("flag_b", Predicate::AlwaysTrue {}, "FLAG: always", "flag"),
            rule(
                "flag_c",
                Predicate::Eq {
                    field: "x".into(),
                    value: Value::Int(1),
                },
                "FLAG: on x=1",
                "flag",
            ),
            rule("pass_d", Predicate::AlwaysFalse {}, "PASS: never", "pass"),
        ]);

        let result = apply_all(&p, &json!({"x": 1}));
        let fired: Vec<&str> = result.findings.iter().map(|f| f.rule_id.as_str()).collect();
        assert_eq!(
            fired,
            vec!["fail_a", "flag_b", "flag_c"],
            "every matching rule should appear"
        );
        assert_eq!(
            result.verdict,
            Some("fail".to_string()),
            "verdict = highest severity that fired"
        );

        // Entity where only the false rule fires → no findings, no verdict
        let empty = apply_all(&p, &json!({"x": 99}));
        let fired2: Vec<&str> = empty.findings.iter().map(|f| f.rule_id.as_str()).collect();
        assert_eq!(fired2, vec!["fail_a", "flag_b"]); // unconditional rules still fire
    }

    #[test]
    fn apply_all_verdict_follows_severity_precedence() {
        // Fail > Flag > Manual > Pass > Info
        let fail_p = bare_packet(vec![
            rule("pass_x", Predicate::AlwaysTrue {}, "PASS", "pass"),
            rule("manual_y", Predicate::AlwaysTrue {}, "MANUAL", "manual"),
            rule("flag_z", Predicate::AlwaysTrue {}, "FLAG", "flag"),
        ]);
        assert_eq!(
            apply_all(&fail_p, &json!({})).verdict,
            Some("flag".to_string())
        );

        let with_fail = bare_packet(vec![
            rule("pass_x", Predicate::AlwaysTrue {}, "PASS", "pass"),
            rule("fail_y", Predicate::AlwaysTrue {}, "FAIL", "fail"),
            rule("info_z", Predicate::AlwaysTrue {}, "INFO", "info"),
        ]);
        assert_eq!(
            apply_all(&with_fail, &json!({})).verdict,
            Some("fail".to_string())
        );

        // Nothing fires
        let nothing = bare_packet(vec![rule(
            "fail_never",
            Predicate::AlwaysFalse {},
            "NOPE",
            "fail",
        )]);
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
        assert!(
            res.is_err(),
            "invalid mode string should fail deserialization"
        );
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
        let missing = json!({}); // no key
        let nulled = json!({"x": serde_json::Value::Null}); // key present, value null
        let real = json!({"x": 42}); // key present, real value

        let key_exists = bare_packet(vec![rule(
            "flag_ke",
            Predicate::KeyExists { field: "x".into() },
            "KE",
            "flag",
        )]);
        let is_null = bare_packet(vec![rule(
            "flag_null",
            Predicate::IsNull { field: "x".into() },
            "NULL",
            "flag",
        )]);
        let is_non_null = bare_packet(vec![rule(
            "flag_nn",
            Predicate::IsNonNull { field: "x".into() },
            "NN",
            "flag",
        )]);
        let is_missing = bare_packet(vec![rule(
            "flag_miss",
            Predicate::IsMissing { field: "x".into() },
            "M",
            "flag",
        )]);

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
    fn classification_info_explicitly_preserved_over_prefix_inference() {
        // The phase-2 bug Codex caught: compile loop upgraded every Info
        // from the id prefix, so explicit `classification: "info"` was erased.
        // Post-phase-3: the field is `classification`, and explicit values
        // still beat id-prefix inference.
        let (_dir, store) = tmp_packets();
        let params = CompileParams {
            domain: "classification-preserve".into(),
            rules: json!([
                // Prefix says FAIL, but caller EXPLICITLY says Info — must preserve.
                {"id": "fail_x", "classification": "info", "antecedent": {"op": "True"}, "consequent": "X"},
                // No classification declared — infer from prefix.
                {"id": "fail_y", "antecedent": {"op": "True"}, "consequent": "Y"},
            ]),
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
        store.compile(&params).unwrap();
        let packet = &store.list_all().unwrap()[0];
        assert_eq!(
            packet.rules[0].classification, "info",
            "explicit info must survive prefix inference"
        );
        assert_eq!(
            packet.rules[1].classification, "fail",
            "no classification declared → infer from prefix"
        );
    }

    #[test]
    fn fallback_rules_suppressed_when_independent_fires() {
        // Phase-2.5d: Fallback rules fire ONLY when no Independent rule fired.
        // This is how pass_all_clean ought to behave: disappear when real
        // findings exist, present when nothing else has anything to say.
        let p = bare_packet(vec![
            rule(
                "flag_x",
                Predicate::Eq {
                    field: "trigger".into(),
                    value: Value::Bool(true),
                },
                "FLAG",
                "flag",
            ),
            fallback_rule("pass_catchall", Predicate::AlwaysTrue {}, "PASS", "pass"),
        ]);

        // Trigger fires — fallback is suppressed
        let result = apply_all(&p, &json!({"trigger": true}));
        let fired: Vec<&str> = result.findings.iter().map(|f| f.rule_id.as_str()).collect();
        assert_eq!(
            fired,
            vec!["flag_x"],
            "fallback must be suppressed when Independent fires"
        );
        assert_eq!(result.verdict, Some("flag".to_string()));

        // No trigger — fallback fires
        let result = apply_all(&p, &json!({"trigger": false}));
        let fired: Vec<&str> = result.findings.iter().map(|f| f.rule_id.as_str()).collect();
        assert_eq!(
            fired,
            vec!["pass_catchall"],
            "fallback fires when no Independent matched"
        );
        assert_eq!(result.verdict, Some("pass".to_string()));
    }

    #[test]
    fn fallback_ignored_in_first_mode() {
        // In apply (mode="first"), emit is irrelevant — first-match-wins
        // applies regardless. Fallback rules can still fire.
        let p = bare_packet(vec![
            rule(
                "flag_x",
                Predicate::Eq {
                    field: "a".into(),
                    value: Value::Int(1),
                },
                "FLAG_X",
                "flag",
            ),
            fallback_rule("pass_catchall", Predicate::AlwaysTrue {}, "PASS", "pass"),
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
        assert_eq!(
            serde_json::to_value(&ApplyMode::First).unwrap(),
            json!("first")
        );
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
        let r = ri
            .materialize(&review_lattice(), &review_prefix_inference())
            .unwrap();
        assert_eq!(r.emit, Emit::Independent);
    }

    // ── Phase 4B: multi-finding audit ──

    fn multi_find_packet() -> Packet {
        bare_packet(vec![
            rule("fail_always", Predicate::AlwaysTrue {}, "FAIL", "fail"),
            rule(
                "flag_on_x",
                Predicate::Eq {
                    field: "x".into(),
                    value: Value::Int(1),
                },
                "FLAG_X",
                "flag",
            ),
            fallback_rule("pass_catchall", Predicate::AlwaysTrue {}, "PASS", "pass"),
        ])
    }

    #[test]
    fn verify_all_matches_verdict_and_rule_ids() {
        let p = multi_find_packet();
        // Entity with x=1: both fail_always and flag_on_x fire; verdict = fail.
        let dataset = json!([{
            "entity": {"x": 1},
            "expected_verdict": "fail",
            "expected_rule_ids": ["fail_always", "flag_on_x"]
        }]);
        let report = verify_all(&p, &dataset).unwrap();
        assert_eq!(report.total, 1);
        assert_eq!(report.correct, 1);
        assert!(report.mismatches.is_empty());
    }

    #[test]
    fn verify_all_flags_verdict_mismatch() {
        let p = multi_find_packet();
        let dataset = json!([{
            "entity": {"x": 1},
            "expected_verdict": "flag",  // wrong — actual is "fail"
            "expected_rule_ids": ["fail_always", "flag_on_x"]
        }]);
        let report = verify_all(&p, &dataset).unwrap();
        assert_eq!(report.total, 1);
        assert_eq!(report.correct, 0);
        assert_eq!(report.mismatches.len(), 1);
        assert_eq!(report.mismatches[0].check, "verdict");
        assert_eq!(
            report.mismatches[0].expected_verdict.as_deref(),
            Some("flag")
        );
        assert_eq!(report.mismatches[0].actual_verdict.as_deref(), Some("fail"));
    }

    #[test]
    fn verify_all_flags_rule_ids_mismatch() {
        let p = multi_find_packet();
        let dataset = json!([{
            "entity": {"x": 1},
            "expected_verdict": "fail",
            "expected_rule_ids": ["fail_always"]  // missing flag_on_x
        }]);
        let report = verify_all(&p, &dataset).unwrap();
        assert_eq!(report.correct, 0);
        assert_eq!(report.mismatches[0].check, "rule_ids");
    }

    #[test]
    fn verify_all_flags_both_mismatches() {
        let p = multi_find_packet();
        let dataset = json!([{
            "entity": {"x": 1},
            "expected_verdict": "pass",
            "expected_rule_ids": ["nonexistent_rule"]
        }]);
        let report = verify_all(&p, &dataset).unwrap();
        assert_eq!(report.mismatches[0].check, "both");
    }

    #[test]
    fn verify_all_rule_ids_order_invariant() {
        let p = multi_find_packet();
        // Order of expected_rule_ids differs from firing order — still matches.
        let dataset = json!([{
            "entity": {"x": 1},
            "expected_rule_ids": ["flag_on_x", "fail_always"]  // reversed
        }]);
        let report = verify_all(&p, &dataset).unwrap();
        assert_eq!(
            report.correct, 1,
            "rule_ids comparison is a set, not a list"
        );
    }

    #[test]
    fn verify_all_partial_expectations_ok() {
        let p = multi_find_packet();
        // Only expected_verdict set → only verdict checked.
        let dataset = json!([
            {"entity": {"x": 1}, "expected_verdict": "fail"},
            {"entity": {"x": 99}, "expected_verdict": "fail"}  // fail_always still fires
        ]);
        let report = verify_all(&p, &dataset).unwrap();
        assert_eq!(report.correct, 2);
    }

    #[test]
    fn audit_tool_all_mode_via_mcp_surface() {
        let (_dir, store) = tmp_packets();
        store.save_packet(&multi_find_packet()).unwrap();
        let packet_id = multi_find_packet().id;

        let report = store
            .audit_tool(&AuditParams {
                packet_id: packet_id.clone(),
                dataset: json!([{
                    "entity": {"x": 1},
                    "expected_verdict": "fail",
                    "expected_rule_ids": ["fail_always", "flag_on_x"]
                }]),
                mode: Some(ApplyMode::All),
            })
            .unwrap();
        assert!(report.contains("\"mode\": \"all\""));
        assert!(report.contains("\"correct\": 1"));
        assert!(report.contains("\"fidelity\": 1.0"));
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
        let r = ri
            .materialize(&review_lattice(), &review_prefix_inference())
            .unwrap();
        assert_eq!(r.emit, Emit::Fallback);
    }

    // ── E12-refinement: permissive JSON on tool params ─────────────

    #[test]
    fn unwrap_jsonish_parses_stringified_array() {
        let mut v = serde_json::Value::String(r#"[{"a": 1}, {"a": 2}]"#.into());
        unwrap_jsonish(&mut v);
        assert!(v.is_array());
        assert_eq!(v.as_array().unwrap().len(), 2);
    }

    #[test]
    fn unwrap_jsonish_parses_stringified_object() {
        let mut v = serde_json::Value::String(r#"{"role": "admin"}"#.into());
        unwrap_jsonish(&mut v);
        assert!(v.is_object());
        assert_eq!(v.get("role").unwrap().as_str().unwrap(), "admin");
    }

    #[test]
    fn unwrap_jsonish_noop_on_structured_value() {
        let mut v = serde_json::json!({"already": "structured"});
        let before = v.clone();
        unwrap_jsonish(&mut v);
        assert_eq!(v, before);
    }

    #[test]
    fn unwrap_jsonish_noop_on_plain_string() {
        // A genuinely string param (not a JSON literal) should not be
        // coerced. The `{`/`[` prefix check prevents false positives.
        let mut v = serde_json::Value::String("hello world".into());
        let before = v.clone();
        unwrap_jsonish(&mut v);
        assert_eq!(v, before);
    }

    #[test]
    fn unwrap_jsonish_leaves_invalid_json_string_untouched() {
        // String that starts with `{` but isn't valid JSON — leave it.
        let mut v = serde_json::Value::String("{ not json }".into());
        let before = v.clone();
        unwrap_jsonish(&mut v);
        assert_eq!(v, before);
    }

    #[test]
    fn compile_accepts_stringified_rules_array() {
        // Simulates the Codex first-attempt shape: rules passed as a
        // JSON-encoded string instead of a structured array. Compile
        // should succeed without a retry.
        let (_d, packets) = tmp_packets();
        let rules_as_string = serde_json::Value::String(
            r#"[{"id":"r1","antecedent":{"op":"True"},"consequent":"X","classification":"pass","emit":"fallback"}]"#
                .into(),
        );
        let out = packets
            .compile(&CompileParams {
                domain: "coerce-test".into(),
                scope: Some("global".into()),
                project: None,
                classification_lattice: None,
                prefix_inference: None,
                rank_table: None,
                threshold_table: None,
                rank_lookup_key: None,
                threshold_lookup_key: None,
                source_ids: None,
                rules: rules_as_string,
            })
            .unwrap();
        assert!(out.contains("compiled"));
    }

    #[test]
    fn apply_tool_accepts_stringified_entity() {
        let (_d, packets) = tmp_packets();
        let id = compile_breaking_packet(&packets);
        // Entity passed as string
        let report = packets
            .apply_tool(&ApplyParams {
                packet_id: id,
                entity: serde_json::Value::String(
                    r#"{"api_surface_changed": true, "migration_note_present": false}"#.into(),
                ),
                mode: Some(ApplyMode::First),
            })
            .unwrap();
        assert!(report.contains("\"match\": true"));
        assert!(report.contains("breaking_api_no_migration"));
    }

    // ── Phase 6: StringContains / InRange tests ────────────────────

    fn noop_entity() -> serde_json::Map<String, serde_json::Value> {
        serde_json::Map::new()
    }

    #[test]
    fn string_contains_matches_case_sensitive() {
        let p = Predicate::StringContains {
            field: "message".into(),
            needle: "OOM".into(),
            case_insensitive: false,
        };
        let yes = serde_json::json!({"message": "worker OOMKilled"})
            .as_object()
            .unwrap()
            .clone();
        let mixed = serde_json::json!({"message": "worker oom event"})
            .as_object()
            .unwrap()
            .clone();
        let no_field = noop_entity();
        let non_string = serde_json::json!({"message": 42})
            .as_object()
            .unwrap()
            .clone();
        assert!(eval_predicate(&p, &yes, &NoopResolver, 0));
        assert!(!eval_predicate(&p, &mixed, &NoopResolver, 0));
        assert!(!eval_predicate(&p, &no_field, &NoopResolver, 0));
        assert!(!eval_predicate(&p, &non_string, &NoopResolver, 0));
    }

    #[test]
    fn string_contains_matches_case_insensitive() {
        let p = Predicate::StringContains {
            field: "message".into(),
            needle: "out of memory".into(),
            case_insensitive: true,
        };
        let yes1 = serde_json::json!({"message": "Out Of Memory allocating"})
            .as_object()
            .unwrap()
            .clone();
        let yes2 = serde_json::json!({"message": "OUT OF MEMORY"})
            .as_object()
            .unwrap()
            .clone();
        let no = serde_json::json!({"message": "disk full"})
            .as_object()
            .unwrap()
            .clone();
        assert!(eval_predicate(&p, &yes1, &NoopResolver, 0));
        assert!(eval_predicate(&p, &yes2, &NoopResolver, 0));
        assert!(!eval_predicate(&p, &no, &NoopResolver, 0));
    }

    #[test]
    fn string_contains_composes_via_any_for_multi_needle() {
        // The regex-alternation idiom: Any[Contains{a}, Contains{b}].
        let p = Predicate::Any {
            args: vec![
                Predicate::StringContains {
                    field: "message".into(),
                    needle: "OOM".into(),
                    case_insensitive: true,
                },
                Predicate::StringContains {
                    field: "message".into(),
                    needle: "out of memory".into(),
                    case_insensitive: true,
                },
            ],
        };
        let oom = serde_json::json!({"message": "ooMkilled"})
            .as_object()
            .unwrap()
            .clone();
        let prose = serde_json::json!({"message": "Process Ran Out Of Memory"})
            .as_object()
            .unwrap()
            .clone();
        let neither = serde_json::json!({"message": "disk full"})
            .as_object()
            .unwrap()
            .clone();
        assert!(eval_predicate(&p, &oom, &NoopResolver, 0));
        assert!(eval_predicate(&p, &prose, &NoopResolver, 0));
        assert!(!eval_predicate(&p, &neither, &NoopResolver, 0));
    }

    #[test]
    fn in_range_inclusive_both_ends() {
        let p = Predicate::InRange {
            field: "perf_delta_ms".into(),
            min: 1,
            max: 5,
        };
        for (v, want) in [(0, false), (1, true), (3, true), (5, true), (6, false)] {
            let e = serde_json::json!({"perf_delta_ms": v})
                .as_object()
                .unwrap()
                .clone();
            assert_eq!(
                eval_predicate(&p, &e, &NoopResolver, 0),
                want,
                "v={v} expected {want}"
            );
        }
    }

    #[test]
    fn in_range_missing_or_non_int_is_false() {
        let p = Predicate::InRange {
            field: "x".into(),
            min: 0,
            max: 10,
        };
        let missing = noop_entity();
        let str_field = serde_json::json!({"x": "five"})
            .as_object()
            .unwrap()
            .clone();
        assert!(!eval_predicate(&p, &missing, &NoopResolver, 0));
        assert!(!eval_predicate(&p, &str_field, &NoopResolver, 0));
    }

    #[test]
    fn in_range_f_inclusive_and_rejects_non_numeric() {
        let p = Predicate::InRangeF {
            field: "coverage".into(),
            min: 0.8,
            max: 0.95,
        };
        for (v, want) in [
            (0.79, false),
            (0.80, true),
            (0.90, true),
            (0.95, true),
            (0.96, false),
        ] {
            let e = serde_json::json!({"coverage": v})
                .as_object()
                .unwrap()
                .clone();
            assert_eq!(
                eval_predicate(&p, &e, &NoopResolver, 0),
                want,
                "v={v} expected {want}"
            );
        }
        let str_field = serde_json::json!({"coverage": "high"})
            .as_object()
            .unwrap()
            .clone();
        assert!(!eval_predicate(&p, &str_field, &NoopResolver, 0));
    }

    #[test]
    fn compile_accepts_new_predicates_end_to_end() {
        let (_d, packets) = tmp_packets();
        let out = packets
            .compile(&CompileParams {
                domain: "log-triage".into(),
                scope: Some("global".into()),
                project: None,
                classification_lattice: Some(vec![
                    "critical".into(),
                    "observe".into(),
                    "ignore".into(),
                ]),
                prefix_inference: None,
                rank_table: None,
                threshold_table: None,
                rank_lookup_key: None,
                threshold_lookup_key: None,
                source_ids: None,
                rules: json!([
                    {
                        "id": "critical_oom",
                        "classification": "critical",
                        "antecedent": {
                            "op": "Any",
                            "args": [
                                {"op": "StringContains", "field": "message", "needle": "OOM", "case_insensitive": true},
                                {"op": "StringContains", "field": "message", "needle": "out of memory", "case_insensitive": true}
                            ]
                        },
                        "consequent": "CRIT"
                    },
                    {
                        "id": "observe_elevated_latency",
                        "classification": "observe",
                        "antecedent": {
                            "op": "InRangeF", "field": "p99_ms", "min": 500.0, "max": 2000.0
                        },
                        "consequent": "OBS"
                    },
                    {
                        "id": "ignore_default",
                        "classification": "ignore",
                        "emit": "fallback",
                        "antecedent": {"op": "True"},
                        "consequent": "IGN"
                    }
                ]),
            })
            .unwrap();
        let id = out.split_whitespace().nth(1).unwrap().to_string();
        let pkt = packets.load(&id).unwrap();

        // Apply + multi-finding via bbox_audit dataset.
        assert_eq!(
            apply(&pkt, &json!({"message": "worker OOMKilled at 0x1234"}))
                .unwrap()
                .rule_id,
            "critical_oom"
        );
        assert_eq!(
            apply(&pkt, &json!({"message": "disk full", "p99_ms": 800.0}))
                .unwrap()
                .rule_id,
            "observe_elevated_latency"
        );
        assert_eq!(
            apply(&pkt, &json!({"message": "ok", "p99_ms": 50.0}))
                .unwrap()
                .rule_id,
            "ignore_default"
        );
    }

    // ── Prefix-inference error clarity test (E14 gotcha) ───────────

    #[test]
    fn classification_mismatch_error_names_inferred_prefix() {
        // Reproduces the E14/Gemini gotcha: rule id has prefix `review_`
        // which auto-classifies as `manual` via default prefix_inference,
        // but the packet lattice doesn't include `manual`. Error should
        // explicitly name the inference path so the author knows where
        // `manual` came from.
        let (_d, packets) = tmp_packets();
        let err = packets
            .compile(&CompileParams {
                domain: "prefix-trap".into(),
                scope: Some("global".into()),
                project: None,
                classification_lattice: Some(vec![
                    "BLOCK".into(),
                    "REVIEW".into(),
                    "AUTO_APPROVE".into(),
                ]),
                prefix_inference: None, // uses default review_prefix_inference
                rank_table: None,
                threshold_table: None,
                rank_lookup_key: None,
                threshold_lookup_key: None,
                source_ids: None,
                rules: json!([{
                    "id": "review_one_red",
                    // No explicit classification → inferred from `review_` prefix → "manual"
                    "antecedent": {"op": "True"},
                    "consequent": "REVIEW"
                }]),
            })
            .unwrap_err();
        let msg = format!("{err:#}");
        // Old error: "rule 'review_one_red' classification 'manual' is not in packet lattice [...]"
        // New error: adds "INFERRED from id prefix 'review_' via the packet's prefix_inference map"
        assert!(
            msg.contains("INFERRED from id prefix 'review_'"),
            "expected error to name inferred prefix path, got: {msg}"
        );
        assert!(
            msg.contains("prefix_inference map"),
            "expected error to mention the inference map, got: {msg}"
        );
    }

    #[test]
    fn classification_mismatch_error_no_inference_hint_when_explicit() {
        // When classification is explicit, error should NOT add the
        // inference hint (it would be misleading).
        let (_d, packets) = tmp_packets();
        let err = packets
            .compile(&CompileParams {
                domain: "explicit-mismatch".into(),
                scope: Some("global".into()),
                project: None,
                classification_lattice: Some(vec!["red".into(), "green".into()]),
                prefix_inference: None,
                rank_table: None,
                threshold_table: None,
                rank_lookup_key: None,
                threshold_lookup_key: None,
                source_ids: None,
                rules: json!([{
                    "id": "bad_rule",
                    "classification": "purple", // explicit, not in lattice
                    "antecedent": {"op": "True"},
                    "consequent": "X"
                }]),
            })
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("'purple'"));
        assert!(
            !msg.contains("INFERRED"),
            "explicit classification should not trigger inference hint, got: {msg}"
        );
    }

    // ── CountMatches predicate tests ───────────────────────────────

    #[test]
    fn count_matches_counts_true_subpredicates() {
        // 3 predicates, 2 of which match → count=2
        let p = Predicate::CountMatches {
            args: vec![
                Predicate::Eq {
                    field: "a".into(),
                    value: Value::Bool(true),
                },
                Predicate::Eq {
                    field: "b".into(),
                    value: Value::Bool(true),
                },
                Predicate::Eq {
                    field: "c".into(),
                    value: Value::Bool(true),
                },
            ],
            compare: CmpOp::Ge,
            value: 2,
        };
        let two_true = serde_json::json!({"a": true, "b": true, "c": false})
            .as_object()
            .unwrap()
            .clone();
        let one_true = serde_json::json!({"a": true, "b": false, "c": false})
            .as_object()
            .unwrap()
            .clone();
        let all_true = serde_json::json!({"a": true, "b": true, "c": true})
            .as_object()
            .unwrap()
            .clone();
        assert!(eval_predicate(&p, &two_true, &NoopResolver, 0));
        assert!(!eval_predicate(&p, &one_true, &NoopResolver, 0));
        assert!(eval_predicate(&p, &all_true, &NoopResolver, 0));
    }

    #[test]
    fn count_matches_exactly_k_shape() {
        // "exactly 1 of N" uses CmpOp::Eq
        let p = Predicate::CountMatches {
            args: vec![
                Predicate::Eq {
                    field: "a".into(),
                    value: Value::Bool(true),
                },
                Predicate::Eq {
                    field: "b".into(),
                    value: Value::Bool(true),
                },
                Predicate::Eq {
                    field: "c".into(),
                    value: Value::Bool(true),
                },
            ],
            compare: CmpOp::Eq,
            value: 1,
        };
        for (a, b, c, want) in [
            (false, false, false, false), // 0 true
            (true, false, false, true),   // 1 true
            (true, true, false, false),   // 2 true
            (true, true, true, false),    // 3 true
        ] {
            let e = serde_json::json!({"a": a, "b": b, "c": c})
                .as_object()
                .unwrap()
                .clone();
            assert_eq!(
                eval_predicate(&p, &e, &NoopResolver, 0),
                want,
                "a={a},b={b},c={c}"
            );
        }
    }

    #[test]
    fn count_matches_empty_args_is_zero_count() {
        let p = Predicate::CountMatches {
            args: vec![],
            compare: CmpOp::Eq,
            value: 0,
        };
        let e = noop_entity();
        assert!(eval_predicate(&p, &e, &NoopResolver, 0));

        let p_ge_1 = Predicate::CountMatches {
            args: vec![],
            compare: CmpOp::Ge,
            value: 1,
        };
        assert!(!eval_predicate(&p_ge_1, &e, &NoopResolver, 0));
    }

    #[test]
    fn count_matches_composes_with_apply_for_subpacket_tally() {
        // End-to-end: compose three sub-packet Apply nodes under
        // CountMatches to express "≥ 2 of these 3 sub-packets say red".
        // This is the E14 use case — collapses pairwise enumeration to
        // a single rule.
        let (_d, packets) = tmp_packets();
        let red_lattice = vec!["red".into(), "green".into()];

        let make_red_sub = |packets: &Packets, domain: &str, field: &str| -> String {
            let out = packets
                .compile(&CompileParams {
                    domain: domain.into(),
                    scope: Some("global".into()),
                    project: None,
                    classification_lattice: Some(red_lattice.clone()),
                    prefix_inference: None,
                    rank_table: None,
                    threshold_table: None,
                    rank_lookup_key: None,
                    threshold_lookup_key: None,
                    source_ids: None,
                    rules: json!([
                        {
                            "id": "red_on_true",
                            "classification": "red",
                            "antecedent": {"op": "Eq", "field": field, "value": true},
                            "consequent": "RED"
                        },
                        {
                            "id": "green_default",
                            "classification": "green",
                            "emit": "fallback",
                            "antecedent": {"op": "True"},
                            "consequent": "GREEN"
                        }
                    ]),
                })
                .unwrap();
            out.split_whitespace().nth(1).unwrap().to_string()
        };
        let a = make_red_sub(&packets, "a-sub", "a_red");
        let b = make_red_sub(&packets, "b-sub", "b_red");
        let c = make_red_sub(&packets, "c-sub", "c_red");

        // Master uses CountMatches over three Apply nodes.
        let out = packets
            .compile(&CompileParams {
                domain: "tally".into(),
                scope: Some("global".into()),
                project: None,
                classification_lattice: Some(vec!["block".into(), "review".into(), "ok".into()]),
                prefix_inference: None,
                rank_table: None,
                threshold_table: None,
                rank_lookup_key: None,
                threshold_lookup_key: None,
                source_ids: None,
                rules: json!([
                    {
                        "id": "block_2plus_red",
                        "classification": "block",
                        "antecedent": {
                            "op": "CountMatches",
                            "args": [
                                {"op": "Apply", "packet_id": a, "expect": ["red"]},
                                {"op": "Apply", "packet_id": b, "expect": ["red"]},
                                {"op": "Apply", "packet_id": c, "expect": ["red"]}
                            ],
                            "compare": "ge",
                            "value": 2
                        },
                        "consequent": "BLOCK"
                    },
                    {
                        "id": "review_1_red",
                        "classification": "review",
                        "antecedent": {
                            "op": "CountMatches",
                            "args": [
                                {"op": "Apply", "packet_id": a, "expect": ["red"]},
                                {"op": "Apply", "packet_id": b, "expect": ["red"]},
                                {"op": "Apply", "packet_id": c, "expect": ["red"]}
                            ],
                            "compare": "eq",
                            "value": 1
                        },
                        "consequent": "REVIEW"
                    },
                    {
                        "id": "ok_default",
                        "classification": "ok",
                        "emit": "fallback",
                        "antecedent": {"op": "True"},
                        "consequent": "OK"
                    }
                ]),
            })
            .unwrap();
        let master_id = out.split_whitespace().nth(1).unwrap().to_string();
        let master = packets.load(&master_id).unwrap();

        // All green
        let all_green = json!({"a_red": false, "b_red": false, "c_red": false});
        assert_eq!(
            apply_with(&master, &all_green, &packets).unwrap().rule_id,
            "ok_default"
        );
        // One red
        let one_red = json!({"a_red": true, "b_red": false, "c_red": false});
        assert_eq!(
            apply_with(&master, &one_red, &packets).unwrap().rule_id,
            "review_1_red"
        );
        // Two red
        let two_red = json!({"a_red": true, "b_red": true, "c_red": false});
        assert_eq!(
            apply_with(&master, &two_red, &packets).unwrap().rule_id,
            "block_2plus_red"
        );
        // Three red
        let three_red = json!({"a_red": true, "b_red": true, "c_red": true});
        assert_eq!(
            apply_with(&master, &three_red, &packets).unwrap().rule_id,
            "block_2plus_red"
        );
    }

    #[test]
    fn count_matches_compile_validates_nested_apply_refs() {
        // CountMatches containing Apply with a missing packet_id should
        // fail compile (same invariant as top-level Apply).
        let (_d, packets) = tmp_packets();
        let err = packets
            .compile(&CompileParams {
                domain: "test".into(),
                scope: Some("global".into()),
                project: None,
                classification_lattice: None,
                prefix_inference: None,
                rank_table: None,
                threshold_table: None,
                rank_lookup_key: None,
                threshold_lookup_key: None,
                source_ids: None,
                rules: json!([{
                    "id": "fail_nested_ref",
                    "antecedent": {
                        "op": "CountMatches",
                        "args": [
                            {"op": "Apply", "packet_id": "packet-missing1", "expect": ["red"]}
                        ],
                        "compare": "ge",
                        "value": 1
                    },
                    "consequent": "X"
                }]),
            })
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("packet-missing1"),
            "expected missing-packet error via nested CountMatches, got: {msg}"
        );
    }

    // ── Composition (Apply predicate) tests ────────────────────────

    /// Compile a minimal "is_breaking" sub-packet: breaks if api_surface
    /// changed AND no migration note. Lattice: [breaking, safe].
    fn compile_breaking_packet(packets: &Packets) -> String {
        let out = packets
            .compile(&CompileParams {
                domain: "pr-breakingness".into(),
                scope: Some("global".into()),
                project: None,
                classification_lattice: Some(vec!["breaking".into(), "safe".into()]),
                prefix_inference: None,
                rank_table: None,
                threshold_table: None,
                rank_lookup_key: None,
                threshold_lookup_key: None,
                source_ids: None,
                rules: json!([
                    {
                        "id": "breaking_api_no_migration",
                        "classification": "breaking",
                        "antecedent": {"op": "All", "args": [
                            {"op": "Eq", "field": "api_surface_changed", "value": true},
                            {"op": "Eq", "field": "migration_note_present", "value": false}
                        ]},
                        "consequent": "BREAKING"
                    },
                    {
                        "id": "safe_default",
                        "classification": "safe",
                        "emit": "fallback",
                        "antecedent": {"op": "True"},
                        "consequent": "SAFE"
                    }
                ]),
            })
            .unwrap();
        // compile returns "Packet packet-xxxxxxxx compiled (...)"
        out.split_whitespace().nth(1).unwrap().to_string()
    }

    #[test]
    fn apply_node_composes_sub_packet_verdict() {
        let (_d, packets) = tmp_packets();
        let sub_id = compile_breaking_packet(&packets);

        // Outer packet: REJECT if sub says breaking; else PASS.
        let outer = packets
            .compile(&CompileParams {
                domain: "pr-triage".into(),
                scope: Some("global".into()),
                project: None,
                classification_lattice: None, // use review lattice
                prefix_inference: None,
                rank_table: None,
                threshold_table: None,
                rank_lookup_key: None,
                threshold_lookup_key: None,
                source_ids: None,
                rules: json!([
                    {
                        "id": "fail_breaking",
                        "antecedent": {
                            "op": "Apply",
                            "packet_id": sub_id.clone(),
                            "expect": ["breaking"],
                        },
                        "consequent": "REJECT"
                    },
                    {
                        "id": "pass_default",
                        "emit": "fallback",
                        "antecedent": {"op": "True"},
                        "consequent": "ACCEPT"
                    }
                ]),
            })
            .unwrap();
        let outer_id = outer.split_whitespace().nth(1).unwrap();
        let outer_pkt = packets.load(outer_id).unwrap();

        // Breaking entity → outer fires fail_breaking via Apply.
        let breaking = json!({
            "api_surface_changed": true,
            "migration_note_present": false,
        });
        let pred = apply_with(&outer_pkt, &breaking, &packets).unwrap();
        assert_eq!(pred.rule_id, "fail_breaking");
        assert_eq!(pred.consequent, Value::String("REJECT".into()));

        // Safe entity → outer falls through to pass_default.
        let safe = json!({
            "api_surface_changed": true,
            "migration_note_present": true,
        });
        let pred = apply_with(&outer_pkt, &safe, &packets).unwrap();
        assert_eq!(pred.rule_id, "pass_default");
    }

    #[test]
    fn apply_node_returns_false_when_resolver_cannot_find_packet() {
        let (_d, packets) = tmp_packets();
        let pred = Predicate::Apply {
            packet_id: "packet-deadbeef".into(),
            expect: vec!["breaking".into()],
            entity_map: BTreeMap::new(),
        };
        let entity = serde_json::Map::new();
        // With a real resolver that doesn't have the packet, eval returns false.
        assert!(!eval_predicate(&pred, &entity, &packets, 0));
        // With NoopResolver, also false.
        assert!(!eval_predicate(&pred, &entity, &NoopResolver, 0));
    }

    #[test]
    fn compile_rejects_apply_with_missing_sub_packet() {
        let (_d, packets) = tmp_packets();
        let err = packets
            .compile(&CompileParams {
                domain: "test".into(),
                scope: Some("global".into()),
                project: None,
                classification_lattice: None,
                prefix_inference: None,
                rank_table: None,
                threshold_table: None,
                rank_lookup_key: None,
                threshold_lookup_key: None,
                source_ids: None,
                rules: json!([{
                    "id": "fail_missing",
                    "antecedent": {
                        "op": "Apply",
                        "packet_id": "packet-nonexistent",
                        "expect": ["breaking"]
                    },
                    "consequent": "REJECT"
                }]),
            })
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("packet-nonexistent") && msg.contains("not in the store"),
            "expected missing-packet error, got: {msg}"
        );
    }

    #[test]
    fn compile_rejects_apply_with_expect_outside_sub_lattice() {
        let (_d, packets) = tmp_packets();
        let sub_id = compile_breaking_packet(&packets);
        let err = packets
            .compile(&CompileParams {
                domain: "test".into(),
                scope: Some("global".into()),
                project: None,
                classification_lattice: None,
                prefix_inference: None,
                rank_table: None,
                threshold_table: None,
                rank_lookup_key: None,
                threshold_lookup_key: None,
                source_ids: None,
                rules: json!([{
                    "id": "fail_typo",
                    "antecedent": {
                        "op": "Apply",
                        "packet_id": sub_id,
                        "expect": ["brekaing"]  // typo
                    },
                    "consequent": "REJECT"
                }]),
            })
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("brekaing") && msg.contains("lattice"),
            "expected lattice-mismatch error, got: {msg}"
        );
    }

    #[test]
    fn apply_node_entity_map_rebinds_fields() {
        let (_d, packets) = tmp_packets();
        let sub_id = compile_breaking_packet(&packets);

        // Outer's entity schema uses DIFFERENT field names.
        // Map outer's `did_break` → sub's `api_surface_changed`, etc.
        let outer = packets
            .compile(&CompileParams {
                domain: "pr-triage-remap".into(),
                scope: Some("global".into()),
                project: None,
                classification_lattice: None,
                prefix_inference: None,
                rank_table: None,
                threshold_table: None,
                rank_lookup_key: None,
                threshold_lookup_key: None,
                source_ids: None,
                rules: json!([
                    {
                        "id": "fail_via_mapped",
                        "antecedent": {
                            "op": "Apply",
                            "packet_id": sub_id,
                            "expect": ["breaking"],
                            "entity_map": {
                                "api_surface_changed": "did_break",
                                "migration_note_present": "has_migration_doc"
                            }
                        },
                        "consequent": "REJECT"
                    },
                    {
                        "id": "pass_default",
                        "emit": "fallback",
                        "antecedent": {"op": "True"},
                        "consequent": "ACCEPT"
                    }
                ]),
            })
            .unwrap();
        let outer_id = outer.split_whitespace().nth(1).unwrap();
        let outer_pkt = packets.load(outer_id).unwrap();

        // Entity with outer schema → mapping rebinds to sub schema.
        let breaking = json!({
            "did_break": true,
            "has_migration_doc": false,
        });
        let pred = apply_with(&outer_pkt, &breaking, &packets).unwrap();
        assert_eq!(pred.rule_id, "fail_via_mapped");
    }

    // ── Event logging tests ────────────────────────────────────────

    #[test]
    fn compile_ok_records_event_with_rules_count_and_refs() {
        let (_d, packets) = tmp_packets();
        let sub_id = compile_breaking_packet(&packets);
        // Outer packet composes the sub-packet — events should capture
        // the reference.
        let _ = packets
            .compile(&CompileParams {
                domain: "pr-triage-with-events".into(),
                scope: Some("global".into()),
                project: None,
                classification_lattice: None,
                prefix_inference: None,
                rank_table: None,
                threshold_table: None,
                rank_lookup_key: None,
                threshold_lookup_key: None,
                source_ids: None,
                rules: json!([
                    {
                        "id": "fail_if_breaking",
                        "antecedent": {
                            "op": "Apply",
                            "packet_id": sub_id.clone(),
                            "expect": ["breaking"]
                        },
                        "consequent": "REJECT"
                    },
                    {
                        "id": "pass_default",
                        "emit": "fallback",
                        "antecedent": {"op": "True"},
                        "consequent": "ACCEPT"
                    }
                ]),
            })
            .unwrap();

        let events = packets
            .list_events(Some("compile"), None, None, None, 50)
            .unwrap();
        // Two compile events: sub + outer
        assert_eq!(events.len(), 2);
        // Newest-first: outer is index 0
        let outer = &events[0];
        assert_eq!(outer.op, "compile");
        assert_eq!(outer.outcome, "ok");
        assert_eq!(outer.domain.as_deref(), Some("pr-triage-with-events"));
        let refs = outer
            .details
            .get("referenced_packets")
            .unwrap()
            .as_array()
            .unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].as_str().unwrap(), sub_id);
    }

    #[test]
    fn compile_error_records_event_with_error_message() {
        let (_d, packets) = tmp_packets();
        let _ = packets
            .compile(&CompileParams {
                domain: "broken-compile".into(),
                scope: Some("global".into()),
                project: None,
                classification_lattice: None,
                prefix_inference: None,
                rank_table: None,
                threshold_table: None,
                rank_lookup_key: None,
                threshold_lookup_key: None,
                source_ids: None,
                rules: json!([{
                    "id": "fail_bad_ref",
                    "antecedent": {
                        "op": "Apply",
                        "packet_id": "packet-nonexistent",
                        "expect": ["breaking"]
                    },
                    "consequent": "REJECT"
                }]),
            })
            .unwrap_err();

        let events = packets
            .list_events(Some("compile"), None, Some("error"), None, 50)
            .unwrap();
        assert_eq!(events.len(), 1);
        let ev = &events[0];
        assert_eq!(ev.outcome, "error");
        assert_eq!(ev.domain.as_deref(), Some("broken-compile"));
        let err = ev.details.get("error").unwrap().as_str().unwrap();
        assert!(
            err.contains("packet-nonexistent"),
            "error detail missing id: {err}"
        );
    }

    #[test]
    fn apply_tool_records_ok_and_no_match_events() {
        let (_d, packets) = tmp_packets();
        let id = compile_breaking_packet(&packets);

        // Breaking entity — should match.
        let _ = packets
            .apply_tool(&ApplyParams {
                packet_id: id.clone(),
                entity: json!({
                    "api_surface_changed": true,
                    "migration_note_present": false,
                }),
                mode: Some(ApplyMode::First),
            })
            .unwrap();

        // Safe entity — breaking_api rule won't fire; safe_default
        // (fallback) doesn't fire in mode=first either way because
        // first-match and fallback interact differently; either way
        // we record an event.
        let _ = packets
            .apply_tool(&ApplyParams {
                packet_id: id.clone(),
                entity: json!({
                    "api_surface_changed": true,
                    "migration_note_present": true,
                }),
                mode: Some(ApplyMode::First),
            })
            .unwrap();

        let events = packets
            .list_events(Some("apply"), Some(&id), None, None, 50)
            .unwrap();
        // Two apply events, one per call.
        assert_eq!(events.len(), 2);
        // At least one ok (the breaking entity fired a rule).
        assert!(events.iter().any(|e| e.outcome == "ok"));
    }

    #[test]
    fn audit_tool_records_fidelity_and_mismatch_count() {
        let (_d, packets) = tmp_packets();
        let id = compile_breaking_packet(&packets);

        let _ = packets
            .audit_tool(&AuditParams {
                packet_id: id.clone(),
                dataset: json!([
                    {
                        "entity": {"api_surface_changed": true, "migration_note_present": false},
                        "expected": "BREAKING"
                    }
                ]),
                mode: Some(ApplyMode::First),
            })
            .unwrap();

        let events = packets
            .list_events(Some("audit"), Some(&id), None, None, 50)
            .unwrap();
        assert_eq!(events.len(), 1);
        let ev = &events[0];
        assert_eq!(ev.outcome, "ok");
        let fidelity = ev.details.get("fidelity").unwrap().as_f64().unwrap();
        assert!((fidelity - 1.0).abs() < 1e-6);
    }

    #[test]
    fn log_gap_records_event_with_details() {
        let (_d, packets) = tmp_packets();
        let _ = packets
            .log_gap(
                "wanted to flag requests exceeding 10 per minute per user",
                Some("rate-limit"),
                Some("CountInWindow{path: 'requests[*]', window_seconds: 60, gt: 10}"),
                Some("prose rubric in reviewer instructions"),
                Some("RateCmp or Within{temporal}"),
            )
            .unwrap();

        let events = packets
            .list_events(Some("gap"), None, None, None, 10)
            .unwrap();
        assert_eq!(events.len(), 1);
        let ev = &events[0];
        assert_eq!(ev.op, "gap");
        assert_eq!(ev.outcome, "logged");
        assert_eq!(ev.domain.as_deref(), Some("rate-limit"));
        let desc = ev.details.get("description").unwrap().as_str().unwrap();
        assert!(desc.contains("10 per minute"));
        assert_eq!(
            ev.details
                .get("ast_feature_requested")
                .unwrap()
                .as_str()
                .unwrap(),
            "RateCmp or Within{temporal}"
        );
    }

    #[test]
    fn log_gap_rejects_empty_description() {
        let (_d, packets) = tmp_packets();
        let err = packets.log_gap("", None, None, None, None).unwrap_err();
        assert!(format!("{err:#}").contains("description"));
    }

    #[test]
    fn list_events_filters_and_newest_first() {
        let (_d, packets) = tmp_packets();
        // Fire a mix of operations.
        let id = compile_breaking_packet(&packets);
        let _ = packets.log_gap("gap A", None, None, None, None).unwrap();
        let _ = packets
            .apply_tool(&ApplyParams {
                packet_id: id.clone(),
                entity: json!({"api_surface_changed": false}),
                mode: Some(ApplyMode::First),
            })
            .unwrap();
        let _ = packets.log_gap("gap B", None, None, None, None).unwrap();

        // All events, default ordering (newest-first).
        let all = packets.list_events(None, None, None, None, 100).unwrap();
        assert!(!all.is_empty());
        // Newest first — last logged gap ("gap B") should be first.
        let first_gap = all
            .iter()
            .find(|e| e.op == "gap")
            .expect("at least one gap event");
        assert!(first_gap
            .details
            .get("description")
            .unwrap()
            .as_str()
            .unwrap()
            .contains("gap B"));

        // Filter by op=gap — should be two.
        let gaps = packets
            .list_events(Some("gap"), None, None, None, 100)
            .unwrap();
        assert_eq!(gaps.len(), 2);

        // Limit honored.
        let limited = packets.list_events(None, None, None, None, 1).unwrap();
        assert_eq!(limited.len(), 1);
    }

    #[test]
    fn apply_node_respects_depth_limit() {
        let (_d, packets) = tmp_packets();

        // Build a chain: p0 references p1 references p2 ... up to limit.
        // Each packet has one rule: classification "match" fires iff the
        // NEXT packet in the chain says "match".
        //
        // We can construct this by (a) compiling a base packet that
        // always says "match", then (b) wrapping N times. With N >
        // MAX_COMPOSITION_DEPTH, the outermost call should return false
        // because depth exceeded.
        let base_id = {
            let out = packets
                .compile(&CompileParams {
                    domain: "chain-base".into(),
                    scope: Some("global".into()),
                    project: None,
                    classification_lattice: Some(vec!["match".into(), "nomatch".into()]),
                    prefix_inference: None,
                    rank_table: None,
                    threshold_table: None,
                    rank_lookup_key: None,
                    threshold_lookup_key: None,
                    source_ids: None,
                    rules: json!([{
                        "id": "always_match",
                        "classification": "match",
                        "antecedent": {"op": "True"},
                        "consequent": "M"
                    }]),
                })
                .unwrap();
            out.split_whitespace().nth(1).unwrap().to_string()
        };

        let mut current = base_id.clone();
        // Build a chain longer than the depth limit.
        for i in 0..(MAX_COMPOSITION_DEPTH + 2) {
            let out = packets
                .compile(&CompileParams {
                    domain: format!("chain-{i}"),
                    scope: Some("global".into()),
                    project: None,
                    classification_lattice: Some(vec!["match".into(), "nomatch".into()]),
                    prefix_inference: None,
                    rank_table: None,
                    threshold_table: None,
                    rank_lookup_key: None,
                    threshold_lookup_key: None,
                    source_ids: None,
                    rules: json!([
                        {
                            "id": "match_via_next",
                            "classification": "match",
                            "antecedent": {
                                "op": "Apply",
                                "packet_id": current.clone(),
                                "expect": ["match"]
                            },
                            "consequent": "M"
                        },
                        {
                            "id": "nomatch_default",
                            "classification": "nomatch",
                            "emit": "fallback",
                            "antecedent": {"op": "True"},
                            "consequent": "N"
                        }
                    ]),
                })
                .unwrap();
            current = out.split_whitespace().nth(1).unwrap().to_string();
        }

        // The outermost packet's eval should trip the depth limit
        // before reaching the base. That means `match_via_next` returns
        // false, the fallback `nomatch_default` fires, and the outer
        // verdict is "nomatch" — NOT "match".
        let outer = packets.load(&current).unwrap();
        let pred = apply_with(&outer, &json!({}), &packets).unwrap();
        assert_eq!(
            pred.classification, "nomatch",
            "depth limit should prevent the outer chain from resolving to 'match'"
        );
    }

    // ── Workflow-engine-driven extensions: dotted paths + In + StringMatches ──

    fn eval_simple(pred: &Predicate, entity: &serde_json::Value) -> bool {
        let map = entity.as_object().expect("entity must be a JSON object");
        eval_predicate(pred, map, &NoopResolver, 0)
    }

    #[test]
    fn dotted_path_eq_resolves() {
        let entity = json!({
            "vars": {"issue": 42, "labels": ["bug", "urgent"]},
            "outputs": {"Plan": {"branch": "fix/issue-42"}}
        });
        assert!(eval_simple(
            &Predicate::Eq {
                field: "vars.issue".into(),
                value: Value::Int(42),
            },
            &entity
        ));
        assert!(eval_simple(
            &Predicate::Eq {
                field: "outputs.Plan.branch".into(),
                value: Value::String("fix/issue-42".into()),
            },
            &entity
        ));
        assert!(!eval_simple(
            &Predicate::Eq {
                field: "vars.does_not_exist".into(),
                value: Value::Int(0),
            },
            &entity
        ));
    }

    #[test]
    fn dotted_path_array_index() {
        let entity = json!({"vars": {"labels": ["bug", "urgent"]}});
        assert!(eval_simple(
            &Predicate::Eq {
                field: "vars.labels.0".into(),
                value: Value::String("bug".into()),
            },
            &entity
        ));
        assert!(eval_simple(
            &Predicate::Eq {
                field: "vars.labels.1".into(),
                value: Value::String("urgent".into()),
            },
            &entity
        ));
    }

    #[test]
    fn in_predicate_set_membership() {
        let entity = json!({"last_signal": {"name": "pr-merged"}});
        let pred = Predicate::In {
            field: "last_signal.name".into(),
            values: vec![
                Value::String("pr-merged".into()),
                Value::String("pr-feedback".into()),
            ],
        };
        assert!(eval_simple(&pred, &entity));

        let entity2 = json!({"last_signal": {"name": "pr-other"}});
        assert!(!eval_simple(&pred, &entity2));

        let entity3 = json!({});
        assert!(!eval_simple(&pred, &entity3));
    }

    #[test]
    fn string_matches_regex() {
        let entity = json!({"event": "pull_request.synchronize"});
        let pred = Predicate::StringMatches {
            field: "event".into(),
            pattern: r"^pull_request\..*$".into(),
            case_insensitive: false,
        };
        assert!(eval_simple(&pred, &entity));

        let entity2 = json!({"event": "issue.opened"});
        assert!(!eval_simple(&pred, &entity2));
    }

    #[test]
    fn string_matches_case_insensitive() {
        let entity = json!({"branch": "Fix/Issue-42"});
        let pred = Predicate::StringMatches {
            field: "branch".into(),
            pattern: r"^fix/issue-".into(),
            case_insensitive: true,
        };
        assert!(eval_simple(&pred, &entity));
    }

    #[test]
    fn string_matches_invalid_regex_returns_false() {
        let entity = json!({"x": "anything"});
        let pred = Predicate::StringMatches {
            field: "x".into(),
            pattern: r"(unclosed".into(),
            case_insensitive: false,
        };
        assert!(!eval_simple(&pred, &entity));
    }

    #[test]
    fn dotted_path_in_quantified_path() {
        // Exists over `vars.labels[*]` — the core "labels contains
        // `bug`" idiom.
        let entity = json!({"vars": {"labels": ["bug", "urgent"]}});
        let pred = Predicate::Exists {
            path: "vars.labels[*]".into(),
            pred: Box::new(Predicate::Eq {
                field: "$".into(),
                value: Value::String("bug".into()),
            }),
        };
        assert!(eval_simple(&pred, &entity));
    }
}
