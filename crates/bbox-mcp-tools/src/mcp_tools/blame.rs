use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use rmcp::schemars;
use serde::Deserialize;
use serde_json::json;

use bbox_corpus_core::entity_ref::EntityRef;
use bbox_corpus_core::git::GitBlameLine;
use bbox_edge_index::edge_index::{Edge, EdgeIndex};
use bbox_providers::entity_loader;
use bbox_providers::providers::ProviderContext;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BlameParams {
    #[serde(default)]
    pub file: Option<String>,
    #[serde(default)]
    pub line: Option<u64>,
    #[serde(default)]
    pub entity_ref: Option<String>,
}

/// Corpus identity and an untrusted indexed path hint extracted from caller
/// input. The daemon resolves the hint through checkout authority before
/// calling `blame`.
#[derive(Debug, Clone)]
pub enum BlameTargetIdentity {
    ProjectFile {
        project_id: String,
        indexed_path_hint: PathBuf,
        line: Option<u64>,
        byte_offset: u64,
    },
    File {
        input_path: String,
        line: u64,
    },
}

/// Which history a blame attributes lines against.
///
/// This is an enum rather than an `Option<commit>` because the two cases are
/// not a default and a refinement: a corpus-identity blame that silently ran
/// against the working tree would attribute lines to whatever the checkout
/// happens to be at now, which is the exact defect the split exists to make
/// unrepresentable.
#[derive(Debug, Clone)]
pub enum BlameSource {
    /// The checkout's current working tree, with bytes the caller already
    /// read through its validated lease. Legitimate only for a blame the
    /// caller addressed BY PATH: such a request names no corpus snapshot, so
    /// current history is the only history it could mean.
    WorkingTree { content: Vec<u8> },
    /// One exact commit. Corpus-identity blame must use this: the lines it
    /// attributes have to come from the snapshot the corpus indexed.
    ///
    /// The committed file is read here rather than supplied, so no caller can
    /// pin the revision while passing working-tree bytes and get a line
    /// number resolved against content the commit never had.
    Snapshot { commit: String },
}

/// Caller-supplied file path produced by a validated checkout lease. The
/// caller retains that lease for the complete blame operation.
#[derive(Debug, Clone)]
pub struct ValidatedBlameTarget {
    pub git_root: PathBuf,
    pub git_relative_path: PathBuf,
    pub display_path: String,
    pub line: Option<u64>,
    pub byte_offset: Option<u64>,
    pub source: BlameSource,
}

pub fn target_identity(p: &BlameParams, ctx: &ProviderContext<'_>) -> Result<BlameTargetIdentity> {
    if let Some(entity_ref) = p
        .entity_ref
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        let r = EntityRef::parse(entity_ref)?;
        let project_id = match &r {
            EntityRef::ProjectFile { project_id, .. }
            | EntityRef::ProjectFileV2 { project_id, .. } => project_id.clone(),
            _ => bail!("entity_ref must be a project_file ref"),
        };
        let entity = entity_loader::load(ctx, &r)
            .with_context(|| format!("loading project_file entity {entity_ref}"))?;
        // P3-E: `relative_path` is the authority. `file_path` remains only as
        // the pre-bump compat source for a ref resolved against a document
        // written under the old schema.
        let indexed_path_hint = entity
            .properties
            .get("relative_path")
            .or_else(|| entity.properties.get("file_path"))
            .map(PathBuf::from)
            .ok_or_else(|| anyhow::anyhow!("project_file entity has no relative_path property"))?;
        let byte_offset = entity
            .properties
            .get("byte_offset")
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or_default();
        return Ok(BlameTargetIdentity::ProjectFile {
            project_id,
            indexed_path_hint,
            line: p.line,
            byte_offset,
        });
    }

    let input_path = p
        .file
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("provide either entity_ref or file+line"))?
        .to_string();
    let line = p
        .line
        .ok_or_else(|| anyhow::anyhow!("line is required when file is provided"))?;
    Ok(BlameTargetIdentity::File { input_path, line })
}

/// A repository-relative path safe to hand to Git: non-empty, relative, and
/// free of traversal or root components.
fn git_safe_relative(path: &std::path::Path) -> Result<String> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        anyhow::bail!("error.blame_path_invalid: blame path must be a safe relative path");
    }
    Ok(path.to_string_lossy().replace('\\', "/"))
}

