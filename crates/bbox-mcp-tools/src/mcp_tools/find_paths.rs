use std::collections::{BTreeMap, HashSet, VecDeque};

use anyhow::Result;
use rmcp::schemars;
use serde::Deserialize;
use serde_json::json;

use crate::mcp_tools::inspect::compact_label;
use crate::path_cache::{CachedPath, PROCESS_SESSION_KEY, PathCache, PathDirection, PathStep};
use bbox_corpus_core::entity_ref::{EntityRef, EntityType};
use bbox_edge_index::edge_index::{Edge, EdgeIndex};
use bbox_project_graph::EvidenceEndpointStatus;
use bbox_providers::providers::ProviderContext;

const RENDERED_TEXT_CAP_BYTES: usize = 30 * 1024;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FindPathsParams {
    pub from: String,
    /// Project graph visibility policy: published, own, or all.
    #[serde(default)]
    pub provisional: Option<String>,
    pub to: Option<String>,
    pub to_type: Option<String>,
    pub edge_types: Option<EdgeTypesParam>,
    pub max_depth: Option<usize>,
    pub limit: Option<usize>,
    /// Per-hop fan-out cap: the maximum edges enumerated out of any single
    /// vertex (default 16, range 1..=64). A vertex whose neighborhood exceeds
    /// the cap is expanded to the cap only, and the response says so
    /// explicitly under `truncated_expansions` rather than silently returning
    /// a prefix of the neighborhood.
    #[serde(default)]
    pub max_fanout: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum EdgeTypesParam {
    One(String),
    Many(Vec<String>),
}

struct QueueEntry {
    current: EntityRef,
    steps: Vec<PathStep>,
    visited: HashSet<EntityRef>,
}

pub fn find_paths(
    p: &FindPathsParams,
    ctx: &ProviderContext<'_>,
    edge_index: &EdgeIndex,
    cache: &mut PathCache,
) -> Result<String> {
    let mut from = match EntityRef::parse(&p.from) {
        Ok(from) => from,
        Err(err) => return Ok(bad_input("from", err.to_string())),
    };
    let mut to = match p.to.as_deref().map(EntityRef::parse).transpose() {
        Ok(to) => to,
        Err(err) => return Ok(bad_input("to", err.to_string())),
    };
    from = canonical_graph_ref(ctx, from)?;
    if let Some(target) = to.take() {
        to = Some(canonical_graph_ref(ctx, target)?);
    }
    let to_type = if let Some(raw) = p.to_type.as_deref() {
        match EntityType::from_prefix(raw) {
            Some(to_type) => Some(TargetTypeFilter::new(to_type, ctx.provisional_mode())),
            None => return Ok(bad_input("to_type", "unknown entity type")),
        }
    } else {
        None
    };
    // A traversal with no target has no acceptance test, so every candidate
    // path is rejected and the caller reads "No paths found" as evidence of
    // an empty neighborhood rather than of a malformed call (gap-e41499a9).
    // Refuse loudly instead.
    if to.is_none() && to_type.is_none() {
        return Ok(missing_target_bad_input());
    }
    let max_depth = p.max_depth.unwrap_or(3);
    if !(1..=5).contains(&max_depth) {
        return Ok(bad_input("max_depth", "max_depth must be between 1 and 5"));
    }
    let limit = p.limit.unwrap_or(5);
    if !(1..=30).contains(&limit) {
        return Ok(bad_input("limit", "limit must be between 1 and 30"));
    }
    let max_fanout = p.max_fanout.unwrap_or(16);
    if !(1..=64).contains(&max_fanout) {
        return Ok(bad_input(
            "max_fanout",
            "max_fanout must be between 1 and 64",
        ));
    }
    let edge_filter = parse_edge_filter(p.edge_types.as_ref());
    // Over-fetch so we can dedup terminal-file collisions and still return
    // `limit` distinct files. Without this, queries terminating at chunked
    // doc files (which have many chunks) return N near-identical paths
    // pointing at successive chunks of the same file, starving the agent
    // of breadth across other files reachable in the same step budget.
    let raw_limit = limit.saturating_mul(8).max(20);
    let (raw, truncated_expansions) = bfs(
        ctx,
        edge_index,
        from,
        to.as_ref(),
        to_type,
        edge_filter.as_ref(),
        max_depth,
        raw_limit,
        max_fanout,
    );
    let collapsed = collapse_paths_by_terminal_file(raw, limit);
    let cached = cache.insert_paths(PROCESS_SESSION_KEY, collapsed);
    Ok(render_response(ctx, &cached, &truncated_expansions))
}

/// The `to_type` acceptance test for a traversal.
///
/// `project_graph_vertex` is the logical entity type a caller reads off the
/// graph schema. Under a visibility policy that admits provisional overlay
/// generations, the very same vertex materializes as an overlay-scoped
/// `provisional_project_graph_vertex` compound ref, so a logical filter that
/// compared entity types by equality matched nothing and forced the caller to
/// know the overlay type name (gap-e41499a9). `bbox_inspect_entity` already
/// resolves logical refs to their provisional form; this extends the same
/// transparency to the type filter.
///
/// The widening is one-directional on purpose: an explicit
/// `to_type="provisional_project_graph_vertex"` keeps matching provisional
/// vertices only, so a caller can still target the overlay form exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TargetTypeFilter {
    requested: EntityType,
    admit_provisional_graph_vertex: bool,
}

