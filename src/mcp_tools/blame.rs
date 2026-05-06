use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use rmcp::schemars;
use serde::Deserialize;
use serde_json::json;

use crate::edge_index::{Edge, EdgeIndex};
use crate::entity_loader;
use crate::entity_ref::EntityRef;
use crate::git::GitBlameLine;
use crate::projects::ProjectRecord;
use crate::providers::ProviderContext;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BlameParams {
    #[serde(default)]
    pub file: Option<String>,
    #[serde(default)]
    pub line: Option<u64>,
    #[serde(default)]
    pub entity_ref: Option<String>,
}

pub fn blame(
    p: &BlameParams,
    ctx: &ProviderContext<'_>,
    edge_index: &EdgeIndex,
    projects: &[ProjectRecord],
) -> Result<String> {
    let target = match resolve_target(p, ctx, projects) {
        Ok(target) => target,
        Err(err) => return Ok(bad_input(err.to_string())),
    };
    let Some(blame) = crate::git::blame_for_line(&target.file_path, target.line)? else {
        return Ok(serde_json::to_string_pretty(&json!({
            "status": "error.not_found",
            "error": {
                "code": "error.not_found",
                "message": format!("No git blame data found for {}:{}", target.display_path, target.line),
            },
            "file": target.display_path,
            "line": target.line,
        }))?);
    };
    let anchors = matching_anchors(edge_index, &blame.commit_sha, &blame.rel_path);
    let selected = anchors.first().copied();
    let prior_reads = selected
        .map(|edge| prior_read_edges(edge_index, edge))
        .unwrap_or_default();
    let session = selected.and_then(session_from_edge);
    let brofiles = session
        .as_ref()
        .map(|session| edge_targets(edge_index.forward_edges(session), "SESSION_USED_BROFILE"))
        .unwrap_or_default();
    let threads = session
        .as_ref()
        .map(|session| edge_sources(edge_index.reverse_edges(session), "THREAD_HAS_SESSION"))
        .unwrap_or_default();
    let text = render_text(RenderInput {
        target: &target,
        blame: &blame,
        selected,
        prior_reads: &prior_reads,
        brofiles: &brofiles,
        threads: &threads,
    });
    Ok(serde_json::to_string_pretty(&json!({
        "status": "ok",
        "text": text,
        "file": target.display_path,
        "line": target.line,
        "git_blame": {
            "commit_sha": blame.commit_sha,
            "author": blame.author,
            "author_time": blame.author_time,
            "rel_path": blame.rel_path,
        },
        "bbox_anchor": selected.map(render_anchor),
        "prior_reads": prior_reads.iter().map(render_read).collect::<Vec<_>>(),
        "brofiles": brofiles.iter().map(ToString::to_string).collect::<Vec<_>>(),
        "threads": threads.iter().map(ToString::to_string).collect::<Vec<_>>(),
    }))?)
}

struct BlameTarget {
    file_path: PathBuf,
    display_path: String,
    line: u64,
}

fn resolve_target(
    p: &BlameParams,
    ctx: &ProviderContext<'_>,
    projects: &[ProjectRecord],
) -> Result<BlameTarget> {
    if let Some(entity_ref) = p.entity_ref.as_deref().filter(|value| !value.trim().is_empty()) {
        let r = EntityRef::parse(entity_ref)?;
        if !matches!(r, EntityRef::ProjectFile { .. }) {
            bail!("entity_ref must be a project_file ref");
        }
        let entity = entity_loader::load(ctx, &r)
            .with_context(|| format!("loading project_file entity {entity_ref}"))?;
        let file_path = entity
            .properties
            .get("file_path")
            .ok_or_else(|| anyhow::anyhow!("project_file entity has no file_path property"))?;
        let file_path = PathBuf::from(file_path);
        let line = match p.line {
            Some(line) => line,
            None => {
                let byte_offset = entity
                    .properties
                    .get("byte_offset")
                    .and_then(|value| value.parse::<u64>().ok())
                    .unwrap_or_default();
                line_for_byte_offset(&file_path, byte_offset)?
            }
        };
        return Ok(BlameTarget {
            display_path: display_path(&file_path, projects),
            file_path,
            line,
        });
    }

    let file = p
        .file
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("provide either entity_ref or file+line"))?;
    let line = p
        .line
        .ok_or_else(|| anyhow::anyhow!("line is required when file is provided"))?;
    Ok(BlameTarget {
        file_path: resolve_file(file, projects)?,
        display_path: file.to_string(),
        line,
    })
}

