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
//! Clustering signal: field co-touch dominates. Two methods sit in the same
//! cluster iff they share at least one field (read OR write) in the
//! `method_to_field` edge set, transitively. Method-to-method call edges are
//! NOT used to merge clusters — they appear in the response as
//! `cross_cluster_calls` so the operator can see the coupling before deciding
//! to split. Methods that touch zero class fields are attached to whichever
//! cluster they call most, falling back to a singleton "cluster" so the
//! operator decides where they belong.

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

    // Union-find on methods that share at least one touched field.
    let mut parent: BTreeMap<String, String> =
        methods.iter().map(|m| (m.clone(), m.clone())).collect();
    for field in fields {
        let touching: Vec<&String> = method_fields
            .iter()
            .filter(|(_, fs)| fs.contains(field))
            .map(|(m, _)| m)
            .collect();
        if touching.len() < 2 {
            continue;
        }
        let anchor = touching[0].clone();
        for other in touching.iter().skip(1) {
            union(&mut parent, &anchor, other);
        }
    }

    // Methods that touch no fields are still singletons at this point. For
    // each, prefer to attach to the cluster of the methods it CALLS — those
    // method bodies likely belong together. Falls back to the cluster of
    // callers if the singleton has no outbound calls.
    let initial_roots: BTreeMap<String, String> = methods
        .iter()
        .map(|m| (m.clone(), find(&mut parent, m)))
        .collect();
    for m in methods {
        if !method_fields.get(m).map(|f| f.is_empty()).unwrap_or(true) {
            continue;
        }
        // Singleton root?
        let root = find(&mut parent, m);
        let same_root_count = methods
            .iter()
            .filter(|n| find(&mut parent, n) == root)
            .count();
        if same_root_count > 1 {
            continue;
        }

        // Count call edges to other clusters' roots.
        let mut tally: BTreeMap<String, usize> = BTreeMap::new();
        for (from, to) in m2m_pairs {
            if from == m {
                if let Some(other_root) = initial_roots.get(to) {
                    if other_root != &root {
                        *tally.entry(other_root.clone()).or_default() += 1;
                    }
                }
            }
            if to == m {
                if let Some(other_root) = initial_roots.get(from) {
                    if other_root != &root {
                        *tally.entry(other_root.clone()).or_default() += 1;
                    }
                }
            }
        }
        if let Some((target_root, _)) = tally
            .into_iter()
            .max_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)))
        {
            union(&mut parent, &target_root, m);
        }
    }

    // Group methods by final root.
    let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for m in methods {
        let root = find(&mut parent, m);
        groups.entry(root).or_default().push(m.clone());
    }

    // Stable ordering: by smallest line / lexicographic first-method.
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
        let name_hint = infer_name_hint(item_names);
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

// ──────────────────────────── helpers ────────────────────────────

fn find(parent: &mut BTreeMap<String, String>, item: &str) -> String {
    let current = parent
        .get(item)
        .cloned()
        .unwrap_or_else(|| item.to_string());
    if current == item {
        return current;
    }
    let root = find(parent, &current);
    parent.insert(item.to_string(), root.clone());
    root
}

fn union(parent: &mut BTreeMap<String, String>, a: &str, b: &str) {
    let root_a = find(parent, a);
    let root_b = find(parent, b);
    if root_a != root_b {
        // Deterministic: lexicographically smaller name wins as root.
        let (winner, loser) = if root_a <= root_b {
            (root_a, root_b)
        } else {
            (root_b, root_a)
        };
        parent.insert(loser, winner);
    }
}

/// Infer a class-name hint from the first camelCase token of each method.
/// Picks the most common token; ties broken lexicographically. Returns the
/// raw token (e.g. `bill`, `search`) — operator capitalizes / pluralizes
/// when accepting.
fn infer_name_hint(method_names: &[String]) -> String {
    if method_names.is_empty() {
        return String::new();
    }
    let tokens: Vec<String> = method_names
        .iter()
        .map(|n| first_camel_token(n.as_str()))
        .collect();
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for t in &tokens {
        if t.is_empty() {
            continue;
        }
        *counts.entry(t.clone()).or_default() += 1;
    }
    counts
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
        let mut p = RefactorPlanParams::default();
        p.kind = "extract_java_class_cohesive_clusters".to_string();
        p.source = source.to_string_lossy().into_owned();
        p
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
}