impl TargetTypeFilter {
    fn new(requested: EntityType, provisional_mode: Option<&str>) -> Self {
        Self {
            requested,
            admit_provisional_graph_vertex: requested == EntityType::ProjectGraphVertex
                && visibility_admits_provisional(provisional_mode),
        }
    }

    fn matches(&self, candidate: &EntityRef) -> bool {
        let candidate_type = candidate.entity_type();
        candidate_type == self.requested
            || (self.admit_provisional_graph_vertex
                && candidate_type == EntityType::ProvisionalProjectGraphVertex)
    }
}

/// Whether the effective visibility policy can surface provisional overlay
/// vertices at all. `published` is the one policy that cannot. `None` defers
/// to the daemon, which resolves it to `own` when the session holds checkout
/// authority and to `published` otherwise; without that authority no
/// provisional vertex is reachable in the first place, so admitting the type
/// is inert rather than a visibility leak.
fn visibility_admits_provisional(provisional_mode: Option<&str>) -> bool {
    !matches!(provisional_mode.map(str::trim), Some("published"))
}

fn canonical_graph_ref(ctx: &ProviderContext<'_>, r: EntityRef) -> Result<EntityRef> {
    if matches!(
        r.entity_type(),
        EntityType::ProjectGraphVertex | EntityType::ProvisionalProjectGraphVertex
    ) {
        let entity = bbox_providers::entity_loader::load(ctx, &r)?;
        Ok(EntityRef::parse(&entity.ref_string)?)
    } else {
        Ok(r)
    }
}

/// Collapses paths whose terminal step lands on a different chunk of the
/// same project_file down to the first (BFS-order = shortest) path per
/// file. Other terminal entity types (commits, sessions, knowledge) are
/// passed through unchanged. Applied AFTER bfs so we still preserve the
/// distinct intermediate paths the agent might want to compare.
fn collapse_paths_by_terminal_file(paths: Vec<Vec<PathStep>>, limit: usize) -> Vec<Vec<PathStep>> {
    let mut seen_files = HashSet::<String>::new();
    let mut out = Vec::with_capacity(limit);
    for path in paths {
        if let Some(last) = path.last() {
            if let Some(key) = path_terminal_file_key(&last.to) {
                if !seen_files.insert(key) {
                    continue;
                }
            }
        }
        out.push(path);
        if out.len() >= limit {
            break;
        }
    }
    out
}

fn path_terminal_file_key(entity: &EntityRef) -> Option<String> {
    match entity {
        EntityRef::ProjectFile {
            project_id,
            rel_path_hash,
            ..
        }
        | EntityRef::ProjectFileV2 {
            project_id,
            rel_path_hash,
            ..
        } => Some(format!("project_file:{project_id}:{rel_path_hash}")),
        _ => None,
    }
}

/// A vertex whose ADMITTED neighborhood exceeded the per-hop fan-out cap,
/// with the count of admitted edges the traversal refused to enumerate past
/// the cap. Reported in the response so a truncated expansion is never
/// mistaken for the whole neighborhood. The count is post-admission by
/// construction: unreadable edges never reach the budget and are never
/// counted, so the disclosure the record makes is bounded by what the caller
/// may already read.
#[derive(Debug)]
struct FanoutTruncation {
    vertex: EntityRef,
    edge_count: usize,
}

