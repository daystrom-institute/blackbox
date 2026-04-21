//! Narrow-subset parser for mermaid `stateDiagram-v2`. Handles exactly
//! what the workflow DSL needs:
//!
//! - `stateDiagram-v2` header (required first non-empty non-comment line)
//! - `[*]` start / end marker
//! - `A --> B` sequential edge
//! - `A --> B: label` labeled edge
//! - `state X <<fork>>` / `<<choice>>` / `<<join>>` typed control nodes
//! - `%%` line comments
//!
//! Everything else is rejected with a line-numbered error. This keeps
//! the surface tight — we don't want mermaid's full grammar complexity
//! leaking into workflow semantics.

use anyhow::{anyhow, bail, Result};
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MermaidNodeKind {
    /// Default — any node id used in an edge that isn't explicitly
    /// declared as a control node.
    Activity,
    /// `state X <<fork>>` — one sync-continuation + >=1 fire-and-forget
    /// branch. Ordering of outgoing edges is significant (first = sync).
    Fork,
    /// `state X <<choice>>` — conditional branch; at most one outgoing
    /// edge is taken per execution based on labels.
    Choice,
    /// `state X <<join>>` — wait for all incoming forked branches
    /// before passing control to the single outgoing edge.
    Join,
}

#[derive(Debug, Clone)]
pub struct MermaidNode {
    pub id: String,
    pub kind: MermaidNodeKind,
}

#[derive(Debug, Clone)]
pub struct MermaidEdge {
    pub from: String,
    pub to: String,
    /// Text label after `:` on the edge line, if any. Used by the
    /// executor as edge metadata ("late-inject", "revise", "converged").
    pub label: Option<String>,
}

#[derive(Debug, Default)]
pub struct MermaidGraph {
    pub nodes: Vec<MermaidNode>,
    pub edges: Vec<MermaidEdge>,
}

pub fn parse_mermaid(src: &str) -> Result<MermaidGraph> {
    let mut declared: HashMap<String, MermaidNodeKind> = HashMap::new();
    let mut edges: Vec<MermaidEdge> = Vec::new();
    let mut seen_in_edges: Vec<String> = Vec::new(); // preserves insertion order
    let mut header_seen = false;

    for (lineno_zero, raw) in src.lines().enumerate() {
        let lineno = lineno_zero + 1;
        let stripped = strip_comment(raw).trim();
        if stripped.is_empty() {
            continue;
        }
        if !header_seen {
            if stripped == "stateDiagram-v2" {
                header_seen = true;
                continue;
            }
            bail!(
                "line {lineno}: expected `stateDiagram-v2` header first, got: {stripped}"
            );
        }

        if let Some((id, kind)) = parse_state_decl(stripped) {
            if let Some(prev) = declared.insert(id.clone(), kind.clone()) {
                if prev != kind {
                    bail!(
                        "line {lineno}: node '{id}' redeclared with different kind ({prev:?} vs {kind:?})"
                    );
                }
            }
            continue;
        }

        if let Some(edge) = parse_edge(stripped).map_err(|e| anyhow!("line {lineno}: {e}"))? {
            for endpoint in [&edge.from, &edge.to] {
                if endpoint != "[*]" && !seen_in_edges.contains(endpoint) {
                    seen_in_edges.push(endpoint.clone());
                }
            }
            edges.push(edge);
            continue;
        }

        bail!("line {lineno}: unrecognized syntax: {stripped}");
    }

    if !header_seen {
        bail!("no `stateDiagram-v2` header found");
    }

    // Materialize nodes: every declared node (fork/choice/join) + every
    // implicitly-referenced activity node. Preserve declaration order,
    // then insertion order for implicits. `[*]` is NOT a node; it's a
    // marker handled at validation time.
    let mut nodes: Vec<MermaidNode> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (id, kind) in &declared {
        nodes.push(MermaidNode {
            id: id.clone(),
            kind: kind.clone(),
        });
        seen.insert(id.clone());
    }
    for id in seen_in_edges {
        if seen.insert(id.clone()) {
            nodes.push(MermaidNode {
                id,
                kind: MermaidNodeKind::Activity,
            });
        }
    }

    Ok(MermaidGraph { nodes, edges })
}

fn strip_comment(line: &str) -> &str {
    match line.find("%%") {
        Some(idx) => &line[..idx],
        None => line,
    }
}

fn parse_state_decl(line: &str) -> Option<(String, MermaidNodeKind)> {
    // Expected: `state <id> <<fork>>` / `<<choice>>` / `<<join>>`
    let rest = line.strip_prefix("state ")?.trim();
    let open_idx = rest.find("<<")?;
    let close_idx = rest.find(">>")?;
    if close_idx <= open_idx + 2 {
        return None;
    }
    let id = rest[..open_idx].trim().to_string();
    let kind_str = &rest[open_idx + 2..close_idx];
    let kind = match kind_str {
        "fork" => MermaidNodeKind::Fork,
        "choice" => MermaidNodeKind::Choice,
        "join" => MermaidNodeKind::Join,
        _ => return None,
    };
    if id.is_empty() {
        return None;
    }
    Some((id, kind))
}

