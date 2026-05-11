use super::*;
use rmcp::schemars;

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
    Default,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
    strum::EnumString,
    strum::AsRefStr,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum ApplyMode {
    #[default]
    First,
    All,
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
#[allow(dead_code)] // used by test-only `apply`/`apply_all` wrappers below
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
pub(crate) fn apply_entity_map(
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
pub(crate) fn as_sub_entity(
    item: &serde_json::Value,
) -> serde_json::Map<String, serde_json::Value> {
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
pub(crate) fn eval_predicate(
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
            let haystack = match entity_get_raw(entity, field) {
                Some(serde_json::Value::String(s)) => s,
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
                    tracing::warn!("StringMatches: invalid regex pattern {pattern:?}: {e}");
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
#[allow(dead_code)] // test-only wrapper around `apply_with`
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
#[allow(dead_code)] // test-only wrapper around `apply_all_with`
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