fn bfs(
    ctx: &ProviderContext<'_>,
    edge_index: &EdgeIndex,
    from: EntityRef,
    to: Option<&EntityRef>,
    to_type: Option<TargetTypeFilter>,
    edge_filter: Option<&HashSet<String>>,
    max_depth: usize,
    limit: usize,
    max_fanout: usize,
) -> (Vec<Vec<PathStep>>, Vec<FanoutTruncation>) {
    let mut queue = VecDeque::new();
    let mut visited = HashSet::new();
    visited.insert(from.clone());
    queue.push_back(QueueEntry {
        current: from,
        steps: Vec::new(),
        visited,
    });
    let mut found = Vec::new();
    let mut truncated_expansions = Vec::new();
    let mut truncated_vertices = HashSet::new();
    while let Some(entry) = queue.pop_front() {
        // Graph selection precedes neighbor enumeration: a vertex whose lane
        // the caller may not read is absent, including the root, so the walk
        // never discloses the existence of an unreadable graph by expanding
        // out of it.
        if !ctx.graph_lane_admitted(&entry.current) {
            continue;
        }
        if entry.steps.len() >= max_depth {
            continue;
        }
        let node_expansions = expansions(ctx, edge_index, &entry.current, edge_filter);
        // Admission also owns the budget and the count: unreadable neighbors
        // never consume fan-out and never appear in edge_count, so a capped
        // answer cannot imply an excluded lane exists.
        let admitted: Vec<Expansion> = node_expansions
            .into_iter()
            .filter(|expansion| ctx.graph_lane_admitted(&expansion.next))
            .collect();
        if admitted.len() > max_fanout && truncated_vertices.insert(entry.current.clone()) {
            truncated_expansions.push(FanoutTruncation {
                vertex: entry.current.clone(),
                edge_count: admitted.len(),
            });
        }
        for Expansion {
            edge_kind,
            direction,
            next,
            metadata,
        } in admitted.into_iter().take(max_fanout)
        {
            if entry.visited.contains(&next) {
                continue;
            }
            let mut steps = entry.steps.clone();
            steps.push(PathStep {
                from: entry.current.clone(),
                edge_kind,
                to: next.clone(),
                direction,
                metadata,
            });
            if to.is_some_and(|target| target == &next)
                || to_type.is_some_and(|filter| filter.matches(&next))
            {
                found.push(steps.clone());
                if found.len() >= limit {
                    return (found, truncated_expansions);
                }
            }
            let mut path_visited = entry.visited.clone();
            path_visited.insert(next.clone());
            queue.push_back(QueueEntry {
                current: next,
                steps,
                visited: path_visited,
            });
        }
    }
    (found, truncated_expansions)
}

/// One admissible next hop, plus whatever per-edge labels produced it.
struct Expansion {
    edge_kind: String,
    direction: PathDirection,
    next: EntityRef,
    metadata: BTreeMap<String, String>,
}

/// Records one candidate hop, applying the edge-kind allowlist and the
/// unauthorized-endpoint refusal.
///
/// A free function rather than a closure over `out`, so the mutable borrow
/// ends at each call instead of spanning the whole enumeration.
fn push_expansion(
    out: &mut Vec<Expansion>,
    edge: Edge,
    direction: PathDirection,
    edge_filter: Option<&HashSet<String>>,
) {
    if edge_filter.is_some_and(|allowed| !allowed.contains(&edge.kind)) {
        return;
    }
    // Traversal never crosses an unauthorized endpoint. Inspection still shows
    // the edge for diagnosis, but a path that walked through a foreign scope
    // would be an answer the caller is not entitled to.
    if !evidence_step_is_traversable(&edge, direction) {
        return;
    }
    let next = match direction {
        PathDirection::Out => edge.target,
        PathDirection::In => edge.source,
    };
    out.push(Expansion {
        edge_kind: edge.kind,
        direction,
        next,
        metadata: edge.metadata,
    });
}

fn expansions(
    ctx: &ProviderContext<'_>,
    edge_index: &EdgeIndex,
    current: &EntityRef,
    edge_filter: Option<&HashSet<String>>,
) -> Vec<Expansion> {
    let mut out = Vec::new();
    if matches!(
        current.entity_type(),
        EntityType::ProjectGraphVertex | EntityType::ProvisionalProjectGraphVertex
    ) {
        if let Ok(entity) = bbox_providers::entity_loader::load(ctx, current) {
            for edge in entity.neighborhood.forward {
                push_expansion(&mut out, edge, PathDirection::Out, edge_filter);
            }
            for edge in entity.neighborhood.reverse {
                push_expansion(&mut out, edge, PathDirection::In, edge_filter);
            }
        }
        return out;
    }
    // forward_edges_with_synthesis surfaces the transcript -> session
    // IN_SESSION edge at query time (see its doc comment on EdgeIndex) so a
    // transcript ref is reachable to its session even without a materialized
    // edge. Forward only: the reverse enumeration isn't a pure function of
    // the session ref, so it isn't synthesized here.
    for edge in edge_index.forward_edges_with_synthesis(current) {
        push_expansion(&mut out, edge, PathDirection::Out, edge_filter);
    }
    for edge in edge_index.reverse_edges(current) {
        push_expansion(&mut out, Edge::clone(edge), PathDirection::In, edge_filter);
    }
    // A non-graph entity can still be an evidence endpoint: a project file or
    // knowledge entry that a binding points at is reachable from the graph
    // side, so it has to be reachable back the other way for the reverse
    // traversal to preserve provenance.
    for edge in ctx.evidence_edges(current) {
        let direction = if &edge.source == current {
            PathDirection::Out
        } else {
            PathDirection::In
        };
        push_expansion(&mut out, edge, direction, edge_filter);
    }
    out
}