fn resolve_file(file: &str, projects: &[ProjectRecord]) -> Result<PathBuf> {
    let input = Path::new(file);
    if input.is_absolute() {
        return Ok(std::fs::canonicalize(input)?);
    }
    for project in projects {
        let candidate = Path::new(&project.canonical_path).join(input);
        if candidate.exists() {
            return Ok(std::fs::canonicalize(candidate)?);
        }
    }
    Ok(std::fs::canonicalize(input)?)
}

fn display_path(path: &Path, projects: &[ProjectRecord]) -> String {
    for project in projects {
        let root = Path::new(&project.canonical_path);
        if let Ok(rel) = path.strip_prefix(root) {
            return rel.to_string_lossy().to_string();
        }
    }
    path.to_string_lossy().to_string()
}

fn line_for_byte_offset(path: &Path, byte_offset: u64) -> Result<u64> {
    let bytes = std::fs::read(path)?;
    let upto = (byte_offset as usize).min(bytes.len());
    Ok(bytes[..upto].iter().filter(|byte| **byte == b'\n').count() as u64 + 1)
}

fn matching_anchors<'a>(
    edge_index: &'a EdgeIndex,
    commit_sha: &str,
    rel_path: &str,
) -> Vec<&'a Edge> {
    let mut exact = Vec::new();
    let mut same_commit = Vec::new();
    for edge in edge_index.edges_with_anchor_commit(commit_sha) {
        if edge
            .metadata
            .get("anchor.file_path")
            .is_some_and(|path| path == rel_path)
        {
            exact.push(edge);
        } else {
            same_commit.push(edge);
        }
    }
    if exact.is_empty() {
        same_commit
    } else {
        exact
    }
}

fn prior_read_edges<'a>(edge_index: &'a EdgeIndex, edit_edge: &Edge) -> Vec<&'a Edge> {
    let Some((provider, session_id, edit_offset, edit_event_idx)) =
        transcript_source(&edit_edge.source)
    else {
        return Vec::new();
    };
    let min_event_idx = edit_event_idx.saturating_sub(20);
    let mut reads = edge_index
        .session_tool_call_edges(provider, session_id)
        .into_iter()
        .filter(|edge| edge.kind == "READ_FILE")
        .filter(|edge| {
            transcript_source(&edge.source).is_some_and(
                |(read_provider, read_session, offset, event_idx)| {
                    read_provider == provider
                        && read_session == session_id
                        && offset < edit_offset
                        && event_idx >= min_event_idx
                        && event_idx <= edit_event_idx
                },
            )
        })
        .collect::<Vec<_>>();
    reads.sort_by_key(|edge| {
        transcript_source(&edge.source).map(|(_, _, offset, idx)| (idx, offset))
    });
    reads
        .into_iter()
        .rev()
        .take(5)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

fn transcript_source(r: &EntityRef) -> Option<(&str, &str, u64, u32)> {
    let EntityRef::Transcript {
        provider,
        session_id,
        line_offset,
        event_idx,
        ..
    } = r
    else {
        return None;
    };
    Some((provider, session_id, *line_offset, *event_idx))
}

fn session_from_edge(edge: &Edge) -> Option<EntityRef> {
    let EntityRef::Transcript {
        provider,
        session_id,
        ..
    } = &edge.source
    else {
        return None;
    };
    Some(EntityRef::Session {
        provider: provider.clone(),
        session_id: session_id.clone(),
    })
}

fn edge_targets(edges: &[Edge], kind: &str) -> Vec<EntityRef> {
    edges
        .iter()
        .filter(|edge| edge.kind == kind)
        .map(|edge| edge.target.clone())
        .collect()
}

fn edge_sources(edges: &[Edge], kind: &str) -> Vec<EntityRef> {
    edges
        .iter()
        .filter(|edge| edge.kind == kind)
        .map(|edge| edge.source.clone())
        .collect()
}

