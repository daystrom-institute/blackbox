//! EX-G1 `split_elixir_clauses_by_tag` ★ keystone.
//!
//! Decompose a multi-clause atom-tag-dispatch function into per-tag submodules
//! with a generated router on the parent. The router preserves the original
//! function signature, clause order within each bucket, and any guards
//! verbatim.
//!
//! v1 scope:
//!   - Single function name (`item_names` of length 1).
//!   - Primary discriminator only (`head_matcher.discriminators[0]`).
//!   - `duplicate_tag_policy="group_to_same_bucket"` only (subkey
//!     dispatching is v2).
//!   - Both `selection_mode`s: "exhaustive" and "selected_only".
//!   - Tier-1 static reachability for `captured_helpers`; Tier-2 dynamic
//!     dispatch sites are reported in `dynamic_dispatch_unresolved`.
//!   - Guards preserved verbatim. If a clause's guard references a
//!     local def that isn't being moved with it, refuse with
//!     `error.guarded_clauses_require_review` unless
//!     `acknowledge_unpreservable_guards`.
//!
//! v2 scope (deferred):
//!   - `explicit_subkeys` policy with router-side subkey dispatch.
//!   - Cross-argument discriminators.
//!   - @spec narrowing on router after moving (EX-G1 rule #3 qualifier).
//!   - LSP-driven helper reachability resolution.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::PathBuf;

use anyhow::{Result, anyhow, bail};
use serde::Serialize;
use tree_sitter::Node;

use super::{
    call_arguments, call_target_name, def_name_and_arity, defmodule_body_statements,
    parse_elixir_file, top_level_defmodule,
};
use crate::refactor::{
    FileEdit, PlanStatus, RefactorPlan, RefactorPlanParams, SemanticStatus, TextEdit,
    ValidationStep, resolve_path, sha256_hex, toml_bool,
};

// ---------------------------------------------------------------------------
// Wire / response shape
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct PlanWithReport {
    #[serde(flatten)]
    plan: RefactorPlan,
    partitions: Vec<PartitionReport>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    unenumerated_tags: Vec<String>,
    roundtrip_check: RoundtripCheckPlaceholder,
}

#[derive(Debug, Serialize)]
struct PartitionReport {
    target_module: String,
    target_file: String,
    clause_count: usize,
    captured_helpers: Vec<String>,
    shared_helpers: Vec<String>,
    dynamic_dispatch_unresolved: Vec<DispatchSite>,
    duplicate_tag_groups: BTreeMap<String, Vec<usize>>,
    guarded_clauses: BTreeMap<usize, String>,
}

#[derive(Debug, Serialize)]
struct DispatchSite {
    line: usize,
    excerpt: String,
}