/// Blame one line at an exact commit, reading the file as that commit had it.
///
/// Both halves are pinned deliberately. Blaming at the commit without reading
/// the committed file would resolve a byte offset against working-tree bytes
/// and land on the wrong line; reading the committed file without pinning the
/// blame would attribute that line to current history.
fn blame_at_commit(
    git_root: &std::path::Path,
    git_relative_path: &std::path::Path,
    commit: &str,
    line: Option<u64>,
    byte_offset: Option<u64>,
) -> Result<(u64, Option<GitBlameLine>)> {
    let rel_path = git_safe_relative(git_relative_path)?;
    let content = bbox_corpus_core::git::read_committed_file_bytes(git_root, commit, &rel_path)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "error.blame_snapshot_unavailable: the corpus snapshot commit does not contain this file"
            )
        })?;
    let line = match line {
        Some(line) => line,
        None => line_for_byte_offset(&content, byte_offset.unwrap_or_default()),
    };
    if line == 0 {
        anyhow::bail!("error.blame_path_invalid: line must be 1-based");
    }
    let line_spec = format!("{line},{line}");
    let output = bbox_corpus_core::git::git_output(
        git_root,
        &[
            "blame",
            "--porcelain",
            "-L",
            &line_spec,
            commit,
            "--",
            &rel_path,
        ],
        "running git blame at the corpus snapshot commit",
    )
    .ok_or_else(|| {
        anyhow::anyhow!(
            "error.checkout_io_failed: git blame could not read the corpus snapshot commit"
        )
    })?;
    if !output.status.success() {
        return Ok((line, None));
    }
    let parsed = bbox_corpus_core::git::parse_blame_porcelain(
        &output.stdout,
        git_root.to_path_buf(),
        rel_path,
    )
    .map_err(|_| {
        anyhow::anyhow!("error.checkout_io_failed: git blame output could not be parsed")
    })?;
    Ok((line, parsed))
}

pub fn blame(target: ValidatedBlameTarget, edge_index: &EdgeIndex) -> Result<String> {
    let (line, blame) = match &target.source {
        BlameSource::WorkingTree { content } => {
            let line = match target.line {
                Some(line) => line,
                None => line_for_byte_offset(content, target.byte_offset.unwrap_or_default()),
            };
            let blame = bbox_corpus_core::git::blame_for_line_in_root(
                &target.git_root,
                &target.git_relative_path,
                line,
            )
            .map_err(|_| {
                anyhow::anyhow!(
                    "error.checkout_io_failed: git blame could not read the validated checkout file"
                )
            })?;
            (line, blame)
        }
        BlameSource::Snapshot { commit } => blame_at_commit(
            &target.git_root,
            &target.git_relative_path,
            commit,
            target.line,
            target.byte_offset,
        )?,
    };
    let target = BlameTarget {
        display_path: target.display_path,
        line,
    };
    let Some(blame) = blame else {
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
        .map(|session| {
            let edges = edge_index.forward_edges(session);
            edge_targets(&edges, "SESSION_USED_BROFILE")
        })
        .unwrap_or_default();
    let threads = session
        .as_ref()
        .map(|session| {
            let edges = edge_index.reverse_edges(session);
            edge_sources(&edges, "THREAD_HAS_SESSION")
        })
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

/// What the response body reports. The revision-bearing inputs are consumed
/// by the blame itself and deliberately do not survive into this struct, so
/// nothing downstream can re-derive a path or root to read again.
struct BlameTarget {
    display_path: String,
    line: u64,
}

fn line_for_byte_offset(bytes: &[u8], byte_offset: u64) -> u64 {
    let upto = (byte_offset as usize).min(bytes.len());
    bytes[..upto].iter().filter(|byte| **byte == b'\n').count() as u64 + 1
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
    if exact.is_empty() { same_commit } else { exact }
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

fn edge_targets(edges: &[&Edge], kind: &str) -> Vec<EntityRef> {
    edges
        .iter()
        .filter(|edge| edge.kind == kind)
        .map(|edge| edge.target.clone())
        .collect()
}

fn edge_sources(edges: &[&Edge], kind: &str) -> Vec<EntityRef> {
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

pub fn bad_input(message: String) -> String {
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
    use bbox_chunker::{EdgeConfidence, EdgeProvenance};
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
            reads[0]
                .metadata
                .get("anchor.file_path")
                .map(String::as_str),
            Some("src/auth.rs")
        );
        assert_eq!(read_before.kind, "READ_FILE");
    }

    #[test]
    fn prior_read_edges_are_bounded_to_twenty_turns() {
        let edit = edit_edge_at_event("src/main.rs", 100, 100);
        let stale_read = read_edge_at_event("src/stale.rs", 80, 79);
        let recent_read = read_edge_at_event("src/recent.rs", 90, 90);
        let index =
            EdgeIndex::from_edges_for_tests(vec![edit.clone(), stale_read, recent_read.clone()]);

        let reads = prior_read_edges(&index, &edit);

        assert_eq!(reads.len(), 1);
        assert_eq!(
            reads[0]
                .metadata
                .get("anchor.file_path")
                .map(String::as_str),
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
                chunk_hash: path
                    .bytes()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>(),
                occurrence_idx: 0,
            },
            provenance: EdgeProvenance::Explicit,
            confidence: EdgeConfidence::Heuristic,
            metadata,
            project_id: None,
        }
    }
}