struct RenderInput<'a> {
    target: &'a BlameTarget,
    blame: &'a GitBlameLine,
    selected: Option<&'a Edge>,
    prior_reads: &'a [&'a Edge],
    brofiles: &'a [EntityRef],
    threads: &'a [EntityRef],
}

fn render_text(input: RenderInput<'_>) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "{}:{} was last edited by:\n",
        input.target.display_path, input.target.line
    ));
    out.push_str(&format!(
        "  commit {}{}  by {}\n",
        short_sha(&input.blame.commit_sha),
        input
            .blame
            .author_time
            .as_deref()
            .map(|time| format!(" ({time})"))
            .unwrap_or_default(),
        if input.blame.author.is_empty() {
            "unknown"
        } else {
            input.blame.author.as_str()
        }
    ));
    if let Some(edge) = input.selected {
        out.push_str(&format!("  bbox anchor: {}\n", edge.source));
        if !input.brofiles.is_empty() {
            out.push_str("  brofile(s):\n");
            for brofile in input.brofiles {
                out.push_str(&format!("    - {brofile}\n"));
            }
        }
        if !input.threads.is_empty() {
            out.push_str("  thread(s):\n");
            for thread in input.threads {
                out.push_str(&format!("    - {thread}\n"));
            }
        }
        if input.prior_reads.is_empty() {
            out.push_str("  informed by prior reads: none tracked\n");
        } else {
            out.push_str("  informed by prior reads of:\n");
            for read in input.prior_reads {
                let path = read
                    .metadata
                    .get("anchor.file_path")
                    .map(String::as_str)
                    .unwrap_or("<unknown>");
                out.push_str(&format!("    - {path} ({})\n", read.source));
            }
        }
    } else {
        out.push_str("  [no bbox-tracked tool call matches this commit]\n");
    }
    out
}