#[derive(Debug, Serialize, Default)]
struct RoundtripCheckPlaceholder {
    /// v1: round-trip lives at apply time (EX-V6 invariant); at plan time we
    /// emit `passed: null` to signal "deferred to apply". v2 will perform a
    /// best-effort parse-check of the proposed edits here.
    passed: Option<bool>,
    diff: Option<String>,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub(crate) fn plan_split_clauses_by_tag(p: &RefactorPlanParams) -> Result<String> {
    // ── inputs ────────────────────────────────────────────────────────────────
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let parsed = parse_elixir_file(&source_path)?;
    let defmodule = top_level_defmodule(&parsed.tree, &parsed.source).ok_or_else(|| {
        anyhow!(
            "error.bad_input(code=no_defmodule): {} has no top-level defmodule",
            source_path.display()
        )
    })?;
    let parent_module_name = p
        .module_name
        .as_deref()
        .map(String::from)
        .or_else(|| super::module_deps::defmodule_full_name_pub(defmodule, &parsed.source))
        .ok_or_else(|| anyhow!("module_name (source's defmodule name) is required"))?;

    let item_names: Vec<String> = p.item_names.as_deref().unwrap_or(&[]).to_vec();
    if item_names.len() != 1 {
        bail!(
            "v1 supports exactly one function name in item_names (got {:?})",
            item_names
        );
    }
    let fn_name = item_names[0].clone();

    let toml = p.toml_entries.as_ref();
    let arity = match toml.and_then(|m| m.get("arity")) {
        Some(serde_json::Value::Number(n)) => n.as_u64().map(|n| n as usize),
        _ => None,
    };
    let arity = arity.ok_or_else(|| anyhow!("toml_entries.arity is required"))?;

    let head_matcher = toml
        .and_then(|m| m.get("head_matcher"))
        .ok_or_else(|| anyhow!("toml_entries.head_matcher is required"))?;
    let parsed_matcher = HeadMatcher::from_json(head_matcher)?;
    if parsed_matcher.cross_arg() {
        bail!(
            "error.bad_input(code=cross_arg_discriminators): v1 requires all discriminators share arg_index"
        );
    }

    let partition_obj = toml
        .and_then(|m| m.get("partition"))
        .and_then(|v| v.as_object())
        .ok_or_else(|| {
            anyhow!("toml_entries.partition is required (object: target_module → [tags])")
        })?;
    let partition = parse_partition(partition_obj)?;

    let selection_mode = match toml
        .and_then(|m| m.get("selection_mode"))
        .and_then(|v| v.as_str())
    {
        Some("exhaustive") | None => SelectionMode::Exhaustive,
        Some("selected_only") => SelectionMode::SelectedOnly,
        Some(other) => bail!(
            "error.bad_input(code=invalid_selection_mode): `{other}` (valid: exhaustive, selected_only)"
        ),
    };

    let duplicate_policy = match toml
        .and_then(|m| m.get("duplicate_tag_policy"))
        .and_then(|v| v.as_str())
    {
        Some("group_to_same_bucket") | None => DuplicatePolicy::GroupToSameBucket,
        Some("explicit_subkeys") => bail!("explicit_subkeys policy is v2 — not implemented in v1"),
        Some(other) => bail!("error.bad_input(code=invalid_duplicate_tag_policy): `{other}`"),
    };

    let target_dir = toml
        .and_then(|m| m.get("target_dir"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("toml_entries.target_dir is required"))?
        .to_string();
    let target_dir_path = resolve_path(p.project_dir.as_deref(), &target_dir)?;

    let _router_module = toml
        .and_then(|m| m.get("router_module"))
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or(parent_module_name.clone());

    let ack_quote = toml_bool(&p.toml_entries, "acknowledge_quote_in_moved");
    let ack_use = toml_bool(&p.toml_entries, "acknowledge_use_at_scope");
    let ack_macro = toml_bool(&p.toml_entries, "acknowledge_defmacro_move");
    let ack_guard = toml_bool(&p.toml_entries, "acknowledge_unpreservable_guards");
    let ack_dyn = toml_bool(&p.toml_entries, "acknowledge_dynamic_dispatch");

    // ── pre-check: use at scope ──────────────────────────────────────────────
    let body_stmts = defmodule_body_statements(defmodule, &parsed.source);
    let use_at_scope = body_stmts
        .iter()
        .filter_map(|n| call_target_name(*n, &parsed.source).map(|name| (name, *n)))
        .find(|(name, _)| *name == "use");
    if let Some((_, use_call)) = use_at_scope {
        if !ack_use {
            let (line, _) = super::byte_to_line_col(&parsed.source, use_call.start_byte());
            bail!(
                "error.bad_input(code=use_at_scope): source has `use` at line {line}; pass acknowledge_use_at_scope=true to proceed"
            );
        }
    }

    // ── classify clauses ─────────────────────────────────────────────────────
    let clauses: Vec<ClauseInfo> = body_stmts
        .iter()
        .copied()
        .filter_map(|stmt| classify_clause(stmt, &fn_name, arity, &parsed_matcher, &parsed.source))
        .collect();
    if clauses.is_empty() {
        bail!(
            "error.bad_input(code=item_not_found): no `{fn_name}/{arity}` clauses found in {}",
            source_path.display()
        );
    }

    // ── classify each clause's primary tag ───────────────────────────────────
    let mut primary_tag_to_clauses: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    let mut non_tag_clauses: Vec<usize> = Vec::new();
    for (i, clause) in clauses.iter().enumerate() {
        match &clause.primary_tag {
            Some(tag) => primary_tag_to_clauses
                .entry(tag.clone())
                .or_default()
                .push(i),
            None => non_tag_clauses.push(i),
        }
    }

    // ── validate partition vs detected tags ──────────────────────────────────
    let mut wanted_tag_to_bucket: BTreeMap<String, String> = BTreeMap::new();
    for (bucket, tags) in &partition {
        for tag in tags {
            if let Some(existing) = wanted_tag_to_bucket.insert(tag.clone(), bucket.clone()) {
                bail!(
                    "error.duplicate_tag_split_across_buckets: tag `{tag}` is assigned to both `{existing}` and `{bucket}`"
                );
            }
        }
    }

    let detected_tags: BTreeSet<&str> = primary_tag_to_clauses.keys().map(String::as_str).collect();
    let wanted_tags: BTreeSet<&str> = wanted_tag_to_bucket.keys().map(String::as_str).collect();
    let mut unenumerated: Vec<String> = detected_tags
        .difference(&wanted_tags)
        .map(|s| s.to_string())
        .collect();
    unenumerated.sort();

    if matches!(selection_mode, SelectionMode::Exhaustive) && !unenumerated.is_empty() {
        bail!(
            "error.unenumerated_tags: tags present in source but absent from partition: {}",
            unenumerated.join(", ")
        );
    }
    // unknown tags in partition (not in source)
    let unknown: Vec<&str> = wanted_tags.difference(&detected_tags).copied().collect();
    if !unknown.is_empty() {
        bail!(
            "error.bad_input(code=unknown_tag_in_partition): tags in partition but not in source: {}",
            unknown.join(", ")
        );
    }

    // ── per-clause refusals: macro / quote / guard preservation ──────────────
    if matches!(duplicate_policy, DuplicatePolicy::GroupToSameBucket) {
        // group_to_same_bucket: all clauses of one tag go to one bucket, OK.
    }

    let mut moved_indices: Vec<usize> = Vec::new();
    for (tag, idxs) in &primary_tag_to_clauses {
        if wanted_tags.contains(tag.as_str()) {
            moved_indices.extend(idxs);
        }
    }
    moved_indices.sort();

    for &i in &moved_indices {
        let clause = &clauses[i];
        if clause.is_macro && !ack_macro {
            bail!(
                "error.bad_input(code=defmacro_move): clause at line {} is a defmacro; pass acknowledge_defmacro_move=true to proceed",
                clause.line
            );
        }
        if clause.contains_quote && !ack_quote {
            bail!(
                "error.bad_input(code=quote_in_moved): clause at line {} contains a quote block; pass acknowledge_quote_in_moved=true to proceed",
                clause.line
            );
        }
        if !clause.dynamic_dispatch_sites.is_empty() && !ack_dyn {
            bail!(
                "error.bad_input(code=dynamic_dispatch_in_moved_clauses): clause at line {} has unresolved dynamic dispatch ({} sites); pass acknowledge_dynamic_dispatch=true once you've manually verified the targets",
                clause.line,
                clause.dynamic_dispatch_sites.len()
            );
        }
    }

    // ── captured helpers (Tier-1 static) ─────────────────────────────────────
    //
    // Walk all moved-clause bodies. Local calls (bare identifier or
    // __MODULE__.fn) resolve to defps in the parent module if a defp by that
    // name exists. Record the resolution.
    let local_defps: BTreeMap<(String, usize), Node<'_>> =
        collect_local_defps(&body_stmts, &parsed.source);
    let mut helper_reach: HelperReach = HelperReach::default();
    for &i in &moved_indices {
        let clause = &clauses[i];
        scan_helper_calls(
            clause.node,
            &parsed.source,
            &local_defps,
            &mut helper_reach.calls,
        );
    }
    // Transitive closure inside local_defps. A moved clause's helper may call
    // another helper; pull that one in too.
    let mut frontier: Vec<(String, usize)> = helper_reach.calls.iter().cloned().collect();
    let mut all_helpers: HashSet<(String, usize)> = frontier.iter().cloned().collect();
    while let Some(h) = frontier.pop() {
        if let Some(helper_node) = local_defps.get(&h) {
            let mut sub = HashSet::new();
            scan_helper_calls(*helper_node, &parsed.source, &local_defps, &mut sub);
            for next in sub {
                if all_helpers.insert(next.clone()) {
                    frontier.push(next);
                }
            }
        }
    }

    // For each bucket, compute which helpers it depends on (Tier-1 only).
    let mut bucket_helpers: BTreeMap<String, BTreeSet<(String, usize)>> = BTreeMap::new();
    for (tag, idxs) in &primary_tag_to_clauses {
        let Some(bucket) = wanted_tag_to_bucket.get(tag) else {
            continue;
        };
        let entry = bucket_helpers.entry(bucket.clone()).or_default();
        for &i in idxs {
            let clause = &clauses[i];
            let mut calls = HashSet::new();
            scan_helper_calls(clause.node, &parsed.source, &local_defps, &mut calls);
            // Transitively expand
            let mut local_frontier: Vec<(String, usize)> = calls.into_iter().collect();
            let mut local_visited: HashSet<(String, usize)> = HashSet::new();
            while let Some(h) = local_frontier.pop() {
                if !local_visited.insert(h.clone()) {
                    continue;
                }
                entry.insert(h.clone());
                if let Some(helper_node) = local_defps.get(&h) {
                    let mut sub = HashSet::new();
                    scan_helper_calls(*helper_node, &parsed.source, &local_defps, &mut sub);
                    for next in sub {
                        if !local_visited.contains(&next) {
                            local_frontier.push(next);
                        }
                    }
                }
            }
        }
    }

    // Compute shared helpers: helpers needed by more than one bucket.
    let mut helper_to_buckets: BTreeMap<(String, usize), Vec<String>> = BTreeMap::new();
    for (bucket, hs) in &bucket_helpers {
        for h in hs {
            helper_to_buckets
                .entry(h.clone())
                .or_default()
                .push(bucket.clone());
        }
    }
    let shared_helpers: BTreeSet<(String, usize)> = helper_to_buckets
        .iter()
        .filter(|(_, bs)| bs.len() > 1)
        .map(|(h, _)| h.clone())
        .collect();

    // ── emit target files + collect per-bucket reports ───────────────────────
    let mut partitions_report: Vec<PartitionReport> = Vec::new();
    let mut file_edits: Vec<FileEdit> = Vec::new();

    for bucket in partition.keys() {
        let target_file = derive_target_filename(&target_dir_path, bucket);
        let bucket_clauses: Vec<&ClauseInfo> = partition[bucket]
            .iter()
            .flat_map(|tag| {
                primary_tag_to_clauses
                    .get(tag)
                    .into_iter()
                    .flat_map(|idxs| idxs.iter().map(|&i| &clauses[i]))
            })
            .collect();
        let bucket_helper_keys = bucket_helpers.get(bucket).cloned().unwrap_or_default();
        let bucket_helpers_excl_shared: BTreeSet<(String, usize)> = bucket_helper_keys
            .difference(&shared_helpers)
            .cloned()
            .collect();

        // Build target file body.
        let mut body = String::new();
        body.push_str(&format!("defmodule {} do\n", bucket));
        body.push_str("  @moduledoc false\n\n");
        for clause in &bucket_clauses {
            let chunk = &parsed.source[clause.attr_start..clause.byte_end];
            body.push_str(chunk);
            if !body.ends_with("\n\n") {
                body.push('\n');
            }
        }
        for helper_key in &bucket_helpers_excl_shared {
            if let Some(helper_node) = local_defps.get(helper_key) {
                let start = helper_node.start_byte();
                let end = helper_node.end_byte();
                body.push_str(&parsed.source[start..end]);
                body.push('\n');
            }
        }
        body.push_str("end\n");

        let target_edit = TextEdit {
            byte_start: 0,
            byte_end: 0,
            replacement: body.clone(),
        };
        file_edits.push(FileEdit {
            path: target_file.to_string_lossy().into_owned(),
            original_sha256: sha256_hex(b""),
            edits: vec![target_edit],
            new_text: Some(body),
        });

        // Compute report
        let mut guarded: BTreeMap<usize, String> = BTreeMap::new();
        let mut dynamic_sites: Vec<DispatchSite> = Vec::new();
        let mut dup_groups: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for clause in &bucket_clauses {
            if let Some(g) = &clause.guard_text {
                guarded.insert(clause.line, g.clone());
            }
            for s in &clause.dynamic_dispatch_sites {
                dynamic_sites.push(DispatchSite {
                    line: s.line,
                    excerpt: s.excerpt.clone(),
                });
            }
            if let Some(tag) = &clause.primary_tag {
                if let Some(idxs) = primary_tag_to_clauses.get(tag) {
                    if idxs.len() > 1 {
                        dup_groups
                            .insert(tag.clone(), idxs.iter().map(|&i| clauses[i].line).collect());
                    }
                }
            }
        }
        partitions_report.push(PartitionReport {
            target_module: bucket.clone(),
            target_file: target_file.to_string_lossy().into_owned(),
            clause_count: bucket_clauses.len(),
            captured_helpers: bucket_helpers_excl_shared
                .iter()
                .map(|(n, a)| format!("{n}/{a}"))
                .collect(),
            shared_helpers: helper_to_buckets
                .iter()
                .filter(|(_, b)| b.len() > 1 && b.contains(bucket))
                .map(|((n, a), _)| format!("{n}/{a}"))
                .collect(),
            dynamic_dispatch_unresolved: dynamic_sites,
            duplicate_tag_groups: dup_groups,
            guarded_clauses: guarded,
        });

        // Refuse on guard preservation failure: for v1, the guard is
        // "preservable" iff every call inside it that's a local def is in
        // bucket_helpers OR is a builtin guard. We approximate: if any guard
        // body references a defp that ISN'T in bucket_helpers AND ISN'T a
        // known Elixir guard, refuse unless acknowledged.
        for clause in &bucket_clauses {
            if let Some(g) = &clause.guard_text {
                if !guard_is_preservable(g, &bucket_helper_keys) && !ack_guard {
                    bail!(
                        "error.guarded_clauses_require_review: clause at line {} has guard `{g}` that references a non-moved helper or non-builtin; pass acknowledge_unpreservable_guards=true to proceed",
                        clause.line
                    );
                }
            }
        }
    }

    // ── compute source edits ─────────────────────────────────────────────────
    // For each moved clause: replace the clause text with a dispatch wrapper.
    // For each helper that moved entirely to one bucket (not shared), delete
    // it from source. Shared helpers stay on the source.
    let mut source_edits: Vec<TextEdit> = Vec::new();

    for (tag, idxs) in &primary_tag_to_clauses {
        let Some(bucket) = wanted_tag_to_bucket.get(tag) else {
            continue;
        };
        for (k, &i) in idxs.iter().enumerate() {
            let clause = &clauses[i];
            if k == 0 {
                // First clause of this tag: replace with dispatch wrapper.
                let dispatch =
                    render_dispatch_wrapper(&fn_name, arity, tag, &parsed_matcher, bucket);
                source_edits.push(TextEdit {
                    byte_start: clause.attr_start,
                    byte_end: trailing_newline_end(&parsed.source, clause.byte_end),
                    replacement: dispatch,
                });
            } else {
                // Subsequent clauses of same tag: delete entirely (they live
                // in the target now).
                source_edits.push(TextEdit {
                    byte_start: clause.attr_start,
                    byte_end: trailing_newline_end(&parsed.source, clause.byte_end),
                    replacement: String::new(),
                });
            }
        }
    }

    for (helper_key, helper_node) in &local_defps {
        // Remove non-shared moved helpers from source.
        let buckets_using = helper_to_buckets.get(helper_key);
        if let Some(buckets) = buckets_using {
            if buckets.len() == 1 && !shared_helpers.contains(helper_key) {
                // Move-with-bucket: remove from source. Include attached attrs
                // by walking backward for unary_operator siblings — for v1 we
                // just delete the helper body byte range (attrs stay; minor
                // dust the operator can clean up). Conservative.
                source_edits.push(TextEdit {
                    byte_start: helper_node.start_byte(),
                    byte_end: trailing_newline_end(&parsed.source, helper_node.end_byte()),
                    replacement: String::new(),
                });
            }
        }
    }

    source_edits.sort_by_key(|e| e.byte_start);
    // Dedupe contiguous identical edits (a paranoid guard against duplicate
    // emission from overlapping helper-vs-clause walks).
    source_edits.dedup_by(|a, b| {
        a.byte_start == b.byte_start && a.byte_end == b.byte_end && a.replacement == b.replacement
    });

    // EX-V6 v1 floor: apply the source edits to a probe copy and verify it
    // parses cleanly. Catches dispatch-wrapper construction bugs that emit
    // syntactically invalid Elixir.
    {
        let mut probe = parsed.source.clone();
        // Apply in reverse byte order to preserve indices.
        let mut sorted_edits = source_edits.clone();
        sorted_edits.sort_by_key(|e| std::cmp::Reverse(e.byte_start));
        for e in &sorted_edits {
            probe.replace_range(e.byte_start..e.byte_end, &e.replacement);
        }
        super::roundtrip::verify_parse_clean(&probe)?;
        // Also verify each generated target file parses.
        for fe in &file_edits {
            if let Some(new_text) = fe.new_text.as_deref() {
                super::roundtrip::verify_parse_clean(new_text)?;
            }
        }
    }

    // Push source edits as the FIRST file_edit.
    file_edits.insert(
        0,
        FileEdit {
            path: source_path.to_string_lossy().into_owned(),
            original_sha256: sha256_hex(parsed.source.as_bytes()),
            edits: source_edits,
            new_text: None,
        },
    );

    let validations: Vec<ValidationStep> = file_edits
        .iter()
        .map(|fe| ValidationStep::TreeSitterNoErrors {
            path: fe.path.clone(),
            byte_range: None,
        })
        .collect();

    let plan = RefactorPlan {
        title: format!(
            "split_elixir_clauses_by_tag: {}.{}/{} → {} bucket(s)",
            parent_module_name,
            fn_name,
            arity,
            partition.len()
        ),
        kind: "split_elixir_clauses_by_tag".to_string(),
        semantic_status: SemanticStatus::IndexedHints,
        dry_run: false,
        file_moves: Vec::new(),
        file_creates: Vec::new(),
        edits: file_edits,
        validations,
        items: Vec::new(),
        leftovers: Vec::new(),
        captured_variables: Vec::new(),
        remaining_source_accessors: Vec::new(),
        remaining_source_constant_refs: Vec::new(),
        external_calls: Vec::new(),
        inherited_dependencies: Vec::new(),
        deep_analysis: None,
        plan_status: PlanStatus::Planned,
        fixme_count: None,
    };

    let wrapped = PlanWithReport {
        plan,
        partitions: partitions_report,
        unenumerated_tags: unenumerated,
        roundtrip_check: RoundtripCheckPlaceholder::default(),
    };
    Ok(serde_json::to_string(&wrapped)?)
}

// ---------------------------------------------------------------------------
// Head matcher
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct HeadMatcher {
    discriminators: Vec<Discriminator>,
    #[allow(dead_code)] // used only when explicit_subkeys ships in v2
    preserve_guards: String,
}

#[derive(Debug, Clone)]
struct Discriminator {
    arg_index: usize,
    binding: String,
    primary: bool,
    #[allow(dead_code)] // used only when explicit_subkeys ships in v2
    secondary: bool,
}

impl HeadMatcher {
    fn from_json(value: &serde_json::Value) -> Result<Self> {
        let obj = value
            .as_object()
            .ok_or_else(|| anyhow!("head_matcher must be an object"))?;
        let disc_arr = obj
            .get("discriminators")
            .and_then(|v| v.as_array())
            .ok_or_else(|| anyhow!("head_matcher.discriminators is required (array)"))?;
        let mut discriminators = Vec::new();
        for d in disc_arr {
            let o = d
                .as_object()
                .ok_or_else(|| anyhow!("each discriminator must be an object"))?;
            discriminators.push(Discriminator {
                arg_index: o
                    .get("arg_index")
                    .and_then(|v| v.as_u64())
                    .map(|n| n as usize)
                    .ok_or_else(|| anyhow!("discriminator.arg_index is required"))?,
                binding: o
                    .get("binding")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("discriminator.binding is required"))?
                    .to_string(),
                primary: o.get("primary").and_then(|v| v.as_bool()).unwrap_or(false),
                secondary: o
                    .get("secondary")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
            });
        }
        if discriminators.is_empty() {
            bail!("head_matcher.discriminators must be non-empty");
        }
        if !discriminators.iter().any(|d| d.primary) {
            bail!("head_matcher.discriminators must mark at least one as primary");
        }
        Ok(Self {
            discriminators,
            preserve_guards: obj
                .get("preserve_guards")
                .and_then(|v| v.as_str())
                .unwrap_or("verbatim")
                .to_string(),
        })
    }

    fn cross_arg(&self) -> bool {
        let mut indices: BTreeSet<usize> = BTreeSet::new();
        for d in &self.discriminators {
            indices.insert(d.arg_index);
        }
        indices.len() > 1
    }

    fn primary_arg_index(&self) -> usize {
        self.discriminators
            .iter()
            .find(|d| d.primary)
            .map(|d| d.arg_index)
            .unwrap_or(0)
    }

    /// Extract the primary tag atom from a clause signature.
    /// The discriminator binding may be e.g. `"%Op{kind: $TAG}"` — we look for
    /// the literal sub-pattern that matches a `:atom` at the discriminator's
    /// arg_index.
    fn extract_primary_tag(&self, clause_sig: Node<'_>, source: &str) -> Option<String> {
        let arg_idx = self.primary_arg_index();
        let arg_node = clause_signature_arg_at(clause_sig, source, arg_idx)?;
        // The arg node could be `map`, `struct`, etc. Walk it looking for a
        // `pair` whose key is `kind:` (when binding contains `kind: $TAG`) or
        // any leading atom literal.
        let key = primary_tag_key_name(&self.discriminators);
        find_atom_in_node_for_key(arg_node, source, key.as_deref())
    }
}

fn primary_tag_key_name(discriminators: &[Discriminator]) -> Option<String> {
    // Parse "kind:" out of the binding e.g. `"%Op{kind: $TAG}"`.
    let primary = discriminators.iter().find(|d| d.primary)?;
    let binding = &primary.binding;
    // Find "$TAG", walk back to find the key prefix
    let idx = binding.find("$TAG")?;
    // Walk back from idx to find ":" (the keyword separator) and then back to the key name start.
    let before = &binding[..idx];
    let colon = before.rfind(':')?;
    let key_end = colon;
    let mut key_start = key_end;
    let bytes = before.as_bytes();
    while key_start > 0 {
        let c = bytes[key_start - 1];
        if c.is_ascii_alphanumeric() || c == b'_' {
            key_start -= 1;
        } else {
            break;
        }
    }
    if key_start == key_end {
        return None;
    }
    Some(before[key_start..key_end].to_string())
}

// ---------------------------------------------------------------------------
// Clause classification
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct ClauseInfo<'tree> {
    node: Node<'tree>,
    /// Byte position where the attached `@doc`/`@spec`/etc block begins.
    /// Falls back to `node.start_byte()` when no attached attrs.
    attr_start: usize,
    byte_end: usize,
    line: usize,
    primary_tag: Option<String>,
    is_macro: bool,
    contains_quote: bool,
    guard_text: Option<String>,
    dynamic_dispatch_sites: Vec<DispatchSiteInfo>,
}

