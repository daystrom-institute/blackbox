//! `extract_java_class_cohesive_clusters` analysis-only plan kind.
//!
//! Source: `note-8967d541` / `java-refactor-remaining-notes.md` §2.
//!
//! Walks the method-to-field and method-to-method affinity graph emitted by
//! `java_class_dependency_analysis` and partitions the methods into cohesive
//! suggested clusters. Each cluster is shaped like an `extract_java_class`
//! preview: `name_hint`, `item_names`, `move_fields`, `score`, and an
//! `expected_wiring` inference for how the source class would talk to the
//! extracted target after the split.
//!
//! v1 is analysis-only: no FileEdits, no apply path. The operator reviews the
//! clusters and decides which (if any) to round-trip into an
//! `extract_java_class` plan. Deterministic conservative clustering keeps the
//! same inputs → same outputs and surfaces uncertainty as singleton clusters
//! rather than aggressive merges that hide cross-cluster coupling.
//!
//! Clustering signal: field co-touch dominates, weighted by *inverse field
//! frequency*. Two methods are linked by the fields they share, but a field's
//! per-pair contribution is `1/(deg-1)` where `deg` is how many methods touch
//! it: a field touched by exactly two methods is a strong (weight 1.0) link;
//! a high-fan-out *connector* field (a shared UI container, a refresh
//! dispatcher) touched by 40 methods contributes only `1/39` to each of its
//! pairs — diffuse weak edges that no longer fuse otherwise-distinct concerns.
//! Methods are then partitioned by **modularity community detection**
//! (Louvain local-moving) over that weighted graph, not transitive closure.
//! This is the connector-aware refinement (gap-2a3f03e5): plain transitive
//! field-sharing collapses a tangled god class into one megacluster the moment
//! a single bridge field touches every concern; modularity keeps concerns
//! whose strong intra-edges dominate the weak connector edges apart.
//!
//! Method-to-method call edges are NOT used to merge clusters — they appear in
//! the response as `cross_cluster_calls` so the operator can see the coupling
//! before deciding to split. Methods that touch zero class fields are attached
//! to whichever cluster they call most, falling back to a singleton "cluster"
//! so the operator decides where they belong. Determinism is preserved
//! end-to-end: nodes are visited in sorted order, gain ties prefer the
//! incumbent community then the smallest community id, so the same inputs
//! always yield the same partition.

use super::*;