fn parse_edge(line: &str) -> Result<Option<MermaidEdge>> {
    let arrow_idx = match line.find("-->") {
        Some(i) => i,
        None => return Ok(None),
    };
    let from = line[..arrow_idx].trim().to_string();
    let rest = line[arrow_idx + 3..].trim();
    let (to, label) = match rest.find(':') {
        Some(idx) => (
            rest[..idx].trim().to_string(),
            Some(rest[idx + 1..].trim().to_string()).filter(|s| !s.is_empty()),
        ),
        None => (rest.to_string(), None),
    };
    if from.is_empty() || to.is_empty() {
        bail!("edge missing endpoint — from={from:?} to={to:?}");
    }
    // Reject invalid identifiers early (whitespace inside id, etc.)
    if !is_valid_endpoint(&from) {
        bail!("invalid edge source identifier: {from:?}");
    }
    if !is_valid_endpoint(&to) {
        bail!("invalid edge target identifier: {to:?}");
    }
    Ok(Some(MermaidEdge { from, to, label }))
}

fn is_valid_endpoint(s: &str) -> bool {
    if s == "[*]" {
        return true;
    }
    if s.is_empty() {
        return false;
    }
    s.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_graph() {
        let src = "stateDiagram-v2\n    [*] --> A\n    A --> [*]";
        let g = parse_mermaid(src).unwrap();
        assert_eq!(g.edges.len(), 2);
        assert_eq!(g.edges[0].from, "[*]");
        assert_eq!(g.edges[0].to, "A");
        assert_eq!(g.nodes.len(), 1);
        assert_eq!(g.nodes[0].id, "A");
        assert!(matches!(g.nodes[0].kind, MermaidNodeKind::Activity));
    }

    #[test]
    fn parses_fork_and_labeled_edge() {
        let src = r#"stateDiagram-v2
    [*] --> A
    state f <<fork>>
    A --> f
    f --> B
    f --> C: async
    B --> [*]
    C --> [*]"#;
        let g = parse_mermaid(src).unwrap();
        let fork = g
            .nodes
            .iter()
            .find(|n| n.id == "f")
            .expect("fork declared");
        assert!(matches!(fork.kind, MermaidNodeKind::Fork));
        let async_edge = g
            .edges
            .iter()
            .find(|e| e.from == "f" && e.to == "C")
            .expect("f->C edge");
        assert_eq!(async_edge.label.as_deref(), Some("async"));
    }

    #[test]
    fn parses_choice_with_back_edge() {
        let src = r#"stateDiagram-v2
    [*] --> Propose
    Propose --> Check
    state Check <<choice>>
    Check --> Propose: revise
    Check --> Done: converged
    Done --> [*]"#;
        let g = parse_mermaid(src).unwrap();
        let choice = g.nodes.iter().find(|n| n.id == "Check").unwrap();
        assert!(matches!(choice.kind, MermaidNodeKind::Choice));
        let back_edge = g
            .edges
            .iter()
            .find(|e| e.from == "Check" && e.to == "Propose")
            .unwrap();
        assert_eq!(back_edge.label.as_deref(), Some("revise"));
    }

    #[test]
    fn strips_comments_and_blank_lines() {
        let src = r#"stateDiagram-v2
    %% this is a comment
    [*] --> A  %% trailing comment
    A --> [*]

"#;
        let g = parse_mermaid(src).unwrap();
        assert_eq!(g.edges.len(), 2);
    }

    #[test]
    fn missing_header_fails() {
        let src = "[*] --> A\nA --> [*]";
        let err = parse_mermaid(src).unwrap_err().to_string();
        assert!(err.contains("stateDiagram-v2"), "err: {err}");
    }

    #[test]
    fn unknown_state_kind_fails_via_unrecognized_line() {
        let src = "stateDiagram-v2\n    state X <<bogus>>";
        let err = parse_mermaid(src).unwrap_err().to_string();
        assert!(err.contains("unrecognized syntax"));
    }

    #[test]
    fn invalid_edge_identifier_fails() {
        let src = "stateDiagram-v2\n    [*] --> A B\n";
        let err = parse_mermaid(src).unwrap_err().to_string();
        assert!(err.contains("invalid edge target"));
    }

    #[test]
    fn redeclare_with_different_kind_fails() {
        let src = r#"stateDiagram-v2
    state X <<fork>>
    state X <<choice>>"#;
        let err = parse_mermaid(src).unwrap_err().to_string();
        assert!(err.contains("redeclared"));
    }
}