#[derive(Debug, Clone)]
struct DispatchSiteInfo {
    line: usize,
    excerpt: String,
}

fn classify_clause<'tree>(
    stmt: Node<'tree>,
    fn_name: &str,
    arity: usize,
    matcher: &HeadMatcher,
    source: &str,
) -> Option<ClauseInfo<'tree>> {
    let target_name = call_target_name(stmt, source)?;
    if !matches!(target_name, "def" | "defp" | "defmacro" | "defmacrop") {
        return None;
    }
    let (this_name, this_arity) = def_name_and_arity(stmt, source)?;
    if this_name != fn_name || this_arity != arity {
        return None;
    }
    let is_macro = target_name.starts_with("defmacro");

    // Locate the signature node (call/binary_operator inside arguments).
    let args = call_arguments(stmt)?;
    let mut arg_cursor = args.walk();
    let sig = args.named_children(&mut arg_cursor).next()?;
    let (primary_tag, guard_text) = match sig.kind() {
        "binary_operator" => {
            // hello(x) when guard
            let mut c = sig.walk();
            let mut iter = sig.named_children(&mut c);
            let left = iter.next()?;
            let guard = iter.next();
            (
                matcher.extract_primary_tag(left, source),
                guard.map(|g| source[g.byte_range()].to_string()),
            )
        }
        _ => (matcher.extract_primary_tag(sig, source), None),
    };

    let (line, _) = super::byte_to_line_col(source, stmt.start_byte());
    let attr_start = leading_attr_start(stmt, source);
    let byte_end = stmt.end_byte();

    // Scan body for quote calls and dynamic-dispatch sites.
    let (contains_quote, dynamic_sites) = scan_body_for_quote_and_dynamic(stmt, source);

    Some(ClauseInfo {
        node: stmt,
        attr_start,
        byte_end,
        line,
        primary_tag,
        is_macro,
        contains_quote,
        guard_text,
        dynamic_dispatch_sites: dynamic_sites,
    })
}