/// Whether traversal may cross this edge into the endpoint it leads to.
///
/// Non-evidence edges are always traversable. An evidence edge is refused
/// only when the endpoint being STEPPED INTO is unauthorized; an unauthorized
/// endpoint behind us is already where we came from.
fn evidence_step_is_traversable(edge: &Edge, direction: PathDirection) -> bool {
    let key = match direction {
        PathDirection::Out => bbox_project_graph::EVIDENCE_META_TARGET_STATUS,
        PathDirection::In => bbox_project_graph::EVIDENCE_META_SOURCE_STATUS,
    };
    edge.metadata
        .get(key)
        .is_none_or(|status| status != EvidenceEndpointStatus::Unauthorized.as_str())
}

fn parse_edge_filter(raw: Option<&EdgeTypesParam>) -> Option<HashSet<String>> {
    let values: Vec<&str> = match raw? {
        EdgeTypesParam::One(value) => value.split(',').collect(),
        EdgeTypesParam::Many(values) => values.iter().map(String::as_str).collect(),
    };
    let set = values
        .into_iter()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect::<HashSet<_>>();
    (!set.is_empty()).then_some(set)
}

fn render_response(
    ctx: &ProviderContext<'_>,
    paths: &[CachedPath],
    truncated_expansions: &[FanoutTruncation],
) -> String {
    let mut text = if paths.is_empty() {
        "No paths found.".to_string()
    } else {
        paths
            .iter()
            .map(|path| format!("{}: {}", path.id, render_path(ctx, path)))
            .collect::<Vec<_>>()
            .join("\n")
    };
    if !truncated_expansions.is_empty() {
        // A capped expansion is a partial answer by construction; say so in
        // the rendered text as well as the structured field, because the
        // text is what a model reads first.
        text.push_str("\n\nExpansion truncated at the max_fanout cap:");
        for truncation in truncated_expansions.iter().take(8) {
            text.push_str(&format!(
                "\n- {} ({} edges enumerated up to the cap)",
                render_node(ctx, &truncation.vertex),
                truncation.edge_count,
            ));
        }
        let hidden = truncated_expansions.len().saturating_sub(8);
        if hidden > 0 {
            text.push_str(&format!("\n- and {hidden} more truncated vertices"));
        }
    }
    let text = cap_rendered_text(text);
    serde_json::to_string_pretty(&json!({
        "status": "ok",
        "text": text,
        "paths": paths.iter().map(|path| json!({
            "id": path.id,
            "summary": render_path(ctx, path),
            "steps": path.steps,
        })).collect::<Vec<_>>(),
        "truncated_expansions": truncated_expansions.iter().map(|truncation| json!({
            "vertex": truncation.vertex.render(),
            "edge_count": truncation.edge_count,
        })).collect::<Vec<_>>(),
    }))
    .expect("path response serializes")
}

fn cap_rendered_text(text: String) -> String {
    if text.len() <= RENDERED_TEXT_CAP_BYTES {
        return text;
    }
    let suffix = "\n[... paths text truncated at 30KB; use a lower limit or narrower edge_types]";
    let target = RENDERED_TEXT_CAP_BYTES.saturating_sub(suffix.len());
    let mut out = String::new();
    for ch in text.chars() {
        if out.len() + ch.len_utf8() > target {
            break;
        }
        out.push(ch);
    }
    out.push_str(suffix);
    out
}

