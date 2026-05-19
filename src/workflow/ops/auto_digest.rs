use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

use anyhow::{Result, anyhow, bail};
use serde_json::{Map, Value, json};

use crate::chunker::{EdgeConfidence, EdgeProvenance};
use crate::entity_ref::EntityRef;
use crate::workflow::context::ArcContext;

use super::{OpEffect, call_blackbox_tool};

pub(super) async fn exec_read_session(
    args: &Value,
    into_var: Option<&str>,
    ctx: &ArcContext,
) -> Result<OpEffect> {
    let session_id = args
        .get("session_id")
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow!("ReadSession requires args.session_id"))?;
    let limit = args
        .get("limit")
        .and_then(|value| value.as_u64())
        .unwrap_or(200);
    let result = call_blackbox_tool(
        "bbox_messages",
        json!({
            "session_id": session_id,
            "limit": limit,
            "max_content_length": 12000u64,
        }),
        ctx,
    )
    .await?;
    Ok(OpEffect::SetVar {
        key: into_var.unwrap_or("session").to_string(),
        value: result,
    })
}

pub(super) fn exec_validate_schema(args: &Value, into_var: Option<&str>) -> Result<OpEffect> {
    let input = args
        .get("from")
        .ok_or_else(|| anyhow!("ValidateSchema requires args.from"))?;
    let candidate = first_candidate(input)
        .ok_or_else(|| anyhow!("ValidateSchema requires a candidate object or candidates array"))?;
    let required = [
        "title",
        "content",
        "category",
        "scope",
        "source_session",
        "source_query",
        "justification",
        "suggested_approval",
    ];
    for field in required {
        if candidate.get(field).and_then(Value::as_str).is_none() {
            bail!("ValidateSchema candidate missing string field '{field}'");
        }
    }
    let source_files = candidate
        .get("source_files")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("ValidateSchema candidate missing source_files array"))?;
    if source_files.iter().any(|value| value.as_str().is_none()) {
        bail!("ValidateSchema source_files must contain only strings");
    }
    Ok(OpEffect::SetVar {
        key: into_var.unwrap_or("candidate").to_string(),
        value: Value::Object(candidate.clone()),
    })
}

pub(super) async fn exec_apply_entry(
    args: &Value,
    into_var: Option<&str>,
    ctx: &ArcContext,
) -> Result<OpEffect> {
    let candidate = first_candidate(
        args.get("from")
            .ok_or_else(|| anyhow!("ApplyEntry requires args.from"))?,
    )
    .ok_or_else(|| anyhow!("ApplyEntry requires a candidate object or candidates array"))?;
    let category = candidate
        .get("category")
        .and_then(Value::as_str)
        .unwrap_or("memory");
    let title = candidate.get("title").and_then(Value::as_str);
    let content = candidate
        .get("content")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("ApplyEntry candidate missing content"))?;
    let scope = candidate
        .get("scope")
        .and_then(Value::as_str)
        .unwrap_or("project");
    let project = candidate
        .get("project")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .or_else(|| ctx.meta.project_dir.clone());
    let mut arguments = Map::new();
    arguments.insert("content".into(), Value::String(content.to_string()));
    if let Some(title) = title {
        arguments.insert("title".into(), Value::String(title.to_string()));
    }
    arguments.insert("scope".into(), Value::String(scope.to_string()));
    if let Some(project) = project {
        arguments.insert("project".into(), Value::String(project));
    }
    let tool = match category {
        "decision" => {
            arguments.insert(
                "rationale".into(),
                Value::String(
                    candidate
                        .get("justification")
                        .and_then(Value::as_str)
                        .unwrap_or("auto-digest candidate")
                        .to_string(),
                ),
            );
            arguments.insert("render".into(), Value::Bool(false));
            "bbox_decide"
        }
        "convention" | "workflow" => {
            arguments.insert("category".into(), Value::String(category.to_string()));
            "bbox_learn"
        }
        _ => {
            arguments.insert("category".into(), Value::String(category.to_string()));
            "bbox_remember"
        }
    };
    let result = call_blackbox_tool(tool, Value::Object(arguments), ctx).await?;
    Ok(OpEffect::SetVar {
        key: into_var.unwrap_or("applied_entry").to_string(),
        value: result,
    })
}