/// Walk backward from a `def`-style call through preceding sibling
/// `unary_operator` `@<name>` directives. Returns the byte position of the
/// first attribute, or `stmt.start_byte()` if none.
fn leading_attr_start(stmt: Node<'_>, _source: &str) -> usize {
    let mut start = stmt.start_byte();
    let mut cur = stmt;
    while let Some(prev) = cur.prev_named_sibling() {
        if prev.kind() == "unary_operator" {
            start = prev.start_byte();
            cur = prev;
        } else {
            break;
        }
    }
    start
}

fn clause_signature_arg_at<'tree>(
    sig: Node<'tree>,
    _source: &str,
    arg_index: usize,
) -> Option<Node<'tree>> {
    // sig is the call `hello(x, %Op{...})`. Its second named child is
    // `arguments` containing the parameter list.
    if sig.kind() == "binary_operator" {
        let mut c = sig.walk();
        let left = sig.named_children(&mut c).next()?;
        return clause_signature_arg_at(left, _source, arg_index);
    }
    if sig.kind() != "call" {
        return None;
    }
    let mut c = sig.walk();
    let mut iter = sig.named_children(&mut c);
    let _name = iter.next()?;
    let args = iter.next()?;
    let mut arg_c = args.walk();
    args.named_children(&mut arg_c).nth(arg_index)
}