pub fn render_path(ctx: &ProviderContext<'_>, path: &CachedPath) -> String {
    path.steps
        .iter()
        .map(|step| {
            let from = render_node(ctx, &step.from);
            let to = render_node(ctx, &step.to);
            let freshness = rendered_evidence_freshness(&step.metadata);
            match step.direction {
                PathDirection::Out => {
                    format!("{from} --{}{freshness}--> {to}", step.edge_kind)
                }
                PathDirection::In => {
                    format!("{from} <--{}{freshness}-- {to}", step.edge_kind)
                }
            }
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

/// Renders an evidence step's freshness inline so a reader sees a stale or
/// missing endpoint in the path text, not only in the structured metadata.
fn rendered_evidence_freshness(metadata: &BTreeMap<String, String>) -> String {
    metadata
        .get(bbox_project_graph::EVIDENCE_META_FRESHNESS)
        .map(|status| format!(" [{status}]"))
        .unwrap_or_default()
}

pub fn render_node(ctx: &ProviderContext<'_>, r: &EntityRef) -> String {
    match compact_label(ctx, r, None) {
        Some(label) => format!("{r} ({label})"),
        None => r.to_string(),
    }
}

fn bad_input(field: &str, message: impl AsRef<str>) -> String {
    json!({
        "status": "error.bad_input",
        "error": {
            "code": "error.bad_input",
            "message": message.as_ref(),
            "field": field,
            "suggested_fix": "Use canonical EntityRef values, comma-separated edge_types, max_depth <= 5, and limit <= 30."
        }
    })
    .to_string()
}

fn missing_target_bad_input() -> String {
    json!({
        "status": "error.bad_input",
        "error": {
            "code": "error.bad_input",
            "message": "find_paths requires a target: neither `to` nor `to_type` was supplied, so no candidate path can be accepted",
            "field": "to|to_type",
            "suggested_fix": "Pass `to` with one exact EntityRef to walk toward it, or `to_type` with an entity type (for example to_type=\"project_graph_vertex\") for an open-ended walk to the nearest entities of that type. Under own/all visibility, to_type=\"project_graph_vertex\" also matches provisional overlay vertices."
        }
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bbox_chunker::{EdgeConfidence, EdgeProvenance};
    use bbox_edge_index::edge_index::Edge;

    fn evidence_edge(
        source: &EntityRef,
        target: &EntityRef,
        status_key: &str,
        status: &str,
    ) -> Edge {
        Edge {
            source: source.clone(),
            kind: "record:CORRESPONDS_TO".into(),
            target: target.clone(),
            provenance: EdgeProvenance::Explicit,
            confidence: EdgeConfidence::Exact,
            metadata: BTreeMap::from([
                (
                    bbox_project_graph::EVIDENCE_META_BINDING_ID.to_string(),
                    "binding-1".to_string(),
                ),
                (status_key.to_string(), status.to_string()),
            ]),
            project_id: None,
        }
    }

    /// Contract: path traversal never crosses an unauthorized endpoint, while
    /// every other non-current status stays traversable and merely labeled.
    ///
    /// This exercises the guard directly rather than through a document,
    /// because a binding document cannot currently express a foreign-scope
    /// endpoint: project-scoped endpoints are materialized with the lane's own
    /// project id, and the literal-ref form refuses project-scoped types. The
    /// guard is the fail-closed backstop for anything that ever does produce
    /// one, so it is tested where it can actually be reached.
    #[test]
    fn traversal_refuses_only_the_unauthorized_endpoint_it_would_step_into() {
        let from = EntityRef::parse("knowledge:a").unwrap();
        let to = EntityRef::parse("knowledge:b").unwrap();

        let unauthorized_target = evidence_edge(
            &from,
            &to,
            bbox_project_graph::EVIDENCE_META_TARGET_STATUS,
            "unauthorized",
        );
        assert!(!evidence_step_is_traversable(
            &unauthorized_target,
            PathDirection::Out
        ));
        // Walking the other way, the unauthorized endpoint is behind us.
        assert!(evidence_step_is_traversable(
            &unauthorized_target,
            PathDirection::In
        ));

        for status in ["current", "stale", "missing", "unresolved"] {
            let edge = evidence_edge(
                &from,
                &to,
                bbox_project_graph::EVIDENCE_META_TARGET_STATUS,
                status,
            );
            assert!(
                evidence_step_is_traversable(&edge, PathDirection::Out),
                "{status} endpoints must stay traversable and labeled"
            );
        }

        // A plain corpus edge carries no evidence metadata and is unaffected.
        let plain = Edge {
            source: from,
            kind: "SUPERSEDES".into(),
            target: to,
            provenance: EdgeProvenance::Derived,
            confidence: EdgeConfidence::Exact,
            metadata: BTreeMap::new(),
            project_id: None,
        };
        assert!(evidence_step_is_traversable(&plain, PathDirection::Out));
        assert!(evidence_step_is_traversable(&plain, PathDirection::In));
    }

    #[test]
    fn bfs_finds_direction_preserving_path() {
        let a = EntityRef::parse("knowledge:a").unwrap();
        let b = EntityRef::parse("knowledge:b").unwrap();
        let edge = Edge {
            source: a.clone(),
            kind: "SUPERSEDES".into(),
            target: b.clone(),
            provenance: EdgeProvenance::Derived,
            confidence: EdgeConfidence::Exact,
            metadata: Default::default(),
            project_id: None,
        };
        let index = EdgeIndex::from_edges_for_tests(vec![edge]);
        let ctx = ProviderContext::empty_for_tests();
        let (paths, truncated) = bfs(&ctx, &index, a, Some(&b), None, None, 3, 5, 16);
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0][0].direction, PathDirection::Out);
        assert!(truncated.is_empty());
    }

    #[test]
    fn bfs_finds_synthesized_transcript_in_session_edge() {
        // gap-edc84378: a transcript ref with zero materialized edges must
        // still be traversable to its session via the query-time synthesized
        // IN_SESSION edge (EdgeIndex::forward_edges_with_synthesis).
        let transcript = EntityRef::parse("transcript:claude:sess-1:42:0").unwrap();
        let session = EntityRef::parse("session:claude:sess-1").unwrap();
        let index = EdgeIndex::default();
        let ctx = ProviderContext::empty_for_tests();

        let (paths, truncated) = bfs(
            &ctx,
            &index,
            transcript.clone(),
            Some(&session),
            None,
            None,
            3,
            5,
            16,
        );
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].len(), 1);
        assert_eq!(paths[0][0].edge_kind, "IN_SESSION");
        assert_eq!(paths[0][0].direction, PathDirection::Out);
        assert_eq!(paths[0][0].to, session);
        assert!(truncated.is_empty());

        // Reachable by to_type too (mirrors the max-depth/limit-boundary
        // traversal an agent would actually run).
        let by_type = bfs(
            &ctx,
            &index,
            transcript,
            None,
            Some(TargetTypeFilter::new(EntityType::Session, None)),
            None,
            3,
            5,
            16,
        );
        assert_eq!(by_type.0.len(), 1);
    }

    #[test]
    fn find_paths_without_a_target_refuses_loudly() {
        // gap-e41499a9: a call with neither `to` nor `to_type` used to walk
        // the whole neighborhood, accept nothing, and answer "No paths
        // found." That reads as an empty graph rather than a malformed call.
        let ctx = ProviderContext::empty_for_tests();
        let index = EdgeIndex::default();
        let mut cache = PathCache::default();
        let params = FindPathsParams {
            from: "knowledge:abcd1234".into(),
            provisional: None,
            to: None,
            to_type: None,
            edge_types: None,
            max_depth: None,
            limit: None,
            max_fanout: None,
        };

        let raw = find_paths(&params, &ctx, &index, &mut cache).unwrap();
        let value: serde_json::Value = serde_json::from_str(&raw).unwrap();

        assert_eq!(value["status"], "error.bad_input");
        assert_eq!(value["error"]["code"], "error.bad_input");
        assert_eq!(value["error"]["field"], "to|to_type");
        let message = value["error"]["message"].as_str().unwrap();
        assert!(message.contains("requires a target"), "{message}");
        let fix = value["error"]["suggested_fix"].as_str().unwrap();
        assert!(fix.contains("`to`"), "{fix}");
        assert!(fix.contains("`to_type`"), "{fix}");
        assert!(!raw.contains("No paths found"), "{raw}");
    }

    #[test]
    fn find_paths_still_answers_when_only_to_type_is_supplied() {
        // The refusal above must not swallow the open-ended walk shape:
        // to_type alone is a complete target.
        let transcript = EntityRef::parse("transcript:claude:sess-1:42:0").unwrap();
        let ctx = ProviderContext::empty_for_tests();
        let index = EdgeIndex::default();
        let mut cache = PathCache::default();
        let params = FindPathsParams {
            from: transcript.to_string(),
            provisional: None,
            to: None,
            to_type: Some("session".into()),
            edge_types: None,
            max_depth: None,
            limit: None,
            max_fanout: None,
        };

        let raw = find_paths(&params, &ctx, &index, &mut cache).unwrap();
        let value: serde_json::Value = serde_json::from_str(&raw).unwrap();

        assert_eq!(value["status"], "ok");
        assert_eq!(value["paths"].as_array().unwrap().len(), 1);
    }

    /// A resolver that admits every ref except a named set, so the graph
    /// lane gate can be exercised without a daemon. `resolve_entity` is
    /// never reached: the gate refuses (or admits) before the loader runs.
    struct DenyingResolver {
        denied: HashSet<EntityRef>,
    }

    impl bbox_providers::providers::ProjectGraphEntityResolver for DenyingResolver {
        fn resolve_entity(
            &self,
            _r: &EntityRef,
            _provisional: Option<&str>,
        ) -> Result<bbox_providers::providers::EntityView> {
            anyhow::bail!("the admission gate must run before entity loading")
        }

        fn traversal_admits(&self, r: &EntityRef, _provisional: Option<&str>) -> bool {
            !self.denied.contains(r)
        }
    }

    /// Unified-retrieval 5.2: graph selection precedes neighbor enumeration.
    /// An edge may name a vertex in a lane the caller may not read; the hop
    /// must drop out before the target test, before the frontier, and before
    /// any count, so the answer cannot imply the excluded graph exists.
    #[test]
    fn bfs_drops_hops_into_graph_lanes_the_resolver_refuses() {
        let from = EntityRef::parse("knowledge:a").unwrap();
        let readable = EntityRef::parse("project_graph_vertex:proj1:graph-a:v1").unwrap();
        let excluded = EntityRef::parse("project_graph_vertex:proj1:graph-b:v2").unwrap();
        let edges = [readable.clone(), excluded.clone()]
            .into_iter()
            .map(|target| Edge {
                source: from.clone(),
                kind: "record:CORRESPONDS_TO".into(),
                target,
                provenance: EdgeProvenance::Explicit,
                confidence: EdgeConfidence::Exact,
                metadata: BTreeMap::new(),
                project_id: None,
            })
            .collect();
        let index = EdgeIndex::from_edges_for_tests(edges);
        let resolver = DenyingResolver {
            denied: HashSet::from([excluded.clone()]),
        };
        let ctx = ProviderContext::empty_for_tests().with_project_graph_resolver(&resolver, None);

        let (paths, truncated) = bfs(
            &ctx,
            &index,
            from.clone(),
            None,
            Some(TargetTypeFilter::new(EntityType::ProjectGraphVertex, None)),
            None,
            1,
            5,
            16,
        );
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0][0].to, readable);
        assert!(truncated.is_empty());

        // Same walk aimed only at the excluded lane: no path, and equally
        // important no truncated-expansion note, because nothing about the
        // excluded vertex was ever enumerated.
        let denied_only = bfs(&ctx, &index, from, Some(&excluded), None, None, 1, 5, 16);
        assert!(denied_only.0.is_empty());
        assert!(denied_only.1.is_empty());

        // The root is gated by the same live check: a walk that starts inside
        // an unreadable lane is absent, not half-expanded.
        let rooted_in_excluded = bfs(
            &ctx,
            &index,
            excluded,
            Some(&readable),
            None,
            None,
            1,
            5,
            16,
        );
        assert!(rooted_in_excluded.0.is_empty());
    }

    /// The admission filter owns the fan-out budget, not just the output: a
    /// hub with a neighborhood beyond the cap still reads as untruncated when
    /// the unreadable edges are what pushed it over, and the readable
    /// neighbors all survive. Unreadable edges consuming budget (or showing
    /// up in edge_count) would disclose the excluded lane's existence.
    #[test]
    fn unreadable_neighbors_neither_consume_fanout_nor_count() {
        let from = EntityRef::parse("knowledge:hub").unwrap();
        let readable: Vec<EntityRef> = (1..=10)
            .map(|idx| EntityRef::parse(&format!("knowledge:leaf-{idx}")).unwrap())
            .collect();
        let excluded: Vec<EntityRef> = (1..=10)
            .map(|idx| {
                EntityRef::parse(&format!("project_graph_vertex:proj1:graph-disabled:v{idx}"))
                    .unwrap()
            })
            .collect();
        let edges = readable
            .iter()
            .chain(excluded.iter())
            .cloned()
            .map(|target| Edge {
                source: from.clone(),
                kind: "SUPERSEDES".into(),
                target,
                provenance: EdgeProvenance::Derived,
                confidence: EdgeConfidence::Exact,
                metadata: BTreeMap::new(),
                project_id: None,
            })
            .collect();
        let index = EdgeIndex::from_edges_for_tests(edges);
        let resolver = DenyingResolver {
            denied: excluded.into_iter().collect(),
        };
        let ctx = ProviderContext::empty_for_tests().with_project_graph_resolver(&resolver, None);

        let (paths, truncated) = bfs(
            &ctx,
            &index,
            from,
            None,
            Some(TargetTypeFilter::new(EntityType::Knowledge, None)),
            None,
            1,
            30,
            16,
        );
        assert_eq!(paths.len(), 10, "every readable neighbor survives: {paths:?}");
        assert!(
            truncated.is_empty(),
            "admitted count is 10, so a 16 cap must not truncate: {truncated:?}"
        );
    }

    /// Unified-retrieval 5.2: the per-hop fan-out cap bounds expansion, and a
    /// capped expansion says so explicitly instead of silently returning a
    /// prefix of the neighborhood.
    #[test]
    fn fanout_cap_bounds_expansion_and_reports_the_truncation() {
        let from = EntityRef::parse("knowledge:hub").unwrap();
        let edges = (1..=20)
            .map(|idx| Edge {
                source: from.clone(),
                kind: "SUPERSEDES".into(),
                target: EntityRef::parse(&format!("knowledge:leaf-{idx}")).unwrap(),
                provenance: EdgeProvenance::Derived,
                confidence: EdgeConfidence::Exact,
                metadata: BTreeMap::new(),
                project_id: None,
            })
            .collect();
        let index = EdgeIndex::from_edges_for_tests(edges);
        let ctx = ProviderContext::empty_for_tests();
        let mut cache = PathCache::default();

        let (paths, truncated) = bfs(
            &ctx,
            &index,
            from.clone(),
            None,
            Some(TargetTypeFilter::new(EntityType::Knowledge, None)),
            None,
            3,
            40,
            5,
        );
        assert_eq!(paths.len(), 5, "only the first five hops are enumerated");
        assert_eq!(truncated.len(), 1);
        assert_eq!(truncated[0].vertex, from);
        assert_eq!(truncated[0].edge_count, 20);

        // The tool response carries the truncation in both the structured
        // field and the rendered text the model reads first.
        let params = FindPathsParams {
            from: "knowledge:hub".into(),
            provisional: None,
            to: None,
            to_type: Some("knowledge".into()),
            edge_types: None,
            max_depth: Some(1),
            limit: Some(5),
            max_fanout: Some(6),
        };
        let raw = find_paths(&params, &ctx, &index, &mut cache).unwrap();
        let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(value["status"], "ok");
        assert_eq!(value["truncated_expansions"][0]["edge_count"], 20, "{raw}");
        let text = value["text"].as_str().unwrap();
        assert!(text.contains("Expansion truncated"), "{text}");
        assert!(text.contains("max_fanout"), "{text}");

        // Out-of-range caps are refused loudly, mirroring max_depth.
        for bad in [0usize, 65] {
            let params = FindPathsParams {
                from: "knowledge:hub".into(),
                provisional: None,
                to: Some("knowledge:leaf-1".into()),
                to_type: None,
                edge_types: None,
                max_depth: None,
                limit: None,
                max_fanout: Some(bad),
            };
            let raw = find_paths(&params, &ctx, &index, &mut cache).unwrap();
            let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
            assert_eq!(value["status"], "error.bad_input", "{raw}");
            assert_eq!(value["error"]["field"], "max_fanout", "{raw}");
        }
    }

    #[test]
    fn logical_graph_vertex_to_type_matches_provisional_overlay_vertices() {
        // gap-e41499a9: under a visibility policy that admits the overlay,
        // to_type="project_graph_vertex" must accept the provisional compound
        // form the traversal actually yields.
        let provisional = EntityRef::parse(&format!(
            "provisional_project_graph_vertex:{}:{}:domain:concept/alpha",
            "b".repeat(64),
            "a".repeat(32)
        ))
        .unwrap();
        let published =
            EntityRef::parse("project_graph_vertex:proj1234:domain:concept/alpha").unwrap();

        for mode in [None, Some("own"), Some("all")] {
            let filter = TargetTypeFilter::new(EntityType::ProjectGraphVertex, mode);
            assert!(filter.matches(&provisional), "mode {mode:?}");
            assert!(filter.matches(&published), "mode {mode:?}");
        }

        // published visibility cannot surface an overlay vertex, so the
        // filter stays exact there.
        let published_only =
            TargetTypeFilter::new(EntityType::ProjectGraphVertex, Some("published"));
        assert!(!published_only.matches(&provisional));
        assert!(published_only.matches(&published));

        // The explicit overlay type name keeps its exact meaning in every
        // policy: it never widens back to the published form.
        for mode in [None, Some("own"), Some("all"), Some("published")] {
            let explicit = TargetTypeFilter::new(EntityType::ProvisionalProjectGraphVertex, mode);
            assert!(explicit.matches(&provisional), "mode {mode:?}");
            assert!(!explicit.matches(&published), "mode {mode:?}");
        }

        // The widening is scoped to the graph-vertex pair; unrelated types
        // are untouched by the visibility policy.
        let sessions = TargetTypeFilter::new(EntityType::Session, Some("own"));
        assert!(!sessions.matches(&provisional));
    }
}
