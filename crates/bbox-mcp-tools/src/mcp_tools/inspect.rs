use std::collections::{BTreeMap, BTreeSet, HashSet};

use sha2::{Digest, Sha256};

use anyhow::{Result, bail};
use rmcp::schemars;
use serde::{Deserialize, Serialize};
use serde_json::json;

use bbox_corpus_core::entity_ref::EntityRef;
use bbox_edge_index::edge_index::{Edge, EdgeIndex};
use bbox_project_graph::EvidenceEndpointStatus;
use bbox_providers::entity_loader;
use bbox_providers::providers::{self, EntityView, Neighborhood, NextHop, ProviderContext};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InspectEntityParams {
    pub entity_ref: String,
    /// Knowledge visibility policy: published, own, or all.
    #[serde(default)]
    pub provisional: Option<String>,
    pub edge_types: Option<String>,
    /// Edge direction: out, in, or both (default).
    pub direction: Option<String>,
    /// Maximum edges per family and direction: default 5.
    /// Zero requests properties only. At most 100 edges are returned per page.
    /// Follow edge_page.next_cursor with the same selectors for remaining edges.
    pub per_type_limit: Option<usize>,
    /// Property detail: smart (default, text shortened to 300 characters),
    /// summary (names, status, source identity and freshness), or full.
    /// property_projection identifies omitted/shortened properties and how
    /// to retrieve them in full. Unknown values are errors.
    pub property_mode: Option<String>,
    /// Opaque edge_page.next_cursor from the preceding response. Keep the same
    /// entity_ref, direction, edge_types and per_type_limit. Changed evidence
    /// or selection rejects the cursor; restart without it.
    #[serde(default)]
    pub edge_cursor: Option<String>,
    /// Read this exact property as text pages instead of an edge/property
    /// overview. Choose a key under properties or property_projection.omitted_keys. Empty strings are
    /// valid property values; absent keys return error.not_found.
    #[serde(default)]
    pub property: Option<String>,
    /// Opaque body.next_cursor from a preceding read of the same property.
    #[serde(default)]
    pub property_cursor: Option<String>,
    /// Bytes per exact property page: default/max 4096, minimum 4.
    #[serde(default)]
    pub property_limit: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InspectDirection {
    Out,
    In,
    Both,
}

impl InspectDirection {
    fn parse(input: Option<&str>) -> Result<Self> {
        match input.unwrap_or("both").to_ascii_lowercase().as_str() {
            "out" => Ok(Self::Out),
            "in" => Ok(Self::In),
            "both" => Ok(Self::Both),
            other => bail!("invalid direction `{other}`; use out, in, or both"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PropertyMode {
    Summary,
    Smart,
    Full,
}

impl PropertyMode {
    fn parse(input: Option<&str>) -> Result<Self> {
        match input.unwrap_or("smart").to_ascii_lowercase().as_str() {
            "summary" => Ok(Self::Summary),
            "smart" => Ok(Self::Smart),
            "full" => Ok(Self::Full),
            other => bail!("invalid property_mode `{other}`; use summary, smart, or full"),
        }
    }
}

#[derive(Debug, Serialize)]
struct RenderedEdge {
    kind: String,
    source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_label: Option<String>,
    target: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    target_label: Option<String>,
    direction: String,
    /// Per-edge labels, currently the `evidence.*` family. Absent on ordinary
    /// corpus edges, so the payload does not grow for the common case.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    properties: BTreeMap<String, String>,
}

#[derive(Debug, Serialize)]
struct RenderedNextHop {
    edge_family: String,
    count: usize,
    /// `out` / `in` when the substrate knows which way the hop goes. Absent for
    /// direction-blind family counts, so the payload does not grow for
    /// providers that have no schema to read a direction from.
    #[serde(skip_serializing_if = "Option::is_none")]
    direction: Option<String>,
    /// What the hop MEANS, from a schema-authored hint. Absent when nobody
    /// wrote one.
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<String>,
}

#[derive(Debug, Serialize)]
struct RenderedCoverage {
    family: String,
    count: usize,
    expected: String,
    status: String,
}

pub fn bad_input(entity_ref: &str, message: impl AsRef<str>) -> String {
    json!({
        "status": "error.bad_input",
        "error": {
            "code": "error.bad_input",
            "message": message.as_ref(),
            "field": "entity_ref",
            "suggested_fix": "Use a canonical EntityRef such as knowledge:<entry_id>, project_file:<project_id>:<rel_path_hash>:<chunk_hash>:<occurrence_idx>, or commit:<repo_id>:<sha>."
        },
        "entity_ref": entity_ref,
    })
    .to_string()
}

pub fn bad_input_field(field: &str, message: impl AsRef<str>, suggested_fix: &str) -> String {
    json!({
        "status": "error.bad_input",
        "error": {
            "code": "error.bad_input",
            "message": message.as_ref(),
            "field": field,
            "suggested_fix": suggested_fix,
        },
    })
    .to_string()
}

pub fn not_found(r: &EntityRef, similar_refs: Vec<String>) -> String {
    json!({
        "status": "error.not_found",
        "error": {
            "code": "error.not_found",
            "message": format!("No entity found for {r}"),
            "ref": r.to_string(),
            "similar_refs": similar_refs,
        }
    })
    .to_string()
}

pub fn similar_refs(edge_index: &EdgeIndex, r: &EntityRef) -> Vec<String> {
    let needle = r.to_string();
    let prefix = r.entity_type().as_str();
    edge_index
        .known_refs()
        .into_iter()
        .map(|known| known.to_string())
        .filter(|known| known.starts_with(prefix))
        .filter(|known| known != &needle)
        .take(5)
        .collect()
}

pub fn inspect_entity(
    p: &InspectEntityParams,
    ctx: &ProviderContext<'_>,
    r: &EntityRef,
    edge_index: &EdgeIndex,
) -> Result<String> {
    let direction = match InspectDirection::parse(p.direction.as_deref()) {
        Ok(direction) => direction,
        Err(err) => {
            return Ok(bad_input_field(
                "direction",
                err.to_string(),
                "Use direction=out, in, or both.",
            ));
        }
    };
    let property_mode = match PropertyMode::parse(p.property_mode.as_deref()) {
        Ok(mode) => mode,
        Err(err) => {
            return Ok(bad_input_field(
                "property_mode",
                err.to_string(),
                "Use property_mode=summary, smart, or full.",
            ));
        }
    };
    if p.property.is_none() && (p.property_cursor.is_some() || p.property_limit.is_some()) {
        return Ok(bad_input_field(
            "property",
            "property_cursor and property_limit require property",
            "Set property to a key returned under properties.",
        ));
    }
    if p.property.is_some() && p.edge_cursor.is_some() {
        return Ok(bad_input_field(
            "edge_cursor",
            "property body reads cannot also continue edges",
            "Read property pages and edge pages in separate calls.",
        ));
    }
    if p.edge_cursor.is_some() && p.per_type_limit == Some(0) {
        return Ok(bad_input_field(
            "per_type_limit",
            "zero disables edge retrieval",
            "Continue with the same positive per_type_limit as the preceding edge page.",
        ));
    }
    let per_type_limit = p.per_type_limit.unwrap_or(5);
    let edge_filter = parse_edge_filter(p.edge_types.as_deref());
    let provider = providers::provider_for(r.entity_type());
    let mut entity = match entity_loader::load(ctx, r) {
        Ok(entity) => entity,
        Err(error) if error.to_string().starts_with("error.checkout_access.") => {
            return Err(error);
        }
        Err(error)
            if error.to_string().starts_with("error.project_graph_")
                || error
                    .to_string()
                    .starts_with("error.not_found: project graph") =>
        {
            return Err(error);
        }
        Err(_) => return Ok(not_found(r, similar_refs(edge_index, r))),
    };
    let canonical_ref = EntityRef::parse(&entity.ref_string).unwrap_or_else(|_| r.clone());
    if let Some(property) = p.property.as_deref() {
        return property_body_response(p, &entity, &canonical_ref, property);
    }
    let full_neighborhood = if matches!(
        r.entity_type(),
        bbox_corpus_core::entity_ref::EntityType::ProjectGraphVertex
            | bbox_corpus_core::entity_ref::EntityType::ProvisionalProjectGraphVertex
    ) {
        // The graph resolver already attached this vertex's evidence edges.
        entity.neighborhood.clone()
    } else {
        // A project file or knowledge entry is a legal evidence endpoint, so
        // its neighborhood has to carry the bindings that point at it. Without
        // this the edge would exist only on the graph side and the reverse
        // traversal would lose it.
        let mut neighborhood = full_neighborhood(edge_index, r);
        for edge in ctx.evidence_edges(r) {
            if &edge.source == r {
                neighborhood.forward.push(edge);
            } else {
                neighborhood.reverse.push(edge);
            }
        }
        neighborhood
    };
    let (neighborhood, edge_page) = match page_neighborhood(
        &full_neighborhood,
        &canonical_ref.to_string(),
        &entity.properties,
        direction,
        edge_filter.as_ref(),
        per_type_limit,
        p.edge_cursor.as_deref(),
    ) {
        Ok(page) => page,
        Err(error) => {
            return Ok(bad_input_field(
                "edge_cursor",
                error.to_string(),
                "Restart without edge_cursor when evidence or selectors change.",
            ));
        }
    };
    entity.neighborhood = neighborhood;
    // Resolve freshness only for the bounded preview. Counts and authored
    // next-hop semantics use the complete neighborhood without endpoint loads.
    for edge in entity
        .neighborhood
        .forward
        .iter_mut()
        .chain(entity.neighborhood.reverse.iter_mut())
    {
        refine_evidence_edge(ctx, edge);
    }
    let rendered_forward = render_edges(ctx, &entity.neighborhood.forward, "out");
    let rendered_reverse = render_edges(ctx, &entity.neighborhood.reverse, "in");
    let recommended = render_next_hops(provider.recommended_next_hops(&entity, &full_neighborhood));
    let coverage = provider
        .expected_edge_families(r)
        .into_iter()
        .map(|expectation| {
            let count = full_neighborhood
                .forward
                .iter()
                .chain(full_neighborhood.reverse.iter())
                .filter(|edge| edge.kind == expectation.family_name)
                .count();
            RenderedCoverage {
                family: expectation.family_name,
                count,
                expected: if expectation.required {
                    "required"
                } else {
                    "optional"
                }
                .into(),
                status: if count > 0 { "present" } else { "0 (expected)" }.into(),
            }
        })
        .collect::<Vec<_>>();
    let properties = render_properties(&entity, property_mode);
    // Keep required absences and observed relationships; generic optional
    // zero-count families add no evidence. Authored absences remain in hops.
    let coverage_json: Vec<&RenderedCoverage> = coverage
        .iter()
        .filter(|c| c.count > 0 || c.expected == "required")
        .collect();
    let mut out = json!({
        "status": "ok",
        "entity_ref": canonical_ref.to_string(),
        "entity_type": canonical_ref.entity_type().as_str(),
        "properties": properties,
        "edges": {
            "out": rendered_forward,
            "in": rendered_reverse,
        },
    });
    if !recommended.is_empty() {
        out["recommended_next_hops"] = json!(recommended);
    }
    if !coverage_json.is_empty() {
        out["edge_family_coverage"] = json!(coverage_json);
    }
    if let Some(page) = edge_page {
        out["edge_page"] = page;
    }
    let omitted_keys: Vec<&str> = entity
        .properties
        .keys()
        .filter(|key| !properties.contains_key(*key))
        .map(String::as_str)
        .collect();
    let shortened: Vec<&str> = properties
        .iter()
        .filter(|(key, value)| entity.properties.get(*key) != Some(*value))
        .map(|(key, _)| key.as_str())
        .collect();
    if !omitted_keys.is_empty() || !shortened.is_empty() {
        out["property_projection"] = json!({
            "omitted": omitted_keys.len(),
            "omitted_keys": omitted_keys,
            "shortened": shortened,
            "expand_hint": "Use property=<key> for exact text pages and follow body.next_cursor; property_mode=full returns all properties when they fit.",
        });
    }
    Ok(serde_json::to_string_pretty(&out)?)
}

fn parse_edge_filter(raw: Option<&str>) -> Option<HashSet<String>> {
    let raw = raw?.trim();
    if raw.is_empty() {
        return None;
    }
    Some(
        raw.split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .collect(),
    )
}

/// Second-pass endpoint scoring for an evidence edge.
///
/// The graph layer can observe graph vertices, because it holds the view
/// catalog, and marks everything else `unresolved`. Here the provider registry
/// is in hand, so an `unresolved` endpoint can be settled by trying to load
/// it: loadable is `current`, not loadable is `missing` (or `stale` when the
/// binding recorded a generation to be stale against). Endpoints the graph
/// layer already scored are left alone, and a non-evidence edge is untouched.
pub fn refine_evidence_edge(ctx: &ProviderContext<'_>, edge: &mut Edge) {
    if !edge
        .metadata
        .contains_key(bbox_project_graph::EVIDENCE_META_BINDING_ID)
    {
        return;
    }
    let source = refined_endpoint_status(
        ctx,
        &edge.source,
        edge.metadata
            .get(bbox_project_graph::EVIDENCE_META_SOURCE_STATUS),
        edge.metadata
            .contains_key(bbox_project_graph::EVIDENCE_META_SOURCE_GENERATION),
    );
    let target = refined_endpoint_status(
        ctx,
        &edge.target,
        edge.metadata
            .get(bbox_project_graph::EVIDENCE_META_TARGET_STATUS),
        edge.metadata
            .contains_key(bbox_project_graph::EVIDENCE_META_TARGET_GENERATION),
    );
    edge.metadata.insert(
        bbox_project_graph::EVIDENCE_META_SOURCE_STATUS.to_string(),
        source.as_str().to_string(),
    );
    edge.metadata.insert(
        bbox_project_graph::EVIDENCE_META_TARGET_STATUS.to_string(),
        target.as_str().to_string(),
    );
    edge.metadata.insert(
        bbox_project_graph::EVIDENCE_META_FRESHNESS.to_string(),
        bbox_project_graph::aggregate_endpoint_status(source, target)
            .as_str()
            .to_string(),
    );
}

fn refined_endpoint_status(
    ctx: &ProviderContext<'_>,
    entity: &EntityRef,
    recorded: Option<&String>,
    has_expected_generation: bool,
) -> EvidenceEndpointStatus {
    let recorded = recorded.map(String::as_str);
    if recorded != Some(EvidenceEndpointStatus::Unresolved.as_str()) {
        return match recorded {
            Some(value) if value == EvidenceEndpointStatus::Current.as_str() => {
                EvidenceEndpointStatus::Current
            }
            Some(value) if value == EvidenceEndpointStatus::Stale.as_str() => {
                EvidenceEndpointStatus::Stale
            }
            Some(value) if value == EvidenceEndpointStatus::Missing.as_str() => {
                EvidenceEndpointStatus::Missing
            }
            Some(value) if value == EvidenceEndpointStatus::Unauthorized.as_str() => {
                EvidenceEndpointStatus::Unauthorized
            }
            _ => EvidenceEndpointStatus::Unresolved,
        };
    }
    let observation = match entity_loader::load(ctx, entity) {
        // A loadable non-graph endpoint carries no generation to compare, so
        // presence alone makes it current.
        Ok(_) => bbox_project_graph::EvidenceEndpointObservation::Present { generation: None },
        Err(_) => bbox_project_graph::EvidenceEndpointObservation::Absent,
    };
    // The exact recorded generation does not matter here, only whether the
    // binding recorded one: that is what separates a stale endpoint (we know
    // it used to be there) from a merely missing one.
    bbox_project_graph::resolve_endpoint_status(
        observation,
        has_expected_generation.then_some(0_u64),
    )
}

fn full_neighborhood(edge_index: &EdgeIndex, r: &EntityRef) -> Neighborhood {
    Neighborhood {
        // forward_edges_with_synthesis fills in the transcript -> session
        // IN_SESSION edge at query time when it isn't materialized (see its
        // doc comment). This is forward only; reverse has no counterpart.
        forward: edge_index.forward_edges_with_synthesis(r),
        reverse: edge_index.reverse_edges(r).into_iter().cloned().collect(),
    }
}

const TOTAL_EDGE_CAP: usize = 100;
const PROPERTY_PAGE_BYTES: usize = 4096;

fn cursor_offset(cursor: Option<&str>, revision: &str) -> Result<usize> {
    let Some(cursor) = cursor else {
        return Ok(0);
    };
    let (expected, offset) = cursor
        .split_once(':')
        .ok_or_else(|| anyhow::anyhow!("invalid continuation cursor"))?;
    if expected != revision {
        bail!("evidence or selectors changed; restart without cursor");
    }
    offset
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid continuation offset"))
}

struct OrderedEdge<'a> {
    round: usize,
    kind: &'a str,
    direction: &'static str,
    within_group: usize,
    wire: String,
    edge: &'a Edge,
}

/// Stable rounds preserve per-family previews while exposing every edge.
/// A page never crosses a round boundary, so a small family cannot consume
/// another family's quota; the aggregate cap can split a wide round.
fn page_neighborhood(
    full: &Neighborhood,
    entity_ref: &str,
    properties: &BTreeMap<String, String>,
    direction: InspectDirection,
    edge_filter: Option<&HashSet<String>>,
    per_type_limit: usize,
    cursor: Option<&str>,
) -> Result<(Neighborhood, Option<serde_json::Value>)> {
    if per_type_limit == 0 {
        return Ok((Neighborhood::default(), None));
    }
    let mut groups = BTreeMap::<(&str, &'static str), Vec<(String, &Edge)>>::new();
    for (label, edges, included) in [
        (
            "out",
            &full.forward,
            matches!(direction, InspectDirection::Out | InspectDirection::Both),
        ),
        (
            "in",
            &full.reverse,
            matches!(direction, InspectDirection::In | InspectDirection::Both),
        ),
    ] {
        if !included {
            continue;
        }
        for edge in edges {
            if edge_filter.is_some_and(|allowed| !allowed.contains(&edge.kind)) {
                continue;
            }
            groups
                .entry((&edge.kind, label))
                .or_default()
                .push((serde_json::to_string(edge)?, edge));
        }
    }
    let mut ordered = Vec::new();
    for ((kind, direction), mut group) in groups {
        group.sort_by(|a, b| a.0.cmp(&b.0));
        for (index, (wire, edge)) in group.into_iter().enumerate() {
            ordered.push(OrderedEdge {
                round: index / per_type_limit,
                kind,
                direction,
                within_group: index % per_type_limit,
                wire,
                edge,
            });
        }
    }
    ordered.sort_by(|a, b| {
        (a.round, a.kind, a.direction, a.within_group).cmp(&(
            b.round,
            b.kind,
            b.direction,
            b.within_group,
        ))
    });
    let mut hash = Sha256::new();
    let selected_types = edge_filter.map(|types| types.iter().collect::<BTreeSet<_>>());
    let authority: BTreeMap<_, _> = properties
        .iter()
        .filter(|(key, _)| is_identity_property(key))
        .collect();
    hash.update(serde_json::to_vec(&(
        "inspect-edges-v1",
        entity_ref,
        authority,
        format!("{direction:?}"),
        selected_types,
        per_type_limit,
    ))?);
    for item in &ordered {
        hash.update(item.direction.as_bytes());
        hash.update(item.wire.as_bytes());
    }
    let revision = format!("{:x}", hash.finalize());
    let offset = cursor_offset(cursor, &revision)?;
    if offset > ordered.len() {
        bail!("continuation offset exceeds selected edge count");
    }
    let mut page = Neighborhood::default();
    let mut end = offset;
    if let Some(first) = ordered.get(offset) {
        for item in ordered.iter().skip(offset).take(TOTAL_EDGE_CAP) {
            if item.round != first.round {
                break;
            }
            if item.direction == "out" {
                page.forward.push(item.edge.clone());
            } else {
                page.reverse.push(item.edge.clone());
            }
            end += 1;
        }
    }
    let metadata = if end < ordered.len() || offset > 0 {
        let mut value =
            json!({"matching": ordered.len(), "offset": offset, "returned": end - offset});
        if end < ordered.len() {
            value["next_cursor"] = json!(format!("{revision}:{end}"));
        }
        Some(value)
    } else {
        None
    };
    Ok((page, metadata))
}

fn property_body_response(
    p: &InspectEntityParams,
    entity: &EntityView,
    canonical_ref: &EntityRef,
    property: &str,
) -> Result<String> {
    let Some(value) = entity.properties.get(property) else {
        return Ok(json!({"status":"error.not_found", "error": {"code":"error.not_found", "field":"property", "message":format!("No property named {property}")}, "entity_ref":canonical_ref.to_string()}).to_string());
    };
    let mut hash = Sha256::new();
    let authority: BTreeMap<_, _> = entity
        .properties
        .iter()
        .filter(|(key, _)| is_identity_property(key))
        .collect();
    hash.update(serde_json::to_vec(&(
        "inspect-property-v1",
        &entity.ref_string,
        property,
        value,
        authority,
    ))?);
    let revision = format!("{:x}", hash.finalize());
    let offset = match cursor_offset(p.property_cursor.as_deref(), &revision) {
        Ok(offset) if offset <= value.len() && value.is_char_boundary(offset) => offset,
        Ok(_) => {
            return Ok(bad_input_field(
                "property_cursor",
                "invalid property byte boundary",
                "Restart this property read without property_cursor.",
            ));
        }
        Err(error) => {
            return Ok(bad_input_field(
                "property_cursor",
                error.to_string(),
                "Restart this property read without property_cursor.",
            ));
        }
    };
    let mut end = offset
        .saturating_add(
            p.property_limit
                .unwrap_or(PROPERTY_PAGE_BYTES)
                .clamp(4, PROPERTY_PAGE_BYTES),
        )
        .min(value.len());
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    let mut body = json!({"text": &value[offset..end], "offset":offset, "total_bytes":value.len()});
    if end < value.len() {
        body["next_cursor"] = json!(format!("{revision}:{end}"));
    }
    let identity: BTreeMap<_, _> = entity
        .properties
        .iter()
        .filter(|(key, _)| is_identity_property(key) && key.as_str() != property)
        .collect();
    let mut out = json!({
        "status":"ok", "entity_ref":canonical_ref.to_string(), "entity_type":canonical_ref.entity_type().as_str(),
        "property":property, "body":body,
    });
    if !identity.is_empty() {
        out["source"] = json!(identity);
    }
    Ok(serde_json::to_string_pretty(&out)?)
}

fn render_edges(ctx: &ProviderContext<'_>, edges: &[Edge], direction: &str) -> Vec<RenderedEdge> {
    edges
        .iter()
        .map(|edge| RenderedEdge {
            kind: edge.kind.clone(),
            source: edge.source.to_string(),
            source_label: compact_label(ctx, &edge.source, None),
            target: edge.target.to_string(),
            target_label: compact_label(ctx, &edge.target, None),
            direction: direction.to_string(),
            properties: edge.metadata.clone(),
        })
        .collect()
}

pub fn compact_label(
    ctx: &ProviderContext<'_>,
    r: &EntityRef,
    loaded: Option<&BTreeMap<String, String>>,
) -> Option<String> {
    entity_loader::compact_label(ctx, r, loaded)
}

const NEXT_HOP_DISPLAY_CAP: usize = 5;

/// Render the provider's recommended hops under a display cap that authored
/// hints are exempt from.
///
/// The cap exists to stop a long observed-family tail from burying the useful
/// hops. An authored schema hint is the opposite of that tail: the author
/// declared it worth following, so truncating one would drop exactly the signal
/// the cap is meant to protect. Everything else fills up to the cap.
fn render_next_hops(hops: Vec<NextHop>) -> Vec<RenderedNextHop> {
    let authored_count = hops.iter().filter(|hop| hop.authored).count();
    let budget = NEXT_HOP_DISPLAY_CAP.max(authored_count);
    let mut rendered = Vec::new();
    for hop in hops {
        if !hop.authored && (hop.count == 0 || rendered.len() >= budget) {
            continue;
        }
        rendered.push(RenderedNextHop {
            edge_family: hop.edge_family_name,
            count: hop.count,
            direction: hop
                .direction
                .map(|direction| direction.as_str().to_string()),
            label: hop.label,
        });
    }
    rendered
}

/// One line of the human "Recommended next hops" section.
///
/// A direction-aware hop renders the arrow, the label, and the exact
/// `edge_types` / `direction` arguments to pass straight back into
/// `bbox_inspect_entity`: the point of the hint is that the reader does not
/// have to assemble the follow-up call. A zero-count authored hint renders
/// `(none)` rather than `(0)`, because "there are no open findings" is the
/// answer, not a missing number. A direction-blind hop keeps the older
/// `family (count)` shape.
#[cfg(test)]
fn next_hop_line(hop: &RenderedNextHop) -> String {
    let Some(direction) = hop.direction.as_deref() else {
        return format!("  {} ({})\n", hop.edge_family, hop.count);
    };
    let arrow = if direction == "out" { "-->" } else { "<--" };
    let label = hop
        .label
        .as_deref()
        .map(|label| format!(" {label}"))
        .unwrap_or_default();
    let count = if hop.count == 0 {
        "none".to_string()
    } else {
        hop.count.to_string()
    };
    format!(
        "  {arrow}[{}]{label} ({count})  inspect: edge_types=\"{}\" direction=\"{direction}\"\n",
        hop.edge_family, hop.edge_family
    )
}

pub(super) fn is_identity_property(key: &str) -> bool {
    key.ends_with("_id")
        || key.ends_with("_ref")
        || key.ends_with("_generation")
        || key.ends_with("_version")
        || key.ends_with("_hash")
        || key.starts_with("evidence.")
        || matches!(
            key,
            "ref"
                | "sha"
                | "commit_sha"
                | "version"
                | "fingerprint"
                | "updated_at"
                | "observed_at"
                | "source_uri"
                | "relative_path"
                | "generation"
                | "graph_source"
                | "graph_source_connector"
                | "provisional"
                | "visibility"
                | "freshness"
        )
}

fn render_properties(entity: &EntityView, property_mode: PropertyMode) -> BTreeMap<String, String> {
    let summary_keys = ["name", "title", "status", "kind", "severity", "id"];
    entity
        .properties
        .iter()
        .filter(|(key, _)| {
            property_mode != PropertyMode::Summary
                || summary_keys.contains(&key.as_str())
                || is_identity_property(key)
        })
        .map(|(key, value)| {
            let value = match property_mode {
                PropertyMode::Full => value.clone(),
                PropertyMode::Smart
                    if !is_identity_property(key) && value.chars().count() > 300 =>
                {
                    format!("{}...", value.chars().take(300).collect::<String>())
                }
                _ => value.clone(),
            };
            (key.clone(), value)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hop(
        family: &str,
        count: usize,
        direction: Option<providers::NextHopDirection>,
        label: Option<&str>,
    ) -> NextHop {
        NextHop {
            edge_family_name: family.to_string(),
            count,
            direction,
            label: label.map(str::to_string),
            authored: label.is_some(),
        }
    }

    #[test]
    fn a_directed_labeled_hop_renders_the_call_it_wants_you_to_make() {
        let rendered = render_next_hops(vec![hop(
            "pgc:LICENSED_BY",
            1,
            Some(providers::NextHopDirection::Out),
            Some("licensing"),
        )]);
        assert_eq!(
            next_hop_line(&rendered[0]),
            "  -->[pgc:LICENSED_BY] licensing (1)  inspect: edge_types=\"pgc:LICENSED_BY\" direction=\"out\"\n"
        );
    }

    #[test]
    fn a_zero_count_authored_hop_says_none_rather_than_zero() {
        let rendered = render_next_hops(vec![hop(
            "pgc:DIAGNOSES",
            0,
            Some(providers::NextHopDirection::In),
            Some("open findings"),
        )]);
        assert_eq!(
            next_hop_line(&rendered[0]),
            "  <--[pgc:DIAGNOSES] open findings (none)  inspect: edge_types=\"pgc:DIAGNOSES\" direction=\"in\"\n"
        );
    }

    #[test]
    fn a_direction_blind_hop_keeps_the_bare_family_count_line() {
        let rendered = render_next_hops(vec![hop("DERIVED_FROM", 3, None, None)]);
        assert_eq!(next_hop_line(&rendered[0]), "  DERIVED_FROM (3)\n");
        let value = serde_json::to_value(&rendered[0]).unwrap();
        assert_eq!(
            value,
            serde_json::json!({"edge_family": "DERIVED_FROM", "count": 3}),
            "direction and label are omitted, not emitted as null"
        );
    }

    #[test]
    fn the_display_cap_never_truncates_an_authored_hop() {
        let mut hops = (0..7)
            .map(|idx| {
                hop(
                    &format!("gov:AUTHORED_{idx}"),
                    idx,
                    Some(providers::NextHopDirection::Out),
                    Some("authored"),
                )
            })
            .collect::<Vec<_>>();
        hops.extend((0..4).map(|idx| hop(&format!("observed:{idx}"), 1, None, None)));
        let rendered = render_next_hops(hops);
        assert_eq!(
            rendered.len(),
            7,
            "all 7 authored hops survive and the observed tail is squeezed out"
        );
        assert!(
            rendered
                .iter()
                .all(|hop| hop.edge_family.starts_with("gov:AUTHORED_"))
        );
    }

    #[test]
    fn the_display_cap_still_bounds_an_unauthored_tail() {
        let hops = (0..9)
            .map(|idx| hop(&format!("observed:{idx}"), 1, None, None))
            .collect::<Vec<_>>();
        assert_eq!(render_next_hops(hops).len(), 5);
    }

    #[test]
    fn property_and_edge_continuation_fields_reject_misspellings() {
        assert!(
            serde_json::from_value::<InspectEntityParams>(json!({
                "entity_ref":"knowledge:example", "edge_cusror":"wrong",
            }))
            .is_err()
        );
    }

    #[test]
    fn invalid_options_report_the_actual_field_before_loading_an_entity() {
        let mut params = InspectEntityParams {
            edge_cursor: None,
            property: None,
            property_cursor: None,
            property_limit: None,
            entity_ref: "knowledge:missing".into(),
            provisional: None,
            edge_types: None,
            direction: Some("sideways".into()),
            per_type_limit: None,
            property_mode: None,
        };
        let entity = EntityRef::parse(&params.entity_ref).unwrap();
        for field in ["direction", "property_mode"] {
            if field == "property_mode" {
                params.direction = None;
                params.property_mode = Some("smrat".into());
            }
            let wire: serde_json::Value = serde_json::from_str(
                &inspect_entity(
                    &params,
                    &ProviderContext::empty_for_tests(),
                    &entity,
                    &EdgeIndex::default(),
                )
                .unwrap(),
            )
            .unwrap();
            assert_eq!(wire["status"], "error.bad_input");
            assert_eq!(wire["error"]["field"], field);
            assert!(
                wire["error"]["suggested_fix"]
                    .as_str()
                    .unwrap()
                    .contains(field)
            );
        }
    }

    #[test]
    fn generic_zero_count_hops_do_not_hide_authored_absences() {
        let rendered = render_next_hops(vec![
            hop("OPTIONAL_GENERIC", 0, None, None),
            hop(
                "OPEN_FINDINGS",
                0,
                Some(providers::NextHopDirection::In),
                Some("open findings"),
            ),
        ]);
        assert_eq!(rendered.len(), 1);
        assert_eq!(rendered[0].edge_family, "OPEN_FINDINGS");
        assert_eq!(rendered[0].count, 0);
        assert_eq!(rendered[0].direction.as_deref(), Some("in"));
    }

    #[test]
    fn inspection_edge_budget_is_aggregate_balanced_and_honest_about_scope() {
        use bbox_chunker::{EdgeConfidence, EdgeProvenance};
        let center = EntityRef::parse("knowledge:center").unwrap();
        let edges: Vec<Edge> = (0..150)
            .map(|idx| Edge {
                source: center.clone(),
                target: EntityRef::parse(&format!("knowledge:item{idx}")).unwrap(),
                kind: format!("FAMILY_{}", idx / 10),
                provenance: EdgeProvenance::Derived,
                confidence: EdgeConfidence::Exact,
                metadata: Default::default(),
                project_id: None,
            })
            .collect();
        let full = Neighborhood {
            forward: edges.clone(),
            reverse: edges
                .into_iter()
                .map(|mut edge| {
                    std::mem::swap(&mut edge.source, &mut edge.target);
                    edge
                })
                .collect(),
        };
        let (preview, page) = page_neighborhood(
            &full,
            "knowledge:center",
            &BTreeMap::new(),
            InspectDirection::Both,
            None,
            5,
            None,
        )
        .unwrap();
        assert_eq!(preview.forward.len(), 50);
        assert_eq!(preview.reverse.len(), 50);
        let page = page.unwrap();
        assert_eq!(page["matching"], 300);
        assert_eq!(page["returned"], 100);
        assert!(page["next_cursor"].is_string());

        let filter = HashSet::from(["FAMILY_0".into()]);
        let (targeted, page) = page_neighborhood(
            &full,
            "knowledge:center",
            &BTreeMap::new(),
            InspectDirection::In,
            Some(&filter),
            5,
            None,
        )
        .unwrap();
        assert!(targeted.forward.is_empty());
        let page = page.unwrap();
        assert_eq!(page["matching"], 10);
        assert_eq!(page["returned"], 5);
    }

    #[test]
    fn explicit_per_family_limit_above_fifty_is_honored_within_aggregate_cap() {
        use bbox_chunker::{EdgeConfidence, EdgeProvenance};
        let full = Neighborhood {
            forward: (0..90)
                .map(|idx| Edge {
                    source: EntityRef::parse("knowledge:center").unwrap(),
                    target: EntityRef::parse(&format!("knowledge:item{idx}")).unwrap(),
                    kind: "RELATED_TO".into(),
                    provenance: EdgeProvenance::Derived,
                    confidence: EdgeConfidence::Exact,
                    metadata: Default::default(),
                    project_id: None,
                })
                .collect(),
            reverse: Vec::new(),
        };
        let (preview, page) = page_neighborhood(
            &full,
            "knowledge:center",
            &BTreeMap::new(),
            InspectDirection::Out,
            None,
            75,
            None,
        )
        .unwrap();
        assert_eq!(preview.forward.len(), 75);
        let page = page.unwrap();
        assert_eq!(page["matching"], 90);
        assert_eq!(page["returned"], 75);
        let (tail, last_page) = page_neighborhood(
            &full,
            "knowledge:center",
            &BTreeMap::new(),
            InspectDirection::Out,
            None,
            75,
            page["next_cursor"].as_str(),
        )
        .unwrap();
        assert_eq!(tail.forward.len(), 15);
        assert!(last_page.unwrap().get("next_cursor").is_none());
    }

    #[test]
    fn edge_continuation_visits_every_selected_edge_once_and_rejects_changed_selection() {
        use bbox_chunker::{EdgeConfidence, EdgeProvenance};
        let mut full = Neighborhood {
            forward: (0..315)
                .map(|index| Edge {
                    source: EntityRef::parse("knowledge:source").unwrap(),
                    target: EntityRef::parse(&format!("knowledge:target{index}")).unwrap(),
                    kind: format!("FAMILY_{}", index % 21),
                    provenance: EdgeProvenance::Derived,
                    confidence: EdgeConfidence::Exact,
                    metadata: BTreeMap::from([("evidence.freshness".into(), "stale".into())]),
                    project_id: None,
                })
                .collect(),
            reverse: Vec::new(),
        };
        let properties = BTreeMap::from([("generation".into(), "generation-1".into())]);
        let mut cursor: Option<String> = None;
        let mut seen = HashSet::new();
        loop {
            let (page, metadata) = page_neighborhood(
                &full,
                "knowledge:source",
                &properties,
                InspectDirection::Out,
                None,
                5,
                cursor.as_deref(),
            )
            .unwrap();
            assert!(page.forward.len() <= TOTAL_EDGE_CAP);
            for edge in page.forward {
                assert_eq!(edge.metadata["evidence.freshness"], "stale");
                assert!(
                    seen.insert(edge.target.to_string()),
                    "duplicate across pages"
                );
            }
            cursor = metadata.and_then(|meta| meta["next_cursor"].as_str().map(str::to_string));
            if cursor.is_none() {
                break;
            }
            // Input enumeration order cannot invalidate or reorder continuation.
            full.forward.reverse();
        }
        assert_eq!(seen.len(), 315);
        let (_, first) = page_neighborhood(
            &full,
            "knowledge:source",
            &properties,
            InspectDirection::Out,
            None,
            5,
            None,
        )
        .unwrap();
        let first = first.unwrap();
        let cursor = first["next_cursor"].as_str();
        let mut ticking_properties = properties.clone();
        ticking_properties.insert("elapsed".into(), "updated display value".into());
        assert!(
            page_neighborhood(
                &full,
                "knowledge:source",
                &ticking_properties,
                InspectDirection::Out,
                None,
                5,
                cursor
            )
            .is_ok()
        );
        assert!(
            page_neighborhood(
                &full,
                "knowledge:source",
                &properties,
                InspectDirection::In,
                None,
                5,
                cursor
            )
            .is_err()
        );
        assert!(
            page_neighborhood(
                &full,
                "knowledge:source",
                &properties,
                InspectDirection::Out,
                None,
                6,
                cursor
            )
            .is_err()
        );
        full.forward[0]
            .metadata
            .insert("evidence.freshness".into(), "current".into());
        assert!(
            page_neighborhood(
                &full,
                "knowledge:source",
                &properties,
                InspectDirection::Out,
                None,
                5,
                cursor
            )
            .is_err()
        );
    }

    #[test]
    fn property_body_pages_are_exact_utf8_and_revision_bound() {
        let reference = EntityRef::parse("knowledge:example").unwrap();
        let original = "🦀 café\n".repeat(1800);
        let mut entity = EntityView {
            ref_string: reference.to_string(),
            entity_type: reference.entity_type(),
            properties: BTreeMap::from([
                ("content".into(), original.clone()),
                ("generation".into(), "v1".into()),
                ("source_uri".into(), "blackbox://knowledge/example".into()),
            ]),
            neighborhood: Neighborhood::default(),
            next_hop_hints: Vec::new(),
        };
        let mut params = InspectEntityParams {
            entity_ref: reference.to_string(),
            provisional: None,
            edge_types: None,
            direction: None,
            per_type_limit: None,
            property_mode: None,
            edge_cursor: None,
            property: Some("content".into()),
            property_cursor: None,
            property_limit: Some(101),
        };
        let mut reconstructed = String::new();
        loop {
            let page: serde_json::Value = serde_json::from_str(
                &property_body_response(&params, &entity, &reference, "content").unwrap(),
            )
            .unwrap();
            assert_eq!(page["source"]["generation"], "v1");
            assert!(page.get("edges").is_none());
            let text = page["body"]["text"].as_str().unwrap();
            assert!(text.len() <= 101);
            reconstructed.push_str(text);
            params.property_cursor = page["body"]["next_cursor"].as_str().map(str::to_string);
            if params.property_cursor.is_none() {
                break;
            }
        }
        assert_eq!(reconstructed, original);
        let first: serde_json::Value = serde_json::from_str(
            &property_body_response(&params, &entity, &reference, "content").unwrap(),
        )
        .unwrap();
        params.property_cursor = first["body"]["next_cursor"].as_str().map(str::to_string);
        entity
            .properties
            .insert("elapsed".into(), "updated display value".into());
        let continued: serde_json::Value = serde_json::from_str(
            &property_body_response(&params, &entity, &reference, "content").unwrap(),
        )
        .unwrap();
        assert_eq!(continued["status"], "ok");
        entity.properties.insert("generation".into(), "v2".into());
        let rejected: serde_json::Value = serde_json::from_str(
            &property_body_response(&params, &entity, &reference, "content").unwrap(),
        )
        .unwrap();
        assert_eq!(rejected["status"], "error.bad_input");
        assert_eq!(rejected["error"]["field"], "property_cursor");
        params.property_cursor = None;
        entity.properties.insert("content".into(), String::new());
        let empty: serde_json::Value = serde_json::from_str(
            &property_body_response(&params, &entity, &reference, "content").unwrap(),
        )
        .unwrap();
        assert_eq!(empty["status"], "ok");
        assert_eq!(empty["body"]["text"], "");
        assert_eq!(empty["body"]["total_bytes"], 0);
        let missing: serde_json::Value = serde_json::from_str(
            &property_body_response(&params, &entity, &reference, "absent").unwrap(),
        )
        .unwrap();
        assert_eq!(missing["status"], "error.not_found");
    }

    #[test]
    fn property_projection_preserves_authority_and_counts_unicode_as_characters() {
        let entity = EntityView {
            ref_string: "knowledge:example".into(),
            entity_type: bbox_corpus_core::entity_ref::EntityType::Knowledge,
            properties: BTreeMap::from([
                ("title".into(), "Example".into()),
                ("content".into(), "語".repeat(200)),
                ("graph_generation".into(), "g".repeat(400)),
                ("graph_source".into(), "published".into()),
                ("evidence.freshness".into(), "stale".into()),
            ]),
            neighborhood: Neighborhood::default(),
            next_hop_hints: Vec::new(),
        };
        assert_eq!(
            render_properties(&entity, PropertyMode::Smart),
            entity.properties
        );
        let summary = render_properties(&entity, PropertyMode::Summary);
        assert!(!summary.contains_key("content"));
        assert_eq!(summary["evidence.freshness"], "stale");
        assert_eq!(summary["graph_generation"], "g".repeat(400));
        assert_eq!(summary["graph_source"], "published");
        assert_eq!(
            render_properties(&entity, PropertyMode::Full),
            entity.properties
        );
    }

    #[test]
    fn bad_input_uses_design_error_shape() {
        let rendered = bad_input("not-a-ref", "invalid ref");
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(value["status"], "error.bad_input");
        assert_eq!(value["error"]["code"], "error.bad_input");
        assert_eq!(value["error"]["field"], "entity_ref");
    }

    #[test]
    fn not_found_includes_similar_refs_field() {
        let r = EntityRef::parse("knowledge:missing").unwrap();
        let rendered = not_found(&r, vec!["knowledge:nearby".to_string()]);
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(value["status"], "error.not_found");
        assert_eq!(value["error"]["code"], "error.not_found");
        assert_eq!(value["error"]["similar_refs"][0], "knowledge:nearby");
    }

    #[test]
    fn inspect_system_memory_ref() {
        bbox_system_memory::init_for_tests_from(std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../system-defaults/memories"
        )));
        let params = InspectEntityParams {
            edge_cursor: None,
            property: None,
            property_cursor: None,
            property_limit: None,
            entity_ref: "system_memory:sm-agentic-opening-sequence".into(),
            provisional: None,
            edge_types: None,
            direction: None,
            per_type_limit: Some(0),
            property_mode: Some("summary".into()),
        };
        let r = EntityRef::parse(&params.entity_ref).unwrap();
        let rendered = inspect_entity(
            &params,
            &ProviderContext::empty_for_tests(),
            &r,
            &EdgeIndex::default(),
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();

        assert_eq!(value["status"], "ok");
        assert!(value.get("text").is_none());
        assert!(value["property_projection"]["omitted"].as_u64().unwrap() > 0);
        assert!(
            value["property_projection"]["omitted_keys"]
                .as_array()
                .unwrap()
                .iter()
                .any(|key| key == "content")
        );
        assert!(value["property_projection"]["expand_hint"].is_string());
        assert_eq!(
            value["entity_ref"],
            "system_memory:sm-agentic-opening-sequence"
        );
        assert_eq!(value["properties"]["id"], "sm-agentic-opening-sequence");
        assert!(value["properties"].get("content").is_none());
    }

    #[test]
    fn inspect_entity_synthesizes_transcript_in_session_edge() {
        // gap-edc84378: a transcript ref with zero materialized edges must
        // still surface an IN_SESSION out-edge via
        // EdgeIndex::forward_edges_with_synthesis, and the (required) edge
        // family coverage row must report it present.
        let params = InspectEntityParams {
            edge_cursor: None,
            property: None,
            property_cursor: None,
            property_limit: None,
            entity_ref: "transcript:claude:sess-1:42:0".into(),
            provisional: None,
            edge_types: None,
            direction: None,
            per_type_limit: None,
            property_mode: Some("summary".into()),
        };
        let r = EntityRef::parse(&params.entity_ref).unwrap();
        let rendered = inspect_entity(
            &params,
            &ProviderContext::empty_for_tests(),
            &r,
            &EdgeIndex::default(),
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();

        assert_eq!(value["status"], "ok");
        let out_edges = value["edges"]["out"].as_array().unwrap();
        assert!(
            out_edges
                .iter()
                .any(|edge| edge["kind"] == "IN_SESSION"
                    && edge["target"] == "session:claude:sess-1"),
            "expected synthesized IN_SESSION out-edge, got {out_edges:?}"
        );
        let coverage = value["edge_family_coverage"].as_array().unwrap();
        assert!(
            coverage
                .iter()
                .any(|row| row["family"] == "IN_SESSION" && row["count"] == 1),
            "expected IN_SESSION coverage row with count=1, got {coverage:?}"
        );
    }

    #[test]
    fn edge_family_coverage_omits_optional_zero_count_rows() {
        // An entity with no edges in the index: every optional expected family
        // resolves to count 0. Those rows are padding and must not reach the
        // structured payload; only present (count > 0) or required families do.
        bbox_system_memory::init_for_tests_from(std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../system-defaults/memories"
        )));
        let params = InspectEntityParams {
            edge_cursor: None,
            property: None,
            property_cursor: None,
            property_limit: None,
            entity_ref: "system_memory:sm-agentic-opening-sequence".into(),
            provisional: None,
            edge_types: None,
            direction: None,
            per_type_limit: Some(0),
            property_mode: Some("summary".into()),
        };
        let r = EntityRef::parse(&params.entity_ref).unwrap();
        let rendered = inspect_entity(
            &params,
            &ProviderContext::empty_for_tests(),
            &r,
            &EdgeIndex::default(),
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        let coverage = value["edge_family_coverage"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        for row in coverage {
            let count = row["count"].as_u64().unwrap();
            let expected = row["expected"].as_str().unwrap();
            assert!(
                count > 0 || expected == "required",
                "optional zero-count family leaked into structured coverage: {row}"
            );
        }
    }
}