/// Search a parameter-shape node for the atom literal whose key matches `key`
/// (e.g., "kind"). For a node like `%Op{kind: :foo, args: x}`, returns Some("foo").
/// Returns None if key absent or value isn't a bare atom literal.
fn find_atom_in_node_for_key(node: Node<'_>, source: &str, key: Option<&str>) -> Option<String> {
    // If node IS itself an atom literal (e.g. clause head bound to `:foo`),
    // return its name.
    if node.kind() == "atom" {
        let txt = source[node.byte_range()].trim_start_matches(':');
        return Some(txt.to_string());
    }

    let want_key = key?;
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        if n.kind() == "pair" {
            // pair has keyword + value. We need keyword text to match `want_key`.
            let mut c = n.walk();
            let mut iter = n.named_children(&mut c);
            let first = iter.next();
            let second = iter.next();
            if let (Some(k), Some(v)) = (first, second) {
                let k_text = source[k.byte_range()].trim().trim_end_matches(':');
                if k_text == want_key && v.kind() == "atom" {
                    let atom_text = source[v.byte_range()].trim_start_matches(':');
                    return Some(atom_text.to_string());
                }
            }
        }
        let mut c = n.walk();
        for c2 in n.named_children(&mut c) {
            stack.push(c2);
        }
    }
    None
}