pub(crate) fn plan_extract_java_class_cohesive_clusters(p: &RefactorPlanParams) -> Result<String> {
    // Reuse `java_class_dependency_analysis` so we don't duplicate the Java
    // class walker. The dependency analysis already returns methods, fields,
    // and the method_to_field / method_to_method edge graph we cluster over.
    let dep_json = plan_java_class_dependency_analysis(p)
        .context("java_class_dependency_analysis prerequisite failed")?;
    let v: serde_json::Value = serde_json::from_str(&dep_json)
        .context("internal: java_class_dependency_analysis produced invalid JSON")?;

    let class = v.get("class").cloned().unwrap_or(serde_json::json!({}));
    let class_name = class
        .get("name")
        .and_then(|n| n.as_str())
        .unwrap_or("(class)")
        .to_string();

    let methods: Vec<String> = v
        .get("methods")
        .and_then(|m| m.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m.get("name").and_then(|n| n.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let fields: Vec<String> = v
        .get("fields")
        .and_then(|f| f.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|f| f.get("name").and_then(|n| n.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let m2f_pairs: Vec<(String, String)> = v
        .get("edges")
        .and_then(|e| e.get("method_to_field"))
        .and_then(|m| m.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|e| {
                    Some((
                        e.get("method")?.as_str()?.to_string(),
                        e.get("field")?.as_str()?.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    let m2m_pairs: Vec<(String, String)> = v
        .get("edges")
        .and_then(|e| e.get("method_to_method"))
        .and_then(|m| m.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|e| {
                    Some((
                        e.get("from")?.as_str()?.to_string(),
                        e.get("to")?.as_str()?.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();

    // Exclude constructors from the clustering signal. A constructor
    // (name == class name) assigns essentially every field, so its
    // method_to_field edges connect all fields transitively and collapse
    // every concern into one cluster — fatal on exactly the god classes
    // this analysis targets (a 30-field view-class ctor merges
    // everything). Constructors are never extracted to a delegate anyway,
    // so dropping them from the graph is strictly correct for cohesion.
    let methods: Vec<String> = methods.into_iter().filter(|m| *m != class_name).collect();
    let m2f_pairs: Vec<(String, String)> = m2f_pairs
        .into_iter()
        .filter(|(m, _)| *m != class_name)
        .collect();
    let m2m_pairs: Vec<(String, String)> = m2m_pairs
        .into_iter()
        .filter(|(from, to)| *from != class_name && *to != class_name)
        .collect();

    let clustering = cluster_methods(&methods, &fields, &m2f_pairs, &m2m_pairs);
    let cross_cluster_calls = compute_cross_cluster_calls(&m2m_pairs, &clustering);

    let suggested_clusters: Vec<serde_json::Value> = clustering
        .clusters
        .iter()
        .map(|c| serialize_cluster(c, &m2f_pairs, &m2m_pairs, &cross_cluster_calls))
        .collect();

    let cross_value: Vec<serde_json::Value> = cross_cluster_calls
        .iter()
        .map(|c| {
            serde_json::json!({
                "from_cluster": c.from_cluster,
                "to_cluster": c.to_cluster,
                "from_method": c.from_method,
                "to_method": c.to_method,
            })
        })
        .collect();

    let body = serde_json::json!({
        "status": "ok",
        "kind": "extract_java_class_cohesive_clusters",
        "title": format!("Cohesive cluster suggestions for `{class_name}`"),
        "semantic_status": SemanticStatus::SyntaxOnly,
        "dry_run": true,
        "analysis_only": true,
        "file_moves": [],
        "edits": [],
        "validations": [],
        "items": [],
        "leftovers": [],
        "plan_status": PlanStatus::Planned,
        "class": class,
        "suggested_clusters": suggested_clusters,
        "cross_cluster_calls": cross_value,
    });
    Ok(serde_json::to_string_pretty(&body)?)
}

// ──────────────────────────── clustering ────────────────────────────

#[derive(Debug, Clone)]
struct Cluster {
    /// Stable cluster id (`cluster-1`, `cluster-2`, ...). Assigned in
    /// deterministic order after clustering.
    id: String,
    /// Operator-facing name hint inferred from method-name prefixes.
    name_hint: String,
    /// Methods in this cluster, sorted.
    item_names: Vec<String>,
    /// Fields that ONLY methods in this cluster touch (read or write).
    /// Fields touched by methods in multiple clusters are not moveable
    /// without a delegate accessor and stay in source.
    move_fields: Vec<String>,
}

#[derive(Debug, Clone)]
struct ClusteringResult {
    clusters: Vec<Cluster>,
    /// method_name -> cluster_id
    membership: BTreeMap<String, String>,
}

/// Resolution parameter for the modularity objective. 1.0 is standard
/// Newman-Girvan modularity; higher values bias toward more, smaller
/// communities (finer seams). Kept at the classic default — the
/// inverse-field-frequency weighting already does the connector down-weighting,
/// so resolution is left as the obvious future tuning knob, not a band-aid.
const MODULARITY_RESOLUTION: f64 = 1.0;

/// Minimum strict modularity gain required to move a node out of its incumbent
/// community. Guards against float noise driving non-deterministic churn.
const MOVE_EPSILON: f64 = 1e-9;

fn cluster_methods(
    methods: &[String],
    fields: &[String],
    m2f_pairs: &[(String, String)],
    m2m_pairs: &[(String, String)],
) -> ClusteringResult {
    // method -> fields touched
    let mut method_fields: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for m in methods {
        method_fields.insert(m.clone(), BTreeSet::new());
    }
    for (m, f) in m2f_pairs {
        method_fields
            .entry(m.clone())
            .or_default()
            .insert(f.clone());
    }

    // field -> the methods that touch it (its degree). Constructors are
    // already filtered out by the caller, so degree reflects real concerns.
    let mut field_methods: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for field in fields {
        let mut touching: Vec<String> = method_fields
            .iter()
            .filter(|(_, fs)| fs.contains(field))
            .map(|(m, _)| m.clone())
            .collect();
        touching.sort();
        if touching.len() >= 2 {
            field_methods.insert(field.clone(), touching);
        }
    }

    // Inverse-field-frequency weighted method↔method affinity. A field of
    // degree `d` contributes `1/(d-1)` to each of its C(d,2) method pairs:
    // a degree-2 field is a full-strength link, a high-degree connector field
    // is spread thin across many pairs and cannot fuse concerns on its own.
    let mut affinity: BTreeMap<(String, String), f64> = BTreeMap::new();
    for touching in field_methods.values() {
        let d = touching.len();
        let contrib = 1.0 / (d as f64 - 1.0);
        for i in 0..touching.len() {
            for j in (i + 1)..touching.len() {
                let key = ordered_pair(&touching[i], &touching[j]);
                *affinity.entry(key).or_default() += contrib;
            }
        }
    }

    // Partition by modularity community detection over the weighted graph.
    let membership_idx = louvain_local_moving(methods, &affinity);

    // Methods that touch ZERO fields are pure helpers with no field-affinity
    // edge; modularity leaves them as singletons. Attach each to the community
    // it calls into (or is called from) most — those bodies likely belong
    // together. This preserves the v1 call-attach post-pass semantics; it fires
    // only for genuinely field-less methods (a method touching a single
    // private field stays its own seam, as before).
    let mut comm = membership_idx;
    let initial_comm = comm.clone();
    for m in methods {
        let field_less = method_fields.get(m).map(|f| f.is_empty()).unwrap_or(true);
        if !field_less {
            continue;
        }
        let own = initial_comm[m];
        // Singleton check against the post-modularity partition.
        let own_count = comm.values().filter(|c| **c == own).count();
        if own_count > 1 {
            continue;
        }
        let mut tally: BTreeMap<usize, usize> = BTreeMap::new();
        for (from, to) in m2m_pairs {
            // The community on the OTHER end of a call edge touching `m`.
            let other = if from == m {
                initial_comm.get(to).copied()
            } else if to == m {
                initial_comm.get(from).copied()
            } else {
                None
            };
            if let Some(other) = other.filter(|o| *o != own) {
                *tally.entry(other).or_default() += 1;
            }
        }
        if let Some((target, _)) = tally
            .into_iter()
            .max_by(|a, b| a.1.cmp(&b.1).then(b.0.cmp(&a.0)))
        {
            comm.insert(m.clone(), target);
        }
    }

    // Group methods by final community.
    let mut groups: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    for m in methods {
        groups.entry(comm[m]).or_default().push(m.clone());
    }

    // Stable ordering: lexicographic first-method.
    let mut group_vec: Vec<Vec<String>> = groups.into_values().collect();
    for g in group_vec.iter_mut() {
        g.sort();
    }
    group_vec.sort_by(|a, b| a[0].cmp(&b[0]));

    // Build clusters with name hints and move_fields.
    let mut clusters = Vec::with_capacity(group_vec.len());
    let mut membership = BTreeMap::new();
    for (idx, item_names) in group_vec.iter().enumerate() {
        let id = format!("cluster-{}", idx + 1);
        for m in item_names {
            membership.insert(m.clone(), id.clone());
        }
        // move_fields = fields touched only by methods in this cluster.
        let in_cluster: BTreeSet<&String> = item_names.iter().collect();
        let mut move_fields = Vec::new();
        for field in fields {
            let touching_methods: Vec<&String> = method_fields
                .iter()
                .filter(|(_, fs)| fs.contains(field))
                .map(|(m, _)| m)
                .collect();
            if touching_methods.is_empty() {
                continue;
            }
            if touching_methods.iter().all(|m| in_cluster.contains(m)) {
                // Only include if at least one method in this cluster touches it.
                if touching_methods.iter().any(|m| in_cluster.contains(m)) {
                    move_fields.push(field.clone());
                }
            }
        }
        move_fields.sort();
        let name_hint = infer_name_hint(item_names, &move_fields);
        clusters.push(Cluster {
            id,
            name_hint,
            item_names: item_names.clone(),
            move_fields,
        });
    }

    ClusteringResult {
        clusters,
        membership,
    }
}

// ──────────────────────────── modularity ────────────────────────────

fn ordered_pair(a: &str, b: &str) -> (String, String) {
    if a <= b {
        (a.to_string(), b.to_string())
    } else {
        (b.to_string(), a.to_string())
    }
}

/// One level of Louvain modularity optimization (local-moving phase) over the
/// weighted method-affinity graph. Returns `method -> community index`.
///
/// Single-level local moving is sufficient at class scale (tens of methods):
/// each node starts in its own community and is repeatedly moved to the
/// neighboring community that maximizes the modularity gain, until a full pass
/// makes no move. Determinism: nodes are visited in the order of `methods`
/// (the caller sorts upstream), candidate communities are evaluated in sorted
/// id order, and a move requires a strictly positive gain over staying — so
/// isolated nodes (degree 0) never drift and ties never flip-flop.
fn louvain_local_moving(
    methods: &[String],
    affinity: &BTreeMap<(String, String), f64>,
) -> BTreeMap<String, usize> {
    // Seed: community index = position in `methods`.
    let index_of: BTreeMap<&str, usize> = methods
        .iter()
        .enumerate()
        .map(|(i, m)| (m.as_str(), i))
        .collect();

    // Weighted adjacency + node degree (k_i) + total edge weight (m).
    let mut adj: Vec<Vec<(usize, f64)>> = vec![Vec::new(); methods.len()];
    let mut degree: Vec<f64> = vec![0.0; methods.len()];
    let mut total_weight = 0.0_f64;
    for ((a, b), w) in affinity {
        let (ia, ib) = match (index_of.get(a.as_str()), index_of.get(b.as_str())) {
            (Some(ia), Some(ib)) => (*ia, *ib),
            _ => continue,
        };
        adj[ia].push((ib, *w));
        adj[ib].push((ia, *w));
        degree[ia] += *w;
        degree[ib] += *w;
        total_weight += *w;
    }

    let mut community: Vec<usize> = (0..methods.len()).collect();
    // sigma_tot[c] = sum of degrees of nodes currently in community c.
    let mut sigma_tot: BTreeMap<usize, f64> = BTreeMap::new();
    for (i, d) in degree.iter().enumerate() {
        *sigma_tot.entry(i).or_default() += *d;
    }

    if total_weight <= 0.0 {
        // No edges at all → every method is its own seam.
        return methods
            .iter()
            .enumerate()
            .map(|(i, m)| (m.clone(), i))
            .collect();
    }
    let two_m = 2.0 * total_weight;

    let mut improved = true;
    let mut passes = 0;
    // Convergence is monotone in modularity; the bound is a deterministic
    // backstop against float-driven cycling on pathological inputs.
    let max_passes = 50;
    while improved && passes < max_passes {
        improved = false;
        passes += 1;
        for i in 0..methods.len() {
            let own = community[i];
            let k_i = degree[i];

            // Weight from i into each neighboring community.
            let mut k_in: BTreeMap<usize, f64> = BTreeMap::new();
            for (j, w) in &adj[i] {
                *k_in.entry(community[*j]).or_default() += *w;
            }

            // Remove i from its community before scoring candidates.
            *sigma_tot.entry(own).or_default() -= k_i;

            // Gain of placing i into community c (relative to i isolated):
            //   ΔQ(c) = k_in(c) - γ · sigma_tot[c] · k_i / (2m)
            let gain = |c: usize| -> f64 {
                let k_in_c = k_in.get(&c).copied().unwrap_or(0.0);
                let sig = sigma_tot.get(&c).copied().unwrap_or(0.0);
                k_in_c - MODULARITY_RESOLUTION * sig * k_i / two_m
            };

            let own_gain = gain(own);
            let mut best = own;
            let mut best_gain = own_gain;
            // Candidates: communities reachable through an incident edge.
            // Sorted iteration over the BTreeMap keys keeps ties deterministic.
            for c in k_in.keys() {
                let g = gain(*c);
                if g > best_gain + MOVE_EPSILON {
                    best_gain = g;
                    best = *c;
                }
            }

            // Re-add i to the chosen community.
            *sigma_tot.entry(best).or_default() += k_i;
            if best != own {
                community[i] = best;
                improved = true;
            }
        }
    }

    methods
        .iter()
        .enumerate()
        .map(|(i, m)| (m.clone(), community[i]))
        .collect()
}

/// Generic verb prefixes that name an *action*, not a *concern*. A cluster of
/// `onFoo`/`onBar` handlers or `getX`/`setX` accessors whose dominant token is
/// one of these gets a worse-than-useless hint ("on", "get"); we prefer the
/// concern carried by the cluster's fields instead.
const GENERIC_TOKENS: &[&str] = &[
    "on", "get", "set", "is", "has", "do", "handle", "update", "refresh", "init", "create",
    "build", "add", "remove",
];

/// Infer a class-name hint for a cluster. Primary signal is the most common
/// non-generic first camelCase token across the cluster's methods (e.g.
/// `bill`, `search`). When that signal is absent — every method shares a
/// generic action verb like `on`/`get`, or methods share no token at all — the
/// hint falls back to the dominant move_field's leading token, which names the
/// *concern* the methods operate on rather than the action they perform. Ties
/// break lexicographically; the raw token is returned (operator capitalizes /
/// pluralizes when accepting).
fn infer_name_hint(method_names: &[String], move_fields: &[String]) -> String {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut generic_counts: BTreeMap<String, usize> = BTreeMap::new();
    for n in method_names {
        let t = first_camel_token(n.as_str());
        if t.is_empty() {
            continue;
        }
        if GENERIC_TOKENS.contains(&t.as_str()) {
            *generic_counts.entry(t).or_default() += 1;
        } else {
            *counts.entry(t).or_default() += 1;
        }
    }

    let best_concept = counts
        .into_iter()
        .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(&a.0)))
        .map(|(t, _)| t);
    if let Some(t) = best_concept {
        return t;
    }

    // No concrete method-name concern. Fall back to the field the cluster owns.
    if let Some(field) = move_fields.first() {
        let t = first_camel_token(field.as_str());
        if !t.is_empty() {
            return t;
        }
    }

    // Last resort: the dominant generic verb (better than empty).
    generic_counts
        .into_iter()
        .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(&a.0)))
        .map(|(t, _)| t)
        .unwrap_or_default()
}

fn first_camel_token(name: &str) -> String {
    let mut out = String::new();
    for (i, c) in name.chars().enumerate() {
        if i > 0 && c.is_uppercase() {
            break;
        }
        if i == 0 {
            out.extend(c.to_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

// ──────────────────────────── cross-cluster + wiring ────────────────────────────

#[derive(Debug, Clone)]
struct CrossClusterCall {
    from_cluster: String,
    to_cluster: String,
    from_method: String,
    to_method: String,
}

fn compute_cross_cluster_calls(
    m2m_pairs: &[(String, String)],
    clustering: &ClusteringResult,
) -> Vec<CrossClusterCall> {
    let mut out = Vec::new();
    for (from, to) in m2m_pairs {
        let from_c = match clustering.membership.get(from) {
            Some(c) => c,
            None => continue,
        };
        let to_c = match clustering.membership.get(to) {
            Some(c) => c,
            None => continue,
        };
        if from_c == to_c {
            continue;
        }
        out.push(CrossClusterCall {
            from_cluster: from_c.clone(),
            to_cluster: to_c.clone(),
            from_method: from.clone(),
            to_method: to.clone(),
        });
    }
    out.sort_by(|a, b| {
        a.from_cluster
            .cmp(&b.from_cluster)
            .then_with(|| a.to_cluster.cmp(&b.to_cluster))
            .then_with(|| a.from_method.cmp(&b.from_method))
            .then_with(|| a.to_method.cmp(&b.to_method))
    });
    out
}

fn serialize_cluster(
    cluster: &Cluster,
    m2f_pairs: &[(String, String)],
    m2m_pairs: &[(String, String)],
    cross: &[CrossClusterCall],
) -> serde_json::Value {
    // Internal field accesses + internal call edges count toward cohesion.
    let in_cluster: BTreeSet<&String> = cluster.item_names.iter().collect();
    let internal_field_touches = m2f_pairs
        .iter()
        .filter(|(m, _)| in_cluster.contains(m))
        .count();
    let internal_calls = m2m_pairs
        .iter()
        .filter(|(f, t)| in_cluster.contains(f) && in_cluster.contains(t))
        .count();

    let inbound = cross.iter().filter(|c| c.to_cluster == cluster.id).count();
    let outbound = cross
        .iter()
        .filter(|c| c.from_cluster == cluster.id)
        .count();

    let internal_total = internal_field_touches + internal_calls;
    let score = if internal_total + inbound + outbound == 0 {
        0.0
    } else {
        internal_total as f64 / (internal_total + inbound + outbound) as f64
    };

    let expected_wiring = infer_wiring(inbound, outbound);

    serde_json::json!({
        "id": cluster.id,
        "name_hint": cluster.name_hint,
        "item_names": cluster.item_names,
        "move_fields": cluster.move_fields,
        "score": round4(score),
        "internal_field_touches": internal_field_touches,
        "internal_calls": internal_calls,
        "inbound_calls": inbound,
        "outbound_calls": outbound,
        "expected_wiring": expected_wiring,
    })
}

fn round4(x: f64) -> f64 {
    (x * 10_000.0).round() / 10_000.0
}

fn infer_wiring(inbound: usize, outbound: usize) -> &'static str {
    // From the perspective of THIS cluster after extraction:
    //  - inbound > 0, outbound == 0  → source class can hold this cluster as
    //    a private delegate field and call into it; nothing needs to call
    //    back. "delegate".
    //  - outbound > 0, inbound == 0  → this cluster needs to invoke methods
    //    still living on the source. Operator threads those as functional
    //    callbacks (Runnable/Consumer/Supplier) when wiring the extract.
    //    "callback".
    //  - both > 0                    → bidirectional coupling. The split is
    //    not clean; operator likely needs to keep a shared source_instance
    //    reference or rethink the partition. "source_instance".
    //  - both 0                      → no coupling at all. Trivially a
    //    delegate (source holds it as field; never calls back).
    match (inbound, outbound) {
        (0, 0) => "delegate",
        (_, 0) => "delegate",
        (0, _) => "callback",
        _ => "source_instance",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn make_params(source: &Path) -> RefactorPlanParams {
        RefactorPlanParams {
            kind: "extract_java_class_cohesive_clusters".to_string(),
            source: source.to_string_lossy().into_owned(),
            ..Default::default()
        }
    }

    #[test]
    fn billing_and_search_god_class_yields_two_clusters() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("Dashboard.java");
        fs::write(
            &source,
            "package com.example;\n\
             public class Dashboard {\n\
            \x20   private double billTotal;\n\
            \x20   private String billCustomer;\n\
            \x20   private String searchQuery;\n\
            \x20   private int searchPageSize;\n\
            \x20   \n\
            \x20   public void billCalculate() {\n\
            \x20       billTotal = 0.0;\n\
            \x20       billCustomer = \"a\";\n\
            \x20   }\n\
            \x20   public double billRefresh() {\n\
            \x20       return billTotal;\n\
            \x20   }\n\
            \x20   public void searchRun() {\n\
            \x20       searchQuery = \"q\";\n\
            \x20       searchPageSize = 10;\n\
            \x20   }\n\
            \x20   public int searchPageSize() {\n\
            \x20       return searchPageSize;\n\
            \x20   }\n\
             }\n",
        )
        .unwrap();

        let response = plan_extract_java_class_cohesive_clusters(&make_params(&source))
            .expect("cohesive clusters plan should succeed");
        let v: serde_json::Value = serde_json::from_str(&response).unwrap();

        assert_eq!(v["kind"], "extract_java_class_cohesive_clusters");
        assert_eq!(v["analysis_only"], true);
        assert_eq!(v["dry_run"], true);
        assert!(
            v["edits"].as_array().unwrap().is_empty(),
            "analysis-only response must not emit edits"
        );

        let clusters = v["suggested_clusters"].as_array().unwrap();
        assert_eq!(
            clusters.len(),
            2,
            "expected 2 clusters (billing, search), got {clusters:?}"
        );

        let billing = clusters
            .iter()
            .find(|c| c["name_hint"] == "bill")
            .expect("missing billing cluster");
        let billing_items: Vec<&str> = billing["item_names"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s.as_str().unwrap())
            .collect();
        assert!(billing_items.contains(&"billCalculate"));
        assert!(billing_items.contains(&"billRefresh"));
        let billing_fields: Vec<&str> = billing["move_fields"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s.as_str().unwrap())
            .collect();
        assert!(billing_fields.contains(&"billTotal"));
        assert!(billing_fields.contains(&"billCustomer"));

        let search = clusters
            .iter()
            .find(|c| c["name_hint"] == "search")
            .expect("missing search cluster");
        let search_items: Vec<&str> = search["item_names"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s.as_str().unwrap())
            .collect();
        assert!(search_items.contains(&"searchRun"));
        assert!(search_items.contains(&"searchPageSize"));
        let search_fields: Vec<&str> = search["move_fields"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s.as_str().unwrap())
            .collect();
        assert!(search_fields.contains(&"searchQuery"));
        assert!(search_fields.contains(&"searchPageSize"));

        // No cross-cluster calls in this fixture — clusters are independent.
        let cross = v["cross_cluster_calls"].as_array().unwrap();
        assert!(
            cross.is_empty(),
            "expected zero cross-cluster calls in disjoint fixture, got {cross:?}"
        );

        // Both clusters have only outbound=0/inbound=0 → wiring "delegate".
        assert_eq!(billing["expected_wiring"], "delegate");
        assert_eq!(search["expected_wiring"], "delegate");
    }

    #[test]
    fn cross_cluster_call_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("Coupled.java");
        fs::write(
            &source,
            "package com.example;\n\
             public class Coupled {\n\
            \x20   private double billTotal;\n\
            \x20   private String searchQuery;\n\
            \x20   \n\
            \x20   public void billCalculate() {\n\
            \x20       billTotal = 1.0;\n\
            \x20       searchHelper();\n\
            \x20   }\n\
            \x20   public void searchHelper() {\n\
            \x20       searchQuery = \"q\";\n\
            \x20   }\n\
             }\n",
        )
        .unwrap();

        let response = plan_extract_java_class_cohesive_clusters(&make_params(&source))
            .expect("plan should succeed");
        let v: serde_json::Value = serde_json::from_str(&response).unwrap();
        let cross = v["cross_cluster_calls"].as_array().unwrap();
        assert!(
            cross.iter().any(|c| {
                c["from_method"] == "billCalculate" && c["to_method"] == "searchHelper"
            }),
            "expected billCalculate→searchHelper cross-cluster call, got {cross:?}"
        );

        // Verify wiring inference: caller cluster (billing) is outbound-only →
        // "callback"; callee cluster (search) is inbound-only → "delegate".
        let clusters = v["suggested_clusters"].as_array().unwrap();
        let billing = clusters
            .iter()
            .find(|c| {
                c["item_names"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|n| n == "billCalculate")
            })
            .unwrap();
        let search = clusters
            .iter()
            .find(|c| {
                c["item_names"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|n| n == "searchHelper")
            })
            .unwrap();
        assert_eq!(
            billing["expected_wiring"], "callback",
            "billing cluster calls outward → callback"
        );
        assert_eq!(
            search["expected_wiring"], "delegate",
            "search cluster only receives calls → delegate"
        );
    }

    #[test]
    fn response_shape_marks_analysis_only_and_includes_class_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("Tiny.java");
        fs::write(
            &source,
            "package com.example;\npublic class Tiny { private int x; public int get() { return x; } }\n",
        )
        .unwrap();
        let response = plan_extract_java_class_cohesive_clusters(&make_params(&source)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(v["kind"], "extract_java_class_cohesive_clusters");
        assert_eq!(v["analysis_only"], true);
        assert_eq!(v["plan_status"], "planned");
        assert!(v["file_moves"].as_array().unwrap().is_empty());
        assert!(v["validations"].as_array().unwrap().is_empty());
        assert_eq!(v["class"]["name"], "Tiny");
        assert_eq!(v["class"]["package"], "com.example");
    }

    // The connector-aware regression (gap-2a3f03e5). Two genuine concerns —
    // pricing (rate/base) and inventory (quantity/threshold) — are bridged by a
    // single high-fan-out `container` field that EVERY method touches (a shared
    // UI panel / dispatcher, the exact god-class shape). Plain transitive
    // field-sharing unions all six methods through `container` into ONE
    // megacluster. Inverse-field-frequency weighting + modularity must keep the
    // two concerns apart: `container` (degree 6) contributes only 1/5 per pair,
    // while the concern-private fields (degree 2-3) are full-strength links.
    #[test]
    fn connector_field_does_not_merge_distinct_concerns() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("ReportPanel.java");
        fs::write(
            &source,
            "package com.acme;\n\
             public class ReportPanel {\n\
            \x20   private final Panel container;\n\
            \x20   private double rate;\n\
            \x20   private double base;\n\
            \x20   private int quantity;\n\
            \x20   private int threshold;\n\
            \x20   public ReportPanel(Panel c) { this.container = c; }\n\
            \x20   public double priceBase() { container.show(); return base * rate; }\n\
            \x20   public double priceDiscount() { container.show(); return base * rate * 0.9; }\n\
            \x20   public void priceReset() { container.show(); base = 0; rate = 0; }\n\
            \x20   public void stockAdd(int n) { container.show(); quantity += n; }\n\
            \x20   public boolean stockLow() { container.show(); return quantity < threshold; }\n\
            \x20   public void stockReset() { container.show(); quantity = 0; threshold = 0; }\n\
             }\n",
        )
        .unwrap();

        let response = plan_extract_java_class_cohesive_clusters(&make_params(&source))
            .expect("plan should succeed");
        let v: serde_json::Value = serde_json::from_str(&response).unwrap();
        let clusters = v["suggested_clusters"].as_array().unwrap();

        let cluster_of = |method: &str| -> String {
            clusters
                .iter()
                .find(|c| {
                    c["item_names"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|n| n == method)
                })
                .unwrap_or_else(|| {
                    panic!("method {method} not placed in any cluster: {clusters:?}")
                })["id"]
                .as_str()
                .unwrap()
                .to_string()
        };

        // The connector did NOT merge the two concerns.
        assert_ne!(
            cluster_of("priceBase"),
            cluster_of("stockLow"),
            "pricing and inventory must not collapse through the connector field: {clusters:?}"
        );

        // Pricing cohesion held: all three pricing methods land together.
        let pricing = cluster_of("priceBase");
        assert_eq!(cluster_of("priceDiscount"), pricing, "{clusters:?}");
        assert_eq!(cluster_of("priceReset"), pricing, "{clusters:?}");

        // The high-fan-out connector field is touched across clusters, so it is
        // NOT moveable — it must appear in no cluster's move_fields and stay in
        // the source class.
        for c in clusters {
            let move_fields: Vec<&str> = c["move_fields"]
                .as_array()
                .unwrap()
                .iter()
                .map(|s| s.as_str().unwrap())
                .collect();
            assert!(
                !move_fields.contains(&"container"),
                "connector field `container` must not be moved: {c:?}"
            );
        }
    }

    #[test]
    fn name_hint_falls_back_to_field_for_generic_handler_cluster() {
        // A cluster of `on*` event handlers all sharing a `selection` field:
        // the dominant method token is the generic verb "on" — useless as a
        // class name — so the hint should fall back to the field concern.
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("Handlers.java");
        fs::write(
            &source,
            "package com.acme;\n\
             public class Handlers {\n\
            \x20   private String selection;\n\
            \x20   public void onClick() { selection = \"a\"; }\n\
            \x20   public void onHover() { selection = \"b\"; }\n\
            \x20   public String onRead() { return selection; }\n\
             }\n",
        )
        .unwrap();
        let response = plan_extract_java_class_cohesive_clusters(&make_params(&source)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&response).unwrap();
        let clusters = v["suggested_clusters"].as_array().unwrap();
        let handler = clusters
            .iter()
            .find(|c| {
                c["item_names"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|n| n == "onClick")
            })
            .expect("handler cluster present");
        assert_eq!(
            handler["name_hint"], "selection",
            "generic on* token should fall back to the `selection` field concern: {handler:?}"
        );
    }
}
