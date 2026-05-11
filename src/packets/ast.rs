use super::Packet;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

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
    pub(super) fn from_json(v: &serde_json::Value) -> Option<Value> {
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

    /// Convert this typed Value back to a serde_json::Value. Round-trips
    /// the four scalar shapes losslessly.
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Value::Bool(b) => serde_json::Value::Bool(*b),
            Value::Int(i) => serde_json::Value::Number((*i).into()),
            Value::Float(f) => serde_json::Number::from_f64(*f)
                .map(serde_json::Value::Number)
                .unwrap_or(serde_json::Value::Null),
            Value::String(s) => serde_json::Value::String(s.clone()),
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
    pub(super) fn apply(self, lhs: usize, rhs: usize) -> bool {
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
pub(super) fn review_lattice() -> Vec<String> {
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
pub(super) fn review_prefix_inference() -> BTreeMap<String, String> {
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
#[allow(dead_code)] // test-only wrapper; tests in packets/mod.rs use this directly
pub(super) fn infer_classification(
    id: &str,
    prefix_map: &BTreeMap<String, String>,
) -> Option<String> {
    infer_classification_detailed(id, prefix_map).map(|(c, _)| c)
}

/// Variant of `infer_classification` that also returns the matching
/// prefix. Used by error messages so that a classification-lattice
/// mismatch can name the inferred prefix path explicitly, which was
/// the hidden-feature-most-likely-to-surprise per E14 (Gemini tripped
/// on a `review_` prefix auto-classifying as `manual` without seeing
/// the inference step in the error).
pub(super) fn infer_classification_detailed<'a>(
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
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum Emit {
    /// Default: rule fires whenever its antecedent matches.
    #[default]
    Independent,
    /// Catchall / default-case rule. In `apply_all`, fires ONLY when no
    /// `Independent` rule fired. In `apply_first`, behaves like any
    /// other rule (first-match-wins ordering still applies).
    Fallback,
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
pub(super) struct RuleInput {
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
pub(super) fn validate_path(path: &str, context: &str) -> Result<()> {
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
        anyhow::bail!("{}: dotted path '{}' has an empty segment", context, path);
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
pub(super) fn validate_predicate(pred: &Predicate, inside_forall: bool) -> Result<()> {
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
pub(super) fn collect_apply_refs(pred: &Predicate, out: &mut Vec<String>) {
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
pub(super) fn rule_antecedents_referencing<'a>(
    rules: &'a [Rule],
    packet_id: &str,
) -> Vec<&'a Predicate> {
    let mut out = Vec::new();
    for rule in rules {
        collect_apply_by_id(&rule.antecedent, packet_id, &mut out);
    }
    out
}

pub(super) fn collect_apply_by_id<'a>(
    pred: &'a Predicate,
    target: &str,
    out: &mut Vec<&'a Predicate>,
) {
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
pub(super) fn check_apply_expect_against_lattice(
    applies: &[&Predicate],
    sub: &Packet,
) -> Result<()> {
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
    pub(super) fn materialize(
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

pub(super) fn default_confidence() -> f32 {
    1.0
}