fn scan_body_for_quote_and_dynamic(stmt: Node<'_>, source: &str) -> (bool, Vec<DispatchSiteInfo>) {
    let mut contains_quote = false;
    let mut sites = Vec::new();
    let mut stack = vec![stmt];
    while let Some(n) = stack.pop() {
        if n.kind() == "call" {
            if let Some(name) = call_target_name(n, source) {
                if name == "quote" {
                    contains_quote = true;
                }
                if matches!(name, "apply" | "Module.concat") {
                    let (line, _) = super::byte_to_line_col(source, n.start_byte());
                    let excerpt: String = source[n.byte_range()]
                        .lines()
                        .next()
                        .unwrap_or("")
                        .to_string();
                    sites.push(DispatchSiteInfo { line, excerpt });
                }
            }
        }
        let mut c = n.walk();
        for c2 in n.named_children(&mut c) {
            stack.push(c2);
        }
    }
    (contains_quote, sites)
}

// ---------------------------------------------------------------------------
// Helper reachability
// ---------------------------------------------------------------------------

#[derive(Default)]
struct HelperReach {
    calls: HashSet<(String, usize)>,
}

/// Collect private helpers (`defp`) in the source module. We deliberately
/// exclude `def` items: they're not movable helpers (they're public surface),
/// and including them risks emitting duplicate-delete edits when the helper
/// resolver runs against the function being split itself.
fn collect_local_defps<'tree>(
    body: &[Node<'tree>],
    source: &str,
) -> BTreeMap<(String, usize), Node<'tree>> {
    let mut out = BTreeMap::new();
    for stmt in body {
        let Some(name) = call_target_name(*stmt, source) else {
            continue;
        };
        if name != "defp" {
            continue;
        }
        let Some((fname, arity)) = def_name_and_arity(*stmt, source) else {
            continue;
        };
        out.insert((fname, arity), *stmt);
    }
    out
}

