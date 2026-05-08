use super::*;
use rmcp::schemars;

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
