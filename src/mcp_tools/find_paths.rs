use std::collections::{HashSet, VecDeque};

use anyhow::Result;
use rmcp::schemars;
use serde::Deserialize;
use serde_json::json;

use crate::edge_index::EdgeIndex;
use crate::entity_ref::{EntityRef, EntityType};
use crate::mcp_tools::inspect::compact_label;
use crate::path_cache::{CachedPath, PROCESS_SESSION_KEY, PathCache, PathDirection, PathStep};
use crate::providers::ProviderContext;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FindPathsParams {
    pub from: String,
    pub to: Option<String>,
    pub to_type: Option<String>,
    pub edge_types: Option<EdgeTypesParam>,
    pub max_depth: Option<usize>,
    pub limit: Option<usize>,
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
    let from = match EntityRef::parse(&p.from) {
        Ok(from) => from,
        Err(err) => return Ok(bad_input("from", err.to_string())),
    };
    let to = match p.to.as_deref().map(EntityRef::parse).transpose() {
        Ok(to) => to,
        Err(err) => return Ok(bad_input("to", err.to_string())),
    };
    let to_type = if let Some(raw) = p.to_type.as_deref() {
        match EntityType::from_prefix(raw) {
            Some(to_type) => Some(to_type),
            None => return Ok(bad_input("to_type", "unknown entity type")),
        }
    } else {
        None
    };
    let max_depth = p.max_depth.unwrap_or(3);
    if !(1..=5).contains(&max_depth) {
        return Ok(bad_input("max_depth", "max_depth must be between 1 and 5"));
    }
    let limit = p.limit.unwrap_or(5);
    if !(1..=30).contains(&limit) {
        return Ok(bad_input("limit", "limit must be between 1 and 30"));
    }
    let edge_filter = parse_edge_filter(p.edge_types.as_ref());
    let paths = bfs(
        edge_index,
        from,
        to.as_ref(),
        to_type,
        edge_filter.as_ref(),
        max_depth,
        limit,
    );
    let cached = cache.insert_paths(PROCESS_SESSION_KEY, paths);
    Ok(render_response(ctx, &cached))
}

fn bfs(
    edge_index: &EdgeIndex,
    from: EntityRef,
    to: Option<&EntityRef>,
    to_type: Option<EntityType>,
    edge_filter: Option<&HashSet<String>>,
    max_depth: usize,
    limit: usize,
) -> Vec<Vec<PathStep>> {
    let mut queue = VecDeque::new();
    let mut visited = HashSet::new();
    visited.insert(from.clone());
    queue.push_back(QueueEntry {
        current: from,
        steps: Vec::new(),
        visited,
    });
    let mut found = Vec::new();
    while let Some(entry) = queue.pop_front() {
        if entry.steps.len() >= max_depth {
            continue;
        }
        for (edge_kind, direction, next) in expansions(edge_index, &entry.current, edge_filter) {
            if entry.visited.contains(&next) {
                continue;
            }
            let mut steps = entry.steps.clone();
            steps.push(PathStep {
                from: entry.current.clone(),
                edge_kind,
                to: next.clone(),
                direction,
            });
            if to.is_some_and(|target| target == &next)
                || to_type.is_some_and(|target_type| next.entity_type() == target_type)
            {
                found.push(steps.clone());
                if found.len() >= limit {
                    return found;
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
    found
}

fn expansions(
    edge_index: &EdgeIndex,
    current: &EntityRef,
    edge_filter: Option<&HashSet<String>>,
) -> Vec<(String, PathDirection, EntityRef)> {
    let mut out = Vec::new();
    for edge in edge_index.forward_edges(current) {
        if edge_filter.is_none_or(|allowed| allowed.contains(&edge.kind)) {
            out.push((edge.kind.clone(), PathDirection::Out, edge.target.clone()));
        }
    }
    for edge in edge_index.reverse_edges(current) {
        if edge_filter.is_none_or(|allowed| allowed.contains(&edge.kind)) {
            out.push((edge.kind.clone(), PathDirection::In, edge.source.clone()));
        }
    }
    out
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

fn render_response(ctx: &ProviderContext<'_>, paths: &[CachedPath]) -> String {
    let text = if paths.is_empty() {
        "No paths found.".to_string()
    } else {
        paths
            .iter()
            .map(|path| format!("{}: {}", path.id, render_path(ctx, path)))
            .collect::<Vec<_>>()
            .join("\n")
    };
    serde_json::to_string_pretty(&json!({
        "status": "ok",
        "text": text,
        "paths": paths.iter().map(|path| json!({
            "id": path.id,
            "summary": render_path(ctx, path),
            "steps": path.steps,
        })).collect::<Vec<_>>(),
    }))
    .expect("path response serializes")
}

pub(crate) fn render_path(ctx: &ProviderContext<'_>, path: &CachedPath) -> String {
    path.steps
        .iter()
        .map(|step| {
            let from = render_node(ctx, &step.from);
            let to = render_node(ctx, &step.to);
            match step.direction {
                PathDirection::Out => format!("{from} --{}--> {to}", step.edge_kind),
                PathDirection::In => format!("{from} <--{}-- {to}", step.edge_kind),
            }
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

pub(crate) fn render_node(ctx: &ProviderContext<'_>, r: &EntityRef) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunker::{EdgeConfidence, EdgeProvenance};
    use crate::edge_index::Edge;

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
        };
        let index = EdgeIndex::from_edges_for_tests(vec![edge]);
        let paths = bfs(&index, a, Some(&b), None, None, 3, 5);
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0][0].direction, PathDirection::Out);
    }
}