pub(super) async fn exec_append_knowledge_link(
    args: &Value,
    into_var: Option<&str>,
    ctx: &ArcContext,
) -> Result<OpEffect> {
    let mut arguments = Map::new();
    for key in ["source", "target", "kind"] {
        let value = args
            .get(key)
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("AppendKnowledgeLink requires args.{key}"))?;
        arguments.insert(key.into(), Value::String(value.to_string()));
    }
    for key in ["note", "source_arc", "confidence"] {
        if let Some(value) = args.get(key).and_then(Value::as_str) {
            arguments.insert(key.into(), Value::String(value.to_string()));
        }
    }
    let result = call_blackbox_tool("bbox_knowledge_link", Value::Object(arguments), ctx).await?;
    Ok(OpEffect::SetVar {
        key: into_var.unwrap_or("knowledge_link").to_string(),
        value: result,
    })
}

/// Scan the knowledge corpus for entity pairs that may warrant a semantic
/// edge (DESCRIBES or REFERENCES) but have no such edge yet.
///
/// Strategy:
/// 1. Two staggered `bbox_hybrid_search` queries seed a pool of topically
///    relevant entities.
/// 2. Consecutive pairs from the merged, deduplicated pool become candidates,
///    capped at `limit`.
/// 3. Each candidate carries entity refs, labels, and excerpts so the
///    downstream ClassifyVote actor can vote without extra lookups.
pub(super) async fn exec_extract_candidate_pairs(
    args: &Value,
    into_var: Option<&str>,
    ctx: &ArcContext,
) -> Result<OpEffect> {
    let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(50) as usize;
    let edge_kinds: Vec<String> = args
        .get("edge_kinds")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_else(|| vec!["DESCRIBES".to_string(), "REFERENCES".to_string()]);
    let edge_kind = edge_kinds
        .first()
        .map(|s| s.as_str())
        .unwrap_or("REFERENCES");

    // Two complementary queries to maximise variety in the candidate pool.
    let queries = ["knowledge convention decision", "learned assumption rule"];
    let mut pool: Vec<Value> = Vec::new();
    for query in &queries {
        let result = call_blackbox_tool(
            "bbox_hybrid_search",
            json!({ "query": query, "limit": limit }),
            ctx,
        )
        .await
        .unwrap_or(Value::Null);
        if let Some(hits) = result.get("hits").and_then(Value::as_array) {
            pool.extend(hits.iter().cloned());
        }
    }

    // Deduplicate by entity_ref before pairing.
    let mut seen: HashSet<String> = HashSet::new();
    pool.retain(|hit| {
        let r = hit
            .get("entity_ref")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        seen.insert(r)
    });

    // Build consecutive pairs up to `limit`.
    let candidates: Vec<Value> = pool
        .chunks(2)
        .take(limit)
        .filter_map(|chunk| {
            let src = chunk.first()?;
            let tgt = chunk.get(1)?;
            let src_ref = src.get("entity_ref").and_then(Value::as_str)?;
            let tgt_ref = tgt.get("entity_ref").and_then(Value::as_str)?;
            if src_ref == tgt_ref {
                return None;
            }
            Some(json!({
                "source": src_ref,
                "target": tgt_ref,
                "edge_kind": edge_kind,
                "source_label": src.get("label").and_then(Value::as_str).unwrap_or(src_ref),
                "target_label": tgt.get("label").and_then(Value::as_str).unwrap_or(tgt_ref),
                "source_excerpt": src.get("excerpt").and_then(Value::as_str).unwrap_or(""),
                "target_excerpt": tgt.get("excerpt").and_then(Value::as_str).unwrap_or(""),
            }))
        })
        .collect();

    Ok(OpEffect::SetVar {
        key: into_var.unwrap_or("candidate_pairs").to_string(),
        value: json!({
            "limit": limit,
            "scanned": pool.len(),
            "candidates": candidates,
        }),
    })
}

pub(super) fn exec_aggregate_auto_edge_votes(
    args: &Value,
    into_var: Option<&str>,
) -> Result<OpEffect> {
    let votes = args
        .get("votes")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut gate = Map::new();
    for idx in 0..3 {
        let vote = votes
            .get(idx)
            .and_then(|value| value.get("vote"))
            .and_then(Value::as_str)
            .unwrap_or("no");
        gate.insert(format!("vote{}", idx + 1), Value::String(vote.to_string()));
    }
    gate.insert("votes".into(), Value::Array(votes));
    Ok(OpEffect::SetVar {
        key: into_var.unwrap_or("vote_aggregate").to_string(),
        value: Value::Object(gate),
    })
}