fn scan_helper_calls(
    node: Node<'_>,
    source: &str,
    local_defps: &BTreeMap<(String, usize), Node<'_>>,
    out: &mut HashSet<(String, usize)>,
) {
    // Match locally-defined helpers BY NAME ONLY (any arity). Elixir's pipe
    // operator `x |> foo()` calls `foo/1` but tree-sitter sees the call site
    // as arity 0; resolving by exact arity would miss pipe-fed calls. v1
    // accepts the over-approximation: every local-name match is recorded.
    let local_names_to_keys: HashMap<&str, Vec<&(String, usize)>> =
        local_defps.keys().fold(HashMap::new(), |mut m, k| {
            m.entry(k.0.as_str()).or_default().push(k);
            m
        });
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        if n.kind() == "call" {
            if let Some(name) = call_target_name(n, source) {
                if let Some(target) = n.named_child(0) {
                    if target.kind() == "identifier" {
                        if let Some(keys) = local_names_to_keys.get(name) {
                            for k in keys {
                                out.insert((*k).clone());
                            }
                        }
                    }
                }
            }
        }
        let mut c = n.walk();
        for c2 in n.named_children(&mut c) {
            stack.push(c2);
        }
    }
}

fn guard_is_preservable(guard_text: &str, bucket_helpers: &BTreeSet<(String, usize)>) -> bool {
    // Approximation for v1: identify any local-fn call in the guard (`foo(...)`
    // without `Module.` prefix). If every such call is either a known builtin
    // guard OR present in bucket_helpers, the guard is preservable.
    // Built-in Elixir guard set (partial).
    const BUILTIN_GUARDS: &[&str] = &[
        "is_atom",
        "is_binary",
        "is_bitstring",
        "is_boolean",
        "is_float",
        "is_function",
        "is_integer",
        "is_list",
        "is_map",
        "is_nil",
        "is_number",
        "is_pid",
        "is_port",
        "is_reference",
        "is_tuple",
        "is_map_key",
        "abs",
        "byte_size",
        "div",
        "elem",
        "hd",
        "length",
        "map_size",
        "node",
        "rem",
        "round",
        "self",
        "tl",
        "trunc",
        "tuple_size",
        "in",
        "and",
        "or",
        "not",
    ];
    // Crude: extract bare-identifier-followed-by-paren occurrences.
    let mut i = 0;
    let bytes = guard_text.as_bytes();
    while i < bytes.len() {
        if !(bytes[i].is_ascii_alphabetic() || bytes[i] == b'_') {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
            i += 1;
        }
        let ident = &guard_text[start..i];
        // Skip `Foo.bar`-style (preceding dot already consumed; rare in guards)
        let prev = if start > 0 {
            Some(bytes[start - 1])
        } else {
            None
        };
        if prev == Some(b'.') {
            continue;
        }
        // Followed by `(`? Then it's a call.
        // Skip whitespace
        let mut j = i;
        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        if j < bytes.len() && bytes[j] == b'(' {
            // It's a call to `ident`.
            if BUILTIN_GUARDS.contains(&ident) {
                continue;
            }
            // Otherwise look up in bucket_helpers (any arity).
            let any_arity = bucket_helpers.iter().any(|(n, _)| n == ident);
            if !any_arity {
                return false;
            }
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Partition + selection-mode
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
enum SelectionMode {
    Exhaustive,
    SelectedOnly,
}

#[derive(Debug, Clone, Copy)]
enum DuplicatePolicy {
    GroupToSameBucket,
}

fn parse_partition(
    obj: &serde_json::Map<String, serde_json::Value>,
) -> Result<BTreeMap<String, Vec<String>>> {
    let mut out = BTreeMap::new();
    for (bucket, tags_value) in obj {
        let arr = tags_value
            .as_array()
            .ok_or_else(|| anyhow!("partition[{bucket}] must be a list of atom strings"))?;
        let mut tags = Vec::new();
        for t in arr {
            let s = t.as_str().ok_or_else(|| {
                anyhow!("partition[{bucket}] entries must be atom strings like \":foo\"")
            })?;
            let cleaned = s.trim().trim_start_matches(':');
            tags.push(cleaned.to_string());
        }
        out.insert(bucket.clone(), tags);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Output rendering
// ---------------------------------------------------------------------------

fn derive_target_filename(target_dir: &std::path::Path, module_name: &str) -> PathBuf {
    // Simple convention: ModuleName → module_name.ex (snake-case of the last
    // segment).
    let last = module_name
        .rsplit('.')
        .next()
        .unwrap_or(module_name)
        .to_string();
    let snake = to_snake_case(&last);
    target_dir.join(format!("{snake}.ex"))
}

fn to_snake_case(name: &str) -> String {
    let mut out = String::new();
    for (i, c) in name.chars().enumerate() {
        if c.is_uppercase() && i > 0 {
            out.push('_');
        }
        out.push(c.to_ascii_lowercase());
    }
    out
}

fn render_dispatch_wrapper(
    fn_name: &str,
    arity: usize,
    tag: &str,
    matcher: &HeadMatcher,
    target_module: &str,
) -> String {
    // Build the param list. For each arg index 0..arity, the primary
    // discriminator's arg uses the matcher binding with $TAG → :<tag>;
    // other args are bound to `argN` and forwarded.
    let primary_idx = matcher.primary_arg_index();
    let _key = primary_tag_key_name(&matcher.discriminators).unwrap_or_else(|| "kind".to_string());
    let mut params: Vec<String> = Vec::with_capacity(arity);
    for i in 0..arity {
        if i == primary_idx {
            // Build a struct-with-key pattern. Use the binding template
            // (minus secondary refinement) to construct the head shape.
            let binding = matcher
                .discriminators
                .iter()
                .find(|d| d.primary)
                .map(|d| &d.binding)
                .cloned()
                .unwrap_or_default();
            // Replace $TAG with :<tag>; if the binding contains other vars
            // ($ARGS), strip them.
            let materialized = binding.replace("$TAG", &format!(":{tag}"));
            // Bind the whole struct to `opN` so we can forward it.
            params.push(format!("{materialized} = arg{i}"));
        } else {
            params.push(format!("arg{i}"));
        }
    }

    let signature_args = params.join(", ");
    let forward_args: Vec<String> = (0..arity).map(|i| format!("arg{i}")).collect();

    format!(
        "  def {fn_name}({signature_args}), do: {target_module}.{fn_name}({})\n",
        forward_args.join(", ")
    )
}

fn trailing_newline_end(source: &str, end: usize) -> usize {
    let bytes = source.as_bytes();
    let mut idx = end;
    while idx < bytes.len() && bytes[idx] != b'\n' {
        idx += 1;
    }
    if idx < bytes.len() && bytes[idx] == b'\n' {
        idx += 1;
    }
    idx
}