fn short_sha(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

fn render_anchor(edge: &Edge) -> serde_json::Value {
    json!({
        "source": edge.source.to_string(),
        "target": edge.target.to_string(),
        "metadata": edge.metadata,
    })
}

fn render_read(edge: &&Edge) -> serde_json::Value {
    json!({
        "source": edge.source.to_string(),
        "target": edge.target.to_string(),
        "file_path": edge.metadata.get("anchor.file_path"),
    })
}

fn bad_input(message: String) -> String {
    json!({
        "status": "error.bad_input",
        "error": {
            "code": "error.bad_input",
            "message": message,
            "suggested_fix": "Pass either file plus 1-based line, or entity_ref for a project_file chunk."
        }
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunker::{EdgeConfidence, EdgeProvenance};
    use std::collections::BTreeMap;

    #[test]
    fn matching_anchors_prefers_same_file() {
        let same_commit_other_file = edit_edge("src/lib.rs");
        let same_file = edit_edge("src/main.rs");
        let index =
            EdgeIndex::from_edges_for_tests(vec![same_commit_other_file, same_file.clone()]);

        let anchors = matching_anchors(&index, "abc123", "src/main.rs");

        assert_eq!(anchors.len(), 1);
        assert_eq!(anchors[0].target, same_file.target);
    }

    #[test]
    fn matching_anchors_uses_commit_anchor_index() {
        let mut edges = (0..10_000)
            .map(|idx| {
                let mut edge = edit_edge_at(&format!("src/{idx}.rs"), idx);
                edge.metadata
                    .insert("anchor.commit_sha_at_edit".into(), format!("commit-{idx}"));
                edge
            })
            .collect::<Vec<_>>();
        let mut target = edit_edge_at("src/main.rs", 10_001);
        target
            .metadata
            .insert("anchor.commit_sha_at_edit".into(), "needle".into());
        edges.push(target);
        let index = EdgeIndex::from_edges_for_tests(edges);
        let started = std::time::Instant::now();

        let anchors = matching_anchors(&index, "needle", "src/main.rs");

        assert_eq!(anchors.len(), 1);
        assert!(
            started.elapsed() < std::time::Duration::from_millis(1),
            "commit anchor lookup should be O(k), elapsed {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn prior_read_edges_uses_session_tool_call_index() {
        let edit = edit_edge_at("src/main.rs", 10_001);
        let mut edges = (0..10_000)
            .map(|idx| read_edge_for_session(&format!("src/{idx}.rs"), "other", idx))
            .collect::<Vec<_>>();
        edges.push(read_edge_at("src/auth.rs", 10_000));
        edges.push(edit.clone());
        let index = EdgeIndex::from_edges_for_tests(edges);
        let started = std::time::Instant::now();

        let reads = prior_read_edges(&index, &edit);

        assert_eq!(reads.len(), 1);
        assert!(
            started.elapsed() < std::time::Duration::from_millis(1),
            "session tool-call lookup should be O(k), elapsed {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn prior_read_edges_are_same_session_before_edit() {
        let edit = edit_edge_at("src/main.rs", 30);
        let read_before = read_edge_at("src/auth.rs", 20);
        let read_after = read_edge_at("src/later.rs", 40);
        let other_session = read_edge_for_session("src/other.rs", "other", 10);
        let index = EdgeIndex::from_edges_for_tests(vec![
            edit.clone(),
            read_before.clone(),
            read_after,
            other_session,
        ]);

        let reads = prior_read_edges(&index, &edit);

        assert_eq!(reads.len(), 1);
        assert_eq!(
            reads[0].metadata.get("anchor.file_path").map(String::as_str),
            Some("src/auth.rs")
        );
        assert_eq!(read_before.kind, "READ_FILE");
    }

    #[test]
    fn prior_read_edges_are_bounded_to_twenty_turns() {
        let edit = edit_edge_at_event("src/main.rs", 100, 100);
        let stale_read = read_edge_at_event("src/stale.rs", 80, 79);
        let recent_read = read_edge_at_event("src/recent.rs", 90, 90);
        let index = EdgeIndex::from_edges_for_tests(vec![
            edit.clone(),
            stale_read,
            recent_read.clone(),
        ]);

        let reads = prior_read_edges(&index, &edit);

        assert_eq!(reads.len(), 1);
        assert_eq!(
            reads[0].metadata.get("anchor.file_path").map(String::as_str),
            Some("src/recent.rs")
        );
    }

    fn edit_edge(path: &str) -> Edge {
        edit_edge_at(path, 10)
    }

    fn edit_edge_at(path: &str, line_offset: u64) -> Edge {
        tool_edge("EDITED_FILE", path, "sess", line_offset)
    }

    fn edit_edge_at_event(path: &str, line_offset: u64, event_idx: u32) -> Edge {
        tool_edge_at_event("EDITED_FILE", path, "sess", line_offset, event_idx)
    }

    fn read_edge_at(path: &str, line_offset: u64) -> Edge {
        read_edge_for_session(path, "sess", line_offset)
    }

    fn read_edge_at_event(path: &str, line_offset: u64, event_idx: u32) -> Edge {
        tool_edge_at_event("READ_FILE", path, "sess", line_offset, event_idx)
    }

    fn read_edge_for_session(path: &str, session_id: &str, line_offset: u64) -> Edge {
        tool_edge("READ_FILE", path, session_id, line_offset)
    }

    fn tool_edge(kind: &str, path: &str, session_id: &str, line_offset: u64) -> Edge {
        tool_edge_at_event(kind, path, session_id, line_offset, 0)
    }

    fn tool_edge_at_event(
        kind: &str,
        path: &str,
        session_id: &str,
        line_offset: u64,
        event_idx: u32,
    ) -> Edge {
        let mut metadata = BTreeMap::new();
        metadata.insert("anchor.commit_sha_at_edit".into(), "abc123".into());
        metadata.insert("anchor.file_path".into(), path.into());
        Edge {
            source: EntityRef::Transcript {
                provider: "claude".into(),
                session_id: session_id.into(),
                line_offset,
                event_idx,
            },
            kind: kind.into(),
            target: EntityRef::ProjectFile {
                project_id: "proj1234".into(),
                rel_path_hash: "pathhash".into(),
                chunk_hash: path.bytes().map(|byte| format!("{byte:02x}")).collect::<String>(),
                occurrence_idx: 0,
            },
            provenance: EdgeProvenance::Explicit,
            confidence: EdgeConfidence::Heuristic,
            metadata,
        }
    }
}