pub(super) async fn exec_write_semantic_edge(
    args: &Value,
    into_var: Option<&str>,
    ctx: &ArcContext,
) -> Result<OpEffect> {
    let source = required_str(args, "source")?;
    let target = required_str(args, "target")?;
    let kind = required_str(args, "kind")?;
    let note = args.get("note").and_then(Value::as_str).unwrap_or("");
    let source_ref = EntityRef::parse(source)?;
    let target_ref = EntityRef::parse(target)?;
    match kind {
        "REFERENCES" => {
            let result = call_blackbox_tool(
                "bbox_knowledge_link",
                json!({
                    "source": source,
                    "target": target,
                    "kind": "REFERENCES",
                    "note": note,
                    "source_arc": ctx.meta.arc_id,
                    "confidence": "heuristic"
                }),
                ctx,
            )
            .await?;
            Ok(OpEffect::SetVar {
                key: into_var.unwrap_or("semantic_edge").to_string(),
                value: result,
            })
        }
        "DESCRIBES" => {
            let project_id = args
                .get("project_id")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| project_id_from_ref(&source_ref))
                .or_else(|| project_id_from_ref(&target_ref))
                .ok_or_else(|| {
                    anyhow!("WriteSemanticEdge DESCRIBES requires a project_id-bearing ref")
                })?;
            let mut metadata = BTreeMap::new();
            metadata.insert("source_arc".to_string(), ctx.meta.arc_id.clone());
            if !note.is_empty() {
                metadata.insert("note".to_string(), note.to_string());
            }
            let edge = crate::edge_index::Edge {
                source: source_ref,
                kind: "DESCRIBES".to_string(),
                target: target_ref,
                provenance: EdgeProvenance::Explicit,
                confidence: EdgeConfidence::Heuristic,
                metadata,
            };
            let edges_dir = args
                .get("edges_dir")
                .and_then(Value::as_str)
                .map(PathBuf::from)
                .unwrap_or_else(default_edges_dir);
            let written =
                crate::edge_index::append_explicit_edges(&edges_dir, &project_id, &[edge])?;
            Ok(OpEffect::SetVar {
                key: into_var.unwrap_or("semantic_edge").to_string(),
                value: json!({
                    "status": "ok",
                    "kind": "DESCRIBES",
                    "project_id": project_id,
                    "written": written
                }),
            })
        }
        other => bail!("WriteSemanticEdge unsupported kind `{other}`"),
    }
}

pub(super) async fn exec_surface_to_inbox(args: &Value, ctx: &ArcContext) -> Result<OpEffect> {
    let candidate = first_candidate(
        args.get("from")
            .ok_or_else(|| anyhow!("SurfaceToInbox requires args.from"))?,
    )
    .ok_or_else(|| anyhow!("SurfaceToInbox requires a candidate object or candidates array"))?;
    let title = candidate
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("(untitled)");
    call_blackbox_tool(
        "bbox_note",
        json!({
            "kind": "followup",
            "project": ctx.meta.project_dir,
            "body": format!("Auto-digest candidate held for review: {title}")
        }),
        ctx,
    )
    .await?;
    Ok(OpEffect::None)
}

pub(super) async fn exec_log_reject(args: &Value, ctx: &ArcContext) -> Result<OpEffect> {
    let candidate = first_candidate(
        args.get("from")
            .ok_or_else(|| anyhow!("LogReject requires args.from"))?,
    )
    .ok_or_else(|| anyhow!("LogReject requires a candidate object or candidates array"))?;
    let title = candidate
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("(untitled)");
    call_blackbox_tool(
        "bbox_note",
        json!({
            "kind": "learned",
            "project": ctx.meta.project_dir,
            "body": format!("Auto-digest candidate rejected by entry-quality gate: {title}")
        }),
        ctx,
    )
    .await?;
    Ok(OpEffect::None)
}

fn required_str<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow!("WriteSemanticEdge requires args.{key}"))
}

fn project_id_from_ref(r: &EntityRef) -> Option<String> {
    match r {
        EntityRef::ProjectFile { project_id, .. }
        | EntityRef::ProjectFileV2 { project_id, .. }
        | EntityRef::Symbol { project_id, .. }
        | EntityRef::SymbolV2 { project_id, .. } => Some(project_id.clone()),
        _ => None,
    }
}

fn default_edges_dir() -> PathBuf {
    dirs::home_dir()
        .map(|home| crate::util::blackbox_state_dir(&home).join("edges"))
        .unwrap_or_else(|| PathBuf::from("edges"))
}

fn first_candidate(value: &Value) -> Option<&Map<String, Value>> {
    if let Some(obj) = value.as_object() {
        if let Some(candidate) = obj.get("candidate").and_then(Value::as_object) {
            return Some(candidate);
        }
        if let Some(candidate) = obj
            .get("candidates")
            .and_then(Value::as_array)
            .and_then(|items| items.first())
            .and_then(Value::as_object)
        {
            return Some(candidate);
        }
        return Some(obj);
    }
    value
        .as_array()
        .and_then(|items| items.first())
        .and_then(Value::as_object)
}
